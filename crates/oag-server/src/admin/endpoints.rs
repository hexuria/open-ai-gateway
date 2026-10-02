//! Endpoints: the upstreams an operator registers.
//!
//! A write is validated exactly as `oag admin endpoint` validates one, because
//! both go through [`crate::endpoints`]. A write that lands reloads this
//! replica's endpoints and catalog at once, as renaming a model does, so the
//! page that made it sees it; the other replicas pick it up on their refresh
//! interval. The keys stay where every key is, sealed in `account` rows: no
//! handler here reads or returns one.

use super::auth::AdminActor;
use super::{failed, invalid, not_found};
use crate::AppState;
use crate::endpoints::{self, Checked, Draft, WriteError};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use oag_core::provider::Platform;
use oag_store::EndpointRow;
use oag_store::repo::{self, EndpointDeletion, EndpointReferences};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// What every write's answer says about the other replicas.
const THIS_REPLICA: &str =
    "this replica serves the change now; the others pick it up on their catalog refresh interval";

/// The body that registers an endpoint. The columns, as `oag admin endpoint
/// add` takes them; `auth` defaults to the one style the platform takes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointInput {
    pub name: String,
    pub dialect: String,
    pub platform: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub auth: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    /// Where a `system_one` endpoint takes a question set, beneath its base
    /// URL; `/v1/systemone`, Jev's own, when left out. Only a `system_one`
    /// endpoint has one.
    #[serde(default)]
    pub path: Option<String>,
    /// Header names to values, sent on every request. Never a key.
    #[serde(default)]
    pub extra_headers: Option<Map<String, Value>>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub discover_models: bool,
}

impl EndpointInput {
    fn into_draft(self) -> Draft {
        let auth = self.auth.unwrap_or_else(|| {
            // An unknown platform gets no default; the platform's own refusal
            // comes first and names it.
            self.platform
                .parse::<Platform>()
                .map(|platform| endpoints::default_auth(platform).as_str().to_owned())
                .unwrap_or_default()
        });
        Draft {
            name: self.name,
            dialect: self.dialect,
            platform: self.platform,
            base_url: self.base_url,
            auth,
            region: self.region,
            project: self.project,
            api_version: self.api_version,
            path: self.path,
            extra_headers: Value::Object(self.extra_headers.unwrap_or_default()),
            display_name: self.display_name,
            discover_models: self.discover_models,
        }
    }
}

/// The body that changes an endpoint's settings. A field left out is left
/// alone; `null` clears one that may be empty.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointPatch {
    /// Refused with [`endpoints::FIXED`], whatever it says: these three are
    /// what the endpoint is.
    #[serde(default)]
    pub name: Option<Value>,
    #[serde(default)]
    pub dialect: Option<Value>,
    #[serde(default)]
    pub platform: Option<Value>,
    #[serde(default, deserialize_with = "sent")]
    pub base_url: Option<Option<String>>,
    #[serde(default)]
    pub auth: Option<String>,
    #[serde(default, deserialize_with = "sent")]
    pub region: Option<Option<String>>,
    #[serde(default, deserialize_with = "sent")]
    pub project: Option<Option<String>>,
    #[serde(default, deserialize_with = "sent")]
    pub api_version: Option<Option<String>>,
    /// A `system_one` endpoint's path; `null` goes back to `/v1/systemone`.
    #[serde(default, deserialize_with = "sent")]
    pub path: Option<Option<String>>,
    /// The whole set, replacing the one stored; `null` removes them all.
    #[serde(default, deserialize_with = "sent")]
    pub extra_headers: Option<Option<Map<String, Value>>>,
    #[serde(default, deserialize_with = "sent")]
    pub display_name: Option<Option<String>>,
    #[serde(default)]
    pub discover_models: Option<bool>,
}

impl EndpointPatch {
    /// Whether this names the endpoint's name, dialect or platform.
    fn moves(&self) -> bool {
        self.name.is_some() || self.dialect.is_some() || self.platform.is_some()
    }

    fn apply(self, draft: &mut Draft) {
        if let Some(base_url) = self.base_url {
            draft.base_url = base_url;
        }
        if let Some(auth) = self.auth {
            draft.auth = auth;
        }
        if let Some(region) = self.region {
            draft.region = region;
        }
        if let Some(project) = self.project {
            draft.project = project;
        }
        if let Some(api_version) = self.api_version {
            draft.api_version = api_version;
        }
        if let Some(path) = self.path {
            draft.path = path;
        }
        if let Some(headers) = self.extra_headers {
            draft.extra_headers = Value::Object(headers.unwrap_or_default());
        }
        if let Some(display_name) = self.display_name {
            draft.display_name = display_name;
        }
        if let Some(discover_models) = self.discover_models {
            draft.discover_models = discover_models;
        }
    }
}

/// A field that was sent, even as `null`, is `Some`; one left out stays
/// `None` through `#[serde(default)]`.
// The two layers are the two questions a PATCH asks of a field that may be
// empty: was it sent, and if so, is it being cleared.
#[allow(clippy::option_option)]
fn sent<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// One endpoint as the console shows it: its row, what names it, and whether
/// this build serves it.
#[derive(Debug, Serialize)]
pub struct EndpointView {
    pub name: String,
    pub display_name: Option<String>,
    pub dialect: String,
    pub platform: String,
    pub base_url: Option<String>,
    pub auth: String,
    pub region: Option<String>,
    pub project: Option<String>,
    pub api_version: Option<String>,
    /// A `system_one` endpoint's path; `None` is `/v1/systemone`.
    pub path: Option<String>,
    /// Every header name and value, in stored order, except a value whose
    /// name suggests a secret, which is [`endpoints::REDACTED`].
    pub extra_headers: Map<String, Value>,
    pub discover_models: bool,
    pub accounts: i64,
    pub schedulable_accounts: i64,
    pub models: i64,
    pub on_ladder: i64,
    /// Whether this build serves the row. When it does not, `problem` says why,
    /// and its credentials and models serve nothing until that changes.
    pub served: bool,
    pub problem: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn view(row: &EndpointRow, refs: EndpointReferences) -> EndpointView {
    let problem = endpoints::refusal(row).map(|refusal| refusal.message);
    EndpointView {
        name: row.name.clone(),
        display_name: row.display_name.clone(),
        dialect: row.dialect.clone(),
        platform: row.platform.clone(),
        base_url: row.base_url.clone(),
        auth: row.auth.clone(),
        region: row.region.clone(),
        project: row.project.clone(),
        api_version: row.api_version.clone(),
        path: row.path.clone(),
        extra_headers: endpoints::shown_headers(&row.extra_headers)
            .into_iter()
            .map(|(name, value)| (name, Value::String(value)))
            .collect(),
        discover_models: row.discover_models,
        accounts: refs.accounts,
        schedulable_accounts: refs.schedulable,
        models: refs.models,
        on_ladder: refs.on_ladder,
        served: problem.is_none(),
        problem,
        created_at: rfc3339(row.created_at),
        updated_at: rfc3339(row.updated_at),
    }
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// `GET /admin/api/endpoints`.
pub async fn list_endpoints(State(state): State<Arc<AppState>>) -> Response {
    let rows = match repo::list_endpoints(&state.db).await {
        Ok(rows) => rows,
        Err(e) => return failed(&e),
    };
    let refs = match repo::endpoint_references(&state.db).await {
        Ok(refs) => refs,
        Err(e) => return failed(&e),
    };
    Json(
        rows.iter()
            .map(|row| view(row, refs.get(&row.name).copied().unwrap_or_default()))
            .collect::<Vec<_>>(),
    )
    .into_response()
}

/// `GET /admin/api/endpoints/{name}`.
pub async fn get_endpoint(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    match repo::get_endpoint(&state.db, &name).await {
        Ok(Some(row)) => answer(&state, &row, StatusCode::OK, "read").await,
        Ok(None) => not_found("no endpoint with that name"),
        Err(e) => failed(&e),
    }
}

/// `POST /admin/api/endpoints`: 201 with the endpoint, 400 naming the rule a
/// body breaks, 409 when the name is taken.
pub async fn create_endpoint(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Json(input): Json<EndpointInput>,
) -> Response {
    match endpoints::register(&state.db, input.into_draft()).await {
        Ok(row) => {
            audit(&actor, "endpoint.create", &row.name);
            reload(&state).await;
            answer(&state, &row, StatusCode::CREATED, "written").await
        }
        Err(e) => write_failed(e),
    }
}

/// `PATCH /admin/api/endpoints/{name}`: the settings, never the dialect or
/// the platform, which are refused with a 400 before anything is read.
///
/// The patch is applied to the row as this request read it, and written only
/// over that row: a write that landed in between is a 409, and nothing is
/// written, rather than lost under a field this request copied unchanged.
pub async fn update_endpoint(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(name): Path<String>,
    Json(patch): Json<EndpointPatch>,
) -> Response {
    if patch.moves() {
        return invalid(endpoints::FIXED);
    }
    let stored = match repo::get_endpoint(&state.db, &name).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found("no endpoint with that name"),
        Err(e) => return failed(&e),
    };
    let mut draft = Draft::from_row(&stored);
    patch.apply(&mut draft);
    match endpoints::change(&state.db, &stored, draft).await {
        Ok(row) => {
            audit(&actor, "endpoint.update", &row.name);
            reload(&state).await;
            answer(&state, &row, StatusCode::OK, "written").await
        }
        Err(e) => write_failed(e),
    }
}

/// `DELETE /admin/api/endpoints/{name}`: 409 with the counts while a
/// credential or a catalog model still names it.
pub async fn delete_endpoint(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(name): Path<String>,
) -> Response {
    match repo::delete_endpoint(&state.db, &name).await {
        Ok(EndpointDeletion::Deleted) => {
            audit(&actor, "endpoint.delete", &name);
            reload(&state).await;
            Json(json!({ "name": name, "deleted": true, "note": THIS_REPLICA })).into_response()
        }
        Ok(EndpointDeletion::NotFound) => not_found("no endpoint with that name"),
        Ok(EndpointDeletion::InUse { accounts, models }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "endpoint {name} is still named by {accounts} credential(s) and {models} \
                     catalog model(s), so nothing was removed"
                ),
                "accounts": accounts,
                "models": models,
                "hint": "remove its credentials and its catalog models first, and take its \
                         models off every ladder",
            })),
        )
            .into_response(),
        Err(e) => failed(&e),
    }
}

/// `POST /admin/api/endpoints/{name}/check`: ask the endpoint for its model
/// list, with no key. 200 whatever it answered, with what it answered.
pub async fn check_endpoint(
    State(state): State<Arc<AppState>>,
    actor: AdminActor,
    Path(name): Path<String>,
) -> Response {
    let row = match repo::get_endpoint(&state.db, &name).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found("no endpoint with that name"),
        Err(e) => return failed(&e),
    };
    audit(&actor, "endpoint.check", &row.name);
    let checked = endpoints::check(&row, None, None).await;
    Json(CheckView {
        name: row.name,
        ok: checked.ok(),
        checked,
    })
    .into_response()
}

/// A check's answer: which endpoint, whether it listed its models, and what
/// was found.
#[derive(Debug, Serialize)]
struct CheckView {
    name: String,
    ok: bool,
    #[serde(flatten)]
    checked: Checked,
}

/// `row` with its counts, as `status`. The counts are read again rather than
/// assumed: an endpoint registered under a name a credential already carried
/// starts with that credential.
async fn answer(state: &AppState, row: &EndpointRow, status: StatusCode, what: &str) -> Response {
    match repo::endpoint_references(&state.db).await {
        Ok(refs) => (
            status,
            Json(view(row, refs.get(&row.name).copied().unwrap_or_default())),
        )
            .into_response(),
        Err(e) => failed(&oag_core::Error::Internal(format!(
            "the endpoint was {what}, but reading what names it failed, so this answer cannot \
             count them: {e}"
        ))),
    }
}

/// Load the endpoints and the catalog on this replica now. A failure is
/// logged, not returned: the write has happened, and the refresh interval
/// loads it anyway.
async fn reload(state: &AppState) {
    if let Err(e) = state.reload_catalog().await {
        tracing::warn!(error = %e, "wrote an endpoint but could not reload the catalog");
    }
}

fn write_failed(e: WriteError) -> Response {
    match e {
        WriteError::Invalid(message) => invalid(&message),
        WriteError::Taken(message) => {
            (StatusCode::CONFLICT, Json(json!({ "error": message }))).into_response()
        }
        WriteError::NotFound => not_found("no endpoint with that name"),
        WriteError::Changed => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "the endpoint changed since this request read it, so nothing was \
                          written",
                "hint": "read it again, and send the change again if it is still wanted",
            })),
        )
            .into_response(),
        WriteError::Failed(e) => failed(&e),
    }
}

/// The audit line every admin write emits, with the endpoint's name as its
/// subject.
///
/// `warn!` for the reason [`super::write`] gives: tightening the log filter
/// must not erase the trail of who changed which upstream.
fn audit(actor: &AdminActor, action: &str, subject: &str) {
    tracing::warn!(
        target: "oag::audit",
        actor = %actor.email,
        actor_id = %actor.principal_id,
        action,
        subject,
        "admin write"
    );
}

#[cfg(test)]
mod tests;
