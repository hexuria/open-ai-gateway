//! Postgres.

use oag_core::{Error, Result};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

/// A Postgres connection pool.
#[derive(Debug, Clone)]
pub struct Db {
    pool: PgPool,
}

/// Serialises concurrent migration runs across replicas.
///
/// An arbitrary but fixed 64-bit constant. Postgres advisory locks are keyed by
/// value, so every replica must use the same one; changing it would let two
/// versions of the binary migrate simultaneously.
const MIGRATION_LOCK_ID: i64 = 0x0A6_1247_0001;

/// How long one statement may run before Postgres cancels it, when the caller
/// does not say. Generous for a request-path query, which is a primary-key
/// probe or an indexed range; a statement still running at ten seconds is not
/// slow, it is a connection that would otherwise never come back.
pub const DEFAULT_STATEMENT_TIMEOUT: Duration = Duration::from_secs(10);

impl Db {
    /// Configure the pool with the default statement timeout. Does not dial —
    /// connections open on first use.
    pub fn connect(url: &str, max_connections: u32) -> Result<Self> {
        Self::connect_with(url, max_connections, DEFAULT_STATEMENT_TIMEOUT)
    }

    /// Configure the pool. Does not dial — connections open on first use.
    ///
    /// `statement_timeout` is set on every connection as it opens. Without one,
    /// a primary that stops answering — a black-holed failover, a network
    /// partition — does not fail a query, it holds it: the connection sits in
    /// the query for as long as the kernel keeps the socket, and the pool's
    /// `acquire_timeout` then refuses every *new* request while the old ones
    /// keep the slots. Sixteen such queries and the replica is deaf until
    /// somebody restarts it. A statement timeout makes that a 10-second error
    /// the pool recovers from on its own.
    pub fn connect_with(
        url: &str,
        max_connections: u32,
        statement_timeout: Duration,
    ) -> Result<Self> {
        let timeout_ms = i64::try_from(statement_timeout.as_millis()).unwrap_or(i64::MAX);
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(10))
            // Recycle connections periodically: a long-lived pool behind a
            // proxy or failover-capable Postgres accumulates connections
            // pointing at a former primary.
            .max_lifetime(Duration::from_mins(30))
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    // `set_config` rather than `SET`, because `SET` cannot
                    // take a bind parameter and the value must not be spliced
                    // into SQL text. `false` is "not local": session-level,
                    // so it survives the pool handing the connection to a
                    // different task, which `SET LOCAL` would not.
                    sqlx::query("SELECT set_config('statement_timeout', $1, false)")
                        .bind(format!("{timeout_ms}ms"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            // Lazy, for the same reason as Redis: a replica that cannot boot
            // until Postgres answers will crash-loop through a failover or a
            // restart, when the correct behaviour is to come up, report
            // `ready: false`, and be routed around until it recovers.
            .connect_lazy(url)
            .map_err(|e| Error::Internal(format!("configuring postgres pool: {e}")))?;
        Ok(Self { pool })
    }

    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Apply migrations, serialised across replicas.
    ///
    /// Every replica may call this on boot. The advisory lock means one applies
    /// and the rest wait and then no-op, rather than racing and half-applying.
    /// The lock is session-scoped and released when the connection returns to
    /// the pool, including if this process dies mid-migration.
    pub async fn migrate(&self) -> Result<()> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| Error::Internal(format!("acquiring migration connection: {e}")))?;

        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(MIGRATION_LOCK_ID)
            .execute(&mut *conn)
            .await
            .map_err(|e| Error::Internal(format!("taking migration lock: {e}")))?;

        // `ignore_missing` lets an older binary run against a database that a
        // newer one has already migrated. sqlx defaults it to false, which
        // sounds safer and is not: the deploy model already does exactly this
        // routinely. During a rolling deploy the migration lands while the
        // previous release is still serving — on ECS for up to the 1800s
        // deregistration delay — so old-binary-against-new-schema is the normal
        // steady state for tens of minutes, and it is only safe because every
        // migration must be expand/contract.
        //
        // With the default, the one case it *blocks* is rollback: the older
        // image's migrate aborts with VersionMissing, and on the platforms
        // where the gateway container depends on the migrate step, that
        // revision can never start again. So the setting forbids at rollback
        // time precisely what the deployment does for half an hour on every
        // release. Expand/contract is the invariant that actually matters, and
        // it is documented as a hard requirement in docs/04-cloud.md.
        let mut migrator = sqlx::migrate!("../../migrations");
        migrator.set_ignore_missing(true);

        let result = migrator
            .run(&mut *conn)
            .await
            .map_err(|e| Error::Internal(format!("applying migrations: {e}")));

        // Release explicitly rather than relying on connection drop, so the
        // next replica proceeds immediately instead of waiting for the pool.
        let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(MIGRATION_LOCK_ID)
            .execute(&mut *conn)
            .await;

        result
    }

    /// Whether the database is actually reachable.
    pub async fn ping(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::Db;

    /// The rollback case, reproduced directly.
    ///
    /// A database migrated by a newer release carries a `_sqlx_migrations` row
    /// this binary knows nothing about. With sqlx's default `ignore_missing`,
    /// that aborts with `VersionMissing` — and on the cloud platforms where the
    /// gateway container depends on the migrate step, the rolled-back revision
    /// could then never start at all.
    ///
    /// Skipped without `OAG_TEST_DATABASE_URL`; CI sets it.
    #[tokio::test]
    async fn an_older_binary_still_migrates_against_a_newer_schema() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("baseline migrate");

        // A migration from the future: exactly what a newer release leaves
        // behind. Version is far above anything this binary embeds.
        sqlx::query(
            "INSERT INTO _sqlx_migrations
                 (version, description, installed_on, success, checksum, execution_time)
             VALUES (99999999, 'from a newer release', now(), true, '\\x00', 0)
             ON CONFLICT (version) DO NOTHING",
        )
        .execute(db.pool())
        .await
        .expect("plant the future migration");

        let result = db.migrate().await;

        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 99999999")
            .execute(db.pool())
            .await
            .expect("clean up");

        result.expect(
            "a rolled-back binary must still be able to migrate; \
             this is what ignore_missing(true) buys",
        );
    }
}
