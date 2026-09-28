use super::climb::{MAX_ESCALATIONS, budget_alone_prevented_the_climb, spawn_unserved};
use super::failover::{
    MAX_RETRY_AFTER, Outcome, Step, TRANSPORT_COOLDOWN, backoff, collect_failed, egress_for,
    may_try_another, step_for, transport_failure, upstream_retry_after,
};
use super::plan::parse_ladder;
use super::respond::{
    client_got_nothing, json_response, no_viable_message, render_collected, stream_response,
    truncate,
};
use super::*;
use crate::breakers::Breakers;
use axum::http::{StatusCode, header};
use oag_core::{AccountId, Disposition, TierName};
use oag_router::{RoutingDecision, RoutingPolicy, TierLadder};
use std::time::Duration;

fn failure_after(output_tokens: u64) -> sse::StreamFailure {
    let mut accumulator = oag_proto::StreamAccumulator::new();
    accumulator.observe(&oag_proto::StreamEvent::UsageUpdate {
        usage: oag_router::Usage {
            output_tokens,
            ..oag_router::Usage::default()
        },
    });
    Box::new((Error::Internal("stream dropped".to_owned()), accumulator))
}

#[test]
fn a_collected_stream_that_failed_after_output_is_lost_and_metered() {
    // One generated token is one invoiced token: dropping the accumulator
    // with the error is how a lost stream reached the ledger as free.
    match collect_failed(failure_after(1)) {
        Outcome::Lost(_, accumulator) => assert_eq!(accumulator.usage().output_tokens, 1),
        _ => panic!("a failure after output must be Lost"),
    }
}

#[test]
fn a_collected_stream_that_failed_before_output_is_a_plain_failover() {
    assert!(
        matches!(collect_failed(failure_after(0)), Outcome::Switch(_)),
        "nothing was generated, so nothing is metered"
    );
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).expect("name"),
            v.parse().expect("value"),
        );
    }
    h
}

/// G5. An invalid `x-oag-tier` does not discard the body's pin.
///
/// `.map(TierName::from).or_else(|| virtual_tier(...))` never reached the
/// body once the header was present, valid or not — so a typo in
/// `x-oag-tier` silently threw away an `oag/frontier` in the request body
/// and the request fell back to classification. Two pins, one of them
/// correct, and the wrong one won by being in the wrong place.
///
/// G6 and G7 ride along: both are single lines whose absence is invisible
/// at runtime without a live provider, a live breaker and a seat.
#[test]
fn the_tier_header_is_validated_before_the_body_is_consulted() {
    let src = include_str!("plan.rs");
    let body = src
        .split_once("let header_tier = headers")
        .expect("the resolution is in this file")
        .1;
    let block = &body[..body.find("signal.explicit_tier").unwrap_or(body.len())];

    let filtered = block
        .find(".filter(|n| policy.rung(n).is_some())")
        .expect("the header is checked against the ladder");
    let fallback = block
        .find(".or_else(|| virtual_tier(&canonical.model))")
        .expect("the body is the fallback");
    assert!(
        filtered < fallback,
        "checking after the fallback means the fallback never runs, which is \
             the whole finding"
    );
}

/// G6. The refresher follows the adapter, not the provider.
#[test]
fn a_seat_refreshes_against_its_own_adapter() {
    let src = include_str!("refresh.rs");
    assert!(
        src.contains("crate::gateway::adapter_for(state, row.provider.parse()?, row)"),
        "a Codex seat is `Provider::OpenAI` and refreshes against ChatGPT's \
             token endpoint; asking by provider alone hands its refresh token to \
             the wrong grant"
    );
}

/// G7's other arm: the transport failures retry against the breaker too.
///
/// The re-check went onto the `Step::Retry` arm — the one reached from an
/// upstream *response* — and not onto the arm below it, which is where a
/// connect, TLS or DNS failure lands. Both loop back to the same credential
/// and both have just recorded a failure that may have opened its breaker,
/// so a credential that tripped on a connect error still received every
/// remaining retry: exactly the traffic a breaker exists to stop, aimed at
/// the credential it had this moment decided was unhealthy.
///
/// A source scan for the same reason its sibling is one: reaching either
/// arm needs a live upstream failing in a specific way against a real
/// lease. What is checkable is that both arms ask, and that is what this
/// asks.
#[test]
fn both_retry_arms_re_ask_the_breaker() {
    // The function's own text, cut before this module — a scan whose
    // haystack includes the test doing the scanning counts its own string
    // literals, which is how the first version of this passed with the fix
    // reverted. It found that on its first revert-check.
    let src = include_str!("failover.rs");
    let code = src
        .split_once("\n#[cfg(test)]\n")
        .map_or(src, |(code, _)| code);
    let body = code
        .split_once("async fn try_credential(")
        .expect("the retry loop is in this file")
        .1;
    let loop_body = &body[..body.find("\n}\n").unwrap_or(body.len())];

    // Both arms that loop back to the same credential, and only those.
    assert_eq!(
        loop_body
            .matches("tokio::time::sleep(backoff(attempt)).await")
            .count(),
        2,
        "a third arm sleeping into another attempt is one this does not \
             know to check: {loop_body}"
    );
    assert_eq!(
        loop_body
            .matches("state.breakers.permits(account, now)")
            .count(),
        2,
        "one on the response-failure arm and one on the transport arm; \
             either without the other is a credential retried past its own \
             breaker on half the ways a request can fail"
    );

    // And each check must come before the sleep it guards, not after it.
    for (i, segment) in loop_body
        .split("tokio::time::sleep(backoff(attempt)).await")
        .enumerate()
        .take(2)
    {
        assert!(
            segment.contains("!state.breakers.permits(account, now)"),
            "the retry at sleep {i} goes out without asking the breaker \
                 about the failure it has just recorded"
        );
    }
}

/// G7. Every same-credential retry asks the breaker, not just the first.
#[test]
fn a_retry_rechecks_the_breaker_it_may_have_just_tripped() {
    let src = include_str!("failover.rs");
    let arm = src
        .split_once("Step::Retry => {")
        .expect("the retry arm is in this file")
        .1;
    let block = &arm[..arm.find("Step::Escalate").unwrap_or(arm.len())];
    assert!(
        block.contains("!state.breakers.permits(account, now)"),
        "`Dispatch::claim` is taken once above the loop, so without this a \
             credential that has just tripped its breaker still receives every \
             remaining retry — the traffic a breaker exists to stop, aimed at the \
             credential it has just called unhealthy"
    );
}

/// G3's wiring: the streamed path hands the ledger the gate that caused
/// the climb.
///
/// The rule itself — triggering gate outranks the accumulator's — is tested
/// behaviourally in `meter`, through the row it produces. What no test
/// reached was whether `stream_response` passes the gate at all, and that
/// is the half G3 changed: the collected path had threaded it for a while
/// and this one had not, so every streamed request that climbed a rung
/// recorded no reason for having climbed.
///
/// A source scan, and it is a source scan for the same reason R1's is:
/// `stream_response` takes a live `reqwest::Response`, a lease and a
/// state, none of which a unit test can produce. Deleting the argument is
/// what this catches; the rule is covered where it can be run.
#[test]
fn the_streamed_path_hands_the_ledger_its_triggering_gate() {
    let src = include_str!("respond.rs");
    let body = src
        .split_once("fn stream_response(")
        .expect("declared in this file")
        .1;
    let path = &body[..body.find("\n}\n").unwrap_or(body.len())];

    assert!(
        path.contains("meter::record(&state2, &ctx, &outcome, triggering_gate)"),
        "the streamed path must pass the gate that caused the escalation, \
             not leave the ledger to read it from an accumulator that is silent \
             exactly when the climb worked"
    );
}

/// R1, the wiring. The selection error path consults `disposition`.
///
/// Reads this file's own source, because reaching that branch needs a
/// gateway with a real route, a real ladder, and a credential pool that is
/// empty in the specific way the finding is about — a fixture larger and
/// less reliable than the thing it would prove. `oag-router` pins the
/// classification and the escalation this depends on; what is left is that
/// anything asks, and this is that.
#[test]
fn the_selection_error_path_asks_the_disposition() {
    let src = include_str!("climb.rs");
    let body = src
        .split_once("async fn run_with_escalation(")
        .expect("the loop is in this file")
        .1;
    let path = &body[..body.find("\n}\n").unwrap_or(body.len())];

    assert!(
        path.contains("e.disposition(), oag_core::Disposition::EscalateTier"),
        "without this the disposition describes a behaviour the gateway does \
             not have, which is how it got that way"
    );
    assert!(
        path.contains("policy.escalate_past_provider("),
        "the climb must skip every rung on the provider that just failed, not \
             stop at the first one: a `[kimi, kimi-2, anthropic]` ladder returned \
             503 while the frontier rung could have served. The rule itself lives \
             in `oag-router` and is tested there against a real ladder; what this \
             pins is that the error path still calls it"
    );
    assert!(
        path.contains("oag_router::climb_allowed(&decision.reason)"),
        "a caller who named a model must not be moved onto another \
             provider's, however unavailable theirs is"
    );

    // Both guards, and the same call the quality-gate path makes.
    //
    // This branch checked the escalation count alone, so a principal the
    // router had just downgraded for being near their cap — whose reason
    // `climb_allowed` permits — was promoted to a rung fifteen times
    // dearer the moment their cheap rung had no credential, and debited
    // against the budget the downgrade was protecting. Two paths asking one
    // question and answering it differently is the defect; the assertion is
    // that they now make the same call.
    let guard = "oag_router::escalation_allowed(pressure, escalations, MAX_ESCALATIONS)";
    assert!(
        path.contains(guard),
        "the selection-failure climb must ask the budget what the quality-gate \
             climb asks it"
    );
    // The module's code, not its tests — this assertion's own string
    // literal is a match otherwise, and a count that includes the thing
    // doing the counting is not a count.
    let code = src
        .split_once("\n#[cfg(test)]\n")
        .map_or(src, |(code, _)| code);
    assert_eq!(
        code.matches(guard).count(),
        2,
        "once on each climb; a third call site means somewhere else is \
             deciding this and is not covered here"
    );
}

/// G4. Budget pressure is the only thing that suppression counts.
///
/// `oag_escalations_suppressed_total` answers one question: how much answer
/// quality is my budget costing me. It was incremented whenever a gate
/// tripped and the principal happened to be near their cap — regardless of
/// whether the budget had anything to do with not climbing. Four other
/// things stop a climb, and on a constrained principal every one of them
/// was counted as a budget suppression, so the number was mostly made of
/// requests the budget did not affect.
///
/// The condition in `run_with_escalation` re-asks `should_climb` with
/// `Normal` substituted for the real pressure. This pins the substitution's
/// arithmetic: the cases below are the ones where the answer differs, and
/// the ones where it does not.
#[test]
fn only_a_climb_that_budget_alone_prevented_is_a_suppression() {
    use oag_router::{BudgetPressure, QualityGate};

    // Through the predicate the counter actually consults, not through
    // `should_climb` alone. `should_climb` is not what G4 changed: the fix
    // was asking it a second time with `Normal` substituted, and asking
    // `escalate` as well, and a test that calls `should_climb` directly
    // passes with both of those deleted.
    //
    // A ladder with somewhere to go, so the escalate half is satisfied and
    // the pressure is genuinely the only thing in the way.
    let policy = suppression_policy();
    let catalog = suppression_catalog();
    let served = std::collections::HashSet::new();
    let signal = oag_router::RequestSignal::default();
    let cheap = decision_on_rung("cheap", 0);

    assert!(
        budget_alone_prevented_the_climb(
            Some(QualityGate::Refusal),
            BudgetPressure::Constrained,
            &cheap,
            0,
            1024,
            &policy,
            &signal,
            &catalog,
            &served,
        ),
        "everything else allows the climb and only the pressure stops it: \
             that is the one case the counter is for"
    );

    // Not a suppression: no pressure at all.
    assert!(!budget_alone_prevented_the_climb(
        Some(QualityGate::Refusal),
        BudgetPressure::Normal,
        &cheap,
        0,
        1024,
        &policy,
        &signal,
        &catalog,
        &served,
    ));

    // Not a suppression: the climb budget is already spent, so headroom
    // would have changed nothing. This is one of the four other blockers
    // that used to be counted as the budget's doing.
    assert!(
        !budget_alone_prevented_the_climb(
            Some(QualityGate::Refusal),
            BudgetPressure::Constrained,
            &cheap,
            MAX_ESCALATIONS,
            1024,
            &policy,
            &signal,
            &catalog,
            &served,
        ),
        "an exhausted escalation budget stops this climb whatever the cap is"
    );

    // Not a suppression: at the ceiling there is no rung above, and a gate
    // with nowhere to go is not the budget's fault either.
    assert!(
        !budget_alone_prevented_the_climb(
            Some(QualityGate::Refusal),
            BudgetPressure::Constrained,
            &decision_on_rung("frontier", 2),
            0,
            1024,
            &policy,
            &signal,
            &catalog,
            &served,
        ),
        "a gate with no rung above it is not a suppression whatever the budget says"
    );

    // Not a suppression: no gate tripped, so nothing was prevented.
    assert!(!budget_alone_prevented_the_climb(
        None,
        BudgetPressure::Constrained,
        &cheap,
        0,
        1024,
        &policy,
        &signal,
        &catalog,
        &served,
    ));

    // `should_climb` itself is not re-tested here. It is not what G4
    // changed — the fix was asking it again with `Normal` substituted, and
    // asking `escalate` too — and it has its own coverage where the climb
    // decision is made.
}

#[test]
fn every_ecosystems_auth_header_is_accepted() {
    // Rejecting any of these makes that ecosystem's SDK unusable.
    assert_eq!(
        extract_key(&headers(&[("authorization", "Bearer oag_live_1")])),
        Some("oag_live_1")
    );
    assert_eq!(
        extract_key(&headers(&[("x-api-key", "oag_live_2")])),
        Some("oag_live_2")
    );
    assert_eq!(
        extract_key(&headers(&[("x-goog-api-key", "oag_live_3")])),
        Some("oag_live_3")
    );
    assert_eq!(extract_key(&HeaderMap::new()), None);
}

#[test]
fn a_non_bearer_authorization_falls_through() {
    assert_eq!(
        extract_key(&headers(&[("authorization", "Basic abc")])),
        None
    );
}

#[test]
fn a_gemini_body_that_is_not_an_object_is_a_client_error() {
    // The router no longer reaches this without a key, so the guard is
    // asserted here rather than through a request. Every one of these
    // used to panic inside `IndexMut`, severing the connection — which on
    // HTTP/2 resets every sibling stream multiplexed onto it. `null` did
    // not panic; `IndexMut` silently turned it into an object.
    for body in ["[]", "123", "\"x\"", "true", "null", "{", ""] {
        let Err(err) = require_object_body(body.as_bytes()) else {
            panic!("a body of {body} is not a request and must not be accepted as one");
        };
        assert!(
            matches!(err, Error::Serde(_)),
            "a body of {body} must map to a 400, got {err}"
        );
    }
}

#[test]
fn a_gemini_object_body_passes_the_guard() {
    // The other half: it must refuse the shape the pipeline cannot read and
    // nothing else. A guard that rejected this would refuse every real
    // request.
    assert!(
        require_object_body(br#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#).is_ok()
    );
}

#[test]
fn an_empty_answer_is_an_error_only_when_we_rendered_it() {
    use oag_core::provider::Dialect::{AnthropicMessages as A, OpenAIChatCompletions as O};
    use oag_router::QualityGate::{EmptyResponse, Truncated};
    let body = bytes::Bytes::from_static(b"{}");
    let none = bytes::Bytes::new();
    // The provider's own body, passed through: what the provider said.
    assert!(!client_got_nothing(Some(EmptyResponse), false, A, A, &body));
    // Ours: rendered from a stream, across dialects, or from no body.
    assert!(client_got_nothing(Some(EmptyResponse), true, A, A, &body));
    assert!(client_got_nothing(Some(EmptyResponse), false, O, A, &body));
    assert!(client_got_nothing(Some(EmptyResponse), false, A, A, &none));
    // Not empty, or no gate: nothing to report.
    assert!(!client_got_nothing(Some(Truncated), true, O, A, &none));
    assert!(!client_got_nothing(None, true, O, A, &none));
}

#[test]
fn backoff_grows_and_is_capped() {
    assert!(backoff(0) < backoff(1));
    assert!(backoff(1) < backoff(2));
    assert!(backoff(9) <= std::time::Duration::from_secs(3));
}

#[test]
fn truncation_never_splits_a_character() {
    let s = "é".repeat(400);
    let out = truncate(&s, 51);
    assert!(out.len() <= 54);
    assert!(std::str::from_utf8(out.as_bytes()).is_ok());
}

#[test]
fn short_bodies_are_not_truncated() {
    assert_eq!(truncate("brief", 512), "brief");
}

#[test]
fn no_viable_model_names_the_route_and_the_fix() {
    let ladder = TierLadder::new(vec![oag_router::ladder::Rung {
        name: oag_core::TierName::from("cheap"),
        models: vec![oag_router::ModelId::new("anthropic/claude-haiku-4.5")],
    }])
    .expect("ladder");
    let msg = no_viable_message("default", "xai/grok-4.3", &ladder);
    assert!(msg.contains("route 'default'"), "{msg}");
    assert!(msg.contains("no xai models"), "{msg}");
    assert!(
        msg.contains("oag admin route tiers --route default cheap=xai/grok-4.3"),
        "{msg}"
    );
}

#[test]
fn transport_error_records_breaker_failure() {
    // A credential behind a dead proxy never returns a status, so nothing
    // on the HTTP path records anything against it. Left unrecorded it
    // fails fastest, looks idlest, and the least-loaded stage keeps picking
    // it — for ever, because it never trips.
    let breakers = Breakers::new();
    let account = AccountId::new();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();

    for _ in 0..64 {
        transport_failure(&breakers, account, true);
    }
    assert!(
        !breakers.permits(account, now),
        "a run of unreachable attempts must trip the breaker"
    );
    assert_eq!(breakers.open_count(now), 1);
}

#[test]
fn a_transport_failure_cools_the_credential_down_once_retries_are_spent() {
    // Only at the end: a cooldown written between two attempts on the same
    // credential is contradicted by the very next attempt.
    let breakers = Breakers::new();
    let account = AccountId::new();

    assert_eq!(transport_failure(&breakers, account, true), None);
    assert_eq!(
        transport_failure(&breakers, account, false),
        Some(Disposition::FailoverAccount {
            cooldown: TRANSPORT_COOLDOWN
        }),
        "the same failover the HTTP path applies"
    );
}

fn upstream(status: u16, body: &str) -> Error {
    Error::Upstream {
        provider: oag_core::Provider::Anthropic,
        account: AccountId::new(),
        status,
        body: body.to_owned(),
        retry_after: None,
    }
}

/// What the client actually parses. A status assertion alone would have
/// missed the double-encoded body this file used to send.
async fn json_body(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a complete body");
    serde_json::from_slice(&bytes).expect("an error envelope is JSON")
}

fn retry_after_of(response: &Response) -> Option<&str> {
    response
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
}

#[test]
fn upstream_413_is_outcome_escalate_not_fatal() {
    // A 413 from a 128k rung used to end the request: `EscalateTier` was
    // mapped to `Fatal` here and never reached `policy.escalate`, so the
    // client got the provider's rejection while the rung that could have
    // held the prompt sat one step up, untried.
    assert_eq!(
        step_for(upstream(413, "").disposition(), false),
        Step::Escalate
    );
    assert_eq!(
        step_for(
            upstream(400, "prompt is too long: 210000 tokens").disposition(),
            false
        ),
        Step::Escalate
    );
}

#[test]
fn a_bad_request_still_fails_the_caller() {
    // The other half of the same decision: escalation costs money, so only
    // a capability rejection buys a rung.
    assert_eq!(
        step_for(upstream(400, "messages: required").disposition(), false),
        Step::Fatal
    );
}

#[test]
fn an_unhealthy_credential_is_switched_rather_than_escalated() {
    // Escalating here would migrate the fleet onto expensive models every
    // time a provider had a bad afternoon.
    assert_eq!(
        step_for(upstream(503, "").disposition(), false),
        Step::Switch
    );
    assert_eq!(
        step_for(upstream(429, "").disposition(), false),
        Step::Switch
    );
    // Transient, and the retry budget decides which of the two it is.
    assert_eq!(step_for(upstream(408, "").disposition(), true), Step::Retry);
    assert_eq!(
        step_for(upstream(408, "").disposition(), false),
        Step::Switch
    );
}

/// H4: the three unserved writes must not ride on the request future.
///
/// They were inline `.await`s on the same future hyper drops when a client
/// hangs up, so a provider that had already generated — and would already
/// invoice — those tokens left no ledger row at all. For the abandoned rows
/// this is the last chance they get.
///
/// The guard is taken synchronously, before the spawn, which is what makes
/// this deterministic rather than a race: the moment `spawn_unserved`
/// returns, the work is counted as in flight, so a drain waits for it and
/// dropping the caller cannot take it away.
#[tokio::test]
async fn unserved_rows_are_spawned_off_the_request_future() {
    let state = state();
    let ctx = meter::Context {
        request_id: RequestId::new(),
        auth: auth_context(),
        decision: decision_for(oag_core::Provider::Anthropic),
        account: oag_core::AccountId::new(),
        started: std::time::Instant::now(),
        attempt: 0,
        flat_rate: false,
    };
    let accumulator = oag_proto::StreamAccumulator::new();
    let abandoned = meter::abandon(
        ctx.clone(),
        &accumulator,
        oag_router::QualityGate::EmptyResponse,
    );
    let lost = meter::lose(
        ctx,
        &accumulator,
        &oag_core::Error::Internal("stream lost".to_owned()),
    );

    let before = state.lifecycle.in_flight();

    // Called from inside a future that is then dropped without ever being
    // polled to completion — the shape of a client hang-up.
    let caller = {
        let state = Arc::clone(&state);
        async move {
            spawn_unserved(&state, Some(abandoned), vec![lost]);
            std::future::pending::<()>().await;
        }
    };
    let mut caller = Box::pin(caller);
    // One poll: enough to reach `spawn_unserved` and park on `pending`.
    std::future::poll_fn(|cx| {
        let _ = caller.as_mut().poll(cx);
        std::task::Poll::Ready(())
    })
    .await;

    let tracked = state.lifecycle.in_flight();
    assert!(
        tracked > before,
        "the unserved writes are not tracked as in-flight work, so a drain \
             will not wait for them and an inline await would be cancelled with \
             the request: {before} -> {tracked}"
    );

    drop(caller);
    assert!(
        state.lifecycle.in_flight() > before,
        "the writes went with the request future when it was dropped — \
             which is H4 exactly: the provider invoices the tokens and the \
             ledger has no row"
    );
}

/// H4's other half: the call sites, which the test above cannot see.
///
/// `unserved_rows_are_spawned_off_the_request_future` proves
/// `spawn_unserved` detaches. It says nothing about whether
/// `run_with_escalation` uses it — put an inline `.await` back on any of the
/// three exits and that test stays green while the rows go with the dropped
/// request future, which is H4 exactly.
///
/// A source scan, and it says so: the three exits are a client hang-up, a
/// budget refusal and an exhausted ladder, each reached only by driving a
/// real request to a real upstream failure. What is checkable without that
/// is the shape of the code, so that is what this checks.
///
/// The served path's own `record_abandoned` / `record_lost` are inline on
/// purpose and are not a violation: they sit inside the `tokio::spawn` that
/// already writes the served row, after it, so the answer the client got is
/// in the ledger before the attempts that failed to be it. The assertion is
/// therefore about position, not about absence.
#[test]
fn every_unserved_exit_detaches_its_writes() {
    let src = include_str!("climb.rs");
    let body = src
        .split_once("async fn run_with_escalation(")
        .expect("the function is in this file")
        .1;
    let body = &body[..body.find("\n}\n").unwrap_or(body.len())];

    assert_eq!(
        body.matches("spawn_unserved(state, abandoned, lost);")
            .count(),
        3,
        "three exits leave without serving — a hang-up, a budget refusal \
             and an exhausted ladder — and each owes the ledger its attempts"
    );

    // The served path's spawn. Everything after it is already detached.
    let detached = body
        .find("let state2 = Arc::clone(state);")
        .expect("the served path spawns its writes");
    for call in ["meter::record_abandoned(", "meter::record_lost("] {
        let sites: Vec<usize> = body.match_indices(call).map(|(i, _)| i).collect();
        assert_eq!(
            sites.len(),
            1,
            "{call} appears {} times; every unserved exit should be going \
                 through `spawn_unserved`",
            sites.len()
        );
        assert!(
            sites[0] > detached,
            "{call} is awaited on the request's own future, so a client \
                 that hangs up takes the row with it and the provider invoices \
                 tokens the ledger never heard of"
        );
    }
}

/// A three-rung ladder with somewhere to climb to, for the suppression
/// predicate: the question it asks is counterfactual, so the fixture has to
/// make the *other* blockers absent rather than merely unlikely.
fn suppression_catalog() -> oag_router::Catalog {
    use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
    let model = |id: &str, input: rust_decimal::Decimal| ModelSpec {
        id: ModelId::new(id),
        provider: oag_core::Provider::Anthropic,
        upstream_name: id.split('/').next_back().unwrap_or(id).to_owned(),
        pricing: Pricing {
            input_per_mtok: input,
            output_per_mtok: input,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        },
        context_window: 200_000,
        max_output_tokens: 8192,
        capabilities: Capabilities {
            vision: true,
            tools: true,
            reasoning: true,
            prompt_cache: true,
        },
        display_label: None,
    };
    oag_router::Catalog::from_entries([
        model("anthropic/haiku", rust_decimal::Decimal::ONE),
        model("anthropic/sonnet", rust_decimal::Decimal::from(3)),
        model("anthropic/opus", rust_decimal::Decimal::from(15)),
    ])
}

fn suppression_policy() -> RoutingPolicy {
    use oag_core::TierName;
    use oag_router::{ModelId, ladder::Rung};
    let ladder = TierLadder::new(vec![
        Rung {
            name: TierName::new("cheap"),
            models: vec![ModelId::new("anthropic/haiku")],
        },
        Rung {
            name: TierName::new("balanced"),
            models: vec![ModelId::new("anthropic/sonnet")],
        },
        Rung {
            name: TierName::new("frontier"),
            models: vec![ModelId::new("anthropic/opus")],
        },
    ])
    .expect("non-empty");
    RoutingPolicy::new(ladder, Box::new(oag_router::HeuristicClassifier::default()))
}

fn decision_on_rung(rung: &str, index: u8) -> RoutingDecision {
    let mut d = decision_for(oag_core::Provider::Anthropic);
    d.model.id = oag_router::ModelId::new(match rung {
        "cheap" => "anthropic/haiku",
        "balanced" => "anthropic/sonnet",
        _ => "anthropic/opus",
    });
    d.model.max_output_tokens = 8192;
    d.tier = Some(oag_core::Tier::new(rung, index));
    d
}

fn decision_for(provider: oag_core::Provider) -> RoutingDecision {
    use oag_router::{Capabilities, ModelId, ModelSpec, Pricing};
    RoutingDecision {
        model: ModelSpec {
            id: ModelId::new("p/m"),
            provider,
            upstream_name: "m".to_owned(),
            pricing: Pricing {
                input_per_mtok: rust_decimal::Decimal::ONE,
                output_per_mtok: rust_decimal::Decimal::ONE,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: 1000,
            max_output_tokens: 100,
            capabilities: Capabilities::default(),
            display_label: None,
        },
        tier: Some(oag_core::Tier::new("cheap", 0)),
        reason: oag_router::SelectionReason::Classified,
        capability_escalated_from: None,
        ceiling_model: None,
    }
}

#[tokio::test]
async fn a_same_dialect_body_is_served_verbatim_as_json() {
    // The upstream's own bytes are the most faithful answer there is: a
    // same-dialect collected response goes out byte for byte, as JSON.
    let body = bytes::Bytes::from_static(br#"{"id":"msg_1","content":[]}"#);
    let response = json_response(
        &body,
        &[],
        None,
        &decision_for(oag_core::Provider::Anthropic),
        RequestId::new(),
        Dialect::AnthropicMessages,
        Dialect::AnthropicMessages,
        false,
        false,
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(axum::http::HeaderValue::as_bytes),
        Some(&b"application/json"[..])
    );
    let sent = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .expect("body");
    assert_eq!(sent, body);
}

#[test]
fn a_client_reaches_a_same_dialect_upstream_without_re_serialising() {
    // The bug this pins: OpenAI declared its dialect as Responses while the
    // adapter serving it spoke Chat Completions, so this case never took
    // the passthrough path and round-tripped every frame for nothing.
    let d = decision_for(oag_core::Provider::OpenAI);
    let e = egress_for(
        Dialect::OpenAIChatCompletions,
        &d,
        RequestId::new(),
        oag_upstream::Framing::Sse,
        Dialect::OpenAIChatCompletions,
        false,
    )
    .expect("supported");
    assert!(matches!(
        e,
        sse::Egress::Passthrough {
            dialect: Dialect::OpenAIChatCompletions,
            ..
        }
    ));
}

#[test]
fn rewritten_tool_names_disable_byte_passthrough() {
    // luna 400: we sanitise `user-Github.get_file` on the way out. If we
    // then forwarded the upstream's bytes, the client would see the wire
    // name and fail to dispatch. Same dialect is not sufficient.
    let d = decision_for(oag_core::Provider::OpenAI);
    let e = egress_for(
        Dialect::OpenAIChatCompletions,
        &d,
        RequestId::new(),
        oag_upstream::Framing::Sse,
        Dialect::OpenAIChatCompletions,
        true,
    )
    .expect("supported");
    assert!(
        matches!(e, sse::Egress::ChatCompletions { .. }),
        "must re-render so restored names reach the client"
    );
}

#[test]
fn a_binary_framed_upstream_is_never_passed_through() {
    // Bedrock's dialect *is* Anthropic, so dialect alone would say
    // passthrough — and hand a client expecting `data:` lines a
    // length-prefixed binary envelope.
    let d = decision_for(oag_core::Provider::Bedrock);
    let e = egress_for(
        Dialect::AnthropicMessages,
        &d,
        RequestId::new(),
        oag_upstream::Framing::AwsEventStream,
        Dialect::AnthropicMessages,
        false,
    )
    .expect("supported");
    assert!(
        matches!(e, sse::Egress::AnthropicMessages { .. }),
        "must be rendered, not forwarded"
    );
}

#[test]
fn a_cross_dialect_pair_selects_the_client_s_renderer() {
    let d = decision_for(oag_core::Provider::Anthropic);
    let e = egress_for(
        Dialect::OpenAIChatCompletions,
        &d,
        RequestId::new(),
        oag_upstream::Framing::Sse,
        Dialect::AnthropicMessages,
        false,
    )
    .expect("supported");
    assert!(matches!(e, sse::Egress::ChatCompletions { .. }));
}

#[test]
fn a_codex_seat_is_rendered_rather_than_forwarded_to_a_chat_client() {
    // THE BUG THIS PINS. A Codex subscription is `Provider::OpenAI`, whose
    // native dialect is Chat Completions, but the adapter serving it speaks
    // Responses. While this function asked the PROVIDER, the two dialects
    // appeared to agree and the Responses bytes were forwarded verbatim —
    // a 200 that a Chat Completions client reads as an empty answer, which
    // is how it reached a person as "the model is broken".
    //
    // The upstream dialect is a parameter now precisely so this case can
    // differ from the provider's.
    let d = decision_for(oag_core::Provider::OpenAI);
    let e = egress_for(
        Dialect::OpenAIChatCompletions,
        &d,
        RequestId::new(),
        oag_upstream::Framing::Sse,
        Dialect::OpenAIResponses,
        false,
    )
    .expect("supported");
    assert!(
        matches!(e, sse::Egress::ChatCompletions { .. }),
        "must be rendered into the client's dialect, not forwarded"
    );
}

#[test]
fn the_codex_adapter_says_it_speaks_responses() {
    // The other half: the parameter above is only right if the adapter
    // reports its own dialect rather than inheriting the provider's.
    use oag_upstream::ProviderAdapter;
    let codex = oag_upstream::codex::CodexAdapter::new();
    assert_eq!(codex.provider(), oag_core::Provider::OpenAI);
    assert_eq!(codex.dialect(), Dialect::OpenAIResponses);
    assert_ne!(codex.dialect(), codex.provider().native_dialect());
    // And that it streams regardless of the client: the third adapter
    // fact the response path has to ask for rather than assume.
    assert!(codex.always_streams());
    assert!(!oag_upstream::AnthropicAdapter::default().always_streams());
}

#[test]
fn every_client_dialect_gets_a_body_with_the_answer_in_it() {
    // THE MATRIX. One answer as canonical events — text, a whole tool
    // call, a stop, usage — rendered for each client dialect, then read
    // back by that dialect's own reader. Every body must carry the text
    // and the tool call and non-zero usage. Before the hub, the converter
    // was picked by the client's dialect and handed whatever the upstream
    // sent, and for eight of the twelve pairs that body had a shape the
    // converter did not read: a well-formed, fully-billed, empty 200.
    use oag_proto::{StopReason, StreamEvent};
    let events = [
        StreamEvent::UsageUpdate {
            usage: oag_router::Usage {
                input_tokens: 1000,
                cache_read_tokens: 200,
                ..oag_router::Usage::default()
            },
        },
        StreamEvent::TextDelta {
            text: "Reading it now.".to_owned(),
        },
        StreamEvent::ToolUseStart {
            id: "toolu_1".to_owned(),
            name: "read_file".to_owned(),
        },
        StreamEvent::ToolUseDelta {
            id: "toolu_1".to_owned(),
            partial_json: r#"{"path":"a.rs"}"#.to_owned(),
        },
        StreamEvent::ToolUseEnd {
            id: "toolu_1".to_owned(),
        },
        StreamEvent::Stop {
            reason: StopReason::ToolUse,
            usage: oag_router::Usage {
                output_tokens: 42,
                ..oag_router::Usage::default()
            },
        },
    ];

    for ingress in [
        Dialect::AnthropicMessages,
        Dialect::OpenAIChatCompletions,
        Dialect::GeminiGenerateContent,
        Dialect::OpenAIResponses,
    ] {
        let bytes = render_collected(&events, ingress, "req1", "oag/auto")
            .unwrap_or_else(|e| panic!("{ingress:?}: {e}"));
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{ingress:?}: not JSON: {e}"));

        // Read back with the dialect's own reader — the same one the
        // gateway would use if this body came from an upstream.
        let back = match ingress {
            Dialect::AnthropicMessages => oag_proto::anthropic::parse_response(&v),
            Dialect::OpenAIChatCompletions => oag_proto::openai::parse_response(&v),
            Dialect::GeminiGenerateContent => oag_proto::gemini::parse_response(&v),
            Dialect::OpenAIResponses => oag_proto::responses::parse_response(&v),
            _ => unreachable!(),
        };
        let text: String = back
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Reading it now.", "{ingress:?} lost the text: {v}");
        assert!(
            back.iter().any(|e| matches!(
                e, StreamEvent::ToolUseStart { name, .. } if name == "read_file"
            )),
            "{ingress:?} lost the tool call: {v}"
        );
        let mut acc = oag_proto::StreamAccumulator::new();
        for e in &back {
            acc.observe(e);
        }
        assert!(
            acc.usage().output_tokens > 0,
            "{ingress:?} lost the usage: {v}"
        );
        assert_eq!(acc.quality_gate(), None, "{ingress:?}: {v}");
    }
}

#[test]
fn a_client_dialect_with_no_converter_is_an_error_not_the_upstream_body() {
    // `Dialect` is non-exhaustive; nothing constructs an unknown one here,
    // so what this pins is that an *empty* answer still renders as a
    // well-formed body rather than a crash — the crash being the one
    // silence worse than the empty 200.
    let bytes = render_collected(&[], Dialect::OpenAIChatCompletions, "r", "m").expect("renders");
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(v["choices"].is_array());
}

/// A state that dials nothing: `Db::connect` builds a lazy pool and
/// `Cache::connect` only opens a redis client, so the adapter lookup these
/// tests are about runs long before any backend would.
fn state() -> Arc<AppState> {
    crate::testing::state("")
}

fn auth_context() -> oag_store::AuthContext {
    oag_store::AuthContext {
        api_key_id: uuid::Uuid::nil(),
        principal_id: uuid::Uuid::nil(),
        route_id: uuid::Uuid::nil(),
        key_floor_tier: None,
        admin: false,
        quota_usd: None,
        principal_budget_usd: None,
        principal_hard_stop_multiple: rust_decimal::Decimal::ONE,
        expires_at: None,
    }
}

#[tokio::test]
async fn streaming_adapter_or_egress_error_releases_slot() {
    // `vertex` is a routable provider with no adapter registered, so the
    // streaming arm fails *after* a credential has been leased — the same
    // shape as a dialect pair with no renderer. Both used to return past
    // every release, stranding the slot for the whole SLOT_TTL; eight of
    // those on one credential and it answers AtCapacity with nothing in
    // flight.
    let state = state();
    let slots = Arc::new(select::testing::CountingSlots::default());

    let result = stream_response(
        &state,
        reqwest::Response::from(http::Response::new("stub")),
        select::testing::lease(&slots),
        &auth_context(),
        &decision_for(oag_core::Provider::Vertex),
        RequestId::new(),
        Instant::now(),
        0,
        Dialect::AnthropicMessages,
        None,
        state.lifecycle.track(),
        oag_proto::FunctionNameMap::identity(),
    );

    assert!(result.is_err(), "there is no adapter for vertex");
    assert_eq!(slots.settled().await, 1, "and the slot came back");
}

#[tokio::test]
async fn a_live_stream_keeps_its_slot_until_the_pump_is_done() {
    // The other half of it. The lease is moved into the pump task rather
    // than dropped when the handler returns, because a handler returns as
    // soon as the headers are decided — releasing there would hand back a
    // slot that is still streaming, which oversubscribes the credential
    // rather than merely leaking from it.
    let state = state();
    let slots = Arc::new(select::testing::CountingSlots::default());

    // A body that never yields, so the pump is still running when the
    // assertion below looks.
    let body = reqwest::Body::wrap_stream(futures_util::stream::pending::<
        std::result::Result<bytes::Bytes, std::io::Error>,
    >());

    let result = stream_response(
        &state,
        reqwest::Response::from(http::Response::new(body)),
        select::testing::lease(&slots),
        &auth_context(),
        &decision_for(oag_core::Provider::Anthropic),
        RequestId::new(),
        Instant::now(),
        0,
        Dialect::AnthropicMessages,
        None,
        state.lifecycle.track(),
        oag_proto::FunctionNameMap::identity(),
    );

    assert!(result.is_ok());
    assert_eq!(slots.settled().await, 0, "still in flight");
}

#[test]
fn an_empty_ladder_is_rejected_rather_than_serving_nothing() {
    assert!(parse_ladder(&serde_json::json!([])).is_err());
    assert!(parse_ladder(&serde_json::json!("not a ladder")).is_err());
    assert!(parse_ladder(&serde_json::json!([{"name": "cheap", "models": ["kimi/k2"]}])).is_ok());
}

#[tokio::test]
async fn internal_errors_do_not_leak_their_message() {
    // They can carry connection strings and file paths. Asserting on the
    // status alone never checked the thing the test is named for.
    let response = error_response(&Error::Internal("postgres://user:pw@host/db".to_owned()));
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "internal_error");
    assert_eq!(body["error"]["message"], "internal error");
    assert!(
        !body.to_string().contains("postgres://"),
        "nothing from the error may reach the client: {body}"
    );
}

#[tokio::test]
async fn upstream_401_maps_to_502() {
    // The collision: 401 is *our* "your gateway key is wrong". A 401 here
    // means the provider credentials are expired, and every one has already
    // been tried — but an SDK reading the status deletes the gateway key the
    // operator just issued and sends the user to re-authenticate against a
    // key that was never the problem.
    let response = error_response(&upstream(
        401,
        r#"{"error":{"type":"authentication_error","message":"OAuth token has expired"}}"#,
    ));
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "upstream_error");
    // Still diagnosable: the provider's status is reported, just not as ours.
    assert_eq!(body["error"]["upstream_status"], 401);
    // And its body is a value a parser can walk into, not a string that
    // happens to hold JSON.
    assert_eq!(
        body["error"]["upstream"]["error"]["message"],
        "OAuth token has expired"
    );
    // Above all, not something the client mistakes for its own auth failure.
    assert_ne!(body["error"]["type"], "authentication_error");
}

#[test]
fn our_own_credential_failures_are_never_dressed_as_the_clients() {
    // 402 is BudgetExhausted, 403 is a route this key may not use, and 407
    // asks the client to authenticate to our proxy. Every one of them sends
    // the caller to fix something on their side that is already fine.
    for status in [401u16, 402, 403, 407] {
        let response = error_response(&upstream(status, ""));
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "upstream {status} must not become the client's {status}"
        );
    }
}

#[test]
fn a_request_the_client_can_fix_keeps_the_providers_status() {
    // The other half. These are about the bytes the client sent, so the
    // client is the only party who can act on them — and 413 in particular
    // is what the ladder failed to escalate past, which is worth saying
    // plainly rather than collapsing into "bad gateway".
    for status in [400u16, 413, 422] {
        let response = error_response(&upstream(status, ""));
        assert_eq!(response.status().as_u16(), status);
    }
}

#[tokio::test]
async fn upstream_429_forwards_retry_after() {
    // The provider told us how long to wait and we dropped it on the floor:
    // the header was only ever set for our own inbound throttle, so a
    // forwarded 429 reached the client with nothing to back off by.
    let response = error_response(&Error::Upstream {
        provider: oag_core::Provider::Anthropic,
        account: AccountId::new(),
        status: 429,
        body: r#"{"error":{"message":"rate limit"}}"#.to_owned(),
        retry_after: Some(std::time::Duration::from_secs(30)),
    });
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(retry_after_of(&response), Some("30"));
    assert_eq!(
        json_body(response).await["error"]["upstream"]["error"]["message"],
        "rate limit"
    );
}

#[test]
fn an_upstream_429_without_a_hint_still_names_a_wait() {
    // No header from the provider is not licence to omit ours: a 429 with
    // no Retry-After is an invitation to retry immediately.
    assert_eq!(
        retry_after_of(&error_response(&upstream(429, ""))),
        Some("1")
    );
}

#[test]
fn a_providers_retry_after_is_read_from_the_response() {
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    let mut h = HeaderMap::new();
    h.insert(RETRY_AFTER, HeaderValue::from_static("42"));
    assert_eq!(
        upstream_retry_after(&h),
        Some(std::time::Duration::from_secs(42))
    );

    // The HTTP-date form is legal and unparsed on purpose: the caller's
    // default beats a date read wrongly.
    h.insert(
        RETRY_AFTER,
        HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
    );
    assert_eq!(upstream_retry_after(&h), None);
    assert_eq!(upstream_retry_after(&HeaderMap::new()), None);
}

#[test]
fn an_unusable_retry_after_is_treated_as_no_header_at_all() {
    // This number is persisted as `rate_limited_until`, and
    // `repo::clear_cooldown` deliberately leaves that column alone — so
    // anything accepted here is a credential no operator can get back
    // without psql. It used to be accepted verbatim.
    for (value, why) in [
        (
            "0",
            "Cloudflare sends it, and it un-benches the credential the \
                 provider just throttled",
        ),
        (
            "1756000000",
            "an epoch timestamp in seconds holds the credential out until the 2080s",
        ),
        (
            "1756000000000",
            "the same mistake in milliseconds overflowed the clock and panicked",
        ),
        ("-5", "a negative wait is not a wait"),
        (
            "86400",
            "a day is longer than we will hold a credential out on a provider's word",
        ),
        ("3601", "one second past the ceiling is still past it"),
        ("", "no value"),
        ("later", "not a number"),
    ] {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_str(value).expect("a valid header value"),
        );
        assert_eq!(
            upstream_retry_after(&h),
            None,
            "Retry-After: {value} — {why}"
        );
    }

    // And the edges of what is accepted, so the bound is pinned from both
    // sides rather than only rejected from one.
    for secs in [1u64, MAX_RETRY_AFTER] {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_str(&secs.to_string()).expect("value"),
        );
        assert_eq!(
            upstream_retry_after(&h),
            Some(std::time::Duration::from_secs(secs))
        );
    }
}

#[test]
fn no_provider_header_can_bench_a_credential_past_the_ceiling() {
    // The arithmetic `apply_disposition` performs, without a database
    // behind it. The millisecond case used to panic on this very addition
    // and take the request task with it; the second case landed in 2081,
    // and 0 released the credential immediately.
    let now = time::OffsetDateTime::now_utc();
    let ceiling = now + std::time::Duration::from_secs(MAX_RETRY_AFTER);

    for value in [
        "1756000000000",
        "1756000000",
        "0",
        "-5",
        "99999999999999999999",
        "86400",
        "30",
    ] {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_str(value).expect("a valid header value"),
        );

        // Exactly what `apply_disposition` computes for a rate limit, and
        // it panics here rather than returning if the wait is absurd.
        let wait = upstream_retry_after(&h).unwrap_or(std::time::Duration::from_mins(1));
        let until = now + wait;

        assert!(
            until > now,
            "Retry-After: {value} must still hold the credential out for something"
        );
        assert!(
            until <= ceiling,
            "Retry-After: {value} must not outlast the ceiling"
        );
    }
}

/// Where the client-facing catalogue lives.
const ERROR_FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../deploy/test/api/errors.json"
);

/// Render every `Error` variant to the bytes a client actually receives, and
/// hold the committed fixture to them.
///
/// This is the mechanism behind `deploy/test/api/ERRORS.md`. Two ways it
/// stays honest, and both matter:
///
/// 1. `every_variant()` is exhaustive over `Error` — it lives in `oag-core`
///    where `#[non_exhaustive]` does not apply, and its match has no wildcard
///    arm. A new variant stops THAT crate compiling.
/// 2. This asserts the rendered shapes equal the committed file. A change to
///    a status code or a `type` string fails here rather than in somebody's
///    client, months later, as an unhandled case.
///
/// Regenerate deliberately after an intended change:
///
/// ```text
/// UPDATE_ERROR_FIXTURES=1 cargo test -p oag-server error_shape
/// ```
#[tokio::test]
async fn every_error_shape_matches_the_committed_catalogue() {
    let mut rendered = Vec::new();
    for error in oag_core::error::every_variant() {
        // The Rust variant that produced this shape. Worth recording because
        // the mapping is many-to-one and deliberately so: three different
        // internal failures all render as `internal_error` with the same
        // redacted message, and without this the fixture shows three
        // identical entries and no way to tell why.
        let variant = format!("{error:?}");
        let variant = variant
            .split(['(', ' ', '{'])
            .next()
            .unwrap_or("?")
            .to_owned();
        let response = error_response(&error);
        let status = response.status().as_u16();
        let retry_after = retry_after_of(&response).map(str::to_owned);
        let body = json_body(response).await;
        let kind = body["error"]["type"]
            .as_str()
            .expect("every envelope names its kind")
            .to_owned();

        // The envelope shape itself, asserted for every variant rather than
        // trusted: a client branches on these two fields and nothing else.
        assert_eq!(body["type"], "error", "{kind} lost its outer type");
        assert!(
            body["error"]["message"].is_string(),
            "{kind} has no message"
        );

        let mut entry = serde_json::json!({
            "type": kind,
            "status": status,
            "variant": variant,
            "body": body,
        });
        if let Some(wait) = retry_after {
            entry["retry_after_header"] = serde_json::json!(wait);
        }
        rendered.push(entry);
    }

    // Sorted so the file is stable and a diff shows a real change rather
    // than a reordering.
    rendered.sort_by(|a, b| {
        (a["type"].as_str(), a["variant"].as_str())
            .cmp(&(b["type"].as_str(), b["variant"].as_str()))
    });
    let actual = format!(
        "{}\n",
        serde_json::to_string_pretty(&rendered).expect("serialisable")
    );

    if std::env::var_os("UPDATE_ERROR_FIXTURES").is_some() {
        std::fs::write(ERROR_FIXTURES, &actual).expect("write the catalogue");
        return;
    }

    let expected = std::fs::read_to_string(ERROR_FIXTURES).unwrap_or_default();
    assert_eq!(
        actual.trim(),
        expected.trim(),
        "the error shapes a client receives have changed.\n\
             If that was intended, regenerate the catalogue and update ERRORS.md:\n\
             \n    UPDATE_ERROR_FIXTURES=1 cargo test -p oag-server error_shapes\n"
    );
}

/// The catalogue is only useful if the kinds are distinct. Two variants
/// rendering to one `type` would leave a client unable to tell them apart —
/// and the two would have to be handled differently, or they would not be
/// two variants.
#[tokio::test]
async fn no_two_errors_are_indistinguishable_to_a_client() {
    let mut seen: std::collections::HashMap<String, u16> = std::collections::HashMap::new();
    for error in oag_core::error::every_variant() {
        let response = error_response(&error);
        let status = response.status().as_u16();
        let body = json_body(response).await;
        let kind = body["error"]["type"].as_str().expect("a kind").to_owned();
        // Sharing a kind is allowed only when the status also matches: those
        // are deliberate groupings, like the two model-qualifier failures.
        if let Some(previous) = seen.insert(kind.clone(), status) {
            assert_eq!(
                previous, status,
                "`{kind}` is returned with two different statuses, so a \
                     client branching on it cannot know which it has"
            );
        }
    }
}

#[test]
fn the_first_attempt_is_never_refused_by_the_failover_budget() {
    // A request that arrives during a slow moment must still reach an
    // upstream. Refusing before trying at all would turn "this took a while"
    // into "this never happened", which is worse than the wait.
    assert!(may_try_another(
        0,
        Duration::from_mins(10),
        Duration::from_mins(2)
    ));
}

#[test]
fn another_credential_is_tried_while_the_budget_holds() {
    assert!(may_try_another(
        1,
        Duration::from_secs(30),
        Duration::from_mins(2)
    ));
}

#[test]
fn a_spent_budget_stops_the_next_credential_not_the_current_one() {
    // The point of the whole mechanism: once the time is gone, stop STARTING
    // work. Anything already in flight runs to completion elsewhere — this
    // decision is only ever consulted between attempts.
    assert!(!may_try_another(
        1,
        Duration::from_mins(2),
        Duration::from_mins(2)
    ));
    assert!(!may_try_another(
        3,
        Duration::from_secs(500),
        Duration::from_mins(2)
    ));
}

#[test]
fn budget_exhaustion_is_distinguishable_from_an_auth_failure() {
    // The client needs to tell "you are out of money" from "your key is
    // wrong": one is fixed by waiting, the other never is.
    assert_eq!(
        error_response(&Error::BudgetExhausted {
            scope: oag_core::BudgetScope::Principal,
        })
        .status(),
        StatusCode::PAYMENT_REQUIRED
    );
    assert_eq!(
        error_response(&Error::Unauthenticated).status(),
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn a_field_the_target_dialect_cannot_express_is_the_client_s_error() {
    // Not a 500: nothing is broken here, the request simply asked for
    // something the model it routed to has no way to do. And not a silent
    // success either — the whole point is that the caller is told.
    let r = error_response(&Error::UnsupportedField {
        field: "response_format",
        dialect: Dialect::AnthropicMessages,
    });
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn throttling_answers_429_and_says_how_long_to_wait() {
    let response = error_response(&Error::RateLimited {
        retry_after: std::time::Duration::from_millis(1500),
    });
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    // Rounded up: a client told to wait 1s when a token lands at 1.5s
    // simply comes back too early and is refused again.
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("2")
    );
}

#[test]
fn a_sub_second_wait_still_asks_for_at_least_one_second() {
    let response = error_response(&Error::RateLimited {
        retry_after: std::time::Duration::from_millis(1),
    });
    // Retry-After: 0 is an invitation to hot-loop.
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
}

#[test]
fn a_virtual_model_name_pins_its_rung_and_auto_pins_nothing() {
    // `oag/cheap` and `oag/frontier` used to be indistinguishable from
    // `oag/auto`: the prefix forced managed mode and the rung after it was
    // never read, so every virtual name meant "classify for me".
    assert_eq!(virtual_tier("oag/cheap"), Some(TierName::from("cheap")));
    assert_eq!(
        virtual_tier("oag/frontier"),
        Some(TierName::from("frontier"))
    );
    assert_eq!(virtual_tier("oag/auto"), None, "auto is the unpinned one");
    assert_eq!(virtual_tier("claude-opus-5"), None);
    assert_eq!(virtual_tier("anthropic/claude-opus-5"), None);
}
