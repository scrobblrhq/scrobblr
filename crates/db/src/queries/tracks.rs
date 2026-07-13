use sqlx::PgPool;

use shared::models::{Album, Artist, TopListener, TopTrack, Track};

/// Looks up an artist by their normalized name, creating one if none exists.
///
/// On conflict the original casing is preserved — the `DO UPDATE SET name`
/// clause is intentionally a no-op so the first-inserted casing wins.
pub async fn find_or_create_artist(pool: &PgPool, name: &str) -> Result<Artist, sqlx::Error> {
    let normalized = name.trim().to_lowercase();

    sqlx::query_as!(
        Artist,
        r#"
        INSERT INTO artists (name, name_normalized)
        VALUES ($1, $2)
        ON CONFLICT (name_normalized) DO UPDATE
            SET name = artists.name
        RETURNING id, name, name_normalized, mbid, image_url, bio,
                  scrobble_count, listener_count, created_at
        "#,
        name.trim(),
        normalized,
    )
    .fetch_one(pool)
    .await
}

pub async fn find_artist_by_id(pool: &PgPool, id: i64) -> Result<Option<Artist>, sqlx::Error> {
    sqlx::query_as!(
        Artist,
        r#"
        SELECT id, name, name_normalized, mbid, image_url, bio,
               scrobble_count, listener_count, created_at
        FROM artists
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Searches artists by fuzzy name match using pg_trgm similarity.
///
/// Results are ranked by trigram similarity first, then by global popularity
/// (`scrobble_count`) as a tiebreaker. The `%` operator applies a minimum
/// similarity threshold (default 0.3) set via `pg_trgm.similarity_threshold`.
pub async fn search_artists(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<Artist>, sqlx::Error> {
    sqlx::query_as!(
        Artist,
        r#"
        SELECT id, name, name_normalized, mbid, image_url, bio,
               scrobble_count, listener_count, created_at
        FROM artists
        WHERE name % $1
        ORDER BY similarity(name, $1) DESC, scrobble_count DESC
        LIMIT $2
        "#,
        query,
        limit,
    )
    .fetch_all(pool)
    .await
}

pub async fn find_or_create_album(
    pool: &PgPool,
    artist_id: i64,
    title: &str,
) -> Result<i64, sqlx::Error> {
    let normalized = title.trim().to_lowercase();

    let row = sqlx::query!(
        r#"
        INSERT INTO albums (artist_id, title, title_normalized)
        VALUES ($1, $2, $3)
        ON CONFLICT (artist_id, title_normalized) DO UPDATE
            SET title = albums.title
        RETURNING id
        "#,
        artist_id,
        title.trim(),
        normalized,
    )
    .fetch_one(pool)
    .await?;

    Ok(row.id)
}

/// Looks up a track by `(artist_id, title_normalized)`, creating one if none exists.
///
/// On conflict, `album_id` and `duration_ms` are filled in only if the existing
/// row has `NULL` in those columns (`COALESCE` keeps the stored value otherwise).
/// This lets callers enrich incomplete records without overwriting good data.
pub async fn find_or_create_track(
    pool: &PgPool,
    artist_id: i64,
    album_id: Option<i64>,
    title: &str,
    duration_ms: Option<i32>,
) -> Result<Track, sqlx::Error> {
    let normalized = title.trim().to_lowercase();

    sqlx::query_as!(
        Track,
        r#"
        INSERT INTO tracks (artist_id, album_id, title, title_normalized, duration_ms)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (artist_id, title_normalized) DO UPDATE
            SET album_id    = COALESCE(tracks.album_id,    EXCLUDED.album_id),
                duration_ms = COALESCE(tracks.duration_ms, EXCLUDED.duration_ms)
        RETURNING id, artist_id, album_id, title, title_normalized, mbid,
                  duration_ms, scrobble_count, created_at
        "#,
        artist_id,
        album_id,
        title.trim(),
        normalized,
        duration_ms,
    )
    .fetch_one(pool)
    .await
}

pub async fn find_album_by_id(pool: &PgPool, id: i64) -> Result<Option<Album>, sqlx::Error> {
    sqlx::query_as!(
        Album,
        r#"
        SELECT id, artist_id, title, title_normalized, mbid, image_url,
               release_date, scrobble_count, created_at
        FROM albums
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

pub async fn find_track_by_id(pool: &PgPool, id: i64) -> Result<Option<Track>, sqlx::Error> {
    sqlx::query_as!(
        Track,
        r#"
        SELECT id, artist_id, album_id, title, title_normalized, mbid,
               duration_ms, scrobble_count, created_at
        FROM tracks
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// See [`search_artists`] — same ranking strategy applied to track titles.
pub async fn search_tracks(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<Track>, sqlx::Error> {
    sqlx::query_as!(
        Track,
        r#"
        SELECT id, artist_id, album_id, title, title_normalized, mbid,
               duration_ms, scrobble_count, created_at
        FROM tracks
        WHERE title % $1
        ORDER BY similarity(title, $1) DESC, scrobble_count DESC
        LIMIT $2
        "#,
        query,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// The artist's most-scrobbled tracks across all users.
pub async fn artist_top_tracks(
    pool: &PgPool,
    artist_id: i64,
    limit: i64,
) -> Result<Vec<TopTrack>, sqlx::Error> {
    sqlx::query_as!(
        TopTrack,
        r#"
        SELECT t.id            AS "track_id!",
               t.title         AS "track_title!",
               t.artist_id     AS "artist_id!",
               a.name          AS "artist_name!",
               al.image_url    AS "album_image?",
               COUNT(*)        AS "play_count!"
        FROM scrobbles s
        JOIN tracks t       ON t.id = s.track_id
        JOIN artists a      ON a.id = t.artist_id
        LEFT JOIN albums al ON al.id = t.album_id
        WHERE s.artist_id = $1
        GROUP BY t.id, t.title, t.artist_id, a.name, al.image_url
        ORDER BY COUNT(*) DESC
        LIMIT $2
        "#,
        artist_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Users who listen to this artist most, private profiles excluded.
pub async fn artist_listeners(
    pool: &PgPool,
    artist_id: i64,
    limit: i64,
) -> Result<Vec<TopListener>, sqlx::Error> {
    sqlx::query_as!(
        TopListener,
        r#"
        SELECT u.id           AS "user_id!",
               u.username     AS "username!",
               u.display_name AS "display_name?",
               u.image_url    AS "image_url?",
               COUNT(*)       AS "play_count!"
        FROM scrobbles s
        JOIN users u ON u.id = s.user_id
        WHERE s.artist_id = $1 AND NOT u.is_private
        GROUP BY u.id, u.username, u.display_name, u.image_url
        ORDER BY COUNT(*) DESC
        LIMIT $2
        "#,
        artist_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// See [`artist_listeners`], scoped to a single track.
pub async fn track_listeners(
    pool: &PgPool,
    track_id: i64,
    limit: i64,
) -> Result<Vec<TopListener>, sqlx::Error> {
    sqlx::query_as!(
        TopListener,
        r#"
        SELECT u.id           AS "user_id!",
               u.username     AS "username!",
               u.display_name AS "display_name?",
               u.image_url    AS "image_url?",
               COUNT(*)       AS "play_count!"
        FROM scrobbles s
        JOIN users u ON u.id = s.user_id
        WHERE s.track_id = $1 AND NOT u.is_private
        GROUP BY u.id, u.username, u.display_name, u.image_url
        ORDER BY COUNT(*) DESC
        LIMIT $2
        "#,
        track_id,
        limit,
    )
    .fetch_all(pool)
    .await
}
