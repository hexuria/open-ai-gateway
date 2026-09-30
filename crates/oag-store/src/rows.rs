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
    /// Month-to-date spend on this route, read from the denormalised
    /// `route.spent_usd` and zero once the month `route.spent_month` names
    /// has passed. Kept whether or not the route has a budget: the column is
    /// written on every debit, so reading it costs nothing.
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
    /// When this key stops working, if it ever does.
    ///
    /// Carried so the cache cannot outlive it. `authenticate` filters expired
    /// keys at read time, but an entry cached just before expiry kept
    /// authenticating for the L2 TTL's full five minutes — on every replica, and
    /// with the row in the database already saying no. A key with an expiry is a
    /// key someone chose to time-box, and five minutes past the deadline is a
    /// promise broken quietly.
    ///
    /// `#[serde(default)]` for the same reason `admin` has it: this struct is
    /// the L2 cache value, and an entry written by an older binary must still
    /// deserialise rather than poisoning every request that hits it. An older
    /// entry reads as `None`, which is the pre-existing behaviour and expires
    /// on the TTL as it always did.
    #[serde(default)]
    pub expires_at: Option<OffsetDateTime>,
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

/// A catalog row with whether an operator override protects it from a refresh:
/// what an endpoint's catalog sync compares the endpoint's list against.
#[derive(Debug, Clone, FromRow)]
pub struct StoredModelRow {
    #[sqlx(flatten)]
    pub model: ModelRow,
    pub is_override: bool,
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

/// An upstream an operator registered, as stored.
///
/// Strings, not the core types: the store persists what it is given, and the
/// schema's CHECKs are the second line behind whoever parses these. There is no
/// key here and never will be. A credential for an endpoint is an `account` row
/// whose `provider` is this row's `name`.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct EndpointRow {
    /// Identity and model prefix: `groq` serves `groq/<model>`.
    pub name: String,
    /// `openai`, `anthropic`, `gemini`, `system_one` or `bedrock_converse`.
    pub dialect: String,
    /// `plain`, `azure`, `aws` or `gcp`.
    pub platform: String,
    /// `None` only on `aws` and `gcp`, whose host comes from the region.
    pub base_url: Option<String>,
    /// How the key is presented to a `plain` upstream: `bearer`, `x_api_key`,
    /// `x_goog_api_key`, `api_key_header` or `none`.
    pub auth: String,
    pub region: Option<String>,
    pub project: Option<String>,
    pub api_version: Option<String>,
    /// A JSON object of headers that carry no authority. The schema promises
    /// only that it is an object; its values are for the caller to check.
    pub extra_headers: serde_json::Value,
    pub display_name: Option<String>,
    pub discover_models: bool,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl EndpointRow {
    /// This row as an endpoint, or the first rule it breaks.
    ///
    /// The one reading of an endpoint row. The gateway's reload serves what
    /// passes and skips what does not, and the CLI registers what passes
    /// before it parses `--provider`, so a row that breaks a rule is one the
    /// CLI will not file a key under either.
    pub fn to_endpoint(
        &self,
    ) -> Result<oag_core::endpoint::EndpointConfig, oag_core::endpoint::Refusal> {
        oag_core::endpoint::EndpointConfig::from_columns(&oag_core::endpoint::Columns {
            name: &self.name,
            dialect: &self.dialect,
            platform: &self.platform,
            base_url: self.base_url.as_deref(),
            auth: &self.auth,
            region: self.region.as_deref(),
            project: self.project.as_deref(),
            api_version: self.api_version.as_deref(),
            extra_headers: &self.extra_headers,
        })
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

#[cfg(test)]
mod tests {
    use super::EndpointRow;
    use oag_core::endpoint::Reason;
    use oag_core::provider::{AuthStyle, Dialect, Platform};

    /// A Vertex row: the platform that uses every optional column but the API
    /// version, which is given anyway.
    fn row() -> EndpointRow {
        EndpointRow {
            name: "t4-rows-vertex".to_owned(),
            dialect: "anthropic".to_owned(),
            platform: "gcp".to_owned(),
            base_url: Some("https://us-east5-aiplatform.googleapis.com/".to_owned()),
            auth: "bearer".to_owned(),
            region: Some("us-east5".to_owned()),
            project: Some("acme-prod".to_owned()),
            api_version: Some("v1".to_owned()),
            extra_headers: serde_json::json!({"x-goog-user-project": "acme-prod"}),
            display_name: Some("Vertex Claude".to_owned()),
            discover_models: true,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn an_endpoint_row_lends_every_column_to_the_one_mapping() {
        let config = row().to_endpoint().expect("a valid row");
        assert_eq!(config.endpoint.name(), "t4-rows-vertex");
        assert_eq!(config.endpoint.dialect(), Dialect::AnthropicMessages);
        assert_eq!(config.endpoint.platform(), Platform::Gcp);
        assert_eq!(
            config.base_url.as_deref(),
            Some("https://us-east5-aiplatform.googleapis.com")
        );
        assert_eq!(config.auth, AuthStyle::Bearer);
        assert_eq!(config.region.as_deref(), Some("us-east5"));
        assert_eq!(config.project.as_deref(), Some("acme-prod"));
        assert_eq!(config.api_version.as_deref(), Some("v1"));
        assert_eq!(
            config.extra_headers,
            [("x-goog-user-project".to_owned(), "acme-prod".to_owned())]
        );
    }

    #[test]
    fn a_row_the_mapping_refuses_comes_back_with_its_reason() {
        let mut region = row();
        region.region = Some("us east5".to_owned());
        assert_eq!(
            region.to_endpoint().map_err(|r| r.reason),
            Err(Reason::Region)
        );

        let mut project = row();
        project.project = None;
        assert_eq!(
            project.to_endpoint().map_err(|r| r.reason),
            Err(Reason::Project)
        );

        let mut auth = row();
        auth.auth = "x_goog_api_key".to_owned();
        assert_eq!(
            auth.to_endpoint().map_err(|r| r.reason),
            Err(Reason::Auth),
            "a minted token rides as a bearer, so a gcp row says so"
        );

        let mut plain = row();
        plain.platform = "plain".to_owned();
        assert_eq!(
            plain.to_endpoint().map_err(|r| r.reason),
            Err(Reason::Compliance),
            "the same Google host is refused to a plain endpoint"
        );
    }
}
