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

/// Database integration tests — `#[ignore]`d so `cargo test` keeps needing
/// no database. Run them against a scratch database with migrations
/// 0001–0008 applied, serially:
///
/// ```text
/// createdb scrobblr_test   # then psql scrobblr_test -f migrations/000N_*.sql for N = 1..7
/// DATABASE_URL=postgresql://localhost:5432/scrobblr_test \
///     cargo test -p worker -- --ignored --test-threads=1
/// ```
///
/// Each test creates its own users (deleted at the end), but some flip
/// global state — the active ruleset, temporary `CHECK (false)` constraints
/// that make a table reject writes — so never point them at a database a
/// live worker uses, and never run them in parallel.
#[cfg(test)]
mod db_tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use db::queries::scrobbles::{self as scrobbles_db, IngestError, InsertScrobble};
    use db::queries::tracks as tracks_db;
    use shared::scrobble::ScrobbleInput;

    type Label = (i64, String, String, i32, Option<f32>, DateTime<Utc>);

    async fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL (scratch DB) required");
        db::pool::connect(&url).await.expect("connect")
    }

    fn unique(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
    }

    async fn new_user(pool: &PgPool) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO users (username, email, password_hash) VALUES ($1, $2, 'x') RETURNING id",
        )
        .bind(unique("clf"))
        .bind(format!("{}@test.invalid", unique("clf")))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Deleting the users cascades to their scrobbles, labels and ledger rows.
    async fn drop_users(pool: &PgPool, ids: &[i64]) {
        sqlx::query("DELETE FROM users WHERE id = ANY($1)")
            .bind(ids)
            .execute(pool)
            .await
            .unwrap();
    }

    /// A fresh artist + track; returns `(artist_id, track_id, artist, title)`.
    async fn new_track(pool: &PgPool, duration_ms: Option<i32>) -> (i64, i64, String, String) {
        let (artist_name, title) = (unique("artist"), unique("track"));
        let artist = tracks_db::find_or_create_artist(pool, &artist_name)
            .await
            .unwrap();
        let track = tracks_db::find_or_create_track(pool, artist.id, None, &title, duration_ms)
            .await
            .unwrap();
        (artist.id, track.id, artist_name, title)
    }

    /// Inserts a scrobble directly, bypassing ingest (and its dirty mark).
    async fn insert(
        pool: &PgPool,
        user_id: i64,
        (artist_id, track_id): (i64, i64),
        played_at: DateTime<Utc>,
        source: &str,
    ) -> i64 {
        scrobbles_db::insert_scrobble(
            pool,
            &InsertScrobble {
                user_id,
                track_id,
                artist_id,
                album_id: None,
                played_at,
                source: source.into(),
                duration_ms: None,
            },
        )
        .await
        .unwrap()
    }

    fn day(d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, d, 0, 0, 0).unwrap()
    }

    fn at(d: u32, h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, d, h, m, 0).unwrap()
    }

    async fn classifier(pool: &PgPool, rule: TimeBudgetConfig) -> Classifier {
        Classifier::start(
            pool.clone(),
            Settings {
                rule,
                settle_secs: 0.0,
            },
        )
        .await
        .unwrap()
    }

    /// Processes every due day (including leftovers from earlier runs).
    async fn drain(c: &Classifier) {
        while c.tick().await.unwrap() > 0 {}
    }

    async fn labels(pool: &PgPool, user_id: i64) -> Vec<Label> {
        sqlx::query_as(
            "SELECT scrobble_id, status, reason, ruleset_id, score, classified_at
             FROM scrobble_classifications WHERE user_id = $1 ORDER BY played_at, scrobble_id",
        )
        .bind(user_id)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    fn count(labels: &[Label], status: &str) -> usize {
        labels.iter().filter(|l| l.1 == status).count()
    }

    /// `(day, dirty)` for every ledger row of a user.
    async fn ledger(pool: &PgPool, user_id: i64) -> Vec<(DateTime<Utc>, bool)> {
        sqlx::query_as("SELECT day, dirty FROM classification_days WHERE user_id = $1 ORDER BY day")
            .bind(user_id)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn reject_writes(pool: &PgPool, table: &str) {
        sqlx::query(&format!(
            "ALTER TABLE {table} ADD CONSTRAINT test_reject_writes CHECK (false) NOT VALID"
        ))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn allow_writes(pool: &PgPool, table: &str) {
        sqlx::query(&format!(
            "ALTER TABLE {table} DROP CONSTRAINT test_reject_writes"
        ))
        .execute(pool)
        .await
        .unwrap();
    }

    fn input(artist: &str, title: &str, played_at: DateTime<Utc>) -> ScrobbleInput {
        ScrobbleInput {
            track_title: title.into(),
            artist_name: artist.into(),
            featured_artists: Vec::new(),
            album_title: None,
            played_at,
            duration_ms: Some(200_000),
            listened_ms: Some(200_000),
            source: "extension".into(),
        }
    }

    // ---- Regression contract: ingest must never reject a scrobble ----------

    /// A broken ledger must not cost a scrobble: ingest still returns the id,
    /// the row exists and the counter trigger ran.
    #[tokio::test]
    #[ignore]
    async fn ingest_survives_ledger_write_failure() {
        let pool = pool().await;
        let user = new_user(&pool).await;
        let (_, _, artist, title) = new_track(&pool, Some(200_000)).await;

        reject_writes(&pool, "classification_days").await;
        let result =
            scrobbles_db::ingest_scrobble(&pool, user, &input(&artist, &title, at(14, 12, 0)))
                .await;
        allow_writes(&pool, "classification_days").await;

        let id = result.expect("ingest must succeed when the ledger rejects writes");
        let (rows, counter): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM scrobbles WHERE id = $1),
                    (SELECT scrobble_count FROM users WHERE id = $2)",
        )
        .bind(id)
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((rows, counter), (1, 1));
        assert!(ledger(&pool, user).await.is_empty());
        drop_users(&pool, &[user]).await;
    }

    /// The tracks trigger runs inside the track upsert at ingest; if its
    /// queue rejects the insert, the duration fill must still succeed.
    #[tokio::test]
    #[ignore]
    async fn track_duration_fill_survives_refresh_queue_failure() {
        let pool = pool().await;
        let (artist_id, track_id, _, title) = new_track(&pool, None).await;

        reject_writes(&pool, "classification_track_refresh").await;
        let result =
            tracks_db::find_or_create_track(&pool, artist_id, None, &title, Some(180_000)).await;
        allow_writes(&pool, "classification_track_refresh").await;

        assert_eq!(
            result.expect("track upsert must succeed").duration_ms,
            Some(180_000)
        );
        let queued: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM classification_track_refresh WHERE track_id = $1",
        )
        .bind(track_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(queued, 0);
    }

    /// Dedup and its error are unchanged, and a rejected duplicate writes no
    /// ledger mark.
    #[tokio::test]
    #[ignore]
    async fn duplicate_is_still_rejected_without_a_ledger_write() {
        let pool = pool().await;
        let user = new_user(&pool).await;
        let (_, _, artist, title) = new_track(&pool, Some(200_000)).await;

        scrobbles_db::ingest_scrobble(&pool, user, &input(&artist, &title, at(14, 12, 0)))
            .await
            .unwrap();
        let gen_before: i64 = sqlx::query_scalar(
            "SELECT SUM(dirty_gen)::bigint FROM classification_days WHERE user_id = $1",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();

        let dup = scrobbles_db::ingest_scrobble(
            &pool,
            user,
            &input(&artist, &title, at(14, 12, 0) + ChronoDuration::seconds(10)),
        )
        .await;
        assert!(matches!(dup, Err(IngestError::Duplicate)));

        let gen_after: i64 = sqlx::query_scalar(
            "SELECT SUM(dirty_gen)::bigint FROM classification_days WHERE user_id = $1",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(gen_before, gen_after);
        drop_users(&pool, &[user]).await;
    }

    // ---- Ledger marking --------------------------------------------------

    /// The hook marks the next day only when the scrobble is within one
    /// window of midnight, reading the window from the active ruleset; with
    /// no active ruleset it always marks the next day.
    #[tokio::test]
    #[ignore]
    async fn mark_dirty_marks_next_day_only_near_midnight() {
        let pool = pool().await;
        classifier(&pool, TimeBudgetConfig::default()).await; // W = 1h active
        let (late, noon, fallback) = (
            new_user(&pool).await,
            new_user(&pool).await,
            new_user(&pool).await,
        );

        cdb::mark_dirty(&pool, late, at(14, 23, 30)).await.unwrap();
        cdb::mark_dirty(&pool, noon, at(14, 12, 0)).await.unwrap();

        let active: Vec<(i32, DateTime<Utc>)> = sqlx::query_as(
            "SELECT id, activated_at FROM classification_rulesets WHERE activated_at IS NOT NULL",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE classification_rulesets SET activated_at = NULL")
            .execute(&pool)
            .await
            .unwrap();
        let result = cdb::mark_dirty(&pool, fallback, at(14, 12, 0)).await;
        for (id, activated_at) in &active {
            sqlx::query("UPDATE classification_rulesets SET activated_at = $2 WHERE id = $1")
                .bind(id)
                .bind(activated_at)
                .execute(&pool)
                .await
                .unwrap();
        }
        result.unwrap();

        let days =
            |rows: Vec<(DateTime<Utc>, bool)>| rows.into_iter().map(|r| r.0).collect::<Vec<_>>();
        assert_eq!(days(ledger(&pool, late).await), vec![day(14), day(15)]);
        assert_eq!(days(ledger(&pool, noon).await), vec![day(14)]);
        assert_eq!(days(ledger(&pool, fallback).await), vec![day(14), day(15)]);
        drop_users(&pool, &[late, noon, fallback]).await;
    }

    // ---- Classification ----------------------------------------------------

    /// A normal evening plus a bot burst; classifying the day twice leaves
    /// every label — including classified_at — untouched the second time.
    #[tokio::test]
    #[ignore]
    async fn classifying_twice_is_identical() {
        let pool = pool().await;
        let c = classifier(&pool, TimeBudgetConfig::default()).await;
        drain(&c).await;
        let user = new_user(&pool).await;
        let (a, t, _, _) = new_track(&pool, Some(240_000)).await;
        for k in 0..10 {
            insert(
                &pool,
                user,
                (a, t),
                at(14, 18, 0) + ChronoDuration::minutes(4 * k),
                "extension",
            )
            .await;
        }
        for k in 0..400 {
            insert(
                &pool,
                user,
                (a, t),
                at(14, 21, 0) + ChronoDuration::seconds(5 * k),
                "bot",
            )
            .await;
        }
        cdb::enqueue_range(&pool, Some(user), None, None, cdb::PRIORITY_MANUAL)
            .await
            .unwrap();
        drain(&c).await;
        let first = labels(&pool, user).await;
        assert_eq!(first.len(), 410);
        assert!(count(&first, "counted") >= 10 && count(&first, "suspect") > 300);

        cdb::enqueue_range(&pool, Some(user), None, None, cdb::PRIORITY_MANUAL)
            .await
            .unwrap();
        drain(&c).await;
        assert_eq!(first, labels(&pool, user).await);
        drop_users(&pool, &[user]).await;
    }

    /// A mark landing while the day is processed keeps it dirty, and it
    /// becomes claimable only after the settle delay again (R3).
    #[tokio::test]
    #[ignore]
    async fn re_marked_day_waits_out_settle_again() {
        let pool = pool().await;
        let c = classifier(&pool, TimeBudgetConfig::default()).await;
        drain(&c).await;
        let user = new_user(&pool).await;
        let (a, t, _, _) = new_track(&pool, Some(200_000)).await;
        insert(&pool, user, (a, t), at(14, 12, 0), "extension").await;
        cdb::mark_dirty(&pool, user, at(14, 12, 0)).await.unwrap();

        let claimed = cdb::claim_dirty_days(&pool, 10, 0.0, CLAIM_LEASE_SECS)
            .await
            .unwrap();
        let ours = claimed.iter().find(|d| d.user_id == user).unwrap().clone();
        cdb::mark_dirty(&pool, user, at(14, 12, 5)).await.unwrap(); // arrives mid-flight
        c.classify_day(&ours).await.unwrap();

        assert_eq!(ledger(&pool, user).await, vec![(day(14), true)]);
        let with_settle = cdb::claim_dirty_days(&pool, 100, 60.0, CLAIM_LEASE_SECS)
            .await
            .unwrap();
        assert!(!with_settle.iter().any(|d| d.user_id == user));
        let without = cdb::claim_dirty_days(&pool, 100, 0.0, CLAIM_LEASE_SECS)
            .await
            .unwrap();
        assert!(without.iter().any(|d| d.user_id == user));
        drop_users(&pool, &[user]).await;
    }

    /// Reconciliation finds a recent day the ingest hook never marked, and
    /// leaves a correctly classified day alone (R2).
    #[tokio::test]
    #[ignore]
    async fn reconciliation_requeues_unmarked_recent_days() {
        let pool = pool().await;
        let c = classifier(&pool, TimeBudgetConfig::default()).await;
        drain(&c).await;
        let user = new_user(&pool).await;
        let (a, t, _, _) = new_track(&pool, Some(200_000)).await;
        insert(
            &pool,
            user,
            (a, t),
            Utc::now() - ChronoDuration::days(1),
            "extension",
        )
        .await;
        assert!(ledger(&pool, user).await.is_empty());

        cdb::reconcile_recent(&pool, RECONCILE_LOOKBACK_DAYS)
            .await
            .unwrap();
        assert!(ledger(&pool, user).await.iter().all(|(_, dirty)| *dirty));

        drain(&c).await;
        cdb::reconcile_recent(&pool, RECONCILE_LOOKBACK_DAYS)
            .await
            .unwrap();
        assert!(ledger(&pool, user).await.iter().all(|(_, dirty)| !*dirty));
        assert_eq!(labels(&pool, user).await.len(), 1);
        drop_users(&pool, &[user]).await;
    }

    /// A track gaining a duration re-checks its no_data labels: the trigger
    /// queues it (NULL→value only), the drain waits for the lease, then
    /// re-dirties the user-day found through the labels' track_id (R1).
    #[tokio::test]
    #[ignore]
    async fn track_duration_fill_rechecks_no_data_labels() {
        let pool = pool().await;
        let c = classifier(&pool, TimeBudgetConfig::default()).await;
        drain(&c).await;
        let user = new_user(&pool).await;
        let (a, t, _, title) = new_track(&pool, None).await;
        for k in 0..3 {
            insert(&pool, user, (a, t), at(14, 12, 4 * k), "extension").await;
        }
        cdb::mark_dirty(&pool, user, at(14, 12, 0)).await.unwrap();
        drain(&c).await;
        assert_eq!(count(&labels(&pool, user).await, "no_data"), 3);

        tracks_db::find_or_create_track(&pool, a, None, &title, Some(240_000))
            .await
            .unwrap();
        let queued = |pool: PgPool| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM classification_track_refresh WHERE track_id = $1",
            )
            .bind(t)
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        assert_eq!(queued(pool.clone()).await, 1);

        // Too fresh for a drain that respects the lease.
        cdb::drain_track_refresh(&pool, CLAIM_LEASE_SECS, 1_000)
            .await
            .unwrap();
        assert_eq!(queued(pool.clone()).await, 1);

        cdb::drain_track_refresh(&pool, 0.0, 1_000).await.unwrap();
        assert_eq!(queued(pool.clone()).await, 0);
        assert_eq!(ledger(&pool, user).await.first(), Some(&(day(14), true)));
        drain(&c).await;
        assert_eq!(count(&labels(&pool, user).await, "counted"), 3);

        // value → value is not a fill: no new queue entry.
        sqlx::query("UPDATE tracks SET duration_ms = 250000 WHERE id = $1")
            .bind(t)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(queued(pool.clone()).await, 0);
        drop_users(&pool, &[user]).await;
    }

    /// Changing a threshold creates a new ruleset; starting with it re-queues
    /// and relabels days classified under the old one.
    #[tokio::test]
    #[ignore]
    async fn ruleset_change_relabels_history() {
        let pool = pool().await;
        let lenient = classifier(&pool, TimeBudgetConfig::default()).await;
        drain(&lenient).await;
        let user = new_user(&pool).await;
        let (a, t, _, _) = new_track(&pool, Some(240_000)).await;
        // A second device playing *different* music (the same track from
        // another source would be paired away as a double-scrobble).
        let (a2, t2, _, _) = new_track(&pool, Some(240_000)).await;
        // ~1.5× wall-clock time for an hour: counted under the 2.0 ratio.
        for k in 0..15 {
            insert(
                &pool,
                user,
                (a, t),
                at(14, 18, 0) + ChronoDuration::minutes(4 * k),
                "extension",
            )
            .await;
        }
        for k in 0..8 {
            insert(
                &pool,
                user,
                (a2, t2),
                at(14, 18, 2) + ChronoDuration::minutes(7 * k),
                "mobile",
            )
            .await;
        }
        cdb::enqueue_range(&pool, Some(user), None, None, cdb::PRIORITY_MANUAL)
            .await
            .unwrap();
        drain(&lenient).await;
        assert_eq!(count(&labels(&pool, user).await, "suspect"), 0);

        let strict_rule = TimeBudgetConfig {
            margin_ratio: 1.0,
            margin_slack_secs: 0,
            ..Default::default()
        };
        let strict = classifier(&pool, strict_rule).await;
        assert_ne!(strict.ruleset_id, lenient.ruleset_id);
        assert_eq!(ledger(&pool, user).await, vec![(day(14), true)]);
        drain(&strict).await;
        let relabeled = labels(&pool, user).await;
        assert!(count(&relabeled, "suspect") > 0);
        assert!(relabeled.iter().all(|l| l.3 == strict.ruleset_id));

        classifier(&pool, TimeBudgetConfig::default()).await; // restore the default as active
        drop_users(&pool, &[user]).await;
    }

    // ---- Queue mechanics (G4–G8) ------------------------------------------

    /// G4. Value: protects=a day claimed by a crashed worker is reclaimable
    /// after the lease; fails_when=the claim query loses its lease clause.
    #[tokio::test]
    #[ignore]
    async fn expired_claim_lease_is_reclaimable() {
        let pool = pool().await;
        let c = classifier(&pool, TimeBudgetConfig::default()).await;
        drain(&c).await;
        let user = new_user(&pool).await;
        cdb::mark_dirty(&pool, user, at(14, 12, 0)).await.unwrap();

        let first = cdb::claim_dirty_days(&pool, 10, 0.0, CLAIM_LEASE_SECS)
            .await
            .unwrap();
        assert!(first.iter().any(|d| d.user_id == user));
        let again = cdb::claim_dirty_days(&pool, 10, 0.0, CLAIM_LEASE_SECS)
            .await
            .unwrap();
        assert!(!again.iter().any(|d| d.user_id == user));

        sqlx::query("UPDATE classification_days SET claimed_at = NOW() - INTERVAL '1 hour' WHERE user_id = $1")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        let reclaimed = cdb::claim_dirty_days(&pool, 10, 0.0, CLAIM_LEASE_SECS)
            .await
            .unwrap();
        assert!(reclaimed.iter().any(|d| d.user_id == user));
        drop_users(&pool, &[user]).await;
    }

    /// G5. Value: protects=processing day D never deletes day D−1 labels
    /// loaded as lookback; fails_when=the orphan delete uses the load range.
    #[tokio::test]
    #[ignore]
    async fn processing_a_day_keeps_the_previous_days_labels() {
        let pool = pool().await;
        let c = classifier(&pool, TimeBudgetConfig::default()).await;
        drain(&c).await;
        let user = new_user(&pool).await;
        let (a, t, _, _) = new_track(&pool, Some(200_000)).await;
        let late = insert(&pool, user, (a, t), at(13, 23, 50), "extension").await;
        insert(&pool, user, (a, t), at(14, 0, 10), "extension").await;
        cdb::enqueue_range(&pool, Some(user), None, None, cdb::PRIORITY_MANUAL)
            .await
            .unwrap();
        drain(&c).await;
        assert_eq!(labels(&pool, user).await.len(), 2);

        cdb::enqueue_range(
            &pool,
            Some(user),
            Some(day(14)),
            Some(day(15)),
            cdb::PRIORITY_MANUAL,
        )
        .await
        .unwrap();
        drain(&c).await;
        let after = labels(&pool, user).await;
        assert_eq!(after.len(), 2);
        assert!(after.iter().any(|l| l.0 == late));
        drop_users(&pool, &[user]).await;
    }

    /// G6. Value: protects=a failing day gets attempts+1, the error and a
    /// future retry; fails_when=reschedule leaves it due → hot retry loop.
    #[tokio::test]
    #[ignore]
    async fn failed_day_is_rescheduled_with_backoff() {
        let pool = pool().await;
        let c = classifier(&pool, TimeBudgetConfig::default()).await;
        drain(&c).await;
        let user = new_user(&pool).await;
        let (a, t, _, _) = new_track(&pool, Some(200_000)).await;
        insert(&pool, user, (a, t), at(14, 12, 0), "extension").await;
        cdb::mark_dirty(&pool, user, at(14, 12, 0)).await.unwrap();

        reject_writes(&pool, "scrobble_classifications").await;
        let claimed = cdb::claim_dirty_days(&pool, 10, 0.0, CLAIM_LEASE_SECS).await;
        if let Ok(days) = &claimed {
            for d in days.iter().filter(|d| d.user_id == user) {
                c.process(d).await;
            }
        }
        allow_writes(&pool, "scrobble_classifications").await;
        assert!(claimed.unwrap().iter().any(|d| d.user_id == user));

        let (attempts, error, backed_off): (i32, Option<String>, bool) = sqlx::query_as(
            "SELECT attempts, last_error, next_attempt_at > NOW() + INTERVAL '30 seconds'
             FROM classification_days WHERE user_id = $1",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(attempts, 1);
        assert!(error.unwrap().contains("test_reject_writes"));
        assert!(backed_off);
        let due = cdb::claim_dirty_days(&pool, 100, 0.0, CLAIM_LEASE_SECS)
            .await
            .unwrap();
        assert!(!due.iter().any(|d| d.user_id == user));
        drop_users(&pool, &[user]).await;
    }

    /// G7. Value: protects=reclassify --user/--from/--to dirties exactly the
    /// matching user-days; fails_when=a dropped filter dirties all history.
    #[tokio::test]
    #[ignore]
    async fn reclassify_range_marks_only_matching_user_days() {
        let pool = pool().await;
        let c = classifier(&pool, TimeBudgetConfig::default()).await;
        drain(&c).await;
        let (u1, u2) = (new_user(&pool).await, new_user(&pool).await);
        let (a, t, _, _) = new_track(&pool, Some(200_000)).await;
        for u in [u1, u2] {
            for d in [13, 14, 15] {
                insert(&pool, u, (a, t), at(d, 12, 0), "extension").await;
            }
        }
        cdb::enqueue_range(&pool, Some(u1), None, None, cdb::PRIORITY_MANUAL)
            .await
            .unwrap();
        cdb::enqueue_range(&pool, Some(u2), None, None, cdb::PRIORITY_MANUAL)
            .await
            .unwrap();
        drain(&c).await;

        let marked = cdb::enqueue_range(
            &pool,
            Some(u1),
            Some(day(14)),
            Some(day(15)),
            cdb::PRIORITY_MANUAL,
        )
        .await
        .unwrap();
        assert_eq!(marked, 1);
        assert_eq!(
            ledger(&pool, u1).await,
            vec![(day(13), false), (day(14), true), (day(15), false)]
        );
        assert!(ledger(&pool, u2).await.iter().all(|(_, dirty)| !*dirty));

        cdb::enqueue_range(&pool, Some(u1), None, None, cdb::PRIORITY_MANUAL)
            .await
            .unwrap();
        assert!(ledger(&pool, u1).await.iter().all(|(_, dirty)| *dirty));
        assert!(ledger(&pool, u2).await.iter().all(|(_, dirty)| !*dirty));
        drop_users(&pool, &[u1, u2]).await;
    }

    /// G8. Value: protects=the report's distribution and top-N match the
    /// seeded labels; fails_when=a join/grouping error misleads the review.
    #[tokio::test]
    #[ignore]
    async fn report_matches_seeded_labels() {
        let pool = pool().await;
        let c = classifier(&pool, TimeBudgetConfig::default()).await;
        drain(&c).await;
        let totals = |rows: Vec<cdb::StatusCount>| {
            let mut by_status: HashMap<String, i64> = HashMap::new();
            for r in rows {
                *by_status.entry(r.status).or_default() += r.count;
            }
            by_status
        };
        let before = totals(cdb::status_distribution(&pool).await.unwrap());

        let user = new_user(&pool).await;
        let (a, t, _, _) = new_track(&pool, Some(200_000)).await;
        let (na, nt, _, _) = new_track(&pool, None).await;
        for k in 0..300 {
            insert(
                &pool,
                user,
                (a, t),
                at(14, 20, 0) + ChronoDuration::seconds(10 * k),
                "bot",
            )
            .await;
        }
        for k in 0..5 {
            insert(
                &pool,
                user,
                (na, nt),
                at(14, 8, 0) + ChronoDuration::minutes(5 * k),
                "extension",
            )
            .await;
        }
        cdb::enqueue_range(&pool, Some(user), None, None, cdb::PRIORITY_MANUAL)
            .await
            .unwrap();
        drain(&c).await;

        let ours = labels(&pool, user).await;
        let (counted, suspect, no_data) = (
            count(&ours, "counted") as i64,
            count(&ours, "suspect") as i64,
            count(&ours, "no_data") as i64,
        );
        assert!(suspect > 0 && no_data == 5);

        let after = totals(cdb::status_distribution(&pool).await.unwrap());
        let delta =
            |s: &str| after.get(s).copied().unwrap_or(0) - before.get(s).copied().unwrap_or(0);
        assert_eq!(
            (delta("counted"), delta("suspect"), delta("no_data")),
            (counted, suspect, no_data)
        );

        let top = cdb::top_suspects(&pool, 10_000).await.unwrap();
        let row = top.iter().find(|r| r.user_id == user).expect("user listed");
        assert_eq!(
            (row.suspect, row.total),
            (suspect, counted + suspect + no_data)
        );
        assert!(row.max_score.unwrap() > 1.0);
        drop_users(&pool, &[user]).await;
    }
}
