//! One authentication check for the whole inference surface, in front of the
//! body rather than behind it.
//!
//! Every public handler used to take `axum::body::Bytes` and call `authenticate`
//! as its first statement. An extractor runs *before* the handler, so by the
//! time the key was looked at the whole body had already been read into memory
//! — up to `server.max_body_bytes` of it, from a caller who had presented no
//! credential at all. A replica with a 1 Gi limit and no unauthenticated rate
//! limit in front of it needs very few concurrent anonymous POSTs to die of it.
//!
//! A middleware sees the request head and leaves the body a stream. Refusing
//! here costs the bytes already on the wire and nothing more, and the handlers
//! keep exactly one authentication path — a second copy is how one endpoint
//! ends up accepting a key the others reject.

use crate::AppState;
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::response::Response;
use oag_core::Error;
use std::sync::Arc;

/// The authenticated caller, as established before the body was touched.
///
/// Handlers take this instead of calling `authenticate` themselves, so a
/// handler that forgot the check cannot compile.
#[derive(Debug, Clone)]
pub struct Caller(pub Arc<oag_store::AuthContext>);

impl<S> FromRequestParts<S> for Caller
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Absent means [`require_key_layer`] is not in front of this route,
        // which is a wiring bug. The safe reading of a wiring bug on a billed
        // path is to refuse rather than to serve an unidentified caller.
        parts.extensions.get::<Self>().cloned().ok_or_else(|| {
            tracing::error!(
                "an inference handler was reached without the auth layer in front of it"
            );
            super::error_response(&Error::Internal(
                "inference route is missing its auth layer".to_owned(),
            ))
        })
    }
}

/// Authenticate from the request head, before any handler asks for the body.
pub async fn require_key_layer(
    State(state): State<Arc<AppState>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(key) = super::extract_key(req.headers()) else {
        return super::error_response(&Error::Unauthenticated);
    };
    // Shape first, before the caches and long before Postgres. Every key
    // this gateway has issued has one exact shape; a string without it is not
    // an unknown key, it is not a key, and it used to buy a Redis GET and a
    // Postgres probe from anyone who could reach the port. Same answer as an
    // unknown key — a 401 — just without the round trips.
    if !oag_store::repo::is_issued_key_shape(key) {
        return super::error_response(&Error::Unauthenticated);
    }
    let caller = match state.auth.authenticate(key).await {
        Ok(Some(ctx)) => ctx,
        Ok(None) => return super::error_response(&Error::Unauthenticated),
        // A backend that cannot answer is not a valid key. `error_response`
        // renders this as a 500 with no detail; the key never reaches a log.
        Err(e) => return super::error_response(&e),
    };

    req.extensions_mut().insert(Caller(caller));
    next.run(req).await
}
