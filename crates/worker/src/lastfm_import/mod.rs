//! Imports a user's Last.fm history (`db::queries::imports` for the job
//! model). A job is processed in slices of pages under a lease, so several
//! imports take turns and a crashed worker's job is picked up again; every
//! page commits together with the cursor, so nothing is fetched twice
//! beyond the page in flight.
//!
//! Downstream work runs at checkpoints (a segment boundary, the end of a
//! slice, the end of the job), never per scrobble: the aggregate refresh,
//! the classification queue, and enrichment for new catalog entries.

pub mod cli;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::PgPool;

use crate::enrichment::ratelimit::RateLimiter;
use db::queries::{
    classification as classification_db, connected_accounts as connected_accounts_db,
    imports as imports_db, scrobbles as scrobbles_db,
};
use imports_db::{ClaimedImport, ImportPlay, Page, PageOutcome};
use shared::lastfm::{
    ERROR_INVALID_PARAMETERS, ERROR_INVALID_SESSION, ERROR_LOGIN_REQUIRED, LastfmClient,
    LastfmError, RecentTracksQuery,
};

pub const LEASE_SECS: f64 = 180.0;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Pages per lease before the job goes back in line behind other imports.
pub const SLICE_PAGES: u32 = 25;
/// Consecutive transient failures (about four hours of backoff) before the
/// job fails.
const MAX_ATTEMPTS: i32 = 10;
const RATE_LIMIT_COOLDOWN: TimeDelta = TimeDelta::seconds(60);
const DEFAULT_MAX_SCROBBLES: i64 = 1_000_000;

pub const USER_NOT_FOUND: &str = "user_not_found";
pub const HISTORY_HIDDEN: &str = "history_hidden";
pub const CAP_REACHED: &str = "cap_reached";
pub const LASTFM_UNAVAILABLE: &str = "lastfm_unavailable";
pub const LASTFM_ERROR: &str = "lastfm_error";

/// How a slice of work ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliceEnd {
    Done,
    Failed(&'static str),
    /// Waiting out a Last.fm error until the given time.
    Deferred(DateTime<Utc>),
    /// Used up its pages; back in line.
    Paused,
    /// Cancelled, or another worker took over.
    LeaseLost,
}

pub struct Importer {
    db: PgPool,
    client: LastfmClient,
    limiter: Arc<RateLimiter>,
    max_scrobbles: i64,
    rate_limit_cooldown: TimeDelta,
}

impl Importer {
    pub fn new(
        db: PgPool,
        client: LastfmClient,
        limiter: Arc<RateLimiter>,
        max_scrobbles: i64,
    ) -> Self {
        Self {
            db,
            client,
            limiter,
            max_scrobbles,
            rate_limit_cooldown: RATE_LIMIT_COOLDOWN,
        }
    }

    #[cfg(test)]
    fn with_rate_limit_cooldown(mut self, cooldown: TimeDelta) -> Self {
        self.rate_limit_cooldown = cooldown;
        self
    }

    /// `None` when `LASTFM_API_KEY` is unset.
    pub fn from_env(
        db: PgPool,
        http: reqwest::Client,
        limiter: Arc<RateLimiter>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(client) = LastfmClient::from_env(http) else {
            return Ok(None);
        };
        let max_scrobbles = match crate::non_empty_env("LASTFM_IMPORT_MAX_SCROBBLES") {
            Some(v) => v
                .trim()
                .parse()
                .map_err(|e| anyhow::anyhow!("LASTFM_IMPORT_MAX_SCROBBLES={v}: {e}"))?,
            None => DEFAULT_MAX_SCROBBLES,
        };
        Ok(Some(Self::new(db, client, limiter, max_scrobbles)))
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            match imports_db::claim_next(&self.db, LEASE_SECS).await {
                Ok(Some(job)) => {
                    let id = job.id;
                    match self.process(job, SLICE_PAGES).await {
                        Ok(end) => tracing::debug!(import_id = id, "import: slice ended: {end:?}"),
                        // The lease expires and the job is retried.
                        Err(e) => tracing::error!(import_id = id, "import: database error: {e}"),
                    }
                }
                Ok(None) => {
                    self.checkpoint_leftovers().await;
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Err(e) => {
                    tracing::error!("import: failed to claim a job: {e}");
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    /// Fetches and records up to `max_pages` pages of a leased job, and
    /// compresses the history it wrote once the job ends.
    pub async fn process(
        &self,
        job: ClaimedImport,
        max_pages: u32,
    ) -> Result<SliceEnd, sqlx::Error> {
        let end = self.fetch_pages(job, max_pages).await?;
        if matches!(end, SliceEnd::Done | SliceEnd::Failed(_))
            && let Err(e) = scrobbles_db::compress_history(&self.db).await
        {
            tracing::warn!("import: compression skipped, the policy will catch up: {e}");
        }
        Ok(end)
    }

    async fn fetch_pages(
        &self,
        mut job: ClaimedImport,
        max_pages: u32,
    ) -> Result<SliceEnd, sqlx::Error> {
        let mut session_key = self.session_key(&job).await;
        let window_to = job.window_to.timestamp();
        let window_from = job.window_from.map(|t| t.timestamp());

        let mut pages = 0;
        while pages < max_pages {
            if job.imported_before + job.imported >= self.max_scrobbles {
                self.checkpoint(job.id, job.user_id).await;
                let message = format!(
                    "stopped at {} imported scrobbles for this user, the limit (LASTFM_IMPORT_MAX_SCROBBLES)",
                    job.imported_before + job.imported
                );
                imports_db::fail(&self.db, &job, CAP_REACHED, &message).await?;
                return Ok(SliceEnd::Failed(CAP_REACHED));
            }

            self.limiter.acquire().await;
            let query = RecentTracksQuery {
                user: &job.external_user,
                from: window_from,
                to: Some(job.cursor.segment_to),
                page: job.cursor.page,
                session_key: session_key.as_deref(),
            };
            let fetched = match self.client.recent_tracks(&query).await {
                Ok(fetched) => fetched,
                Err(e) if e.code() == Some(ERROR_INVALID_SESSION) && session_key.is_some() => {
                    tracing::warn!(
                        import_id = job.id,
                        "import: Last.fm session rejected, continuing unsigned"
                    );
                    session_key = None;
                    continue;
                }
                Err(e) => return self.handle_error(&job, e).await,
            };

            let next = job.cursor.advance(&fetched);
            let plays: Vec<ImportPlay> = fetched
                .plays
                .iter()
                .filter(|p| p.uts <= window_to && window_from.is_none_or(|from| p.uts > from))
                .filter_map(|p| {
                    Some(ImportPlay {
                        played_at: DateTime::from_timestamp(p.uts, 0)?,
                        artist: p.artist.clone(),
                        track: p.track.clone(),
                        album: p.album.clone(),
                        track_mbid: p.track_mbid,
                    })
                })
                .collect();
            let page = Page {
                plays: &plays,
                next: next.unwrap_or(job.cursor),
                fetched: fetched.dated() as i64,
                skipped: i64::from(fetched.invalid),
                total_expected: fetched.total as i64,
            };
            let PageOutcome::Recorded { imported, .. } =
                imports_db::record_page(&self.db, &job, &page, LEASE_SECS).await?
            else {
                self.checkpoint(job.id, job.user_id).await;
                return Ok(SliceEnd::LeaseLost);
            };
            job.imported += imported;
            pages += 1;

            let Some(next) = next else {
                self.checkpoint(job.id, job.user_id).await;
                imports_db::finish(&self.db, &job).await?;
                return Ok(SliceEnd::Done);
            };
            if next.segment_to != job.cursor.segment_to {
                self.checkpoint(job.id, job.user_id).await;
            }
            job.cursor = next;
        }

        self.checkpoint(job.id, job.user_id).await;
        imports_db::release(&self.db, &job).await?;
        Ok(SliceEnd::Paused)
    }

    async fn handle_error(
        &self,
        job: &ClaimedImport,
        error: LastfmError,
    ) -> Result<SliceEnd, sqlx::Error> {
        self.checkpoint(job.id, job.user_id).await;

        let failed = match error.code() {
            Some(ERROR_INVALID_PARAMETERS) => Some((
                USER_NOT_FOUND,
                format!("Last.fm has no user named {}", job.external_user),
            )),
            Some(ERROR_LOGIN_REQUIRED) => Some((
                HISTORY_HIDDEN,
                format!(
                    "{} hides their recent listening on Last.fm; make it public in Last.fm's privacy settings, or connect the account so the import can read it",
                    job.external_user
                ),
            )),
            _ if error.is_transient() => None,
            _ => Some((LASTFM_ERROR, error.to_string())),
        };
        if let Some((code, message)) = failed {
            imports_db::fail(&self.db, job, code, &message).await?;
            return Ok(SliceEnd::Failed(code));
        }

        // Rate limiting is Last.fm pacing us, not failing: no attempt spent.
        let (attempts, delay) = if error.is_rate_limited() {
            self.limiter
                .penalize(self.rate_limit_cooldown.to_std().unwrap_or_default())
                .await;
            (job.attempts, self.rate_limit_cooldown)
        } else {
            (job.attempts + 1, backoff(job.attempts + 1))
        };
        if attempts >= MAX_ATTEMPTS {
            let message = format!("gave up after {attempts} failed attempts: {error}");
            imports_db::fail(&self.db, job, LASTFM_UNAVAILABLE, &message).await?;
            return Ok(SliceEnd::Failed(LASTFM_UNAVAILABLE));
        }
        tracing::warn!(
            import_id = job.id,
            "import: {error}; retrying in {}s",
            delay.num_seconds()
        );
        imports_db::defer(&self.db, job, attempts, delay, &error.to_string()).await?;
        Ok(SliceEnd::Deferred(Utc::now() + delay))
    }

    /// The user's own Last.fm session, which can read a hidden history.
    /// Only for verified imports of the account that was connected.
    async fn session_key(&self, job: &ClaimedImport) -> Option<String> {
        if !job.verified || !self.client.can_sign() {
            return None;
        }
        match connected_accounts_db::find_account(
            &self.db,
            job.user_id,
            imports_db::PROVIDER_LASTFM,
        )
        .await
        {
            Ok(Some(account))
                if account
                    .provider_user_id
                    .eq_ignore_ascii_case(&job.external_user) =>
            {
                Some(account.access_token)
            }
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(
                    import_id = job.id,
                    "import: can't read the Last.fm session ({e}), fetching unsigned"
                );
                None
            }
        }
    }

    /// Hands what was imported since the last checkpoint to the aggregates,
    /// the classifier and enrichment. Idempotent; on failure the range stays
    /// pending for the next checkpoint.
    pub async fn checkpoint(&self, import_id: i64, user_id: i64) {
        let range = match imports_db::pending_range(&self.db, import_id).await {
            Ok(Some(range)) => range,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(import_id, "import: checkpoint skipped: {e}");
                return;
            }
        };
        let (from, to) = range;
        refresh_aggregates(&self.db, from, to).await;

        let queued = async {
            classification_db::enqueue_scrobble_classification(&self.db, user_id, from, to).await?;
            imports_db::enqueue_enrichment(&self.db, import_id, user_id, range).await?;
            imports_db::clear_pending(&self.db, import_id, range).await
        };
        if let Err(e) = queued.await {
            tracing::warn!(
                import_id,
                "import: checkpoint incomplete, retrying later: {e}"
            );
        }
    }

    async fn checkpoint_leftovers(&self) {
        match imports_db::unfinished_checkpoints(&self.db).await {
            Ok(jobs) => {
                for (import_id, user_id) in jobs {
                    self.checkpoint(import_id, user_id).await;
                }
            }
            Err(e) => tracing::warn!("import: leftover checkpoints: {e}"),
        }
    }
}

/// 30 s, doubling per attempt, at most an hour.
fn backoff(attempt: i32) -> TimeDelta {
    let secs = 30i64.saturating_mul(1 << (attempt - 1).clamp(0, 20));
    TimeDelta::seconds(secs.min(3600))
}

/// Best effort: a refresh that keeps colliding with the aggregates'
/// scheduled policy (which also catches up on these rows) is skipped.
async fn refresh_aggregates(db: &PgPool, from: DateTime<Utc>, to: DateTime<Utc>) {
    for attempt in 1..=5u32 {
        match scrobbles_db::refresh_scrobble_aggregates(db, from, to).await {
            Ok(()) => return,
            Err(e) if attempt < 5 && is_lock_not_available(&e) => {
                tokio::time::sleep(Duration::from_secs(2 * u64::from(attempt))).await;
            }
            Err(e) => {
                tracing::warn!(
                    "import: aggregate refresh skipped, the hourly policy will catch up: {e}"
                );
                return;
            }
        }
    }
}

fn is_lock_not_available(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|e| e.code())
        .is_some_and(|code| code == "55P03")
}
