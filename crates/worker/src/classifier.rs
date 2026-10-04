//! Shadow-mode scrobble classifier (anti-botting, Part 1).
//!
//! Labels every scrobble `counted` / `suspect` / `no_data` after ingest, by
//! the listening-time budget in [`shared::classification`]. Nothing reads the
//! labels yet: no chart, ranking, query or endpoint changes, nobody is
//! blocked. They exist to be reviewed by hand (`worker classify report`).
//!
//! Work flows through the per user-day ledger in `classification_days`:
//!
//! ```text
//!  ingest_scrobble ──mark──┐      tracks trigger (duration filled)
//!  6h reconcile ───mark────┤        └─► classification_track_refresh ─drain─┐
//!  CLI reclassify/backfill ┤                                                │
//!  startup: outdated days ─┴──► classification_days (dirty) ◄──── mark ─────┘
//!                                   │ claim (settle passed, lease)
//!                                   ▼
//!        load [day − W, day + 1d) ─► classify() ─► labels for the day's scrobbles
//! ```
//!
//! Runs as two loops: [`Classifier::run`] claims and classifies due days;
//! [`Classifier::run_maintenance`] drains track-duration fills, reconciles
//! recent days against the ledger, and logs running totals.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Duration as ChronoDuration;
use sqlx::PgPool;

use db::queries::classification as cdb;
use shared::classification::{
    self as rule, RULES_VERSION, ScrobbleSample, Status, TimeBudgetConfig,
};

use crate::enrichment::backoff_secs;

const CLAIM_BATCH: i64 = 20;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// A claimed day not completed within this long is claimable again (crashed
/// worker). Also the minimum age of a track-refresh entry before it is
/// drained, so an in-flight classification has committed first.
const CLAIM_LEASE_SECS: f64 = 15.0 * 60.0;
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60);
const TRACK_REFRESH_BATCH: i64 = 200;
/// Reconcile every 6 h (in maintenance ticks), over today + 3 previous days.
const RECONCILE_EVERY_TICKS: u64 = 6 * 60;
const RECONCILE_LOOKBACK_DAYS: i32 = 3;
const TOTALS_LOG_EVERY_TICKS: u64 = 10;

/// Classifier configuration, read from the environment. All thresholds live
/// here so they can be tuned without a code change; any change produces a
/// new ruleset and the worker re-labels history at background priority.
#[derive(Debug, Clone)]
pub struct Settings {
    pub rule: TimeBudgetConfig,
    /// Delay between a day's first mark and its classification, so a burst
    /// of scrobbles is classified once instead of once per scrobble.
    pub settle_secs: f64,
}

impl Settings {
    /// `Ok(None)` when disabled with `CLASSIFIER_ENABLED=false`.
    pub fn from_env() -> Result<Option<Self>, String> {
        if let Ok(v) = std::env::var("CLASSIFIER_ENABLED")
            && matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "false" | "0" | "no" | "off"
            )
        {
            return Ok(None);
        }
        let defaults = TimeBudgetConfig::default();
        let rule = TimeBudgetConfig {
            window_secs: env_or("CLASSIFIER_WINDOW_SECS", defaults.window_secs)?,
            margin_ratio: env_or("CLASSIFIER_MARGIN_RATIO", defaults.margin_ratio)?,
            margin_slack_secs: env_or("CLASSIFIER_MARGIN_SLACK_SECS", defaults.margin_slack_secs)?,
        };
        rule.validate().map_err(|e| e.to_string())?;
        let settle_secs: i32 = env_or("CLASSIFIER_SETTLE_SECS", 120)?;
        if settle_secs < 0 {
            return Err(format!(
                "CLASSIFIER_SETTLE_SECS must be >= 0, got {settle_secs}"
            ));
        }
        Ok(Some(Self {
            rule,
            settle_secs: f64::from(settle_secs),
        }))
    }
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> Result<T, String> {
    match std::env::var(key) {
        Ok(v) => v
            .trim()
            .parse()
            .map_err(|_| format!("{key}: invalid value {v:?}")),
        Err(_) => Ok(default),
    }
}

/// Running totals since startup, logged periodically.
#[derive(Default)]
struct Totals {
    days: AtomicU64,
    counted: AtomicU64,
    suspect: AtomicU64,
    no_data: AtomicU64,
}

pub struct Classifier {
    db: PgPool,
    settings: Settings,
    ruleset_id: i32,
    totals: Totals,
}

impl Classifier {
    /// Registers this worker's ruleset and re-queues days labeled under any
    /// other ruleset (low priority, behind fresh ingest).
    pub async fn start(db: PgPool, settings: Settings) -> Result<Self, sqlx::Error> {
        let params =
            serde_json::to_string(&settings.rule).expect("TimeBudgetConfig always serializes");
        let ruleset_id = cdb::activate_ruleset(&db, RULES_VERSION, &params).await?;
        let outdated = cdb::mark_outdated(&db, ruleset_id).await?;
        tracing::info!(
            ruleset_id,
            rules_version = RULES_VERSION,
            params = %params,
            outdated_days_requeued = outdated,
            "classifier: ruleset active (shadow mode)"
        );
        Ok(Self {
            db,
            settings,
            ruleset_id,
            totals: Totals::default(),
        })
    }

    /// Main loop: claim due days and classify them one by one.
    pub async fn run(self: Arc<Self>) {
        loop {
            match self.tick().await {
                Ok(0) => tokio::time::sleep(POLL_INTERVAL).await,
                Ok(_) => {}
                Err(e) => {
                    tracing::error!("classifier: failed to claim days: {e}");
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// Claims one batch of due days and processes it. Returns how many days
    /// were claimed.
    async fn tick(&self) -> Result<usize, sqlx::Error> {
        let days = cdb::claim_dirty_days(
            &self.db,
            CLAIM_BATCH,
            self.settings.settle_secs,
            CLAIM_LEASE_SECS,
        )
        .await?;
        for day in &days {
            self.process(day).await;
        }
        Ok(days.len())
    }

    async fn process(&self, claimed: &cdb::ClaimedDay) {
        if let Err(e) = self.classify_day(claimed).await {
            let attempt = claimed.attempts + 1;
            let delay = backoff_secs(attempt);
            tracing::warn!(user_id = claimed.user_id, day = %claimed.day.date_naive(),
                "classifier: day failed, retrying in {delay:.0}s (attempt {attempt}): {e}");
            if let Err(e) = cdb::reschedule_day(
                &self.db,
                claimed.user_id,
                claimed.day,
                &e.to_string(),
                delay,
            )
            .await
            {
                // The lease expires on its own; the day is retried then.
                tracing::error!("classifier: reschedule failed: {e}");
            }
        }
    }

    /// Labels every scrobble of one user-day. The scrobbles of the preceding
    /// window are loaded too, since they count toward the budget of the day's
    /// first scrobbles, but they are labeled by their own day's job.
    async fn classify_day(&self, claimed: &cdb::ClaimedDay) -> Result<(), sqlx::Error> {
        let window = ChronoDuration::seconds(i64::from(self.settings.rule.window_secs));
        let day_end = claimed.day + ChronoDuration::days(1);
        let rows =
            cdb::load_user_window(&self.db, claimed.user_id, claimed.day - window, day_end).await?;

        let track_of: HashMap<i64, i64> = rows.iter().map(|r| (r.id, r.track_id)).collect();
        let samples: Vec<ScrobbleSample> = rows
            .into_iter()
            .map(|r| ScrobbleSample {
                id: r.id,
                played_at: r.played_at,
                track_id: r.track_id,
                source: r.source,
                duration_ms: r.duration_ms,
            })
            .collect();

        let mut labels = cdb::DayLabels::default();
        for c in rule::classify(&samples, &self.settings.rule)
            .into_iter()
            .filter(|c| c.played_at >= claimed.day)
        {
            match c.status {
                Status::Counted => labels.counted += 1,
                Status::Suspect => labels.suspect += 1,
                Status::NoData => labels.no_data += 1,
            }
            labels.scrobble_ids.push(c.id);
            labels.played_at.push(c.played_at);
            labels.track_ids.push(track_of[&c.id]);
            labels.statuses.push(c.status.as_str().to_string());
            labels.reasons.push(c.reason.as_str().to_string());
            labels.scores.push(c.score);
        }

        let clean = cdb::apply_day_results(&self.db, claimed, self.ruleset_id, &labels).await?;

        self.totals.days.fetch_add(1, Ordering::Relaxed);
        self.totals
            .counted
            .fetch_add(labels.counted as u64, Ordering::Relaxed);
        self.totals
            .suspect
            .fetch_add(labels.suspect as u64, Ordering::Relaxed);
        self.totals
            .no_data
            .fetch_add(labels.no_data as u64, Ordering::Relaxed);

        // Days with suspects are what shadow-mode review is about; log those
        // at info, everything else at debug to keep the worker log readable.
        if labels.suspect > 0 {
            tracing::info!(user_id = claimed.user_id, day = %claimed.day.date_naive(),
                counted = labels.counted, suspect = labels.suspect, no_data = labels.no_data,
                ruleset_id = self.ruleset_id, requeued = !clean, "classifier: day classified");
        } else {
            tracing::debug!(user_id = claimed.user_id, day = %claimed.day.date_naive(),
                counted = labels.counted, no_data = labels.no_data,
                ruleset_id = self.ruleset_id, requeued = !clean, "classifier: day classified");
        }
        Ok(())
    }

    /// Maintenance loop: every minute drains track-duration fills (re-checks
    /// their no_data labels); every 6 h reconciles recent days against the
    /// ledger (the ingest hook is best-effort); every 10 minutes logs totals.
    /// The first tick fires at startup.
    pub async fn run_maintenance(self: Arc<Self>) {
        let mut interval = tokio::time::interval(MAINTENANCE_INTERVAL);
        let mut tick: u64 = 0;
        loop {
            interval.tick().await;

            loop {
                match cdb::drain_track_refresh(&self.db, CLAIM_LEASE_SECS, TRACK_REFRESH_BATCH)
                    .await
                {
                    Ok((0, _)) => break,
                    Ok((tracks, days)) => tracing::info!(
                        "classifier: {tracks} tracks gained a duration, re-queued {days} days"
                    ),
                    Err(e) => {
                        tracing::error!("classifier: track refresh drain failed: {e}");
                        break;
                    }
                }
            }

            if tick.is_multiple_of(RECONCILE_EVERY_TICKS) {
                match cdb::reconcile_recent(&self.db, RECONCILE_LOOKBACK_DAYS).await {
                    Ok(0) => {}
                    Ok(n) => tracing::warn!("classifier: reconciliation re-queued {n} days"),
                    Err(e) => tracing::error!("classifier: reconciliation failed: {e}"),
                }
            }

            if tick.is_multiple_of(TOTALS_LOG_EVERY_TICKS) && tick > 0 {
                tracing::info!(
                    days = self.totals.days.load(Ordering::Relaxed),
                    counted = self.totals.counted.load(Ordering::Relaxed),
                    suspect = self.totals.suspect.load(Ordering::Relaxed),
                    no_data = self.totals.no_data.load(Ordering::Relaxed),
                    ruleset_id = self.ruleset_id,
                    "classifier: totals since startup"
                );
            }
            tick += 1;
        }
    }
}
