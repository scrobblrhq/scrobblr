mod classification;
mod connected_accounts;
mod enrichment;
#[cfg(test)]
mod fake_lastfm;
mod lastfm_import;
#[cfg(test)]
mod test_support;
mod uploads;

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

    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        println!(
            "{USAGE}\n{}\n{}\n{}",
            classification::cli::USAGE,
            lastfm_import::cli::USAGE,
            uploads::USAGE
        );
        return Ok(());
    }

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");

    tracing::info!("worker: connecting to database...");
    let db = db::pool::connect(&database_url).await?;

    // Every Last.fm caller in the worker shares one pace.
    let lastfm_limiter = Arc::new(enrichment::ratelimit::RateLimiter::new(
        enrichment::LASTFM_INTERVAL,
    ));
    let lastfm_http = reqwest::Client::builder()
        .user_agent("scrobblr-worker/0.1 (+https://github.com/scrobblrhq/scrobblr)")
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()?;

    match args.first().map(String::as_str) {
        None => {}
        Some("migrate") => return migrate(&db, &args[1..]).await,
        Some("classify") => {
            db::migrate::ensure_current(&db).await?;
            return classification::cli::run(&db, &args[1..]).await;
        }
        Some("import") => {
            db::migrate::ensure_current(&db).await?;
            return lastfm_import::cli::run(&db, lastfm_http, &args[1..]).await;
        }
        Some("uploads") => {
            db::migrate::ensure_current(&db).await?;
            return uploads::run(&db, &args[1..]).await;
        }
        Some(other) => anyhow::bail!("unknown command `{other}` (see `worker --help`)"),
    }

    db::migrate::ensure_current(&db).await?;
    // The now playing it re-publishes carries image URLs, built like the API's.
    shared::media::set_public_url(shared::media::public_url_from_env()?);

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
                Ok((sessions, now_playing, authorizations)) => {
                    tracing::info!(
                        "cleanup: removed {sessions} expired sessions, {now_playing} stale now_playing rows, {authorizations} expired scrobbler authorizations"
                    );
                }
                Err(e) => tracing::error!("cleanup error: {e}"),
            }
        }
    });

    // Claims jobs from enrichment_jobs and queries the metadata providers
    // (MusicBrainz, Cover Art Archive, Deezer, optionally Last.fm).
    let enricher = Arc::new(enrichment::Enricher::from_env(
        db.clone(),
        redis.clone(),
        lastfm_limiter.clone(),
    )?);
    let enrichment_handle = tokio::spawn(enricher.clone().run());
    let maintenance_handle = tokio::spawn(enricher.run_maintenance());

    // Polls connected Spotify accounts and turns their listening history
    // into scrobbles (and live now-playing state, if `redis` is available).
    // Logs once and idles if Spotify OAuth credentials
    // aren't configured.
    let connected_accounts_poller = Arc::new(
        connected_accounts::ConnectedAccountsPoller::from_env(db.clone(), redis),
    );
    let connected_accounts_handle = tokio::spawn(connected_accounts_poller.run());

    // Fills track lengths from Last.fm for tracks no source has one for
    // (imported history), so the classifier can label their scrobbles.
    let lengths = enrichment::lengths::LengthBackfill::from_env(
        db.clone(),
        lastfm_http.clone(),
        lastfm_limiter.clone(),
    );
    let lengths_handle = tokio::spawn(async move {
        match lengths {
            Some(lengths) => Arc::new(lengths).run().await,
            None => std::future::pending().await,
        }
    });

    // Runs Last.fm history imports, started from the API or `worker import`.
    let importer = lastfm_import::Importer::from_env(db.clone(), lastfm_http, lastfm_limiter)?;
    let import_handle = tokio::spawn(async move {
        match importer {
            Some(importer) => Arc::new(importer).run().await,
            None => {
                tracing::info!("worker: LASTFM_API_KEY not set — Last.fm imports disabled");
                std::future::pending().await
            }
        }
    });

    // Labels scrobbles counted / suspect / duplicate / no_data (shadow mode).
    let classifier = Arc::new(classification::Classifier::from_env(db.clone()).await?);
    let classification_handle = tokio::spawn(classifier.clone().run());
    let classification_sweep_handle = tokio::spawn(classifier.run_sweeps());

    // The tasks loop forever; one finishing means it died, which must end
    // the process with a failure so a supervisor restarts it.
    let died = tokio::select! {
        _ = cleanup_handle => "cleanup",
        _ = enrichment_handle => "enrichment",
        _ = maintenance_handle => "enrichment maintenance",
        _ = connected_accounts_handle => "connected-accounts poller",
        _ = import_handle => "import",
        _ = lengths_handle => "length backfill",
        _ = classification_handle => "classification",
        _ = classification_sweep_handle => "classification sweep",
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received Ctrl-C, shutting down");
            return Ok(());
        }
    };
    anyhow::bail!("{died} task exited unexpectedly")
}

const USAGE: &str = "\
usage: worker                      run the background jobs
       worker migrate              apply pending migrations
       worker migrate status       list migrations and whether each is applied
       worker migrate --baseline N record 1..=N as applied without running them
       worker --help";

async fn migrate(db: &sqlx::PgPool, args: &[String]) -> anyhow::Result<()> {
    let applied = match args {
        [] => db::migrate::run(db).await?,
        [cmd] if cmd == "status" => {
            for m in db::migrate::status(db).await? {
                let state = if m.applied { "applied" } else { "pending" };
                println!("{:04} {:<8} {}", m.version, state, m.description);
            }
            return Ok(());
        }
        [flag, version] if flag == "--baseline" => {
            db::migrate::baseline(db, version.parse()?).await?
        }
        _ => anyhow::bail!("{USAGE}"),
    };
    match applied.as_slice() {
        [] => println!("database is up to date"),
        versions => println!("applied {} migration(s): {versions:?}", versions.len()),
    }
    Ok(())
}

/// A setting, unless unset or blank (docker-compose passes unset ones as
/// empty strings).
fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Connects to Redis for now-playing republishing. Any failure (unset,
/// malformed, or unreachable) degrades to `None` with a log line rather than
/// taking the worker down — enrichment does not depend on Redis. Once
/// connected, it reconnects whenever Redis comes back.
async fn connect_redis() -> Option<fred::clients::Client> {
    let Ok(url) = std::env::var("REDIS_URL") else {
        tracing::info!("worker: REDIS_URL not set — now-playing won't refresh after enrichment");
        return None;
    };
    let mut config = match fred::types::config::Config::from_url(&url) {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!("worker: invalid REDIS_URL, now-playing won't refresh live: {e}");
            return None;
        }
    };
    // Unlike one inside REDIS_URL, it may hold any character.
    if let Some(password) = non_empty_env("REDIS_PASSWORD") {
        config.password = Some(password);
    }
    let mut builder = RedisBuilder::from_config(config);
    builder
        .set_policy(fred::types::config::ReconnectPolicy::new_exponential(
            0, 100, 10_000, 2,
        ))
        .with_performance_config(|c| {
            c.default_command_timeout = std::time::Duration::from_secs(3);
        });
    match builder.build() {
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

/// Purges expired `user_sessions`, stale `now_playing` rows and expired
/// scrobbler authorizations, returning how many of each.
async fn cleanup_expired(db: &sqlx::PgPool) -> Result<(u64, u64, u64), sqlx::Error> {
    let sessions = db::queries::auth::delete_expired_sessions(db).await?;

    let np_result = sqlx::query!("DELETE FROM now_playing WHERE expires_at <= NOW()")
        .execute(db)
        .await?;

    let authorizations = db::queries::scrobblers::delete_expired_authorizations(db).await?;

    Ok((sessions, np_result.rows_affected(), authorizations))
}
