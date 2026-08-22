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
pub mod metrics;
pub mod shutdown;
pub mod state;

pub use shutdown::Lifecycle;
pub use state::AppState;

use axum::Router;
use axum::routing::get;
use oag_core::Result;
use std::sync::Arc;

/// Inference. Fronted by the load balancer.
pub fn public_router(state: Arc<AppState>) -> Router {
    Router::new()
        // Liveness only. Readiness lives on the admin listener because it is an
        // operational detail, and because answering it does not require being
        // reachable from the internet.
        .route("/health/live", get(health::live))
        .route("/v1/messages", axum::routing::post(gateway::messages))
        .route(
            "/v1/chat/completions",
            axum::routing::post(gateway::chat_completions),
        )
        // The same surface without the version prefix; several SDKs default
        // to it when given a custom base URL.
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
        .layer(axum::extract::DefaultBodyLimit::max(
            state.config.server.max_body_bytes,
        ))
        .with_state(state)
}

/// Admin API, metrics, readiness. Internal network only.
pub fn admin_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health/live", get(health::live))
        .route("/health/ready", get(health::ready))
        .route("/metrics", get(metrics::render))
        .route("/", get(admin::dashboard))
        .route("/admin/api/summary", get(admin::summary))
        .route("/admin/api/accounts", get(admin::accounts))
        .route("/admin/api/routes", get(admin::routes))
        .route("/admin/api/usage", get(admin::usage))
        .route(
            "/admin/api/catalog/reload",
            axum::routing::post(admin::reload_catalog),
        )
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

/// Bind both listeners and serve until shutdown.
pub async fn serve(state: Arc<AppState>) -> Result<()> {
    let public_addr = state.config.server.public_addr.clone();
    let admin_addr = state.config.server.admin_addr.clone();

    let public = tokio::net::TcpListener::bind(&public_addr)
        .await
        .map_err(|e| oag_core::Error::Internal(format!("binding {public_addr}: {e}")))?;
    let admin = tokio::net::TcpListener::bind(&admin_addr)
        .await
        .map_err(|e| oag_core::Error::Internal(format!("binding {admin_addr}: {e}")))?;

    spawn_catalog_refresh(Arc::clone(&state));

    tracing::info!(%public_addr, %admin_addr, "listening");

    let lifecycle = Arc::clone(&state.lifecycle);
    let drain = state.config.gateway.max_stream_duration;

    let public_srv = axum::serve(public, public_router(Arc::clone(&state)))
        .with_graceful_shutdown(shutdown::signal(Arc::clone(&lifecycle), drain));
    let admin_srv = axum::serve(admin, admin_router(state))
        .with_graceful_shutdown(shutdown::signal(lifecycle, drain));

    let (a, b) = tokio::join!(public_srv, admin_srv);
    a.map_err(|e| oag_core::Error::Internal(format!("public listener: {e}")))?;
    b.map_err(|e| oag_core::Error::Internal(format!("admin listener: {e}")))?;
    Ok(())
}
