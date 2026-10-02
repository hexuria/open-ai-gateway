//! Shared application state.

use crate::breakers::Breakers;
use crate::shutdown::Lifecycle;
use oag_core::config::Config;
use oag_core::endpoint::{Reason, Refusal, normalise_base_url};
use oag_core::provider::{Dialect, Endpoint, EndpointRegistry};
use oag_core::{Error, Kek, Provider, Result};
use oag_router::{Catalog, ModelSpec};
use oag_store::{AuthCache, Cache, Db, EndpointRow};
use oag_upstream::custom::EndpointSpec;
use oag_upstream::gcp_token::GcpTokenCache;
use oag_upstream::{JevUpstream, ProviderAdapter, TransportPool};
use std::collections::HashMap;
use std::sync::{Arc, PoisonError};
use std::time::Duration;
use tokio::sync::RwLock;

/// Every adapter a request can be served by, keyed by provider.
type AdapterMap = HashMap<Provider, Arc<dyn ProviderAdapter>>;

/// Every upstream a System One request can be served by, keyed by provider:
/// the built-in Jev and each System One endpoint the last reload loaded.
type SystemOneMap = HashMap<Provider, Arc<JevUpstream>>;

/// What serves one endpoint.
pub(crate) enum Served {
    /// A chat endpoint's adapter.
    Chat(Arc<dyn ProviderAdapter>),
    /// A System One endpoint's upstream, which only the System One route
    /// calls.
    SystemOne(Arc<JevUpstream>),
}

/// Reloads, one at a time in this process.
///
/// A reload writes two things that must agree: the process-wide endpoint
/// registry and this state's adapter map. Two reloads interleaved could leave
/// the registry holding one's endpoints and the map the other's until the next
/// reload, and an endpoint registered in the first and missing from the
/// second would then resolve to a provider with no adapter for a whole
/// interval. One at a time, a later reload reads later rows and writes all of
/// them after the earlier one is done. Process-wide because the registry is.
static RELOADS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    /// The built-in providers' adapters, built once from config and never
    /// rebuilt. Every map `adapters` holds is these plus the endpoints the
    /// last reload loaded.
    builtins: Arc<AdapterMap>,
    /// Swapped whole on every reload, like the catalog, and never edited in
    /// place. A lookup clones the adapter it finds, and whoever holds the
    /// clone keeps it whatever a reload does. A request looks its adapter up
    /// once, to build and send, and its answer is read by that same clone,
    /// which the attempt carries (`gateway::failover::Attempt`): an endpoint
    /// whose settings changed, or that was removed, while its upstream was
    /// answering still has that answer read in the dialect and framing it was
    /// asked in, and metered.
    ///
    /// A std lock: it is held only to clone the `Arc` inside, never across an
    /// `.await`. See [`AppState::apply_endpoints`] for the order the swaps
    /// happen in.
    adapters: Arc<std::sync::RwLock<Arc<AdapterMap>>>,
    /// The Codex/`ChatGPT` subscription adapter. Held apart from `adapters`
    /// because it shares OpenAI's provider key but not its dialect — it is
    /// selected per-account for an OpenAI OAuth seat, in the gateway.
    codex: Arc<dyn ProviderAdapter>,
    /// The built-in Jev's upstream, built once from config and never rebuilt.
    /// Every map `system_one` holds has it.
    jev: Arc<JevUpstream>,
    /// The System One upstreams: outside `adapters`, because none of them is
    /// a chat adapter at all. Swapped with `adapters` on every reload, by the
    /// same three steps; see [`AppState::apply_endpoints`].
    system_one_upstreams: Arc<std::sync::RwLock<Arc<SystemOneMap>>>,
    /// The Google access tokens every `gcp` endpoint's credentials are sent
    /// with, minted at `gateway.gcp_token_url` and kept per credential until
    /// five minutes before they expire.
    ///
    /// One for the state's life, handed to each gcp adapter a reload builds.
    /// Every reload rebuilds every endpoint's adapter, so a cache an adapter
    /// owned would be thrown away with it, and each account's token minted
    /// again after every reload. Per replica: each mints its own, which costs
    /// nothing, unlike an OAuth refresh.
    pub(crate) gcp_tokens: Arc<GcpTokenCache>,
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
            .field("providers", &self.providers())
            .finish_non_exhaustive()
    }
}

/// The endpoints a reload will serve: an adapter, or for a System One endpoint
/// the upstream the System One route calls, for each row that passes every
/// rule and that this build can serve.
///
/// Every other row is skipped, and says why in the log and in
/// `oag_endpoint_invalid_total`, on every reload for as long as it stays that
/// way. Its credentials and models then serve nothing: the provider name they
/// carry parses to nothing, as an unknown provider's always has.
///
/// `gcp_tokens` is what every `gcp` endpoint's adapter mints through: the
/// state's one cache, which outlives the adapters a reload replaces.
fn load_endpoints(
    rows: &[EndpointRow],
    gcp_tokens: &Arc<GcpTokenCache>,
) -> Vec<(Endpoint, Served)> {
    let mut served = Vec::with_capacity(rows.len());
    for row in rows {
        match endpoint_upstream(row, gcp_tokens) {
            Ok(loaded) => served.push(loaded),
            Err(refusal) => {
                metrics::counter!(
                    "oag_endpoint_invalid_total",
                    "reason" => refusal.reason.as_str(),
                )
                .increment(1);
                // The refusal names the rule and at most the value that broke
                // it, never an extra header's value.
                tracing::warn!(
                    endpoint = %row.name,
                    reason = refusal.reason.as_str(),
                    problem = %refusal,
                    "endpoint not served; its credentials and models serve nothing until the row is fixed"
                );
            }
        }
    }
    served
}

/// What serves one endpoint row, or why nothing does.
///
/// A System One row gets its upstream from `custom::system_one`, at the path it
/// names or Jev's own, and every other row its dialect's adapter from
/// `custom::adapter`. Both come from the one factory, so a row is judged by the
/// same rules whichever it is.
pub(crate) fn endpoint_upstream(
    row: &EndpointRow,
    gcp_tokens: &Arc<GcpTokenCache>,
) -> std::result::Result<(Endpoint, Served), Refusal> {
    let config = row.to_endpoint()?;
    // Only aws and gcp may have no base URL, and the adapter for either reads
    // the empty string as its region's own host, so the empty string is never
    // sent. The region is the row's: `gateway.bedrock_region` is the built-in
    // provider's alone. Every spec is handed the token cache, and only a gcp
    // endpoint's adapter reads it.
    let spec = EndpointSpec::new(
        config.endpoint,
        config.base_url.unwrap_or_default(),
        config.auth,
        config.extra_headers,
    )
    .map_err(|e| Refusal::new(Reason::Headers, e))?
    .with_discovery(row.discover_models)
    .with_region(config.region)
    .with_project(config.project)
    .with_api_version(config.api_version)
    .with_gcp_tokens(Arc::clone(gcp_tokens));
    let served = if config.endpoint.dialect() == Dialect::SystemOne {
        Served::SystemOne(Arc::new(
            oag_upstream::custom::system_one(&spec, config.path.as_deref()).map_err(unsupported)?,
        ))
    } else {
        Served::Chat(oag_upstream::custom::adapter(&spec).map_err(unsupported)?)
    };
    Ok((config.endpoint, served))
}

/// The factory's refusal of a row, as the reason it is not served: the words,
/// not the `Display`, whose `configuration: ` would lead every reason `oag
/// admin endpoint list` and the console print.
fn unsupported(e: Error) -> Refusal {
    let message = match e {
        Error::Config(message) => message,
        other => other.to_string(),
    };
    Refusal::new(Reason::Unsupported, message)
}

/// The System One map before any endpoint: the built-in Jev alone.
fn only_jev(jev: &Arc<JevUpstream>) -> SystemOneMap {
    SystemOneMap::from([(Provider::Jev, Arc::clone(jev))])
}

/// Every entry in `next`, and every one in `current` that `next` has no entry
/// for: what a map holds while the registry is being replaced.
fn bridge<V: Clone>(
    next: &HashMap<Provider, V>,
    current: &HashMap<Provider, V>,
) -> HashMap<Provider, V> {
    let mut both = next.clone();
    for (provider, upstream) in current {
        both.entry(*provider).or_insert_with(|| upstream.clone());
    }
    both
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

        let jev = Arc::new(JevUpstream::new(base(
            Provider::Jev,
            oag_upstream::jev::DEFAULT_BASE_URL,
        )?));
        // No endpoint yet: the first reload loads them.
        let builtins = Arc::new(adapters);

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
            // Here, so a token URL that is not one stops the gateway at startup.
            gcp_tokens: Arc::new(GcpTokenCache::new(config.gateway.gcp_token_url.clone())?),
            config: Arc::new(config),
            db,
            cache,
            lifecycle: Arc::new(Lifecycle::new()),
            kek: Arc::new(kek),
            breakers: Arc::new(Breakers::new()),
            refresh_gates: Arc::new(std::sync::Mutex::new(HashMap::new())),
            adapters: Arc::new(std::sync::RwLock::new(Arc::clone(&builtins))),
            builtins,
            codex,
            system_one_upstreams: Arc::new(std::sync::RwLock::new(Arc::new(only_jev(&jev)))),
            jev,
            catalog: Arc::new(RwLock::new(Arc::new(Catalog::new()))),
            system_one: Arc::new(RwLock::new(Arc::new(Catalog::new()))),
            readiness: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    /// The adapter for a provider, or an error naming the provider we lack.
    pub fn adapter(&self, provider: Provider) -> Result<Arc<dyn ProviderAdapter>> {
        self.adapter_map()
            .get(&provider)
            .cloned()
            .ok_or_else(|| Error::Internal(format!("no adapter for provider {provider}")))
    }

    /// The adapter map as it stands. Cheap: one `Arc` clone.
    fn adapter_map(&self) -> Arc<AdapterMap> {
        Arc::clone(&self.adapters.read().unwrap_or_else(PoisonError::into_inner))
    }

    fn set_adapters(&self, map: Arc<AdapterMap>) {
        *self
            .adapters
            .write()
            .unwrap_or_else(PoisonError::into_inner) = map;
    }

    /// The System One map as it stands. Cheap: one `Arc` clone.
    fn system_one_map(&self) -> Arc<SystemOneMap> {
        Arc::clone(
            &self
                .system_one_upstreams
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    fn set_system_one(&self, map: Arc<SystemOneMap>) {
        *self
            .system_one_upstreams
            .write()
            .unwrap_or_else(PoisonError::into_inner) = map;
    }

    /// Make `served` the endpoints this state serves and `registry` resolves.
    ///
    /// Three swaps, in an order that leaves no instant at which `registry`
    /// resolves an endpoint that has nothing to serve it here:
    ///
    /// 1. both maps, the adapters' and the System One upstreams', gain this
    ///    reload's entries and keep every one they had;
    /// 2. the registry is replaced with this reload's endpoints;
    /// 3. both maps drop what the registry no longer names.
    ///
    /// So a new endpoint's adapter or upstream is in place before its name
    /// parses, and a removed one's name stops parsing before its adapter or
    /// upstream goes. Parsing is what a request needs to reach an endpoint at
    /// all: a catalog row naming it becomes a model only through
    /// [`Provider`]'s `FromStr`, and so does a credential filed under it. An
    /// endpoint whose settings changed gets its new adapter or upstream in the
    /// first swap, and a request the old one already built goes where it was
    /// built to go.
    ///
    /// The built-in adapters and the built-in Jev are carried into every map
    /// as they are.
    pub(crate) fn apply_endpoints(
        &self,
        registry: &EndpointRegistry,
        served: Vec<(Endpoint, Served)>,
    ) {
        let mut next = (*self.builtins).clone();
        let mut system_one = only_jev(&self.jev);
        let mut endpoints = Vec::with_capacity(served.len());
        for (endpoint, served) in served {
            let provider = Provider::Custom(endpoint);
            match served {
                Served::Chat(adapter) => {
                    next.insert(provider, adapter);
                }
                Served::SystemOne(upstream) => {
                    system_one.insert(provider, upstream);
                }
            }
            endpoints.push(endpoint);
        }
        let (next, system_one) = (Arc::new(next), Arc::new(system_one));
        self.set_adapters(Arc::new(bridge(&next, &self.adapter_map())));
        self.set_system_one(Arc::new(bridge(&system_one, &self.system_one_map())));
        registry.install(endpoints);
        self.set_adapters(next);
        self.set_system_one(system_one);
    }

    /// The Codex adapter, for an OpenAI subscription seat. Selected in the
    /// gateway when the leased account is an OpenAI OAuth credential.
    #[must_use]
    pub fn codex_adapter(&self) -> Arc<dyn ProviderAdapter> {
        Arc::clone(&self.codex)
    }

    /// The System One upstream for a leased credential of `provider`: the
    /// built-in Jev, or a System One endpoint the last reload loaded.
    ///
    /// A lookup clones the upstream it finds, as [`AppState::adapter`] does,
    /// and whoever holds the clone keeps it whatever a reload does.
    pub fn system_one(&self, provider: Provider) -> Result<Arc<JevUpstream>> {
        self.system_one_map()
            .get(&provider)
            .cloned()
            .ok_or_else(|| {
                Error::Internal(format!("no System One upstream for provider {provider}"))
            })
    }

    /// Every provider the System One route can call, the built-in Jev first
    /// and then each System One endpoint by name.
    #[must_use]
    pub fn system_one_providers(&self) -> Vec<Provider> {
        let mut providers: Vec<Provider> = self.system_one_map().keys().copied().collect();
        providers.sort_unstable();
        providers
    }

    /// Every provider this gateway can call: one per chat adapter, which
    /// includes each chat endpoint the last reload loaded, and one per System
    /// One upstream, which is Jev and each System One endpoint.
    #[must_use]
    pub fn providers(&self) -> Vec<Provider> {
        self.adapter_map()
            .keys()
            .chain(self.system_one_map().keys())
            .copied()
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

    /// A snapshot of System One's models: the built-in Jev's and every System
    /// One endpoint's, for resolving the model a request names to the
    /// provider that answers it, and for pricing the answer.
    pub async fn system_one_catalog(&self) -> Arc<Catalog> {
        Arc::clone(&*self.system_one.read().await)
    }

    /// Replace both catalogs wholesale, from one set of models.
    ///
    /// Split here, on the one way in, by whether the model's provider speaks a
    /// chat dialect. That is the whole of what keeps a chat request off a
    /// System One credential, Jev's or an endpoint's: a request is leased a
    /// credential for the provider of the model routing picked, routing only
    /// ever picks from [`Self::catalog`], and a System One model is never in
    /// it — not by name, not by bare upstream name, not on a rung someone
    /// wrote into a ladder.
    pub async fn set_catalog(&self, specs: impl IntoIterator<Item = ModelSpec>) {
        let (chat, system_one): (Vec<_>, Vec<_>) = specs
            .into_iter()
            .partition(|spec| spec.provider.native_dialect().is_chat());
        *self.system_one.write().await = Arc::new(Catalog::from_entries(system_one));
        *self.catalog.write().await = Arc::new(Catalog::from_entries(chat));
    }

    /// Load the endpoints, then the catalog, from the database into memory.
    ///
    /// Endpoints first, because a catalog row names its provider and a row
    /// naming an endpoint nobody has registered parses as nothing and is
    /// dropped. Loaded first, a model added with its endpoint is served on the
    /// same reload rather than the one after. The returned count is the
    /// catalog's.
    ///
    /// An endpoint table that cannot be read keeps the endpoints already
    /// loaded, and the catalog loads anyway: the built-in providers must never
    /// stop routing over a table only endpoints use.
    pub async fn reload_catalog(&self) -> Result<usize> {
        let _one_at_a_time = RELOADS.lock().await;
        match oag_store::repo::list_endpoints(&self.db).await {
            Ok(rows) => self.apply_endpoints(
                EndpointRegistry::global(),
                load_endpoints(&rows, &self.gcp_tokens),
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "could not read the endpoints; keeping the ones already loaded"
            ),
        }
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
    use super::{AppState, bridge, endpoint_upstream, load_endpoints, unsupported};
    use oag_core::endpoint::Reason;
    use oag_core::provider::{Dialect, Endpoint, EndpointRegistry, Platform};
    use oag_core::{Error, Provider};
    use oag_upstream::ProviderAdapter;
    use std::sync::Arc;

    /// A plain endpoint row. Nothing is ever sent to its base URL.
    fn endpoint_row(name: &str, dialect: &str, base_url: &str) -> oag_store::EndpointRow {
        oag_store::EndpointRow {
            name: name.to_owned(),
            dialect: dialect.to_owned(),
            platform: "plain".to_owned(),
            base_url: Some(base_url.to_owned()),
            auth: "bearer".to_owned(),
            region: None,
            project: None,
            api_version: None,
            path: None,
            extra_headers: serde_json::json!({}),
            display_name: None,
            discover_models: false,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// An aws endpoint row at a stand-in host. Nothing is ever sent there.
    fn aws_row(name: &str, dialect: &str, region: &str) -> oag_store::EndpointRow {
        let mut row = endpoint_row(name, dialect, "http://127.0.0.1:9");
        row.platform = "aws".to_owned();
        row.auth = "none".to_owned();
        row.region = Some(region.to_owned());
        row
    }

    /// A gcp endpoint row in project `oag-test`, at its region's own host or
    /// at `base_url`.
    fn gcp_row(
        name: &str,
        dialect: &str,
        region: &str,
        base_url: Option<&str>,
    ) -> oag_store::EndpointRow {
        let mut row = endpoint_row(name, dialect, "http://127.0.0.1:9");
        row.platform = "gcp".to_owned();
        row.base_url = base_url.map(str::to_owned);
        row.region = Some(region.to_owned());
        row.project = Some("oag-test".to_owned());
        row
    }

    /// A token cache for a test with no state, at a port nothing listens on.
    fn tokens() -> Arc<oag_upstream::gcp_token::GcpTokenCache> {
        Arc::new(
            oag_upstream::gcp_token::GcpTokenCache::new("http://127.0.0.1:1/token").expect("a URL"),
        )
    }

    /// The provider an endpoint row of this name is served as. An endpoint is
    /// its name, so the dialect here does not have to be the row's.
    fn custom(name: &str) -> Provider {
        Provider::Custom(
            Endpoint::new(name, Dialect::OpenAIChatCompletions, Platform::Plain).expect("a name"),
        )
    }

    /// Every built-in with a chat adapter: all but Jev.
    fn chat_builtins() -> impl Iterator<Item = Provider> {
        Provider::ALL
            .iter()
            .copied()
            .filter(|p| *p != Provider::Jev)
    }

    /// Where an adapter sends a request: the URL it builds.
    fn target(adapter: &Arc<dyn ProviderAdapter>) -> String {
        let canonical = oag_proto::CanonicalRequest {
            model: "m".to_owned(),
            system: vec![],
            messages: vec![oag_proto::Message {
                role: oag_proto::Role::User,
                content: vec![oag_proto::ContentBlock::Text {
                    text: "hi".to_owned(),
                    cache_control: None,
                }],
            }],
            tools: vec![],
            max_tokens: 16,
            stream: false,
            temperature: None,
            thinking_budget: None,
            thinking_effort: None,
            client_session: None,
            tool_choice: None,
            response_format: None,
            stop: Vec::new(),
            previous_response_id: None,
            passthrough: None,
        };
        let model = spec("t4/m", adapter.provider(), "m");
        let credential = oag_core::credential::SecretMaterial {
            access_token: "t4-key".to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        };
        adapter
            .build(&oag_upstream::UpstreamRequest {
                canonical: &canonical,
                model: &model,
                credential: &credential,
                session: None,
            })
            .expect("builds")
            .url()
            .to_string()
    }

    /// An endpoint row is served from the reload that loads it until the
    /// reload that no longer finds it, and the built-ins never move.
    #[tokio::test]
    async fn an_endpoint_is_served_while_its_row_exists_and_not_after() {
        let state = crate::testing::state("");
        // A registry of its own: the global one is the whole process's.
        let registry = EndpointRegistry::default();
        let groq = custom("t4-state-groq");

        state.apply_endpoints(
            &registry,
            load_endpoints(
                &[endpoint_row(
                    "t4-state-groq",
                    "anthropic",
                    "http://127.0.0.1:9/",
                )],
                &state.gcp_tokens,
            ),
        );
        let adapter = state.adapter(groq).expect("the endpoint has an adapter");
        assert_eq!(adapter.provider(), groq);
        assert_eq!(adapter.dialect(), Dialect::AnthropicMessages);
        assert_eq!(target(&adapter), "http://127.0.0.1:9/v1/messages");
        assert_eq!(
            registry.get("t4-state-groq").map(Endpoint::dialect),
            Some(Dialect::AnthropicMessages),
            "and its name resolves"
        );
        assert!(state.providers().contains(&groq), "and it is listed");

        state.apply_endpoints(&registry, load_endpoints(&[], &state.gcp_tokens));
        assert!(
            state.adapter(groq).is_err(),
            "its adapter went with its row"
        );
        assert_eq!(registry.get("t4-state-groq"), None, "and so did its name");
        assert!(!state.providers().contains(&groq));
        for provider in chat_builtins() {
            state
                .adapter(provider)
                .expect("a built-in is never dropped");
        }
    }

    /// What an adapter builds for one request with a packed AWS credential:
    /// where it goes, and the scope its `SigV4` signature names.
    fn signed(adapter: &Arc<dyn ProviderAdapter>) -> (String, String) {
        let canonical = oag_proto::openai::parse_request(&serde_json::json!({
            "model": "m", "messages": [{ "role": "user", "content": "hi" }],
        }))
        .expect("parses");
        let model = spec("t9/m", adapter.provider(), "m");
        let credential = oag_core::credential::SecretMaterial {
            access_token: "TESTACCESSKEY:TESTSECRETKEY".to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        };
        let built = adapter
            .build(&oag_upstream::UpstreamRequest {
                canonical: &canonical,
                model: &model,
                credential: &credential,
                session: None,
            })
            .expect("builds");
        let authorization = built.headers()["authorization"]
            .to_str()
            .expect("ASCII")
            .to_owned();
        let scope = authorization
            .split('/')
            .skip(2)
            .take(2)
            .collect::<Vec<_>>()
            .join("/");
        (built.url().to_string(), scope)
    }

    /// An aws row is Bedrock in the row's own region, whatever region the
    /// config names for the built-in provider: Claude through `InvokeModel`,
    /// every other model through `Converse`, each sent to its region's host or
    /// the row's base URL and signed for its region.
    #[tokio::test]
    async fn an_aws_endpoint_is_served_in_its_own_region_and_not_the_gateways() {
        let state = crate::testing::state("gateway:\n  bedrock_region: \"sa-east-1\"\n");
        let registry = EndpointRegistry::default();
        let mut claude = aws_row("t9-state-claude", "anthropic", "eu-west-3");
        claude.base_url = None;
        let converse = aws_row("t9-state-converse", "bedrock_converse", "ap-northeast-2");

        state.apply_endpoints(
            &registry,
            load_endpoints(&[claude, converse], &state.gcp_tokens),
        );
        for (name, dialect, url, scope) in [
            (
                "t9-state-claude",
                Dialect::AnthropicMessages,
                "https://bedrock-runtime.eu-west-3.amazonaws.com/model/m/invoke",
                "eu-west-3/bedrock",
            ),
            (
                "t9-state-converse",
                Dialect::BedrockConverse,
                "http://127.0.0.1:9/model/m/converse",
                "ap-northeast-2/bedrock",
            ),
        ] {
            let adapter = state
                .adapter(custom(name))
                .expect("an aws endpoint is served");
            assert_eq!(adapter.provider(), custom(name));
            assert_eq!(adapter.dialect(), dialect, "{name}");
            assert_eq!(
                signed(&adapter),
                (url.to_owned(), scope.to_owned()),
                "{name}"
            );
            assert_eq!(
                registry.get(name).map(Endpoint::dialect),
                Some(dialect),
                "{name}"
            );
        }
        let (_, builtin) = signed(&state.adapter(Provider::Bedrock).expect("built-in"));
        assert_eq!(
            builtin, "sa-east-1/bedrock",
            "the built-in keeps the region configured for it"
        );
    }

    /// A gcp row is Vertex in the row's own project and region, Gemini or
    /// Claude. Every adapter a reload builds for it mints through the state's
    /// one cache, at `gateway.gcp_token_url`, so a token minted before a
    /// reload is the one sent after it.
    #[tokio::test]
    async fn a_gcp_endpoint_is_served_and_its_tokens_outlive_a_reload() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let google = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.t10-state",
                "expires_in": 3600,
                "token_type": "Bearer",
            })))
            .expect(1)
            .mount(&google)
            .await;
        let state = crate::testing::state(&format!(
            "gateway:\n  gcp_token_url: \"{}/token\"\n",
            google.uri()
        ));
        let registry = EndpointRegistry::default();
        let rows = [
            gcp_row("t10-state-gemini", "gemini", "us-central1", None),
            gcp_row(
                "t10-state-claude",
                "anthropic",
                "global",
                Some("http://127.0.0.1:9"),
            ),
        ];

        state.apply_endpoints(&registry, load_endpoints(&rows, &state.gcp_tokens));
        for (name, dialect, url) in [
            (
                "t10-state-gemini",
                Dialect::GeminiGenerateContent,
                "https://us-central1-aiplatform.googleapis.com/v1/projects/oag-test/\
                 locations/us-central1/publishers/google/models/m:generateContent",
            ),
            (
                "t10-state-claude",
                Dialect::AnthropicMessages,
                "http://127.0.0.1:9/v1/projects/oag-test/\
                 locations/global/publishers/anthropic/models/m:rawPredict",
            ),
        ] {
            let adapter = state
                .adapter(custom(name))
                .expect("a gcp endpoint is served");
            assert_eq!(adapter.provider(), custom(name));
            assert_eq!(adapter.dialect(), dialect, "{name}");
            assert_eq!(target(&adapter), url, "{name}");
            assert_eq!(
                registry.get(name).map(Endpoint::dialect),
                Some(dialect),
                "{name}"
            );
        }

        let account = oag_core::AccountId::new();
        let stored = oag_core::credential::SecretMaterial {
            access_token: serde_json::json!({
                "type": "service_account",
                "private_key_id": "t10",
                "private_key": oag_upstream::gcp_token::TEST_KEY_PEM,
                "client_email": "t10-state@oag-test.invalid",
            })
            .to_string(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        };
        let before = state.adapter(custom("t10-state-gemini")).expect("served");
        let minted = before
            .prepare_credential(account, &stored, None)
            .await
            .expect("minted")
            .access_token
            .clone();
        assert_eq!(minted, "ya29.t10-state");

        state.apply_endpoints(&registry, load_endpoints(&rows, &state.gcp_tokens));
        let after = state
            .adapter(custom("t10-state-gemini"))
            .expect("still served");
        assert!(!Arc::ptr_eq(&before, &after), "rebuilt on reload");
        let again = after
            .prepare_credential(account, &stored, None)
            .await
            .expect("cached");
        assert_eq!(again.access_token, minted, "and the token outlived it");
        google.verify().await;
    }

    /// A System One row is served too, by the System One route rather than a
    /// chat adapter: an upstream at the path it names, beside the built-in
    /// Jev, from the reload that loads it until the one that no longer finds
    /// it.
    #[tokio::test]
    async fn a_system_one_endpoint_is_served_by_the_system_one_route_while_its_row_exists() {
        let state = crate::testing::state("");
        let registry = EndpointRegistry::default();
        let decisions = custom("t7-state-decisions");
        let mut row = endpoint_row("t7-state-decisions", "system_one", "http://127.0.0.1:9/");
        row.path = Some("/v1/decisions".to_owned());
        let jev_shaped = custom("t7-state-jev");

        state.apply_endpoints(
            &registry,
            load_endpoints(
                &[
                    row,
                    endpoint_row("t7-state-jev", "system_one", "http://127.0.0.1:9/jev"),
                ],
                &state.gcp_tokens,
            ),
        );
        let credential = oag_core::credential::SecretMaterial {
            access_token: "t7-key".to_owned(),
            refresh_token: None,
            expires_at: None,
            version: 0,
            client_id: None,
            account_id: None,
        };
        let asked = |provider: Provider| {
            state
                .system_one(provider)
                .expect("a System One upstream")
                .system_one(&credential, "{}")
                .expect("builds")
                .url()
                .to_string()
        };
        assert_eq!(asked(decisions), "http://127.0.0.1:9/v1/decisions");
        assert_eq!(
            asked(jev_shaped),
            "http://127.0.0.1:9/jev/v1/systemone",
            "no path is Jev's own"
        );
        assert!(
            state.adapter(decisions).is_err(),
            "and no chat adapter: no chat request can be built for it"
        );
        assert_eq!(
            registry.get("t7-state-decisions").map(Endpoint::dialect),
            Some(Dialect::SystemOne),
            "its name resolves"
        );
        assert_eq!(
            state.system_one_providers(),
            [Provider::Jev, decisions, jev_shaped],
            "Jev first, then each endpoint by name"
        );
        assert!(state.providers().contains(&decisions), "and it is listed");

        state.apply_endpoints(&registry, load_endpoints(&[], &state.gcp_tokens));
        assert!(state.system_one(decisions).is_err(), "gone with its row");
        assert_eq!(registry.get("t7-state-decisions"), None);
        assert_eq!(state.system_one_providers(), [Provider::Jev]);
        assert!(!state.providers().contains(&decisions));
        assert!(
            state
                .system_one(Provider::Jev)
                .is_ok_and(|jev| Arc::ptr_eq(&jev, &state.jev)),
            "the built-in Jev is never rebuilt or dropped"
        );
    }

    /// An azure endpoint row: Azure's v1 API, or its deployments API at
    /// `api_version`. Nothing is ever sent to its base URL.
    fn azure_row(name: &str, base_url: &str, api_version: Option<&str>) -> oag_store::EndpointRow {
        let mut row = endpoint_row(name, "openai", base_url);
        row.platform = "azure".to_owned();
        row.auth = "api_key_header".to_owned();
        row.api_version = api_version.map(str::to_owned);
        row
    }

    /// An azure row is served at its resource, in the API its row names: the
    /// v1 API with no API version, and the deployments API at the row's
    /// version with one, the deployment in the path. The version comes from
    /// the row, so a reload that left it behind would post a deployments row
    /// to the v1 API.
    #[tokio::test]
    async fn an_azure_endpoint_is_served_at_its_resource_in_the_api_its_row_names() {
        let state = crate::testing::state("");
        let registry = EndpointRegistry::default();
        state.apply_endpoints(
            &registry,
            load_endpoints(
                &[
                    azure_row("t8-state-v1", "https://res.openai.azure.com/", None),
                    azure_row(
                        "t8-state-deployments",
                        "https://RES.services.ai.azure.com",
                        Some("2024-10-21"),
                    ),
                ],
                &state.gcp_tokens,
            ),
        );
        for (name, url) in [
            (
                "t8-state-v1",
                "https://res.openai.azure.com/openai/v1/chat/completions",
            ),
            (
                "t8-state-deployments",
                "https://res.services.ai.azure.com/openai/deployments/m/chat/completions\
                 ?api-version=2024-10-21",
            ),
        ] {
            let adapter = state
                .adapter(custom(name))
                .expect("an azure endpoint is served");
            assert_eq!(adapter.provider(), custom(name));
            assert_eq!(adapter.dialect(), Dialect::OpenAIChatCompletions, "{name}");
            assert_eq!(target(&adapter), url, "{name}");
            assert_eq!(
                registry.get(name).map(Endpoint::platform),
                Some(Platform::Azure),
                "{name}"
            );
        }

        state.apply_endpoints(&registry, load_endpoints(&[], &state.gcp_tokens));
        assert!(
            state.adapter(custom("t8-state-v1")).is_err(),
            "gone with its row"
        );
    }

    /// Every row that breaks a rule, beside one that does not.
    fn bad_rows() -> Vec<(oag_store::EndpointRow, Reason)> {
        let mut region = endpoint_row("t4-bad-region", "openai", "http://127.0.0.1:9/v1");
        region.region = Some("US_EAST_1".to_owned());
        // A region Google would name, on Bedrock.
        let mut converse = aws_row("t9-bad-converse", "bedrock_converse", "us-central1");
        converse.base_url = None;
        let mut azure = endpoint_row("t4-bad-azure", "openai", "https://res.openai.azure.com");
        azure.platform = "azure".to_owned();
        azure.auth = "api_key_header".to_owned();
        // Azure's v1 API is the one a row without an API version gets.
        azure.api_version = Some("v1".to_owned());
        // A model server of the operator's own, which an azure row may not name.
        let azure_host = azure_row("t8-bad-azure-host", "https://10.0.0.7", None);
        let mut header = endpoint_row("t4-bad-header", "openai", "http://127.0.0.1:9/v1");
        header.extra_headers = serde_json::json!({"Authorization": "Bearer not-here"});
        let mut jev_header = endpoint_row("t7-bad-jev-header", "system_one", "http://127.0.0.1:9");
        jev_header.extra_headers = serde_json::json!({"X-API-Key": "not-here"});
        let mut jev_path = endpoint_row("t7-bad-jev-path", "system_one", "http://127.0.0.1:9");
        jev_path.path = Some("/v1/./decisions".to_owned());
        let mut chat_path = endpoint_row("t7-bad-chat-path", "openai", "http://127.0.0.1:9/v1");
        chat_path.path = Some("/v1/decisions".to_owned());
        vec![
            (region, Reason::Region),
            (
                endpoint_row(
                    "t4-bad-metadata",
                    "openai",
                    "http://169.254.169.254/latest/v1",
                ),
                Reason::BaseUrl,
            ),
            (
                endpoint_row("t4-bad-openai", "openai", "https://api.openai.com/v1"),
                Reason::Compliance,
            ),
            (
                endpoint_row("t4-bad-dialect", "klingon", "http://127.0.0.1:9"),
                Reason::Dialect,
            ),
            (converse, Reason::Region),
            (
                endpoint_row("anthropic", "anthropic", "http://127.0.0.1:9"),
                Reason::Name,
            ),
            (header, Reason::Headers),
            // A System One row is held to the same header rules as a chat one.
            (jev_header, Reason::Headers),
            (jev_path, Reason::Path),
            (chat_path, Reason::Path),
            (azure, Reason::ApiVersion),
            (azure_host, Reason::BaseUrl),
        ]
    }

    /// Every platform is served, so no row that passes the rules reaches a
    /// factory refusal; what one would say is pinned here instead: its words,
    /// with no `configuration: ` before them.
    #[test]
    fn a_factory_refusal_reads_as_its_words() {
        let words = unsupported(Error::Config(
            "endpoint `t5-x` is on the y platform".to_owned(),
        ));
        assert_eq!(words.reason, Reason::Unsupported);
        assert_eq!(words.message, "endpoint `t5-x` is on the y platform");
        let other = unsupported(Error::Internal("down".to_owned()));
        assert_eq!(
            other.message,
            Error::Internal("down".to_owned()).to_string()
        );
    }

    #[test]
    fn each_bad_row_is_refused_for_its_own_reason() {
        for (row, reason) in bad_rows() {
            let refused = endpoint_upstream(&row, &tokens())
                .map(|_| ())
                .expect_err(&row.name);
            assert_eq!(refused.reason, reason, "{}: {refused}", row.name);
            assert!(
                !refused.message.contains("not-here"),
                "a header value is never quoted: {refused}"
            );
        }
    }

    #[tokio::test]
    async fn a_bad_row_is_skipped_and_everything_else_still_serves() {
        let state = crate::testing::state("");
        let registry = EndpointRegistry::default();
        let (mut rows, _): (Vec<_>, Vec<_>) = bad_rows().into_iter().unzip();
        rows.push(endpoint_row(
            "t4-good",
            "gemini",
            "http://127.0.0.1:9/v1beta",
        ));

        let served = load_endpoints(&rows, &state.gcp_tokens);
        assert_eq!(
            served.iter().map(|(e, _)| e.name()).collect::<Vec<_>>(),
            ["t4-good"],
            "only the good row is served"
        );
        state.apply_endpoints(&registry, served);

        assert!(state.adapter(custom("t4-good")).is_ok());
        assert!(registry.get("t4-good").is_some());
        for row in &rows[..rows.len() - 1] {
            assert_eq!(registry.get(&row.name), None, "{} registered", row.name);
            if Endpoint::validate_name(&row.name).is_ok() {
                assert!(
                    state.adapter(custom(&row.name)).is_err(),
                    "{} has an adapter",
                    row.name
                );
            }
        }
        for provider in chat_builtins() {
            state.adapter(provider).expect("the built-ins still serve");
        }
        assert!(
            state
                .adapter(Provider::Anthropic)
                .is_ok_and(|a| a.provider() == Provider::Anthropic),
            "an endpoint row spelt like a built-in changes nothing about it"
        );
    }

    /// A reload that changes an endpoint's settings gives new requests a new
    /// adapter, and leaves the old one with whoever already holds it.
    #[tokio::test]
    async fn a_changed_endpoint_gets_a_new_adapter_and_a_request_keeps_its_own() {
        let state = crate::testing::state("");
        let registry = EndpointRegistry::default();
        let moved = custom("t4-state-moved");

        state.apply_endpoints(
            &registry,
            load_endpoints(
                &[endpoint_row(
                    "t4-state-moved",
                    "openai",
                    "http://127.0.0.1:9/old/v1",
                )],
                &state.gcp_tokens,
            ),
        );
        let in_flight = state.adapter(moved).expect("served");

        state.apply_endpoints(
            &registry,
            load_endpoints(
                &[endpoint_row(
                    "t4-state-moved",
                    "openai",
                    "http://127.0.0.1:9/new/v1",
                )],
                &state.gcp_tokens,
            ),
        );
        let next = state.adapter(moved).expect("still served");
        assert!(!Arc::ptr_eq(&in_flight, &next), "rebuilt on reload");
        assert_eq!(target(&next), "http://127.0.0.1:9/new/v1/chat/completions");
        assert_eq!(
            target(&in_flight),
            "http://127.0.0.1:9/old/v1/chat/completions",
            "the request that held the old adapter still has it"
        );
        let anthropic = state.adapter(Provider::Anthropic).expect("built-in");
        state.apply_endpoints(&registry, load_endpoints(&[], &state.gcp_tokens));
        assert!(
            Arc::ptr_eq(
                &anthropic,
                &state.adapter(Provider::Anthropic).expect("built-in")
            ),
            "a built-in adapter is never rebuilt"
        );
    }

    /// While the registry is replaced, the map holds the old set and the new,
    /// with the new adapter wherever both name one.
    #[test]
    fn the_bridge_holds_both_sets_and_prefers_the_new_adapter() {
        let adapter = |provider: Provider| -> Arc<dyn ProviderAdapter> {
            Arc::new(oag_upstream::OpenAICompatAdapter::new(
                provider,
                "http://127.0.0.1:9".to_owned(),
            ))
        };
        let (kept, changed, added, removed) = (
            Provider::OpenAI,
            custom("t4-bridge-changed"),
            custom("t4-bridge-added"),
            custom("t4-bridge-removed"),
        );
        let builtin = adapter(kept);
        let (old, new) = (adapter(changed), adapter(changed));
        let current = super::AdapterMap::from([
            (kept, Arc::clone(&builtin)),
            (changed, Arc::clone(&old)),
            (removed, adapter(removed)),
        ]);
        let next = super::AdapterMap::from([
            (kept, Arc::clone(&builtin)),
            (changed, Arc::clone(&new)),
            (added, adapter(added)),
        ]);

        let both = bridge(&next, &current);
        assert_eq!(both.len(), 4, "{:?}", both.keys().collect::<Vec<_>>());
        assert!(both.contains_key(&removed), "the outgoing endpoint stays");
        assert!(both.contains_key(&added), "the incoming one is there");
        assert!(Arc::ptr_eq(&both[&changed], &new), "the new adapter wins");
        assert!(Arc::ptr_eq(&both[&kept], &builtin));
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

    /// Endpoints load before the catalog, so a model added with its endpoint
    /// is served by the one reload that finds them both, and leaves with it.
    ///
    /// Gated, because both are Postgres rows. With the two loads the other way
    /// round the model's provider would not parse yet and that reload would
    /// drop it; the end-to-end test cannot tell, because it waits for the
    /// next one.
    #[tokio::test]
    async fn a_model_added_with_its_endpoint_is_served_by_the_same_reload() {
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

        let name = format!("t4-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let model = oag_router::ModelId::new(format!("{name}/m"));
        oag_store::repo::insert_endpoint(
            &db,
            &oag_store::NewEndpoint {
                name: &name,
                dialect: "openai",
                platform: "plain",
                base_url: Some("http://127.0.0.1:9/v1"),
                auth: "bearer",
                region: None,
                project: None,
                api_version: None,
                path: None,
                extra_headers: &serde_json::json!({}),
                display_name: None,
                discover_models: false,
            },
        )
        .await
        .expect("an endpoint");
        let row = oag_store::ModelRow {
            id: model.as_str().to_owned(),
            provider: name.clone(),
            upstream_name: "m".to_owned(),
            input_per_mtok: rust_decimal::Decimal::ONE,
            output_per_mtok: rust_decimal::Decimal::TWO,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
            context_window: 128_000,
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

        let first = state.reload_catalog().await;
        let served = state.catalog().await.get(&model).map(|m| m.provider);
        let adapted = state.adapter(custom(&name)).is_ok();

        // Cleaned up before asserting, so a failure leaves nothing behind.
        sqlx::query("DELETE FROM model_catalog WHERE id = $1")
            .bind(model.as_str())
            .execute(db.pool())
            .await
            .expect("clean up");
        let deleted = oag_store::repo::delete_endpoint(&db, &name).await;
        let second = state.reload_catalog().await;

        first.expect("reload");
        assert_eq!(
            served,
            Some(custom(&name)),
            "the reload that found the endpoint served its model"
        );
        assert!(adapted, "with the endpoint's adapter");
        assert_eq!(
            deleted.expect("delete"),
            oag_store::EndpointDeletion::Deleted
        );
        second.expect("reload");
        assert!(state.catalog().await.get(&model).is_none());
        assert!(
            state.adapter(custom(&name)).is_err(),
            "and the adapter left with the row"
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

    /// `AppState` shows up in panics and `tracing` fields, so its `Debug` is
    /// what an operator reads: it names the providers this replica can call,
    /// and nothing it holds a secret for.
    #[tokio::test]
    async fn debug_names_the_callable_providers_and_no_secret() {
        let config = crate::testing::config("");
        let kek = config.security.credential_kek.clone();
        let db = oag_store::Db::connect(&config.database.url, 1).expect("lazy pool");
        let cache = oag_store::Cache::connect(&config.redis.url).expect("lazy client");
        let shown = format!("{:?}", AppState::new(config, db, cache).expect("state"));
        assert!(shown.starts_with("AppState"), "{shown}");
        for provider in oag_core::Provider::ALL {
            assert!(
                shown.contains(&format!("{provider:?}")),
                "{provider} missing: {shown}"
            );
        }
        assert!(
            !kek.is_empty(),
            "the test config must carry a KEK to look for"
        );
        assert!(
            !shown.contains(&kek),
            "the credential KEK leaked into Debug"
        );
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
