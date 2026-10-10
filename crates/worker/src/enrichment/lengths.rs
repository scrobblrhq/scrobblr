//! Track lengths for tracks no source has a length for: imported history
//! carries none, and some clients send none, which leaves their scrobbles
//! `no_data` for the classifier. Most scrobbled tracks first, in the
//! background, never on the ingest path.
//!
//! - Last.fm (`track.getInfo`, with `LASTFM_API_KEY`), at a pace that leaves
//!   room for imports. Fills only the catalog value (`tracks.duration_ms`);
//!   MusicBrainz enrichment still adds `mb_duration_ms` on its own schedule.
//! - Deezer's public search, also for tracks whose catalog and MusicBrainz
//!   lengths disagree, where it tells which one is wrong (`worker tracks
//!   mb-review`). Kept as `deezer_duration_ms`, which stands in for the
//!   catalog's length when there is none.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::PgPool;

use super::providers::deezer;
use super::ratelimit::RateLimiter;
use crate::heartbeat::Beat;
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

    pub async fn run(self: Arc<Self>, beat: Beat) {
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
                Ok(0) => {
                    beat.ok_then_wait(IDLE).await;
                    tokio::time::sleep(IDLE).await
                }
                Ok(_) => {
                    failures = 0;
                    beat.ok().await;
                }
                Err(e) => {
                    failures += 1;
                    let delay = Duration::from_secs((30u64 << failures.min(6)).min(1800));
                    tracing::warn!("lengths: {e}; pausing {}s", delay.as_secs());
                    report_pause(&beat, &e, delay).await;
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

/// On top of the Deezer limiter image enrichment shares: one lookup a
/// second, a fifth of Deezer's allowance.
const DEEZER_PACE: Duration = Duration::from_secs(1);

pub struct DeezerLengths {
    db: PgPool,
    http: reqwest::Client,
    limiter: Arc<RateLimiter>,
    pace: RateLimiter,
    base: String,
}

impl DeezerLengths {
    pub fn new(
        db: PgPool,
        http: reqwest::Client,
        limiter: Arc<RateLimiter>,
        pace: Duration,
        base: &str,
    ) -> Self {
        Self {
            db,
            http,
            limiter,
            pace: RateLimiter::new(pace),
            base: base.to_string(),
        }
    }

    pub fn from_env(db: PgPool, http: reqwest::Client, limiter: Arc<RateLimiter>) -> Self {
        Self::new(db, http, limiter, DEEZER_PACE, deezer::BASE)
    }

    pub async fn run(self: Arc<Self>, beat: Beat) {
        let mut last_recheck: Option<Instant> = None;
        let mut failures = 0u32;
        loop {
            if last_recheck.is_none_or(|t| t.elapsed() >= RECHECK_EVERY) {
                match edb::requeue_deezer_checks(&self.db, RECHECK_AFTER_DAYS).await {
                    Ok(n) if n > 0 => {
                        tracing::info!("lengths: asking Deezer again about {n} tracks")
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("lengths: Deezer recheck failed: {e}"),
                }
                last_recheck = Some(Instant::now());
            }
            match self.step(BATCH).await {
                Ok(0) => {
                    beat.ok_then_wait(IDLE).await;
                    tokio::time::sleep(IDLE).await
                }
                Ok(_) => {
                    failures = 0;
                    beat.ok().await;
                }
                Err(e) => {
                    failures += 1;
                    let delay = Duration::from_secs((30u64 << failures.min(6)).min(1800));
                    tracing::warn!("lengths: Deezer: {e}; pausing {}s", delay.as_secs());
                    report_pause(&beat, &e, delay).await;
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Asks Deezer about one batch and returns how many tracks it settled.
    /// A transient error ends the batch, leaving the rest unasked.
    pub async fn step(&self, limit: i64) -> anyhow::Result<usize> {
        let tracks = edb::tracks_for_deezer_length(&self.db, limit).await?;
        for track in &tracks {
            self.pace.acquire().await;
            match deezer::track_length(
                &self.http,
                &self.limiter,
                &self.base,
                &track.artist_name,
                &track.title,
            )
            .await
            {
                Ok(length) => edb::record_deezer_length(&self.db, track.id, length).await?,
                Err(super::providers::ProviderError::Transient(e)) => anyhow::bail!(e),
                Err(e) => {
                    tracing::debug!(track_id = track.id, "lengths: Deezer gave no length: {e}");
                    edb::record_deezer_length(&self.db, track.id, None).await?;
                }
            }
        }
        Ok(tracks.len())
    }
}

/// A database error is the loop failing; the provider being away is its
/// own business, logged above.
async fn report_pause(beat: &Beat, error: &anyhow::Error, delay: Duration) {
    if error.downcast_ref::<sqlx::Error>().is_some() {
        beat.failed_then_wait(error, delay).await
    } else {
        beat.ok_then_wait(delay).await
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

    /// Serves Deezer's `/search/track` from canned hits, keyed by the title
    /// in the query; returns the base URL.
    async fn fake_deezer(hits: Vec<(&'static str, serde_json::Value)>) -> String {
        use axum::extract::Query;
        use std::collections::HashMap;

        let app = axum::Router::new().route(
            "/search/track",
            axum::routing::get(move |Query(q): Query<HashMap<String, String>>| {
                let query = q.get("q").cloned().unwrap_or_default();
                let data = hits
                    .iter()
                    .find(|(title, _)| query.ends_with(&format!(" {title}")))
                    .map(|(_, hits)| hits.clone())
                    .unwrap_or_else(|| serde_json::json!([]));
                async move { axum::Json(serde_json::json!({ "data": data })) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    fn hit(artist: &str, title: &str, secs: i64) -> serde_json::Value {
        serde_json::json!({
            "title": title,
            "title_short": title.split(" (").next().unwrap(),
            "duration": secs,
            "artist": { "name": artist },
        })
    }

    #[tokio::test]
    async fn deezer_lengths_need_the_artist_and_the_title() {
        let url = fake_deezer(vec![
            (
                "Song",
                serde_json::json!([hit("Someone Else", "Song", 100), hit("Band", "Song", 213)]),
            ),
            (
                "Going Under - Remastered 2023",
                serde_json::json!([hit("Band", "Going Under", 221)]),
            ),
            ("Other", serde_json::json!([hit("Band", "Other Song", 180)])),
            ("Teaser", serde_json::json!([hit("Band", "Teaser", 30)])),
        ])
        .await;
        let limiter = RateLimiter::new(Duration::ZERO);
        let http = reqwest::Client::new();
        let length = |title: &'static str| {
            let (http, url, limiter) = (&http, &url, &limiter);
            async move {
                deezer::track_length(http, limiter, url, "band", title)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(length("Song").await, Some(213_000));
        assert_eq!(length("Going Under - Remastered 2023").await, Some(221_000));
        assert_eq!(length("Other").await, None);
        assert_eq!(length("Teaser").await, None);
        assert_eq!(length("Missing").await, None);
    }

    #[tokio::test]
    #[ignore = "needs Postgres: just test-db"]
    async fn deezer_answers_missing_and_disputed_lengths() {
        with_db(|pool| async move {
            let url = fake_deezer(vec![
                ("Untimed", serde_json::json!([hit("Band", "Untimed", 200)])),
                (
                    "Runaway Baby",
                    serde_json::json!([hit("Band", "Runaway Baby", 148)]),
                ),
            ])
            .await;
            let lookup = DeezerLengths::new(
                pool.clone(),
                reqwest::Client::new(),
                Arc::new(RateLimiter::new(Duration::ZERO)),
                Duration::ZERO,
                &url,
            );

            let untimed = track(&pool, "Band", "Untimed", 50, None).await;
            let unknown = track(&pool, "Band", "Nowhere", 10, None).await;
            let disputed = track(&pool, "Band", "Runaway Baby", 5, Some(147_000)).await;
            let settled = track(&pool, "Band", "Fine", 500, Some(200_000)).await;
            for (id, mb) in [(disputed, 510_000), (settled, 201_000)] {
                sqlx::query("UPDATE tracks SET mb_duration_ms = $2 WHERE id = $1")
                    .bind(id)
                    .bind(mb)
                    .execute(&pool)
                    .await
                    .unwrap();
            }

            assert_eq!(lookup.step(10).await.unwrap(), 3);
            let deezer = |id: i64| {
                let pool = pool.clone();
                async move {
                    sqlx::query_as::<_, (Option<i32>, Option<i32>, bool)>(
                        "SELECT duration_ms, deezer_duration_ms, deezer_checked_at IS NOT NULL
                         FROM tracks WHERE id = $1",
                    )
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
                }
            };
            // Kept apart from the catalog's length, never written into it.
            assert_eq!(deezer(untimed).await, (None, Some(200_000), true));
            assert_eq!(deezer(unknown).await, (None, None, true));
            assert_eq!(deezer(disputed).await, (Some(147_000), Some(148_000), true));
            assert_eq!(deezer(settled).await, (Some(200_000), None, false));
            assert_eq!(lookup.step(10).await.unwrap(), 0);

            sqlx::query(
                "UPDATE tracks SET deezer_checked_at = NOW() - INTERVAL '31 days' WHERE id = $1",
            )
            .bind(unknown)
            .execute(&pool)
            .await
            .unwrap();
            assert_eq!(
                edb::requeue_deezer_checks(&pool, RECHECK_AFTER_DAYS)
                    .await
                    .unwrap(),
                1
            );
        })
        .await;
    }
}
