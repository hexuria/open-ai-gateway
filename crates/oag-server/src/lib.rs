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
        .with_state(state)
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
