//! What an account export reads. Secrets are never selected: no password,
//! token or key hashes, no OAuth tokens, no session ids.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct ExportedScrobble {
    pub id: i64,
    pub played_at: DateTime<Utc>,
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub track_mbid: Option<Uuid>,
    pub duration_ms: Option<i32>,
    pub listened_ms: Option<i32>,
    pub source: String,
    pub client_protocol: Option<String>,
    pub client_name: Option<String>,
    pub client_verified: Option<bool>,
    pub import_id: Option<i64>,
    /// `None` until the day is classified.
    pub status: Option<String>,
    pub reason: Option<String>,
}

/// `(played_at, id)` of the last scrobble read.
pub type Cursor = (DateTime<Utc>, i64);

/// Before every scrobble (the first chunk's start); `None` without any.
pub async fn first_cursor(pool: &PgPool) -> Result<Option<Cursor>, sqlx::Error> {
    let start: Option<DateTime<Utc>> = sqlx::query_scalar(
        r#"
        SELECT min(range_start) FROM timescaledb_information.chunks
        WHERE hypertable_schema = 'public' AND hypertable_name = 'scrobbles'
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(start.map(|start| (start, i64::MIN)))
}

/// The user's next scrobbles after `after`, oldest first. One statement a
/// page, so no transaction holds chunk locks for the whole export.
pub async fn scrobbles_page(
    pool: &PgPool,
    user_id: i64,
    after: Cursor,
    limit: i64,
) -> Result<Vec<ExportedScrobble>, sqlx::Error> {
    sqlx::query_as!(
        ExportedScrobble,
        r#"
        SELECT s.id, s.played_at, a.name AS artist, t.title AS track,
               al.title AS "album?", t.mbid AS track_mbid,
               s.duration_ms, s.listened_ms, s.source,
               c.protocol AS "client_protocol?", c.name AS "client_name?",
               c.verified AS "client_verified?", s.import_id,
               CASE WHEN d.user_id IS NULL OR s.id > d.max_scrobble_id THEN NULL
                    ELSE COALESCE(f.status::text, 'counted') END AS status,
               f.reason AS "reason?"
        FROM scrobbles s
        JOIN tracks t  ON t.id = s.track_id
        JOIN artists a ON a.id = s.artist_id
        LEFT JOIN albums al ON al.id = s.album_id
        LEFT JOIN scrobble_clients c ON c.id = s.client_id
        LEFT JOIN scrobble_flags f
               ON f.scrobble_id = s.id AND f.played_at = s.played_at
        LEFT JOIN scrobble_classification_days d
               ON d.user_id = s.user_id AND d.day = (s.played_at AT TIME ZONE 'UTC')::date
        WHERE s.user_id = $1
          AND s.played_at >= $2
          AND (s.played_at, s.id) > ($2, $3)
        ORDER BY s.played_at, s.id
        LIMIT $4
        "#,
        user_id,
        after.0,
        after.1,
        limit,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Serialize)]
pub struct ExportedLove {
    pub loved_at: DateTime<Utc>,
    pub artist: String,
    pub track: String,
    /// The album the catalog files the track under.
    pub album: Option<String>,
    pub track_mbid: Option<Uuid>,
    /// `scrobblr`, `lastfm`, `audioscrobbler` or `lastfm_import`.
    pub source: String,
    pub client_protocol: Option<String>,
    pub client_name: Option<String>,
    pub client_verified: Option<bool>,
    pub import_id: Option<i64>,
}

/// Every track the user loves, oldest love first (50,000 at most).
pub async fn loved_tracks(pool: &PgPool, user_id: i64) -> Result<Vec<ExportedLove>, sqlx::Error> {
    sqlx::query_as!(
        ExportedLove,
        r#"
        SELECT l.loved_at, a.name AS artist, t.title AS track, al.title AS "album?",
               t.mbid AS track_mbid, l.source,
               c.protocol AS "client_protocol?", c.name AS "client_name?",
               c.verified AS "client_verified?", l.import_id
        FROM loved_tracks l
        JOIN tracks t  ON t.id = l.track_id
        JOIN artists a ON a.id = t.artist_id
        LEFT JOIN albums al ON al.id = t.album_id
        LEFT JOIN scrobble_clients c ON c.id = l.client_id
        WHERE l.user_id = $1
        ORDER BY l.loved_at, l.track_id
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Serialize)]
pub struct Follow {
    pub username: String,
    pub since: DateTime<Utc>,
}

pub async fn follows(
    pool: &PgPool,
    user_id: i64,
    following: bool,
) -> Result<Vec<Follow>, sqlx::Error> {
    sqlx::query_as!(
        Follow,
        r#"
        SELECT u.username, f.created_at AS since
        FROM user_follows f
        JOIN users u ON u.id = CASE WHEN $2 THEN f.followee_id ELSE f.follower_id END
        WHERE CASE WHEN $2 THEN f.follower_id ELSE f.followee_id END = $1
        ORDER BY f.created_at
        "#,
        user_id,
        following,
    )
    .fetch_all(pool)
    .await
}

/// The accounts the user blocks; never who blocks the user.
pub async fn blocks(pool: &PgPool, user_id: i64) -> Result<Vec<Follow>, sqlx::Error> {
    sqlx::query_as!(
        Follow,
        r#"
        SELECT u.username, b.created_at AS since
        FROM user_blocks b
        JOIN users u ON u.id = b.blocked_id
        WHERE b.blocker_id = $1
        ORDER BY b.created_at
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Serialize)]
pub struct RemovedFollower {
    pub username: String,
    /// Until when it can't follow the user again.
    pub until: DateTime<Utc>,
}

/// Followers the user removed whose cooldown isn't over; never who removed
/// the user.
pub async fn removed_followers(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<RemovedFollower>, sqlx::Error> {
    sqlx::query_as!(
        RemovedFollower,
        r#"
        SELECT u.username, r.until
        FROM follower_removals r
        JOIN users u ON u.id = r.follower_id
        WHERE r.user_id = $1 AND r.until > NOW()
        ORDER BY r.until
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Serialize)]
pub struct ExportedComment {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: i64,
    pub entity_name: Option<String>,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

pub async fn comments(pool: &PgPool, user_id: i64) -> Result<Vec<ExportedComment>, sqlx::Error> {
    sqlx::query_as!(
        ExportedComment,
        r#"
        SELECT c.id, c.entity_type, c.entity_id,
               COALESCE(a.name, t.title) AS entity_name, c.body, c.created_at
        FROM comments c
        LEFT JOIN artists a ON c.entity_type = 'artist' AND a.id = c.entity_id
        LEFT JOIN tracks t  ON c.entity_type = 'track' AND t.id = c.entity_id
        WHERE c.user_id = $1
        ORDER BY c.created_at
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Serialize)]
pub struct ExportedUpload {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: i64,
    #[serde(skip)]
    pub url: String,
    pub vote_count: i32,
    pub created_at: DateTime<Utc>,
}

pub async fn image_uploads(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<ExportedUpload>, sqlx::Error> {
    sqlx::query_as!(
        ExportedUpload,
        r#"
        SELECT id, entity_type, entity_id, url, vote_count, created_at
        FROM image_candidates
        WHERE uploaded_by = $1
        ORDER BY created_at
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Serialize)]
pub struct ExportedVote {
    pub candidate_id: i64,
    pub created_at: DateTime<Utc>,
}

pub async fn image_votes(pool: &PgPool, user_id: i64) -> Result<Vec<ExportedVote>, sqlx::Error> {
    sqlx::query_as!(
        ExportedVote,
        r#"
        SELECT candidate_id, created_at FROM image_candidate_votes
        WHERE user_id = $1
        ORDER BY created_at
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Serialize)]
pub struct ExportedSession {
    /// The bearer token: never written out.
    #[serde(skip)]
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub current: bool,
}

pub async fn sessions(pool: &PgPool, user_id: i64) -> Result<Vec<ExportedSession>, sqlx::Error> {
    sqlx::query_as!(
        ExportedSession,
        r#"
        SELECT id, created_at, expires_at, last_used_at, ip_address::text AS ip_address,
               user_agent, FALSE AS "current!"
        FROM user_sessions
        WHERE user_id = $1 AND expires_at > NOW()
        ORDER BY created_at
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Serialize)]
pub struct ExportedAuthorization {
    pub api_key: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

pub async fn scrobbler_authorizations(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<ExportedAuthorization>, sqlx::Error> {
    sqlx::query_as!(
        ExportedAuthorization,
        r#"
        SELECT api_key, created_at, expires_at FROM scrobbler_authorizations
        WHERE user_id = $1
        ORDER BY created_at
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

pub async fn origin_recorded_at(
    pool: &PgPool,
    user_id: i64,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar!(
        "SELECT created_at FROM registration_origins WHERE user_id = $1",
        user_id
    )
    .fetch_optional(pool)
    .await
}
