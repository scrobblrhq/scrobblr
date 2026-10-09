use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::FromRef;
use fred::clients::Client as RedisClient;
use sqlx::PgPool;

use crate::compat::CompatConfig;
use crate::live::LiveHub;
use crate::media::Media;
use crate::middleware::app_signature::AppKeys;
use crate::middleware::cors::CorsOrigins;
use crate::middleware::rate_limit::TrustedProxies;
use db::queries::scrobble_clients::{self as clients_db, ClientIdentity};

/// Shared application state injected into every handler via Axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub redis: RedisClient,
    /// External base URL clients reach the API on (`PUBLIC_BASE_URL`).
    pub public_base_url: Arc<str>,
    pub media: Arc<Media>,
    /// `None` when `AUTH_APP_KEYS` is unset, which leaves the auth
    /// endpoints open to any client.
    pub app_keys: Option<Arc<AppKeys>>,
    /// The reverse proxies whose `X-Forwarded-For` names the client.
    pub proxies: Arc<TrustedProxies>,
    /// The pages that may call the native API from a browser.
    pub cors: Arc<CorsOrigins>,
    pub clients: Arc<ClientCache>,
    /// The scrobbler-compatible APIs' settings.
    pub compat: Arc<CompatConfig>,
    pub live: Arc<LiveHub>,
}

/// `scrobble_clients` ids already resolved, so ingest doesn't look one up
/// per scrobble. Names are client-chosen, hence the bound.
#[derive(Default)]
pub struct ClientCache(Mutex<HashMap<ClientIdentity, i32>>);

impl ClientCache {
    const MAX_ENTRIES: usize = 10_000;

    fn lock(&self) -> MutexGuard<'_, HashMap<ClientIdentity, i32>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `None` (logged) if the lookup fails: the scrobble is still worth
    /// recording without it.
    pub async fn id(&self, db: &PgPool, client: &ClientIdentity) -> Option<i32> {
        if let Some(id) = self.lock().get(client) {
            return Some(*id);
        }
        match clients_db::resolve_client(db, client).await {
            Ok(id) => {
                let mut cached = self.lock();
                if cached.len() >= Self::MAX_ENTRIES {
                    cached.clear();
                }
                cached.insert(client.clone(), id);
                Some(id)
            }
            Err(e) => {
                tracing::warn!(?client, "failed to resolve scrobble client: {e}");
                None
            }
        }
    }
}

impl FromRef<AppState> for PgPool {
    fn from_ref(state: &AppState) -> Self {
        state.db.clone()
    }
}

impl FromRef<AppState> for RedisClient {
    fn from_ref(state: &AppState) -> Self {
        state.redis.clone()
    }
}
