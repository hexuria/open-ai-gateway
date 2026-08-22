//! Tier ladders: the ordered rungs a route can serve from.

use crate::catalog::{Catalog, ModelId, ModelSpec, Requirements};
use oag_core::{Tier, TierName};
use serde::{Deserialize, Serialize};

/// One rung and the models that can serve it, in preference order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rung {
    pub name: TierName,
    /// Preference order. The first that satisfies the request and has a live
    /// credential wins.
    pub models: Vec<ModelId>,
}

/// A route's ordered ladder, cheapest first.
///
/// Ordering is positional: index 0 is the cheapest rung. That makes escalation
/// and budget downgrade index arithmetic rather than name lookups, and makes
/// "is this rung above that one" a total order rather than a convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TierLadder {
    rungs: Vec<Rung>,
}

impl TierLadder {
    /// Build a ladder. Rungs must be given cheapest-first.
    ///
    /// Returns `None` for an empty ladder: a route with no rungs can serve
    /// nothing, and that is a configuration error worth catching at load time
    /// rather than on the first request.
    #[must_use]
    pub fn new(rungs: Vec<Rung>) -> Option<Self> {
        if rungs.is_empty() {
            return None;
        }
        Some(Self { rungs })
    }

    #[must_use]
    pub fn rungs(&self) -> &[Rung] {
        &self.rungs
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rungs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        false // construction rejects empty ladders
    }

    /// The cheapest rung.
    #[must_use]
    pub fn floor(&self) -> Tier {
        Tier::new(self.rungs[0].name.clone(), 0)
    }

    /// The most capable rung. Used as the counterfactual baseline.
    #[must_use]
    pub fn ceiling(&self) -> Tier {
        let rank = u8::try_from(self.rungs.len() - 1).unwrap_or(u8::MAX);
        Tier::new(self.rungs[self.rungs.len() - 1].name.clone(), rank)
    }

    /// Look a rung up by name.
    #[must_use]
    pub fn tier(&self, name: &TierName) -> Option<Tier> {
        self.rungs
            .iter()
            .position(|r| &r.name == name)
            .and_then(|i| u8::try_from(i).ok())
            .map(|rank| Tier::new(name.clone(), rank))
    }

    /// The rung one step more capable, or `None` at the ceiling.
    #[must_use]
    pub fn escalate(&self, from: &Tier) -> Option<Tier> {
        let next = usize::from(from.rank).checked_add(1)?;
        let rung = self.rungs.get(next)?;
        u8::try_from(next).ok().map(|r| Tier::new(rung.name.clone(), r))
    }

    /// The rung one step cheaper, or `None` at the floor.
    #[must_use]
    pub fn downgrade(&self, from: &Tier) -> Option<Tier> {
        let prev = usize::from(from.rank).checked_sub(1)?;
        let rung = self.rungs.get(prev)?;
        u8::try_from(prev).ok().map(|r| Tier::new(rung.name.clone(), r))
    }

    /// Clamp a tier to a floor, so a key pinned to `frontier` never routes below it.
    #[must_use]
    pub fn clamp_to_floor(&self, tier: Tier, floor: Option<&Tier>) -> Tier {
        match floor {
            Some(f) if tier < *f => f.clone(),
            _ => tier,
        }
    }

    /// First model on `tier` that can actually serve this request.
    ///
    /// Returns `None` when the rung exists but nothing on it is capable enough,
    /// which is the caller's cue to escalate rather than to fail.
    #[must_use]
    pub fn pick<'c>(
        &self,
        tier: &Tier,
        catalog: &'c Catalog,
        need: &Requirements,
    ) -> Option<&'c ModelSpec> {
        let rung = self.rungs.get(usize::from(tier.rank))?;
        rung.models
            .iter()
            .filter_map(|id| catalog.get(id))
            .find(|spec| spec.satisfies(need))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder() -> TierLadder {
        TierLadder::new(vec![
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
        .expect("non-empty")
    }

    #[test]
    fn empty_ladder_is_rejected_at_construction() {
        assert!(TierLadder::new(vec![]).is_none());
    }

    #[test]
    fn escalation_walks_up_and_stops_at_the_ceiling() {
        let l = ladder();
        let cheap = l.floor();
        let balanced = l.escalate(&cheap).expect("cheap escalates");
        assert_eq!(balanced.name.as_str(), "balanced");
        let frontier = l.escalate(&balanced).expect("balanced escalates");
        assert_eq!(frontier.name.as_str(), "frontier");
        assert!(l.escalate(&frontier).is_none(), "ceiling must not escalate");
    }

    #[test]
    fn downgrade_walks_down_and_stops_at_the_floor() {
        let l = ladder();
        let top = l.ceiling();
        let mid = l.downgrade(&top).expect("ceiling downgrades");
        let bottom = l.downgrade(&mid).expect("mid downgrades");
        assert_eq!(bottom.name.as_str(), "cheap");
        assert!(l.downgrade(&bottom).is_none(), "floor must not downgrade");
    }

    #[test]
    fn floor_pin_overrides_a_cheaper_choice() {
        let l = ladder();
        let pinned = l.tier(&TierName::new("frontier")).expect("known rung");
        // Policy wanted cheap; the key is pinned to frontier and wins.
        let got = l.clamp_to_floor(l.floor(), Some(&pinned));
        assert_eq!(got.name.as_str(), "frontier");
    }

    #[test]
    fn floor_pin_does_not_drag_a_higher_choice_down() {
        let l = ladder();
        let pin = l.floor();
        let got = l.clamp_to_floor(l.ceiling(), Some(&pin));
        assert_eq!(got.name.as_str(), "frontier", "a floor is a minimum, not a target");
    }
}
