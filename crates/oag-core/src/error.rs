//! Errors, and the classification that drives retry and failover.

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
    Principal,
}

impl std::fmt::Display for BudgetScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ApiKey => "the quota on this API key",
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

    #[error("upstream {provider} returned {status}")]
    Upstream {
        provider: Provider,
        account: AccountId,
        status: u16,
        body: String,
    },

    #[error("upstream stream stalled after {0:?} with no data")]
    StreamIdle(Duration),

    #[error("serialisation: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("{0}")]
    Internal(String),
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
            Self::Upstream { status, .. } => match status {
                401 | 403 => Disposition::FailoverAccount { cooldown: COOLDOWN },
                402 => Disposition::FailoverAccount {
                    cooldown: Duration::from_hours(1),
                },
                408 | 409 | 425 => Disposition::RetrySameAccount,
                429 => Disposition::RateLimited { retry_after: None },
                // 529 is Anthropic's "overloaded". Not our credential's fault,
                // but hammering it makes things worse for everyone.
                500 | 502 | 503 | 504 | 529 => Disposition::FailoverAccount {
                    cooldown: Duration::from_secs(30),
                },
                // A 400 usually means we built a bad request, which another
                // credential will not fix — but a context-length or capability
                // rejection is exactly what a bigger model solves.
                400 | 413 | 422 => Disposition::EscalateTier,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(status: u16) -> Error {
        Error::Upstream {
            provider: Provider::Anthropic,
            account: AccountId::new(),
            status,
            body: String::new(),
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
}
