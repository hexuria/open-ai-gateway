//! Shared application state.

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
    adapters: Arc<HashMap<Provider, Arc<dyn ProviderAdapter>>>,
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

impl AppState {
    pub fn new(config: Config, db: Db, cache: Cache) -> Result<Self> {
        let kek = Kek::from_base64(&config.security.credential_kek)?;

        let base = |p: Provider, default: &str| -> String {
            config
                .gateway
                .provider_base_urls
                .get(p.as_str())
                .cloned()
                .unwrap_or_else(|| default.to_owned())
        };

        let mut adapters: HashMap<Provider, Arc<dyn ProviderAdapter>> = HashMap::new();
        adapters.insert(
            Provider::Anthropic,
            Arc::new(oag_upstream::AnthropicAdapter::new(base(
                Provider::Anthropic,
                "https://api.anthropic.com",
            ))),
        );

        Ok(Self {
            auth: AuthCache::new(db.clone(), cache.clone(), 10_000),
            transports: TransportPool::new(2_048, Duration::from_mins(15), Duration::from_secs(10)),
            config: Arc::new(config),
            db,
            cache,
            lifecycle: Arc::new(Lifecycle::new()),
            kek: Arc::new(kek),
            adapters: Arc::new(adapters),
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

    #[must_use]
    pub fn providers(&self) -> Vec<Provider> {
        self.adapters.keys().copied().collect()
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
