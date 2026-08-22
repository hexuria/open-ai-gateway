//! Cost tiers.
//!
//! A tier is a named rung on a route's ladder — `cheap`, `balanced`,
//! `frontier`. Names are operator-defined rather than a fixed enum, because
//! "how many rungs and what they mean" is a policy question that differs per
//! organisation. What the type system *does* enforce is that rungs are ordered,
//! so escalation and budget-downgrade can move up and down without stringly
//! comparing names.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The name of a rung. Cheap to clone, compared case-sensitively.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TierName(pub String);

impl TierName {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TierName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TierName {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// A rung, with its position on the ladder.
///
/// `rank` is the ordering key: 0 is cheapest. Ordering derives from `rank`
/// alone so `a < b` means "a is cheaper than b" regardless of naming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tier {
    pub name: TierName,
    /// 0 is the cheapest rung on the ladder.
    pub rank: u8,
}

impl Tier {
    #[must_use]
    pub fn new(name: impl Into<TierName>, rank: u8) -> Self {
        Self {
            name: name.into(),
            rank,
        }
    }
}

impl PartialEq for Tier {
    fn eq(&self, other: &Self) -> bool {
        self.rank == other.rank
    }
}

impl Eq for Tier {}

impl PartialOrd for Tier {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Tier {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank.cmp(&other.rank)
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// How the caller wants a model chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    /// The caller named a concrete model. Honour it, subject to the key's floor
    /// tier. Never surprise a caller who was explicit — that is how you get
    /// a bug report about "the gateway silently downgraded my agent".
    Passthrough,
    /// The caller asked for a virtual model (`oag/auto`, `oag/cheap`, …) and
    /// policy decides. This is where the savings come from.
    Managed,
}
