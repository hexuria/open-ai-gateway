//! One attempt at one rung: credentials, retries, backoff and failover.

use super::climb::{Dispatches, meter_context};
use super::respond::truncate;
use super::{meter, refresh, select, sse};
use crate::AppState;
use crate::breakers::{Breakers, Dispatch};
use oag_core::provider::Dialect;
use oag_core::{AccountId, Disposition, Error, RequestId, Result};
use oag_pool::SessionKey;
use oag_proto::FunctionNameMap;
use oag_router::RoutingDecision;
use oag_upstream::Transport as _;
use std::collections::HashSet;
use std::sync::Arc;

/// What one forwarding attempt produced.
pub(super) enum Attempt {
    /// Handed to the client as a stream. Nothing further can be decided.
    Streaming {
        response: reqwest::Response,
        lease: select::Lease,
        /// Which dispatch of this request produced it; see [`Dispatches`].
        attempt: u8,
        /// Original ↔ wire function names, for restoring `tool_calls`.
        names: FunctionNameMap,
    },
    /// Read in full, so the answer can still be judged and retried.
    Collected {
        /// The upstream's own bytes, for the one case they can be forwarded
        /// as they are: a client in the upstream's dialect, over an adapter
        /// that sent a JSON body rather than a stream. Empty otherwise.
        body: bytes::Bytes,
        /// The answer as canonical events, which is what every other case
        /// renders the client's body from.
        events: Vec<oag_proto::StreamEvent>,
        accumulator: oag_proto::StreamAccumulator,
        lease: select::Lease,
        /// Which dispatch of this request produced it; see [`Dispatches`].
        attempt: u8,
    },
    /// The model refused the request itself — too long, or beyond what it can
    /// do. No credential can help and the lease is already released, but a
    /// rung up can, so this is carried back rather than returned as an error.
    Rejected(Error),
}

/// A complete, non-streamed response.
/// Choose how to produce the client's bytes.
///
/// Passthrough whenever the dialects agree, which is both faster and more
/// faithful — we hand back the bytes the upstream considered correct.
pub(super) fn egress_for(
    ingress: Dialect,
    decision: &RoutingDecision,
    request_id: RequestId,
    framing: oag_upstream::Framing,
    // The dialect the chosen ADAPTER speaks, which is not always the provider's:
    // a Codex seat is `Provider::OpenAI` and speaks Responses.
    upstream: Dialect,
    // True when tool names were rewritten for the OpenAI wire. Verbatim
    // passthrough would then hand the client the sanitised names, which it
    // cannot dispatch.
    rewrite_tool_names: bool,
) -> Result<sse::Egress> {
    let model = decision.model.id.as_str().to_owned();
    let request_id = request_id.to_string();

    // Matching dialects are not sufficient: passthrough forwards the upstream's
    // *bytes*, so it also requires that those bytes are already SSE. Bedrock's
    // dialect is Anthropic and its framing is binary — passing that through
    // would hand a client expecting `data:` lines a length-prefixed envelope.
    if ingress == upstream && framing == oag_upstream::Framing::Sse && !rewrite_tool_names {
        return Ok(sse::Egress::Passthrough {
            dialect: ingress,
            request_id,
            model,
        });
    }
    Ok(match ingress {
        Dialect::OpenAIChatCompletions => sse::Egress::ChatCompletions { request_id, model },
        Dialect::AnthropicMessages => sse::Egress::AnthropicMessages { request_id, model },
        Dialect::GeminiGenerateContent => sse::Egress::Gemini,
        Dialect::OpenAIResponses => sse::Egress::Responses { request_id, model },
        // Falling back to passthrough here would send the upstream's dialect to
        // a client expecting a different one — bytes that parse as nothing and
        // fail somewhere far from the cause. An error names the problem.
        // `Dialect` is non-exhaustive, so this arm also catches anything added
        // later — the safe direction: a new dialect fails loudly here until
        // someone writes its renderer, rather than silently passing bytes
        // through in the wrong shape.
        _ => {
            return Err(Error::Internal(format!(
                "no renderer from {upstream:?} to {ingress:?}; \
                 route this request to a {ingress:?}-native provider instead"
            )));
        }
    })
}

/// Which adapter this account actually gets.
///
/// An OpenAI OAuth seat is a Codex subscription: the same provider key, a
/// different dialect and backend, so it takes the Codex adapter rather than the
/// Chat Completions one. Every other account uses its provider's adapter.
///
/// ONE PLACE, because the answer is needed twice and the two must agree. The
/// request path picks an adapter to build with; the response path needs the
/// same adapter's dialect and framing to decide whether the upstream's bytes
/// can be forwarded as they are. When only the first knew about Codex seats,
/// the second asked the provider instead, was told Chat Completions, and passed
/// Responses bytes through to a client that reads them as an empty answer.
pub(crate) fn adapter_for(
    state: &AppState,
    provider: oag_core::Provider,
    account: &oag_store::AccountRow,
) -> Result<Arc<dyn oag_upstream::ProviderAdapter>> {
    let is_codex_seat = matches!(provider, oag_core::Provider::OpenAI)
        && oag_core::credential::CredentialKind::from_column(&account.kind)
            .is_some_and(|k| matches!(k, oag_core::credential::CredentialKind::OAuth));
    if is_codex_seat {
        Ok(state.codex_adapter())
    } else {
        state.adapter(provider)
    }
}

/// Function names as an OpenAI-shaped upstream must see them.
///
/// Canonical keeps the client's names. Only Chat Completions, Responses and
/// Bedrock Converse rewrite, because those are the dialects that hold a name
/// to the OpenAI function-name pattern and refuse one outside it with a 400.
/// Converse's codec sanitises the same way, so this map is the one that puts
/// the client's names back; and it respells a tool call's id its own pattern
/// refuses, which the map puts back too. Other dialects keep identity, so
/// same-dialect passthrough is undisturbed.
pub(super) fn openai_function_names(
    canonical: &oag_proto::CanonicalRequest,
    upstream: Dialect,
) -> FunctionNameMap {
    match upstream {
        Dialect::OpenAIChatCompletions | Dialect::OpenAIResponses | Dialect::BedrockConverse => {
            let mut names = FunctionNameMap::from_request(canonical);
            if upstream == Dialect::BedrockConverse {
                names = names
                    .with_tool_use_ids(oag_proto::converse::ToolUseIds::from_request(canonical));
            }
            if names.rewrites() {
                for (original, wire) in names.rewritten() {
                    tracing::debug!(original, wire, "sanitized OpenAI function name");
                }
            }
            names
        }
        _ => FunctionNameMap::identity(),
    }
}

/// Try credentials until one works or the budget of attempts runs out.
///
/// Two nested bounds, and they count different things:
///
/// - `same_account_retries` covers a *transient* failure — the credential is
///   fine, the moment was not.
/// - `max_account_switches` covers an *unhealthy* credential, and each switch
///   adds the failed one to an exclusion set so the cascade cannot hand it back.
///
/// Both are bounded because an unbounded retry loop against a provider having a
/// bad afternoon is indistinguishable from an attack on it.
/// Namespace for [`conversation_id`]. Fixed: changing it gives every live
/// conversation a new id mid-stream.
const CONVERSATION_NAMESPACE: uuid::Uuid =
    uuid::Uuid::from_u128(0x6f61_672d_636f_6e76_6572_7361_7469_6f6e);

/// One id per conversation, the same on every request in it.
///
/// A person's own Codex CLI sends one `session_id` for a whole conversation.
/// A fresh one per request made one seat look like a crowd — hundreds of
/// sessions an hour from one account — which is the pattern a subscription's
/// abuse checks look for. The sticky [`SessionKey`] already names the
/// conversation (and is per principal), so the id is derived from it: stable
/// across requests and replicas, and meaningless outside this gateway.
pub(super) fn conversation_id(session: &SessionKey) -> uuid::Uuid {
    uuid::Uuid::new_v5(&CONVERSATION_NAMESPACE, session.as_str().as_bytes())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn forward_with_failover(
    state: &Arc<AppState>,
    auth: &oag_store::AuthContext,
    decision: &RoutingDecision,
    canonical: &oag_proto::CanonicalRequest,
    session: &SessionKey,
    dispatches: &mut Dispatches,
    lost: &mut Vec<meter::Lost>,
    channel: Option<oag_core::credential::CredentialKind>,
) -> Result<Attempt> {
    let request_id = dispatches.request_id;
    let provider = decision.model.provider;
    let conversation = conversation_id(session);
    let mut excluded: HashSet<AccountId> = HashSet::new();
    let mut last_error = Error::NoCredential { provider };

    let began = std::time::Instant::now();
    let budget = state.config.gateway.failover_budget;

    for switch in 0..=state.config.gateway.max_account_switches {
        // Between attempts only. `max_account_switches` bounds how MANY
        // credentials are tried and nothing bounded how long that took, so a
        // provider slow to answer rather than quick to refuse could hold a
        // caller indefinitely: the upstream client sets no total-response
        // timeout on purpose, and `stream_idle_timeout` starts only once a
        // response exists, so neither covers the gap before first headers.
        //
        // Never interrupts an attempt in flight. Giving up on a slow-but-working
        // stream is the failure this must not cause, so the check sits here,
        // where the only thing it can prevent is starting ANOTHER one.
        if !may_try_another(switch, began.elapsed(), budget) {
            tracing::warn!(
                %request_id, switch, elapsed_ms = began.elapsed().as_millis(),
                error = %last_error,
                "failover budget spent; returning the last upstream error rather than trying another credential"
            );
            metrics::counter!("oag_failover_budget_exhausted_total").increment(1);
            break;
        }
        let lease = match select::lease(
            state,
            auth.route_id,
            auth.principal_id,
            provider,
            session,
            &excluded,
            &request_id.to_string(),
            channel,
        )
        .await
        {
            Ok(l) => l,
            Err(e) => {
                // Running out of candidates is only the real cause on the first
                // pass. After a failover, the interesting error is why the
                // credential we already tried failed — "no credential
                // available" would bury it and send whoever is debugging to
                // look at the pool, which is fine.
                if matches!(last_error, Error::NoCredential { .. }) {
                    last_error = e;
                }
                tracing::debug!(%request_id, switch, error = %last_error, "no further candidates");
                break;
            }
        };
        let account = lease.account.account_id();
        let attempt = dispatches.take();

        match try_credential(
            state,
            decision,
            canonical,
            &lease,
            request_id,
            attempt,
            conversation,
        )
        .await
        {
            Outcome::Ok(attempt) => {
                if switch > 0 {
                    metrics::counter!("oag_failovers_total").increment(1);
                }
                return Ok(*attempt);
            }
            // Nothing about another credential would help.
            Outcome::Fatal(e) => {
                lease.release().await;
                return Err(e);
            }
            // The credential did its job; the *model* would not take the
            // request. Every other credential reaches the same model, so
            // failing over is pointless — hand it up to escalation instead.
            Outcome::Escalate(e) => {
                lease.release().await;
                return Ok(Attempt::Rejected(e));
            }
            Outcome::Switch(e) => {
                tracing::warn!(%request_id, %account, error = %e, "switching credential");
                last_error = e;
                lease.release().await;
                excluded.insert(account);
            }
            // The credential generated an answer and the stream died before
            // it was whole. The provider invoiced those tokens, and the retry
            // on another credential used to be the only attempt metered, so
            // the ledger saw one generation for every two paid for.
            //
            // Captured here, under its own dispatch number and while the lease
            // that names the account is still in hand; written by the caller,
            // on the detached task, after the served row. Writing it here put
            // it on the request future, where the client hanging up during the
            // retry — the very thing a lost stream makes likely — cancelled it.
            Outcome::Lost(e, accumulator) => {
                tracing::warn!(%request_id, %account, error = %e, "switching credential after a lost answer");
                let ctx = meter_context(
                    auth,
                    decision,
                    &lease,
                    request_id,
                    dispatches.started,
                    attempt,
                );
                lost.push(meter::lose(ctx, &accumulator, &e));
                last_error = e;
                lease.release().await;
                excluded.insert(account);
            }
            // Not an error and not this credential's fault: move on without
            // touching `last_error`, which still names whatever genuinely
            // went wrong before.
            Outcome::Raced => {
                tracing::debug!(%request_id, %account, "half-open probe already taken; trying another credential");
                lease.release().await;
                excluded.insert(account);
            }
        }
    }

    Err(last_error)
}

/// What one credential's attempts came to.
///
/// The success variant is boxed: it carries a whole `reqwest::Response` and a
/// lease, which makes every `Outcome` — including the common error ones — as
/// large as the largest variant otherwise.
pub(super) enum Outcome {
    Ok(Box<Attempt>),
    /// Try a different credential.
    Switch(Error),
    /// Try a different credential, and meter what this one generated first:
    /// the answer was read far enough to cost something before it was lost.
    /// Boxed for the same reason `Ok` is.
    Lost(Error, Box<oag_proto::StreamAccumulator>),
    /// Another request took this credential's half-open probe between
    /// selection and dispatch. Nothing was sent and nothing failed: try a
    /// different credential, and say nothing about this one.
    ///
    /// Its own variant rather than `Switch(NoCredential)`, which is what it
    /// was: that placeholder became `last_error`, overwriting the genuine
    /// upstream error from the credential tried before — so a request that
    /// failed on a real 5xx and then raced a probe told the caller "no
    /// credential available", and sent whoever read the log to stare at a
    /// healthy pool.
    Raced,
    /// Stop switching credentials and try a better model instead.
    Escalate(Error),
    /// Stop: another credential cannot help.
    Fatal(Error),
}

/// What a rejected attempt says to do next.
///
/// Split out of [`try_credential`] as a pure function because it is the point
/// where a context-length rejection either climbs the ladder or fails the
/// caller, and that decision should be testable without a transport, a lease,
/// and a database behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// Same credential, after a backoff.
    Retry,
    /// A bigger model.
    Escalate,
    /// A different credential.
    Switch,
    /// Nothing.
    Fatal,
}

/// Whether another CREDENTIAL may be tried, given how long this request has
/// already spent failing over.
///
/// Pure, and split out for the same reason as [`step_for`]: it is a decision
/// about a caller's wait, and it should be checkable without a transport, a
/// lease and a database behind it.
///
/// The first attempt is always allowed however long the request has taken. A
/// budget that could refuse to try at all would turn a slow moment into a
/// request that never reached an upstream, which is a worse failure than the
/// wait it exists to bound.
pub(super) const fn may_try_another(
    switch: u8,
    elapsed: std::time::Duration,
    budget: std::time::Duration,
) -> bool {
    switch == 0 || elapsed.as_millis() < budget.as_millis()
}

pub(super) fn step_for(disposition: Disposition, retries_left: bool) -> Step {
    match disposition {
        Disposition::RetrySameAccount if retries_left => Step::Retry,
        Disposition::EscalateTier => Step::Escalate,
        Disposition::Fatal => Step::Fatal,
        // Rate limited, unhealthy, or out of same-credential retries: all of
        // them are answered by somebody else's credential.
        _ => Step::Switch,
    }
}

/// Say so when a client's vendor fields are about to be dropped.
///
/// A `CanonicalRequest` carries what the four dialects have in common, so a
/// field only one vendor defines has nowhere to land. It rides along in
/// `passthrough` and a renderer for its own dialect puts it back; a renderer
/// for any other dialect leaves it, which is right — we do not invent
/// translations for fields we do not understand.
///
/// What was wrong was that this cost nothing to notice. Losing a Gemini
/// `audioTranscriptionConfig` on the way to another upstream is a 200 with an
/// empty transcript, and nothing anywhere said a field had gone. Now the
/// count is on `/metrics` and the names are one log level away.
///
/// Here rather than in `oag-proto`, which is a pure crate with no recorder,
/// and here rather than at the routing decision, which knows the provider but
/// not the adapter: a Codex seat is `Provider::OpenAI` and speaks Responses.
/// This is the one place per dispatch that holds both the request and the
/// adapter that will actually render it.
fn note_dropped_vendor_fields(
    canonical: &oag_proto::CanonicalRequest,
    upstream: Dialect,
    request_id: RequestId,
) {
    let Some(extra) = &canonical.passthrough else {
        return;
    };
    if extra.dialect == upstream {
        return;
    }
    metrics::counter!(
        "oag_vendor_fields_dropped_total",
        "ingress" => extra.dialect.as_str(),
        "upstream" => upstream.as_str(),
    )
    .increment(1);
    tracing::debug!(
        %request_id,
        ingress = %extra.dialect.as_str(),
        upstream = %upstream.as_str(),
        fields = ?extra.body.as_object().map(|m| m.keys().collect::<Vec<_>>()),
        "vendor fields dropped translating between dialects"
    );
}

/// Try one credential, with bounded same-credential retries.
///
/// The retries here are for failures that are about *the moment* — a timeout, a
/// conflict — rather than about the credential. Anything that says the
/// credential itself is unhealthy returns `Switch` immediately rather than
/// spending the retry budget on it.
#[allow(clippy::too_many_lines)]
pub(super) async fn try_credential(
    state: &Arc<AppState>,
    decision: &RoutingDecision,
    canonical: &oag_proto::CanonicalRequest,
    lease: &select::Lease,
    request_id: RequestId,
    // `attempt` in the ledger's sense — this request's dispatch ordinal — as
    // distinct from the same-credential retry index the loop below counts.
    ordinal: u8,
    // The conversation's stable id, from [`conversation_id`].
    conversation: uuid::Uuid,
) -> Outcome {
    let provider = decision.model.provider;
    let account = lease.account.account_id();

    let adapter = match adapter_for(state, provider, &lease.account) {
        Ok(a) => a,
        Err(e) => return Outcome::Fatal(e),
    };
    let names = openai_function_names(canonical, adapter.dialect());
    note_dropped_vendor_fields(canonical, adapter.dialect(), request_id);
    // Refreshes first if the token is close to expiry. A credential that is
    // merely expiring must not be treated as a credential that is broken.
    let stored = match refresh::ensure_fresh(state, &lease.account).await {
        Ok(c) => c,
        // Broken for everyone, not just this request — but another credential
        // may well work, so switch rather than fail the request outright.
        Err(e) => return Outcome::Switch(e),
    };
    // What the request is built with, which is not always what is stored: a
    // service account's JSON key is exchanged for a token. Failing to make one
    // is this credential failing, so it is answered as a failed refresh is.
    // Through the credential's own proxy, as its refresh and its requests go.
    let credential = match adapter
        .prepare_credential(account, &stored, lease.account.proxy_url.as_deref())
        .await
    {
        Ok(c) => c,
        Err(e) => return Outcome::Switch(e),
    };

    let mut last = Error::NoCredential { provider };

    // Claim the breaker here rather than in selection. Selection reads, so a
    // recovering credential keeps its half-open probe until something is
    // actually about to be sent to it; the guard hands the probe back if we
    // return before reaching the wire.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let Some(mut dispatch) = Dispatch::claim(&state.breakers, account, now) else {
        // Raced: another request took the probe between the filter and here.
        return Outcome::Raced;
    };

    for attempt in 0..=state.config.gateway.same_account_retries {
        let request = match adapter.build(&oag_upstream::UpstreamRequest {
            canonical,
            model: &decision.model,
            credential: &credential,
            session: Some(conversation),
        }) {
            Ok(r) => r,
            // We built a bad request; a different credential will build the
            // same bad request.
            Err(e) => return Outcome::Fatal(e),
        };

        let transport = match state
            .transports
            .get(&oag_upstream::TransportKey {
                account,
                proxy: lease.account.proxy_url.clone(),
            })
            .await
        {
            Ok(t) => t,
            Err(e) => return Outcome::Switch(e),
        };

        dispatch.sent();
        match transport.execute(request).await {
            Ok(response) if response.status().is_success() => {
                return succeeded(
                    state,
                    provider,
                    lease,
                    response,
                    canonical.stream,
                    ordinal,
                    names,
                )
                .await;
            }

            Ok(response) => {
                let status = response.status().as_u16();
                // Read before the body is consumed: `text()` takes the whole
                // response, headers included.
                let retry_after = upstream_retry_after(response.headers());
                let body = response.text().await.unwrap_or_default();
                let err = Error::Upstream {
                    provider,
                    account,
                    status,
                    body: truncate(&body, 512),
                    retry_after,
                };
                let disposition = err.disposition();
                tracing::warn!(%request_id, status, ?disposition, "upstream rejected");
                state.breakers.record_failure(account);
                apply_disposition(state, account, disposition).await;
                last = err;

                let retries_left = attempt < state.config.gateway.same_account_retries;
                match step_for(disposition, retries_left) {
                    Step::Retry => {
                        // The breaker may have opened on the failure just
                        // recorded. `Dispatch::claim` is taken once, above the
                        // loop, so nothing re-checked it — a credential that has
                        // this moment tripped its breaker still received every
                        // remaining same-credential retry, which is exactly the
                        // traffic a breaker exists to stop, aimed at exactly the
                        // credential it has just decided is unhealthy.
                        let now = time::OffsetDateTime::now_utc().unix_timestamp();
                        if !state.breakers.permits(account, now) {
                            return Outcome::Switch(last);
                        }
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    Step::Escalate => return Outcome::Escalate(last),
                    Step::Switch => return Outcome::Switch(last),
                    Step::Fatal => return Outcome::Fatal(last),
                }
            }

            // The provider accepted the connection and then said nothing for
            // the whole headers deadline. That is not a transport blip worth
            // a same-credential retry: it already cost the caller the full
            // deadline, and two more attempts here would cost it twice more
            // before another credential was tried. The error's own
            // disposition says fail over like a 5xx, and until now nothing
            // on this path read it.
            Err(e @ Error::UpstreamTimeout { .. }) => {
                let disposition = e.disposition();
                tracing::warn!(%request_id, %account, error = %e, ?disposition, "upstream silent");
                state.breakers.record_failure(account);
                apply_disposition(state, account, disposition).await;
                return Outcome::Switch(e);
            }

            // Nothing came back at all: connect, TLS, or DNS.
            Err(e) => {
                last = e;
                let retrying = attempt < state.config.gateway.same_account_retries;
                tracing::warn!(%request_id, %account, error = %last, retrying, "upstream unreachable");
                if let Some(d) = transport_failure(&state.breakers, account, retrying) {
                    apply_disposition(state, account, d).await;
                }
                if !retrying {
                    return Outcome::Switch(last);
                }
                // The same re-check the `Step::Retry` arm makes, for the same
                // reason and against the same failure it has just recorded.
                // G7 added it there and not here, so a connect, TLS or DNS
                // failure that tripped the breaker still received every
                // remaining same-credential retry — the traffic a breaker
                // exists to stop, aimed at the credential it has this moment
                // decided is unhealthy. `transport_failure` above is what
                // records that failure, so the breaker's answer here is fresh.
                let now = time::OffsetDateTime::now_utc().unix_timestamp();
                if !state.breakers.permits(account, now) {
                    return Outcome::Switch(last);
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }

    Outcome::Switch(last)
}

/// Turn a successful response into the attempt the caller returns.
///
/// The body is collected here unless the client asked for a stream: only a
/// streaming client can be handed the upstream body as it arrives.
pub(super) async fn succeeded(
    state: &Arc<AppState>,
    provider: oag_core::Provider,
    lease: &select::Lease,
    response: reqwest::Response,
    stream: bool,
    attempt: u8,
    names: FunctionNameMap,
) -> Outcome {
    let account = lease.account.account_id();
    // No `touch_account` here any more. It was a Postgres write awaited
    // between "the upstream answered" and "the first byte reaches the client"
    // — on the one latency the user feels — to stamp `last_used_at`, which
    // the ledger write now stamps in the same statement as the row. Recency
    // for the scheduler's tie-break moves from "last dispatched to" to "last
    // completed on", which is a finer definition of used anyway.
    state.breakers.record_success(account);
    metrics::counter!(
        "oag_requests_total",
        "outcome" => "ok",
        "provider" => provider.as_str(),
    )
    .increment(1);

    if stream {
        return Outcome::Ok(Box::new(Attempt::Streaming {
            response,
            lease: lease.clone(),
            attempt,
            names,
        }));
    }
    // The ADAPTER's facts, not the provider's: a Codex seat is
    // `Provider::OpenAI`, speaks Responses, and streams whatever the client
    // asked. Asking the provider parsed a Responses stream as Chat
    // Completions, found nothing it recognised, and collected an empty body —
    // the 200 that reached a client as "no completion in it". And asking the
    // client's `stream` flag whether the upstream streamed read that stream
    // as a JSON body, handed the raw `data:` lines back, and metered zero.
    let adapter = match adapter_for(state, provider, &lease.account) {
        Ok(adapter) => adapter,
        Err(e) => return Outcome::Switch(e),
    };
    let collected = if adapter.always_streams() {
        let idle = state.config.gateway.stream_idle_timeout;
        let max = state.config.gateway.max_stream_duration;
        match sse::collect_stream_with(response, adapter, idle, max, names).await {
            Ok((events, accumulator)) => Ok((bytes::Bytes::new(), events, accumulator)),
            Err(failure) => return collect_failed(failure),
        }
    } else {
        sse::collect_with(
            response,
            adapter.dialect(),
            &names,
            state.config.gateway.max_stream_duration,
        )
        .await
    };
    match collected {
        Ok((body, events, accumulator)) => Outcome::Ok(Box::new(Attempt::Collected {
            body,
            events,
            accumulator,
            lease: lease.clone(),
            attempt,
        })),
        Err(e) => Outcome::Switch(e),
    }
}

/// A collected stream that failed: `Lost` once the provider had generated
/// part of the answer, because that much is invoiced whether or not it was
/// whole, so it goes out with the error rather than being dropped with it.
/// Before any output it is a plain failover.
pub(super) fn collect_failed(failure: sse::StreamFailure) -> Outcome {
    let (e, accumulator) = *failure;
    if accumulator.usage().output_tokens > 0 {
        Outcome::Lost(e, Box::new(accumulator))
    } else {
        Outcome::Switch(e)
    }
}

/// How long a credential sits out after the transport itself failed.
///
/// The same thirty seconds a 5xx gets, for the same reason: the credential is
/// probably fine and something between us and the provider is not, so the pause
/// wants to be long enough to stop hammering and short enough to come back.
pub(super) const TRANSPORT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

/// Account for a failure that never produced an HTTP status — connect, TLS,
/// DNS, timeout.
///
/// [`Error::disposition`] cannot classify these: all it sees is `Internal`, so
/// it says fatal, and this path used to skip the breaker entirely as a result.
/// The consequence is backwards. A credential behind a dead proxy fails
/// *fastest*, so it always carries the lowest in-flight count, so the
/// least-loaded stage prefers it over every healthy credential — and it never
/// trips, because nothing ever records the failures.
///
/// Returns the disposition to persist, or `None` while same-credential retries
/// remain: a cooldown written between two attempts on the same credential would
/// only be contradicted by the next one.
pub(super) fn transport_failure(
    breakers: &Breakers,
    account: AccountId,
    retrying: bool,
) -> Option<Disposition> {
    breakers.record_failure(account);
    if retrying {
        None
    } else {
        Some(Disposition::FailoverAccount {
            cooldown: TRANSPORT_COOLDOWN,
        })
    }
}

/// Persist what a failure says about a credential.
pub(super) async fn apply_disposition(state: &AppState, account: AccountId, d: Disposition) {
    use time::OffsetDateTime;
    match d {
        Disposition::FailoverAccount { cooldown } => {
            let until = OffsetDateTime::now_utc() + cooldown;
            let _ = oag_store::repo::cool_down(&state.db, account, until, "upstream error").await;
        }
        Disposition::RateLimited { retry_after } => {
            let wait = retry_after.unwrap_or(std::time::Duration::from_mins(1));
            let until = OffsetDateTime::now_utc() + wait;
            let _ = oag_store::repo::rate_limit(&state.db, account, until).await;
        }
        _ => {}
    }
}

/// The longest a provider's own `Retry-After` may bench one of our credentials.
///
/// An hour, and the ceiling matters more than the number. A genuinely day-long
/// quota costs at most one refused request per hour past this, which the breaker
/// and the cooldown then absorb — whereas trusting the provider's arithmetic
/// costs a credential nobody can get back.
pub(super) const MAX_RETRY_AFTER: u64 = 3_600;

/// How long the provider asked us to wait, if it said — and if the answer is
/// usable.
///
/// Only the delta-seconds form. The HTTP-date form is equally legal and no
/// provider we speak to sends it, and a date read wrongly is worse than no hint
/// at all — the caller has a sane default and a misparsed one would override it.
///
/// Anything outside one second to [`MAX_RETRY_AFTER`] is treated as no header at
/// all, and that validation is not decoration: this value is *persisted* as
/// `rate_limited_until`, and `repo::clear_cooldown` deliberately does not clear
/// that column, so a number we accept here cannot be undone from the admin
/// surface at all. Somebody has to reach for psql. Two values in the wild:
///
/// - **`0`**, which Cloudflare sends in front of several providers. Taken
///   literally it means "wait no time", so the credential the provider has just
///   throttled becomes immediately selectable again — strictly worse than the
///   one-minute default it replaced.
/// - **an epoch timestamp**, from confusing `Retry-After` with a reset time. In
///   seconds it benches the credential until the 2080s. In milliseconds it
///   overflows the `OffsetDateTime` addition in [`apply_disposition`] and takes
///   the request down with it.
pub(super) fn upstream_retry_after(
    headers: &reqwest::header::HeaderMap,
) -> Option<std::time::Duration> {
    // A negative or fractional value fails this parse and is thereby rejected
    // too, which is the right answer for both.
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (1..=MAX_RETRY_AFTER)
        .contains(&secs)
        .then(|| std::time::Duration::from_secs(secs))
}

/// The longest [`backoff`] waits. The slot heartbeat's lifetime counts it.
pub(crate) const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(3);

/// Exponential backoff, capped at [`MAX_BACKOFF`].
pub(super) fn backoff(attempt: u8) -> std::time::Duration {
    let ms = 300u64.saturating_mul(1 << u32::from(attempt.min(4)));
    std::time::Duration::from_millis(ms).min(MAX_BACKOFF)
}
