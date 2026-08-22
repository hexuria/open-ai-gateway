//! Readiness.

use crate::{Cache, Db};
use serde::Serialize;

/// What `/health/ready` reports.
#[derive(Debug, Clone, Serialize)]
pub struct Readiness {
    pub ready: bool,
    pub database: bool,
    pub redis: bool,
}

/// Check the dependencies a request actually needs.
///
/// sub2api's `/health` returns a static `{"status":"ok"}` regardless of whether
/// its database is reachable, so a replica with a dead connection pool stays in
/// the load balancer's rotation and the failure spreads to every client instead
/// of being routed around. Readiness has to be a real check or it is worse than
/// no check at all.
pub async fn readiness(db: &Db, cache: &Cache) -> Readiness {
    let (database, redis) = tokio::join!(db.ping(), cache.ping());
    Readiness {
        ready: database && redis,
        database,
        redis,
    }
}
