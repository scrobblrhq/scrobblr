use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;

use crate::queries::{enrichment as enrichment_db, tracks as tracks_db};
use shared::models::{ActivityDay, NowPlayingRich, Scrobble, ScrobbleRich, TopArtist, TopTrack};
use shared::scrobble::{self as scrobble_logic, ScrobbleInput, ScrobbleValidationError};

#[derive(Debug, Error)]
pub enum IngestError {
    #[error("scrobble validation failed: {0}")]
    Validation(#[from] ScrobbleValidationError),
    #[error("duplicate scrobble detected")]
    Duplicate,
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Validates, resolves catalog entries, dedups against the user's last
/// scrobble, and inserts a new scrobble row. This is the single ingestion
/// path shared by the `/v1/scrobble` HTTP handler (extension, mobile app)
/// and the worker's connected-accounts poller (Spotify), so both sources
/// get identical validation/dedup/catalog-resolution behavior for free.
pub async fn ingest_scrobble(
    pool: &PgPool,
    user_id: i64,
    input: &ScrobbleInput,
) -> Result<i64, IngestError> {
    scrobble_logic::validate(input)?;

    if let Some(last) = get_last_scrobble(pool, user_id).await? {
        let track_normalized = scrobble_logic::normalize_name(&input.track_title);
        let last_artist = tracks_db::find_artist_by_id(pool, last.artist_id)
            .await?
            .map(|a| scrobble_logic::normalize_name(&a.name))
            .unwrap_or_default();
        let last_track = tracks_db::find_track_by_id(pool, last.track_id)
            .await?
            .map(|t| scrobble_logic::normalize_name(&t.title))
            .unwrap_or_default();

        let same_track = last_track == track_normalized
            && last_artist == scrobble_logic::normalize_name(&input.artist_name);
        let delta_secs = (input.played_at - last.played_at).num_seconds().abs();

        if same_track && delta_secs < 30 {
            return Err(IngestError::Duplicate);
        }
    }

    let artist = tracks_db::find_or_create_artist(pool, &input.artist_name).await?;

    let album_id = if let Some(album_title) = &input.album_title {
        Some(tracks_db::find_or_create_album(pool, artist.id, album_title).await?)
    } else {
        None
    };

    let track = tracks_db::find_or_create_track(
        pool,
        artist.id,
        album_id,
        &input.track_title,
        input.duration_ms,
    )
    .await?;

    // Best-effort: a failure here must never reject the scrobble.
    if let Err(e) = enrichment_db::enqueue_for_ingest(pool, artist.id, album_id, track.id).await {
        tracing::warn!("failed to enqueue enrichment for scrobble: {e}");
    }

    let scrobble_id = insert_scrobble(
        pool,
        &InsertScrobble {
            user_id,
            track_id: track.id,
            artist_id: artist.id,
            album_id,
            played_at: input.played_at,
            source: input.source.clone(),
            duration_ms: input.duration_ms,
        },
    )
    .await?;

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
    /// Actual listening duration, which may be shorter than the track's full duration.
    pub duration_ms: Option<i32>,
}

/// Inserts a new scrobble row and returns the generated row ID.
///
/// Scrobble counters on `tracks`, `artists`, `albums`, and `users` are
/// incremented automatically by the `trg_scrobble_counts` database trigger.
pub async fn insert_scrobble(pool: &PgPool, s: &InsertScrobble) -> Result<i64, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        INSERT INTO scrobbles (user_id, track_id, artist_id, album_id, played_at, source, duration_ms)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id
        "#,
        s.user_id,
        s.track_id,
        s.artist_id,
        s.album_id,
        s.played_at,
        s.source,
        s.duration_ms,
    )
        .fetch_one(pool)
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
/// and album metadata.
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
            s.duration_ms
        FROM scrobbles s
        JOIN tracks  t  ON t.id = s.track_id
        JOIN artists a  ON a.id = s.artist_id
        LEFT JOIN albums al ON al.id = s.album_id
        WHERE s.user_id    = $1
          AND s.played_at  < $2
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
        })
        .collect();

    Ok(scrobbles)
}

/// Returns the top artists for a user within the given time window, ordered by
/// total play count descending.
///
/// Reads from the `scrobbles_daily_by_artist` continuous aggregate, so this
/// query is effectively a materialized view scan — no raw scrobble rows are
/// touched.
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
        SELECT
            s.artist_id                                 AS "artist_id!",
            a.name                                      AS artist_name,
            a.image_url,
            COALESCE(SUM(s.play_count), 0)::BIGINT      AS "play_count!"
        FROM scrobbles_daily_by_artist s
        JOIN artists a ON a.id = s.artist_id
        WHERE s.user_id = $1
          AND s.day    >= $2
        GROUP BY s.artist_id, a.name, a.image_url
        ORDER BY "play_count!" DESC
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
        })
        .collect();

    Ok(artists)
}

/// Returns the top tracks for a user within the given time window, ordered by
/// total play count descending.
///
/// Reads from the `scrobbles_daily_by_track` continuous aggregate. Album art
/// is resolved via the track's `album_id` rather than the scrobble's, since
/// the aggregate does not store `album_id` at the track level.
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
        SELECT
            s.track_id                                  AS "track_id!",
            t.title                                     AS track_title,
            s.artist_id                                 AS "artist_id!",
            a.name                                      AS artist_name,
            al.image_url                                AS album_image,
            COALESCE(SUM(s.play_count), 0)::BIGINT      AS "play_count!"
        FROM scrobbles_daily_by_track s
        JOIN tracks  t  ON t.id  = s.track_id
        JOIN artists a  ON a.id  = s.artist_id
        LEFT JOIN albums al ON al.id = t.album_id
        WHERE s.user_id = $1
          AND s.day    >= $2
        GROUP BY s.track_id, t.title, s.artist_id, a.name, al.image_url
        ORDER BY "play_count!" DESC
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
        })
        .collect();

    Ok(tracks)
}

/// Returns the daily scrobble counts for a user starting from `since`, ordered
/// chronologically.
///
/// Intended for rendering activity heatmaps on user profiles. Reads from the
/// `user_activity_daily` continuous aggregate.
pub async fn get_activity_heatmap(
    pool: &PgPool,
    user_id: i64,
    since: DateTime<Utc>,
) -> Result<Vec<ActivityDay>, sqlx::Error> {
    sqlx::query_as!(
        ActivityDay,
        r#"
        SELECT
            day             AS "day!",
            scrobble_count  AS "scrobble_count!"
        FROM user_activity_daily
        WHERE user_id = $1
          AND day    >= $2
        ORDER BY day
        "#,
        user_id,
        since,
    )
    .fetch_all(pool)
    .await
}

/// Returns the most recent scrobble for a user, or `None` if the user has no
/// history.
///
/// Used for deduplication checks before inserting a new scrobble — callers
/// should compare the returned track and timestamp against the incoming data.
pub async fn get_last_scrobble(
    pool: &PgPool,
    user_id: i64,
) -> Result<Option<Scrobble>, sqlx::Error> {
    sqlx::query_as!(
        Scrobble,
        r#"
        SELECT id, user_id, track_id, artist_id, album_id, played_at, source, duration_ms
        FROM scrobbles
        WHERE user_id = $1
        ORDER BY played_at DESC
        LIMIT 1
        "#,
        user_id,
    )
    .fetch_optional(pool)
    .await
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
            al.title     AS album_title,
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
