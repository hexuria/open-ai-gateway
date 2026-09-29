//! Shared application state.

use crate::breakers::Breakers;
use crate::shutdown::Lifecycle;
use oag_core::config::Config;
use oag_core::{Error, Kek, Provider, Result};
use oag_router::{Catalog, ModelSpec};
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
    /// The System One upstream. Outside `adapters` for the reason Codex is,
    /// and more so: it is not a chat adapter at all.
    jev: oag_upstream::JevUpstream,
    /// Swapped wholesale on refresh rather than mutated in place, so a request
    /// that started with one catalog finishes with it — a price changing
    /// halfway through a request would make the ledger disagree with itself.
    ///
    /// Chat models only: everything a chat request routes over, lists or
    /// prices against reads this. See [`AppState::set_catalog`].
    catalog: Arc<RwLock<Arc<Catalog>>>,
    /// The models no chat request may reach — System One's — which that route
    /// reads for nothing but their prices.
    system_one: Arc<RwLock<Arc<Catalog>>>,
    /// A8. The readiness answer, memoised for a second by `health::ready`.
    ///
    /// A field rather than the process-global `OnceLock` it was: that made the
    /// memo shared by every `AppState` in the process, so a test priming one
    /// state's readiness answered for another's, and two gateways in one binary
    /// would report each other's backends. Nothing else on this struct is
    /// process-global, and this had no reason to be.
    pub(crate) readiness:
        Arc<tokio::sync::Mutex<Option<(std::time::Instant, oag_store::Readiness)>>>,
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

        // Slot TTL used to have to outlive `max_stream_duration`: a live
        // request never refreshed its Redis score, so a shorter TTL expired
        // the slot under the stream and oversubscribed the credential. The
        // guard now heartbeats, so the TTL is a crash lease. A 30-minute
        // stream on a two-minute lease is the point, not a config error.

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

        let jev = oag_upstream::JevUpstream::new(base(
            Provider::Jev,
            oag_upstream::jev::DEFAULT_BASE_URL,
        )?);

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
            jev,
            catalog: Arc::new(RwLock::new(Arc::new(Catalog::new()))),
            system_one: Arc::new(RwLock::new(Arc::new(Catalog::new()))),
            readiness: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// The adapter for a provider, or an error naming the provider we lack.
    /// This state with `provider`'s adapter taken out, so a test can reach the
    /// "leased a credential, then found no adapter" arm now that every provider
    /// in the enum has one.
    #[cfg(test)]
    pub(crate) fn without_adapter(mut self, provider: Provider) -> Self {
        Arc::make_mut(&mut self.adapters).remove(&provider);
        self
    }

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

    /// The System One upstream, for a leased Jev credential.
    #[must_use]
    pub fn jev(&self) -> &oag_upstream::JevUpstream {
        &self.jev
    }

    /// Every provider this build can call: one per chat adapter, and Jev,
    /// which the System One route serves without one.
    #[must_use]
    pub fn providers(&self) -> Vec<Provider> {
        self.adapters
            .keys()
            .copied()
            .chain([Provider::Jev])
            .collect()
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

    /// A snapshot of the chat catalog. Cheap: one `Arc` clone.
    pub async fn catalog(&self) -> Arc<Catalog> {
        Arc::clone(&*self.catalog.read().await)
    }

    /// A snapshot of System One's models, for pricing an answer.
    pub async fn system_one_catalog(&self) -> Arc<Catalog> {
        Arc::clone(&*self.system_one.read().await)
    }

    /// Replace both catalogs wholesale, from one set of models.
    ///
    /// Split here, on the one way in, by whether the model's provider speaks a
    /// chat dialect. That is the whole of what keeps a chat request off a Jev
    /// credential: a request is leased a credential for the provider of the
    /// model routing picked, routing only ever picks from [`Self::catalog`],
    /// and a System One model is never in it — not by name, not by bare
    /// upstream name, not on a rung someone wrote into a ladder.
    pub async fn set_catalog(&self, specs: impl IntoIterator<Item = ModelSpec>) {
        let (chat, system_one): (Vec<_>, Vec<_>) = specs
            .into_iter()
            .partition(|spec| spec.provider.native_dialect().is_chat());
        *self.system_one.write().await = Arc::new(Catalog::from_entries(system_one));
        *self.catalog.write().await = Arc::new(Catalog::from_entries(chat));
    }

    /// Load the catalog from the database into memory.
    pub async fn reload_catalog(&self) -> Result<usize> {
        let rows = oag_store::repo::catalog(&self.db).await?;
        let specs: Vec<_> = rows
            .iter()
            .filter_map(oag_store::ModelRow::to_spec)
            .collect();
        let n = specs.len();
        self.set_catalog(specs).await;
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
        let build = |base: &str| {
            let config =
                crate::testing::config(&format!("gateway:\n  codex:\n    base_url: \"{base}\"\n"));
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
    /// refused is a provider whose base URL went through it.
    #[tokio::test]
    async fn every_configured_base_url_goes_through_the_normaliser() {
        let build = |provider: &str, url: &str| {
            let config = crate::testing::config(&format!(
                "gateway:\n  provider_base_urls:\n    {provider}: \"{url}\"\n"
            ));
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
            "jev",
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

    fn spec(id: &str, provider: oag_core::Provider, upstream: &str) -> oag_router::ModelSpec {
        oag_router::ModelSpec {
            id: oag_router::ModelId::new(id),
            provider,
            upstream_name: upstream.to_owned(),
            pricing: oag_router::Pricing {
                input_per_mtok: rust_decimal::Decimal::ONE,
                output_per_mtok: rust_decimal::Decimal::TWO,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
            context_window: 200_000,
            max_output_tokens: 8_192,
            capabilities: oag_router::Capabilities::default(),
            display_label: None,
        }
    }

    /// No chat request can lease a Jev credential, because no chat request can
    /// route to a Jev model: the chat catalog never holds one.
    ///
    /// A lease is for the provider of the model routing picked, and routing
    /// picks only from `catalog()` — by full id, by bare upstream name, or off
    /// a rung. So the split is asserted at all three spellings a chat request
    /// could reach it by, and the model is still there for System One to price.
    #[tokio::test]
    async fn a_system_one_model_never_reaches_the_chat_catalog() {
        use oag_core::Provider;
        let state = crate::testing::state("");
        state
            .set_catalog([
                spec(
                    "anthropic/claude-haiku-4.5",
                    Provider::Anthropic,
                    "claude-haiku-4-5",
                ),
                spec("jev/jev-latest", Provider::Jev, "jev-latest"),
            ])
            .await;

        let chat = state.catalog().await;
        assert!(chat.resolve("anthropic/claude-haiku-4.5").is_some());
        assert!(chat.resolve("jev/jev-latest").is_none(), "by id");
        assert!(chat.resolve("jev-latest").is_none(), "by upstream name");
        assert!(
            chat.iter().all(|m| m.provider != Provider::Jev),
            "off a rung: a ladder naming it finds nothing to pick"
        );

        let system_one = state.system_one_catalog().await;
        let jev = oag_router::ModelId::new("jev/jev-latest");
        assert_eq!(
            system_one.get(&jev).map(|m| m.pricing.output_per_mtok),
            Some(rust_decimal::Decimal::TWO),
            "System One still has it, prices and all"
        );
        assert_eq!(system_one.len(), 1, "and nothing that is not its own");
    }

    /// The real way in: rows from `model_catalog`, through `reload_catalog`,
    /// land in the catalog their provider's dialect says — and the count it
    /// returns is every row it placed.
    ///
    /// Gated, because the rows are Postgres's. Cleaned up before asserting,
    /// so a failure cannot leave a Jev model in a shared test database.
    #[tokio::test]
    async fn a_reload_puts_each_model_in_the_catalog_its_dialect_says() {
        use oag_core::Provider;
        let Ok(db_url) = std::env::var("OAG_TEST_DATABASE_URL") else {
            eprintln!("skipped: OAG_TEST_DATABASE_URL unset");
            return;
        };
        let config = oag_core::config::Config::from_yaml(&crate::testing::config_yaml(
            &db_url,
            "redis://127.0.0.1:1",
            "",
        ))
        .expect("test config");
        let db = oag_store::Db::connect(&config.database.url, 2).expect("pool");
        db.migrate().await.expect("migrate");
        let cache = oag_store::Cache::connect(&config.redis.url).expect("lazy client");
        let state = AppState::new(config, db.clone(), cache).expect("state");

        let tag = uuid::Uuid::new_v4().simple().to_string();
        let chat_id = format!("anthropic/reload-{tag}");
        let jev_id = format!("jev/reload-{tag}");
        for (id, provider) in [(&chat_id, Provider::Anthropic), (&jev_id, Provider::Jev)] {
            let row = oag_store::ModelRow {
                id: id.clone(),
                provider: provider.as_str().to_owned(),
                upstream_name: format!("reload-{tag}"),
                input_per_mtok: rust_decimal::Decimal::ONE,
                output_per_mtok: rust_decimal::Decimal::TWO,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
                context_window: 200_000,
                max_output_tokens: 8_192,
                supports_vision: false,
                supports_tools: false,
                supports_reasoning: false,
                supports_prompt_cache: false,
                display_label: None,
            };
            oag_store::repo::upsert_model(&db, &row, false)
                .await
                .expect("a catalog row");
        }

        let loaded = state.reload_catalog().await;
        sqlx::query("DELETE FROM model_catalog WHERE id = ANY($1)")
            .bind(vec![chat_id.clone(), jev_id.clone()])
            .execute(db.pool())
            .await
            .expect("clean up");
        let loaded = loaded.expect("reload");

        let chat = state.catalog().await;
        let system_one = state.system_one_catalog().await;
        assert!(chat.get(&oag_router::ModelId::new(&chat_id)).is_some());
        assert!(
            chat.get(&oag_router::ModelId::new(&jev_id)).is_none(),
            "a Jev model read from the database never reaches chat"
        );
        assert!(system_one.get(&oag_router::ModelId::new(&jev_id)).is_some());
        assert_eq!(
            loaded,
            chat.len() + system_one.len(),
            "every row counted went to one catalog or the other"
        );
    }

    /// Every provider in the build can be called. The admin matrix renders a
    /// provider missing from this as "no adapter in this build"; Jev has no
    /// chat adapter and is served all the same.
    #[tokio::test]
    async fn every_provider_is_reachable_including_the_one_with_no_chat_adapter() {
        let reachable = crate::testing::state("").providers();
        for provider in oag_core::Provider::ALL {
            assert!(
                reachable.contains(provider),
                "{provider} is in the enum and nothing calls it"
            );
        }
    }

    /// A stream ceiling may outlive the slot lease: the guard heartbeats.
    ///
    /// G8 used to refuse `max_stream_duration >= SLOT_TTL` at startup because a
    /// live request never refreshed its Redis score. That forced a 35-minute
    /// crash lease so a 30-minute stream would not oversubscribe. The lease is
    /// now two minutes; deleting the heartbeat would oversubscribe, and this
    /// test only proves the configuration that used to be illegal still boots.
    #[tokio::test]
    async fn a_stream_ceiling_may_outlive_the_slot_lease() {
        let config = crate::testing::config("");
        assert!(
            config.gateway.max_stream_duration > crate::gateway::select::SLOT_TTL,
            "the shipped ceiling is longer than the crash lease; heartbeats are \
             what make that safe"
        );
        let db = oag_store::Db::connect(&config.database.url, 1).expect("lazy pool");
        let cache = oag_store::Cache::connect(&config.redis.url).expect("lazy client");
        AppState::new(config, db, cache).expect("the shipped default must start");
    }
}
