use chrono::{DateTime, NaiveTime, TimeDelta, Utc};
use sqlx::PgPool;
use thiserror::Error;

use crate::queries::{classification as classification_db, tracks as tracks_db};
use shared::models::{
    ActivityDay, NowPlayingRich, ScrobbleLabel, ScrobbleRich, TopArtist, TopTrack,
};
use shared::scrobble::{
    self as scrobble_logic, DUPLICATE_WINDOW, ScrobbleInput, ScrobbleValidationError,
};

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("scrobble validation failed: {0}")]
    Validation(#[from] ScrobbleValidationError),
    #[error("duplicate scrobble detected")]
    Duplicate,
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Validates, resolves catalog entries, dedups, and inserts a new scrobble
/// row. This is the single ingestion path shared by the `/v1/scrobble` HTTP
/// handler (extension, mobile app), the scrobbler-compatible APIs and the
/// worker's connected-accounts poller (Spotify), so every source gets
/// identical validation/dedup/catalog-resolution behavior for free.
///
/// A duplicate is the same track within [`DUPLICATE_WINDOW`] of any of the
/// user's scrobbles, not only the latest: clients retry batches of older
/// plays, in any order.
pub async fn ingest_scrobble(
    pool: &PgPool,
    user_id: i64,
    input: &ScrobbleInput,
) -> Result<i64, IngestError> {
    scrobble_logic::validate(input)?;

    let catalog = tracks_db::resolve(
        pool,
        &tracks_db::CatalogInput {
            artist: &input.artist_name,
            featured_artists: &input.featured_artists,
            album: input.album_title.as_deref(),
            track: &input.track_title,
            duration_ms: input.duration_ms,
            recording_mbid: input.recording_mbid,
        },
    )
    .await?;

    let mut tx = pool.begin().await?;
    // Two copies of one submission in flight at once (a client retrying a
    // slow request) must not both pass the check below.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('ingest:' || $1, 0))")
        .bind(user_id.to_string())
        .execute(&mut *tx)
        .await?;
    let duplicate = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM scrobbles
            WHERE user_id = $1 AND track_id = $2 AND played_at > $3 AND played_at < $4
        ) AS "exists!"
        "#,
        user_id,
        catalog.track.id,
        input.played_at - DUPLICATE_WINDOW,
        input.played_at + DUPLICATE_WINDOW,
    )
    .fetch_one(&mut *tx)
    .await?;
    if duplicate {
        return Err(IngestError::Duplicate);
    }

    let scrobble_id = insert_scrobble(
        &mut *tx,
        &InsertScrobble {
            user_id,
            track_id: catalog.track.id,
            artist_id: catalog.artist_id,
            album_id: catalog.album_id,
            played_at: input.played_at,
            source: input.source.clone(),
            duration_ms: input.duration_ms,
            listened_ms: input.listened_ms,
            client_id: input.client_id,
        },
    )
    .await?;
    tx.commit().await?;

    // After the insert, so a classification already under way for this day
    // can't miss it. Best-effort, like the enrichment enqueue.
    if let Err(e) = classification_db::mark_scrobble_dirty(pool, user_id, input.played_at).await {
        tracing::warn!("failed to queue classification for scrobble: {e}");
    }

    Ok(scrobble_id)
}

/// Parameters required to record a new scrobble.
pub struct InsertScrobble {
    pub user_id: i64,
    pub track_id: i64,
    pub artist_id: i64,
    pub album_id: Option<i64>,
    pub played_at: DateTime<Utc>,
    pub source: String,
    /// Track length as reported by the client for this play.
    pub duration_ms: Option<i32>,
    /// How long the client says the user actually listened.
    pub listened_ms: Option<i32>,
    pub client_id: Option<i32>,
}

/// Inserts a new scrobble row and returns the generated row ID.
///
/// Scrobble counters on `tracks`, `artists`, `albums`, and `users` are
/// incremented automatically by the `trg_scrobble_counts` database trigger.
pub async fn insert_scrobble(
    executor: impl sqlx::PgExecutor<'_>,
    s: &InsertScrobble,
) -> Result<i64, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        INSERT INTO scrobbles (user_id, track_id, artist_id, album_id, played_at, source, duration_ms, listened_ms, client_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id
        "#,
        s.user_id,
        s.track_id,
        s.artist_id,
        s.album_id,
        s.played_at,
        s.source,
        s.duration_ms,
        s.listened_ms,
        s.client_id,
    )
    .fetch_one(executor)
    .await?;

    Ok(row.id)
}

/// Parameters required to upsert the current now-playing state for a user.
pub struct UpsertNowPlaying {
    pub user_id: i64,
    pub track_id: i64,
    pub artist_id: i64,
    pub album_id: Option<i64>,
    pub source: String,
    /// Timestamp at which this now-playing entry should be considered stale.
    /// Typically `NOW() + track.duration_ms`.
    pub expires_at: DateTime<Utc>,
}

/// Upserts the now-playing state for a user.
///
/// If a row already exists for `user_id`, all fields are overwritten and
/// `started_at` is reset to the current time.
pub async fn upsert_now_playing(pool: &PgPool, np: &UpsertNowPlaying) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO now_playing (user_id, track_id, artist_id, album_id, source, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (user_id) DO UPDATE
            SET track_id   = EXCLUDED.track_id,
                artist_id  = EXCLUDED.artist_id,
                album_id   = EXCLUDED.album_id,
                source     = EXCLUDED.source,
                started_at = NOW(),
                expires_at = EXCLUDED.expires_at
        "#,
        np.user_id,
        np.track_id,
        np.artist_id,
        np.album_id,
        np.source,
        np.expires_at,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Returns the most recent scrobbles for a user, enriched with track, artist,
/// and album metadata and their label. Duplicates are left out.
///
/// Results are ordered newest-first. Pass `before` to paginate backwards
/// through history (keyset pagination). If `before` is `None`, results start
/// from the current time.
///
/// Uses the `idx_scrobbles_user_time` index for efficient time-range scans.
pub async fn get_recent_scrobbles(
    pool: &PgPool,
    user_id: i64,
    limit: i64,
    before: Option<DateTime<Utc>>,
) -> Result<Vec<ScrobbleRich>, sqlx::Error> {
    let cutoff = before.unwrap_or_else(Utc::now);

    let rows = sqlx::query!(
        r#"
        SELECT
            s.id,
            s.played_at,
            s.source,
            s.track_id,
            t.title          AS track_title,
            s.artist_id,
            a.name           AS artist_name,
            s.album_id,
            al.title         AS "album_title?",
            al.image_url     AS "album_image?",
            s.duration_ms,
            CASE WHEN d.user_id IS NULL OR s.id > d.max_scrobble_id THEN NULL
                 ELSE COALESCE(f.status::text, 'counted') END AS status
        FROM scrobbles s
        JOIN tracks  t  ON t.id = s.track_id
        JOIN artists a  ON a.id = s.artist_id
        LEFT JOIN albums al ON al.id = s.album_id
        LEFT JOIN scrobble_flags f
               ON f.scrobble_id = s.id AND f.played_at = s.played_at
        LEFT JOIN scrobble_classification_days d
               ON d.user_id = s.user_id AND d.day = (s.played_at AT TIME ZONE 'UTC')::date
        WHERE s.user_id    = $1
          AND s.played_at  < $2
          AND f.status IS DISTINCT FROM 'duplicate'
        ORDER BY s.played_at DESC
        LIMIT $3
        "#,
        user_id,
        cutoff,
        limit,
    )
    .fetch_all(pool)
    .await?;

    let scrobbles = rows
        .into_iter()
        .map(|r| ScrobbleRich {
            id: r.id,
            played_at: r.played_at,
            source: r.source,
            track_id: r.track_id,
            track_title: r.track_title,
            artist_id: r.artist_id,
            artist_name: r.artist_name,
            album_id: r.album_id,
            album_title: r.album_title,
            album_image: r.album_image,
            duration_ms: r.duration_ms,
            status: r.status.as_deref().and_then(ScrobbleLabel::from_status),
        })
        .collect();

    Ok(scrobbles)
}

/// One page of a user's scrobbles in `[from, to]`, newest first, by offset
/// (Last.fm's `user.getRecentTracks` pages that way). Duplicates are left
/// out.
pub async fn recent_scrobbles_page(
    pool: &PgPool,
    user_id: i64,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    limit: i64,
    offset: i64,
) -> Result<Vec<ScrobbleRich>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT s.id, s.played_at, s.source, s.track_id, t.title AS track_title,
               s.artist_id, a.name AS artist_name, s.album_id,
               al.title AS "album_title?", al.image_url AS "album_image?", s.duration_ms,
               CASE WHEN d.user_id IS NULL OR s.id > d.max_scrobble_id THEN NULL
                    ELSE COALESCE(f.status::text, 'counted') END AS status
        FROM scrobbles s
        JOIN tracks  t  ON t.id = s.track_id
        JOIN artists a  ON a.id = s.artist_id
        LEFT JOIN albums al ON al.id = s.album_id
        LEFT JOIN scrobble_flags f
               ON f.scrobble_id = s.id AND f.played_at = s.played_at
        LEFT JOIN scrobble_classification_days d
               ON d.user_id = s.user_id AND d.day = (s.played_at AT TIME ZONE 'UTC')::date
        WHERE s.user_id = $1
          AND ($2::timestamptz IS NULL OR s.played_at >= $2)
          AND ($3::timestamptz IS NULL OR s.played_at <= $3)
          AND f.status IS DISTINCT FROM 'duplicate'
        ORDER BY s.played_at DESC, s.id DESC
        LIMIT $4 OFFSET $5
        "#,
        user_id,
        from,
        to,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ScrobbleRich {
            id: r.id,
            played_at: r.played_at,
            source: r.source,
            track_id: r.track_id,
            track_title: r.track_title,
            artist_id: r.artist_id,
            artist_name: r.artist_name,
            album_id: r.album_id,
            album_title: r.album_title,
            album_image: r.album_image,
            duration_ms: r.duration_ms,
            status: r.status.as_deref().and_then(ScrobbleLabel::from_status),
        })
        .collect())
}

/// A user's scrobbles in `[from, to]`, duplicates left out.
pub async fn count_scrobbles(
    pool: &PgPool,
    user_id: i64,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT (SELECT count(*) FROM scrobbles
                WHERE user_id = $1
                  AND ($2::timestamptz IS NULL OR played_at >= $2)
                  AND ($3::timestamptz IS NULL OR played_at <= $3))
             - (SELECT count(*) FROM scrobble_flags
                WHERE user_id = $1 AND status = 'duplicate'
                  AND ($2::timestamptz IS NULL OR played_at >= $2)
                  AND ($3::timestamptz IS NULL OR played_at <= $3)) AS "count!"
        "#,
        user_id,
        from,
        to,
    )
    .fetch_one(pool)
    .await
}

/// Returns the top artists for a user within the given time window, ordered by
/// total play count descending, duplicates left out.
///
/// Reads from the `scrobbles_daily_by_artist` continuous aggregate, so this
/// query is effectively a materialized view scan — only rows past the
/// watermark (about the last day) are read raw. The classifier's flags (only
/// the scrobbles it didn't count) take out duplicates and say how many plays
/// are unverified.
///
/// `since` should be aligned to a day boundary to maximise aggregate cache hits.
pub async fn get_top_artists(
    pool: &PgPool,
    user_id: i64,
    since: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<TopArtist>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        WITH flagged AS (
            SELECT t.artist_id,
                   count(*) FILTER (WHERE f.status = 'duplicate') AS duplicates,
                   count(*) FILTER (WHERE f.status IN ('suspect', 'no_data')) AS unverified
            FROM scrobble_flags f
            JOIN tracks t ON t.id = f.track_id
            WHERE f.user_id = $1
              AND (f.day::timestamp AT TIME ZONE 'UTC') >= $2
            GROUP BY t.artist_id
        ), plays AS (
            SELECT artist_id, SUM(play_count)::BIGINT AS play_count
            FROM scrobbles_daily_by_artist
            WHERE user_id = $1
              AND day    >= $2
            GROUP BY artist_id
        )
        SELECT
            p.artist_id                                     AS "artist_id!",
            a.name                                          AS artist_name,
            a.image_url,
            (p.play_count - COALESCE(fl.duplicates, 0))::BIGINT AS "play_count!",
            COALESCE(fl.unverified, 0)::BIGINT              AS "unverified_count!"
        FROM plays p
        JOIN artists a ON a.id = p.artist_id
        LEFT JOIN flagged fl ON fl.artist_id = p.artist_id
        WHERE p.play_count > COALESCE(fl.duplicates, 0)
        ORDER BY "play_count!" DESC, p.artist_id
        LIMIT $3
        "#,
        user_id,
        since,
        limit,
    )
    .fetch_all(pool)
    .await?;

    let artists = rows
        .into_iter()
        .map(|row| TopArtist {
            artist_id: row.artist_id,
            artist_name: row.artist_name,
            image_url: row.image_url,
            play_count: row.play_count,
            unverified_count: row.unverified_count,
        })
        .collect();

    Ok(artists)
}

/// Returns the top tracks for a user within the given time window, ordered by
/// total play count descending, duplicates left out.
///
/// Reads from the `scrobbles_daily_by_track` continuous aggregate and the
/// classifier's flags, like [`get_top_artists`]. Album art is resolved via
/// the track's `album_id` rather than the scrobble's, since the aggregate
/// does not store `album_id` at the track level.
///
/// `since` should be aligned to a day boundary to maximise aggregate cache hits.
pub async fn get_top_tracks(
    pool: &PgPool,
    user_id: i64,
    since: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<TopTrack>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        WITH flagged AS (
            SELECT track_id,
                   count(*) FILTER (WHERE status = 'duplicate') AS duplicates,
                   count(*) FILTER (WHERE status IN ('suspect', 'no_data')) AS unverified
            FROM scrobble_flags
            WHERE user_id = $1
              AND (day::timestamp AT TIME ZONE 'UTC') >= $2
            GROUP BY track_id
        ), plays AS (
            SELECT track_id, artist_id, SUM(play_count)::BIGINT AS play_count
            FROM scrobbles_daily_by_track
            WHERE user_id = $1
              AND day    >= $2
            GROUP BY track_id, artist_id
        )
        SELECT
            p.track_id                                      AS "track_id!",
            t.title                                         AS track_title,
            p.artist_id                                     AS "artist_id!",
            a.name                                          AS artist_name,
            al.image_url                                    AS album_image,
            (p.play_count - COALESCE(fl.duplicates, 0))::BIGINT AS "play_count!",
            COALESCE(fl.unverified, 0)::BIGINT              AS "unverified_count!"
        FROM plays p
        JOIN tracks  t  ON t.id  = p.track_id
        JOIN artists a  ON a.id  = p.artist_id
        LEFT JOIN albums al ON al.id = t.album_id
        LEFT JOIN flagged fl ON fl.track_id = p.track_id
        WHERE p.play_count > COALESCE(fl.duplicates, 0)
        ORDER BY "play_count!" DESC, p.track_id
        LIMIT $3
        "#,
        user_id,
        since,
        limit,
    )
    .fetch_all(pool)
    .await?;

    let tracks = rows
        .into_iter()
        .map(|row| TopTrack {
            track_id: row.track_id,
            track_title: row.track_title,
            artist_id: row.artist_id,
            artist_name: row.artist_name,
            album_image: row.album_image,
            play_count: row.play_count,
            unverified_count: Some(row.unverified_count),
        })
        .collect();

    Ok(tracks)
}

/// Returns the daily scrobble counts for a user starting from `since`, ordered
/// chronologically, duplicates left out.
///
/// Intended for rendering activity heatmaps on user profiles. Reads from the
/// `user_activity_daily` continuous aggregate and the classified days.
pub async fn get_activity_heatmap(
    pool: &PgPool,
    user_id: i64,
    since: DateTime<Utc>,
) -> Result<Vec<ActivityDay>, sqlx::Error> {
    sqlx::query_as!(
        ActivityDay,
        r#"
        SELECT
            a.day                                                AS "day!",
            (a.scrobble_count - COALESCE(d.duplicate, 0))::BIGINT AS "scrobble_count!"
        FROM user_activity_daily a
        LEFT JOIN scrobble_classification_days d
               ON d.user_id = a.user_id AND d.day = (a.day AT TIME ZONE 'UTC')::date
        WHERE a.user_id = $1
          AND a.day    >= $2
          AND a.scrobble_count > COALESCE(d.duplicate, 0)
        ORDER BY a.day
        "#,
        user_id,
        since,
    )
    .fetch_all(pool)
    .await
}

const SCROBBLE_AGGREGATES: [&str; 3] = [
    "scrobbles_daily_by_artist",
    "scrobbles_daily_by_track",
    "user_activity_daily",
];

/// Refreshes the daily aggregates over `[from, to]` so backfilled scrobbles
/// show up now instead of on the next hourly policy run. Call it after the
/// inserts commit: Timescale rejects the refresh inside a transaction.
pub async fn refresh_scrobble_aggregates(
    pool: &PgPool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    // Timescale skips partially covered buckets, so widen to whole UTC days.
    // Stop before today's open bucket: materializing it would leave today's
    // later scrobbles out until tomorrow.
    let day_start = |t: DateTime<Utc>| t.date_naive().and_time(NaiveTime::MIN).and_utc();
    let start = day_start(from);
    let end = (day_start(to) + TimeDelta::days(1)).min(day_start(Utc::now()));
    if start >= end {
        return Ok(());
    }

    for aggregate in SCROBBLE_AGGREGATES {
        sqlx::query(
            "CALL refresh_continuous_aggregate($1::regclass, $2::timestamptz, $3::timestamptz)",
        )
        .bind(aggregate)
        .bind(start)
        .bind(end)
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// Returns the currently playing track for a user, enriched with track, artist,
/// and album metadata. Returns `None` if the user has no active now-playing
/// entry or if the entry has expired.
///
/// Expiry is checked in the database (`expires_at > NOW()`), so callers do not
/// need to perform client-side staleness checks.
pub async fn get_now_playing(
    pool: &PgPool,
    user_id: i64,
) -> Result<Option<NowPlayingRich>, sqlx::Error> {
    sqlx::query_as!(
        NowPlayingRich,
        r#"
        SELECT
            t.title      AS track_title,
            a.name       AS artist_name,
            al.title     AS "album_title?",
            al.image_url AS album_image,
            a.image_url  AS artist_image,
            np.started_at,
            np.expires_at,
            np.source
        FROM now_playing np
        JOIN tracks  t  ON t.id = np.track_id
        JOIN artists a  ON a.id = np.artist_id
        LEFT JOIN albums al ON al.id = np.album_id
        WHERE np.user_id   = $1
          AND np.expires_at > NOW()
        "#,
        user_id,
    )
    .fetch_optional(pool)
    .await
}

/// A user's active now-playing paired with their id, for republishing over
/// SSE. Used by the worker after enrichment fills an image so the live card
/// updates from the fallback to the real cover.
#[derive(Debug)]
pub struct NowPlayingForUser {
    pub user_id: i64,
    pub rich: NowPlayingRich,
}

/// Active now-playing entries whose artist or album matches the given entity.
/// `entity_type` is `'artist'` or `'album'`; other values match nothing.
pub async fn active_now_playing_for_entity(
    pool: &PgPool,
    entity_type: &str,
    entity_id: i64,
) -> Result<Vec<NowPlayingForUser>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            np.user_id   AS "user_id!",
            t.title      AS "track_title!",
            a.name       AS "artist_name!",
            al.title     AS "album_title?",
            al.image_url AS "album_image?",
            a.image_url  AS "artist_image?",
            np.started_at,
            np.expires_at,
            np.source
        FROM now_playing np
        JOIN tracks  t  ON t.id = np.track_id
        JOIN artists a  ON a.id = np.artist_id
        LEFT JOIN albums al ON al.id = np.album_id
        WHERE np.expires_at > NOW()
          AND (($1 = 'artist' AND np.artist_id = $2)
            OR ($1 = 'album'  AND np.album_id  = $2))
        "#,
        entity_type,
        entity_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| NowPlayingForUser {
            user_id: r.user_id,
            rich: NowPlayingRich {
                track_title: r.track_title,
                artist_name: r.artist_name,
                album_title: r.album_title,
                album_image: r.album_image,
                artist_image: r.artist_image,
                started_at: r.started_at,
                expires_at: r.expires_at,
                source: r.source,
            },
        })
        .collect())
}
