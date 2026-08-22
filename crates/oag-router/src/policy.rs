//! Putting it together: mode, ladder, classifier, floor, and budget produce a
//! model. Plus the rules for when to try again one rung up.

use crate::catalog::{Catalog, ModelSpec, Requirements};
use crate::classify::{Classifier, RequestSignal};
use crate::ladder::TierLadder;
use oag_core::{Error, Result, Tier, TierName, tier::RoutingMode};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// How close a principal is to their cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetPressure {
    /// Plenty of headroom. Route on merit.
    Normal,
    /// Close to or over the cap. Degrade to the cheapest rung rather than
    /// cutting someone off mid-task — a developer who cannot get *any* answer
    /// will go around the gateway, and then you have neither the savings nor
    /// the visibility.
    Constrained,
    /// Past the hard ceiling. Refuse.
    Exhausted,
}

/// Spend against a cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetState {
    pub spent_usd: Decimal,
    /// `None` means uncapped.
    pub limit_usd: Option<Decimal>,
    /// Multiple of the limit at which to stop serving entirely. `1.0` would
    /// make the cap a hard wall; the default leaves a deliberate grace band so
    /// crossing the line degrades before it denies.
    pub hard_stop_multiple: Decimal,
}

impl BudgetState {
    /// Uncapped spend.
    #[must_use]
    pub fn unlimited(spent_usd: Decimal) -> Self {
        Self {
            spent_usd,
            limit_usd: None,
            hard_stop_multiple: Decimal::ONE,
        }
    }

    #[must_use]
    pub fn pressure(&self) -> BudgetPressure {
        let Some(limit) = self.limit_usd else {
            return BudgetPressure::Normal;
        };
        if limit <= Decimal::ZERO {
            return BudgetPressure::Exhausted;
        }
        if self.spent_usd >= limit * self.hard_stop_multiple {
            return BudgetPressure::Exhausted;
        }
        // The last fifth of the budget buys cheap models only.
        let warn_at = limit * Decimal::new(8, 1);
        if self.spent_usd >= warn_at {
            return BudgetPressure::Constrained;
        }
        BudgetPressure::Normal
    }
}

/// Why a response was inadequate, and worth paying more to redo.
///
/// Each variant is a signal that a *better model* would plausibly help. An
/// upstream 500 does not appear here — that is a credential problem, handled by
/// failover, and escalating on it would quietly move traffic to expensive
/// models every time a provider had a bad afternoon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum QualityGate {
    /// Model declined a task within policy.
    Refusal,
    /// Ran into the output limit mid-answer.
    Truncated,
    /// Emitted a tool call with unparseable or empty arguments — the classic
    /// small-model failure on an agentic prompt.
    MalformedToolCall,
    /// Returned nothing usable.
    EmptyResponse,
    /// Rejected the request as too long or too complex for it.
    ContextOverflow,
}

/// Why this model was picked. Recorded on the usage row so the routing
/// decisions are auditable after the fact, not just their outcomes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionReason {
    /// Caller named a concrete model.
    Passthrough,
    /// Classifier picked the rung.
    Classified,
    /// Key or route pins a minimum rung.
    FloorPinned,
    /// Budget pressure forced a cheaper rung than merit would have chosen.
    BudgetDowngraded,
    /// A previous attempt tripped a quality gate.
    Escalated { from: TierName, gate: QualityGate },
}

/// The outcome: which model, on which rung, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingDecision {
    pub model: ModelSpec,
    pub tier: Tier,
    /// Why this rung was chosen.
    pub reason: SelectionReason,
    /// Set when the chosen rung had nothing capable enough and we had to walk
    /// up to find a model that fit.
    ///
    /// Orthogonal to `reason`, and deliberately a separate field: budget
    /// pressure can push a request *down* to a rung whose models cannot hold
    /// the prompt, which then pushes it back *up*. Both facts are true, both
    /// matter when reading the ledger, and collapsing them into one enum
    /// silently discards whichever happened first.
    pub capability_escalated_from: Option<TierName>,
    /// The ladder's top rung, for the counterfactual on the usage row.
    pub ceiling_model: Option<ModelSpec>,
}

/// A route's routing rules.
pub struct RoutingPolicy {
    ladder: TierLadder,
    classifier: Box<dyn Classifier>,
    /// Minimum rung this route will ever serve from.
    floor: Option<Tier>,
}

impl std::fmt::Debug for RoutingPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingPolicy")
            .field("ladder", &self.ladder)
            .field("floor", &self.floor)
            .finish_non_exhaustive()
    }
}

impl RoutingPolicy {
    #[must_use]
    pub fn new(ladder: TierLadder, classifier: Box<dyn Classifier>) -> Self {
        Self {
            ladder,
            classifier,
            floor: None,
        }
    }

    #[must_use]
    pub fn with_floor(mut self, floor: Option<Tier>) -> Self {
        self.floor = floor;
        self
    }

    #[must_use]
    pub fn ladder(&self) -> &TierLadder {
        &self.ladder
    }

    /// Choose a model for a request.
    ///
    /// Order matters and is the whole policy:
    /// 1. An explicitly named model is honoured — never surprise a caller who
    ///    was specific. It is still floor-clamped, because a floor is an
    ///    entitlement rather than a preference.
    /// 2. Otherwise classify, clamp to the floor, then apply budget pressure.
    /// 3. If the resulting rung has nothing capable enough, walk up until one
    ///    does. Escalating on capability is not a cost failure; sending a
    ///    200k-token prompt to a model that cannot hold it is.
    pub fn decide(
        &self,
        mode: &RoutingMode,
        requested_model: Option<&str>,
        signal: &RequestSignal,
        budget: &BudgetState,
        catalog: &Catalog,
        max_output_tokens: u32,
    ) -> Result<RoutingDecision> {
        let need = signal.requirements(max_output_tokens);
        let ceiling_model = self
            .ladder
            .pick(&self.ladder.ceiling(), catalog, &need)
            .cloned();

        if budget.pressure() == BudgetPressure::Exhausted {
            return Err(Error::BudgetExhausted);
        }

        if *mode == RoutingMode::Passthrough
            && let Some(name) = requested_model
        {
            let spec = catalog.resolve(name).ok_or(Error::NoViableModel)?;
            let tier = self
                .tier_of(&spec.id)
                .unwrap_or_else(|| self.ladder.floor());
            let clamped = self
                .ladder
                .clamp_to_floor(tier.clone(), self.floor.as_ref());
            // A floor pin outranks the caller's choice: that is what makes
            // it an entitlement rather than a default.
            if clamped == tier {
                return Ok(RoutingDecision {
                    model: spec.clone(),
                    tier,
                    reason: SelectionReason::Passthrough,
                    capability_escalated_from: None,
                    ceiling_model,
                });
            }
            return self.resolve_from(
                clamped,
                SelectionReason::FloorPinned,
                catalog,
                &need,
                ceiling_model,
            );
        }

        let classified = self.classifier.classify(signal);
        let tier = self
            .ladder
            .tier(&classified)
            .unwrap_or_else(|| self.ladder.floor());

        let floored = self
            .ladder
            .clamp_to_floor(tier.clone(), self.floor.as_ref());
        let mut reason = if floored == tier {
            SelectionReason::Classified
        } else {
            SelectionReason::FloorPinned
        };

        // Budget pressure never overrides an explicit floor: a route pinned to
        // frontier is pinned because cheaper answers are unacceptable there,
        // and quietly downgrading it would be the wrong kind of thrift.
        let final_tier = if budget.pressure() == BudgetPressure::Constrained {
            let cheapest = self
                .ladder
                .clamp_to_floor(self.ladder.floor(), self.floor.as_ref());
            if cheapest < floored {
                reason = SelectionReason::BudgetDowngraded;
                cheapest
            } else {
                floored
            }
        } else {
            floored
        };

        self.resolve_from(final_tier, reason, catalog, &need, ceiling_model)
    }

    /// Redo a request one rung up after a quality gate tripped.
    ///
    /// Returns `None` at the ceiling — there is nothing better to try, and
    /// looping would turn one bad answer into unbounded spend.
    pub fn escalate(
        &self,
        from: &Tier,
        gate: QualityGate,
        signal: &RequestSignal,
        catalog: &Catalog,
        max_output_tokens: u32,
    ) -> Option<RoutingDecision> {
        let next = self.ladder.escalate(from)?;
        let need = signal.requirements(max_output_tokens);
        let ceiling_model = self
            .ladder
            .pick(&self.ladder.ceiling(), catalog, &need)
            .cloned();
        self.resolve_from(
            next,
            SelectionReason::Escalated {
                from: from.name.clone(),
                gate,
            },
            catalog,
            &need,
            ceiling_model,
        )
        .ok()
    }

    /// Walk up from `tier` until a rung has a model that can serve the request.
    fn resolve_from(
        &self,
        tier: Tier,
        reason: SelectionReason,
        catalog: &Catalog,
        need: &Requirements,
        ceiling_model: Option<ModelSpec>,
    ) -> Result<RoutingDecision> {
        let started_at = tier.name.clone();
        let mut current = tier;
        loop {
            if let Some(spec) = self.ladder.pick(&current, catalog, need) {
                let capability_escalated_from =
                    (current.name != started_at).then(|| started_at.clone());
                return Ok(RoutingDecision {
                    model: spec.clone(),
                    tier: current,
                    reason,
                    capability_escalated_from,
                    ceiling_model,
                });
            }
            let Some(next) = self.ladder.escalate(&current) else {
                return Err(Error::NoViableModel);
            };
            current = next;
        }
    }

    fn tier_of(&self, model: &crate::catalog::ModelId) -> Option<Tier> {
        self.ladder
            .rungs()
            .iter()
            .position(|r| r.models.contains(model))
            .and_then(|i| u8::try_from(i).ok())
            .map(|rank| Tier::new(self.ladder.rungs()[usize::from(rank)].name.clone(), rank))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Capabilities, ModelId, Pricing};
    use crate::classify::HeuristicClassifier;
    use crate::ladder::Rung;
    use oag_core::Provider;
    use rust_decimal::dec;

    /// Pins every request to the cheapest rung, so capability escalation can be
    /// tested independently of what the real classifier would have chosen.
    struct AlwaysCheap;
    impl Classifier for AlwaysCheap {
        fn classify(&self, _: &RequestSignal) -> TierName {
            TierName::new("cheap")
        }
    }

    fn model(id: &str, provider: Provider, ctx: u32, input: Decimal) -> ModelSpec {
        ModelSpec {
            id: ModelId::new(id),
            provider,
            upstream_name: id.split('/').next_back().unwrap_or(id).to_owned(),
            pricing: Pricing {
                input_per_mtok: input,
                output_per_mtok: input * dec!(4),
                cache_read_per_mtok: Some(input / dec!(10)),
                cache_write_per_mtok: Some(input * dec!(1.25)),
            },
            context_window: ctx,
            max_output_tokens: 8192,
            capabilities: Capabilities {
                vision: true,
                tools: true,
                reasoning: true,
                prompt_cache: true,
            },
        }
    }

    fn catalog() -> Catalog {
        Catalog::from_entries([
            model("kimi/k2", Provider::Kimi, 128_000, dec!(0.6)),
            model("anthropic/haiku", Provider::Anthropic, 200_000, dec!(1)),
            model("anthropic/opus", Provider::Anthropic, 400_000, dec!(15)),
        ])
    }

    fn policy() -> RoutingPolicy {
        let ladder = TierLadder::new(vec![
            Rung {
                name: TierName::new("cheap"),
                models: vec![ModelId::new("kimi/k2")],
            },
            Rung {
                name: TierName::new("balanced"),
                models: vec![ModelId::new("anthropic/haiku")],
            },
            Rung {
                name: TierName::new("frontier"),
                models: vec![ModelId::new("anthropic/opus")],
            },
        ])
        .expect("non-empty");
        RoutingPolicy::new(ladder, Box::new(HeuristicClassifier::default()))
    }

    fn rich() -> BudgetState {
        BudgetState::unlimited(dec!(0))
    }

    #[test]
    fn a_trivial_managed_request_routes_cheap() {
        let d = policy()
            .decide(
                &RoutingMode::Managed,
                None,
                &RequestSignal {
                    prompt_tokens: 200,
                    turn_count: 1,
                    ..RequestSignal::default()
                },
                &rich(),
                &catalog(),
                1024,
            )
            .expect("routes");
        assert_eq!(d.model.id.as_str(), "kimi/k2");
        assert_eq!(d.reason, SelectionReason::Classified);
    }

    #[test]
    fn passthrough_honours_the_named_model() {
        // The caller was explicit. Even though this prompt would classify cheap,
        // we do not second-guess them.
        let d = policy()
            .decide(
                &RoutingMode::Passthrough,
                Some("anthropic/opus"),
                &RequestSignal {
                    prompt_tokens: 50,
                    ..RequestSignal::default()
                },
                &rich(),
                &catalog(),
                1024,
            )
            .expect("routes");
        assert_eq!(d.model.id.as_str(), "anthropic/opus");
        assert_eq!(d.reason, SelectionReason::Passthrough);
    }

    #[test]
    fn counterfactual_model_is_always_the_ceiling() {
        let d = policy()
            .decide(
                &RoutingMode::Managed,
                None,
                &RequestSignal {
                    prompt_tokens: 100,
                    ..RequestSignal::default()
                },
                &rich(),
                &catalog(),
                1024,
            )
            .expect("routes");
        assert_eq!(
            d.ceiling_model.expect("ceiling exists").id.as_str(),
            "anthropic/opus",
            "savings are measured against the top rung, whatever we actually used"
        );
    }

    #[test]
    fn budget_pressure_downgrades_rather_than_denying() {
        // 85% spent: still serving, just cheaply.
        let constrained = BudgetState {
            spent_usd: dec!(85),
            limit_usd: Some(dec!(100)),
            hard_stop_multiple: dec!(1.2),
        };
        assert_eq!(constrained.pressure(), BudgetPressure::Constrained);

        let d = policy()
            .decide(
                &RoutingMode::Managed,
                None,
                // Would classify frontier on merit.
                &RequestSignal {
                    prompt_tokens: 150_000,
                    ..RequestSignal::default()
                },
                &constrained,
                &catalog(),
                1024,
            )
            .expect("still serves");
        // Budget is still the reason the rung was chosen...
        assert_eq!(d.reason, SelectionReason::BudgetDowngraded);
        // ...and capability is separately why we did not stay there: k2's 128k
        // window cannot hold a 150k prompt, so we walked back up. Both facts
        // survive, which is the point of keeping them in separate fields.
        assert_eq!(d.capability_escalated_from, Some(TierName::new("cheap")));
        assert!(u64::from(d.model.context_window) >= 150_000);
    }

    #[test]
    fn hard_ceiling_denies() {
        let blown = BudgetState {
            spent_usd: dec!(121),
            limit_usd: Some(dec!(100)),
            hard_stop_multiple: dec!(1.2),
        };
        assert_eq!(blown.pressure(), BudgetPressure::Exhausted);
        let err = policy().decide(
            &RoutingMode::Managed,
            None,
            &RequestSignal::default(),
            &blown,
            &catalog(),
            1024,
        );
        assert!(matches!(err, Err(Error::BudgetExhausted)));
    }

    #[test]
    fn budget_pressure_does_not_override_an_explicit_floor() {
        let ladder = TierLadder::new(vec![
            Rung {
                name: TierName::new("cheap"),
                models: vec![ModelId::new("kimi/k2")],
            },
            Rung {
                name: TierName::new("frontier"),
                models: vec![ModelId::new("anthropic/opus")],
            },
        ])
        .expect("non-empty");
        let floor = ladder.tier(&TierName::new("frontier"));
        let p =
            RoutingPolicy::new(ladder, Box::new(HeuristicClassifier::default())).with_floor(floor);

        let constrained = BudgetState {
            spent_usd: dec!(90),
            limit_usd: Some(dec!(100)),
            hard_stop_multiple: dec!(1.2),
        };
        let d = p
            .decide(
                &RoutingMode::Managed,
                None,
                &RequestSignal {
                    prompt_tokens: 10,
                    ..RequestSignal::default()
                },
                &constrained,
                &catalog(),
                1024,
            )
            .expect("routes");
        assert_eq!(
            d.model.id.as_str(),
            "anthropic/opus",
            "a pinned route stays pinned; thrift does not get to override an entitlement"
        );
    }

    #[test]
    fn capability_escalation_walks_up_until_something_fits() {
        // 300k prompt: only opus can hold it, but this classifies frontier
        // anyway. Force the issue from the floor instead.
        let ladder = TierLadder::new(vec![
            Rung {
                name: TierName::new("cheap"),
                models: vec![ModelId::new("kimi/k2")],
            },
            Rung {
                name: TierName::new("frontier"),
                models: vec![ModelId::new("anthropic/opus")],
            },
        ])
        .expect("non-empty");
        let p = RoutingPolicy::new(ladder, Box::new(AlwaysCheap));
        let d = p
            .decide(
                &RoutingMode::Managed,
                None,
                &RequestSignal {
                    prompt_tokens: 300_000,
                    ..RequestSignal::default()
                },
                &rich(),
                &catalog(),
                1024,
            )
            .expect("escalates on capability");
        assert_eq!(d.model.id.as_str(), "anthropic/opus");
        assert_eq!(
            d.reason,
            SelectionReason::Classified,
            "the classifier still picked cheap"
        );
        assert_eq!(d.capability_escalated_from, Some(TierName::new("cheap")));
    }

    #[test]
    fn escalation_moves_exactly_one_rung_and_records_why() {
        let p = policy();
        let cheap = p.ladder().floor();
        let d = p
            .escalate(
                &cheap,
                QualityGate::MalformedToolCall,
                &RequestSignal::default(),
                &catalog(),
                1024,
            )
            .expect("escalates");
        assert_eq!(
            d.model.id.as_str(),
            "anthropic/haiku",
            "one rung, not straight to the top"
        );
        assert!(matches!(
            d.reason,
            SelectionReason::Escalated {
                gate: QualityGate::MalformedToolCall,
                ..
            }
        ));
    }

    #[test]
    fn escalation_terminates_at_the_ceiling() {
        let p = policy();
        let top = p.ladder().ceiling();
        assert!(
            p.escalate(
                &top,
                QualityGate::Refusal,
                &RequestSignal::default(),
                &catalog(),
                1024
            )
            .is_none(),
            "unbounded escalation would turn one bad answer into unbounded spend"
        );
    }

    #[test]
    fn uncapped_budget_never_applies_pressure() {
        assert_eq!(
            BudgetState::unlimited(dec!(1_000_000)).pressure(),
            BudgetPressure::Normal
        );
    }
}
