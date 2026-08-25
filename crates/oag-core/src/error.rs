//! Errors, and the classification that drives retry and failover.

use crate::credential::CredentialKind;
use crate::{AccountId, Provider};
use std::time::Duration;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Which spend cap stopped a request.
///
/// Both caps apply to every request. Naming the binding one is the difference
/// between "raise this key's quota" and "raise this person's budget", which is
/// the first question an operator asks when a request comes back 402.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetScope {
    ApiKey,
    Route,
    Principal,
}

impl std::fmt::Display for BudgetScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ApiKey => "the quota on this API key",
            Self::Route => "the monthly budget for this route",
            Self::Principal => "the monthly budget for this principal",
        })
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("unknown provider: {0}")]
    UnknownProvider(String),

    #[error("configuration: {0}")]
    Config(String),

    #[error("no credential available for provider {provider} on this route")]
    NoCredential { provider: Provider },

    /// A model id carried an `@` qualifier nobody can parse.
    ///
    /// A client error, not something to shrug off. Ignoring the pin would route
    /// the request through whichever credential is cheapest — which is the one
    /// the caller wrote the qualifier to exclude — and they would never learn
    /// that the word they typed meant nothing here.
    #[error(
        "unknown model qualifier `@{qualifier}`; use one of {}",
        model_channel_qualifiers()
    )]
    UnknownModelChannel { qualifier: String },

    /// The qualifier parses, and this provider cannot be reached that way at
    /// all. Distinct from having no such credential *configured*: adding one is
    /// the fix for that, and there is no fix for this.
    #[error("{provider} cannot be reached through a {} credential", .kind.channel_label())]
    ChannelNotOffered {
        provider: Provider,
        kind: CredentialKind,
    },

    /// The route holds credentials for this provider, and none of the kind the
    /// request pinned.
    ///
    /// Deliberately not [`Error::NoCredential`]. "No credential for xai" sends
    /// an operator to look at a pool that has three healthy keys in it; naming
    /// the kind says which one is missing.
    #[error("no {} credential for {provider} on this route", .kind.channel_label())]
    NoCredentialOfKind {
        provider: Provider,
        kind: CredentialKind,
    },

    /// Credentials exist and are healthy — every one of them is simply busy.
    ///
    /// Distinct from `NoCredential` because the two need opposite responses. No
    /// credential is a configuration problem: someone must add one. At capacity
    /// is a sizing problem that resolves on its own, and the caller should
    /// retry rather than page anyone.
    #[error("all {candidates} credentials for {provider} are at their concurrency limit")]
    AtCapacity {
        provider: Provider,
        candidates: usize,
    },

    #[error("no model on the ladder satisfies the request")]
    NoViableModel,

    #[error("authentication failed")]
    Unauthenticated,

    #[error("{scope} exhausted")]
    BudgetExhausted { scope: BudgetScope },

    /// Inbound throttling: the caller's route is over its requests-per-minute
    /// limit. Distinct from `Disposition::RateLimited`, which is an *upstream*
    /// provider throttling us.
    #[error("route rate limit exceeded")]
    RateLimited { retry_after: Duration },

    /// A dialect path named an operation this gateway does not implement.
    /// Distinct from a bad request: the path parsed, the verb just is not one
    /// of ours, and answering 400 would send the caller looking at their body.
    #[error("unsupported action: {action}")]
    UnsupportedAction { action: String },

    /// The client set a request field the chosen upstream dialect has no way to
    /// put on the wire.
    ///
    /// A refusal rather than a silent drop, because the two are
    /// indistinguishable from the client's side and only one of them is
    /// debuggable: a caller who asked for a JSON object and received prose sees
    /// a model that ignored its instructions, not a gateway that removed them.
    #[error("{dialect} cannot express `{field}`, which this request set")]
    UnsupportedField {
        field: &'static str,
        dialect: crate::provider::Dialect,
    },

    #[error("upstream {provider} returned {status}")]
    Upstream {
        provider: Provider,
        account: AccountId,
        status: u16,
        body: String,
        /// The provider's own `Retry-After`, when it sent one.
        ///
        /// Carried on the error because the response it came from is long gone
        /// by the time either consumer needs it: the credential's cooldown and
        /// the client's own `Retry-After` are both guesses without it, and a
        /// 429 answered with a guess is a 429 answered badly.
        ///
        /// Must already be bounded by whoever builds this. It is added to the
        /// clock and persisted as a credential's `rate_limited_until`, so an
        /// unvalidated one either benches a working credential for decades or
        /// overflows the addition outright. See `upstream_retry_after`.
        retry_after: Option<Duration>,
    },

    #[error("upstream stream stalled after {0:?} with no data")]
    StreamIdle(Duration),

    #[error("serialisation: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("{0}")]
    Internal(String),
}

/// The qualifiers a model id may carry, as a client writes them.
///
/// Derived from the vocabulary rather than written out beside it, so the
/// message in [`Error::UnknownModelChannel`] cannot end up naming a qualifier
/// the parser does not accept — which is the one failure this error exists to
/// prevent the caller from having.
fn model_channel_qualifiers() -> String {
    CredentialKind::QUALIFIED
        .iter()
        .filter_map(|k| {
            k.qualifier()
                .map(|q| format!("@{q} ({})", k.channel_label()))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What to do about a failed attempt.
///
/// This is the single decision that separates a gateway that degrades from one
/// that just fails. It is a pure function of the error so it can be unit-tested
/// exhaustively without a network — sub2api's equivalent logic is spread across
/// a 2595-line service and is correspondingly hard to reason about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Transient. Retry the same credential after a backoff.
    RetrySameAccount,
    /// This credential is unhealthy. Cool it down, exclude it, pick another.
    FailoverAccount { cooldown: Duration },
    /// This credential is rate limited until a known time. Not its fault.
    RateLimited { retry_after: Option<Duration> },
    /// The model itself refused or could not cope. A better model might.
    EscalateTier,
    /// Nothing will help. Surface it to the caller.
    Fatal,
}

impl Error {
    /// Classify an upstream failure.
    ///
    /// The status-code mapping mirrors what sub2api learned the hard way, with
    /// one deliberate change: a 401 on a refreshable credential is a *cooldown*,
    /// not a permanent disable. An expiring OAuth token 401s routinely and
    /// disabling the account on the first one takes a healthy credential out of
    /// the pool until a human notices.
    #[must_use]
    pub fn disposition(&self) -> Disposition {
        const COOLDOWN: Duration = Duration::from_mins(10);

        match self {
            Self::Upstream {
                status,
                body,
                retry_after,
                ..
            } => match status {
                401 | 403 => Disposition::FailoverAccount { cooldown: COOLDOWN },
                402 => Disposition::FailoverAccount {
                    cooldown: Duration::from_hours(1),
                },
                408 | 409 | 425 => Disposition::RetrySameAccount,
                // The provider's own `Retry-After` when it sent one. This was
                // hard-coded `None`, which meant every upstream throttle got
                // the same flat one-minute sit-out — twelve times too long for
                // a five-second limit, and far too short for an hourly quota.
                429 => Disposition::RateLimited {
                    retry_after: *retry_after,
                },
                // 529 is Anthropic's "overloaded". Not our credential's fault,
                // but hammering it makes things worse for everyone.
                500 | 502 | 503 | 504 | 529 => Disposition::FailoverAccount {
                    cooldown: Duration::from_secs(30),
                },
                // A well-formed request this model could not take: exactly what
                // a bigger model solves.
                413 | 422 => Disposition::EscalateTier,
                // A 400 usually means we built a bad request, which neither
                // another credential nor a bigger model will fix — so only the
                // ones whose body says otherwise climb a rung. This used to be
                // lumped in with 413, which was harmless while `EscalateTier`
                // meant "fail" and is not now that it retries: a malformed
                // request would be sent to the most expensive model on the
                // ladder to be rejected a second time.
                400 if names_a_capability_limit(body) => Disposition::EscalateTier,
                _ => Disposition::Fatal,
            },
            Self::StreamIdle(_) => Disposition::FailoverAccount {
                cooldown: Duration::from_mins(1),
            },
            Self::NoCredential { .. } | Self::NoViableModel => Disposition::EscalateTier,
            // Waiting helps; a bigger model does not.
            Self::AtCapacity { .. } => Disposition::RetrySameAccount,
            _ => Disposition::Fatal,
        }
    }
}

/// Whether an upstream body says the request was too big for *this* model
/// rather than malformed.
///
/// The providers disagree about the status code for a context-length rejection:
/// Anthropic and Gemini answer 400, OpenAI answers 400 with a typed code, and
/// only some return 413. The status alone therefore cannot separate "too long"
/// from "your JSON is wrong", and the body is the cheapest thing that can.
///
/// Matched case-insensitively on substrings because these messages are prose
/// that providers reword; a false negative costs one un-escalated request,
/// which is the old behaviour, while a false positive costs one wasted call.
fn names_a_capability_limit(body: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "context_length_exceeded",
        "context length",
        "context window",
        "prompt is too long",
        "token count",
        "too many tokens",
    ];
    let body = body.to_ascii_lowercase();
    MARKERS.iter().any(|marker| body.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(status: u16) -> Error {
        upstream_saying(status, "")
    }

    fn upstream_saying(status: u16, body: &str) -> Error {
        Error::Upstream {
            provider: Provider::Anthropic,
            account: AccountId::new(),
            status,
            body: body.to_owned(),
            retry_after: None,
        }
    }

    #[test]
    fn auth_failures_cool_down_rather_than_disable() {
        // The whole point: a routinely-expiring OAuth token must not take a
        // healthy credential permanently out of the pool.
        assert!(matches!(
            upstream(401).disposition(),
            Disposition::FailoverAccount { .. }
        ));
    }

    #[test]
    fn overload_is_not_treated_as_a_bad_credential() {
        let Disposition::FailoverAccount { cooldown } = upstream(529).disposition() else {
            panic!("529 should fail over");
        };
        assert!(
            cooldown < Duration::from_mins(1),
            "overload cooldown should be short"
        );
    }

    #[test]
    fn context_overflow_escalates_instead_of_failing() {
        assert_eq!(upstream(413).disposition(), Disposition::EscalateTier);
        assert_eq!(upstream(422).disposition(), Disposition::EscalateTier);
    }

    #[test]
    fn a_context_rejection_dressed_as_a_400_still_escalates() {
        // Anthropic and Gemini answer 400 for a prompt that does not fit, so
        // reading the status alone would fail the very request a bigger model
        // was going to serve.
        for body in [
            r#"{"error":{"message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            r#"{"error":{"message":"The input token count (1048576) exceeds the maximum"}}"#,
        ] {
            assert_eq!(
                upstream_saying(400, body).disposition(),
                Disposition::EscalateTier,
                "{body}"
            );
        }
    }

    #[test]
    fn a_malformed_request_is_not_sent_to_a_bigger_model() {
        // Escalation costs real money, and the frontier rung will reject bad
        // JSON exactly as the cheap one did.
        assert_eq!(
            upstream_saying(
                400,
                r#"{"error":{"type":"invalid_request_error","message":"messages: at least one message is required"}}"#
            )
            .disposition(),
            Disposition::Fatal
        );
        assert_eq!(upstream(400).disposition(), Disposition::Fatal);
    }

    #[test]
    fn being_at_capacity_is_not_treated_as_a_missing_credential() {
        // One is a config problem and one is a sizing problem. Collapsing them
        // sends whoever is on call to look at the credential pool when what
        // they needed was more concurrency.
        let busy = Error::AtCapacity {
            provider: Provider::Anthropic,
            candidates: 3,
        };
        assert_eq!(busy.disposition(), Disposition::RetrySameAccount);
        assert!(busy.to_string().contains("concurrency limit"));
    }

    #[test]
    fn rate_limit_is_distinct_from_unhealthy() {
        assert!(matches!(
            upstream(429).disposition(),
            Disposition::RateLimited { .. }
        ));
    }

    #[test]
    fn a_providers_retry_after_decides_how_long_the_credential_sits_out() {
        // It used to be discarded and replaced with a flat minute, so a
        // credential throttled for five seconds was benched for sixty and one
        // throttled for an hour came back to be refused again.
        let Disposition::RateLimited { retry_after } = Error::Upstream {
            provider: Provider::Anthropic,
            account: AccountId::new(),
            status: 429,
            body: String::new(),
            retry_after: Some(Duration::from_secs(30)),
        }
        .disposition() else {
            panic!("429 is a rate limit");
        };
        assert_eq!(retry_after, Some(Duration::from_secs(30)));
        // And with no hint the caller still has to pick a default.
        assert_eq!(
            upstream(429).disposition(),
            Disposition::RateLimited { retry_after: None }
        );
    }
}
