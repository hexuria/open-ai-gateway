#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Choosing which credential serves a request, and noticing when one is sick.
//!
//! Like `oag-router`, this crate is pure: it takes a snapshot of candidate
//! credentials and returns a choice. Reading that snapshot from Postgres,
//! holding concurrency slots in Redis, and persisting cooldowns all live in
//! `oag-store`, so the policy that decides *which credential* can be tested
//! exhaustively without any of it.

pub mod breaker;
pub mod schedule;
pub mod sticky;

pub use breaker::{Admission, Breaker, BreakerState};
pub use schedule::{Candidate, Selection, select};
pub use sticky::SessionKey;
