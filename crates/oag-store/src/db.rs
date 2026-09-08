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
/// The advisory lock [`Db::migrate`] holds while it applies migrations.
///
/// Public because migrating is not the only thing that touches
/// `_sqlx_migrations`: a test that deliberately corrupts that table — to prove
/// a gap or a `success = false` row is caught — opens a window in which the
/// table does not describe the schema, and a concurrent `migrate()` walking
/// into that window re-applies a migration and collides on the primary key.
/// Anything mutating that table must hold this for the width of its window.
pub const MIGRATION_LOCK_ID: i64 = 0x0A6_1247_0001;

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

    /// Whether the schema this binary was built against is actually applied.
    ///
    /// `ping` answers a different question — "is the connection alive" — and
    /// answering only that is how a replica came to report itself ready while
    /// every request 500'd. `SELECT 1` succeeds against a database with no
    /// tables at all, so after the dev Postgres (whose data directory is
    /// tmpfs) restarted empty, `/health/ready` returned
    /// `{"ready":true,"database":true}` while `/v1/models` returned 500. A
    /// readiness endpoint vouching for a process that cannot serve is worse
    /// than none: in Kubernetes that pod takes the whole rotation.
    ///
    /// So this asks whether every migration the binary carries has been
    /// applied. `>=` and not `==`, deliberately: a schema *ahead* of this
    /// binary is the normal middle of a rolling deploy — every migration here
    /// is expand-then-contract for exactly that reason — and refusing traffic
    /// for it would make every old replica unready during a routine upgrade.
    /// A schema *behind* is the case that cannot serve, and is the one this
    /// refuses.
    ///
    /// Three states it distinguishes that `ping` cannot: the schema is gone
    /// (`_sqlx_migrations` missing, the query errors), the schema is behind
    /// this binary (fewer applied than embedded — a binary deployed ahead of
    /// its migration), and a migration that ran and failed (`success` is
    /// false, so it does not count).
    ///
    /// One extra query per probe, and `health::cached_readiness` memoises the
    /// whole answer for a second, so a burst of probes still costs one.
    pub async fn schema_ready(&self) -> bool {
        let applied: std::result::Result<i64, _> =
            sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
                .fetch_one(&self.pool)
                .await;
        let Ok(applied) = applied else { return false };
        schema_is_ready(applied, sqlx::migrate!("../../migrations").migrations.len())
    }
}

/// Whether `applied` successful migrations satisfy a binary carrying `embedded`.
///
/// Separated from the query so the comparison can be tested over the cases a
/// live database cannot cheaply be put into. The wiring is covered by
/// `a_reachable_database_with_no_schema_is_not_ready`, which drives the query.
fn schema_is_ready(applied: i64, embedded: usize) -> bool {
    usize::try_from(applied).is_ok_and(|applied| applied >= embedded)
}

#[cfg(test)]
mod tests {
    use super::Db;

    /// A reachable database with no schema is not a ready one.
    ///
    /// This is the case `ping` cannot see and the one that produced a replica
    /// answering `/health/ready` with `{"ready":true,"database":true}` while
    /// every request returned 500. Driven against a real database rather than
    /// asserted about the query, because what has to be true is a property of
    /// the rows in `_sqlx_migrations`.
    #[tokio::test]
    async fn a_reachable_database_with_no_schema_is_not_ready() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        assert!(db.ping().await, "the connection is alive");
        assert!(
            db.schema_ready().await,
            "and a migrated database is ready to serve"
        );

        // A database that answers and has nothing in it. `postgres` exists on
        // every cluster and carries no `_sqlx_migrations`, so it is the empty
        // schema without creating or dropping anything.
        let Some((prefix, _)) = url.rsplit_once('/') else {
            eprintln!("skipped: no database name in OAG_TEST_DATABASE_URL");
            return;
        };
        let bare = Db::connect(&format!("{prefix}/postgres"), 2).expect("connect");
        assert!(
            bare.ping().await,
            "SELECT 1 succeeds — which is exactly the problem, and why `ping` \
             alone could vouch for a replica that cannot serve"
        );
        assert!(
            !bare.schema_ready().await,
            "but there is no schema, so nothing this gateway queries exists"
        );

        // And a database that is merely unreachable fails both.
        let dead = Db::connect("postgres://oag:oag@127.0.0.1:1/oag", 1).expect("lazy pool");
        assert!(!dead.ping().await);
        assert!(!dead.schema_ready().await);
    }

    /// A schema BEHIND this binary is refused; one AHEAD is not.
    ///
    /// Behind is a binary deployed before its migration ran: it queries columns
    /// that do not exist yet, so it cannot serve and must not be handed
    /// traffic. Ahead is the ordinary middle of a rolling deploy — the new
    /// replica migrated, the old ones have not been replaced — and every
    /// migration here is expand-then-contract precisely so those old replicas
    /// keep serving. Refusing traffic for it would take a fleet down during a
    /// routine upgrade, so the comparison is `>=` and this pins that.
    #[tokio::test]
    async fn a_schema_behind_the_binary_is_refused_and_one_ahead_is_not() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect(&url, 2).expect("connect");
        db.migrate().await.expect("migrate");

        // Ahead: one more applied row than the binary embeds. Rolled back, so
        // the real migration state is untouched either way.
        let mut tx = db.pool().begin().await.expect("begin");
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, installed_on, success, checksum, execution_time) \
             VALUES (99999, 'from a newer binary', now(), true, '\\x00', 0)",
        )
        .execute(&mut *tx)
        .await
        .expect("plant a migration this binary does not carry");
        let applied: i64 =
            sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
                .fetch_one(&mut *tx)
                .await
                .expect("count");
        tx.rollback().await.expect("rollback");

        let embedded = sqlx::migrate!("../../migrations").migrations.len();
        assert!(
            applied > i64::try_from(embedded).expect("a small number"),
            "the fixture has to actually be ahead, or it tests nothing: \
             {applied} applied vs {embedded} embedded"
        );
        assert!(
            super::schema_is_ready(applied, embedded),
            "a schema ahead of this binary is the ordinary middle of a rolling \
             deploy, and refusing traffic for it would take a fleet down on a \
             routine upgrade"
        );
    }

    /// The rule the query feeds, over the cases a live database cannot cheaply
    /// be put into.
    #[test]
    fn a_schema_short_of_what_the_binary_carries_is_not_ready() {
        use super::schema_is_ready;

        assert!(schema_is_ready(17, 17), "exactly what it carries");
        assert!(schema_is_ready(18, 17), "ahead — a newer replica migrated");
        assert!(
            !schema_is_ready(16, 17),
            "one behind: this binary queries something the schema has not got yet"
        );
        assert!(!schema_is_ready(0, 17), "an empty schema is the wipe case");
        assert!(
            schema_is_ready(0, 0),
            "a binary carrying no migrations is satisfied by anything, which is \
             only reachable if the migrations directory is empty"
        );
        assert!(
            !schema_is_ready(-1, 1),
            "count() cannot go negative, but the conversion must refuse rather \
             than wrap into a large usize and read as ready"
        );
    }

    /// The session's timezone is UTC, and it is the client that says so.
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
    /// startup packet on every connection. Nothing needed fixing — but the
    /// money then depends on a driver default that no line in this repository
    /// asks for, and that is worth an assertion rather than a comment.
    ///
    /// `source` is what makes this test able to fail. Asserting the zone alone
    /// would pass against a pool that pins nothing, because the dev and CI
    /// containers are both UTC anyway; `pg_settings.source` reads `client` only
    /// when the connection asked, and a server that merely happens to be UTC
    /// reports `configuration file` or `default` instead. So the day a driver
    /// upgrade, a connection-option change or a different backend stops
    /// supplying it, this fails here rather than in a month-end invoice —
    /// and it does so without writing anything to any database.
    #[tokio::test]
    async fn the_session_timezone_is_utc_because_the_client_asked() {
        let Ok(url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let db = Db::connect_with(&url, 1, std::time::Duration::from_secs(10)).expect("pool");

        let (zone, source, session_month, utc_month): (String, String, String, String) =
            sqlx::query_as(
                "SELECT current_setting('TimeZone'),
                        (SELECT source FROM pg_settings WHERE name = 'TimeZone'),
                        date_trunc('month', now())::text,
                        date_trunc('month', now() AT TIME ZONE 'UTC')::text",
            )
            .fetch_one(db.pool())
            .await
            .expect("read the session");

        assert_eq!(zone, "UTC", "every Rust-side month boundary is UTC");
        assert_eq!(
            source, "client",
            "and the connection is what makes it so. `configuration file` here \
             would mean the gateway is inheriting whatever the operator's \
             Postgres was installed with"
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
