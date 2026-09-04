//! The capability-service catalog.
//!
//! Incident writes stay in [`super::write`]: disable a credential, revoke a
//! key. This module is the other kind of admin mutation — a row that says
//! "this organisation also runs *that* service". The gateway does not grow
//! a sandbox or a guardrail by absorbing one; it registers a URL, probes
//! `base_url + health_path`, and deep-links to the service's own dashboard.
//!
//! These handlers have request bodies because a catalog row has fields. The
//! disable/enable/check verbs do not.

use super::auth::AdminActor;
use super::{failed, invalid, not_found};
use crate::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use oag_core::{ServiceKind, catalog_url, health_url, ip_is_denied};
use oag_store::ServiceRow;
use oag_store::repo::{self, NewService, ServiceUpdate};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct ServiceInput {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub health_path: String,
    #[serde(default)]
    pub dashboard_url: Option<String>,
    #[serde(default)]
    pub auth_ref: Option<Uuid>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug)]
struct Validated {
    name: String,
    kind: ServiceKind,
    base_url: String,
    health_path: String,
    dashboard_url: Option<String>,
    auth_ref: Option<Uuid>,
    enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct ServiceView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub health_path: String,
    pub dashboard_url: Option<String>,
    pub auth_ref: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub last_ok: Option<String>,
    pub last_error: Option<String>,
    /// Derived: `ok`, `error`, `unknown`, or `disabled`.
    pub health: &'static str,
}

pub async fn list_services(State(state): State<Arc<AppState>>) -> Response {
    match repo::list_services(&state.db).await {
        Ok(rows) => Json(rows.iter().map(view).collect::<Vec<_>>()).into_response(),
        Err(e) => failed(&e),
    }
}

pub async fn create_service(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Json(input): Json<ServiceInput>,
) -> Response {
    let validated = match validate(&input, true) {
        Ok(v) => v,
        Err(e) => return invalid_err(&e),
    };
    let id = Uuid::now_v7();
    let row = match repo::insert_service(
        &state.db,
        &NewService {
            id,
            name: &validated.name,
            kind: validated.kind.as_str(),
            base_url: &validated.base_url,
            health_path: &validated.health_path,
            dashboard_url: validated.dashboard_url.as_deref(),
            auth_ref: validated.auth_ref,
        },
    )
    .await
    {
        Ok(row) => row,
        Err(e) => return write_failed(&e),
    };
    audit(&actor, "service.create", id, &row.name);
    let row = probe_and_record(&state, row).await;
    (StatusCode::CREATED, Json(view(&row))).into_response()
}

pub async fn update_service(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(id): Path<Uuid>,
    Json(input): Json<ServiceInput>,
) -> Response {
    let existing = match repo::service_by_id(&state.db, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found("no service with that id"),
        Err(e) => return failed(&e),
    };
    let validated = match validate(&input, existing.enabled) {
        Ok(v) => v,
        Err(e) => return invalid_err(&e),
    };
    let row = match repo::update_service(
        &state.db,
        id,
        &ServiceUpdate {
            name: &validated.name,
            kind: validated.kind.as_str(),
            base_url: &validated.base_url,
            health_path: &validated.health_path,
            dashboard_url: validated.dashboard_url.as_deref(),
            auth_ref: validated.auth_ref,
            enabled: validated.enabled,
        },
    )
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return not_found("no service with that id"),
        Err(e) => return write_failed(&e),
    };
    audit(&actor, "service.update", id, &row.name);
    let row = probe_and_record(&state, row).await;
    Json(view(&row)).into_response()
}

pub async fn disable_service(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(id): Path<Uuid>,
) -> Response {
    set_enabled(&state, &actor, id, false).await
}

pub async fn enable_service(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(id): Path<Uuid>,
) -> Response {
    set_enabled(&state, &actor, id, true).await
}

pub async fn check_service(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(id): Path<Uuid>,
) -> Response {
    let Some(row) = (match repo::service_by_id(&state.db, id).await {
        Ok(row) => row,
        Err(e) => return failed(&e),
    }) else {
        return not_found("no service with that id");
    };
    audit(&actor, "service.check", id, &row.name);
    let row = probe_and_record(&state, row).await;
    Json(view(&row)).into_response()
}

async fn set_enabled(state: &AppState, actor: &AdminActor, id: Uuid, enabled: bool) -> Response {
    match repo::set_service_enabled(&state.db, id, enabled).await {
        Ok(Some(name)) => {
            let action = if enabled {
                "service.enable"
            } else {
                "service.disable"
            };
            audit(actor, action, id, &name);
            Json(json!({ "id": id, "name": name, "enabled": enabled })).into_response()
        }
        Ok(None) => not_found("no service with that id"),
        Err(e) => failed(&e),
    }
}

fn validate(input: &ServiceInput, default_enabled: bool) -> oag_core::Result<Validated> {
    let name = clean_name(&input.name)?;
    let kind: ServiceKind = input.kind.parse()?;
    let base = catalog_url(&input.base_url)?;
    let path = input.health_path.trim();
    let health = health_url(base.as_str(), path)?;
    let dashboard_url = match input.dashboard_url.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(catalog_url(raw)?.to_string()),
    };
    Ok(Validated {
        name,
        kind,
        // Store the canonical form so a later probe sees the same string
        // the operator was shown.
        base_url: base.to_string(),
        health_path: health.path().to_owned(),
        dashboard_url,
        auth_ref: input.auth_ref,
        enabled: input.enabled.unwrap_or(default_enabled),
    })
}

fn clean_name(name: &str) -> oag_core::Result<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(oag_core::Error::Config(
            "name must be 1-128 characters".to_owned(),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(oag_core::Error::Config(
            "name must not contain control characters".to_owned(),
        ));
    }
    Ok(name.to_owned())
}

async fn probe_and_record(state: &AppState, row: ServiceRow) -> ServiceRow {
    let outcome = probe_health(&row.base_url, &row.health_path).await;
    let (ok, error) = match &outcome {
        Ok(()) => (true, None),
        Err(e) => (false, Some(truncate(e, 500))),
    };
    if let Ok(Some(updated)) =
        repo::record_service_health(&state.db, row.id, ok, error.as_deref()).await
    {
        updated
    } else {
        // The row vanished or the write failed; return what we have so
        // the operator still sees the probe outcome on this response.
        let mut fallback = row;
        if ok {
            fallback.last_error = None;
        } else {
            fallback.last_error = error;
        }
        fallback
    }
}

/// GET `base_url + health_path`. Refuses link-local and metadata targets
/// both as literals (already caught by [`health_url`]) and after a DNS
/// resolution, and does not follow redirects.
///
/// "A" resolution, not "the": the check resolves the name once and the HTTP
/// client then resolves it again for the connection, and nothing pins the
/// second answer to the first. This defeats a misconfigured or careless
/// target; a resolver that deliberately answers differently on consecutive
/// lookups is not what it defends against.
pub(crate) async fn probe_health(base_url: &str, health_path: &str) -> Result<(), String> {
    let target = health_url(base_url, health_path).map_err(|e| e.to_string())?;
    deny_resolved_target(&target).await?;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(concat!(
            "open-ai-gateway/",
            env!("CARGO_PKG_VERSION"),
            " service-health"
        ))
        .build()
        .map_err(|e| format!("health client: {e}"))?;

    let response = client
        .get(target.as_str())
        .send()
        .await
        .map_err(|e| format!("health request failed: {e}"))?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("health returned HTTP {status}"))
    }
}

async fn deny_resolved_target(url: &Url) -> Result<(), String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL is missing a host".to_owned())?;
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let lookup = host.to_owned();
    let resolved = tokio::task::spawn_blocking(move || (lookup.as_str(), port).to_socket_addrs())
        .await
        .map_err(|e| format!("resolving host: {e}"))?
        .map_err(|e| format!("resolving {host}: {e}"))?;

    let mut any = false;
    for addr in resolved {
        any = true;
        if ip_is_denied(addr.ip()) {
            return Err(format!(
                "refusing to probe {host}: it resolves to a link-local or metadata address"
            ));
        }
    }
    if !any {
        return Err(format!("resolving {host}: no addresses"));
    }
    Ok(())
}

fn view(row: &ServiceRow) -> ServiceView {
    ServiceView {
        id: row.id.to_string(),
        name: row.name.clone(),
        kind: row.kind.clone(),
        base_url: row.base_url.clone(),
        health_path: row.health_path.clone(),
        dashboard_url: row.dashboard_url.clone(),
        auth_ref: row.auth_ref.map(|id| id.to_string()),
        enabled: row.enabled,
        created_at: format_time(row.created_at),
        last_ok: row.last_ok.map(format_time),
        last_error: row.last_error.clone(),
        health: if !row.enabled {
            "disabled"
        } else if row.last_error.is_some() {
            "error"
        } else if row.last_ok.is_some() {
            "ok"
        } else {
            "unknown"
        },
    }
}

fn format_time(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect()
}

fn invalid_err(e: &oag_core::Error) -> Response {
    match e {
        oag_core::Error::Config(msg) => invalid(msg),
        other => invalid(&other.to_string()),
    }
}

fn write_failed(e: &oag_core::Error) -> Response {
    match e {
        oag_core::Error::Config(msg) if msg.contains("already exists") => {
            (StatusCode::CONFLICT, Json(json!({ "error": msg }))).into_response()
        }
        oag_core::Error::Config(msg) => invalid(msg),
        _ => failed(e),
    }
}

fn audit(actor: &AdminActor, action: &str, subject: Uuid, name: &str) {
    tracing::warn!(
        target: "oag::audit",
        actor = %actor.email,
        actor_id = %actor.principal_id,
        action,
        %subject,
        name,
        "admin write"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use tokio::net::TcpListener;

    async fn mock_health_server() -> String {
        let app = Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/down", get(|| async { StatusCode::SERVICE_UNAVAILABLE }))
            .route(
                "/redir",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [(axum::http::header::LOCATION, "http://169.254.169.254/")],
                    )
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    #[tokio::test]
    async fn a_healthy_mock_server_probes_ok() {
        let base = mock_health_server().await;
        probe_health(&base, "/health").await.expect("healthy");
    }

    #[tokio::test]
    async fn an_unhealthy_mock_server_records_the_status() {
        let base = mock_health_server().await;
        let err = probe_health(&base, "/down").await.expect_err("down");
        assert!(
            err.contains("503"),
            "failure should name the status, got {err}"
        );
    }

    #[tokio::test]
    async fn redirects_are_not_followed_to_metadata() {
        // A 302 to the metadata well-known is how a naive health check
        // becomes SSRF. We refuse to follow redirects at all.
        let base = mock_health_server().await;
        let err = probe_health(&base, "/redir").await.expect_err("redirect");
        assert!(
            err.contains("302") || err.contains("FOUND") || err.contains("Found"),
            "redirect must surface as a failed probe, not a follow, got {err}"
        );
    }

    #[tokio::test]
    async fn a_metadata_literal_is_refused_before_any_request() {
        let err = probe_health("http://169.254.169.254", "/latest/meta-data")
            .await
            .expect_err("metadata");
        assert!(
            err.contains("link-local") || err.contains("metadata"),
            "{err}"
        );
    }

    #[test]
    fn validation_rejects_a_bad_kind_and_a_bad_url() {
        let bad_kind = validate(
            &ServiceInput {
                name: "orgo".into(),
                kind: "firecracker".into(),
                base_url: "https://orgo.example.invalid".into(),
                health_path: "/health".into(),
                dashboard_url: None,
                auth_ref: None,
                enabled: None,
            },
            true,
        )
        .unwrap_err();
        assert!(bad_kind.to_string().contains("kind"));

        let bad_url = validate(
            &ServiceInput {
                name: "orgo".into(),
                kind: "sandbox".into(),
                base_url: "file:///etc/passwd".into(),
                health_path: "/health".into(),
                dashboard_url: None,
                auth_ref: None,
                enabled: None,
            },
            true,
        )
        .unwrap_err();
        assert!(bad_url.to_string().contains("http"));
    }

    #[test]
    fn validation_accepts_a_minimal_honest_row() {
        let v = validate(
            &ServiceInput {
                name: "  berthos  ".into(),
                kind: "sandbox".into(),
                base_url: "http://127.0.0.1:9090".into(),
                health_path: "/health".into(),
                dashboard_url: Some("http://127.0.0.1:9090/ui".into()),
                auth_ref: None,
                enabled: None,
            },
            true,
        )
        .unwrap();
        assert_eq!(v.name, "berthos");
        assert_eq!(v.kind, ServiceKind::Sandbox);
        assert_eq!(v.health_path, "/health");
        assert!(v.enabled);
    }
}
