#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Postgres and Redis.
//!
//! Postgres is the truth; Redis is coordination between replicas. The split
//! matters: everything in Redis must be reconstructible or expendable, because
//! a Redis restart must not lose money or credentials.

pub mod auth;
pub mod cache;
pub mod db;
pub mod health;
pub mod repo;
pub mod rows;

pub use auth::AuthCache;
pub use cache::Cache;
pub use db::Db;
pub use health::{Readiness, readiness};
pub use repo::{NewService, ServiceUpdate};
pub use rows::{AccountRow, AuthContext, ModelRow, RouteRow, ServiceRow, UsageWrite};
