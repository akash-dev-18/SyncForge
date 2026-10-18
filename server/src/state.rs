//! Shared application state for the control-plane API.

use std::sync::Arc;

use crate::db::PgPool;
use crate::relay::RelayRegistry;

/// State handed to every axum handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub relay: Arc<RelayRegistry>,
}

impl AppState {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            relay: Arc::new(RelayRegistry::new()),
        }
    }
}
