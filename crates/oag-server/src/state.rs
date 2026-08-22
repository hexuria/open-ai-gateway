//! Shared application state.

use crate::shutdown::Lifecycle;
use oag_core::config::Config;
use oag_store::{Cache, Db};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub cache: Cache,
    pub lifecycle: Arc<Lifecycle>,
}

impl AppState {
    #[must_use]
    pub fn new(config: Config, db: Db, cache: Cache) -> Self {
        Self {
            config: Arc::new(config),
            db,
            cache,
            lifecycle: Arc::new(Lifecycle::new()),
        }
    }
}
