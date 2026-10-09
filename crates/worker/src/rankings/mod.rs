//! Ranking weights, shadow mode: weighs every classified day's scrobbles
//! for global rankings; nothing user-facing reads them yet. The weights live
//! in `shared::ranking`, storage, queue and rankings in
//! `db::queries::rankings`.
//!
//! Two loops, like classification's: one drains `ranking_queue` (fed when a
//! day's classification is written), the other periodically queues what
//! the queue missed or new params made stale.

pub mod cli;

use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use db::queries::rankings::{self as rdb, Ruleset};
use shared::ranking::{RankingParams, RankingSettings};

const CLAIM_BATCH: i64 = 50;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Days queued per sweep, so new params reweigh history gradually.
const SWEEP_LIMIT: i64 = 5_000;

/// Params from `RANKING_*`. Invalid values are an error, not a fallback:
/// they decide what gets stored. Blank values count as unset.
pub fn params_from_env() -> anyhow::Result<RankingParams> {
    let d = RankingSettings::default();
    let mut known_clients = d.known_clients.clone();
    if let Some(extra) = crate::non_empty_env("RANKING_KNOWN_CLIENTS") {
        known_clients.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(str::to_string),
        );
    }
    let settings = RankingSettings {
        unknown_client: env_or("RANKING_UNKNOWN_CLIENT_WEIGHT", d.unknown_client)?,
        no_listened: env_or("RANKING_NO_LISTENED_WEIGHT", d.no_listened)?,
        no_data: env_or("RANKING_NO_DATA_WEIGHT", d.no_data)?,
        imported: env_or("RANKING_IMPORT_WEIGHT", d.imported)?,
        min_account_days: env_or("RANKING_MIN_ACCOUNT_DAYS", d.min_account_days)?,
        track_daily_cap: env_or("RANKING_TRACK_DAILY_CAP", d.track_daily_cap)?,
        artist_daily_cap: env_or("RANKING_ARTIST_DAILY_CAP", d.artist_daily_cap)?,
        known_clients,
    };
    RankingParams::new(&settings).map_err(|e| anyhow::anyhow!("ranking config: {e}"))
}

fn env_or<T>(name: &str, default: T) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    match crate::non_empty_env(name) {
        Some(value) => value
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("{name}={value}: {e}")),
        None => Ok(default),
    }
}

pub struct Weigher {
    db: PgPool,
    ruleset: Ruleset,
}

impl Weigher {
    pub async fn from_env(db: PgPool) -> anyhow::Result<Self> {
        let ruleset = rdb::register_ruleset(&db, &params_from_env()?).await?;
        tracing::info!(
            ruleset = ruleset.id,
            "rankings: {}",
            ruleset.params.fingerprint()
        );
        Ok(Self { db, ruleset })
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            let days = match rdb::claim_due(&self.db, CLAIM_BATCH).await {
                Ok(days) => days,
                Err(e) => {
                    tracing::error!("rankings: failed to claim days: {e}");
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
            };
            if days.is_empty() {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            for day in days {
                if let Err(e) =
                    rdb::weigh_user_day(&self.db, &self.ruleset, day.user_id, day.day, false).await
                {
                    tracing::error!(user_id = day.user_id, day = %day.day,
                        "rankings: weighing failed, retrying on the next sweep: {e}");
                }
            }
        }
    }

    /// First tick fires at startup, so history predating these params
    /// starts filling in immediately.
    pub async fn run_sweeps(self: Arc<Self>) {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            match self.sweep().await {
                Ok(0) => {}
                Ok(n) => tracing::info!("rankings: sweep queued {n} days"),
                Err(e) => tracing::error!("rankings: sweep failed: {e}"),
            }
        }
    }

    async fn sweep(&self) -> Result<u64, sqlx::Error> {
        let mut conn = self.db.acquire().await?;
        rdb::enqueue_stale(&mut conn, self.ruleset.id, None, None, Some(SWEEP_LIMIT)).await
    }
}
