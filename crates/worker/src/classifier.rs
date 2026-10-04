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

/// `worker classify …` — internal commands for reviewing shadow-mode results
/// by hand. Not reachable over HTTP. `reclassify` / `backfill` only mark days
/// dirty; the running worker does the classification.
pub mod cli {
    use chrono::{DateTime, Utc};
    use sqlx::PgPool;

    use db::queries::classification as cdb;
    use db::queries::users as users_db;

    pub const USAGE: &str = "\
usage:
  worker classify report [--top N]
      status distribution, ledger state, and the N users (default 20)
      with the most suspect scrobbles
  worker classify reclassify [--user USERNAME | --user-id ID] [--from RFC3339] [--to RFC3339]
      re-queue matching user-days at manual priority (whole UTC days)
  worker classify backfill
      queue every user-day with scrobbles at background priority";

    #[derive(Debug, PartialEq)]
    pub enum UserSelector {
        /// Case-insensitive, like every username lookup.
        Username(String),
        Id(i64),
    }

    #[derive(Debug, PartialEq)]
    pub enum Command {
        Report {
            top: i64,
        },
        Reclassify {
            user: Option<UserSelector>,
            from: Option<DateTime<Utc>>,
            to: Option<DateTime<Utc>>,
        },
        Backfill,
    }

    /// Parses the arguments after `classify`. Usernames may be all digits
    /// (registration allows it), so a username and a user id are separate
    /// flags rather than one guessed `--user <name|id>`.
    pub fn parse(args: &[String]) -> Result<Command, String> {
        let (sub, rest) = args.split_first().ok_or("missing subcommand")?;
        // Every flag takes exactly one value.
        if rest.len() % 2 == 1 {
            return Err(format!("{} needs a value", rest[rest.len() - 1]));
        }
        let pairs: Vec<(&str, String)> = rest
            .chunks(2)
            .map(|pair| (pair[0].as_str(), pair[1].clone()))
            .collect();

        match sub.as_str() {
            "report" => {
                let mut top = 20;
                for (flag, v) in pairs {
                    match flag {
                        "--top" => {
                            top = v.parse().ok().filter(|n: &i64| *n > 0).ok_or_else(|| {
                                format!("--top must be a positive integer, got {v:?}")
                            })?
                        }
                        other => return Err(format!("unknown flag for report: {other}")),
                    }
                }
                Ok(Command::Report { top })
            }
            "reclassify" => {
                let (mut user, mut from, mut to) = (None, None, None);
                for (flag, v) in pairs {
                    match flag {
                        "--user" | "--user-id" if user.is_some() => {
                            return Err("pass only one of --user / --user-id".into());
                        }
                        "--user" => user = Some(UserSelector::Username(v)),
                        "--user-id" => {
                            let id = v
                                .parse()
                                .map_err(|_| format!("--user-id must be a number, got {v:?}"))?;
                            user = Some(UserSelector::Id(id));
                        }
                        "--from" => from = Some(parse_time("--from", &v)?),
                        "--to" => to = Some(parse_time("--to", &v)?),
                        other => return Err(format!("unknown flag for reclassify: {other}")),
                    }
                }
                if let (Some(f), Some(t)) = (from, to)
                    && f >= t
                {
                    return Err("--from must be before --to".into());
                }
                Ok(Command::Reclassify { user, from, to })
            }
            "backfill" if pairs.is_empty() => Ok(Command::Backfill),
            "backfill" => Err("backfill takes no flags".into()),
            other => Err(format!("unknown subcommand: {other}")),
        }
    }

    fn parse_time(flag: &str, v: &str) -> Result<DateTime<Utc>, String> {
        DateTime::parse_from_rfc3339(v)
            .map(|t| t.with_timezone(&Utc))
            .map_err(|_| format!("{flag} must be an RFC 3339 timestamp, got {v:?}"))
    }

    /// Entry point for `worker classify …`.
    pub async fn run(db: &PgPool, args: &[String]) -> anyhow::Result<()> {
        let command = parse(args).map_err(|e| anyhow::anyhow!("{e}\n\n{USAGE}"))?;
        match command {
            Command::Report { top } => report(db, top).await,
            Command::Reclassify { user, from, to } => {
                let user_id = match user {
                    None => None,
                    Some(UserSelector::Id(id)) => {
                        let exists = users_db::find_by_id(db, id).await?.is_some();
                        anyhow::ensure!(exists, "no user with id {id}");
                        Some(id)
                    }
                    Some(UserSelector::Username(name)) => Some(
                        users_db::find_by_username(db, &name)
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("no user named {name:?}"))?
                            .id,
                    ),
                };
                let days = cdb::enqueue_range(db, user_id, from, to, cdb::PRIORITY_MANUAL).await?;
                println!(
                    "marked {days} user-days for reclassification (manual priority); \
                     the running worker picks them up"
                );
                Ok(())
            }
            Command::Backfill => {
                let days =
                    cdb::enqueue_range(db, None, None, None, cdb::PRIORITY_BACKGROUND).await?;
                println!(
                    "marked {days} user-days for classification (background priority); \
                     the running worker picks them up"
                );
                Ok(())
            }
        }
    }

    async fn report(db: &PgPool, top: i64) -> anyhow::Result<()> {
        let active = cdb::active_ruleset(db).await?;
        match &active {
            Some(r) => println!(
                "active ruleset: #{} (rules v{}) {}",
                r.id, r.rules_version, r.params
            ),
            None => println!("active ruleset: none (no worker has started the classifier yet)"),
        }

        let ledger = cdb::ledger_summary(db, active.as_ref().map(|r| r.id)).await?;
        println!(
            "user-days: {} current, {} outdated, {} waiting (dirty), {} with errors",
            ledger.current, ledger.outdated, ledger.dirty, ledger.erroring
        );

        println!("\nlabels by ruleset / status / reason:");
        let distribution = cdb::status_distribution(db).await?;
        if distribution.is_empty() {
            println!("  (none yet)");
        }
        for row in &distribution {
            println!(
                "  #{:<6} {:<8} {:<22} {:>12}",
                row.ruleset_id, row.status, row.reason, row.count
            );
        }

        println!("\ntop {top} users by suspect scrobbles:");
        let suspects = cdb::top_suspects(db, top).await?;
        if suspects.is_empty() {
            println!("  (none)");
        } else {
            println!(
                "  {:>8}  {:<24} {:<7} {:>9} {:>9} {:>9} {:>9}",
                "user_id", "username", "private", "suspect", "total", "suspect%", "max_score"
            );
        }
        for s in &suspects {
            let pct = if s.total > 0 {
                100.0 * s.suspect as f64 / s.total as f64
            } else {
                0.0
            };
            println!(
                "  {:>8}  {:<24} {:<7} {:>9} {:>9} {:>8.1}% {:>9}",
                s.user_id,
                s.username,
                if s.is_private { "yes" } else { "no" },
                s.suspect,
                s.total,
                pct,
                s.max_score.map_or("-".into(), |m| format!("{m:.2}")),
            );
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn args(s: &str) -> Vec<String> {
            s.split_whitespace().map(String::from).collect()
        }

        #[test]
        fn parses_report_with_default_and_explicit_top() {
            assert_eq!(parse(&args("report")), Ok(Command::Report { top: 20 }));
            assert_eq!(
                parse(&args("report --top 5")),
                Ok(Command::Report { top: 5 })
            );
            assert!(parse(&args("report --top 0")).is_err());
            assert!(parse(&args("report --top")).is_err());
        }

        /// A numeric username and a user id must never be confused.
        #[test]
        fn user_flags_are_explicit_and_exclusive() {
            assert_eq!(
                parse(&args("reclassify --user 12345")),
                Ok(Command::Reclassify {
                    user: Some(UserSelector::Username("12345".into())),
                    from: None,
                    to: None
                })
            );
            assert_eq!(
                parse(&args("reclassify --user-id 12345")),
                Ok(Command::Reclassify {
                    user: Some(UserSelector::Id(12345)),
                    from: None,
                    to: None
                })
            );
            assert!(parse(&args("reclassify --user alice --user-id 3")).is_err());
            assert!(parse(&args("reclassify --user-id alice")).is_err());
        }

        #[test]
        fn parses_time_range_and_rejects_bad_input() {
            let parsed = parse(&args(
                "reclassify --from 2026-01-01T00:00:00Z --to 2026-02-01T00:00:00+02:00",
            ))
            .unwrap();
            let Command::Reclassify { from, to, .. } = parsed else {
                panic!("expected reclassify")
            };
            assert_eq!(from.unwrap().to_rfc3339(), "2026-01-01T00:00:00+00:00");
            assert_eq!(to.unwrap().to_rfc3339(), "2026-01-31T22:00:00+00:00");

            assert!(parse(&args("reclassify --from yesterday")).is_err());
            assert!(
                parse(&args(
                    "reclassify --from 2026-02-01T00:00:00Z --to 2026-01-01T00:00:00Z"
                ))
                .is_err()
            );
        }

        #[test]
        fn rejects_unknown_subcommands_and_flags() {
            assert_eq!(parse(&args("backfill")), Ok(Command::Backfill));
            assert!(parse(&args("backfill --user x")).is_err());
            assert!(parse(&args("reclassify --since 2026-01-01T00:00:00Z")).is_err());
            assert!(parse(&args("purge")).is_err());
            assert!(parse(&[]).is_err());
        }
    }
}
