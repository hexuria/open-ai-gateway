//! Putting it together: mode, ladder, classifier, floor, and budget produce a
//! model. Plus the rules for when to try again one rung up.

use crate::catalog::{Catalog, ModelSpec, Requirements};
use crate::classify::{Classifier, RequestSignal};
use crate::ladder::TierLadder;
use oag_core::Provider;
use oag_core::{BudgetScope, Error, Result, Tier, TierName, tier::RoutingMode};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// How close a principal is to their cap.
/// Variant order is load-bearing: it ascends from most to least headroom, so
/// `max()` over several caps yields the tightest one. Do not reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

impl BudgetPressure {
    /// The spelling `/v1/models` puts on the wire. Distinct from the variant
    /// name so a JSON client sees `exhausted`, not `Exhausted`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Constrained => "constrained",
            Self::Exhausted => "exhausted",
        }
    }
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

/// One model a caller is entitled to name, as reported by `/v1/models`.
#[derive(Debug, Clone)]
pub struct Entitlement<'c> {
    pub spec: &'c ModelSpec,
    /// The rung it sits on, or `None` for a catalog model that is on no rung.
    pub tier: Option<TierName>,
    /// Whether naming it will be honoured. False in managed mode, and false for
    /// an off-ladder name under a floor above the cheapest rung.
    pub honoured: bool,
}

/// Every spend cap that applies to one request.
///
/// Spend is capped per-key *and* per-principal, and the two are independent:
/// a generous principal budget must never buy past a small per-key quota, and
/// a fresh key must never buy past a principal who has spent their month. The
/// tighter cap governs, which is what `pressure` computes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budgets {
    pub key: BudgetState,
    pub route: BudgetState,
    pub principal: BudgetState,
}

impl Budgets {
    /// Only a principal-level cap applies; the key is uncapped.
    #[must_use]
    pub fn principal_only(principal: BudgetState) -> Self {
        Self {
            key: BudgetState::unlimited(Decimal::ZERO),
            route: BudgetState::unlimited(Decimal::ZERO),
            principal,
        }
    }

    /// The tighter of the two caps.
    #[must_use]
    pub fn pressure(&self) -> BudgetPressure {
        self.key
            .pressure()
            .max(self.route.pressure())
            .max(self.principal.pressure())
    }

    /// Which cap is binding. Ties go to the key: it is the narrower object and
    /// the one an operator can raise without widening spend for everything
    /// else the principal owns.
    #[must_use]
    pub fn binding(&self) -> BudgetScope {
        // Listed widest first because `max_by_key` keeps the *last* maximum, so
        // a tie resolves to the narrowest cap — the one an operator can raise
        // without widening spend for everything else that shares the others.
        // The ordering is the tie-break; do not sort this array.
        [
            (BudgetScope::Principal, self.principal.pressure()),
            (BudgetScope::Route, self.route.pressure()),
            (BudgetScope::ApiKey, self.key.pressure()),
        ]
        .into_iter()
        .max_by_key(|&(_, pressure)| pressure)
        .map_or(BudgetScope::Principal, |(scope, _)| scope)
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
    /// The ladder rung this sat on, if it sat on one.
    ///
    /// `None` for passthrough of a model that is on no rung. Mapping those
    /// onto the floor used to make a named Grok request report as `cheap` on
    /// `x-oag-tier` and in the ledger — a tier it was never on, and the label
    /// a client Test button then surfaces.
    pub tier: Option<Tier>,
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

impl RoutingDecision {
    /// The rung name to put on `x-oag-tier` and the ledger, if this sat on one.
    #[must_use]
    pub fn rung_name(&self) -> Option<&str> {
        self.tier.as_ref().map(|t| t.name.as_str())
    }
}

/// Whether an unusable answer should be retried one rung up.
///
/// A pure function because this is a policy question with a surprising answer,
/// and the surprising part deserves a test rather than a comment in a handler:
/// **a principal under budget pressure does not get escalated.** They were
/// downgraded on purpose; spending more to fix the answer undoes exactly the
/// saving the downgrade existed to make. Accepting the worse answer *is* the
/// policy at that point.
#[must_use]
pub fn escalation_allowed(
    pressure: BudgetPressure,
    escalations_so_far: u8,
    max_escalations: u8,
) -> bool {
    pressure == BudgetPressure::Normal && escalations_so_far < max_escalations
}

/// Whether a quality gate may climb to a *different* model.
///
/// Failover still retries the same model on another credential. Climbing the
/// ladder is how a named `xai/grok-4.6` becomes "no credential for anthropic":
/// passthrough parks an off-ladder name on the cheap rung, then `escalate`
/// walks to the next provider on the ladder. A caller who was specific is not
/// migrated onto a model they did not name.
#[must_use]
pub const fn climb_allowed(reason: &SelectionReason) -> bool {
    !matches!(reason, SelectionReason::Passthrough)
}

/// Hitting the output cap the caller set is not a weaker-model failure.
///
/// `StopReason::MaxTokens` does not say *whose* cap it was. If the request
/// asked for no more tokens than this model can emit, the stop is the
/// contract — climbing a rung is how `max_tokens=2` on a named Grok request
/// becomes "no credential for anthropic" on the ladder's next provider.
#[must_use]
pub const fn truncated_by_client_cap(requested_max: u32, model_max: u32) -> bool {
    requested_max <= model_max
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

    /// The rung this name refers to, if the route's ladder has one.
    #[must_use]
    pub fn rung(&self, name: &TierName) -> Option<Tier> {
        self.ladder.tier(name)
    }

    /// The floor rung's name, for logging.
    #[must_use]
    pub fn floor_name(&self) -> Option<&str> {
        self.floor.as_ref().map(|t| t.name.as_str())
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
        budget: &Budgets,
        catalog: &Catalog,
        max_output_tokens: u32,
    ) -> Result<RoutingDecision> {
        let need = signal.requirements(max_output_tokens);
        let ceiling_model = self
            .ladder
            .pick(&self.ladder.ceiling(), catalog, &need)
            .cloned();

        if budget.pressure() == BudgetPressure::Exhausted {
            return Err(Error::BudgetExhausted {
                scope: budget.binding(),
            });
        }

        if *mode == RoutingMode::Passthrough
            && let Some(name) = requested_model
        {
            let spec = catalog.resolve(name).ok_or_else(|| {
                Error::NoViableModel("no model on the ladder satisfies the request".to_owned())
            })?;
            if let Some(tier) = self.tier_of(&spec.id) {
                let clamped = self
                    .ladder
                    .clamp_to_floor(tier.clone(), self.floor.as_ref());
                // A floor pin outranks the caller's choice: that is what
                // makes it an entitlement rather than a default.
                if clamped == tier {
                    return Ok(RoutingDecision {
                        model: spec.clone(),
                        tier: Some(tier),
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
            // Off the ladder. Honour the name and do not pretend it sat on
            // cheap — unless a floor above rank 0 would substitute a
            // different model, which is FloorPinned.
            if let Some(floor) = &self.floor
                && floor.rank > 0
            {
                return self.resolve_from(
                    floor.clone(),
                    SelectionReason::FloorPinned,
                    catalog,
                    &need,
                    ceiling_model,
                );
            }
            return Ok(RoutingDecision {
                model: spec.clone(),
                tier: None,
                reason: SelectionReason::Passthrough,
                capability_escalated_from: None,
                ceiling_model,
            });
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
                    tier: Some(current),
                    reason,
                    capability_escalated_from,
                    ceiling_model,
                });
            }
            let Some(next) = self.ladder.escalate(&current) else {
                return Err(Error::NoViableModel(
                    "no model on the ladder satisfies the request".to_owned(),
                ));
            };
            current = next;
        }
    }

    /// The ladder rungs this route advertises as `oag/<rung>` names.
    ///
    /// Rung names come from the route's own ladder rather than a hardcoded
    /// cheap/balanced/frontier trio, because `TierName` is operator-defined and
    /// a route with a `[budget, standard, premium]` ladder must advertise those.
    /// `oag/auto` is universal and is not returned here — it is not a rung.
    ///
    /// Under [`BudgetPressure::Constrained`], only the rung `decide` would
    /// actually serve from is advertised: `oag/<rung>` forces managed mode,
    /// and managed mode degrades to that ceiling. Advertising `oag/frontier`
    /// would pin a rung the next turn then silently leaves.
    #[must_use]
    pub fn virtual_names(&self, pressure: BudgetPressure) -> Vec<TierName> {
        if pressure == BudgetPressure::Exhausted {
            return Vec::new();
        }
        let floor_rank = self.floor.as_ref().map_or(0, |t| t.rank);
        let ceiling_rank = if pressure == BudgetPressure::Constrained {
            self.ladder
                .clamp_to_floor(self.ladder.floor(), self.floor.as_ref())
                .rank
        } else {
            u8::MAX
        };
        self.ladder
            .rungs()
            .iter()
            .enumerate()
            .filter_map(|(i, rung)| {
                let rank = u8::try_from(i).ok()?;
                (rank >= floor_rank && rank <= ceiling_rank).then(|| rung.name.clone())
            })
            .collect()
    }

    /// Models this caller may actually name, and whether naming one is honoured.
    ///
    /// `providers` is the set the route holds usable credentials for: a model
    /// nobody can reach is worse listed than omitted, because the failure
    /// arrives later and further from the cause.
    ///
    /// `honoured` is whether `decide` in this mode would return the named
    /// model. Under [`BudgetPressure::Constrained`] that is unchanged for a
    /// passthrough name — `decide` still honours it, budget pressure applies
    /// only on the managed path — and still false for every managed name.
    /// Exhausted spend lists nothing: the next turn is a hard stop.
    #[must_use]
    pub fn entitled<'c>(
        &self,
        mode: &RoutingMode,
        catalog: &'c Catalog,
        providers: &BTreeSet<Provider>,
        pressure: BudgetPressure,
    ) -> Vec<Entitlement<'c>> {
        if pressure == BudgetPressure::Exhausted {
            return Vec::new();
        }
        let floor_rank = self.floor.as_ref().map_or(0, |t| t.rank);
        let managed = *mode == RoutingMode::Managed;
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();

        for (i, rung) in self.ladder.rungs().iter().enumerate() {
            let Ok(rank) = u8::try_from(i) else { continue };
            if rank < floor_rank {
                continue;
            }
            for spec in rung.models.iter().filter_map(|id| catalog.get(id)) {
                if !providers.contains(&spec.provider) || !seen.insert(spec.id.as_str().to_owned())
                {
                    continue;
                }
                out.push(Entitlement {
                    spec,
                    tier: Some(rung.name.clone()),
                    // In managed mode the name is advisory: `decide` classifies
                    // and picks for itself. Constrained does not change that —
                    // and does not un-honour a passthrough name, because
                    // `decide`'s passthrough branch returns before budget.
                    honoured: !managed,
                });
            }
        }

        if !managed {
            // An off-ladder name lands in `decide`'s passthrough branch, where
            // `tier_of` returns `None` and it is treated as rank 0. With a floor
            // above rank 0 the clamp then substitutes a *different* model
            // entirely, silently — so under a floor these are advertised as not
            // honoured rather than as free choices.
            let honoured = floor_rank == 0;
            for spec in catalog.iter() {
                if !providers.contains(&spec.provider) || seen.contains(spec.id.as_str()) {
                    continue;
                }
                out.push(Entitlement {
                    spec,
                    tier: None,
                    honoured,
                });
            }
        }

        out
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
            display_label: None,
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

    fn rich() -> Budgets {
        Budgets::principal_only(BudgetState::unlimited(dec!(0)))
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
        assert_eq!(d.rung_name(), Some("frontier"));
    }

    #[test]
    fn passthrough_of_an_off_ladder_model_has_no_rung() {
        // Mapping these onto cheap made a named Grok request report as a
        // rung it was never on. Honour the name; do not invent a tier.
        let d = policy()
            .decide(
                &RoutingMode::Passthrough,
                Some("anthropic/sonnet"),
                &RequestSignal {
                    prompt_tokens: 50,
                    ..RequestSignal::default()
                },
                &rich(),
                &catalog_with_off_ladder(),
                1024,
            )
            .expect("routes");
        assert_eq!(d.model.id.as_str(), "anthropic/sonnet");
        assert_eq!(d.reason, SelectionReason::Passthrough);
        assert_eq!(d.rung_name(), None);
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
                &Budgets::principal_only(constrained.clone()),
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
            &Budgets::principal_only(blown.clone()),
            &catalog(),
            1024,
        );
        assert!(matches!(err, Err(Error::BudgetExhausted { .. })));
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
                &Budgets::principal_only(constrained.clone()),
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
    fn a_context_overflow_climbs_to_a_rung_that_can_hold_the_prompt() {
        // What makes escalating on an upstream 413 worth doing at all: the
        // rung above is not merely more expensive, it is re-picked against the
        // request's requirements, so the model we climb to is one whose window
        // actually fits the prompt the last one refused.
        let p = policy();
        let cheap = p.ladder().floor();
        let d = p
            .escalate(
                &cheap,
                QualityGate::ContextOverflow,
                &RequestSignal {
                    prompt_tokens: 150_000,
                    ..RequestSignal::default()
                },
                &catalog(),
                1024,
            )
            .expect("escalates");
        assert!(
            u64::from(d.model.context_window) >= 150_000,
            "climbed to {} — a window no bigger than the one that just refused",
            d.model.id.as_str()
        );
        assert!(matches!(
            d.reason,
            SelectionReason::Escalated {
                gate: QualityGate::ContextOverflow,
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
    fn budget_pressure_suppresses_escalation() {
        // The saving from a downgrade is undone entirely if the request then
        // climbs back to the most expensive rung.
        assert!(escalation_allowed(BudgetPressure::Normal, 0, 1));
        assert!(!escalation_allowed(BudgetPressure::Constrained, 0, 1));
        assert!(!escalation_allowed(BudgetPressure::Exhausted, 0, 1));
    }

    #[test]
    fn escalation_is_bounded_even_with_headroom() {
        // Unbounded escalation turns one bad answer into unbounded spend.
        assert!(escalation_allowed(BudgetPressure::Normal, 0, 1));
        assert!(!escalation_allowed(BudgetPressure::Normal, 1, 1));
        assert!(!escalation_allowed(BudgetPressure::Normal, 9, 1));
    }

    #[test]
    fn a_zero_budget_of_escalations_never_escalates() {
        assert!(!escalation_allowed(BudgetPressure::Normal, 0, 0));
    }

    #[test]
    fn uncapped_budget_never_applies_pressure() {
        assert_eq!(
            BudgetState::unlimited(dec!(1_000_000)).pressure(),
            BudgetPressure::Normal
        );
    }

    #[test]
    fn a_key_quota_binds_even_when_the_principal_has_room() {
        // The regression this guards: `quota_usd` was read from the database,
        // carried through auth, and then never consulted, so a per-key cap was
        // decorative and spend was governed only per-principal.
        let budgets = Budgets {
            key: BudgetState {
                spent_usd: dec!(45),
                limit_usd: Some(dec!(50)),
                hard_stop_multiple: Decimal::ONE,
            },
            route: BudgetState::unlimited(dec!(0)),
            principal: BudgetState::unlimited(dec!(45)),
        };
        assert_eq!(budgets.pressure(), BudgetPressure::Constrained);
        assert_eq!(budgets.binding(), BudgetScope::ApiKey);

        let d = policy()
            .decide(
                &RoutingMode::Managed,
                None,
                &RequestSignal {
                    prompt_tokens: 150_000,
                    ..RequestSignal::default()
                },
                &budgets,
                &catalog(),
                1024,
            )
            .expect("a constrained key still gets served, just cheaply");
        assert_eq!(d.reason, SelectionReason::BudgetDowngraded);
    }

    #[test]
    fn an_exhausted_key_quota_names_the_key_not_the_principal() {
        let budgets = Budgets {
            key: BudgetState {
                spent_usd: dec!(50),
                limit_usd: Some(dec!(50)),
                hard_stop_multiple: Decimal::ONE,
            },
            route: BudgetState::unlimited(dec!(0)),
            principal: BudgetState::unlimited(dec!(50)),
        };
        let err = policy().decide(
            &RoutingMode::Managed,
            None,
            &RequestSignal::default(),
            &budgets,
            &catalog(),
            1024,
        );
        assert!(matches!(
            err,
            Err(Error::BudgetExhausted {
                scope: BudgetScope::ApiKey
            })
        ));
    }

    #[test]
    fn an_exhausted_principal_is_not_rescued_by_a_fresh_key() {
        let budgets = Budgets {
            key: BudgetState {
                spent_usd: dec!(5),
                limit_usd: Some(dec!(1000)),
                hard_stop_multiple: Decimal::ONE,
            },
            route: BudgetState::unlimited(dec!(0)),
            principal: BudgetState {
                spent_usd: dec!(121),
                limit_usd: Some(dec!(100)),
                hard_stop_multiple: dec!(1.2),
            },
        };
        assert_eq!(budgets.binding(), BudgetScope::Principal);
        let err = policy().decide(
            &RoutingMode::Managed,
            None,
            &RequestSignal::default(),
            &budgets,
            &catalog(),
            1024,
        );
        assert!(matches!(
            err,
            Err(Error::BudgetExhausted {
                scope: BudgetScope::Principal
            })
        ));
    }

    #[test]
    fn pressure_ascends_so_the_tighter_cap_always_wins() {
        // `Budgets::pressure` is a `max()`, which is only correct while the
        // variants stay ordered from most headroom to least.
        assert!(BudgetPressure::Normal < BudgetPressure::Constrained);
        assert!(BudgetPressure::Constrained < BudgetPressure::Exhausted);
    }

    #[test]
    fn pressure_wire_spellings_are_lowercase() {
        assert_eq!(BudgetPressure::Normal.as_str(), "normal");
        assert_eq!(BudgetPressure::Constrained.as_str(), "constrained");
        assert_eq!(BudgetPressure::Exhausted.as_str(), "exhausted");
    }

    #[test]
    fn a_route_budget_binds_when_key_and_principal_both_have_room() {
        // The team-level cap. Same regression as the key quota:
        // route.monthly_budget_usd was selected, shown in `oag admin`, and
        // never compared against anything.
        let budgets = Budgets {
            key: BudgetState::unlimited(dec!(0)),
            route: BudgetState {
                spent_usd: dec!(500),
                limit_usd: Some(dec!(500)),
                hard_stop_multiple: Decimal::ONE,
            },
            principal: BudgetState::unlimited(dec!(0)),
        };
        assert_eq!(budgets.pressure(), BudgetPressure::Exhausted);
        assert_eq!(budgets.binding(), BudgetScope::Route);

        let err = policy().decide(
            &RoutingMode::Managed,
            None,
            &RequestSignal::default(),
            &budgets,
            &catalog(),
            1024,
        );
        assert!(matches!(
            err,
            Err(Error::BudgetExhausted {
                scope: BudgetScope::Route
            })
        ));
    }

    #[test]
    fn a_tie_names_the_narrowest_cap() {
        let exhausted = || BudgetState {
            spent_usd: dec!(10),
            limit_usd: Some(dec!(10)),
            hard_stop_multiple: Decimal::ONE,
        };

        // All three blown: name the key, which is the one an operator can raise
        // without giving the whole route or the whole person more money.
        let all = Budgets {
            key: exhausted(),
            route: exhausted(),
            principal: exhausted(),
        };
        assert_eq!(all.binding(), BudgetScope::ApiKey);

        // Key is fine, the other two tie: name the route over the principal for
        // the same reason.
        let wider = Budgets {
            key: BudgetState::unlimited(dec!(0)),
            route: exhausted(),
            principal: exhausted(),
        };
        assert_eq!(wider.binding(), BudgetScope::Route);
    }

    /// The ladder's three models plus one the ladder never names.
    fn catalog_with_off_ladder() -> Catalog {
        Catalog::from_entries([
            model("kimi/k2", Provider::Kimi, 128_000, dec!(0.6)),
            model("anthropic/haiku", Provider::Anthropic, 200_000, dec!(1)),
            model("anthropic/opus", Provider::Anthropic, 400_000, dec!(15)),
            model("anthropic/sonnet", Provider::Anthropic, 200_000, dec!(3)),
        ])
    }

    fn all_providers() -> BTreeSet<Provider> {
        [Provider::Kimi, Provider::Anthropic].into_iter().collect()
    }

    fn ids(entries: &[Entitlement<'_>]) -> Vec<String> {
        entries
            .iter()
            .map(|e| e.spec.id.as_str().to_owned())
            .collect()
    }

    fn names(rungs: &[TierName]) -> Vec<String> {
        rungs.iter().map(|r| r.as_str().to_owned()).collect()
    }

    #[test]
    fn entitled_excludes_rungs_below_the_floor() {
        let floored = policy().with_floor(Some(Tier::new(TierName::new("frontier"), 2)));
        let catalog = catalog();
        let listed = floored.entitled(
            &RoutingMode::Managed,
            &catalog,
            &all_providers(),
            BudgetPressure::Normal,
        );
        assert_eq!(ids(&listed), ["anthropic/opus"]);
    }

    #[test]
    fn entitled_excludes_a_provider_with_no_credentials() {
        // Listing a model nobody can reach moves the failure away from its
        // cause: the caller picks it and finds out two layers later.
        let only_kimi: BTreeSet<Provider> = [Provider::Kimi].into_iter().collect();
        let catalog = catalog();
        let listed = policy().entitled(
            &RoutingMode::Managed,
            &catalog,
            &only_kimi,
            BudgetPressure::Normal,
        );
        assert_eq!(ids(&listed), ["kimi/k2"]);
    }

    #[test]
    fn passthrough_lists_off_ladder_models_and_managed_does_not() {
        let catalog = catalog_with_off_ladder();

        let managed = policy().entitled(
            &RoutingMode::Managed,
            &catalog,
            &all_providers(),
            BudgetPressure::Normal,
        );
        assert!(
            !ids(&managed).iter().any(|id| id == "anthropic/sonnet"),
            "managed mode picks for itself, so an off-ladder name is not on offer"
        );

        let passthrough = policy().entitled(
            &RoutingMode::Passthrough,
            &catalog,
            &all_providers(),
            BudgetPressure::Normal,
        );
        assert!(
            ids(&passthrough).iter().any(|id| id == "anthropic/sonnet"),
            "passthrough honours a named model, so the catalog is the menu"
        );
    }

    #[test]
    fn an_off_ladder_name_under_a_floor_is_advertised_as_not_honoured() {
        // The subtle one. In passthrough, `decide` maps an off-ladder name to
        // rank 0 and then clamps to the floor — which returns a *different*
        // model, silently. The caller was explicit and gets something else, so
        // the listing has to say so.
        let catalog = catalog_with_off_ladder();
        let floored = policy().with_floor(Some(Tier::new(TierName::new("balanced"), 1)));
        let listed = floored.entitled(
            &RoutingMode::Passthrough,
            &catalog,
            &all_providers(),
            BudgetPressure::Normal,
        );

        let off_ladder = listed
            .iter()
            .find(|e| e.spec.id.as_str() == "anthropic/sonnet")
            .expect("off-ladder models are listed in passthrough");
        assert!(off_ladder.tier.is_none());
        assert!(
            !off_ladder.honoured,
            "a floor above the cheapest rung silently substitutes a different model"
        );

        // With no floor, the same name is honoured exactly as given.
        let open = policy().entitled(
            &RoutingMode::Passthrough,
            &catalog,
            &all_providers(),
            BudgetPressure::Normal,
        );
        let off_ladder = open
            .iter()
            .find(|e| e.spec.id.as_str() == "anthropic/sonnet")
            .expect("listed");
        assert!(off_ladder.honoured);
    }

    #[test]
    fn managed_mode_honours_no_name() {
        let catalog = catalog();
        let listed = policy().entitled(
            &RoutingMode::Managed,
            &catalog,
            &all_providers(),
            BudgetPressure::Normal,
        );
        assert!(
            listed.iter().all(|e| !e.honoured),
            "in managed mode the model name is advisory; the classifier decides"
        );
    }

    #[test]
    fn virtual_names_come_from_this_ladder_not_a_hardcoded_trio() {
        let ladder = TierLadder::new(vec![
            Rung {
                name: TierName::new("budget"),
                models: vec![ModelId::new("kimi/k2")],
            },
            Rung {
                name: TierName::new("premium"),
                models: vec![ModelId::new("anthropic/opus")],
            },
        ])
        .expect("non-empty");
        let policy = RoutingPolicy::new(ladder, Box::new(HeuristicClassifier::default()));
        assert_eq!(
            names(&policy.virtual_names(BudgetPressure::Normal)),
            ["budget", "premium"]
        );
    }

    #[test]
    fn virtual_names_stop_at_the_floor() {
        let floored = policy().with_floor(Some(Tier::new(TierName::new("balanced"), 1)));
        assert_eq!(
            names(&floored.virtual_names(BudgetPressure::Normal)),
            ["balanced", "frontier"],
            "advertising oag/cheap to a key floored at balanced promises what it cannot deliver"
        );
    }

    fn constrained() -> Budgets {
        Budgets {
            key: BudgetState {
                spent_usd: dec!(25),
                limit_usd: Some(dec!(30)),
                hard_stop_multiple: Decimal::ONE,
            },
            route: BudgetState::unlimited(dec!(0)),
            principal: BudgetState::unlimited(dec!(0)),
        }
    }

    fn exhausted() -> Budgets {
        Budgets {
            key: BudgetState {
                spent_usd: dec!(30),
                limit_usd: Some(dec!(30)),
                hard_stop_multiple: Decimal::ONE,
            },
            route: BudgetState::unlimited(dec!(0)),
            principal: BudgetState::unlimited(dec!(0)),
        }
    }

    fn trivial_signal() -> RequestSignal {
        RequestSignal {
            prompt_tokens: 200,
            turn_count: 1,
            ..RequestSignal::default()
        }
    }

    fn naming_is_honoured(
        policy: &RoutingPolicy,
        mode: &RoutingMode,
        id: &str,
        budget: &Budgets,
        catalog: &Catalog,
    ) -> bool {
        policy
            .decide(mode, Some(id), &trivial_signal(), budget, catalog, 1024)
            .is_ok_and(|d| d.model.id.as_str() == id)
    }

    #[test]
    fn constrained_virtual_names_are_only_the_rung_decide_would_serve() {
        // oag/frontier under Constrained forces managed mode, then budget
        // degrades to cheap. Advertising it is a pin the next turn abandons.
        assert_eq!(
            names(&policy().virtual_names(BudgetPressure::Constrained)),
            ["cheap"]
        );
        let floored = policy().with_floor(Some(Tier::new(TierName::new("balanced"), 1)));
        assert_eq!(
            names(&floored.virtual_names(BudgetPressure::Constrained)),
            ["balanced"],
            "a floor is an entitlement; Constrained cannot hide it"
        );
        assert!(policy().virtual_names(BudgetPressure::Exhausted).is_empty());
    }

    #[test]
    fn entitled_honoured_matches_decide_for_the_same_inputs() {
        // `honoured` is "naming this id is obeyed", not "decide happens to
        // pick this model". Managed classification can land on kimi/k2
        // without the name having been consulted.
        let catalog = catalog();
        let providers = all_providers();
        let p = policy();
        for (mode, budget) in [
            (RoutingMode::Passthrough, rich()),
            (RoutingMode::Passthrough, constrained()),
            (RoutingMode::Managed, rich()),
            (RoutingMode::Managed, constrained()),
        ] {
            let pressure = budget.pressure();
            let listed = p.entitled(&mode, &catalog, &providers, pressure);
            assert!(!listed.is_empty(), "{mode:?} {pressure:?} listed nothing");
            for e in &listed {
                let id = e.spec.id.as_str();
                match mode {
                    RoutingMode::Passthrough => assert_eq!(
                        e.honoured,
                        naming_is_honoured(&p, &mode, id, &budget, &catalog),
                        "{mode:?} {pressure:?} {id}"
                    ),
                    RoutingMode::Managed => {
                        assert!(!e.honoured, "managed never honours a name: {id}");
                    }
                }
            }
        }
    }

    #[test]
    fn exhausted_entitled_is_empty() {
        let catalog = catalog();
        let listed = policy().entitled(
            &RoutingMode::Passthrough,
            &catalog,
            &all_providers(),
            BudgetPressure::Exhausted,
        );
        assert!(listed.is_empty());
        assert!(matches!(
            policy().decide(
                &RoutingMode::Passthrough,
                Some("kimi/k2"),
                &trivial_signal(),
                &exhausted(),
                &catalog,
                1024,
            ),
            Err(Error::BudgetExhausted { .. })
        ));
    }

    #[test]
    fn constrained_passthrough_still_honours_a_named_frontier_model() {
        // decide() returns before budget on the passthrough branch. Listing a
        // frontier name as honoured:false would disagree with the router.
        let catalog = catalog();
        let listed = policy().entitled(
            &RoutingMode::Passthrough,
            &catalog,
            &all_providers(),
            BudgetPressure::Constrained,
        );
        let opus = listed
            .iter()
            .find(|e| e.spec.id.as_str() == "anthropic/opus")
            .expect("listed");
        assert!(opus.honoured);
        assert!(naming_is_honoured(
            &policy(),
            &RoutingMode::Passthrough,
            "anthropic/opus",
            &constrained(),
            &catalog,
        ));
    }

    #[test]
    fn truncated_by_the_callers_own_cap_is_not_a_weaker_model() {
        assert!(truncated_by_client_cap(2, 8192));
        assert!(truncated_by_client_cap(8, 8192));
        assert!(truncated_by_client_cap(8192, 8192));
        assert!(!truncated_by_client_cap(16_000, 8192));
    }

    #[test]
    fn a_named_model_does_not_climb_the_ladder() {
        // The other half of honouring a name: an unusable answer retries the
        // same model on another credential, not the next provider on the
        // ladder. Classified / floor-pinned / budget-downgraded still climb.
        assert!(!climb_allowed(&SelectionReason::Passthrough));
        assert!(climb_allowed(&SelectionReason::Classified));
        assert!(climb_allowed(&SelectionReason::FloorPinned));
        assert!(climb_allowed(&SelectionReason::BudgetDowngraded));
        assert!(climb_allowed(&SelectionReason::Escalated {
            from: TierName::new("cheap"),
            gate: QualityGate::EmptyResponse,
        }));
    }
}
