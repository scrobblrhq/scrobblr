use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::FromRef;
use fred::clients::Client as RedisClient;
use sqlx::PgPool;

use crate::compat::CompatConfig;
use crate::live::LiveHub;
use crate::middleware::app_signature::AppKeys;
use db::queries::scrobble_clients::{self as clients_db, ClientIdentity};

/// Where uploaded images are written and how their public URLs are built.
#[derive(Debug)]
pub struct UploadConfig {
    /// Local directory backing `/uploads` (created at startup).
    pub dir: PathBuf,
    /// External base URL clients can reach the API on; stored image URLs
    /// are `{public_base_url}/uploads/{file}`.
    pub public_base_url: String,
}

/// Shared application state injected into every handler via Axum's `State` extractor.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub redis: RedisClient,
    pub uploads: Arc<UploadConfig>,
    /// `None` when `AUTH_APP_KEYS` is unset, which leaves the auth
    /// endpoints open to any client.
    pub app_keys: Option<Arc<AppKeys>>,
    /// Reverse proxies in front of the API (`TRUSTED_PROXY_HOPS`), whose
    /// `X-Forwarded-For` entries name the client; 0 trusts none.
    pub trusted_proxy_hops: usize,
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

    /// `None` (logged) if the lookup fails: the scrobble is still worth
    /// recording without it.
    pub async fn id(&self, db: &PgPool, client: &ClientIdentity) -> Option<i32> {
        if let Some(id) = self.0.lock().unwrap().get(client) {
            return Some(*id);
        }
        match clients_db::resolve_client(db, client).await {
            Ok(id) => {
                let mut cached = self.0.lock().unwrap();
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
