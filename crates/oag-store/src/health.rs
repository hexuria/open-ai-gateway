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
///
/// Redis stays in the readiness check even though the request path no longer
/// refuses traffic without it: credential selection runs open when Redis
/// cannot answer (see `oag_slot_accounting_degraded_total`), which keeps
/// requests flowing on a replica that is already serving them, but a replica
/// with no Redis is oversubscribing every credential it touches and should
/// not be handed *new* traffic while healthy replicas exist. Unready routes
/// around it; degraded selection covers the requests already inside it.
pub async fn readiness(db: &Db, cache: &Cache) -> Readiness {
    let (database, redis) = tokio::join!(db.ping(), cache.ping());
    Readiness {
        ready: database && redis,
        database,
        redis,
    }
}
