//! Scrobble classification, shadow mode: labels every scrobble counted /
//! suspect / duplicate / no_data; nothing reads the labels yet. The rule lives in
//! `shared::classification`, storage and queue in `db::queries::classification`.
//!
//! Two loops: one drains `classification_queue` (fed by ingest, imports and
//! enrichment), the other periodically queues whatever the queue missed or
//! a threshold change made stale.

pub mod cli;

use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use db::queries::classification::{self as cdb, Ruleset};
use shared::classification::{BudgetParams, Status};

const CLAIM_BATCH: i64 = 50;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Days queued per sweep query, so a threshold change reclassifies history
/// gradually instead of flooding the queue.
const SWEEP_LIMIT: i64 = 5_000;
const RETRY_DELAY_SECS: f64 = 60.0;

/// Thresholds from `CLASSIFIER_WINDOW_SECS`, `CLASSIFIER_MAX_RATIO` and
/// `CLASSIFIER_SLACK_SECS`. Invalid values are an error, not a fallback:
/// they decide what gets stored.
pub fn params_from_env() -> anyhow::Result<BudgetParams> {
    let defaults = BudgetParams::default();
    let window = env_or("CLASSIFIER_WINDOW_SECS", defaults.window_ms / 1000)?;
    let ratio = env_or(
        "CLASSIFIER_MAX_RATIO",
        defaults.max_ratio_permille as f64 / 1000.0,
    )?;
    let slack = env_or("CLASSIFIER_SLACK_SECS", defaults.slack_ms / 1000)?;
    BudgetParams::new(window, ratio, slack).map_err(|e| anyhow::anyhow!("classifier config: {e}"))
}

fn env_or<T>(name: &str, default: T) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    match std::env::var(name) {
        Ok(value) => value
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("{name}={value}: {e}")),
        Err(_) => Ok(default),
    }
}

pub struct Classifier {
    db: PgPool,
    ruleset: Ruleset,
}

impl Classifier {
    pub async fn from_env(db: PgPool) -> anyhow::Result<Self> {
        let ruleset = cdb::register_ruleset(&db, params_from_env()?).await?;
        tracing::info!(
            ruleset = ruleset.id,
            "classification: {}",
            ruleset.params.fingerprint()
        );
        Ok(Self { db, ruleset })
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            let days = match cdb::claim_due(&self.db, CLAIM_BATCH).await {
                Ok(days) => days,
                Err(e) => {
                    tracing::error!("classification: failed to claim days: {e}");
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
            };
            if days.is_empty() {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }

            for day in days {
                match cdb::classify_user_day(&self.db, &self.ruleset, day.user_id, day.day, false)
                    .await
                {
                    Ok(outcome) => {
                        let newly_suspect: i64 = outcome
                            .changes
                            .iter()
                            .filter(|((_, to), _)| *to == Status::Suspect)
                            .map(|(_, n)| n)
                            .sum();
                        if newly_suspect > 0 {
                            tracing::info!(user_id = day.user_id, day = %day.day,
                                "classification: {newly_suspect} scrobbles newly suspect ({} in the day)",
                                outcome.counts.suspect);
                        }
                    }
                    Err(e) => {
                        tracing::error!(user_id = day.user_id, day = %day.day,
                            "classification failed, retrying later: {e}");
                        if let Err(e) =
                            cdb::requeue(&self.db, day, cdb::PRIORITY_INGEST, RETRY_DELAY_SECS)
                                .await
                        {
                            tracing::error!("classification: requeue failed: {e}");
                        }
                    }
                }
            }
        }
    }

    /// First tick fires at startup, so history predating the classifier (or
    /// the current thresholds) starts filling in immediately.
    pub async fn run_sweeps(self: Arc<Self>) {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            match self.sweep().await {
                Ok(0) => {}
                Ok(n) => tracing::info!("classification: sweep queued {n} days"),
                Err(e) => tracing::error!("classification: sweep failed: {e}"),
            }
        }
    }

    async fn sweep(&self) -> Result<u64, sqlx::Error> {
        let mut conn = self.db.acquire().await?;
        cdb::enqueue_stale(
            &mut conn,
            self.ruleset.id,
            None,
            None,
            Some(SWEEP_LIMIT),
            cdb::PRIORITY_SWEEP,
        )
        .await
    }
}
