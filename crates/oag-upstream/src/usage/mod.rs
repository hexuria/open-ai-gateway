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

pub mod grok;

use oag_core::Provider;
use oag_core::credential::SecretMaterial;

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

/// Read the seat's remaining quota, or `None` for a provider with no usage API
/// wired up (in which case the poller leaves the account untouched).
pub async fn fetch(
    provider: Provider,
    credential: &SecretMaterial,
) -> oag_core::Result<Option<UsageSnapshot>> {
    match provider {
        Provider::XAI => grok::fetch(&credential.access_token).await,
        // Codex usage (chatgpt.com/backend-api/wham/usage) attaches once the
        // Codex credential path exists; every other provider is metered, not
        // flat-rate, so its spend is the ledger's job, not a quota poll.
        _ => Ok(None),
    }
}
