//! Ranking weights, shadow mode: weighs every classified day's scrobbles
//! for global rankings; nothing user-facing reads them yet. The weights live
//! in `shared::ranking`, storage, queue and rankings in
//! `db::queries::rankings`.
//!
//! Two loops, like classification's: one drains `ranking_queue` (fed when a
//! day's classification is written), the other periodically queues what
//! the queue missed or new params made stale. A third keeps each period's
//! ranking precomputed in `ranking_snapshots`.

pub mod cli;

use std::collections::HashMap;
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::PgPool;
use tokio::time::{Instant, MissedTickBehavior};

use crate::heartbeat::Beat;
use db::queries::rankings::{self as rdb, Kind, Period, Ruleset, Snapshot};
use shared::ranking::{RankingParams, RankingSettings};

const CLAIM_BATCH: i64 = 50;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Days queued per sweep, so new params reweigh history gradually.
const SWEEP_LIMIT: i64 = 5_000;
/// Positions kept per period and kind.
pub const SNAPSHOT_TOP: i64 = 1_000;
const SNAPSHOT_TICK: Duration = Duration::from_secs(60);
const SNAPSHOT_RETRY: Duration = Duration::from_secs(5 * 60);

/// How old a period's snapshots may get. They are also recomputed when the
/// UTC day changes, which moves the window, and with new params.
fn refresh_every(period: Period) -> TimeDelta {
    match period {
        Period::Week | Period::Month => TimeDelta::hours(1),
        Period::Year => TimeDelta::days(1),
    }
}

fn snapshot_due(period: Period, stored: &[Snapshot], ruleset_id: i32, now: DateTime<Utc>) -> bool {
    let mine: Vec<&Snapshot> = stored.iter().filter(|s| s.period == period).collect();
    mine.len() < Kind::ALL.len()
        || mine.iter().any(|s| {
            s.ruleset_id != ruleset_id
                || s.to_day != now.date_naive()
                || now - s.computed_at >= refresh_every(period)
        })
}

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

    pub async fn run(self: Arc<Self>, beat: Beat) {
        loop {
            let days = match rdb::claim_due(&self.db, CLAIM_BATCH).await {
                Ok(days) => days,
                Err(e) => {
                    tracing::error!("rankings: failed to claim days: {e}");
                    beat.failed(e).await;
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
            };
            if days.is_empty() {
                beat.ok().await;
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            let mut failure = None;
            for day in days {
                if let Err(e) =
                    rdb::weigh_user_day(&self.db, &self.ruleset, day.user_id, day.day, false).await
                {
                    tracing::error!(user_id = day.user_id, day = %day.day,
                        "rankings: weighing failed, retrying on the next sweep: {e}");
                    failure = Some(e.to_string());
                }
            }
            match failure {
                Some(e) => beat.failed(e).await,
                None => beat.ok().await,
            }
        }
    }

    /// First tick fires at startup, so history predating these params
    /// starts filling in immediately. Once a UTC day, also deletes the
    /// weights no period reaches any more.
    pub async fn run_sweeps(self: Arc<Self>, beat: Beat) {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        let mut purged_on = None;
        loop {
            interval.tick().await;
            let mut failure = None;
            match self.sweep().await {
                Ok(0) => {}
                Ok(n) => tracing::info!("rankings: sweep queued {n} days"),
                Err(e) => {
                    tracing::error!("rankings: sweep failed: {e}");
                    failure = Some(e.to_string());
                }
            }
            let today = Utc::now().date_naive();
            if purged_on != Some(today) {
                match rdb::purge_expired(&self.db, today).await {
                    Ok(n) => {
                        purged_on = Some(today);
                        if n > 0 {
                            tracing::info!("rankings: deleted the weights of {n} expired days");
                        }
                    }
                    Err(e) => {
                        tracing::error!("rankings: purging expired weights failed: {e}");
                        failure = Some(e.to_string());
                    }
                }
            }
            match failure {
                Some(e) => beat.failed(e).await,
                None => beat.ok().await,
            }
        }
    }

    /// Recomputes each period's snapshots when due. A failed refresh leaves
    /// the previous snapshots in place and is retried after a pause.
    pub async fn run_snapshots(self: Arc<Self>, beat: Beat) {
        let mut retry_at: HashMap<Period, Instant> = HashMap::new();
        let mut interval = tokio::time::interval(SNAPSHOT_TICK);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let stored = match rdb::snapshots(&self.db).await {
                Ok(stored) => stored,
                Err(e) => {
                    tracing::error!("rankings: failed to read snapshots: {e}");
                    beat.failed(e).await;
                    continue;
                }
            };
            let now = Utc::now();
            let mut failure = None;
            for period in Period::ALL {
                if retry_at.get(&period).is_some_and(|at| Instant::now() < *at)
                    || !snapshot_due(period, &stored, self.ruleset.id, now)
                {
                    continue;
                }
                let today = now.date_naive();
                match rdb::refresh_snapshots(&self.db, &self.ruleset, period, today, SNAPSHOT_TOP)
                    .await
                {
                    Ok(snapshots) => {
                        retry_at.remove(&period);
                        for s in snapshots {
                            tracing::info!(
                                period = period.as_str(),
                                kind = s.kind.as_str(),
                                ranked = s.ranked,
                                pending_days = s.pending_days,
                                "rankings: snapshot refreshed in {} ms",
                                s.took_ms
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            period = period.as_str(),
                            "rankings: snapshot refresh failed, keeping the previous one and \
                             retrying in {} min: {e}",
                            SNAPSHOT_RETRY.as_secs() / 60
                        );
                        retry_at.insert(period, Instant::now() + SNAPSHOT_RETRY);
                        failure = Some(e.to_string());
                    }
                }
            }
            match failure {
                Some(e) => beat.failed(e).await,
                None => beat.ok().await,
            }
        }
    }

    async fn sweep(&self) -> Result<u64, sqlx::Error> {
        let mut conn = self.db.acquire().await?;
        rdb::enqueue_stale(&mut conn, self.ruleset.id, None, None, Some(SWEEP_LIMIT)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn snapshot(period: Period, kind: Kind, computed_at: DateTime<Utc>) -> Snapshot {
        let (from_day, to_day) = period.range(computed_at.date_naive());
        Snapshot {
            period,
            kind,
            from_day,
            to_day,
            ruleset_id: 1,
            ranked: 10,
            pending_days: 0,
            computed_at,
            took_ms: 5,
        }
    }

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(2026, 10, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
            .and_utc()
    }

    #[test]
    fn snapshots_are_due_when_missing_old_from_another_day_or_params() {
        let computed = at(9, 10, 0);
        let both = |period| {
            vec![
                snapshot(period, Kind::Artist, computed),
                snapshot(period, Kind::Track, computed),
            ]
        };
        assert!(snapshot_due(Period::Week, &[], 1, computed));
        assert!(snapshot_due(
            Period::Week,
            &both(Period::Week)[..1],
            1,
            computed
        ));

        let week = both(Period::Week);
        assert!(!snapshot_due(Period::Week, &week, 1, at(9, 10, 59)));
        assert!(snapshot_due(Period::Week, &week, 1, at(9, 11, 0)));
        assert!(snapshot_due(Period::Week, &week, 2, at(9, 10, 1)));
        assert!(snapshot_due(Period::Month, &week, 1, at(9, 10, 1)));

        let year = both(Period::Year);
        assert!(!snapshot_due(Period::Year, &year, 1, at(9, 23, 59)));
        assert!(snapshot_due(Period::Year, &year, 1, at(10, 0, 0)));
    }
}
