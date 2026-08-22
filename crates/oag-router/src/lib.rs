#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! The cost engine: catalog, classification, tier ladders, and escalation.
//!
//! This is the crate that justifies the project. sub2api pools credentials and
//! meters usage but always sends a request to whatever model the client named.
//! Most requests do not need a frontier model, and the difference between
//! `claude-opus` and `kimi-k2` on a routine edit is roughly thirty-fold.
//!
//! The whole crate is pure: no database, no network, no clock. That is
//! deliberate — routing policy is the thing you most want to test exhaustively
//! and least want to spin up Postgres for.

pub mod catalog;
pub mod classify;
pub mod cost;
pub mod ladder;
pub mod policy;

pub use catalog::{Capabilities, Catalog, ModelId, ModelSpec, Pricing};
pub use classify::{Classifier, HeuristicClassifier, RequestSignal};
pub use cost::{Usage, counterfactual};
pub use ladder::TierLadder;
pub use policy::{
    BudgetPressure, BudgetState, Budgets, QualityGate, RoutingDecision, RoutingPolicy,
    SelectionReason, escalation_allowed,
};
