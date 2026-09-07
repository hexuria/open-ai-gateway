//! One authentication check for the whole admin API.
//!
//! Applied as a `route_layer` over the `/admin/api` sub-router rather than
//! called from each handler. The difference matters: with per-handler calls,
//! adding a route and forgetting the call produces an endpoint that is silently
//! unauthenticated and looks exactly like the others. With a layer, the only
//! way to reach that state is to declare the route in the wrong function, which
//! is visible in the ten lines of `admin_routes`.

use crate::AppState;
use axum::Json;
use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;

/// Who performed an admin write. Recorded on every mutation.
#[derive(Debug, Clone)]
pub struct AdminActor {
    pub principal_id: uuid::Uuid,
    pub email: String,
}

impl<S> FromRequestParts<S> for AdminActor
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Absent means the layer is not in front of this route. That is a wiring
        // bug, and the safe reading of a wiring bug on an admin write path is to
        // refuse rather than to proceed with no idea who is asking.
        parts.extensions.get::<Self>().cloned().ok_or_else(|| {
            tracing::error!("admin handler reached without the auth layer in front of it");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "admin route is misconfigured" })),
            )
                .into_response()
        })
    }
}

pub async fn require_admin_layer(
    State(state): State<Arc<AppState>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(key) = crate::gateway::extract_key(req.headers()) else {
        return unauthorised();
    };
    let ctx = match state.auth.authenticate(key).await {
        Ok(Some(ctx)) => ctx,
        Ok(None) => return unauthorised(),
        // Shedding is not a bad key, and saying so matters most at the moment
        // it happens. Under a key flood the lookup semaphore returns
        // `Overloaded`, and collapsing every error into 401 told the operator
        // opening the dashboard to stop the incident that their key was wrong —
        // so the obvious next move is to mint a new one, which is another
        // lookup, during a flood of lookups.
        //
        // The inference path has always preserved this distinction, for exactly
        // this reason. 503 with a `Retry-After` is what a client already knows
        // how to read, and what a load balancer routes around.
        Err(oag_core::Error::Overloaded) => return overloaded(),
        Err(e) => {
            tracing::error!(error = %e, "admin authentication failed");
            return unauthorised();
        }
    };

    // Checked before the principal lookup, and it is the check that matters:
    // every key of an admin principal used to be an admin key, including the
    // one `oag admin init` prints for pasting into a client.
    if !ctx.admin {
        return forbidden_key();
    }

    let row: Option<(String, String)> =
        match sqlx::query_as("SELECT role, email FROM principal WHERE id = $1")
            .bind(ctx.principal_id)
            .fetch_optional(state.db.pool())
            .await
        {
            Ok(row) => row,
            Err(e) => {
                tracing::error!(error = %e, "admin principal lookup failed");
                return unauthorised();
            }
        };

    let Some((role, email)) = row else {
        return forbidden();
    };
    if role != "admin" {
        return forbidden();
    }

    req.extensions_mut().insert(AdminActor {
        principal_id: ctx.principal_id,
        email,
    });
    next.run(req).await
}

/// Shed, not refused. See the note at the `authenticate` call.
///
/// One second, because the condition it reports is a queue that is draining
/// rather than an outage: long enough that a retry is not part of the flood,
/// short enough that a dashboard recovers on its own without a reload.
fn overloaded() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(axum::http::header::RETRY_AFTER, "1")],
        Json(json!({
            "error": "the gateway is shedding load; this is not an authentication failure"
        })),
    )
        .into_response()
}

fn unauthorised() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "an admin API key is required" })),
    )
        .into_response()
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "this key's principal is not an admin" })),
    )
        .into_response()
}

fn forbidden_key() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "this key was not minted as an admin key; mint one with `oag admin key create --admin`. An inference key is deliberately not enough",
            "hint": "oag admin key create --email <you> --admin",
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::overloaded;
    use axum::http::StatusCode;

    /// A2, the wiring, driven: a shed lookup really does leave here as a 503.
    ///
    /// The permit is what has to be exhausted. `AuthCache` takes twice
    /// `database.max_connections` with a floor of one, so a configured zero
    /// buys exactly one permit — and the pool is sized separately, at four, so
    /// the second request is refused by the permit rather than queued at the
    /// pool.
    ///
    /// The holder parks in the Postgres handshake against a listener that
    /// accepts and never speaks, so it keeps its permit for as long as the test
    /// needs and the second request's answer is deterministic rather than a
    /// race. It is aborted rather than awaited: waiting out the pool's
    /// ten-second acquire timeout would put thirty-odd seconds into every run
    /// of this crate's tests to observe something already observed.
    ///
    /// Distinct keys on purpose: `authenticate` single-flights by hash, so two
    /// requests carrying the same key would be one lookup holding one permit.
    /// Redis is a closed port, refused immediately, so the lookup reaches the
    /// permit rather than being answered from L2.
    #[tokio::test]
    async fn a_shed_admin_lookup_leaves_the_layer_as_a_503() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        // Accepts the connection and never sends a byte, so the Postgres
        // startup message is never answered and the lookup holds its permit.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let hangs = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((conn, _)) = listener.accept().await {
                held.push(conn);
            }
        });

        let config = oag_core::config::Config::from_yaml(&format!(
            r#"
database:
  url: "postgres://oag:oag@{hangs}/oag"
  max_connections: 0
redis:
  url: "redis://127.0.0.1:1"
security:
  signing_secret: "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0="
  credential_kek: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
"#
        ))
        .expect("test config");
        let db = oag_store::Db::connect(&config.database.url, 4).expect("lazy pool");
        let cache = oag_store::Cache::connect(&config.redis.url).expect("lazy client");
        let state = std::sync::Arc::new(crate::AppState::new(config, db, cache).expect("state"));
        let app = crate::admin_router(state);

        let ask = |key: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .uri("/admin/api/summary")
                        .header("authorization", format!("Bearer {key}"))
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response")
            }
        };

        // One request takes the only permit and parks in the handshake. It is
        // never awaited: it would sit there for the pool's whole acquire
        // timeout, and what it is for is holding the permit while the next
        // request asks for one.
        let holder = tokio::spawn(ask("oag_live_a_aaaaaaaaaaaaaaaaaaaaaaaa"));
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        let shed = ask("oag_live_b_bbbbbbbbbbbbbbbbbbbbbbbb").await;
        holder.abort();

        assert_eq!(
            shed.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a refused permit has to leave this layer as a shed, not as a 401 \
             telling the operator their admin key is wrong"
        );
        assert_eq!(
            shed.headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1"),
            "a 503 without a Retry-After is one a dashboard cannot recover from"
        );
    }

    // The source scan that used to sit here — reading this file for the
    // `Overloaded` arm and checking it came before the catch-all — is retired.
    // The test above drives the layer to that arm, which proves both facts:
    // an arm below the catch-all would never match, and the request would come
    // back 401.

    /// A2. Shedding is not a bad key, and the difference matters most now.
    ///
    /// Under a key flood the lookup semaphore returns `Overloaded`, and every
    /// error was collapsed into 401 — so the operator opening the dashboard to
    /// stop the incident was told their key was wrong. The obvious next move is
    /// to mint a new one, which is another lookup, during a flood of lookups.
    /// The inference path has always preserved this distinction.
    #[test]
    fn a_shed_admin_lookup_is_a_503_with_a_retry_after() {
        let response = overloaded();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1"),
            "a client and a load balancer both already know how to read this"
        );
        assert_ne!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "the one thing it must not say is that the key is wrong"
        );
    }
}
