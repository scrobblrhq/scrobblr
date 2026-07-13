use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::FromRef;
use fred::clients::Client as RedisClient;
use sqlx::PgPool;

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
