#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! The HTTP surface.
//!
//! Two listeners, not one:
//!
//! - **public** carries inference only. This is what the load balancer fronts,
//!   and the only port that needs to be reachable from anywhere.
//! - **admin** carries the admin API, the SPA, `/metrics`, and `/health/ready`.
//!   Bound to the internal network.
//!
//! sub2api serves both from one port, which means every admin endpoint inherits
//! whatever exposure the inference endpoint has. Splitting them makes "do not
//! expose the admin API" a deployment fact rather than a routing rule someone
//! has to remember to write.

pub mod admin;
pub mod breakers;
pub mod gateway;
pub mod health;
pub mod listen;
pub mod metrics;
pub mod shutdown;
pub mod state;

pub use shutdown::Lifecycle;
pub use state::AppState;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse as _;
use axum::routing::{get, patch, post};
use oag_core::Result;
use std::sync::Arc;

/// Turn a panic anywhere under a router into a 500 on the connection.
///
/// Insurance, not a fix: the fix is for the request path not to panic, and
/// `clippy::panic` is denied workspace-wide to keep it that way. But that lint
/// only covers the panics we *write*. It says nothing about an `IndexMut` on a
/// `serde_json::Value`, a slice index, an arithmetic overflow, or any of the
/// same inside a dependency — and a gateway is the wrong place to find out which
/// of those is reachable.
///
/// What the layer buys is the difference between an answer and a severed
/// connection. An unwind that reaches hyper closes the connection without a
/// response, so the client cannot tell a bug from a network fault; on HTTP/2 it
/// resets every *other* stream multiplexed onto that connection too, which turns
/// one caller's malformed request into unrelated callers' failures.
fn catch_panic() -> tower_http::catch_panic::CatchPanicLayer<PanicIsFiveHundred> {
    tower_http::catch_panic::CatchPanicLayer::custom(PanicIsFiveHundred)
}

/// The error envelope every other failure on these surfaces uses, at 500.
#[derive(Debug, Clone, Copy)]
struct PanicIsFiveHundred;

impl tower_http::catch_panic::ResponseForPanic for PanicIsFiveHundred {
    type ResponseBody = axum::body::Body;

    fn response_for_panic(
        &mut self,
        err: Box<dyn std::any::Any + Send + 'static>,
    ) -> axum::http::Response<Self::ResponseBody> {
        // Logged, never returned. A panic message carries whatever was in scope
        // — the same reason `Error::Internal` is not surfaced to a client.
        let detail = err
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| err.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        tracing::error!(panic = detail, "a handler panicked; answering 500");
        ::metrics::counter!("oag_panics_total").increment(1);

        (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "type": "error",
                "error": { "type": "internal_error", "message": "internal error" }
            })),
        )
            .into_response()
    }
}

/// The inference routes, without the shared health endpoint.
///
/// Every route declared here is authenticated before its body is read, by the
/// `route_layer` at the bottom of the function. Declaring an inference route
/// anywhere else is the only way to produce an unauthenticated one, and the
/// handlers cannot be written without a [`gateway::Caller`] anyway.
fn inference_routes(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/messages", axum::routing::post(gateway::messages))
        .route(
            "/v1/messages/count_tokens",
            axum::routing::post(gateway::count_tokens::count_tokens)
                // The outer limit is `server.max_body_bytes`. This endpoint
                // does a full parse plus a `to_string` of every tool schema,
                // and a real count_tokens body carries no payload worth
                // megabytes. The inner limit wins for this route.
                .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024)),
        )
        // Discovery. Both spellings, because SDKs given a custom base URL
        // differ on whether they keep the version prefix.
        .route("/v1/models", get(gateway::models::list))
        .route("/models", get(gateway::models::list))
        .route("/v1beta/models", get(gateway::models::list_gemini))
        .route(
            "/v1/chat/completions",
            axum::routing::post(gateway::chat_completions),
        )
        // The same surface without the version prefix; several SDKs default to
        // it when given a custom base URL.
        .route(
            "/chat/completions",
            axum::routing::post(gateway::chat_completions),
        )
        .route("/v1/responses", axum::routing::post(gateway::responses))
        .route("/responses", axum::routing::post(gateway::responses))
        // Gemini puts the model and the mode in the path, separated by a colon.
        .route(
            "/v1beta/models/{*model_action}",
            axum::routing::post(gateway::gemini_generate),
        )
        // `route_layer`, so an unmatched path 404s without an auth round trip
        // — and, more importantly, so this runs *before* a handler's `Bytes`
        // extractor. An unauthenticated POST is refused on its headers rather
        // than after `max_body_bytes` have been read into this process.
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(state),
            gateway::require_key_layer,
        ))
}

/// The admin routes, without the shared health endpoint.
fn admin_routes(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    // The only place an `/admin/api` path may be declared. A route added here
    // is authenticated whether or not its author thought about it; producing an
    // unauthenticated admin handler requires declaring it in the wrong
    // function, which is visible in the ten lines below.
    let api = Router::new()
        .route("/summary", get(admin::summary))
        .route("/accounts", get(admin::accounts))
        .route("/routes", get(admin::routes))
        .route("/usage", get(admin::usage))
        .route("/keys", get(admin::keys))
        .route(
            "/services",
            get(admin::list_services).post(admin::create_service),
        )
        .route("/services/{id}", patch(admin::update_service))
        .route("/services/{id}/disable", post(admin::disable_service))
        .route("/services/{id}/enable", post(admin::enable_service))
        .route("/services/{id}/check", post(admin::check_service))
        .route("/catalog/reload", post(admin::reload_catalog))
        .route("/accounts/{id}/disable", post(admin::disable_account))
        .route("/accounts/{id}/enable", post(admin::enable_account))
        .route("/accounts/{id}/clear-cooldown", post(admin::clear_cooldown))
        .route("/keys/{id}/revoke", post(admin::revoke_key))
        // `route_layer` rather than `layer`: an unmatched path under
        // /admin/api should 404 without a database round trip.
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(state),
            admin::require_admin_layer,
        ));

    Router::new()
        // Deliberately outside the layer. The dashboard HTML has to load before
        // the operator can type a key into it; /metrics is scraped without one;
        // /health/ready is probed by the orchestrator without one. A blanket
        // layer over this router breaks all three.
        .route("/", get(admin::dashboard))
        .route("/health/ready", get(health::ready))
        .route("/metrics", get(metrics::render))
        .nest("/admin/api", api)
}

/// Inference. Fronted by the load balancer.
///
/// Carries `/health/live` but not `/health/ready`: readiness is an operational
/// detail, and answering it does not require being reachable from outside.
pub fn public_router(state: Arc<AppState>) -> Router {
    let limit = state.config.server.max_body_bytes;
    let routes = inference_routes(&state).route("/health/live", get(health::live));

    // On a single-port platform there is nowhere else for the admin routes to
    // live, so they join this listener rather than vanishing entirely.
    //
    // What joins it is all of [`admin_routes`], not just `/admin/api`. The
    // three routes that sit outside the admin layer on purpose — `/`,
    // `/metrics`, `/health/ready` — have nothing but reachability protecting
    // them, and here there is none: the dashboard and every gauge in it answer
    // whoever can reach the port. `/admin/api` keeps its key, so the writes
    // lose one layer of two and the reads lose the only one they had. Restrict
    // the service with the platform's ingress rules or IAM.
    let routes = if state.config.server.single_listener {
        routes.merge(admin_routes(&state))
    } else {
        routes
    };

    routes
        .layer(axum::extract::DefaultBodyLimit::max(limit))
        // Outermost, so it also covers the layers below it.
        .layer(catch_panic())
        .with_state(state)
}

/// Admin API, metrics, readiness. Internal network only.
pub fn admin_router(state: Arc<AppState>) -> Router {
    admin_routes(&state)
        .route("/health/live", get(health::live))
        .layer(catch_panic())
        .with_state(state)
}

/// Reload the catalog on an interval, for as long as the process runs.
///
/// The catalog lives in memory, so without this a repriced or newly-seeded
/// model needs every replica restarted before it is visible — and a replica
/// holding a stale catalog looks perfectly healthy while failing to route.
fn spawn_catalog_refresh(state: Arc<AppState>) {
    let interval = state.config.gateway.catalog_refresh_interval;
    if interval.is_zero() {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick fires immediately, and the catalog was already loaded
        // at boot; skip it rather than doing the same query twice.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match state.reload_catalog().await {
                Ok(n) => tracing::debug!(models = n, "catalog refreshed"),
                Err(e) => tracing::warn!(error = %e, "catalog refresh failed; keeping the old one"),
            }
        }
    });
}

/// Bind the listeners and serve until shutdown.
pub async fn serve(state: Arc<AppState>) -> Result<()> {
    let public_addr = state.config.server.public_addr.clone();
    let lifecycle = Arc::clone(&state.lifecycle);
    let drain = state.config.gateway.max_stream_duration;
    // `axum::serve` builds its own hyper connection and exposes none of it, so
    // these two settings were config and documentation with nothing reading
    // them. See [`listen`].
    let deadlines = listen::Deadlines::from(&state.config.server);

    let public = tokio::net::TcpListener::bind(&public_addr)
        .await
        .map_err(|e| oag_core::Error::Internal(format!("binding {public_addr}: {e}")))?;

    if state.config.server.single_listener {
        tracing::warn!(
            %public_addr,
            "single-listener mode: the admin API, the dashboard, /metrics and \
             /health/ready are on the public listener. /admin/api still requires an \
             admin key; the dashboard, /metrics and /health/ready never did, so on \
             this port they are unauthenticated — restrict it at the edge."
        );
        spawn_catalog_refresh(Arc::clone(&state));
        listen::serve(
            public,
            public_router(state),
            deadlines,
            shutdown::signal(lifecycle, drain),
        )
        .await;
        return Ok(());
    }

    let admin_addr = state.config.server.admin_addr.clone();
    let admin = tokio::net::TcpListener::bind(&admin_addr)
        .await
        .map_err(|e| oag_core::Error::Internal(format!("binding {admin_addr}: {e}")))?;

    tracing::info!(%public_addr, %admin_addr, "listening");
    spawn_catalog_refresh(Arc::clone(&state));

    let public_srv = listen::serve(
        public,
        public_router(Arc::clone(&state)),
        deadlines,
        shutdown::signal(Arc::clone(&lifecycle), drain),
    );
    let admin_srv = listen::serve(
        admin,
        admin_router(state),
        deadlines,
        shutdown::signal(lifecycle, drain),
    );

    tokio::join!(public_srv, admin_srv);
    Ok(())
}

#[cfg(test)]
mod router_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    /// A state that dials nothing.
    ///
    /// `Db::connect` builds a lazy pool and `Cache::connect` only opens a redis
    /// client, so neither touches the network here. Every assertion below is
    /// about routing and the auth layer, which run before any backend does.
    fn state(single_listener: bool) -> Arc<AppState> {
        state_with(single_listener, 32 * 1024 * 1024)
    }

    fn state_with(single_listener: bool, max_body_bytes: usize) -> Arc<AppState> {
        let src = format!(
            r#"
database:
  url: "postgres://oag:oag@127.0.0.1:1/oag"
redis:
  url: "redis://127.0.0.1:1"
security:
  signing_secret: "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0="
  credential_kek: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
server:
  single_listener: {single_listener}
  max_body_bytes: {max_body_bytes}
"#
        );
        let config = oag_core::config::Config::from_yaml(&src).expect("test config");
        let db = oag_store::Db::connect(&config.database.url, 1).expect("lazy pool");
        let cache = oag_store::Cache::connect(&config.redis.url).expect("lazy client");
        Arc::new(AppState::new(config, db, cache).expect("state"))
    }

    /// Every mutating and reading path under /admin/api.
    ///
    /// Hardcoded rather than derived from the router: a list generated from the
    /// thing under test would pass even if every route disappeared.
    const ADMIN_API: &[(&str, &str)] = &[
        ("GET", "/admin/api/summary"),
        ("GET", "/admin/api/accounts"),
        ("GET", "/admin/api/routes"),
        ("GET", "/admin/api/usage"),
        ("GET", "/admin/api/keys"),
        ("POST", "/admin/api/catalog/reload"),
        (
            "POST",
            "/admin/api/accounts/00000000-0000-0000-0000-000000000001/disable",
        ),
        (
            "POST",
            "/admin/api/accounts/00000000-0000-0000-0000-000000000001/enable",
        ),
        (
            "POST",
            "/admin/api/accounts/00000000-0000-0000-0000-000000000001/clear-cooldown",
        ),
        (
            "POST",
            "/admin/api/keys/00000000-0000-0000-0000-000000000001/revoke",
        ),
        ("GET", "/admin/api/services"),
        ("POST", "/admin/api/services"),
        (
            "PATCH",
            "/admin/api/services/00000000-0000-0000-0000-000000000001",
        ),
        (
            "POST",
            "/admin/api/services/00000000-0000-0000-0000-000000000001/disable",
        ),
        (
            "POST",
            "/admin/api/services/00000000-0000-0000-0000-000000000001/enable",
        ),
        (
            "POST",
            "/admin/api/services/00000000-0000-0000-0000-000000000001/check",
        ),
    ];

    async fn status(router: Router, method: &str, path: &str) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .expect("request");
        router.oneshot(request).await.expect("response").status()
    }

    async fn post_status(router: Router, path: &str, body: &'static str) -> StatusCode {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .expect("request");
        router.oneshot(request).await.expect("response").status()
    }

    /// Valid JSON that is not an object.
    ///
    /// The first four *panicked* before the previous change: `gemini_generate`
    /// re-expressed the path's model and mode as body fields by assigning
    /// `wire["__oag_model"]`, and `IndexMut` on a `serde_json::Value` panics for
    /// anything that is not an object or null. `null` did not panic — `IndexMut`
    /// silently turns it into an object — but it is not a request either.
    ///
    /// None of them reach the parse any more; the layer below answers first.
    /// `gateway::require_object_body` owns that assertion now, as a unit test
    /// that needs no key.
    const NON_OBJECT_BODIES: [&str; 5] = ["[]", "123", "\"x\"", "true", "null"];

    #[tokio::test]
    async fn a_public_post_is_answered_from_its_headers_before_its_body_is_read() {
        // The finding. Every handler took `axum::body::Bytes`, and an extractor
        // runs before the handler — so a caller with no credential at all had
        // `server.max_body_bytes` buffered on its behalf before anything looked
        // at a key. That was 256 MiB against a 1 Gi replica with no
        // unauthenticated rate limit in front of it.
        //
        // These bodies used to come back 400, from a shape check the Gemini
        // handler ran on the buffered bytes. 401 now, because nothing looks at
        // the body until a key has resolved.
        for body in NON_OBJECT_BODIES {
            let got = post_status(
                public_router(state(false)),
                "/v1beta/models/gemini-2.5-pro:generateContent",
                body,
            )
            .await;
            assert_eq!(
                got,
                StatusCode::UNAUTHORIZED,
                "a body of {body} was inspected before the key was"
            );
        }
    }

    #[tokio::test]
    async fn an_oversized_anonymous_post_is_401_rather_than_413() {
        // The sharpest form of the same assertion. 413 is the body extractor
        // talking: to know the body was too large, it had to be reading it. A
        // caller with no key must never get that far, whatever it claims to be
        // sending — so the answer is 401, decided from the head alone.
        let router = public_router(state_with(false, 1024));
        let request = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from("x".repeat(64 * 1024)))
            .expect("request");
        let got = router.oneshot(request).await.expect("response").status();
        assert_eq!(
            got,
            StatusCode::UNAUTHORIZED,
            "the body was measured before the key was read"
        );
    }

    #[tokio::test]
    async fn every_public_post_refuses_an_anonymous_caller() {
        // One layer over the whole inference router rather than a call at the
        // top of each handler, for the same reason the admin API has one: with
        // per-handler checks this could only cover the handlers someone
        // remembered to write a check into.
        const POSTS: [&str; 7] = [
            "/v1/messages",
            "/v1/messages/count_tokens",
            "/v1/chat/completions",
            "/chat/completions",
            "/v1/responses",
            "/responses",
            "/v1beta/models/gemini-2.5-pro:generateContent",
        ];
        for path in POSTS {
            let got =
                post_status(public_router(state(false)), path, r#"{"model":"oag/auto"}"#).await;
            assert_eq!(got, StatusCode::UNAUTHORIZED, "{path} answered {got}");
        }
    }

    #[tokio::test]
    async fn an_empty_body_is_still_refused_on_the_key() {
        // The other half of "authentication runs from the headers": with no
        // body at all there is nothing to buffer, and the answer must still be
        // the same 401 rather than whatever the parse would have said.
        let request = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .body(Body::empty())
            .expect("request");
        let got = public_router(state(false))
            .oneshot(request)
            .await
            .expect("response")
            .status();
        assert_eq!(got, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn liveness_stays_outside_the_key_layer() {
        // The layer is on the inference routes, not on the public router: an
        // orchestrator probing liveness holds no API key, and a replica that
        // answered 401 here would be restarted forever.
        //
        // A key that is *present but wrong* is deliberately not asserted
        // anywhere in this module — resolving one reaches Postgres, whose pool
        // has a ten-second acquire timeout, so it would cost a real stall and
        // depend on a local database existing.
        let got = status(public_router(state(false)), "GET", "/health/live").await;
        assert_eq!(got, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_unmatched_inference_path_is_404_without_touching_the_backend() {
        // `route_layer`, not `layer`: authenticating an unroutable path costs a
        // cache lookup and possibly a database round trip for a request that
        // was never going to be served.
        let got = post_status(public_router(state(false)), "/v1/nonexistent", "{}").await;
        assert_eq!(got, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_panicking_handler_answers_500_rather_than_dropping_the_connection() {
        // The insurance behind the fix above. `clippy::panic` is denied, which
        // covers the panics this workspace writes and says nothing about an
        // `IndexMut`, a slice index, or an overflow in a dependency. Both public
        // routers carry this layer; the route here is synthetic because, after
        // the fix, there is no longer a request that panics.
        async fn boom() -> StatusCode {
            panic!("boom")
        }

        let router: Router = Router::new().route("/boom", get(boom)).layer(catch_panic());

        assert_eq!(
            status(router, "GET", "/boom").await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn every_admin_api_route_rejects_an_anonymous_request() {
        // The point of the layer. With per-handler checks this test could only
        // ever cover the handlers someone remembered to write a check into;
        // here a new route is covered by construction, and adding one to the
        // wrong function is what this catches.
        for (method, path) in ADMIN_API {
            let got = status(admin_router(state(false)), method, path).await;
            assert_eq!(
                got,
                StatusCode::UNAUTHORIZED,
                "{method} {path} answered {got} without a key"
            );
        }
    }

    #[tokio::test]
    async fn the_dashboard_and_metrics_stay_open() {
        // Deliberately outside the layer: the page has to load before anyone
        // can type a key into it, and /metrics is scraped without one.
        //
        // /health/ready is excluded on purpose — it pings Postgres, whose pool
        // has a ten-second acquire timeout, so asserting on it here would cost
        // a real stall and depend on whether a local Postgres happens to exist.
        for path in ["/", "/metrics"] {
            let got = status(admin_router(state(false)), "GET", path).await;
            assert_ne!(got, StatusCode::UNAUTHORIZED, "{path} must not need a key");
        }
    }

    #[tokio::test]
    async fn admin_writes_are_absent_from_the_public_listener_by_default() {
        // 404, not 401: with two listeners the admin surface is not merely
        // guarded on the inference port, it is not routed there at all.
        let got = status(
            public_router(state(false)),
            "POST",
            "/admin/api/accounts/00000000-0000-0000-0000-000000000001/disable",
        )
        .await;
        assert_eq!(got, StatusCode::NOT_FOUND);

        // With single_listener they share a port — Cloud Run and Container Apps
        // route to one — and then the key is the only thing in the way.
        let got = status(
            public_router(state(true)),
            "POST",
            "/admin/api/accounts/00000000-0000-0000-0000-000000000001/disable",
        )
        .await;
        assert_eq!(got, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn single_listener_puts_the_dashboard_and_metrics_on_the_public_port_unauthenticated() {
        // The other half of the test above, and the part the deploy comments
        // used to get wrong by saying the merged routes "still require an admin
        // key". `/admin/api` does. These two never have — `/` has to render
        // before an operator can type a key into it and `/metrics` is scraped
        // without one — so merging them onto the public listener publishes them.
        //
        // Asserting the exposure rather than fixing it: the fix is reachability,
        // which is the edge's job. `deploy/caddy/Caddyfile` keeps them off the
        // published vhost and the Helm ingress routes the public port only;
        // Cloud Run and Container Apps route to one port and cannot, which is
        // why their modules point at ingress rules and IAM instead. If this
        // test ever fails, a key was added to one of them and every scraper and
        // orchestrator probe in every deployment just broke.
        for path in ["/", "/metrics"] {
            let got = status(public_router(state(true)), "GET", path).await;
            assert_ne!(
                got,
                StatusCode::NOT_FOUND,
                "{path} is not merged onto the public listener at all"
            );
            assert_ne!(
                got,
                StatusCode::UNAUTHORIZED,
                "{path} now wants a key; the scrapers and probes that hold none are broken"
            );
        }

        // And with two listeners they are not on the public port to begin with.
        for path in ["/", "/metrics"] {
            let got = status(public_router(state(false)), "GET", path).await;
            assert_eq!(got, StatusCode::NOT_FOUND, "{path} leaked onto inference");
        }
    }

    #[tokio::test]
    async fn client_discovery_routes_require_a_key_too() {
        // /v1/models reports the org's provider inventory and this route's
        // entitlements. It is not public information.
        for path in ["/v1/models", "/models", "/v1beta/models"] {
            let got = status(public_router(state(false)), "GET", path).await;
            assert_eq!(got, StatusCode::UNAUTHORIZED, "{path} leaked without a key");
        }
    }
}
