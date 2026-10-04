//! Track lengths from Last.fm (`track.getInfo`) for tracks no source has a
//! length for: imported history carries none, and some clients send none,
//! which leaves their scrobbles `no_data` for the classifier. Most
//! scrobbled tracks first, at a pace that leaves room for imports. Fills
//! only the catalog value (`tracks.duration_ms`); MusicBrainz enrichment
//! still adds the more trusted `mb_duration_ms` on its own schedule.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::PgPool;

use super::ratelimit::RateLimiter;
use db::queries::enrichment as edb;
use shared::lastfm::LastfmClient;

const BATCH: i64 = 50;
const IDLE: Duration = Duration::from_secs(60);
/// On top of the shared Last.fm limiter: at most 2 lookups a second.
const PACE: Duration = Duration::from_millis(500);
const RECHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
const RECHECK_AFTER_DAYS: i32 = 30;
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);

pub struct LengthBackfill {
    db: PgPool,
    client: LastfmClient,
    limiter: Arc<RateLimiter>,
    pace: RateLimiter,
}

impl LengthBackfill {
    pub fn new(
        db: PgPool,
        client: LastfmClient,
        limiter: Arc<RateLimiter>,
        pace: Duration,
    ) -> Self {
        Self {
            db,
            client,
            limiter,
            pace: RateLimiter::new(pace),
        }
    }

    pub fn from_env(db: PgPool, http: reqwest::Client, limiter: Arc<RateLimiter>) -> Option<Self> {
        LastfmClient::from_env(http).map(|client| Self::new(db, client, limiter, PACE))
    }

    pub async fn run(self: Arc<Self>) {
        let mut last_recheck: Option<Instant> = None;
        let mut failures = 0u32;
        loop {
            if last_recheck.is_none_or(|t| t.elapsed() >= RECHECK_EVERY) {
                match edb::requeue_length_checks(&self.db, RECHECK_AFTER_DAYS).await {
                    Ok(n) if n > 0 => {
                        tracing::info!("lengths: asking Last.fm again about {n} tracks")
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("lengths: recheck failed: {e}"),
                }
                last_recheck = Some(Instant::now());
            }
            match self.step(BATCH).await {
                Ok(0) => tokio::time::sleep(IDLE).await,
                Ok(_) => failures = 0,
                Err(e) => {
                    failures += 1;
                    let delay = Duration::from_secs((30u64 << failures.min(6)).min(1800));
                    tracing::warn!("lengths: {e}; pausing {}s", delay.as_secs());
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Asks Last.fm about one batch and returns how many tracks it settled.
    /// A transient error ends the batch, leaving the rest unasked.
    pub async fn step(&self, limit: i64) -> anyhow::Result<usize> {
        let tracks = edb::tracks_missing_length(&self.db, limit).await?;
        for track in &tracks {
            self.pace.acquire().await;
            self.limiter.acquire().await;
            match self
                .client
                .track_duration(&track.artist_name, &track.title)
                .await
            {
                Ok(length) => edb::record_lastfm_length(&self.db, track.id, length).await?,
                Err(e) if e.is_rate_limited() => {
                    self.limiter.penalize(RATE_LIMIT_COOLDOWN).await;
                    return Err(e.into());
                }
                Err(e) if e.is_transient() => return Err(e.into()),
                Err(e) => {
                    tracing::debug!(track_id = track.id, "lengths: Last.fm gave no length: {e}");
                    edb::record_lastfm_length(&self.db, track.id, None).await?;
                }
            }
        }
        Ok(tracks.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_lastfm::{API_KEY, Failure, FakeLastfm};
    use crate::test_support::with_db;

    async fn track(
        pool: &PgPool,
        artist: &str,
        title: &str,
        plays: i64,
        duration: Option<i32>,
    ) -> i64 {
        let artist = db::queries::tracks::find_or_create_artist(pool, artist)
            .await
            .unwrap();
        let track =
            db::queries::tracks::find_or_create_track(pool, artist.id, None, title, duration)
                .await
                .unwrap();
        sqlx::query("UPDATE tracks SET scrobble_count = $2 WHERE id = $1")
            .bind(track.id)
            .bind(plays)
            .execute(pool)
            .await
            .unwrap();
        track.id
    }

    async fn length(pool: &PgPool, id: i64) -> (Option<i32>, bool) {
        sqlx::query_as(
            "SELECT duration_ms, lastfm_checked_at IS NOT NULL FROM tracks WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    #[ignore = "needs Postgres: just test-db"]
    async fn fills_the_most_played_lengths_first_and_remembers_misses() {
        with_db(|pool| async move {
            let fake = FakeLastfm::default();
            fake.length("Cher", "Believe", 239_000);
            fake.length("Cher", "Strong Enough", 223_000);
            let url = fake.start().await;
            let client = LastfmClient::new(reqwest::Client::new(), url, API_KEY, None);
            let backfill = LengthBackfill::new(
                pool.clone(),
                client,
                Arc::new(RateLimiter::new(Duration::ZERO)),
                Duration::ZERO,
            );

            let believe = track(&pool, "Cher", "Believe", 90, None).await;
            let strong = track(&pool, "Cher", "Strong Enough", 5, None).await;
            let unknown = track(&pool, "Nobody", "Demo", 40, None).await;
            let known = track(&pool, "Cher", "Known", 500, Some(200_000)).await;

            assert_eq!(backfill.step(2).await.unwrap(), 2);
            assert_eq!(length(&pool, believe).await, (Some(239_000), true));
            assert_eq!(length(&pool, unknown).await, (None, true));
            assert_eq!(length(&pool, strong).await, (None, false));

            fake.fail_next(&[Failure::Status(503)]);
            assert!(backfill.step(10).await.is_err());
            assert_eq!(length(&pool, strong).await, (None, false));
            assert_eq!(backfill.step(10).await.unwrap(), 1);
            assert_eq!(length(&pool, strong).await, (Some(223_000), true));
            assert_eq!(backfill.step(10).await.unwrap(), 0);
            assert_eq!(length(&pool, known).await, (Some(200_000), false));

            sqlx::query(
                "UPDATE tracks SET lastfm_checked_at = NOW() - INTERVAL '31 days' WHERE id = $1",
            )
            .bind(unknown)
            .execute(&pool)
            .await
            .unwrap();
            assert_eq!(
                edb::requeue_length_checks(&pool, RECHECK_AFTER_DAYS)
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(backfill.step(10).await.unwrap(), 1);
        })
        .await;
    }
}
