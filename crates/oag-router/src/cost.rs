//! Token accounting and what it costs.

use crate::catalog::{ModelSpec, Pricing};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

const TOKENS_PER_MILLION: Decimal = Decimal::from_parts(1_000_000, 0, 0, false, 0);

/// Tokens consumed by one request.
///
/// Cache tiers are separate fields rather than folded into `input_tokens`
/// because they are priced an order of magnitude apart, and because reporting
/// cache hit rate is the fastest way to see that sticky routing is working.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt prefix served from the provider's cache.
    pub cache_read_tokens: u64,
    /// Prompt prefix written into the provider's cache.
    pub cache_write_tokens: u64,
}

impl Usage {
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// Fraction of prompt tokens served from cache, 0.0-1.0.
    ///
    /// The headline number for whether sticky session routing is earning its
    /// keep: rotate credentials mid-conversation and this collapses.
    #[must_use]
    pub fn cache_hit_rate(&self) -> Decimal {
        let prompt = self.input_tokens.saturating_add(self.cache_read_tokens);
        if prompt == 0 {
            return Decimal::ZERO;
        }
        Decimal::from(self.cache_read_tokens) / Decimal::from(prompt)
    }

    /// Merge a partial usage report into this one.
    ///
    /// Anthropic splits usage across `message_start` and `message_delta`: the
    /// first carries input and cache counts, later ones carry output. A naive
    /// assignment zeroes the fields the newer event omits, which silently
    /// under-bills every streamed request. Take the maximum per field instead —
    /// counts only ever grow within a response.
    pub fn merge(&mut self, patch: &Self) {
        self.input_tokens = self.input_tokens.max(patch.input_tokens);
        self.output_tokens = self.output_tokens.max(patch.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(patch.cache_read_tokens);
        self.cache_write_tokens = self.cache_write_tokens.max(patch.cache_write_tokens);
    }
}

impl Pricing {
    /// USD for this usage at these prices.
    ///
    /// Cache tiers fall back to the plain input rate when a provider does not
    /// publish a separate one — over-billing ourselves in the report is the
    /// safe direction of error.
    #[must_use]
    pub fn cost(&self, usage: &Usage) -> Decimal {
        let read_rate = self.cache_read_per_mtok.unwrap_or(self.input_per_mtok);
        let write_rate = self.cache_write_per_mtok.unwrap_or(self.input_per_mtok);

        let total = Decimal::from(usage.input_tokens) * self.input_per_mtok
            + Decimal::from(usage.output_tokens) * self.output_per_mtok
            + Decimal::from(usage.cache_read_tokens) * read_rate
            + Decimal::from(usage.cache_write_tokens) * write_rate;

        total / TOKENS_PER_MILLION
    }
}

/// What this request would have cost on a different model.
///
/// Recorded on every usage row against the route's top rung. The delta,
/// summed, is the number that justifies the gateway to whoever signs off on
/// the spend — and it is only honest if it is computed per request from real
/// token counts rather than estimated after the fact.
#[must_use]
pub fn counterfactual(usage: &Usage, alternative: &ModelSpec) -> Decimal {
    alternative.pricing.cost(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Capabilities, ModelId};
    use oag_core::Provider;
    use rust_decimal::dec;

    fn model(input: Decimal, output: Decimal) -> ModelSpec {
        ModelSpec {
            id: ModelId::new("test/model"),
            provider: Provider::Anthropic,
            upstream_name: "model".to_owned(),
            pricing: Pricing {
                input_per_mtok: input,
                output_per_mtok: output,
                cache_read_per_mtok: Some(input / dec!(10)),
                cache_write_per_mtok: Some(input * dec!(1.25)),
            },
            context_window: 200_000,
            max_output_tokens: 8192,
            capabilities: Capabilities::default(),
        }
    }

    #[test]
    fn cost_is_exact_not_floating_point() {
        // $3/Mtok in, $15/Mtok out. 1M in + 1M out = $18.00 exactly.
        let m = model(dec!(3), dec!(15));
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Usage::default()
        };
        assert_eq!(m.pricing.cost(&usage), dec!(18));
    }

    #[test]
    fn cache_reads_are_priced_at_the_cache_rate() {
        let m = model(dec!(3), dec!(15));
        let cached = Usage {
            cache_read_tokens: 1_000_000,
            ..Usage::default()
        };
        // 10% of input rate.
        assert_eq!(m.pricing.cost(&cached), dec!(0.3));
    }

    #[test]
    fn merge_never_loses_a_field_a_later_event_omits() {
        // The exact shape of an Anthropic stream: input arrives first, output
        // accumulates later without repeating the input counts.
        let mut usage = Usage {
            input_tokens: 1000,
            cache_read_tokens: 5000,
            ..Usage::default()
        };
        usage.merge(&Usage {
            output_tokens: 250,
            ..Usage::default()
        });
        assert_eq!(usage.input_tokens, 1000, "input must survive the delta event");
        assert_eq!(usage.cache_read_tokens, 5000);
        assert_eq!(usage.output_tokens, 250);
    }

    #[test]
    fn counterfactual_shows_the_saving() {
        let cheap = model(dec!(0.6), dec!(2.5));
        let frontier = model(dec!(15), dec!(75));
        let usage = Usage {
            input_tokens: 100_000,
            output_tokens: 10_000,
            ..Usage::default()
        };
        let actual = cheap.pricing.cost(&usage);
        let alternative = counterfactual(&usage, &frontier);
        assert!(alternative > actual);
        // Roughly thirtyfold on this mix; assert the order of magnitude holds.
        assert!(alternative / actual > dec!(10));
    }

    #[test]
    fn cache_hit_rate_reports_zero_for_an_empty_prompt() {
        assert_eq!(Usage::default().cache_hit_rate(), Decimal::ZERO);
    }
}
