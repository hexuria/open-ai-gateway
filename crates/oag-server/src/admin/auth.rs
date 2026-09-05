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
    use axum::response::IntoResponse as _;

    /// A2, the wiring. The shed arm is actually on the path.
    ///
    /// Reads this file's own source, because driving `require_admin_layer` to
    /// the `Overloaded` branch needs an `AppState` whose auth semaphore is
    /// exhausted — a fixture larger and less reliable than the thing it would
    /// prove. The test below covers what the response looks like; this covers
    /// that anything reaches it, which is the half that was missing.
    #[test]
    fn the_shed_arm_is_wired_into_the_admin_layer() {
        let src = include_str!("auth.rs");
        let layer = src
            .split_once("pub async fn require_admin_layer(")
            .expect("the layer is in this file")
            .1;
        let body = &layer[..layer.find("\n}\n").unwrap_or(layer.len())];
        assert!(
            body.contains("Err(oag_core::Error::Overloaded) => return overloaded()"),
            "without this arm every error collapses into 401, and the operator \
             opening the dashboard to stop a key flood is told their key is wrong"
        );
        // Above the catch-all, or it never matches.
        let shed = body.find("Error::Overloaded").expect("checked above");
        let catch_all = body
            .find("Err(e) => {")
            .expect("the layer still has a catch-all");
        assert!(shed < catch_all, "a later arm would never be reached");
    }

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
