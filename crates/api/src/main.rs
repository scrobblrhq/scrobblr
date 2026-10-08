mod compat;
mod errors;
mod handlers;
mod live;
mod middleware;
mod router;
mod state;
#[cfg(test)]
mod test_app;

use fred::{interfaces::ClientLike, types::Builder as RedisBuilder};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Connected-account OAuth tokens are encrypted at rest, so a missing or
    // malformed TOKEN_ENCRYPTION_KEY has to fail here rather than halfway
    // through a user's OAuth callback. Only enforced when the feature is
    // actually configured — deployments not using connected accounts don't
    // need a key at all.
    for feature in ["SPOTIFY_CLIENT_ID", "LASTFM_SHARED_SECRET"] {
        if std::env::var(feature).is_ok_and(|v| !v.trim().is_empty()) {
            shared::crypto::check_key().map_err(|e| anyhow::anyhow!("{feature} is set but {e}"))?;
        }
    }
    // Optional otherwise (scrobbler tokens then can't serve Audioscrobbler
    // 1.2), but a malformed key must not surface on the first token made.
    if std::env::var("TOKEN_ENCRYPTION_KEY").is_ok_and(|v| !v.trim().is_empty()) {
        shared::crypto::check_key()?;
    }

    let app_keys = middleware::app_signature::AppKeys::from_env()?.map(std::sync::Arc::new);
    match &app_keys {
        Some(keys) => tracing::info!(
            "auth endpoints restricted to signed clients: {}",
            keys.app_ids().collect::<Vec<_>>().join(", ")
        ),
        None => tracing::warn!(
            "AUTH_APP_KEYS not set - /v1/auth/register and /v1/auth/login accept unsigned clients"
        ),
    }

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    // Database
    tracing::info!("connecting to database...");
    let db = db::pool::connect(&database_url).await?;

    // Migrations are a deploy step (`worker migrate`), never run on startup:
    // replicas and the worker would race, and long or non-transactional ones
    // don't belong in a boot path. Refuse to serve an outdated schema instead.
    db::migrate::ensure_current(&db).await?;

    // Redis
    tracing::info!("connecting to redis...");
    let redis_builder = redis_builder(fred::types::config::Config::from_url(&redis_url)?);
    let redis = redis_builder.build()?;
    redis.init().await?;
    let live = std::sync::Arc::new(live::LiveHub::default());
    live::subscribe(live.clone(), redis_builder.build_subscriber_client()?).await?;

    // Uploaded images (avatars, artist/album art)
    let upload_dir =
        std::path::PathBuf::from(std::env::var("UPLOAD_DIR").unwrap_or_else(|_| "uploads".into()));
    tokio::fs::create_dir_all(&upload_dir).await?;
    let public_base_url = std::env::var("PUBLIC_BASE_URL").unwrap_or_else(|_| {
        tracing::warn!(
            "PUBLIC_BASE_URL not set — uploaded image URLs will point at http://localhost:8080"
        );
        "http://localhost:8080".into()
    });
    let uploads = std::sync::Arc::new(state::UploadConfig {
        dir: upload_dir,
        public_base_url: public_base_url.trim_end_matches('/').to_string(),
    });

    let compat = std::sync::Arc::new(compat::CompatConfig::from_env(app_keys.is_some())?);
    if !compat.password_login {
        tracing::info!(
            "scrobbler APIs take scrobbler tokens only, not account passwords (SCROBBLER_PASSWORD_LOGIN)"
        );
    }

    let trusted_proxy_hops = match std::env::var("TRUSTED_PROXY_HOPS") {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("TRUSTED_PROXY_HOPS={v}: {e}"))?,
        _ => 0,
    };

    // Axum
    let state = state::AppState {
        db,
        redis,
        uploads,
        app_keys,
        trusted_proxy_hops,
        clients: Default::default(),
        compat,
        live,
    };
    let app = router::build(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("scrobblr api listening on {bind_addr}");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Reconnects for as long as Redis is away, and fails a command after a
/// few seconds rather than holding the request until it's back.
fn redis_builder(config: fred::types::config::Config) -> RedisBuilder {
    let mut builder = RedisBuilder::from_config(config);
    builder
        .set_policy(fred::types::config::ReconnectPolicy::new_exponential(
            0, 100, 10_000, 2,
        ))
        .with_performance_config(|c| {
            c.default_command_timeout = std::time::Duration::from_secs(3);
        });
    builder
}
