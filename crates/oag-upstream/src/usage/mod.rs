//! Reading a subscription's remaining quota from the provider's own usage API.
//!
//! Separate from inference: the gateway records what a seat's traffic would
//! *cost*, but a flat-rate seat also has an *allowance* (Grok's weekly pool,
//! Codex's rolling window), and only the provider knows how much is left. A
//! background poller reads it here and lands it where the scheduler and the
//! dashboard can use it.
//!
//! Per-provider contracts are ported from openusage (MIT). Each provider that
//! exposes a usable endpoint gets a submodule; the rest return `None` and are
//! simply not polled.

pub mod codex;
pub mod grok;

use oag_core::Provider;
use oag_core::credential::{CredentialKind, SecretMaterial};

/// One reading of a subscription's remaining allowance.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    /// Remaining allowance as a percentage, 0..=100.
    pub remaining_pct: f64,
    /// The window the percentage is measured over, e.g. "weekly" — the number
    /// is meaningless without it, because providers meter on different periods.
    pub window_label: String,
    /// When the current window resets, unix seconds; `None` if the provider did
    /// not say. Drives the scheduler's use-it-or-lose-it stage.
    pub resets_at: Option<i64>,
}

/// Which usage API a credential resolves to, if any.
///
/// Split out from [`fetch`] so the dispatch — the part that decides whether a
/// credential gets called at all — is decidable and testable without a network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    /// xAI's billing proxy, the weekly Grok pool.
    GrokBilling,
    /// The `ChatGPT` backend's rate-limit meters, for a Codex seat.
    CodexWham,
}

/// Route a seat to its provider's usage API.
///
/// Gated on the credential *kind* first, and not on the provider alone: a Codex
/// seat is `Provider::OpenAI` — the gateway tells the two apart by kind
/// everywhere else too (see `gateway::is_codex_seat`) — so dispatching on the
/// provider would send an ordinary metered OpenAI API key to the subscription
/// backend. That key has no allowance to read; it is billed per token and its
/// spend is the ledger's job. `flat_rate()` is the same predicate the accounting
/// uses to decide a seat is a seat, so the two cannot drift apart.
fn endpoint_for(provider: Provider, kind: CredentialKind) -> Option<Endpoint> {
    if !kind.flat_rate() {
        return None;
    }
    match provider {
        Provider::XAI => Some(Endpoint::GrokBilling),
        Provider::OpenAI => Some(Endpoint::CodexWham),
        // Every other provider is metered, not flat-rate, so its spend is the
        // ledger's job rather than a quota poll.
        _ => None,
    }
}

/// Read the seat's remaining quota, or `None` for a credential with no usage
/// API to read (in which case the poller leaves the account untouched).
pub async fn fetch(
    provider: Provider,
    kind: CredentialKind,
    credential: &SecretMaterial,
) -> oag_core::Result<Option<UsageSnapshot>> {
    match endpoint_for(provider, kind) {
        Some(Endpoint::GrokBilling) => grok::fetch(&credential.access_token).await,
        Some(Endpoint::CodexWham) => codex::fetch(credential).await,
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_openai_api_key_is_never_polled_because_it_has_no_allowance() {
        // It is metered per token: there is no quota endpoint to ask, and
        // asking the subscription backend with it would 401 every interval.
        assert_eq!(endpoint_for(Provider::OpenAI, CredentialKind::ApiKey), None);
    }

    #[test]
    fn an_openai_oauth_seat_is_a_codex_seat_and_is_polled() {
        assert_eq!(
            endpoint_for(Provider::OpenAI, CredentialKind::OAuth),
            Some(Endpoint::CodexWham)
        );
    }

    #[test]
    fn a_grok_seat_still_routes_to_the_billing_proxy() {
        assert_eq!(
            endpoint_for(Provider::XAI, CredentialKind::OAuth),
            Some(Endpoint::GrokBilling)
        );
    }

    #[test]
    fn a_metered_provider_has_nothing_to_poll_whatever_its_kind() {
        for kind in [
            CredentialKind::ApiKey,
            CredentialKind::OAuth,
            CredentialKind::Bedrock,
            CredentialKind::Vertex,
            CredentialKind::ServiceAccount,
        ] {
            assert_eq!(endpoint_for(Provider::Anthropic, kind), None);
            assert_eq!(endpoint_for(Provider::Gemini, kind), None);
        }
    }

    #[test]
    fn only_a_flat_rate_kind_is_polled_at_all() {
        // A quota exists because a fee bought one. Any kind that is billed per
        // token has nothing to read, whichever provider it belongs to.
        for kind in [
            CredentialKind::ApiKey,
            CredentialKind::Bedrock,
            CredentialKind::Vertex,
            CredentialKind::ServiceAccount,
        ] {
            assert_eq!(endpoint_for(Provider::XAI, kind), None);
            assert_eq!(endpoint_for(Provider::OpenAI, kind), None);
        }
    }
}
