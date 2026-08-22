//! Deciding how hard a request is, from what we can see for free.
//!
//! v1 is heuristics, deliberately. The obvious alternative — ask a cheap model
//! to grade every request — costs a call and adds latency to the hot path on
//! every single request, to save money on some of them. That trade only makes
//! sense once heuristics are demonstrably leaving money on the table, so the
//! [`Classifier`] trait exists to make swapping in a model-based one later a
//! contained change rather than a rewrite.

use crate::catalog::Requirements;
use oag_core::TierName;
use serde::{Deserialize, Serialize};

/// What we can observe about a request without asking anyone.
// Independent observations about one request; no combination is invalid.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSignal {
    /// Estimated prompt size. Local estimate is fine — this feeds a threshold,
    /// not a bill.
    pub prompt_tokens: u64,
    /// Tool definitions attached. An agent handing over twenty tools is doing
    /// something structurally harder than a one-shot question.
    pub tool_count: usize,
    /// Messages in the conversation. Deep conversations carry accumulated
    /// context that cheap models lose track of.
    pub turn_count: usize,
    pub has_images: bool,
    /// The caller asked for extended thinking, which is itself a declaration
    /// that the task is hard.
    pub thinking_requested: bool,
    /// Fenced code or a diff in the prompt.
    pub has_code: bool,
    /// `x-oag-tier`. An explicit request always wins over inference.
    pub explicit_tier: Option<TierName>,
}

impl RequestSignal {
    /// Translate into hard requirements a model must meet.
    #[must_use]
    pub fn requirements(&self, max_output_tokens: u32) -> Requirements {
        Requirements {
            prompt_tokens: self.prompt_tokens,
            max_output_tokens,
            vision: self.has_images,
            tools: self.tool_count > 0,
            reasoning: self.thinking_requested,
        }
    }
}

/// Picks a rung for a request.
pub trait Classifier: Send + Sync {
    /// The rung this request should start on, by name.
    ///
    /// Returning a name rather than a rank keeps classifiers independent of any
    /// particular ladder's depth. A name the ladder does not have falls back to
    /// the ladder floor, so a misconfigured classifier is cheap, not broken.
    fn classify(&self, signal: &RequestSignal) -> TierName;
}

/// Thresholds for [`HeuristicClassifier`].
///
/// Exposed and serialisable because the right numbers are workload-specific,
/// and an operator watching their own escalation rate is better placed to tune
/// them than we are to guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thresholds {
    pub cheap_tier: TierName,
    pub balanced_tier: TierName,
    pub frontier_tier: TierName,
    /// Above this prompt size, stop using the cheapest rung.
    pub balanced_prompt_tokens: u64,
    /// Above this prompt size, go straight to the top.
    pub frontier_prompt_tokens: u64,
    /// Tool count that implies agentic work.
    pub balanced_tool_count: usize,
    /// Conversation depth that implies accumulated context.
    pub balanced_turn_count: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            cheap_tier: TierName::new("cheap"),
            balanced_tier: TierName::new("balanced"),
            frontier_tier: TierName::new("frontier"),
            balanced_prompt_tokens: 8_000,
            frontier_prompt_tokens: 100_000,
            balanced_tool_count: 3,
            balanced_turn_count: 8,
        }
    }
}

/// Rule-based classification.
///
/// The rules are ordered by how strong a signal each is, and every one of them
/// is a statement about the *task*, not about the text. That matters: a rule
/// keyed on prompt wording would be gameable and would drift as prompts change.
#[derive(Debug, Clone, Default)]
pub struct HeuristicClassifier {
    thresholds: Thresholds,
}

impl HeuristicClassifier {
    #[must_use]
    pub fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }

    #[must_use]
    pub fn thresholds(&self) -> &Thresholds {
        &self.thresholds
    }
}

impl Classifier for HeuristicClassifier {
    fn classify(&self, signal: &RequestSignal) -> TierName {
        let t = &self.thresholds;

        // An explicit ask is not a heuristic input; it is the answer.
        if let Some(explicit) = &signal.explicit_tier {
            return explicit.clone();
        }

        // Asking for extended thinking is the caller telling us it is hard.
        if signal.thinking_requested {
            return t.frontier_tier.clone();
        }

        // A prompt this large is both expensive to get wrong and beyond most
        // cheap models' effective (as opposed to advertised) context.
        if signal.prompt_tokens >= t.frontier_prompt_tokens {
            return t.frontier_tier.clone();
        }

        let agentic = signal.tool_count >= t.balanced_tool_count
            || signal.turn_count >= t.balanced_turn_count;
        let substantial = signal.prompt_tokens >= t.balanced_prompt_tokens;

        if agentic || substantial || signal.has_images || signal.has_code {
            return t.balanced_tier.clone();
        }

        t.cheap_tier.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(signal: &RequestSignal) -> String {
        HeuristicClassifier::default().classify(signal).0
    }

    #[test]
    fn a_short_plain_question_goes_cheap() {
        assert_eq!(
            classify(&RequestSignal {
                prompt_tokens: 200,
                turn_count: 1,
                ..RequestSignal::default()
            }),
            "cheap"
        );
    }

    #[test]
    fn an_explicit_tier_always_wins() {
        // Even a trivial request routes frontier if the caller asked.
        assert_eq!(
            classify(&RequestSignal {
                prompt_tokens: 10,
                explicit_tier: Some(TierName::new("frontier")),
                ..RequestSignal::default()
            }),
            "frontier"
        );
        // And a hard-looking one routes cheap if the caller insists.
        assert_eq!(
            classify(&RequestSignal {
                prompt_tokens: 500_000,
                thinking_requested: true,
                explicit_tier: Some(TierName::new("cheap")),
                ..RequestSignal::default()
            }),
            "cheap"
        );
    }

    #[test]
    fn requesting_thinking_implies_a_hard_task() {
        assert_eq!(
            classify(&RequestSignal {
                prompt_tokens: 100,
                thinking_requested: true,
                ..RequestSignal::default()
            }),
            "frontier"
        );
    }

    #[test]
    fn agentic_shape_lifts_off_the_cheapest_rung() {
        // Many tools is the signal, not prompt size.
        assert_eq!(
            classify(&RequestSignal {
                prompt_tokens: 300,
                tool_count: 12,
                ..RequestSignal::default()
            }),
            "balanced"
        );
        // As is a long conversation.
        assert_eq!(
            classify(&RequestSignal {
                prompt_tokens: 300,
                turn_count: 20,
                ..RequestSignal::default()
            }),
            "balanced"
        );
    }

    #[test]
    fn huge_prompts_go_straight_to_the_top() {
        assert_eq!(
            classify(&RequestSignal {
                prompt_tokens: 250_000,
                ..RequestSignal::default()
            }),
            "frontier"
        );
    }

    #[test]
    fn classification_is_deterministic() {
        let signal = RequestSignal {
            prompt_tokens: 9_000,
            tool_count: 2,
            turn_count: 3,
            has_code: true,
            ..RequestSignal::default()
        };
        let c = HeuristicClassifier::default();
        let first = c.classify(&signal);
        for _ in 0..100 {
            assert_eq!(c.classify(&signal), first, "same input, same rung, always");
        }
    }
}
