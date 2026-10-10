//! Cross-account coordination, shadow mode: every few hours, looks for
//! accounts pushing the same artists and tracks together and stores a
//! factor for each flagged account's days, which global rankings apply.
//! Detection lives in `shared::coordination`, storage in
//! `db::queries::coordination`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use sqlx::PgPool;
use tokio::time::MissedTickBehavior;

use super::env_or;
use crate::heartbeat::Beat;
use db::queries::coordination::{self as cdb, Loaded, Window};
use shared::coordination::{
    self as coordination, CoordinationParams, CoordinationSettings, Detection, Mode, Signal,
};

/// How often a run is due, besides when the UTC day or the params change.
pub const INTERVAL: TimeDelta = TimeDelta::hours(6);
const TICK: Duration = Duration::from_secs(10 * 60);
/// Runs (with their cohorts and accounts) kept for review.
const RUNS_KEPT_DAYS: i64 = 30;
pub const DEFAULT_ORIGIN_DAYS: i64 = 90;

/// Params from `RANKING_COORDINATION*`. Invalid values are an error, not a
/// fallback. Blank values count as unset.
pub fn params_from_env() -> anyhow::Result<CoordinationParams> {
    let d = CoordinationSettings::default();
    let mode = match crate::non_empty_env("RANKING_COORDINATION") {
        Some(v) => Mode::parse(&v).ok_or_else(|| {
            anyhow::anyhow!("RANKING_COORDINATION={v}: expected off, report or apply")
        })?,
        None => d.mode,
    };
    let mut weights = d.weights;
    if let Some(list) = crate::non_empty_env("RANKING_COORDINATION_WEIGHTS") {
        for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let parsed = entry
                .split_once('=')
                .and_then(|(name, value)| Some((Signal::parse(name.trim())?, value.trim())))
                .and_then(|(signal, value)| Some((signal, value.parse::<f64>().ok()?)));
            let Some((signal, value)) = parsed else {
                anyhow::bail!(
                    "RANKING_COORDINATION_WEIGHTS: `{entry}` is not signal=weight (signals: age, \
                     origin, client, sync, shape, burst)"
                );
            };
            weights[signal as usize] = value;
        }
    }
    let settings = CoordinationSettings {
        mode,
        from: env_or("RANKING_COORDINATION_FROM", d.from)?,
        to: env_or("RANKING_COORDINATION_TO", d.to)?,
        min_accounts: env_or("RANKING_COORDINATION_MIN_ACCOUNTS", d.min_accounts)?,
        min_cohort: env_or("RANKING_COORDINATION_MIN_COHORT", d.min_cohort)?,
        min_z: env_or("RANKING_COORDINATION_MIN_Z", d.min_z)?,
        window_days: env_or("RANKING_COORDINATION_WINDOW_DAYS", d.window_days)?,
        age_days: env_or("RANKING_COORDINATION_AGE_DAYS", d.age_days)?,
        sync_window_days: env_or("RANKING_COORDINATION_SYNC_WINDOW_DAYS", d.sync_window_days)?,
        sync_days: env_or("RANKING_COORDINATION_SYNC_DAYS", d.sync_days)?,
        sync_secs: env_or("RANKING_COORDINATION_SYNC_SECS", d.sync_secs)?,
        sync_context_secs: env_or(
            "RANKING_COORDINATION_SYNC_CONTEXT_SECS",
            d.sync_context_secs,
        )?,
        sync_ratio: env_or("RANKING_COORDINATION_SYNC_RATIO", d.sync_ratio)?,
        rare_client: env_or("RANKING_COORDINATION_RARE_CLIENT", d.rare_client)?,
        shape_from: env_or("RANKING_COORDINATION_SHAPE_FROM", d.shape_from)?,
        shape_to: env_or("RANKING_COORDINATION_SHAPE_TO", d.shape_to)?,
        weights,
    };
    CoordinationParams::new(&settings).map_err(|e| anyhow::anyhow!("coordination config: {e}"))
}

/// `REGISTRATION_ORIGIN_DAYS`: how long registration origins are kept.
pub fn origin_days_from_env() -> anyhow::Result<i64> {
    let days = env_or("REGISTRATION_ORIGIN_DAYS", DEFAULT_ORIGIN_DAYS)?;
    if !(1..=3650).contains(&days) {
        anyhow::bail!("REGISTRATION_ORIGIN_DAYS={days}: expected 1 to 3650");
    }
    Ok(days)
}

/// A run's outcome.
pub struct Outcome {
    pub window: Window,
    pub detection: Detection,
    pub loaded: Loaded,
    /// `None` for a dry run.
    pub run_id: Option<i32>,
    pub load: Duration,
    pub detect: Duration,
}

/// Loads, detects and, unless `dry_run`, stores. Detection runs on a
/// blocking thread: it takes seconds of CPU on a large population.
pub async fn run_once(
    db: &PgPool,
    params: &CoordinationParams,
    today: NaiveDate,
    dry_run: bool,
) -> anyhow::Result<Outcome> {
    let started_at = Utc::now();
    let started = Instant::now();
    let window = Window::new(today, params);
    let (input, loaded) = cdb::load(db, &window).await?;
    let load = started.elapsed();
    let detect_started = Instant::now();
    let detection = {
        let params = params.clone();
        tokio::task::spawn_blocking(move || coordination::detect(&input, &params)).await?
    };
    let detect = detect_started.elapsed();
    let run_id = if dry_run {
        None
    } else {
        let took_ms = started.elapsed().as_millis().min(i32::MAX as u128) as i32;
        Some(cdb::store(db, params, &window, &detection, started_at, took_ms).await?)
    };
    Ok(Outcome {
        window,
        detection,
        loaded,
        run_id,
        load,
        detect,
    })
}

fn due(latest: Option<&cdb::Run>, fingerprint: &str, now: DateTime<Utc>) -> bool {
    latest.is_none_or(|run| {
        run.fingerprint != fingerprint
            || run.window_to != now.date_naive()
            || now - run.started_at >= INTERVAL
    })
}

pub struct Detector {
    db: PgPool,
    params: CoordinationParams,
    origin_days: i64,
}

impl Detector {
    pub fn from_env(db: PgPool) -> anyhow::Result<Self> {
        let params = params_from_env()?;
        let origin_days = origin_days_from_env()?;
        tracing::info!("coordination: {}", params.fingerprint());
        Ok(Self {
            db,
            params,
            origin_days,
        })
    }

    /// Runs detection when due and, once a UTC day, deletes registration
    /// origins past their retention and old runs. Off, it removes the
    /// stored factors and only purges origins.
    pub async fn run(self: Arc<Self>, beat: Beat) {
        let off = self.params.mode == Mode::Off;
        if off {
            match cdb::clear_factors(&self.db).await {
                Ok(n) => tracing::info!("coordination: off, removed the factors of {n} user-days"),
                Err(e) => tracing::error!("coordination: failed to remove the factors: {e}"),
            }
        }
        let mut purged_on = None;
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let now = Utc::now();
            let mut failure = None;
            if purged_on != Some(now.date_naive()) {
                match self.purge().await {
                    Ok(()) => purged_on = Some(now.date_naive()),
                    Err(e) => {
                        tracing::error!("coordination: purge failed: {e}");
                        failure = Some(e.to_string());
                    }
                }
            }
            if !off && let Err(e) = self.run_if_due(now).await {
                tracing::error!("coordination: run failed, keeping the previous factors: {e}");
                failure = Some(e.to_string());
            }
            match failure {
                Some(e) => beat.failed(e).await,
                None => beat.ok().await,
            }
        }
    }

    async fn purge(&self) -> Result<(), sqlx::Error> {
        let origins = cdb::purge_origins(&self.db, self.origin_days).await?;
        if origins > 0 {
            tracing::info!(
                "coordination: deleted {origins} registration origins older than {} days",
                self.origin_days
            );
        }
        cdb::purge_runs(&self.db, RUNS_KEPT_DAYS).await?;
        Ok(())
    }

    async fn run_if_due(&self, now: DateTime<Utc>) -> anyhow::Result<()> {
        let latest = cdb::latest_run(&self.db).await?;
        if !due(latest.as_ref(), &self.params.fingerprint(), now) {
            return Ok(());
        }
        let o = run_once(&self.db, &self.params, now.date_naive(), false).await?;
        let d = &o.detection;
        tracing::info!(
            run = o.run_id,
            accounts = d.active,
            groups = d.groups,
            flagged = d.accounts.len(),
            cohorts = d.cohorts.len(),
            too_small = d.too_small,
            pairs = o.loaded.pairs,
            plays = o.loaded.plays,
            "coordination: run over {} .. {} (load {} ms, detect {} ms)",
            o.window.from,
            o.window.to,
            o.load.as_millis(),
            o.detect.as_millis()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(fingerprint: &str, started_at: DateTime<Utc>) -> cdb::Run {
        cdb::Run {
            id: 1,
            fingerprint: fingerprint.into(),
            mode: "apply".into(),
            window_from: started_at.date_naive() - TimeDelta::days(27),
            window_to: started_at.date_naive(),
            accounts: 0,
            groups: 0,
            flagged: 0,
            cohorts: 0,
            too_small: false,
            started_at,
            took_ms: 1,
            finished_at: started_at,
        }
    }

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(2026, 10, day)
            .unwrap()
            .and_hms_opt(hour, 0, 0)
            .unwrap()
            .and_utc()
    }

    #[test]
    fn a_run_is_due_when_missing_old_from_another_day_or_params() {
        assert!(due(None, "a", at(9, 10)));
        let last = run("a", at(9, 10));
        assert!(!due(Some(&last), "a", at(9, 15)));
        assert!(due(Some(&last), "a", at(9, 16)));
        assert!(due(Some(&last), "b", at(9, 11)));
        let late = run("a", at(9, 23));
        assert!(due(Some(&late), "a", at(10, 0)));
    }
}
