//! Loved tracks (`loved_tracks`, migration 0034; docs/social.md, Loved
//! tracks). Nothing else reads them: rankings, recommendations,
//! similarity and album attribution ignore loves.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use shared::cursor::Cursor;

/// The most tracks an account may love: a bound on what one account's
/// list, its export and the cap's own count cost.
pub const MAX_LOVED_TRACKS: i64 = 50_000;

pub const SOURCE_SCROBBLR: &str = "scrobblr";
pub const SOURCE_LASTFM: &str = "lastfm";
pub const SOURCE_AUDIOSCROBBLER: &str = "audioscrobbler";
pub const SOURCE_LISTENBRAINZ: &str = "listenbrainz";
pub const SOURCE_LASTFM_IMPORT: &str = "lastfm_import";

/// Where a love came from.
#[derive(Debug, Clone, Copy)]
pub struct LoveSource {
    pub source: &'static str,
    pub client_id: Option<i32>,
}

impl LoveSource {
    pub const NATIVE: LoveSource = LoveSource {
        source: SOURCE_SCROBBLR,
        client_id: None,
    };
}

/// What a love came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loved {
    Now(DateTime<Utc>),
    /// Loved before; the first love's time stands.
    Already(DateTime<Utc>),
    /// The user already loves `max` tracks.
    AtLimit,
}

/// Loves the track unless the user already loves `max` tracks. Loving a
/// loved track changes nothing.
pub async fn love(
    pool: &PgPool,
    user_id: i64,
    track_id: i64,
    from: LoveSource,
    max: i64,
) -> Result<Loved, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('loves:' || $1, 0))")
        .bind(user_id.to_string())
        .execute(&mut *tx)
        .await?;
    let r = sqlx::query!(
        r#"
        SELECT (SELECT loved_at FROM loved_tracks WHERE user_id = $1 AND track_id = $2)
                   AS "already?",
               (SELECT count(*) FROM loved_tracks WHERE user_id = $1) AS "loved!"
        "#,
        user_id,
        track_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if let Some(at) = r.already {
        return Ok(Loved::Already(at));
    }
    if r.loved >= max {
        return Ok(Loved::AtLimit);
    }
    let at = sqlx::query_scalar!(
        r#"
        INSERT INTO loved_tracks (user_id, track_id, source, client_id)
        VALUES ($1, $2, $3, $4)
        RETURNING loved_at
        "#,
        user_id,
        track_id,
        from.source,
        from.client_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Loved::Now(at))
}

/// Whether there was a love to take back.
pub async fn unlove(pool: &PgPool, user_id: i64, track_id: i64) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query!(
        "DELETE FROM loved_tracks WHERE user_id = $1 AND track_id = $2",
        user_id,
        track_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted > 0)
}

/// When the user loved the track, if they do.
pub async fn loved_at(
    pool: &PgPool,
    user_id: i64,
    track_id: i64,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar!(
        "SELECT loved_at FROM loved_tracks WHERE user_id = $1 AND track_id = $2",
        user_id,
        track_id,
    )
    .fetch_optional(pool)
    .await
}

pub async fn count(pool: &PgPool, user_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM loved_tracks WHERE user_id = $1"#,
        user_id
    )
    .fetch_one(pool)
    .await
}

/// A loved track with what lists show of it: the album is the one the
/// catalog files the track under.
#[derive(Debug, Clone)]
pub struct Love {
    pub track_id: i64,
    pub track_title: String,
    pub track_mbid: Option<Uuid>,
    pub artist_id: i64,
    pub artist_name: String,
    pub album_id: Option<i64>,
    pub album_title: Option<String>,
    pub album_image: Option<String>,
    pub artist_image: Option<String>,
    pub loved_at: DateTime<Utc>,
}

/// The user's loves before `before`, newest first.
pub async fn page(
    pool: &PgPool,
    user_id: i64,
    before: Option<Cursor>,
    limit: i64,
) -> Result<Vec<Love>, sqlx::Error> {
    sqlx::query_as!(
        Love,
        r#"
        SELECT l.track_id, t.title AS track_title, t.mbid AS track_mbid,
               t.artist_id, a.name AS artist_name, t.album_id,
               al.title AS "album_title?", al.image_url AS "album_image?",
               a.image_url AS artist_image, l.loved_at
        FROM loved_tracks l
        JOIN tracks t  ON t.id = l.track_id
        JOIN artists a ON a.id = t.artist_id
        LEFT JOIN albums al ON al.id = t.album_id
        WHERE l.user_id = $1
          AND ($2::timestamptz IS NULL OR (l.loved_at, l.track_id) < ($2, $3))
        ORDER BY l.loved_at DESC, l.track_id DESC
        LIMIT $4
        "#,
        user_id,
        before.map(|c| c.at),
        before.map_or(0, |c| c.id),
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Page `offset / limit` of the user's loves, newest first, for the
/// Last.fm-compatible API's numbered pages.
pub async fn page_at(
    pool: &PgPool,
    user_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Love>, sqlx::Error> {
    sqlx::query_as!(
        Love,
        r#"
        SELECT l.track_id, t.title AS track_title, t.mbid AS track_mbid,
               t.artist_id, a.name AS artist_name, t.album_id,
               al.title AS "album_title?", al.image_url AS "album_image?",
               a.image_url AS artist_image, l.loved_at
        FROM (
            SELECT track_id, loved_at FROM loved_tracks
            WHERE user_id = $1
            ORDER BY loved_at DESC, track_id DESC
            LIMIT $2 OFFSET $3
        ) l
        JOIN tracks t  ON t.id = l.track_id
        JOIN artists a ON a.id = t.artist_id
        LEFT JOIN albums al ON al.id = t.album_id
        ORDER BY l.loved_at DESC, l.track_id DESC
        "#,
        user_id,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await
}
