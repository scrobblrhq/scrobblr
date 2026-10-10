use chrono::NaiveDate;
use sqlx::PgPool;
use uuid::Uuid;

/// Job priorities; higher runs first.
pub const PRIORITY_BACKFILL: i32 = 10;
/// History imports: 20 plus up to 9 by play count, so an import's most
/// played entities go first without delaying live ingest.
pub const PRIORITY_IMPORT: i32 = 20;
pub const PRIORITY_INGEST: i32 = 50;
pub const PRIORITY_REFRESH: i32 = 100;

/// A claimed enrichment job, ready to be processed by the worker.
#[derive(Debug)]
pub struct Job {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: i64,
    pub attempts: i32,
    pub force: bool,
}

/// Enqueues enrichment jobs for the entities referenced by a scrobble or
/// now-playing update. Skips entities already enriched, and does nothing when
/// a job row already exists (pending, done or failed) — permanent failures are
/// only retried via manual refresh or the periodic re-sweep.
pub async fn enqueue_for_ingest(
    pool: &PgPool,
    artist_id: i64,
    album_id: Option<i64>,
    track_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority)
        SELECT 'artist', $1::bigint, $4::int
        WHERE EXISTS (SELECT 1 FROM artists WHERE id = $1 AND enriched_at IS NULL)
        UNION ALL
        SELECT 'album', $2::bigint, $4::int
        WHERE $2::bigint IS NOT NULL
          AND EXISTS (SELECT 1 FROM albums WHERE id = $2 AND enriched_at IS NULL)
        UNION ALL
        SELECT 'track', $3::bigint, $4::int
        WHERE EXISTS (SELECT 1 FROM tracks WHERE id = $3 AND enriched_at IS NULL)
        ON CONFLICT (entity_type, entity_id) DO NOTHING
        "#,
        artist_id,
        album_id,
        track_id,
        PRIORITY_INGEST,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Requests a forced re-enrichment (manual refresh). Resets any existing job,
/// including one currently `running`: the in-flight pass was claimed without
/// `force`, so it must run again — the worker's completion writes are guarded
/// on `status = 'running'` and become no-ops after this reset. `force` makes
/// the worker overwrite provider-owned fields (images, bio) instead of only
/// filling NULLs.
pub async fn request_refresh(
    pool: &PgPool,
    entity_type: &str,
    entity_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority, force)
        VALUES ($1, $2, $3, TRUE)
        ON CONFLICT (entity_type, entity_id) DO UPDATE
            SET status          = 'pending',
                attempts        = 0,
                force           = TRUE,
                priority        = $3,
                next_attempt_at = NOW(),
                last_error      = NULL,
                finished_at     = NULL
        "#,
        entity_type,
        entity_id,
        PRIORITY_REFRESH,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Backfill sweep: enqueues never-enriched entities that have no job row yet
/// (catalog rows that predate the enrichment system, or whose failed jobs were
/// pruned). Most-scrobbled entities first. Returns jobs created.
pub async fn enqueue_backfill(pool: &PgPool, per_table_limit: i64) -> Result<u64, sqlx::Error> {
    let artists = sqlx::query!(
        r#"
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority)
        SELECT 'artist', id, $2 FROM artists
        WHERE enriched_at IS NULL
        ORDER BY scrobble_count DESC
        LIMIT $1
        ON CONFLICT (entity_type, entity_id) DO NOTHING
        "#,
        per_table_limit,
        PRIORITY_BACKFILL,
    )
    .execute(pool)
    .await?
    .rows_affected();

    let albums = sqlx::query!(
        r#"
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority)
        SELECT 'album', id, $2 FROM albums
        WHERE enriched_at IS NULL
        ORDER BY scrobble_count DESC
        LIMIT $1
        ON CONFLICT (entity_type, entity_id) DO NOTHING
        "#,
        per_table_limit,
        PRIORITY_BACKFILL,
    )
    .execute(pool)
    .await?
    .rows_affected();

    let tracks = sqlx::query!(
        r#"
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority)
        SELECT 'track', id, $2 FROM tracks
        WHERE enriched_at IS NULL
        ORDER BY scrobble_count DESC
        LIMIT $1
        ON CONFLICT (entity_type, entity_id) DO NOTHING
        "#,
        per_table_limit,
        PRIORITY_BACKFILL,
    )
    .execute(pool)
    .await?
    .rows_affected();

    Ok(artists + albums + tracks)
}

/// Re-sweep: entities enriched more than 30 days ago that are still missing
/// key fields (providers gain coverage over time — new releases get MBIDs and
/// cover art added later). Resets their finished jobs back to pending.
pub async fn enqueue_incomplete_resweep(
    pool: &PgPool,
    per_table_limit: i64,
) -> Result<u64, sqlx::Error> {
    let artists = sqlx::query!(
        r#"
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority)
        SELECT 'artist', id, $2 FROM artists
        WHERE enriched_at < NOW() - INTERVAL '30 days'
          AND (mbid IS NULL OR image_url IS NULL)
        ORDER BY scrobble_count DESC
        LIMIT $1
        ON CONFLICT (entity_type, entity_id) DO UPDATE
            SET status = 'pending', attempts = 0, next_attempt_at = NOW(),
                priority = EXCLUDED.priority, force = FALSE, last_error = NULL
            WHERE enrichment_jobs.status IN ('done', 'failed')
        "#,
        per_table_limit,
        PRIORITY_BACKFILL,
    )
    .execute(pool)
    .await?
    .rows_affected();

    let albums = sqlx::query!(
        r#"
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority)
        SELECT 'album', id, $2 FROM albums
        WHERE enriched_at < NOW() - INTERVAL '30 days'
          AND (mbid IS NULL OR image_url IS NULL)
        ORDER BY scrobble_count DESC
        LIMIT $1
        ON CONFLICT (entity_type, entity_id) DO UPDATE
            SET status = 'pending', attempts = 0, next_attempt_at = NOW(),
                priority = EXCLUDED.priority, force = FALSE, last_error = NULL
            WHERE enrichment_jobs.status IN ('done', 'failed')
        "#,
        per_table_limit,
        PRIORITY_BACKFILL,
    )
    .execute(pool)
    .await?
    .rows_affected();

    let tracks = sqlx::query!(
        r#"
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority)
        SELECT 'track', id, $2 FROM tracks
        WHERE enriched_at < NOW() - INTERVAL '30 days'
          AND (mbid IS NULL OR duration_ms IS NULL)
        ORDER BY scrobble_count DESC
        LIMIT $1
        ON CONFLICT (entity_type, entity_id) DO UPDATE
            SET status = 'pending', attempts = 0, next_attempt_at = NOW(),
                priority = EXCLUDED.priority, force = FALSE, last_error = NULL
            WHERE enrichment_jobs.status IN ('done', 'failed')
        "#,
        per_table_limit,
        PRIORITY_BACKFILL,
    )
    .execute(pool)
    .await?
    .rows_affected();

    Ok(artists + albums + tracks)
}

/// Claims up to `limit` due jobs, marking them `running`. Uses
/// FOR UPDATE SKIP LOCKED so concurrent claimers never double-process.
pub async fn claim_due_jobs(pool: &PgPool, limit: i64) -> Result<Vec<Job>, sqlx::Error> {
    sqlx::query_as!(
        Job,
        r#"
        UPDATE enrichment_jobs
        SET status = 'running', started_at = NOW()
        WHERE id IN (
            SELECT id FROM enrichment_jobs
            WHERE status = 'pending' AND next_attempt_at <= NOW()
            ORDER BY priority DESC, next_attempt_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, entity_type, entity_id, attempts, force
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Worker status writes (`complete_job`, `reschedule_job`, `fail_job`) only
/// apply while the job is still `running`: if a manual refresh (or the
/// stuck-job sweep) reset it back to `pending` mid-flight, that reset wins
/// and the job runs again.
pub async fn complete_job(pool: &PgPool, job_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE enrichment_jobs SET status = 'done', finished_at = NOW(), last_error = NULL WHERE id = $1 AND status = 'running'",
        job_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Reschedules a job after a transient failure. `delay_secs` should come from
/// the caller's backoff policy; the attempt counter increments here.
pub async fn reschedule_job(
    pool: &PgPool,
    job_id: i64,
    error: &str,
    delay_secs: f64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE enrichment_jobs
        SET status = 'pending',
            attempts = attempts + 1,
            next_attempt_at = NOW() + make_interval(secs => $3),
            last_error = $2
        WHERE id = $1 AND status = 'running'
        "#,
        job_id,
        error,
        delay_secs,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks a job permanently failed (retries exhausted). Only a manual refresh
/// or the monthly re-sweep will pick the entity up again.
pub async fn fail_job(pool: &PgPool, job_id: i64, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE enrichment_jobs SET status = 'failed', finished_at = NOW(), attempts = attempts + 1, last_error = $2 WHERE id = $1 AND status = 'running'",
        job_id,
        error,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Deletes jobs that finished `done` more than `days` days ago. Such a row
/// decides nothing: its entity is stamped `enriched_at`, and the enqueues
/// that would reset it to pending insert the same job without it. `failed`
/// rows stay, so the backfill doesn't retry them.
pub async fn delete_done_jobs(pool: &PgPool, days: i32) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        DELETE FROM enrichment_jobs
        WHERE status = 'done' AND finished_at < NOW() - make_interval(days => $1)
        "#,
        days,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Returns `running` jobs whose worker likely died back to `pending`.
pub async fn reset_stuck_jobs(pool: &PgPool, stuck_after_mins: i32) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        UPDATE enrichment_jobs
        SET status = 'pending', next_attempt_at = NOW()
        WHERE status = 'running' AND started_at < NOW() - make_interval(mins => $1)
        "#,
        stuck_after_mins,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[derive(Debug)]
pub struct ArtistCtx {
    pub id: i64,
    pub name: String,
    pub mbid: Option<Uuid>,
    pub image_url: Option<String>,
    pub bio: Option<String>,
}

pub async fn get_artist_ctx(pool: &PgPool, id: i64) -> Result<Option<ArtistCtx>, sqlx::Error> {
    sqlx::query_as!(
        ArtistCtx,
        "SELECT id, name, mbid, image_url, bio FROM artists WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await
}

#[derive(Debug)]
pub struct AlbumCtx {
    pub id: i64,
    pub title: String,
    pub mbid: Option<Uuid>,
    pub image_url: Option<String>,
    pub release_date: Option<NaiveDate>,
    pub artist_name: String,
}

pub async fn get_album_ctx(pool: &PgPool, id: i64) -> Result<Option<AlbumCtx>, sqlx::Error> {
    sqlx::query_as!(
        AlbumCtx,
        r#"
        SELECT al.id, al.title, al.mbid, al.image_url, al.release_date,
               a.name AS "artist_name!"
        FROM albums al
        JOIN artists a ON a.id = al.artist_id
        WHERE al.id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

#[derive(Debug)]
pub struct TrackCtx {
    pub id: i64,
    pub title: String,
    pub mbid: Option<Uuid>,
    pub duration_ms: Option<i32>,
    pub mb_duration_ms: Option<i32>,
    /// Recording id Last.fm suggested; unverified.
    pub mbid_hint: Option<Uuid>,
    pub artist_id: i64,
    pub artist_name: String,
    pub artist_mbid: Option<Uuid>,
    pub album_id: Option<i64>,
    pub album_title: Option<String>,
    pub album_mbid: Option<Uuid>,
    /// Recordings `worker tracks mb-review` took back: never adopted again.
    pub rejected_mbids: Vec<Uuid>,
}

pub async fn get_track_ctx(pool: &PgPool, id: i64) -> Result<Option<TrackCtx>, sqlx::Error> {
    sqlx::query_as!(
        TrackCtx,
        r#"
        SELECT t.id, t.title, t.mbid, t.duration_ms, t.mb_duration_ms, t.mbid_hint,
               t.artist_id, a.name AS "artist_name!", a.mbid AS "artist_mbid?",
               t.album_id, al.title AS "album_title?", al.mbid AS "album_mbid?",
               ARRAY(SELECT r.mbid FROM track_mbid_rejections r WHERE r.track_id = t.id)
                   AS "rejected_mbids!"
        FROM tracks t
        JOIN artists a ON a.id = t.artist_id
        LEFT JOIN albums al ON al.id = t.album_id
        WHERE t.id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

// Merge policy: `mbid` is an identity anchor — fill-only-NULL, never
// overwritten, even on forced refresh. Provider-owned fields (images, bio)
// are fill-only-NULL normally and overwritten when `force` is set. Names and
// titles are never touched (first-inserted casing wins, by design — see
// tracks::find_or_create_artist).
//
// `mbid` columns are UNIQUE: two catalog rows can resolve to the same
// MusicBrainz entity (e.g. "AC/DC" and "AC-DC" before normalization catches
// it). On conflict we retry the update without the mbid rather than failing
// the whole job.

fn is_mbid_conflict(err: &sqlx::Error) -> bool {
    matches!(
        err,
        sqlx::Error::Database(db) if db.constraint().is_some_and(|c| c.contains("_mbid_"))
    )
}

#[derive(Debug, Default)]
pub struct ArtistMetadata {
    pub mbid: Option<Uuid>,
    pub image_url: Option<String>,
    pub bio: Option<String>,
}

pub async fn apply_artist_metadata(
    pool: &PgPool,
    id: i64,
    meta: &ArtistMetadata,
    force: bool,
) -> Result<(), sqlx::Error> {
    let run = |mbid: Option<Uuid>| {
        sqlx::query!(
            r#"
            UPDATE artists SET
                mbid      = COALESCE(artists.mbid, $2),
                image_url = CASE WHEN artists.image_locked THEN artists.image_url
                                 WHEN $5 THEN COALESCE($3, artists.image_url)
                                 ELSE COALESCE(artists.image_url, $3) END,
                bio       = CASE WHEN $5 THEN COALESCE($4, artists.bio)
                                 ELSE COALESCE(artists.bio, $4) END
            WHERE id = $1
            "#,
            id,
            mbid,
            meta.image_url.as_deref(),
            meta.bio.as_deref(),
            force,
        )
        .execute(pool)
    };

    match run(meta.mbid).await {
        Err(e) if is_mbid_conflict(&e) => {
            tracing::warn!(artist_id = id, mbid = ?meta.mbid, "mbid already claimed by another artist, skipping");
            run(None).await?;
        }
        other => {
            other?;
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct AlbumMetadata {
    pub mbid: Option<Uuid>,
    pub image_url: Option<String>,
    pub release_date: Option<NaiveDate>,
}

pub async fn apply_album_metadata(
    pool: &PgPool,
    id: i64,
    meta: &AlbumMetadata,
    force: bool,
) -> Result<(), sqlx::Error> {
    let run = |mbid: Option<Uuid>| {
        sqlx::query!(
            r#"
            UPDATE albums SET
                mbid         = COALESCE(albums.mbid, $2),
                image_url    = CASE WHEN albums.image_locked THEN albums.image_url
                                    WHEN $5 THEN COALESCE($3, albums.image_url)
                                    ELSE COALESCE(albums.image_url, $3) END,
                release_date = COALESCE(albums.release_date, $4)
            WHERE id = $1
            "#,
            id,
            mbid,
            meta.image_url.as_deref(),
            meta.release_date,
            force,
        )
        .execute(pool)
    };

    match run(meta.mbid).await {
        Err(e) if is_mbid_conflict(&e) => {
            tracing::warn!(album_id = id, mbid = ?meta.mbid, "mbid already claimed by another album, skipping");
            run(None).await?;
        }
        other => {
            other?;
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct TrackMetadata {
    pub mbid: Option<Uuid>,
    /// The MusicBrainz length: fills `duration_ms` when NULL and is always
    /// kept as `mb_duration_ms`, which the scrobble classifier trusts.
    pub duration_ms: Option<i32>,
}

pub async fn apply_track_metadata(
    pool: &PgPool,
    id: i64,
    meta: &TrackMetadata,
) -> Result<(), sqlx::Error> {
    let run = |mbid: Option<Uuid>| {
        sqlx::query!(
            r#"
            UPDATE tracks SET
                mbid           = COALESCE(tracks.mbid, $2),
                duration_ms    = COALESCE(tracks.duration_ms, $3),
                mb_duration_ms = COALESCE(tracks.mb_duration_ms, $3)
            WHERE id = $1
            "#,
            id,
            mbid,
            meta.duration_ms,
        )
        .execute(pool)
    };

    match run(meta.mbid).await {
        Err(e) if is_mbid_conflict(&e) => {
            tracing::warn!(track_id = id, mbid = ?meta.mbid, "mbid already claimed by another track, skipping");
            run(None).await?;
        }
        other => {
            other?;
        }
    }
    Ok(())
}

/// Stamps the entity as enriched. Called only when a job finishes `done`
/// (partial applications from a job that will retry do NOT stamp, so the
/// entity stays eligible for ingest-time enqueueing and backfill).
pub async fn mark_enriched(pool: &PgPool, entity_type: &str, id: i64) -> Result<(), sqlx::Error> {
    match entity_type {
        "artist" => {
            sqlx::query!("UPDATE artists SET enriched_at = NOW() WHERE id = $1", id)
                .execute(pool)
                .await?;
        }
        "album" => {
            sqlx::query!("UPDATE albums SET enriched_at = NOW() WHERE id = $1", id)
                .execute(pool)
                .await?;
        }
        "track" => {
            sqlx::query!("UPDATE tracks SET enriched_at = NOW() WHERE id = $1", id)
                .execute(pool)
                .await?;
        }
        other => {
            tracing::error!(entity_type = other, "mark_enriched: unknown entity type");
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct TrackNeedingLength {
    pub id: i64,
    pub title: String,
    pub artist_name: String,
}

/// Tracks with no length from any source that Last.fm hasn't been asked
/// about yet, most scrobbled first. Deezer's lengths count: they are kept
/// apart, but stand in for the catalog's.
pub async fn tracks_missing_length(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<TrackNeedingLength>, sqlx::Error> {
    sqlx::query_as!(
        TrackNeedingLength,
        r#"
        SELECT t.id, t.title, a.name AS artist_name
        FROM tracks t
        JOIN artists a ON a.id = t.artist_id
        WHERE t.duration_ms IS NULL AND t.mb_duration_ms IS NULL AND t.lastfm_checked_at IS NULL
          AND t.deezer_duration_ms IS NULL
        ORDER BY t.scrobble_count DESC, t.id
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Records Last.fm's answer. A length fills the catalog value only when
/// it is still empty, like any other source.
pub async fn record_lastfm_length(
    pool: &PgPool,
    track_id: i64,
    length_ms: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE tracks
        SET duration_ms = COALESCE(duration_ms, $2), lastfm_checked_at = NOW()
        WHERE id = $1
        "#,
        track_id,
        length_ms,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Lets tracks Last.fm had no length for be asked again, since its
/// catalogue grows.
pub async fn requeue_length_checks(
    pool: &PgPool,
    older_than_days: i32,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        UPDATE tracks SET lastfm_checked_at = NULL
        WHERE lastfm_checked_at < NOW() - make_interval(days => $1)
          AND duration_ms IS NULL AND mb_duration_ms IS NULL
        "#,
        older_than_days,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Tracks to ask Deezer about, most scrobbled first: those with no length
/// at all, then those whose catalog and MusicBrainz lengths differ by 1.5x
/// or more, where a third length tells which one is wrong.
pub async fn tracks_for_deezer_length(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<TrackNeedingLength>, sqlx::Error> {
    sqlx::query_as!(
        TrackNeedingLength,
        r#"
        SELECT id AS "id!", title AS "title!", artist_name AS "artist_name!"
        FROM (
            (SELECT t.id, t.title, a.name AS artist_name, 0 AS kind, t.scrobble_count
             FROM tracks t
             JOIN artists a ON a.id = t.artist_id
             WHERE t.deezer_checked_at IS NULL AND t.duration_ms IS NULL AND t.mb_duration_ms IS NULL
             ORDER BY t.scrobble_count DESC, t.id
             LIMIT $1)
            UNION ALL
            (SELECT t.id, t.title, a.name, 1, t.scrobble_count
             FROM tracks t
             JOIN artists a ON a.id = t.artist_id
             WHERE t.deezer_checked_at IS NULL AND t.duration_ms > 0 AND t.mb_duration_ms > 0
               AND GREATEST(t.duration_ms, t.mb_duration_ms) * 2
                   >= LEAST(t.duration_ms, t.mb_duration_ms) * 3
             ORDER BY t.scrobble_count DESC, t.id
             LIMIT $1)
        ) backlog
        ORDER BY kind, scrobble_count DESC, id
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Records Deezer's answer: its length, kept apart from the catalog's.
pub async fn record_deezer_length(
    pool: &PgPool,
    track_id: i64,
    length_ms: Option<i32>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE tracks
        SET deezer_duration_ms = COALESCE($2, deezer_duration_ms), deezer_checked_at = NOW()
        WHERE id = $1
        "#,
        track_id,
        length_ms,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Lets tracks Deezer had no length for be asked again.
pub async fn requeue_deezer_checks(
    pool: &PgPool,
    older_than_days: i32,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        UPDATE tracks SET deezer_checked_at = NULL
        WHERE deezer_checked_at < NOW() - make_interval(days => $1)
          AND deezer_duration_ms IS NULL
          AND ((duration_ms IS NULL AND mb_duration_ms IS NULL)
               OR (duration_ms > 0 AND mb_duration_ms > 0
                   AND GREATEST(duration_ms, mb_duration_ms) * 2
                       >= LEAST(duration_ms, mb_duration_ms) * 3))
        "#,
        older_than_days,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// A track's three lengths.
#[derive(Debug)]
pub struct TrackLengths {
    pub track_id: i64,
    pub artist_name: String,
    pub title: String,
    pub mbid: Option<Uuid>,
    pub musicbrainz_ms: i32,
    pub catalog_ms: i32,
    pub deezer_ms: i32,
    pub scrobble_count: i64,
}

/// Tracks with a MusicBrainz, a catalog and a Deezer length, two of which
/// differ by 1.5x or more; most scrobbled first.
pub async fn disputed_lengths(pool: &PgPool, limit: i64) -> Result<Vec<TrackLengths>, sqlx::Error> {
    sqlx::query_as!(
        TrackLengths,
        r#"
        SELECT t.id AS track_id, a.name AS artist_name, t.title, t.mbid,
               t.mb_duration_ms AS "musicbrainz_ms!", t.duration_ms AS "catalog_ms!",
               t.deezer_duration_ms AS "deezer_ms!", t.scrobble_count
        FROM tracks t
        JOIN artists a ON a.id = t.artist_id
        WHERE t.mb_duration_ms > 0 AND t.duration_ms > 0 AND t.deezer_duration_ms > 0
          AND (GREATEST(t.mb_duration_ms, t.duration_ms) * 2
                   >= LEAST(t.mb_duration_ms, t.duration_ms) * 3
               OR GREATEST(t.mb_duration_ms, t.deezer_duration_ms) * 2
                   >= LEAST(t.mb_duration_ms, t.deezer_duration_ms) * 3
               OR GREATEST(t.duration_ms, t.deezer_duration_ms) * 2
                   >= LEAST(t.duration_ms, t.deezer_duration_ms) * 3)
        ORDER BY t.scrobble_count DESC, t.id
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Takes back a track's MusicBrainz match: records the recording as
/// rejected, clears the mbid and its length (and the hint, if it named
/// that recording), queues enrichment to search again, and queues every
/// day with plays of the track for classification, since its lengths
/// changed. Does nothing unless the stored MusicBrainz length is still
/// `seen_ms` (the one the review judged). Returns the days queued.
pub async fn unlink_musicbrainz(
    pool: &PgPool,
    track_id: i64,
    seen_ms: i32,
) -> Result<Option<u64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let track = sqlx::query!(
        r#"
        SELECT mbid, mb_duration_ms, duration_ms, deezer_duration_ms
        FROM tracks WHERE id = $1
        FOR UPDATE
        "#,
        track_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(track) = track.filter(|t| t.mb_duration_ms == Some(seen_ms)) else {
        return Ok(None);
    };
    if let Some(mbid) = track.mbid {
        sqlx::query!(
            r#"
            INSERT INTO track_mbid_rejections
                (track_id, mbid, mb_duration_ms, catalog_duration_ms, deezer_duration_ms)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (track_id, mbid) DO NOTHING
            "#,
            track_id,
            mbid,
            track.mb_duration_ms,
            track.duration_ms,
            track.deezer_duration_ms,
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        r#"
        UPDATE tracks
        SET mbid = NULL, mb_duration_ms = NULL,
            mbid_hint = CASE WHEN mbid_hint = mbid THEN NULL ELSE mbid_hint END
        WHERE id = $1
        "#,
        track_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority)
        VALUES ('track', $1, $2)
        ON CONFLICT (entity_type, entity_id) DO UPDATE
            SET status = 'pending', attempts = 0, next_attempt_at = NOW(),
                priority = EXCLUDED.priority, force = FALSE, last_error = NULL
            WHERE enrichment_jobs.status IN ('done', 'failed')
        "#,
        track_id,
        PRIORITY_BACKFILL,
    )
    .execute(&mut *tx)
    .await?;
    let days = sqlx::query!(
        r#"
        INSERT INTO classification_queue (user_id, day, priority)
        SELECT DISTINCT user_id, (day AT TIME ZONE 'UTC')::date, $2::int
        FROM scrobbles_daily_by_track
        WHERE track_id = $1
        ON CONFLICT (user_id, day) DO NOTHING
        "#,
        track_id,
        crate::queries::classification::PRIORITY_SWEEP,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(Some(days))
}
