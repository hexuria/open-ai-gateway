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
