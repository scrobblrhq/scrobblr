//! Metadata enrichment pipeline.
//!
//! Jobs land in `enrichment_jobs` from three places: entity creation at
//! ingest time (scrobble / now-playing), manual refresh endpoints, and the
//! periodic backfill/re-sweep in this worker. This module claims due jobs and
//! queries the providers, cheapest-budget last:
//!
//! - **MusicBrainz** (1 req/s) — MBIDs, durations, release dates. Canonical;
//!   everything else keys off the MBIDs it resolves.
//! - **Cover Art Archive** — album covers by release / release-group MBID.
//! - **Deezer** — artist images, and album covers CAA doesn't have.
//! - **Last.fm** (optional, `LASTFM_API_KEY`) — artist bios.
//!
//! A track job piggybacks on its single MusicBrainz recording search to also
//! resolve the artist's MBID and — when a release title matches — the album's
//! MBID and release date, conserving the 1 req/s MusicBrainz budget.
//!
//! Failure model: provider errors are split into transient (retry the job
//! with exponential backoff, keeping whatever fields were already applied)
//! and fatal (log, treat as no contribution). "No match" is a valid final
//! result — the monthly re-sweep gives providers a chance to have gained
//! coverage since.

pub mod providers;
pub mod ratelimit;

use std::sync::Arc;
use std::time::Duration;

use fred::interfaces::PubsubInterface;
use rand::Rng;
use sqlx::PgPool;

use db::queries::enrichment as edb;
use db::queries::scrobbles as edb_scrobbles;
use shared::scrobble::normalize_name;

use providers::musicbrainz::{self as mb, parse_mb_date};
use providers::{ProviderError, coverart, deezer, lastfm};
use ratelimit::RateLimiter;

const CLAIM_BATCH: i64 = 10;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_ATTEMPTS: i32 = 5;
const STUCK_AFTER_MINS: i32 = 15;
const BACKFILL_PER_TABLE: i64 = 500;
const RESWEEP_PER_TABLE: i64 = 200;

/// Per-provider minimum request intervals.
const MUSICBRAINZ_INTERVAL: Duration = Duration::from_millis(1100); // hard 1 req/s limit
const COVERART_INTERVAL: Duration = Duration::from_millis(600); // no hard limit; be nice
const DEEZER_INTERVAL: Duration = Duration::from_millis(250); // limit is 50 req / 5 s
const LASTFM_INTERVAL: Duration = Duration::from_millis(250);

struct Lastfm {
    api_key: String,
    limiter: RateLimiter,
}

pub struct Enricher {
    db: PgPool,
    http: reqwest::Client,
    musicbrainz: RateLimiter,
    coverart: RateLimiter,
    deezer: RateLimiter,
    lastfm: Option<Lastfm>,
    /// Publishes now-playing refreshes when an image is filled; `None`
    /// disables live refresh (worker still enriches).
    redis: Option<fred::clients::Client>,
}

enum Processed {
    /// Job ran; carries the transient provider errors encountered (empty =
    /// clean finish).
    Done(Vec<String>),
    /// The catalog row no longer exists.
    EntityGone,
}

impl Enricher {
    pub fn from_env(db: PgPool, redis: Option<fred::clients::Client>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            // MusicBrainz requires an identifying User-Agent.
            .user_agent("scrobblr-worker/0.1 (+https://github.com/scrobblrhq/scrobblr)")
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .build()?;

        let lastfm = std::env::var("LASTFM_API_KEY").ok().map(|api_key| Lastfm {
            api_key,
            limiter: RateLimiter::new(LASTFM_INTERVAL),
        });
        if lastfm.is_none() {
            tracing::info!("enrichment: LASTFM_API_KEY not set — artist bios disabled");
        }

        Ok(Self {
            db,
            http,
            musicbrainz: RateLimiter::new(MUSICBRAINZ_INTERVAL),
            coverart: RateLimiter::new(COVERART_INTERVAL),
            deezer: RateLimiter::new(DEEZER_INTERVAL),
            lastfm,
            redis,
        })
    }

    /// After an artist/album gains an image, re-publish now-playing for
    /// anyone currently playing it so their live card swaps the fallback for
    /// the real artwork. Best-effort: SSE is ephemeral, so failures are logged
    /// and ignored.
    async fn republish_now_playing(&self, entity_type: &str, entity_id: i64) {
        let Some(redis) = &self.redis else { return };
        if entity_type != "artist" && entity_type != "album" {
            return;
        }
        let entries =
            match edb_scrobbles::active_now_playing_for_entity(&self.db, entity_type, entity_id)
                .await
            {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::warn!("enrichment: now-playing lookup failed: {e}");
                    return;
                }
            };
        for entry in entries {
            let Ok(payload) = serde_json::to_string(&entry.rich) else {
                continue;
            };
            let channel = format!("now_playing:{}", entry.user_id);
            if let Err(e) = redis.publish::<(), _, _>(channel, payload).await {
                tracing::warn!("enrichment: now-playing republish failed: {e}");
            }
        }
    }

    /// Main loop: claim due jobs and process them sequentially (throughput is
    /// bounded by the MusicBrainz rate limit anyway, so parallelism inside
    /// one worker buys nothing).
    pub async fn run(self: Arc<Self>) {
        loop {
            let jobs = match edb::claim_due_jobs(&self.db, CLAIM_BATCH).await {
                Ok(jobs) => jobs,
                Err(e) => {
                    tracing::error!("enrichment: failed to claim jobs: {e}");
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
            };

            if jobs.is_empty() {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }

            for job in jobs {
                self.process(&job).await;
            }
        }
    }

    /// Maintenance loop: rescues jobs stuck in `running` (crashed worker)
    /// every tick, and every 6 h enqueues backfill (never-enriched entities)
    /// plus the re-sweep of stale incomplete ones. First tick fires at
    /// startup so a pre-existing catalog starts filling immediately.
    pub async fn run_maintenance(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(30 * 60));
        let mut tick: u64 = 0;
        loop {
            interval.tick().await;

            match edb::reset_stuck_jobs(&self.db, STUCK_AFTER_MINS).await {
                Ok(0) => {}
                Ok(n) => tracing::warn!("enrichment: reset {n} stuck jobs"),
                Err(e) => tracing::error!("enrichment: stuck-job sweep failed: {e}"),
            }

            if tick.is_multiple_of(12) {
                match edb::enqueue_backfill(&self.db, BACKFILL_PER_TABLE).await {
                    Ok(n) if n > 0 => tracing::info!("enrichment: backfill enqueued {n} jobs"),
                    Ok(_) => {}
                    Err(e) => tracing::error!("enrichment: backfill failed: {e}"),
                }
                match edb::enqueue_incomplete_resweep(&self.db, RESWEEP_PER_TABLE).await {
                    Ok(n) if n > 0 => tracing::info!("enrichment: re-sweep enqueued {n} jobs"),
                    Ok(_) => {}
                    Err(e) => tracing::error!("enrichment: re-sweep failed: {e}"),
                }
            }
            tick += 1;
        }
    }

    async fn process(&self, job: &edb::Job) {
        let result = match job.entity_type.as_str() {
            "artist" => self.enrich_artist(job).await,
            "album" => self.enrich_album(job).await,
            "track" => self.enrich_track(job).await,
            other => {
                tracing::error!(job_id = job.id, "enrichment: unknown entity type {other}");
                self.try_db(edb::fail_job(&self.db, job.id, "unknown entity type"))
                    .await;
                return;
            }
        };

        match result {
            Ok(Processed::Done(transient)) if transient.is_empty() => {
                self.try_db(edb::mark_enriched(
                    &self.db,
                    &job.entity_type,
                    job.entity_id,
                ))
                .await;
                self.try_db(edb::complete_job(&self.db, job.id)).await;
                // The image the card may be waiting on is now stored; push a
                // fresh now-playing to whoever is listening to this entity.
                self.republish_now_playing(&job.entity_type, job.entity_id)
                    .await;
                tracing::debug!(job_id = job.id, entity = %job.entity_type, id = job.entity_id, "enriched");
            }
            Ok(Processed::Done(transient)) => {
                self.retry_or_fail(job, &transient.join("; ")).await;
            }
            Ok(Processed::EntityGone) => {
                // Row was deleted between enqueue and processing; nothing to do.
                self.try_db(edb::complete_job(&self.db, job.id)).await;
            }
            Err(e) => {
                // DB hiccup mid-job: same retry policy as a transient provider error.
                self.retry_or_fail(job, &format!("db error: {e}")).await;
            }
        }
    }

    async fn retry_or_fail(&self, job: &edb::Job, error: &str) {
        let attempt = job.attempts + 1;
        if attempt >= MAX_ATTEMPTS {
            tracing::warn!(job_id = job.id, entity = %job.entity_type, id = job.entity_id,
                "enrichment: giving up after {attempt} attempts: {error}");
            self.try_db(edb::fail_job(&self.db, job.id, error)).await;
        } else {
            let delay = backoff_secs(attempt);
            tracing::info!(job_id = job.id, entity = %job.entity_type, id = job.entity_id,
                "enrichment: retrying in {delay:.0}s (attempt {attempt}): {error}");
            self.try_db(edb::reschedule_job(&self.db, job.id, error, delay))
                .await;
        }
    }

    /// Bookkeeping updates must not kill the loop; a failed status write is
    /// recovered later by the stuck-job sweep.
    async fn try_db<F>(&self, fut: F)
    where
        F: Future<Output = Result<(), sqlx::Error>>,
    {
        if let Err(e) = fut.await {
            tracing::error!("enrichment: bookkeeping update failed: {e}");
        }
    }

    async fn enrich_artist(&self, job: &edb::Job) -> Result<Processed, sqlx::Error> {
        let Some(ctx) = edb::get_artist_ctx(&self.db, job.entity_id).await? else {
            return Ok(Processed::EntityGone);
        };

        let mut transient = Vec::new();
        let mut meta = edb::ArtistMetadata::default();

        // MBID first: it anchors the Last.fm lookup below and future jobs.
        if ctx.mbid.is_none() {
            match mb::search_artist(&self.http, &self.musicbrainz, &ctx.name).await {
                Ok(found) => meta.mbid = found.map(|a| a.id),
                Err(e) => note(&mut transient, e),
            }
        }

        // Artist images: Deezer is the only keyless source (MusicBrainz hosts
        // none, Last.fm returns placeholders).
        if ctx.image_url.is_none() || job.force {
            match deezer::artist_image(&self.http, &self.deezer, &ctx.name).await {
                Ok(found) => meta.image_url = found,
                Err(e) => note(&mut transient, e),
            }
        }

        if let Some(lastfm) = &self.lastfm
            && (ctx.bio.is_none() || job.force)
        {
            let mbid = ctx.mbid.or(meta.mbid);
            match lastfm::artist_bio(
                &self.http,
                &lastfm.limiter,
                &lastfm.api_key,
                &ctx.name,
                mbid,
            )
            .await
            {
                Ok(found) => meta.bio = found,
                Err(e) => note(&mut transient, e),
            }
        }

        // Forced refreshes are user-initiated; log what actually happened so
        // "nothing changed" is diagnosable from the worker output.
        if job.force {
            let image = match (&meta.image_url, &ctx.image_url) {
                (Some(new), Some(old)) if new == old => "re-fetched, unchanged",
                (Some(_), Some(_)) => "updated",
                (Some(_), None) => "added",
                (None, Some(_)) => "kept (provider returned none)",
                (None, None) => "none found",
            };
            let bio = if self.lastfm.is_none() {
                "skipped (LASTFM_API_KEY not set)"
            } else if meta.bio.is_some() {
                "fetched"
            } else if ctx.bio.is_some() {
                "kept (provider returned none)"
            } else {
                "none found"
            };
            tracing::info!(artist_id = ctx.id, name = %ctx.name,
                "enrichment: forced artist refresh — image: {image}; bio: {bio}");
        }

        edb::apply_artist_metadata(&self.db, ctx.id, &meta, job.force).await?;
        Ok(Processed::Done(transient))
    }

    async fn enrich_track(&self, job: &edb::Job) -> Result<Processed, sqlx::Error> {
        let Some(ctx) = edb::get_track_ctx(&self.db, job.entity_id).await? else {
            return Ok(Processed::EntityGone);
        };

        let mut transient = Vec::new();

        // One MusicBrainz call covers everything a track needs: search when
        // the MBID is unknown, direct lookup when only the duration is missing.
        let lookup = match (ctx.mbid, ctx.duration_ms) {
            (None, _) => Some(
                mb::search_recording(&self.http, &self.musicbrainz, &ctx.title, &ctx.artist_name)
                    .await,
            ),
            (Some(mbid), None) => {
                Some(mb::lookup_recording(&self.http, &self.musicbrainz, mbid).await)
            }
            (Some(_), Some(_)) => None, // nothing missing that MusicBrainz provides
        };
        let recording = match lookup {
            Some(Ok(found)) => found,
            Some(Err(e)) => {
                note(&mut transient, e);
                None
            }
            None => None,
        };

        if let Some(rec) = recording {
            edb::apply_track_metadata(
                &self.db,
                ctx.id,
                &edb::TrackMetadata {
                    mbid: Some(rec.id),
                    duration_ms: rec.length.and_then(|l| i32::try_from(l).ok()),
                },
            )
            .await?;

            // Piggyback: the recording carries the artist MBID…
            if ctx.artist_mbid.is_none()
                && let Some(credit) = rec.artist_credit.first()
            {
                edb::apply_artist_metadata(
                    &self.db,
                    ctx.artist_id,
                    &edb::ArtistMetadata {
                        mbid: Some(credit.artist.id),
                        ..Default::default()
                    },
                    false,
                )
                .await?;
            }

            // …and its releases can identify the album, saving that job its
            // own MusicBrainz search. Only on a normalized title match.
            if let (Some(album_id), Some(album_title)) = (ctx.album_id, &ctx.album_title)
                && ctx.album_mbid.is_none()
            {
                let wanted = normalize_name(album_title);
                let mut matches: Vec<&mb::Release> = rec
                    .releases
                    .iter()
                    .filter(|r| normalize_name(&r.title) == wanted)
                    .collect();
                matches.sort_by_key(|r| {
                    let official = r.status.as_deref() == Some("Official");
                    std::cmp::Reverse((official, r.date.is_some()))
                });

                if let Some(release) = matches.first() {
                    edb::apply_album_metadata(
                        &self.db,
                        album_id,
                        &edb::AlbumMetadata {
                            mbid: Some(release.id),
                            release_date: release.date.as_deref().and_then(parse_mb_date),
                            image_url: None,
                        },
                        false,
                    )
                    .await?;
                }
            }
        }

        Ok(Processed::Done(transient))
    }

    async fn enrich_album(&self, job: &edb::Job) -> Result<Processed, sqlx::Error> {
        let Some(ctx) = edb::get_album_ctx(&self.db, job.entity_id).await? else {
            return Ok(Processed::EntityGone);
        };

        let mut transient = Vec::new();
        let mut meta = edb::AlbumMetadata::default();
        let mut release_group: Option<uuid::Uuid> = None;

        if ctx.mbid.is_none() {
            match mb::search_release(&self.http, &self.musicbrainz, &ctx.title, &ctx.artist_name)
                .await
            {
                Ok(Some(release)) => {
                    meta.mbid = Some(release.id);
                    meta.release_date = release.date.as_deref().and_then(parse_mb_date);
                    release_group = release.release_group.map(|rg| rg.id);
                }
                Ok(None) => {}
                Err(e) => note(&mut transient, e),
            }
        }

        // Cover art chain: CAA by release → CAA by release-group → Deezer.
        if ctx.image_url.is_none() || job.force {
            let effective_mbid = ctx.mbid.or(meta.mbid);

            if let Some(mbid) = effective_mbid {
                match coverart::front_cover_url(
                    &self.http,
                    &self.coverart,
                    coverart::CoverKind::Release,
                    mbid,
                )
                .await
                {
                    Ok(Some(url)) => meta.image_url = Some(url),
                    Ok(None) => {
                        // The release has no art; its release-group often does
                        // (any edition's cover). Resolve the group if the
                        // search above didn't already carry it.
                        if release_group.is_none() {
                            match mb::lookup_release_group(&self.http, &self.musicbrainz, mbid)
                                .await
                            {
                                Ok(found) => release_group = found,
                                Err(e) => note(&mut transient, e),
                            }
                        }
                        if let Some(rgid) = release_group {
                            match coverart::front_cover_url(
                                &self.http,
                                &self.coverart,
                                coverart::CoverKind::ReleaseGroup,
                                rgid,
                            )
                            .await
                            {
                                Ok(found) => meta.image_url = found,
                                Err(e) => note(&mut transient, e),
                            }
                        }
                    }
                    Err(e) => note(&mut transient, e),
                }
            }

            if meta.image_url.is_none() {
                match deezer::album_cover(&self.http, &self.deezer, &ctx.artist_name, &ctx.title)
                    .await
                {
                    Ok(found) => meta.image_url = found,
                    Err(e) => note(&mut transient, e),
                }
            }
        }

        edb::apply_album_metadata(&self.db, ctx.id, &meta, job.force).await?;
        Ok(Processed::Done(transient))
    }
}

/// Transient errors accumulate on the job (it will retry); fatal ones are
/// logged and dropped — retrying cannot fix them.
fn note(transient: &mut Vec<String>, err: ProviderError) {
    match err {
        ProviderError::Transient(msg) => transient.push(msg),
        ProviderError::Fatal(msg) => tracing::warn!("enrichment: {msg}"),
    }
}

/// Exponential backoff with ±20 % jitter: ~1 min, 5 min, 25 min, ~2 h,
/// capped at 24 h.
fn backoff_secs(attempt: i32) -> f64 {
    let base = 60.0 * 5f64.powi(attempt - 1);
    let capped = base.min(86_400.0);
    let jitter = rand::thread_rng().gen_range(0.8..1.2);
    capped * jitter
}
