//! Readiness.

use crate::{Cache, Db};
use serde::Serialize;

/// What `/health/ready` reports.
///
/// Four bools rather than a state enum, which `struct_excessive_bools` would
/// prefer. They are not a state: each names one dependency, independently, and
/// this struct is a wire format an operator and a load balancer both read.
/// Collapsing them would lose the property A3 established for the admin
/// summary — that a caller told only "something failed" cannot tell which
/// number to distrust — on the endpoint where it matters most.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize)]
pub struct Readiness {
    pub ready: bool,
    pub database: bool,
    pub redis: bool,
    /// Whether the schema this binary needs is applied. See [`Db::schema_ready`].
    pub schema: bool,
}

/// Check the dependencies a request actually needs.
///
/// sub2api's `/health` returns a static `{"status":"ok"}` regardless of whether
/// its database is reachable, so a replica with a dead connection pool stays in
/// the load balancer's rotation and the failure spreads to every client instead
/// of being routed around. Readiness has to be a real check or it is worse than
/// no check at all.
///
/// A reachable database is not a usable one, which is the third check. `ping`
/// is `SELECT 1` and succeeds against a database with no tables, so a replica
/// whose schema had vanished reported `{"ready":true,"database":true}` while
/// every request 500'd — a readiness endpoint actively vouching for a process
/// that could not serve. `schema` is reported beside `database` rather than
/// folded into it, because "cannot reach Postgres" and "reached it and the
/// schema is wrong" need different operators and different fixes.
///
/// Redis stays in the readiness check even though the request path no longer
/// refuses traffic without it: credential selection runs open when Redis
/// cannot answer (see `oag_slot_accounting_degraded_total`), which keeps
/// requests flowing on a replica that is already serving them, but a replica
/// with no Redis is oversubscribing every credential it touches and should
/// not be handed *new* traffic while healthy replicas exist. Unready routes
/// around it; degraded selection covers the requests already inside it.
pub async fn readiness(db: &Db, cache: &Cache) -> Readiness {
    let (database, redis, schema) = tokio::join!(db.ping(), cache.ping(), db.schema_ready());
    Readiness {
        ready: database && redis && schema,
        database,
        redis,
        schema,
    }
}

#[cfg(test)]
mod tests {
    use super::readiness;
    use crate::{Cache, Db};

    /// `readiness` consults the schema, not just the connection.
    ///
    /// `Db::schema_ready` has its own tests; nothing there says this function
    /// calls it. Delete the call and every one of them still passes while
    /// `/health/ready` goes back to vouching for a replica whose schema has
    /// vanished — which is the failure this whole change is about, and it is
    /// the shape a helper test cannot see.
    #[tokio::test]
    async fn a_reachable_database_with_no_schema_is_not_ready_here_either() {
        let (Ok(url), Ok(redis_url)) = (
            std::env::var("OAG_TEST_DATABASE_URL"),
            std::env::var("OAG_TEST_REDIS_URL"),
        ) else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL / OAG_TEST_REDIS_URL unset");
            return;
        };
        let Some((prefix, _)) = url.rsplit_once('/') else {
            eprintln!("skipped: no database name in OAG_TEST_DATABASE_URL");
            return;
        };
        let cache = Cache::connect(&redis_url).expect("connect");

        let migrated = Db::connect(&url, 2).expect("connect");
        migrated.migrate().await.expect("migrate");
        let ok = readiness(&migrated, &cache).await;
        assert!(
            ok.ready && ok.database && ok.schema,
            "a migrated database with a live Redis is ready: {ok:?}"
        );

        // Reachable, answers `SELECT 1`, and has none of this gateway's tables.
        let bare = Db::connect(&format!("{prefix}/postgres"), 2).expect("connect");
        let empty = readiness(&bare, &cache).await;
        assert!(
            empty.database,
            "the connection is alive — which is exactly why `database` alone \
             could vouch for this: {empty:?}"
        );
        assert!(
            !empty.schema,
            "but the schema is not there, and that has to be visible: {empty:?}"
        );
        assert!(
            !empty.ready,
            "so the replica must not be handed traffic — a load balancer reads \
             this one field: {empty:?}"
        );
    }
}
