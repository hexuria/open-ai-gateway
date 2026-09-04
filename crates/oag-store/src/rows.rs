//! Row types.
//!
//! Plain `FromRow` structs rather than `sqlx::query!` macros: the macros need a
//! live database at *compile* time, which makes `cargo build` fail on a machine
//! that has never run Postgres and makes CI depend on a service to typecheck.
//! The trade is that column mistakes surface as a runtime error on first query
//! instead of a compile error, which the integration tests catch.

use oag_core::{AccountId, ApiKeyId, PrincipalId, RouteId};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

/// An upstream credential, as stored.
#[derive(Debug, Clone, FromRow)]
pub struct AccountRow {
    pub id: Uuid,
    pub name: String,
    pub provider: String,
    pub kind: String,
    pub credentials_sealed: Vec<u8>,
    pub credentials_nonce: Vec<u8>,
    pub token_version: i64,
    pub token_expires_at: Option<OffsetDateTime>,
    pub owner_principal_id: Option<Uuid>,
    pub proxy_url: Option<String>,
    pub priority: i16,
    pub max_concurrency: i32,
    pub schedulable: bool,
    pub cooldown_until: Option<OffsetDateTime>,
    pub rate_limited_until: Option<OffsetDateTime>,
    pub window_resets_at: Option<OffsetDateTime>,
    /// The poller's last reading of how much of a subscription's allowance is
    /// left. `None` is unknown, which is not the same as spent.
    pub usage_remaining_pct: Option<Decimal>,
    /// The floor an operator set under that allowance. `None` is no reserve.
    pub usage_reserve_pct: Option<i16>,
    pub last_used_at: OffsetDateTime,
}

impl AccountRow {
    #[must_use]
    pub fn account_id(&self) -> AccountId {
        AccountId::from_uuid(self.id)
    }

    #[must_use]
    pub fn sealed(&self) -> oag_core::Sealed {
        oag_core::Sealed {
            ciphertext: self.credentials_sealed.clone(),
            nonce: self.credentials_nonce.clone(),
        }
    }

    /// Convert into what the scheduler consumes.
    ///
    /// `in_flight` is not a column: it lives in Redis, because it changes many
    /// times a second and every replica has to agree on it. Passing it in keeps
    /// the scheduler a pure function of a snapshot.
    #[must_use]
    pub fn to_candidate(&self, in_flight: u32, waiting: u32) -> Option<oag_pool::Candidate> {
        Some(oag_pool::Candidate {
            account: self.account_id(),
            provider: self.provider.parse().ok()?,
            priority: u8::try_from(self.priority).unwrap_or(u8::MAX),
            max_concurrency: u32::try_from(self.max_concurrency).unwrap_or(0),
            in_flight,
            waiting,
            schedulable: self.schedulable,
            cooldown_until: self.cooldown_until.map(OffsetDateTime::unix_timestamp),
            rate_limited_until: self.rate_limited_until.map(OffsetDateTime::unix_timestamp),
            window_resets_at: self.window_resets_at.map(OffsetDateTime::unix_timestamp),
            usage_remaining_pct: self.usage_remaining_pct,
            usage_reserve_pct: self.usage_reserve_pct.map(Decimal::from),
            last_used_at: self.last_used_at.unix_timestamp(),
        })
    }

    /// Whether this credential is being held out of the pool by its reserve.
    ///
    /// Answers from the columns rather than from a [`oag_pool::Candidate`],
    /// because the caller that needs it most is the one explaining why nothing
    /// could be selected — and by then there is no candidate to ask.
    #[must_use]
    pub fn held_by_reserve(&self) -> bool {
        oag_pool::held_by_reserve(
            self.usage_remaining_pct,
            self.usage_reserve_pct.map(Decimal::from),
        )
    }
}

/// A route's ladder and entitlements.
#[derive(Debug, Clone, FromRow)]
pub struct RouteRow {
    pub id: Uuid,
    pub name: String,
    pub tiers: serde_json::Value,
    pub default_mode: String,
    pub floor_tier: Option<String>,
    pub rpm_limit: Option<i32>,
    pub monthly_budget_usd: Option<Decimal>,
    /// Month-to-date spend on this route. Zero when the route has no budget,
    /// because the query does not bother summing what nothing will compare.
    pub spent_usd: Decimal,
    pub active: bool,
}

impl RouteRow {
    #[must_use]
    pub fn route_id(&self) -> RouteId {
        RouteId::from_uuid(self.id)
    }
}

/// The result of authenticating an inbound key.
///
/// Deliberately a flat, owned, cheaply-cloned struct: it is what goes into the
/// auth cache, and a cache entry that borrows from a connection would pin one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthContext {
    pub api_key_id: Uuid,
    pub principal_id: Uuid,
    pub route_id: Uuid,
    pub key_floor_tier: Option<String>,
    /// Admin authority, carried on the key rather than the principal.
    ///
    /// `#[serde(default)]` is load-bearing: this struct is the Redis L2 cache
    /// value, and an entry written by an older binary must still deserialise
    /// rather than poisoning every request that hits it.
    #[serde(default)]
    pub admin: bool,
    pub quota_usd: Option<Decimal>,
    pub principal_budget_usd: Option<Decimal>,
    pub principal_hard_stop_multiple: Decimal,
    /// sha256 of the key, so a write that changes this identity's limits can
    /// evict it from every cache tier without the plaintext — see
    /// `AuthCache::invalidate_hash`. `#[serde(default)]` for the same reason
    /// as `admin`: an L2 entry written by an older binary must still open.
    #[serde(default)]
    pub key_hash: String,
}

/// What the caller has spent: the key's lifetime total and the principal's
/// month to date.
///
/// Deliberately NOT a field of [`AuthContext`]. That struct is cached for
/// minutes, and it used to carry these two numbers — so the spend cap was
/// enforced against a snapshot up to five minutes old, and N concurrent
/// requests all read the same stale figure, all evaluated the wall as not yet
/// reached, and all went through. `record_usage` increments the columns on
/// every attempt and touches no cache; the only eviction was revocation. Read
/// fresh, per request, by `repo::spend_for`, from columns the ledger write
/// maintains — a primary-key read, not a SUM over the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Spend {
    /// Lifetime, against `api_key.quota_usd`, which is a wall at the number
    /// written on it.
    pub key_usd: Decimal,
    /// Month to date, against `principal.monthly_budget_usd`.
    pub principal_usd: Decimal,
}

impl AuthContext {
    #[must_use]
    pub fn key(&self) -> ApiKeyId {
        ApiKeyId::from_uuid(self.api_key_id)
    }

    #[must_use]
    pub fn principal(&self) -> PrincipalId {
        PrincipalId::from_uuid(self.principal_id)
    }

    #[must_use]
    pub fn route(&self) -> RouteId {
        RouteId::from_uuid(self.route_id)
    }
}

/// Why a credential is or is not serving, as `/v1/models` reports it.
///
/// No name, no id, no sealed material: an inference key learns the health of
/// the seats it may draw on, not the operator's inventory and not a secret.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ChannelStatusRow {
    pub provider: String,
    pub kind: String,
    pub schedulable: bool,
    pub rate_limited_until: Option<OffsetDateTime>,
    pub window_resets_at: Option<OffsetDateTime>,
    pub usage_remaining_pct: Option<Decimal>,
    pub usage_reserve_pct: Option<i16>,
}

/// One catalog entry.
// The capability flags mirror the catalog columns one-for-one; folding them
// into an enum here would just mean unfolding them again on every query.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, FromRow)]
pub struct ModelRow {
    pub id: String,
    pub provider: String,
    pub upstream_name: String,
    pub input_per_mtok: Decimal,
    pub output_per_mtok: Decimal,
    pub cache_read_per_mtok: Option<Decimal>,
    pub cache_write_per_mtok: Option<Decimal>,
    pub context_window: i32,
    pub max_output_tokens: i32,
    pub supports_vision: bool,
    pub supports_tools: bool,
    pub supports_reasoning: bool,
    pub supports_prompt_cache: bool,
    /// What an operator named this model. `None` means nobody has, and the
    /// label is derived — see [`ModelRow::derived_label`].
    pub display_label: Option<String>,
}

impl ModelRow {
    /// What a picker shows when no operator has named this model.
    ///
    /// The same derivation the router does, reached through the same function,
    /// because this one feeds the placeholder in the rename box: a placeholder
    /// that disagreed with the listing would make an operator "fix" a name that
    /// was already right.
    ///
    /// `provider` is free text with no CHECK constraint, so a row nobody can
    /// parse falls back to its own spelling rather than losing the label.
    #[must_use]
    pub fn derived_label(&self) -> String {
        let vendor = self.provider.parse::<oag_core::Provider>().map_or_else(
            |_| self.provider.clone(),
            |p| p.support().display_name.to_owned(),
        );
        oag_router::derive_label(&vendor, &self.upstream_name)
    }

    /// Convert into what the router consumes.
    #[must_use]
    pub fn to_spec(&self) -> Option<oag_router::ModelSpec> {
        Some(oag_router::ModelSpec {
            id: oag_router::ModelId::new(&self.id),
            provider: self.provider.parse().ok()?,
            upstream_name: self.upstream_name.clone(),
            pricing: oag_router::Pricing {
                input_per_mtok: self.input_per_mtok,
                output_per_mtok: self.output_per_mtok,
                cache_read_per_mtok: self.cache_read_per_mtok,
                cache_write_per_mtok: self.cache_write_per_mtok,
            },
            context_window: u32::try_from(self.context_window).unwrap_or(0),
            max_output_tokens: u32::try_from(self.max_output_tokens).unwrap_or(0),
            capabilities: oag_router::Capabilities {
                vision: self.supports_vision,
                tools: self.supports_tools,
                reasoning: self.supports_reasoning,
                prompt_cache: self.supports_prompt_cache,
            },
            display_label: self.display_label.clone(),
        })
    }
}

/// One registered capability service.
///
/// The catalog stores a pointer, not an implementation. `auth_ref` is a
/// foreign key into `account` — the existing credential pool — so a service
/// that needs a secret does not get a second vault.
#[derive(Debug, Clone, FromRow)]
pub struct ServiceRow {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub health_path: String,
    pub dashboard_url: Option<String>,
    pub auth_ref: Option<Uuid>,
    pub enabled: bool,
    pub last_ok: Option<OffsetDateTime>,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
}

impl ServiceRow {
    #[must_use]
    pub fn service_id(&self) -> oag_core::ServiceId {
        oag_core::ServiceId::from_uuid(self.id)
    }
}

/// A row to append to the ledger.
#[derive(Debug, Clone)]
pub struct UsageWrite {
    pub request_id: Uuid,
    /// Which forwarding attempt this row accounts for, counted from zero.
    ///
    /// One client request can pay for two when a quality gate abandons a cheap
    /// answer and retries a rung up. Recorded and uniquely indexed now, but not
    /// yet part of the ledger's primary key: while that is still `request_id`
    /// alone, the second attempt's write is dropped rather than kept. Dropping
    /// the key here instead would break the previous release mid-deploy, so it
    /// waits for a contract release of its own.
    pub attempt: i16,
    pub principal_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub route_id: Option<Uuid>,
    pub account_id: Option<Uuid>,
    pub model_id: String,
    pub tier: String,
    pub selection_reason: String,
    pub escalated_from_tier: Option<String>,
    pub escalation_gate: Option<String>,
    pub usage: oag_router::Usage,
    pub cost_usd: Decimal,
    pub counterfactual_usd: Decimal,
    pub counterfactual_model_id: Option<String>,
    /// What these tokens would have cost at the served model's list API price.
    /// Equals `cost_usd` for a metered account; for a flat-rate seat it is the
    /// pay-per-token bill the subscription displaced, while `cost_usd` is zero.
    pub counterfactual_api_usd: Decimal,
    pub status: i16,
    pub latency_ms: Option<i32>,
    pub ttft_ms: Option<i32>,
    pub streamed: bool,
}
