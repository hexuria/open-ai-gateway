//! Postgres.

use oag_core::{Error, Result};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

/// A Postgres connection pool.
#[derive(Debug, Clone)]
pub struct Db {
    pool: PgPool,
    /// What `after_connect` set on every connection, so `migrate` can put it
    /// back after lifting it for its own session.
    statement_timeout_ms: i64,
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
        Ok(Self {
            pool,
            statement_timeout_ms: timeout_ms,
        })
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

        // This is a pooled connection, and `after_connect` gave it the
        // request path's statement timeout. A backfill over the ledger, an
        // index build on a large table, or simply waiting on the advisory
        // lock behind another replica's migration all legitimately outlast
        // ten seconds — and cancelled at that point they leave the deploy red
        // with the schema half moved. Lifted for this session, put back below
        // before the connection returns to the pool.
        Self::lift_statement_timeout(&mut conn).await?;

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

        // Back to the request path's value: the pool will hand this
        // connection to a query that expects the bound.
        self.restore_statement_timeout(&mut conn).await;

        result
    }

    /// Remove the session's statement timeout, for the migration.
    ///
    /// Its own function so the test can see the lifted value: from outside
    /// `migrate` only the restored one is observable, and a test of that
    /// alone passes with the lift deleted.
    async fn lift_statement_timeout(conn: &mut sqlx::PgConnection) -> Result<()> {
        sqlx::query("SELECT set_config('statement_timeout', '0', false)")
            .execute(conn)
            .await
            .map(|_| ())
            .map_err(|e| Error::Internal(format!("lifting statement timeout: {e}")))
    }

    /// Put the request path's statement timeout back on the session.
    ///
    /// Best-effort: the connection is about to return to a pool whose
    /// `after_connect` cannot run again for it, and there is nothing better
    /// to do with a failure here than let the next query find out.
    async fn restore_statement_timeout(&self, conn: &mut sqlx::PgConnection) {
        let _ = sqlx::query("SELECT set_config('statement_timeout', $1, false)")
            .bind(format!("{}ms", self.statement_timeout_ms))
            .execute(conn)
            .await;
    }

    /// Whether the database is actually reachable.
    pub async fn ping(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::Db;

    /// The session's timezone is UTC, whatever the server's own default is.
    ///
    /// This is a guard on an assumption, not a fix. Finding S3 of the
    /// 2026-09-05 review said the pool sets no `TimeZone`, so
    /// `date_trunc('month', now())` — which truncates in the session's zone —
    /// would disagree with every Rust-side month boundary, which is UTC. On a
    /// server defaulting to a local zone that would put two contradictory
    /// month-to-date figures in one admin response and charge last month's
    /// spend against this month's cap for the offset.
    ///
    /// The premise turned out to be wrong: sqlx sends `TimeZone=UTC` in its
    /// startup packet on every connection, so `pg_settings.source` for it reads
    /// `client` and the server's default never applies. Nothing needed fixing —
    /// but the money depends on a driver default that no line in this
    /// repository asks for, and that is worth an assertion rather than a
    /// comment. If a driver upgrade, a connection-option change or a different
    /// backend ever stops supplying it, this fails here rather than in a
    /// month-end invoice.
    ///
    /// The test database is given a non-UTC default for the length of the test,
    /// because without one this asserts nothing: the dev and CI containers are
    /// both UTC already. It is read back before being reset, so a green result
    /// cannot come from the forcing step having silently failed, and the reset
    /// runs before any assertion so a failure cannot leave it behind.
    #[tokio::test]
    async fn the_session_timezone_is_utc_whatever_the_server_prefers() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let Some(name) = url
            .rsplit('/')
            .next()
            .map(|n| n.split('?').next().unwrap_or(n))
        else {
            eprintln!("skipped: no database name in OAG_TEST_DATABASE_URL");
            return;
        };
        // Not the live database: this changes a database-level setting, and on
        // a dev host `oag` is served by a running gateway.
        assert_ne!(name, "oag", "point OAG_TEST_DATABASE_URL at a test database");

        // A database name cannot be a bind parameter, and sqlx accepts only
        // `&'static str` by design, so that a dynamic statement has to be
        // deliberate. Leaking two short strings for the life of a test process
        // is what being deliberate costs. Nothing but the checked name is
        // interpolated.
        let force: &'static str = Box::leak(
            format!("ALTER DATABASE {name} SET TimeZone = 'Pacific/Auckland'").into_boxed_str(),
        );
        let restore: &'static str =
            Box::leak(format!("ALTER DATABASE {name} RESET TimeZone").into_boxed_str());

        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect");
        sqlx::raw_sql(force)
            .execute(&admin)
            .await
            .expect("force a non-UTC default");

        // Everything is read first and the reset runs before any assertion, so
        // a failing assertion cannot leave the database in the forced state.
        let db = Db::connect_with(&url, 1, std::time::Duration::from_secs(10)).expect("pool");
        let observed: sqlx::Result<(String, String, String, String)> = sqlx::query_as(
            "SELECT current_setting('TimeZone'),
                    (SELECT source FROM pg_settings WHERE name = 'TimeZone'),
                    date_trunc('month', now())::text,
                    date_trunc('month', now() AT TIME ZONE 'UTC')::text",
        )
        .fetch_one(db.pool())
        .await;
        let forced: sqlx::Result<Option<String>> = sqlx::query_scalar(
            "SELECT unnest(setconfig) FROM pg_db_role_setting s
               JOIN pg_database d ON d.oid = s.setdatabase
              WHERE d.datname = current_database()",
        )
        .fetch_optional(&admin)
        .await;

        sqlx::raw_sql(restore)
            .execute(&admin)
            .await
            .expect("reset");

        assert_eq!(
            forced.expect("read the database default").as_deref(),
            Some("TimeZone=Pacific/Auckland"),
            "the forcing step worked, so this test can actually fail"
        );
        let (zone, source, session_month, utc_month) = observed.expect("read the session");
        assert_eq!(
            zone, "UTC",
            "the server prefers Auckland and the session must not"
        );
        assert_eq!(
            source, "client",
            "and it is the client that says so — the day this reads `database`, \
             the month boundary has moved and the ledger has two of them"
        );
        assert_eq!(
            session_month.trim_end_matches("+00"),
            utc_month,
            "so SQL and Rust agree on where the month begins"
        );
    }

    /// Migrations run without the pool's statement timeout, and hand the
    /// connection back with it restored.
    ///
    /// Skipped when `OAG_TEST_DATABASE_URL` is unset; CI sets it. A pool of
    /// one, so the connection `migrate` used is the one handed out after.
    #[tokio::test]
    async fn migrate_lifts_the_statement_timeout_for_itself_and_puts_it_back() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect_with(&url, 1, std::time::Duration::from_millis(1234)).expect("pool");

        let before: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(db.pool())
            .await
            .expect("show");
        assert_eq!(before, "1234ms", "after_connect set it");

        // Already applied, so this is the lock, the version scan and the
        // unlock — and it must not run under 1234ms either: an advisory lock
        // held by another replica's migration is waited on for as long as
        // that migration takes.
        db.migrate().await.expect("migrate");

        let after: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(db.pool())
            .await
            .expect("show");
        assert_eq!(after, "1234ms", "restored before the connection went back");

        // The half a restore-only test cannot see: what the migration itself
        // ran under. The two halves, on the pool's one connection.
        let mut conn = db.pool().acquire().await.expect("the one connection");
        Db::lift_statement_timeout(&mut conn).await.expect("lift");
        let lifted: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(&mut *conn)
            .await
            .expect("show");
        assert_eq!(lifted, "0", "no bound while the migration runs");
        db.restore_statement_timeout(&mut conn).await;
        let restored: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(&mut *conn)
            .await
            .expect("show");
        assert_eq!(restored, "1234ms");
    }

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
