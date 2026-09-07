//! Shared application state.

use crate::breakers::Breakers;
use crate::shutdown::Lifecycle;
use oag_core::config::Config;
use oag_core::{Error, Kek, Provider, Result};
use oag_router::Catalog;
use oag_store::{AuthCache, Cache, Db};
use oag_upstream::{ProviderAdapter, TransportPool};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub cache: Cache,
    pub auth: AuthCache,
    pub lifecycle: Arc<Lifecycle>,
    pub transports: TransportPool,
    pub kek: Arc<Kek>,
    pub breakers: Arc<Breakers>,
    /// One mutex per credential, so concurrent requests on this replica make at
    /// most one attempt at the fleet-wide refresh lock between them.
    refresh_gates: Arc<std::sync::Mutex<HashMap<oag_core::AccountId, Arc<tokio::sync::Mutex<()>>>>>,
    adapters: Arc<HashMap<Provider, Arc<dyn ProviderAdapter>>>,
    /// The Codex/`ChatGPT` subscription adapter. Held apart from `adapters`
    /// because it shares OpenAI's provider key but not its dialect — it is
    /// selected per-account for an OpenAI OAuth seat, in the gateway.
    codex: Arc<dyn ProviderAdapter>,
    /// Swapped wholesale on refresh rather than mutated in place, so a request
    /// that started with one catalog finishes with it — a price changing
    /// halfway through a request would make the ledger disagree with itself.
    catalog: Arc<RwLock<Arc<Catalog>>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("providers", &self.adapters.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// A configured base URL, in the one shape every adapter's concatenation expects.
///
/// Trailing slashes go, because every adapter builds its request URL by
/// appending a path: `https://host/` became `https://host//v1/messages`, which
/// most upstreams tolerate and some do not — a configuration bug that works in
/// the deployment where it was typed and fails in the next one.
///
/// A query or a fragment is refused rather than trimmed. Appending a path after
/// either produces a URL that means something different — `https://host/?x=1`
/// plus `/v1/messages` is a query string containing a path, not a path — and
/// guessing which half the operator meant is worse than saying so. Refused at
/// startup, where it is a config error, rather than surfacing later as a 404
/// from an upstream that never received the request.
fn normalise_base_url(provider: &str, raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(oag_core::Error::Config(format!(
            "the base URL for {provider} is empty"
        )));
    }
    if let Some(bad) = ['?', '#'].into_iter().find(|c| trimmed.contains(*c)) {
        return Err(oag_core::Error::Config(format!(
            "the base URL for {provider} contains '{bad}': {trimmed}. Every request path is \
             appended to it, so a query or fragment here would silently change what the \
             resulting URL means."
        )));
    }
    if !trimmed.contains("://") {
        return Err(oag_core::Error::Config(format!(
            "the base URL for {provider} has no scheme: {trimmed}"
        )));
    }
    Ok(trimmed.trim_end_matches('/').to_owned())
}

impl AppState {
    pub fn new(config: Config, db: Db, cache: Cache) -> Result<Self> {
        let kek = Kek::from_base64(&config.security.credential_kek)?;

        // A slot must outlive the longest request it guards. `SLOT_TTL` is a
        // constant and `max_stream_duration` is configuration, so the only
        // place the two can be compared is here, where both exist.
        //
        // Refused rather than clamped: a slot expiring under a live request
        // oversubscribes the credential silently — nothing observes a slot
        // vanishing, so the first symptom is a provider's own rate limit on a
        // deployment that believes it is within its limits.
        if config.gateway.max_stream_duration >= crate::gateway::select::SLOT_TTL {
            return Err(oag_core::Error::Config(format!(
                "gateway.max_stream_duration ({:?}) must be shorter than the concurrency \
                 slot TTL ({:?}), or a live request's slot expires under it and the \
                 credential is oversubscribed",
                config.gateway.max_stream_duration,
                crate::gateway::select::SLOT_TTL,
            )));
        }

        // Normalised once, here, rather than trusted at every use site.
        //
        // Every adapter builds its request URL by concatenation —
        // `format!("{base}/v1/messages")` and its siblings — so a configured
        // base URL with a trailing slash produced `https://host//v1/messages`.
        // Most upstreams tolerate that and some do not, which is the worst kind
        // of configuration bug: it works in the deployment where it was typed.
        //
        // A query or a fragment cannot be normalised away, because appending a
        // path after either produces a URL that means something else entirely —
        // `https://host/?x=1/v1/messages` is a query string, not a path. That is
        // a config error and is refused as one, at startup, rather than
        // becoming a 404 from an upstream at request time.
        let base = |p: Provider, default: &str| -> Result<String> {
            let raw = config
                .gateway
                .provider_base_urls
                .get(p.as_str())
                .cloned()
                .unwrap_or_else(|| default.to_owned());
            normalise_base_url(p.as_str(), &raw)
        };

        let mut adapters: HashMap<Provider, Arc<dyn ProviderAdapter>> = HashMap::new();
        adapters.insert(
            Provider::Anthropic,
            Arc::new(oag_upstream::AnthropicAdapter::new(base(
                Provider::Anthropic,
                "https://api.anthropic.com",
            )?)),
        );

        // Region and endpoint are separate for Bedrock: the region is part of
        // the SigV4 signing scope and stays correct even when the endpoint is
        // overridden to a VPC endpoint, a proxy, or a mock.
        adapters.insert(
            Provider::Bedrock,
            Arc::new(
                oag_upstream::BedrockAdapter::new(config.gateway.bedrock_region.clone())
                    .with_endpoint(
                        config
                            .gateway
                            .provider_base_urls
                            .get("bedrock")
                            .map(|raw| normalise_base_url("bedrock", raw))
                            .transpose()?,
                    ),
            ),
        );

        adapters.insert(
            Provider::Gemini,
            Arc::new(oag_upstream::GeminiAdapter::new(base(
                Provider::Gemini,
                "https://generativelanguage.googleapis.com/v1beta",
            )?)),
        );

        // Five providers, one adapter: they all speak Chat Completions and
        // differ only in base URL.
        for p in [
            Provider::OpenAI,
            Provider::Kimi,
            Provider::DeepSeek,
            Provider::Zhipu,
            Provider::XAI,
        ] {
            let url = base(p, oag_upstream::OpenAICompatAdapter::default_base_url(p))?;
            adapters.insert(p, Arc::new(oag_upstream::OpenAICompatAdapter::new(p, url)));
        }

        // The Codex adapter shares OpenAI's provider key, so it lives outside
        // the provider map and is chosen per-account. Instructions come from a
        // file when a path is given, else inline; the adapter default is
        // pass-through, which the Codex backend will reject.
        let cx = &config.gateway.codex;
        let instructions = match &cx.instructions_path {
            Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
                Error::Config(format!("reading codex instructions from {path}: {e}"))
            })?),
            None => cx.instructions.clone(),
        };
        let codex: Arc<dyn ProviderAdapter> = Arc::new(
            oag_upstream::CodexAdapter::new()
                // Normalised like every other adapter's. This one was passed
                // through raw, so a configured `https://host/` produced
                // `https://host//responses` and a base URL carrying a query was
                // accepted here and refused everywhere else — the inconsistency
                // being worse than either behaviour, because it makes the rule
                // untrue rather than merely strict.
                .with_base_url(normalise_base_url("codex", &cx.base_url)?)
                .with_instructions(instructions)
                .with_beta(cx.beta.clone())
                .with_originator(cx.originator.clone())
                .with_user_agent(cx.user_agent.clone()),
        );

        Ok(Self {
            auth: AuthCache::new(
                db.clone(),
                cache.clone(),
                10_000,
                &config.security.signing_secret,
                // Twice the pool. A lookup is a primary-key probe that holds a
                // connection for a millisecond, so this bounds the queue at
                // the pool to one lookup per connection rather than reserving
                // connections for them; past that, a miss sheds.
                usize::try_from(config.database.max_connections).unwrap_or(16) * 2,
            ),
            transports: TransportPool::new(
                2_048,
                Duration::from_mins(15),
                Duration::from_secs(10),
                config.gateway.upstream_response_timeout,
            ),
            config: Arc::new(config),
            db,
            cache,
            lifecycle: Arc::new(Lifecycle::new()),
            kek: Arc::new(kek),
            breakers: Arc::new(Breakers::new()),
            refresh_gates: Arc::new(std::sync::Mutex::new(HashMap::new())),
            adapters: Arc::new(adapters),
            codex,
            catalog: Arc::new(RwLock::new(Arc::new(Catalog::new()))),
        })
    }

    /// The adapter for a provider, or an error naming the provider we lack.
    pub fn adapter(&self, provider: Provider) -> Result<Arc<dyn ProviderAdapter>> {
        self.adapters
            .get(&provider)
            .cloned()
            .ok_or_else(|| Error::Internal(format!("no adapter for provider {provider}")))
    }

    /// The Codex adapter, for an OpenAI subscription seat. Selected in the
    /// gateway when the leased account is an OpenAI OAuth credential.
    #[must_use]
    pub fn codex_adapter(&self) -> Arc<dyn ProviderAdapter> {
        Arc::clone(&self.codex)
    }

    #[must_use]
    pub fn providers(&self) -> Vec<Provider> {
        self.adapters.keys().copied().collect()
    }

    /// The per-credential refresh gate, creating it on first use.
    pub fn refresh_gate(&self, account: oag_core::AccountId) -> Arc<tokio::sync::Mutex<()>> {
        self.refresh_gates.lock().map_or_else(
            // A poisoned lock hands back a private mutex rather than failing:
            // the worst case is one extra attempt at the distributed lock,
            // which that lock is there to arbitrate anyway.
            |_| Arc::new(tokio::sync::Mutex::new(())),
            |mut m| Arc::clone(m.entry(account).or_default()),
        )
    }

    /// A snapshot of the catalog. Cheap: one `Arc` clone.
    pub async fn catalog(&self) -> Arc<Catalog> {
        Arc::clone(&*self.catalog.read().await)
    }

    /// Replace the catalog wholesale.
    pub async fn set_catalog(&self, catalog: Catalog) {
        *self.catalog.write().await = Arc::new(catalog);
    }

    /// Load the catalog from the database into memory.
    pub async fn reload_catalog(&self) -> Result<usize> {
        let rows = oag_store::repo::catalog(&self.db).await?;
        let specs: Vec<_> = rows
            .iter()
            .filter_map(oag_store::ModelRow::to_spec)
            .collect();
        let n = specs.len();
        self.set_catalog(Catalog::from_entries(specs)).await;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::{AppState, normalise_base_url};

    /// U5. A configured base URL is normalised once, or refused.
    ///
    /// Every adapter builds its request URL by concatenation, so a trailing
    /// slash produced `https://host//v1/messages` — which most upstreams
    /// tolerate and some do not. That is the worst kind of configuration bug:
    /// it works in the deployment where it was typed.
    #[test]
    fn a_base_url_is_trimmed_or_refused_at_startup() {
        for raw in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://api.anthropic.com///",
            "  https://api.anthropic.com/  ",
        ] {
            assert_eq!(
                normalise_base_url("anthropic", raw).expect("normalises"),
                "https://api.anthropic.com",
                "every spelling of the same endpoint has to reach the adapter \
                 identically: {raw}"
            );
        }

        // A path is legitimate and kept: Gemini's own default carries one.
        assert_eq!(
            normalise_base_url(
                "gemini",
                "https://generativelanguage.googleapis.com/v1beta/"
            )
            .expect("normalises"),
            "https://generativelanguage.googleapis.com/v1beta"
        );

        // A query or fragment cannot be normalised away. Appending a path after
        // either means something else entirely, and guessing which half the
        // operator meant is worse than saying so.
        for raw in ["https://host/?apikey=secret", "https://host/#anchor"] {
            let err = normalise_base_url("openai", raw).expect_err("refused");
            assert!(
                err.to_string().contains(raw.trim()),
                "the operator has to see which value was rejected: {err}"
            );
        }

        // And nonsense is refused at startup rather than becoming a 404 from an
        // upstream that never received the request.
        assert!(normalise_base_url("openai", "api.openai.com").is_err());
        assert!(normalise_base_url("openai", "   ").is_err());
    }

    /// C3: every adapter goes through the normaliser, including this one.
    ///
    /// Codex was constructed with `cx.base_url` passed straight through, so a
    /// configured `https://host/` produced `https://host//responses` and a base
    /// URL carrying a query was accepted here while being refused for every
    /// other provider. The inconsistency is worse than either behaviour on its
    /// own: it makes the rule untrue rather than merely strict.
    ///
    /// Asserted through `AppState::new` rather than by calling the normaliser
    /// again — the helper was never the thing in doubt, the wiring was, and a
    /// second test of the helper would have passed with Codex still bypassing
    /// it.
    // `#[tokio::test]`: `Db::connect` builds a lazy sqlx pool, which needs a
    // runtime in scope even though it dials nothing.
    #[tokio::test]
    async fn a_codex_base_url_is_normalised_like_every_other() {
        let config = |base: &str| {
            oag_core::config::Config::from_yaml(&format!(
                r#"
database:
  url: "postgres://oag:oag@127.0.0.1:1/oag"
redis:
  url: "redis://127.0.0.1:1"
security:
  signing_secret: "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0="
  credential_kek: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
gateway:
  codex:
    base_url: "{base}"
"#
            ))
            .expect("test config")
        };
        let build = |base: &str| {
            let config = config(base);
            let db = oag_store::Db::connect(&config.database.url, 1).expect("lazy pool");
            let cache = oag_store::Cache::connect(&config.redis.url).expect("lazy client");
            AppState::new(config, db, cache)
        };

        build("https://chatgpt.com/backend-api/codex/")
            .expect("a trailing slash is normalised away, not rejected");

        // The refusal is the proof: it can only come from the normaliser, and
        // the normaliser can only be reached if this adapter goes through it.
        let err = build("https://chatgpt.com/backend-api/codex?token=secret")
            .expect_err("a query in a base URL is refused for codex too");
        assert!(err.to_string().contains("codex"), "{err}");
    }

    /// U5, at every adapter rather than at the normaliser.
    ///
    /// `a_base_url_is_trimmed_or_refused_at_startup` above proves the
    /// normaliser; `a_codex_base_url_is_normalised_like_every_other` proves one
    /// call site. Between them sat eight more adapters, each constructed with
    /// its own line, any of which could have been written to pass
    /// `provider_base_urls` straight through — which is exactly what Codex did,
    /// and nothing failed until somebody read it.
    ///
    /// The refusal is the proof, as it was for Codex: a `?` can only be
    /// rejected by the normaliser, so a provider whose configured base URL is
    /// refused is a provider whose base URL went through it. `vertex` is absent
    /// on purpose — it has no adapter here, so its key is inert, and asserting
    /// a refusal for it would pin a behaviour that does not exist.
    #[tokio::test]
    async fn every_configured_base_url_goes_through_the_normaliser() {
        let build = |provider: &str, url: &str| {
            let config = oag_core::config::Config::from_yaml(&format!(
                r#"
database:
  url: "postgres://oag:oag@127.0.0.1:1/oag"
redis:
  url: "redis://127.0.0.1:1"
security:
  signing_secret: "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0="
  credential_kek: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
gateway:
  provider_base_urls:
    {provider}: "{url}"
"#
            ))
            .expect("a base URL override is valid configuration in its own right");
            let db = oag_store::Db::connect(&config.database.url, 1).expect("lazy pool");
            let cache = oag_store::Cache::connect(&config.redis.url).expect("lazy client");
            AppState::new(config, db, cache)
        };

        for provider in [
            "anthropic",
            "bedrock",
            "gemini",
            "openai",
            "kimi",
            "deepseek",
            "zhipu",
            "xai",
        ] {
            build(provider, "https://proxy.internal/upstream/")
                .unwrap_or_else(|e| panic!("{provider}: a trailing slash normalises away: {e}"));

            let Err(err) = build(provider, "https://proxy.internal/upstream?token=secret") else {
                panic!("{provider}: a query cannot be normalised away, so it is refused");
            };
            assert!(
                err.to_string().contains(provider),
                "the operator has to be told which provider's base URL was \
                 rejected, because they configured several: {err}"
            );
        }
    }

    /// G8. A stream ceiling that outlives a concurrency slot is refused here.
    ///
    /// A slot must outlive the longest request it guards. One that expires
    /// under a live request oversubscribes the credential silently — nothing
    /// observes a slot vanishing, so the first symptom is the provider's own
    /// rate limit on a deployment that believes it is inside its limits.
    ///
    /// `select.rs` used to assert this by comparing `SLOT_TTL` against the
    /// shipped default: two constants, which could only disagree if somebody
    /// edited one of them, and which said nothing about the deployment that
    /// raises the ceiling in its own YAML. That is precisely the deployment the
    /// check exists for, so the assertion lives on the call instead — delete the
    /// refusal in `AppState::new` and this fails, whereas the old one did not.
    #[tokio::test]
    async fn a_stream_ceiling_that_outlives_a_slot_is_refused_at_startup() {
        let build = |max_stream_duration: u64| {
            let config = oag_core::config::Config::from_yaml(&format!(
                r#"
database:
  url: "postgres://oag:oag@127.0.0.1:1/oag"
redis:
  url: "redis://127.0.0.1:1"
security:
  signing_secret: "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MGFiY2RlZmdoaWprbG0="
  credential_kek: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
gateway:
  max_stream_duration: {max_stream_duration}
"#
            ))
            .expect("the ceiling is valid configuration in its own right");
            let db = oag_store::Db::connect(&config.database.url, 1).expect("lazy pool");
            let cache = oag_store::Cache::connect(&config.redis.url).expect("lazy client");
            AppState::new(config, db, cache)
        };

        let ttl = crate::gateway::select::SLOT_TTL.as_secs();
        build(oag_core::config::Config::default_gateway_max_stream_duration().as_secs())
            .expect("the shipped default must leave room, or no deployment starts");
        build(ttl - 1).expect("a ceiling one second inside the TTL still fits");

        for over in [ttl, ttl + 60] {
            let err = build(over).expect_err("a ceiling at or past the slot TTL is refused");
            assert!(
                err.to_string().contains("max_stream_duration") && err.to_string().contains("slot"),
                "the refusal names both numbers, because only the operator can \
                 reconcile them: {err}"
            );
        }
    }
}
