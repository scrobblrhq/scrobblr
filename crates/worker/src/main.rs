mod connected_accounts;
mod enrichment;

use std::sync::Arc;

use fred::interfaces::ClientLike;
use fred::types::Builder as RedisBuilder;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .init();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");

    tracing::info!("worker: connecting to database...");
    let db = db::pool::connect(&database_url).await?;

    // Redis lets the worker re-publish now-playing over the API's SSE channel
    // once it fills an image, so live cards swap the fallback for the cover.
    // Best-effort: a missing/unreachable Redis only disables that live
    // refresh — enrichment (the worker's real job) must still run.
    let redis = connect_redis().await;

    tracing::info!("worker: starting background loops");

    // Runs every 5 minutes and purges expired sessions + now_playing rows.
    let db_cleanup = db.clone();
    let cleanup_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            match cleanup_expired(&db_cleanup).await {
                Ok((sessions, now_playing)) => {
                    tracing::info!(
                        "cleanup: removed {sessions} expired sessions, {now_playing} stale now_playing rows"
                    );
                }
                Err(e) => tracing::error!("cleanup error: {e}"),
            }
        }
    });

    // Claims jobs from enrichment_jobs and queries the metadata providers
    // (MusicBrainz, Cover Art Archive, Deezer, optionally Last.fm).
    let enricher = Arc::new(enrichment::Enricher::from_env(db.clone(), redis.clone())?);
    let enrichment_handle = tokio::spawn(enricher.clone().run());
    let maintenance_handle = tokio::spawn(enricher.run_maintenance());

    // Polls connected Spotify accounts and turns their listening history
    // into scrobbles (and live now-playing state, if `redis` is available).
    // A no-op loop (logs once and returns) if Spotify OAuth credentials
    // aren't configured.
    let connected_accounts_poller = Arc::new(
        connected_accounts::ConnectedAccountsPoller::from_env(db.clone(), redis),
    );
    let connected_accounts_handle = tokio::spawn(connected_accounts_poller.run());

    // The tasks loop forever; reaching select! means one died or Ctrl-C.
    tokio::select! {
        _ = cleanup_handle => tracing::warn!("cleanup task exited unexpectedly"),
        _ = enrichment_handle => tracing::warn!("enrichment task exited unexpectedly"),
        _ = maintenance_handle => tracing::warn!("enrichment maintenance task exited unexpectedly"),
        _ = connected_accounts_handle => tracing::warn!("connected-accounts poller exited unexpectedly"),
        _ = tokio::signal::ctrl_c() => tracing::info!("received Ctrl-C, shutting down"),
    }

    Ok(())
}

/// Connects to Redis for now-playing republishing. Any failure (unset,
/// malformed, or unreachable) degrades to `None` with a log line rather than
/// taking the worker down — enrichment does not depend on Redis.
async fn connect_redis() -> Option<fred::clients::Client> {
    let Ok(url) = std::env::var("REDIS_URL") else {
        tracing::info!("worker: REDIS_URL not set — now-playing won't refresh after enrichment");
        return None;
    };
    let config = match fred::types::config::Config::from_url(&url) {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!("worker: invalid REDIS_URL, now-playing won't refresh live: {e}");
            return None;
        }
    };
    match RedisBuilder::from_config(config).build() {
        Ok(client) => match client.init().await {
            Ok(_) => Some(client),
            Err(e) => {
                tracing::warn!("worker: redis unreachable, now-playing won't refresh live: {e}");
                None
            }
        },
        Err(e) => {
            tracing::warn!("worker: redis client build failed, now-playing won't refresh: {e}");
            None
        }
    }
}

/// Purges expired `user_sessions` and stale `now_playing` rows.
/// Returns `(sessions_deleted, now_playing_deleted)`.
async fn cleanup_expired(db: &sqlx::PgPool) -> Result<(u64, u64), sqlx::Error> {
    let sessions = db::queries::auth::delete_expired_sessions(db).await?;

    let np_result = sqlx::query!("DELETE FROM now_playing WHERE expires_at <= NOW()")
        .execute(db)
        .await?;

    Ok((sessions, np_result.rows_affected()))
}
