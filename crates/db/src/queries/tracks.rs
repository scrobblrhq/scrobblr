use sqlx::PgPool;

use crate::queries::enrichment as enrichment_db;
use shared::models::{Album, Artist, TopListener, TopTrack, Track, TrackArtistRole, TrackCredit};
use shared::scrobble::normalize_featured_artists;

/// A track as a scrobble or now playing names it.
pub struct CatalogInput<'a> {
    pub artist: &'a str,
    pub featured_artists: &'a [String],
    pub album: Option<&'a str>,
    pub track: &'a str,
    pub duration_ms: Option<i32>,
}

pub struct CatalogEntry {
    pub artist_id: i64,
    pub album_id: Option<i64>,
    pub track: Track,
}

/// Finds or creates the artist, album and track, records the credits, and
/// queues enrichment for whatever hasn't been enriched (best-effort). The
/// one path every live source takes into the catalog.
pub async fn resolve(pool: &PgPool, input: &CatalogInput<'_>) -> Result<CatalogEntry, sqlx::Error> {
    let artist = find_or_create_artist(pool, input.artist).await?;
    let album_id = match input.album {
        Some(title) => Some(find_or_create_album(pool, artist.id, title).await?),
        None => None,
    };
    let track =
        find_or_create_track(pool, artist.id, album_id, input.track, input.duration_ms).await?;

    let mut featured_ids = Vec::new();
    for name in normalize_featured_artists(input.artist, input.featured_artists) {
        featured_ids.push(find_or_create_artist(pool, &name).await?.id);
    }
    record_track_credits(pool, track.id, artist.id, &featured_ids).await?;

    if let Err(e) = enrichment_db::enqueue_for_ingest(pool, artist.id, album_id, track.id).await {
        tracing::warn!("failed to enqueue enrichment: {e}");
    }
    Ok(CatalogEntry {
        artist_id: artist.id,
        album_id,
        track,
    })
}

/// Looks up an artist by their normalized name, creating one if none
/// exists; the first-inserted casing wins.
///
/// Reads before inserting, so the usual case (the artist exists) writes
/// nothing. A concurrent insert of the same artist makes the first pass
/// come back empty; the second one sees it.
pub async fn find_or_create_artist(pool: &PgPool, name: &str) -> Result<Artist, sqlx::Error> {
    let normalized = name.trim().to_lowercase();
    for _ in 0..2 {
        let artist = sqlx::query_as!(
            Artist,
            r#"
            WITH found AS (
                SELECT id, name, name_normalized, mbid, image_url, bio,
                       scrobble_count, listener_count, created_at
                FROM artists
                WHERE name_normalized = $2
            ), inserted AS (
                INSERT INTO artists (name, name_normalized)
                SELECT $1, $2
                WHERE NOT EXISTS (SELECT 1 FROM found)
                ON CONFLICT (name_normalized) DO NOTHING
                RETURNING id, name, name_normalized, mbid, image_url, bio,
                          scrobble_count, listener_count, created_at
            )
            SELECT id AS "id!", name AS "name!", name_normalized AS "name_normalized!",
                   mbid, image_url, bio, scrobble_count AS "scrobble_count!",
                   listener_count AS "listener_count!", created_at AS "created_at!"
            FROM found
            UNION ALL
            SELECT id, name, name_normalized, mbid, image_url, bio,
                   scrobble_count, listener_count, created_at
            FROM inserted
            "#,
            name.trim(),
            normalized,
        )
        .fetch_optional(pool)
        .await?;
        if let Some(artist) = artist {
            return Ok(artist);
        }
    }
    Err(sqlx::Error::RowNotFound)
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

/// Like [`find_or_create_artist`], for an artist's album.
pub async fn find_or_create_album(
    pool: &PgPool,
    artist_id: i64,
    title: &str,
) -> Result<i64, sqlx::Error> {
    let normalized = title.trim().to_lowercase();
    for _ in 0..2 {
        let id = sqlx::query_scalar!(
            r#"
            WITH found AS (
                SELECT id FROM albums WHERE artist_id = $1 AND title_normalized = $3
            ), inserted AS (
                INSERT INTO albums (artist_id, title, title_normalized)
                SELECT $1, $2, $3
                WHERE NOT EXISTS (SELECT 1 FROM found)
                ON CONFLICT (artist_id, title_normalized) DO NOTHING
                RETURNING id
            )
            SELECT id AS "id!" FROM found
            UNION ALL
            SELECT id FROM inserted
            "#,
            artist_id,
            title.trim(),
            normalized,
        )
        .fetch_optional(pool)
        .await?;
        if let Some(id) = id {
            return Ok(id);
        }
    }
    Err(sqlx::Error::RowNotFound)
}

/// Looks up a track by `(artist_id, title_normalized)`, creating one if
/// none exists, like [`find_or_create_artist`]. An existing track only gains
/// the `album_id` and `duration_ms` it lacks; good data is never overwritten.
pub async fn find_or_create_track(
    pool: &PgPool,
    artist_id: i64,
    album_id: Option<i64>,
    title: &str,
    duration_ms: Option<i32>,
) -> Result<Track, sqlx::Error> {
    let normalized = title.trim().to_lowercase();
    let mut track = None;
    for _ in 0..2 {
        track = sqlx::query_as!(
            Track,
            r#"
            WITH found AS (
                SELECT id, artist_id, album_id, title, title_normalized, mbid,
                       duration_ms, scrobble_count, created_at
                FROM tracks
                WHERE artist_id = $1 AND title_normalized = $4
            ), inserted AS (
                INSERT INTO tracks (artist_id, album_id, title, title_normalized, duration_ms)
                SELECT $1, $2, $3, $4, $5
                WHERE NOT EXISTS (SELECT 1 FROM found)
                ON CONFLICT (artist_id, title_normalized) DO NOTHING
                RETURNING id, artist_id, album_id, title, title_normalized, mbid,
                          duration_ms, scrobble_count, created_at
            )
            SELECT id AS "id!", artist_id AS "artist_id!", album_id, title AS "title!",
                   title_normalized AS "title_normalized!", mbid, duration_ms,
                   scrobble_count AS "scrobble_count!", created_at AS "created_at!"
            FROM found
            UNION ALL
            SELECT id, artist_id, album_id, title, title_normalized, mbid,
                   duration_ms, scrobble_count, created_at
            FROM inserted
            "#,
            artist_id,
            album_id,
            title.trim(),
            normalized,
            duration_ms,
        )
        .fetch_optional(pool)
        .await?;
        if track.is_some() {
            break;
        }
    }
    let track = track.ok_or(sqlx::Error::RowNotFound)?;

    let gains = (track.album_id.is_none() && album_id.is_some())
        || (track.duration_ms.is_none() && duration_ms.is_some());
    if !gains {
        return Ok(track);
    }
    sqlx::query_as!(
        Track,
        r#"
        UPDATE tracks
        SET album_id    = COALESCE(album_id, $2),
            duration_ms = COALESCE(duration_ms, $3)
        WHERE id = $1
        RETURNING id, artist_id, album_id, title, title_normalized, mbid,
                  duration_ms, scrobble_count, created_at
        "#,
        track.id,
        album_id,
        duration_ms,
    )
    .fetch_one(pool)
    .await
}

/// Records the credit list for a track. Add-only: an existing credit is
/// never rewritten or dropped, so a client that omits a collaborator on one
/// submission cannot erase what an earlier, better-informed one reported.
pub async fn record_track_credits(
    pool: &PgPool,
    track_id: i64,
    primary_artist_id: i64,
    featured_artist_ids: &[i64],
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO track_artists (track_id, artist_id, role, position)
        SELECT $1::BIGINT, $2::BIGINT, 'primary'::track_artist_role, 0
        UNION ALL
        SELECT $1, credit.artist_id, 'featured'::track_artist_role, credit.ord::INT
        FROM UNNEST($3::BIGINT[]) WITH ORDINALITY AS credit(artist_id, ord)
        WHERE credit.artist_id <> $2
        ON CONFLICT (track_id, artist_id) DO NOTHING
        "#,
        track_id,
        primary_artist_id,
        featured_artist_ids,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn track_credits(pool: &PgPool, track_id: i64) -> Result<Vec<TrackCredit>, sqlx::Error> {
    sqlx::query_as!(
        TrackCredit,
        r#"
        SELECT ta.artist_id     AS "artist_id!",
               a.name           AS "name!",
               a.image_url      AS "image_url?",
               ta.role          AS "role!: TrackArtistRole",
               ta.position      AS "position!"
        FROM track_artists ta
        JOIN artists a ON a.id = ta.artist_id
        WHERE ta.track_id = $1
        ORDER BY ta.role, ta.position, a.name
        "#,
        track_id,
    )
    .fetch_all(pool)
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

/// The artist's most-scrobbled tracks across all users, from the daily
/// aggregate (migration 0015 indexes it by artist).
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
               s.play_count    AS "play_count!"
        FROM (
            SELECT track_id, SUM(play_count)::BIGINT AS play_count
            FROM scrobbles_daily_by_track
            WHERE artist_id = $1
            GROUP BY track_id
            ORDER BY play_count DESC, track_id
            LIMIT $2
        ) s
        JOIN tracks t       ON t.id = s.track_id
        JOIN artists a      ON a.id = t.artist_id
        LEFT JOIN albums al ON al.id = t.album_id
        ORDER BY s.play_count DESC, t.id
        "#,
        artist_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Users who listen to this artist most, private profiles excluded, from
/// the daily aggregate.
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
               l.play_count   AS "play_count!"
        FROM (
            SELECT user_id, SUM(play_count)::BIGINT AS play_count
            FROM scrobbles_daily_by_artist
            WHERE artist_id = $1
            GROUP BY user_id
        ) l
        JOIN users u ON u.id = l.user_id
        WHERE NOT u.is_private
        ORDER BY l.play_count DESC, u.id
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
               l.play_count   AS "play_count!"
        FROM (
            SELECT user_id, SUM(play_count)::BIGINT AS play_count
            FROM scrobbles_daily_by_track
            WHERE track_id = $1
            GROUP BY user_id
        ) l
        JOIN users u ON u.id = l.user_id
        WHERE NOT u.is_private
        ORDER BY l.play_count DESC, u.id
        LIMIT $2
        "#,
        track_id,
        limit,
    )
    .fetch_all(pool)
    .await
}
