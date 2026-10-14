//! Artist similarity, recommendations and taste neighbours, recomputed once
//! a day from the ranking weights and coordination factors. The computation
//! lives in `shared::similarity`, storage in `db::queries::similarity`.

pub mod cli;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use sqlx::PgPool;
use tokio::time::MissedTickBehavior;

use crate::heartbeat::Beat;
use crate::rankings::env_or;
use db::queries::similarity::{self as sdb, Loaded, Run, Source, Window};
use shared::similarity::{self as similarity, Input, Output, SimilarityParams, SimilaritySettings};

const TICK: Duration = Duration::from_secs(10 * 60);
const RETRY: TimeDelta = TimeDelta::minutes(30);

/// Params from `SIMILARITY_*`. Invalid values are an error, not a fallback.
/// Blank values count as unset.
pub fn params_from_env() -> anyhow::Result<SimilarityParams> {
    let d = SimilaritySettings::default();
    let settings = SimilaritySettings {
        window_days: env_or("SIMILARITY_WINDOW_DAYS", d.window_days)?,
        full_plays: env_or("SIMILARITY_FULL_PLAYS", d.full_plays)?,
        min_factor: env_or("SIMILARITY_MIN_FACTOR", d.min_factor)?,
        min_listeners: env_or("SIMILARITY_MIN_LISTENERS", d.min_listeners)?,
        min_support: env_or("SIMILARITY_MIN_SUPPORT", d.min_support)?,
        min_share: env_or("SIMILARITY_MIN_SHARE", d.min_share)?,
        shrink: env_or("SIMILARITY_SHRINK", d.shrink)?,
        max_cohesion: env_or("SIMILARITY_MAX_COHESION", d.max_cohesion)?,
        artist_neighbours: env_or("SIMILARITY_ARTIST_NEIGHBOURS", d.artist_neighbours)?,
        user_artists: env_or("SIMILARITY_USER_ARTISTS", d.user_artists)?,
        min_user_artists: env_or("SIMILARITY_MIN_USER_ARTISTS", d.min_user_artists)?,
        recommendations: env_or("SIMILARITY_RECOMMENDATIONS", d.recommendations)?,
        taste_neighbours: env_or("SIMILARITY_TASTE_NEIGHBOURS", d.taste_neighbours)?,
        min_shared: env_or("SIMILARITY_MIN_SHARED", d.min_shared)?,
        half_life_days: env_or("SIMILARITY_HALF_LIFE_DAYS", d.half_life_days)?,
        max_postings: env_or("SIMILARITY_MAX_POSTINGS", d.max_postings)?,
        refresh_hours: env_or("SIMILARITY_REFRESH_HOURS", d.refresh_hours)?,
    };
    SimilarityParams::new(&settings).map_err(|e| anyhow::anyhow!("similarity config: {e}"))
}

/// What a run read and made.
pub struct Outcome {
    pub window: Window,
    pub loaded: Loaded,
    pub output: Output,
    /// `None` for a dry run.
    pub run_id: Option<i32>,
    pub load: Duration,
    pub compute: Duration,
    pub store: Duration,
}

/// Computes on a blocking thread: seconds of CPU on a large population.
pub async fn compute(input: Input, params: &SimilarityParams) -> anyhow::Result<Output> {
    let params = params.clone();
    Ok(tokio::task::spawn_blocking(move || similarity::compute(&input, &params)).await?)
}

/// Loads, computes and, unless `dry_run`, stores.
pub async fn run_once(
    db: &PgPool,
    params: &SimilarityParams,
    today: NaiveDate,
    dry_run: bool,
) -> anyhow::Result<Outcome> {
    let started_at = Utc::now();
    let started = Instant::now();
    let window = Window::new(today, params);
    let coordination_run = db::queries::coordination::latest_run(db).await?;
    let (input, loaded) = sdb::load(db, &window, params, Source::Weights).await?;
    let load = started.elapsed();
    let computed = Instant::now();
    let output = compute(input, params).await?;
    let compute = computed.elapsed();
    let stored = Instant::now();
    let run_id = if dry_run {
        None
    } else {
        let took_ms = started.elapsed().as_millis().min(i32::MAX as u128) as i32;
        Some(
            sdb::store(
                db,
                params,
                &window,
                &output,
                coordination_run.map(|r| r.id),
                started_at,
                took_ms,
            )
            .await?,
        )
    };
    Ok(Outcome {
        window,
        loaded,
        output,
        run_id,
        load,
        compute,
        store: stored.elapsed(),
    })
}

fn due(latest: Option<&Run>, params: &SimilarityParams, now: DateTime<Utc>) -> bool {
    latest.is_none_or(|run| {
        run.invalidated
            || run.fingerprint != params.fingerprint()
            || now - run.finished_at >= TimeDelta::hours(params.refresh_hours)
    })
}

pub struct Refresher {
    db: PgPool,
    params: SimilarityParams,
}

impl Refresher {
    pub fn from_env(db: PgPool) -> anyhow::Result<Self> {
        let params = params_from_env()?;
        tracing::info!("similarity: {}", params.fingerprint());
        Ok(Self { db, params })
    }

    /// Recomputes when due; a failed run keeps the previous results and is
    /// retried after a pause. Once a UTC day, deletes expired skips.
    pub async fn run(self: Arc<Self>, beat: Beat) {
        let mut purged_on = None;
        let mut retry_at: Option<DateTime<Utc>> = None;
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let now = Utc::now();
            let mut failure = None;
            if purged_on != Some(now.date_naive()) {
                match sdb::purge_skips(&self.db).await {
                    Ok(n) => {
                        purged_on = Some(now.date_naive());
                        if n > 0 {
                            tracing::info!("similarity: deleted {n} expired skips");
                        }
                    }
                    Err(e) => {
                        tracing::error!("similarity: purging skips failed: {e}");
                        failure = Some(e.to_string());
                    }
                }
            }
            if retry_at.is_none_or(|at| now >= at) {
                match self.run_if_due(now).await {
                    Ok(()) => retry_at = None,
                    Err(e) => {
                        tracing::error!(
                            "similarity: run failed, keeping the previous results and retrying \
                             in {} min: {e}",
                            RETRY.num_minutes()
                        );
                        retry_at = Some(now + RETRY);
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

    async fn run_if_due(&self, now: DateTime<Utc>) -> anyhow::Result<()> {
        let latest = sdb::latest_run(&self.db).await?;
        if !due(latest.as_ref(), &self.params, now) {
            return Ok(());
        }
        let o = run_once(&self.db, &self.params, now.date_naive(), false).await?;
        tracing::info!(
            run = o.run_id,
            accounts = o.output.contributors,
            candidates = o.output.candidates,
            artists = o.output.artists.len(),
            recommended = o.output.recommendations.len(),
            neighboured = o.output.neighbours.len(),
            credits = o.loaded.credits,
            history_rows = o.loaded.history_rows,
            "similarity: run over {} .. {} (load {} ms, compute {} ms, store {} ms)",
            o.window.from,
            o.window.to,
            o.load.as_millis(),
            o.compute.as_millis(),
            o.store.as_millis()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(params: &SimilarityParams, finished_at: DateTime<Utc>) -> Run {
        Run {
            id: 1,
            fingerprint: params.fingerprint(),
            window_from: finished_at.date_naive() - TimeDelta::days(179),
            window_to: finished_at.date_naive(),
            coordination_run_id: None,
            accounts: 0,
            candidates: 0,
            artists: 0,
            neighbours: 0,
            recommended_users: 0,
            neighboured_users: 0,
            refresh_secs: 86_400,
            invalidated: false,
            started_at: finished_at,
            took_ms: 1,
            finished_at,
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
    fn a_run_is_due_when_missing_old_invalidated_or_made_with_other_params() {
        let p = SimilarityParams::default();
        assert!(due(None, &p, at(9, 10)));
        let last = run(&p, at(9, 10));
        assert!(!due(Some(&last), &p, at(10, 9)));
        assert!(due(Some(&last), &p, at(10, 10)));
        let other = SimilarityParams {
            min_share: 50,
            ..Default::default()
        };
        assert!(due(Some(&last), &other, at(9, 11)));
        let erased = Run {
            invalidated: true,
            ..last
        };
        assert!(due(Some(&erased), &p, at(9, 11)));
    }
}
