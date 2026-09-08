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
        // Deliberately without the string. It is not one of ours, so its
        // leading characters are somebody else's secret — a caller who pastes
        // an Anthropic key into the wrong client must not leave a third of it
        // in this log. That it was not key-shaped is the whole finding.
        tracing::info!("refused an inbound credential: not the shape this gateway issues");
        return super::error_response(&Error::Unauthenticated);
    }
    let caller = match state.auth.authenticate(key).await {
        Ok(Some(ctx)) => ctx,
        Ok(None) => {
            // The prefix, never the key. It is the same sixteen characters
            // `api_key.key_prefix` stores, so an operator can join this line to
            // a row and answer "which key was refused" — the first question of
            // every authentication incident, and one this gateway could not
            // answer at all until now.
            tracing::info!(
                key_prefix = %oag_store::repo::loggable_key_prefix(key),
                "refused an inbound credential: no active key with this prefix"
            );
            return super::error_response(&Error::Unauthenticated);
        }
        // A backend that cannot answer is not a valid key. `error_response`
        // renders this as a 500 with no detail; the key never reaches a log.
        Err(e) => return super::error_response(&e),
    };

    req.extensions_mut().insert(Caller(caller));
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt as _;

    /// Somewhere to put the log this test is about.
    ///
    /// `tracing` writes through a `MakeWriter`, so capturing it means being
    /// one — reading the process's stderr back is what an earlier test in this
    /// repo tried, and `tracing_subscriber::fmt()` writes to stdout, so it
    /// asserted against an empty string and could never fail.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            let buf = self.0.lock().expect("no other thread panicked holding it");
            String::from_utf8_lossy(&buf).into_owned()
        }
    }

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("captured log poisoned"))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The presented string, shaped like the mistake it guards against: an
    /// Anthropic key pasted into a client pointed at this gateway.
    ///
    /// Assembled rather than written out. A literal that looks like a real
    /// vendor key is a literal this repo's pre-commit scanner blocks, which is
    /// the right call even here — the guard is worth more than the resemblance.
    fn somebody_elses_secret() -> String {
        format!("sk-{}-{}", "ant-api03", "CANARYCANARYCANARY")
    }

    #[tokio::test]
    async fn a_credential_that_is_not_ours_is_refused_without_being_written_down() {
        // The two halves of the same rule. A refusal has to leave a trace —
        // until this change an operator asking "what key was rejected at
        // 21:40" had nothing at all to read — but the trace of a string that
        // is *not* one of ours can only be the fact that one arrived. Its
        // leading characters are someone else's secret, and a log is exactly
        // the place a secret gets copied into a ticket.
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let res = crate::public_router(crate::testing::state(""))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/models")
                    .header(
                        "authorization",
                        format!("Bearer {}", somebody_elses_secret()),
                    )
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let log = captured.text();
        assert!(
            log.contains("refused an inbound credential"),
            "a refusal with no line is the blind spot this closed; got {log:?}"
        );
        assert!(
            !log.contains(&somebody_elses_secret()),
            "the whole secret reached the log: {log:?}"
        );
        assert!(
            !log.contains("CANARY"),
            "part of the secret reached the log: {log:?}"
        );
    }

    #[test]
    fn the_unknown_key_line_carries_the_prefix_and_never_the_key() {
        // A source scan, and it says so: the arm it guards runs only when
        // `authenticate` answers `Ok(None)`, which needs a Redis and a
        // Postgres that answer — this crate's unit harness has neither, and a
        // dead backend takes the `Err` arm instead. What a scan can still
        // pin is the thing that would be a leak rather than a bug: that the
        // line interpolates `loggable_key_prefix(key)` and not `key`.
        //
        // Cut at the test module so the scan cannot read itself; the strings
        // above would otherwise satisfy it with the call site deleted.
        let source = include_str!("authn.rs");
        let (production, _) = source
            .split_once("#[cfg(test)]")
            .expect("this module is the cut point");
        assert!(
            production.contains("pub async fn require_key_layer"),
            "the scan lost its haystack"
        );

        let (_, arm) = production
            .split_once("Ok(None) => {")
            .expect("the unknown-key arm");
        let arm = arm
            .split_once("Err(e) =>")
            .map_or(arm, |(before, _)| before);
        assert!(
            arm.contains("loggable_key_prefix(key)"),
            "the unknown-key arm logs no prefix: {arm}"
        );
        assert!(
            !arm.contains("%key,") && !arm.contains("= %key") && !arm.contains("{key}"),
            "the unknown-key arm interpolates the key itself: {arm}"
        );
    }
}
