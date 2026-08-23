#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! Domain types, errors, and typed configuration for open-ai-gateway.
//!
//! This crate performs no I/O. It exists so that every other crate agrees on
//! what an account, a tier, a provider, and a cost *are* without any of them
//! having to depend on a database, an HTTP client, or each other.

pub mod config;
pub mod credential;
pub mod error;
pub mod id;
pub mod provider;
pub mod seal;
pub mod service;
pub mod tier;

pub use error::{BudgetScope, Disposition, Error, Result};
pub use id::{AccountId, ApiKeyId, PrincipalId, RequestId, RouteId, ServiceId};
pub use provider::Provider;
pub use seal::{Kek, Sealed};
pub use service::{ServiceKind, catalog_url, health_url, ip_is_denied};
pub use tier::{Tier, TierName};
