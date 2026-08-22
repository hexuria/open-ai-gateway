//! Errors, and the classification that drives retry and failover.

use crate::{AccountId, Provider};
use std::time::Duration;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("unknown provider: {0}")]
    UnknownProvider(String),

    #[error("configuration: {0}")]
    Config(String),

    #[error("no credential available for provider {provider} on this route")]
    NoCredential { provider: Provider },

    #[error("no model on the ladder satisfies the request")]
    NoViableModel,

    #[error("authentication failed")]
    Unauthenticated,

    #[error("budget exhausted for this principal")]
    BudgetExhausted,

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
        assert!(cooldown < Duration::from_mins(1), "overload cooldown should be short");
    }

    #[test]
    fn context_overflow_escalates_instead_of_failing() {
        assert_eq!(upstream(413).disposition(), Disposition::EscalateTier);
    }

    #[test]
    fn rate_limit_is_distinct_from_unhealthy() {
        assert!(matches!(
            upstream(429).disposition(),
            Disposition::RateLimited { .. }
        ));
    }
}
