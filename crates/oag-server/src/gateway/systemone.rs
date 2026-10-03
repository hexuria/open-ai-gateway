//! System One: Jev's question-answering API, served beside the chat surfaces.
//!
//! `POST /jev/v1/systemone` takes a `state` and named `questions` and answers
//! each with a typed answer and its confidence. It is not a conversation, and
//! none of the chat pipeline applies to it: no canonical form, no ladder, no
//! escalation, no stream. Only a System One upstream can answer it — the
//! built-in Jev, or a System One endpoint an operator registered, such as Merge
//! Gateway's Decisions API — so there is no fallback either: a route without a
//! credential for the one a request names refuses, and says so.
//!
//! Which one that is, the request's model says ([`resolve`]): a System One
//! catalog id is its row's provider, `<endpoint>/<name>` is that endpoint's
//! once a catalog row prices it, and a model named by no provider, or by none
//! at all, is Jev's, as it always was.
//!
//! What it does share with the chat path is everything about the credential.
//! The caller's key, rate limit and spend caps admit it; an account of that
//! provider on the caller's route is leased through [`select::lease`], so seat
//! ownership, concurrency slots and the sticky pin apply; the per-credential
//! breaker, same-credential retries and failover follow the chat path's rules
//! through its own helpers; and every answer is a ledger row.
//!
//! The bytes are the SDK's. The request is checked against
//! [`SystemOneRequest`] and forwarded as it arrived, but for the model, which
//! an upstream is sent by its own name for it; the answer is checked against
//! [`SystemOneResponse`] and returned as it arrived. Mounted under `/jev`, so
//! the unmodified `typesafe_sdk::Client` works with
//! `base_url = https://<gateway>/jev`.

use super::climb::meter_context;
use super::failover::{
    Step, apply_disposition, backoff, may_try_another, step_for, transport_failure,
    upstream_retry_after,
};
use super::respond::{identity_headers, truncate};
use super::{Caller, budgets_for, error_response, meter, refresh, select};
use crate::AppState;
use crate::breakers::Dispatch;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use oag_core::credential::SecretMaterial;
use oag_core::{AccountId, Error, Provider, RequestId, Result};
use oag_pool::SessionKey;
use oag_router::{
    BudgetPressure, Capabilities, Catalog, ModelId, ModelSpec, Pricing, RoutingDecision,
    SelectionReason,
};
use oag_upstream::jev::{ListingPage, MAX_LISTING_PAGES, listing_page};
use oag_upstream::{JevUpstream, Transport as _, TransportKey};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use typesafe_sdk::wire::{ListModelsResponse, ModelMetadata, SystemOneRequest, SystemOneResponse};

/// Where Jev names the request that produced an answer, and where the SDK's
/// `request_id()` reads it. Passed through, so a caller can quote it to Jev's
/// operators; the SDK keeps its own copy of the name private.
const REQUEST_ID_HEADER: &str = "x-typesafe-request-id";

/// `POST /jev/v1/systemone`.
///
/// [`Caller`] before `Bytes`, as on every inference route: an unauthenticated
/// request is refused on its head, before its body is buffered.
pub async fn system_one(
    State(state): State<Arc<AppState>>,
    Caller(auth): Caller,
    body: axum::body::Bytes,
) -> Response {
    // Moved into the ledger write, which outlives the response: a shutdown
    // drain waits for the row as it does for a chat answer's.
    let guard = state.lifecycle.track();
    answered(ask(&state, &auth, body, RequestId::new(), guard).await)
}

/// `GET /jev/v1/models`: what the route's System One credentials list.
///
/// The upstreams', not the catalog's. What a key can ask is its host's to say,
/// and a caller reading this is asking exactly that. A route whose only System
/// One credentials are Jev's gets Jev's listing as it arrived; see [`list`] for
/// one that holds an endpoint's too.
pub async fn models(State(state): State<Arc<AppState>>, Caller(auth): Caller) -> Response {
    answered(list(&state, &auth, RequestId::new()).await)
}

fn answered(result: Result<Response>) -> Response {
    result.unwrap_or_else(|e| {
        metrics::counter!("oag_requests_total", "outcome" => "error").increment(1);
        error_response(&e)
    })
}

async fn ask(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    body: axum::body::Bytes,
    request_id: RequestId,
    guard: crate::shutdown::InFlightGuard,
) -> Result<Response> {
    let started = Instant::now();
    // The SDK's own decoder is the check. A body the client would refuse to
    // send is a body this refuses to forward, as a 400 that costs no credential
    // and no upstream call: not an object, no `state`, no question, a question
    // of no shape the client knows — each error names what is wrong. The value
    // is only read; what goes upstream is `body`, as it arrived, or with the
    // one change `renamed` makes.
    let request: SystemOneRequest = serde_json::from_slice(&body)?;
    // One snapshot for the request: the row that chose the provider is the
    // row that prices its answer.
    let catalog = state.system_one_catalog().await;
    let target = resolve(
        request.model.as_deref(),
        &catalog,
        &state.system_one_providers(),
    )?;
    let route = admit(state, auth).await?;
    let body = match &target.upstream_model {
        Some(model) => renamed(&body, model)?,
        None => body,
    };
    tracing::debug!(
        %request_id,
        provider = %target.provider,
        model = ?request.model,
        questions = request.questions.len(),
        "asking System One"
    );

    let answer = call(
        state,
        auth,
        &route,
        target.provider,
        request_id,
        Purpose::Answer,
        |upstream, credential| upstream.system_one(credential, body.clone()),
        decoded::<SystemOneResponse>,
    )
    .await?;

    // The model the upstream says answered, which is what ran, and so what the
    // ledger and `x-oag-model` name — not the one asked for, which may be an
    // alias — unless only the one asked for has a price.
    //
    // Priced from the tokens it reports and the catalog's row, as every answer
    // is. A host that also reports what it charged (Merge Gateway's
    // `usage.cost`) is not believed over the catalog: the ledger has one
    // pricing rule for every provider, and the host's own figure reaches the
    // caller in the body, untouched.
    let response = &answer.reply.decoded;
    let decision = RoutingDecision {
        model: priced(
            &catalog,
            target.provider,
            &response.model,
            target.spec.as_ref(),
        ),
        tier: None,
        reason: SelectionReason::Passthrough,
        capability_escalated_from: None,
        ceiling_model: None,
    };
    let usage = oag_router::Usage {
        input_tokens: tokens(response.usage.input_tokens),
        output_tokens: tokens(response.usage.output_tokens),
        ..oag_router::Usage::default()
    };
    let ctx = meter_context(
        auth,
        &decision,
        &answer.lease,
        request_id,
        started,
        answer.attempt,
    );
    // Before the ledger write, which is ours rather than the credential's.
    answer.lease.release().await;

    // Detached, as the chat path's writes are: a client that hangs up now
    // cancels this future, and the upstream has answered and billed either
    // way.
    let writer = Arc::clone(state);
    tokio::spawn(async move {
        let _guard = guard;
        meter::record_answer(&writer, &ctx, usage).await;
    });

    Ok(relay(
        answer.reply,
        Some(decision.model.id.as_str()),
        request_id,
    ))
}

/// Where a System One request goes.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    /// Whose credential answers it.
    provider: Provider,
    /// The name to send as `model` in place of the one that arrived. `None`
    /// sends the body as it arrived.
    upstream_model: Option<String>,
    /// The catalog's row for the model asked for: what prices an answer whose
    /// model has no row of its own.
    spec: Option<ModelSpec>,
}

/// The System One provider `model` names, and what its upstream calls it.
///
/// In order:
///
/// 1. No model: Jev's, which picks its own.
/// 2. An id in System One's catalog: its row's provider, sent the row's
///    upstream name. `merge/typesafe/jev-1.13` reaches Merge as
///    `typesafe/jev-1.13`, and `jev/jev-latest` reaches Jev as `jev-latest`.
/// 3. `<endpoint>/<name>`, where the endpoint is a System One host in `hosts`
///    and a catalog row of that host's has `<name>` as its upstream name:
///    that endpoint's, sent `<name>`, and priced by that row. A name of a
///    host's that no row prices is refused: the host bills for its answer,
///    and an answer the ledger could not price would be spend nobody sees.
///    Jev's are not, because Jev has always been asked by any name.
/// 4. A name with no provider in it, or Jev's (`jev/…`, `typesafe/…`): Jev's,
///    sent as it arrived, which is all this route did before endpoints.
/// 5. Anything else — a chat provider's model, or an endpoint's that is not
///    served, such as one removed — is refused before any credential is
///    leased, rather than sent to Jev: a question set meant for one host is
///    not sent to another.
///
/// `hosts` are the System One providers this gateway serves; see
/// [`AppState::system_one_providers`].
fn resolve(model: Option<&str>, catalog: &Catalog, hosts: &[Provider]) -> Result<Target> {
    let jev = Target {
        provider: Provider::Jev,
        upstream_model: None,
        spec: None,
    };
    let Some(model) = model else {
        return Ok(jev);
    };
    if let Some(spec) = catalog.get(&ModelId::new(model)) {
        return Ok(Target {
            provider: spec.provider,
            upstream_model: (spec.upstream_name != model).then(|| spec.upstream_name.clone()),
            spec: Some(spec.clone()),
        });
    }
    let Some((prefix, name)) = model.split_once('/') else {
        return Ok(jev);
    };
    if let Some(&host) = hosts
        .iter()
        .find(|host| matches!(host, Provider::Custom(_)) && host.as_str() == prefix)
    {
        let Some(spec) = catalog
            .iter()
            .find(|spec| spec.provider == host && spec.upstream_name == name)
        else {
            return Err(Error::NoViableModel(format!(
                "'{model}' is not in the catalog, so what {prefix} charges for its answer \
                 could not be metered, and it is not sent there. Price it first: oag admin \
                 catalog add --id {model} --upstream {name} --input-per-mtok <usd> \
                 --output-per-mtok <usd> --context <tokens> --max-output <tokens>"
            )));
        };
        return Ok(Target {
            provider: host,
            upstream_model: Some(name.to_owned()),
            spec: Some(spec.clone()),
        });
    }
    if prefix.parse::<Provider>().is_ok_and(|p| p == Provider::Jev) {
        return Ok(jev);
    }
    Err(Error::NoViableModel(format!(
        "'{model}' is not a System One model this gateway serves. GET /jev/v1/models lists \
         the ones it does; a name with no provider in it is sent to Jev as it is"
    )))
}

/// `body` with its `model` naming `model`, and every other member as the
/// caller wrote it.
///
/// Each member's value is copied as the bytes it arrived in, in the order it
/// arrived: a number too long for a float, an escape, a key this gateway has
/// never heard of all reach the upstream as the caller wrote them. Only the
/// space between members is not kept. Every `model` is replaced, so no
/// upstream can read a different one from the one the provider was chosen
/// by — though the SDK's decoder has already refused a body with two.
fn renamed(body: &[u8], model: &str) -> Result<axum::body::Bytes> {
    let Members(members) = serde_json::from_slice(body)?;
    let model = serde_json::to_string(model)?;
    let mut out = Vec::with_capacity(body.len() + model.len());
    out.push(b'{');
    for (i, (key, value)) in members.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut out, key)?;
        out.push(b':');
        let value = if key == "model" {
            model.as_str()
        } else {
            value.get()
        };
        out.extend_from_slice(value.as_bytes());
    }
    out.push(b'}');
    Ok(out.into())
}

/// A JSON object's members, in the order they were written, each value as
/// the bytes it was written in.
struct Members<'a>(Vec<(String, &'a RawValue)>);

impl<'de> Deserialize<'de> for Members<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Object;
        impl<'de> Visitor<'de> for Object {
            type Value = Members<'de>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry::<String, &'de RawValue>()? {
                    members.push(member);
                }
                Ok(Members(members))
            }
        }
        deserializer.deserialize_map(Object)
    }
}

/// `GET /jev/v1/models`, from every System One provider the caller's route
/// holds a credential for.
///
/// A route whose only such credentials are Jev's gets Jev's listing as it
/// arrived, with Jev's request id, as it always has. Once an endpoint is among
/// them, the listing is one the gateway writes, in the SDK's shape: Jev's
/// models by their own names, then each endpoint's by name, its models as
/// `<endpoint>/<name>` — the name [`resolve`] sends to that endpoint.
///
/// A provider the route holds no credential for lists nothing. One that is
/// there and fails is left out, and its failure logged under its name, so one
/// host that cannot list does not stop the caller seeing what every other one
/// serves. Only when every provider there failed does the listing fail, with
/// the first one's error, as Jev's failing always has.
///
/// A listing touches no breaker and no cooldown ([`Purpose::Listing`]): a
/// host's model list failing says nothing about whether its keys answer.
async fn list(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    request_id: RequestId,
) -> Result<Response> {
    let route = admit(state, auth).await?;
    let mut jev = None;
    let mut hosted: Vec<ModelMetadata> = Vec::new();
    let mut hosts_listed = false;
    let mut failed: Option<Error> = None;
    for provider in state.system_one_providers() {
        match listed(state, auth, &route, provider, request_id).await {
            Ok(Listed::Jev(reply)) => jev = Some(reply),
            Ok(Listed::Host(models)) => {
                hosts_listed = true;
                hosted.extend(models);
            }
            Err(Error::SystemOneNotConfigured { .. }) => {}
            Err(e) => {
                tracing::warn!(
                    %request_id, %provider, error = %e,
                    "a System One provider could not list its models; listing the others"
                );
                failed.get_or_insert(e);
            }
        }
    }
    match jev {
        Some(reply) if !hosts_listed => Ok(relay(*reply, None, request_id)),
        None if !hosts_listed => Err(failed.unwrap_or(Error::SystemOneNotConfigured {
            route: route.name,
            provider: Provider::Jev,
        })),
        jev => {
            let models = jev
                .into_iter()
                .flat_map(|reply| reply.decoded.models)
                .chain(hosted);
            Ok(written(&ListModelsResponse::new(models), request_id))
        }
    }
}

/// One System One provider's models.
enum Listed {
    /// Jev's listing, as it arrived. Boxed: it holds the whole reply.
    Jev(Box<Reply<ListModelsResponse>>),
    /// An endpoint's, in the SDK's shape, each named `<endpoint>/<name>`.
    Host(Vec<ModelMetadata>),
}

/// What `provider`'s credential on the route lists: Jev's listing whole, or an
/// endpoint's page by page, until its host says there are no more, repeats a
/// cursor, or [`MAX_LISTING_PAGES`] have been read.
async fn listed(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    route: &oag_store::RouteRow,
    provider: Provider,
    request_id: RequestId,
) -> Result<Listed> {
    if provider == Provider::Jev {
        let answer = call(
            state,
            auth,
            route,
            provider,
            request_id,
            Purpose::Listing,
            JevUpstream::models,
            decoded::<ListModelsResponse>,
        )
        .await?;
        answer.lease.release().await;
        return Ok(Listed::Jev(Box::new(answer.reply)));
    }
    let mut models = Vec::new();
    let mut named = HashSet::new();
    let mut cursors = HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LISTING_PAGES {
        let answer = call(
            state,
            auth,
            route,
            provider,
            request_id,
            Purpose::Listing,
            |upstream, credential| upstream.models_page(credential, cursor.as_deref()),
            listing_page,
        )
        .await?;
        answer.lease.release().await;
        let ListingPage { models: page, next } = answer.reply.decoded;
        for model in page {
            let name = format!("{provider}/{}", model.name);
            if named.insert(name.clone()) {
                models.push(ModelMetadata { name, ..model });
            }
        }
        // A host that no longer knows a cursor starts again at its first
        // page, whose cursor is one already followed.
        match next {
            Some(next) if cursors.insert(next.clone()) => cursor = Some(next),
            _ => return Ok(Listed::Host(models)),
        }
    }
    tracing::warn!(
        %request_id,
        %provider,
        pages = MAX_LISTING_PAGES,
        "a System One host lists more pages than are read; the rest are not listed"
    );
    Ok(Listed::Host(models))
}

/// A listing this gateway wrote, with its identity beside it.
fn written(listing: &ListModelsResponse, request_id: RequestId) -> Response {
    let Ok(body) = serde_json::to_vec(listing) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    identity_headers(Response::builder().status(StatusCode::OK), request_id)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The chat path's admission without its ladder: the route's rate limit, then
/// the caller's spend caps.
///
/// Budget pressure has one meaning here. `Constrained` moves a chat request to
/// a cheaper rung, and System One has no cheaper rung to move to — so only
/// `Exhausted`, the hard stop, refuses.
async fn admit(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
) -> Result<oag_store::RouteRow> {
    let route = oag_store::repo::route_by_id(&state.db, auth.route_id)
        .await?
        .ok_or_else(|| Error::Internal("route vanished between auth and admission".to_owned()))?;
    if let Some(rpm) = route.rpm_limit
        && let Ok(rpm) = u32::try_from(rpm)
        && let Some(retry_after) = state.cache.take_rate_token(route.id, rpm).await?
    {
        return Err(Error::RateLimited { retry_after });
    }
    let spend = oag_store::repo::spend_for(&state.db, auth.api_key_id, auth.principal_id).await?;
    let budgets = budgets_for(auth, &route, &spend);
    if budgets.pressure() == BudgetPressure::Exhausted {
        return Err(Error::BudgetExhausted {
            scope: budgets.binding(),
        });
    }
    Ok(route)
}

/// A 2xx from a System One upstream, read whole and checked.
struct Reply<T> {
    status: StatusCode,
    headers: reqwest::header::HeaderMap,
    body: bytes::Bytes,
    decoded: T,
}

/// A System One answer, with the credential that gave it.
struct Answered<T> {
    lease: select::Lease,
    /// Which credential of this request's answered, counted from zero: the
    /// ledger row's `attempt`.
    attempt: u8,
    reply: Reply<T>,
}

/// What a request through [`call`] is for, which decides what its outcome
/// says about the credential that carried it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// A question set: its failures count against the credential's breaker
    /// and cool it down, and its successes close the breaker, as a chat
    /// request's do.
    Answer,
    /// A model listing: it touches neither the breaker nor the cooldowns. A
    /// host whose `/v1/models` fails is not a host whose answers fail, and
    /// benching its keys over a listing would take them out of the rotation
    /// that serves question sets; nor does a listing that works say a key the
    /// breaker opened on answers again. It claims no half-open probe either,
    /// so one stays for a question set to spend.
    Listing,
}

/// The credential's breaker and cooldowns, as one attempt under `purpose`
/// writes to them: as the chat path does for [`Purpose::Answer`], and not at
/// all for [`Purpose::Listing`].
struct Health<'a> {
    state: &'a AppState,
    account: AccountId,
    purpose: Purpose,
}

impl Health<'_> {
    fn counts(&self) -> bool {
        self.purpose == Purpose::Answer
    }

    fn succeeded(&self) {
        if self.counts() {
            self.state.breakers.record_success(self.account);
        }
    }

    fn failed(&self) {
        if self.counts() {
            self.state.breakers.record_failure(self.account);
        }
    }

    async fn dispose(&self, disposition: oag_core::Disposition) {
        if self.counts() {
            apply_disposition(self.state, self.account, disposition).await;
        }
    }

    async fn unreachable(&self, retrying: bool) {
        if self.counts()
            && let Some(d) = transport_failure(&self.state.breakers, self.account, retrying)
        {
            apply_disposition(self.state, self.account, d).await;
        }
    }
}

/// Send one request through a credential of `provider` on the caller's route,
/// failing over between them under the chat path's rules.
///
/// `build` makes the request for whichever credential was leased, and
/// `decode` checks that a 2xx body is what it claims to be. A 2xx that is not
/// is not an answer, and moves to the next credential as a 5xx would — the
/// chat path's rule for a body that is not JSON.
///
/// Two bounds, as there: `max_account_switches` on how many credentials, and
/// `failover_budget` on how long, checked only between them. `purpose` says
/// whether what happens counts against each credential's health.
#[allow(clippy::too_many_arguments)]
async fn call<T>(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    route: &oag_store::RouteRow,
    provider: Provider,
    request_id: RequestId,
    purpose: Purpose,
    build: impl Fn(&JevUpstream, &SecretMaterial) -> Result<reqwest::Request>,
    decode: impl Fn(&[u8]) -> Result<T>,
) -> Result<Answered<T>> {
    // Held for the request, as an adapter is: a reload that changes the
    // endpoint meanwhile changes the next request's upstream, not this one's.
    let upstream = state.system_one(provider)?;
    // One pin per caller and provider: a coarse affinity, the chat path's
    // fallback form. There is no conversation for a finer one to follow.
    let session = SessionKey::from_caller(&auth.api_key_id.to_string(), provider.as_str());
    let mut excluded: HashSet<AccountId> = HashSet::new();
    let mut last_error: Option<Error> = None;
    let began = Instant::now();
    let budget = state.config.gateway.failover_budget;

    for switch in 0..=state.config.gateway.max_account_switches {
        if !may_try_another(switch, began.elapsed(), budget) {
            tracing::warn!(
                %request_id,
                %provider,
                switch,
                "failover budget spent; returning the last System One error"
            );
            metrics::counter!("oag_failover_budget_exhausted_total").increment(1);
            break;
        }
        let lease = match select::lease(
            state,
            auth.route_id,
            auth.principal_id,
            provider,
            &session,
            &excluded,
            &request_id.to_string(),
            None,
        )
        .await
        {
            Ok(lease) => lease,
            // After a failover the interesting error is why the credential
            // already tried failed, not that there is none left to try.
            Err(e) => {
                return Err(match last_error {
                    Some(last) => last,
                    None => unconfigured(state, auth, route, provider, e).await,
                });
            }
        };
        let account = lease.account.account_id();
        match attempt_on(
            state, &upstream, &lease, provider, request_id, purpose, &build, &decode,
        )
        .await
        {
            Tried::Answered(reply) => {
                metrics::counter!(
                    "oag_requests_total",
                    "outcome" => "ok",
                    "provider" => provider.as_str(),
                )
                .increment(1);
                return Ok(Answered {
                    lease,
                    attempt: switch,
                    reply: *reply,
                });
            }
            Tried::Fatal(e) => {
                lease.release().await;
                return Err(e);
            }
            Tried::Switch(e) => {
                tracing::warn!(
                    %request_id, %provider, %account, error = %e,
                    "switching System One credential"
                );
                last_error = Some(e);
                lease.release().await;
                excluded.insert(account);
            }
            // Nothing was sent and nothing failed: say nothing about this one.
            Tried::Raced => {
                lease.release().await;
                excluded.insert(account);
            }
        }
    }

    Err(last_error.unwrap_or(Error::NoCredential { provider }))
}

/// Name the refusal for a route that holds no credential for `provider` at
/// all.
///
/// `lease` answers `NoCredential` for a route with no account of the provider
/// and for one whose accounts are all disabled or cooling down. Only the first
/// is "not configured", so it is asked separately — on this failure path only,
/// where the extra read costs a request that is refused either way.
async fn unconfigured(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    route: &oag_store::RouteRow,
    provider: Provider,
    e: Error,
) -> Error {
    if matches!(e, Error::NoCredential { .. })
        && oag_store::repo::candidates(
            &state.db,
            auth.route_id,
            provider.as_str(),
            auth.principal_id,
        )
        .await
        .is_ok_and(|accounts| accounts.is_empty())
    {
        return Error::SystemOneNotConfigured {
            route: route.name.clone(),
            provider,
        };
    }
    e
}

/// What one System One credential's attempts came to.
enum Tried<T> {
    /// Boxed: a whole reply beside the error variants would make every one of
    /// them as large.
    Answered(Box<Reply<T>>),
    /// Try another credential.
    Switch(Error),
    /// Another request took this credential's half-open probe between
    /// selection and dispatch. Nothing was sent.
    Raced,
    /// Another credential cannot help.
    Fatal(Error),
}

/// One credential, with bounded same-credential retries: `try_credential`'s
/// rules, for a body that is sent whole and read whole. What happens is
/// recorded against the credential's health only for [`Purpose::Answer`].
// One loop over one credential's attempts, as `try_credential`'s is, and
// long for the same reason: each arm says what its outcome means.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn attempt_on<T>(
    state: &Arc<AppState>,
    upstream: &JevUpstream,
    lease: &select::Lease,
    provider: Provider,
    request_id: RequestId,
    purpose: Purpose,
    build: &impl Fn(&JevUpstream, &SecretMaterial) -> Result<reqwest::Request>,
    decode: &impl Fn(&[u8]) -> Result<T>,
) -> Tried<T> {
    let account = lease.account.account_id();
    let health = Health {
        state,
        account,
        purpose,
    };
    // For an API key this is the unseal, trimmed: it never expires, so there
    // is nothing to refresh.
    let credential = match refresh::ensure_fresh(state, &lease.account).await {
        Ok(credential) => credential,
        Err(e) => return Tried::Switch(e),
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    // A listing claims no dispatch: it records nothing against the breaker,
    // so it must not spend the half-open probe a question set is waiting on.
    let mut dispatch = match purpose {
        Purpose::Answer => match Dispatch::claim(&state.breakers, account, now) {
            Some(dispatch) => Some(dispatch),
            None => return Tried::Raced,
        },
        Purpose::Listing => None,
    };

    let retries = state.config.gateway.same_account_retries;
    let mut last = Error::NoCredential { provider };
    for attempt in 0..=retries {
        let request = match build(upstream, &credential) {
            Ok(request) => request,
            // A request that cannot be built for one credential cannot be
            // built for another.
            Err(e) => return Tried::Fatal(e),
        };
        let transport = match state
            .transports
            .get(&TransportKey {
                account,
                proxy: lease.account.proxy_url.clone(),
            })
            .await
        {
            Ok(transport) => transport,
            Err(e) => return Tried::Switch(e),
        };
        let retries_left = attempt < retries;

        if let Some(dispatch) = dispatch.as_mut() {
            dispatch.sent();
        }
        let step = match transport.execute(request).await {
            Ok(response) if response.status().is_success() => {
                let ceiling = state.config.gateway.max_stream_duration;
                return match read(response, decode, ceiling).await {
                    Ok(reply) => {
                        health.succeeded();
                        Tried::Answered(Box::new(reply))
                    }
                    Err(e) => {
                        tracing::warn!(
                            %request_id, %provider, %account, error = %e,
                            "System One upstream answered 2xx with no answer in it"
                        );
                        health.failed();
                        Tried::Switch(e)
                    }
                };
            }
            Ok(response) => {
                last = refusal(response, provider, account).await;
                let disposition = last.disposition();
                tracing::warn!(
                    %request_id, %provider, %account, error = %last, ?disposition,
                    "System One upstream refused"
                );
                health.failed();
                health.dispose(disposition).await;
                step_for(disposition, retries_left)
            }
            // Silent for the whole headers deadline. Fail over at once, as the
            // chat path does: a same-credential retry would spend that deadline
            // again before another credential was tried.
            Err(e @ Error::UpstreamTimeout { .. }) => {
                tracing::warn!(
                    %request_id, %provider, %account, error = %e,
                    "System One upstream silent"
                );
                health.failed();
                health.dispose(e.disposition()).await;
                return Tried::Switch(e);
            }
            // Nothing came back at all: connect, TLS or DNS.
            Err(e) => {
                tracing::warn!(
                    %request_id, %provider, %account, error = %e, retrying = retries_left,
                    "System One upstream unreachable"
                );
                health.unreachable(retries_left).await;
                last = e;
                if retries_left {
                    Step::Retry
                } else {
                    Step::Switch
                }
            }
        };

        match step {
            // The breaker may have opened on the failure just recorded, and a
            // credential it has this moment called unhealthy gets no more of
            // this request's retries.
            Step::Retry
                if state
                    .breakers
                    .permits(account, time::OffsetDateTime::now_utc().unix_timestamp()) =>
            {
                tokio::time::sleep(backoff(attempt)).await;
            }
            // A bigger model is the chat path's answer to a request too large
            // for this one. System One has no rung to climb, so the caller has
            // the refusal, like any other that no credential can fix.
            Step::Escalate | Step::Fatal => return Tried::Fatal(last),
            Step::Retry | Step::Switch => return Tried::Switch(last),
        }
    }
    Tried::Switch(last)
}

/// A 2xx body, read whole under the same ceiling a chat body is, then checked.
async fn read<T>(
    response: reqwest::Response,
    decode: &impl Fn(&[u8]) -> Result<T>,
    ceiling: Duration,
) -> Result<Reply<T>> {
    let status = response.status();
    let headers = response.headers().clone();
    let body = tokio::time::timeout(ceiling, response.bytes())
        .await
        .map_err(|_| {
            Error::Internal(format!(
                "the System One answer was not complete after {}s",
                ceiling.as_secs()
            ))
        })?
        .map_err(|e| Error::Internal(format!("reading the System One answer: {e}")))?;
    let decoded = decode(&body)?;
    Ok(Reply {
        status,
        headers,
        body,
        decoded,
    })
}

/// A 2xx body checked against the wire type it claims to be.
fn decoded<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| {
        Error::Internal(format!(
            "the System One upstream answered with a body that is not a {}: {e}",
            std::any::type_name::<T>()
        ))
    })
}

/// A System One upstream's refusal, as the error every upstream refusal is.
async fn refusal(response: reqwest::Response, provider: Provider, account: AccountId) -> Error {
    let status = response.status().as_u16();
    // Read before the body is consumed: `text()` takes the whole response.
    let retry_after = upstream_retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    Error::Upstream {
        provider,
        account,
        status,
        body: truncate(&body, 512),
        retry_after,
    }
}

/// The upstream's bytes as they arrived, with its request id and this
/// gateway's identity beside them.
fn relay<T>(reply: Reply<T>, model: Option<&str>, request_id: RequestId) -> Response {
    let content_type = reply
        .headers
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/json"));
    let mut builder = identity_headers(Response::builder().status(reply.status), request_id)
        .header(header::CONTENT_TYPE, content_type);
    if let Some(id) = reply.headers.get(REQUEST_ID_HEADER) {
        builder = builder.header(REQUEST_ID_HEADER, id.clone());
    }
    // Named by Jev's answer, so not trusted to be a header value: a model name
    // that is not one loses this header, never the answer it names.
    if let Some(model) = model.and_then(|m| HeaderValue::from_str(m).ok()) {
        builder = builder.header("x-oag-model", model);
    }
    builder
        .body(Body::from(reply.body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The catalog's entry for the model `provider`'s upstream says answered; or,
/// when that model has no row, the row the request `asked` for; or an unpriced
/// stand-in when neither has one.
///
/// The model that answered comes first, because it is what ran. The one asked
/// for is the fallback because a host may answer with a name no row spells:
/// Merge Gateway, asked for `typesafe/jev-1.13`, answers as `jev-1.13.0`, the
/// concrete version, and the row that prices it is the one it was asked by.
///
/// Only Jev's answers reach the stand-in: a request for another host's model
/// is refused unless a row prices it ([`resolve`]), so it always has `asked`.
/// Unpriced rather than refused: the answer has been given and billed either
/// way, and a row with no cost is a pricing gap the dashboard shows, where no
/// row at all is spend nobody sees.
fn priced(
    catalog: &Catalog,
    provider: Provider,
    model: &str,
    asked: Option<&ModelSpec>,
) -> ModelSpec {
    let id = ModelId::new(format!("{provider}/{model}"));
    catalog
        .get(&id)
        .or(asked)
        .cloned()
        .unwrap_or_else(|| ModelSpec {
            id,
            provider,
            upstream_name: model.to_owned(),
            pricing: Pricing {
                input_per_mtok: rust_decimal::Decimal::ZERO,
                output_per_mtok: rust_decimal::Decimal::ZERO,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: 0,
            max_output_tokens: 0,
            capabilities: Capabilities::default(),
            display_label: None,
            reasoning_efforts: None,
        })
}

/// A count an upstream reported, as the ledger counts. Absent or negative is
/// zero: nothing anyone can be billed for.
fn tokens(count: Option<i64>) -> u64 {
    count.and_then(|n| u64::try_from(n).ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod hosts;
