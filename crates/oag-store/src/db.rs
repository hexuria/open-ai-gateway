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

impl Db {
    /// Configure the pool. Does not dial — connections open on first use.
    pub fn connect(url: &str, max_connections: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(10))
            // Recycle connections periodically: a long-lived pool behind a
            // proxy or failover-capable Postgres accumulates connections
            // pointing at a former primary.
            .max_lifetime(Duration::from_mins(30))
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

        let result = sqlx::migrate!("../../migrations")
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
