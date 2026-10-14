use sqlx::PgPool;
use uuid::Uuid;

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
    pub recording_mbid: Option<Uuid>,
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
    if let Some(hint) = input.recording_mbid
        && track.mbid.is_none()
    {
        record_mbid_hint(pool, track.id, hint).await?;
    }

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

/// Keeps the MusicBrainz recording id a client sent as the track's hint,
/// unless it has an mbid or a hint already. A track enriched without a
/// match goes back in the queue, once: the first hint wins, so a client
/// can't make it look up a new id every scrobble.
pub async fn record_mbid_hint(pool: &PgPool, track_id: i64, hint: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        WITH hinted AS (
            UPDATE tracks SET mbid_hint = $2
            WHERE id = $1 AND mbid IS NULL AND mbid_hint IS NULL
            RETURNING id, enriched_at
        )
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority)
        SELECT 'track', id, $3 FROM hinted WHERE enriched_at IS NOT NULL
        ON CONFLICT (entity_type, entity_id) DO UPDATE
            SET status = 'pending', attempts = 0, next_attempt_at = NOW(),
                priority = EXCLUDED.priority, last_error = NULL
            WHERE enrichment_jobs.status IN ('done', 'failed')
        "#,
        track_id,
        hint,
        enrichment_db::PRIORITY_INGEST,
    )
    .execute(pool)
    .await?;
    Ok(())
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
                SELECT id, name, name_normalized, mbid, image_url, bio, created_at
                FROM artists
                WHERE name_normalized = $2
            ), inserted AS (
                INSERT INTO artists (name, name_normalized)
                SELECT $1, $2
                WHERE NOT EXISTS (SELECT 1 FROM found)
                ON CONFLICT (name_normalized) DO NOTHING
                RETURNING id, name, name_normalized, mbid, image_url, bio, created_at
            )
            SELECT id AS "id!", name AS "name!", name_normalized AS "name_normalized!",
                   mbid, image_url, bio, NULL::bigint AS "scrobble_count?",
                   NULL::bigint AS "listener_count?", created_at AS "created_at!"
            FROM found
            UNION ALL
            SELECT id, name, name_normalized, mbid, image_url, bio, NULL, NULL, created_at
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

/// The track `artist` and `title` name, normalized as ingest normalizes
/// them, if the catalog has it. Creates nothing.
pub async fn find_track_id(
    pool: &PgPool,
    artist: &str,
    title: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT t.id FROM tracks t
        JOIN artists a ON a.id = t.artist_id
        WHERE a.name_normalized = $1 AND t.title_normalized = $2
        "#,
        artist.trim().to_lowercase(),
        title.trim().to_lowercase(),
    )
    .fetch_optional(pool)
    .await
}

/// The track each recording MBID names: the one MusicBrainz enrichment
/// matched it to, else one a client named it for that enrichment hasn't
/// decided yet (nor rejected that MBID for), the most played first. MBIDs
/// naming no track are left out. Creates nothing.
pub async fn find_by_recording_mbids(
    pool: &PgPool,
    mbids: &[Uuid],
) -> Result<Vec<(Uuid, i64)>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT DISTINCT ON (m.mbid) m.mbid AS "mbid!", c.id AS "track_id!"
        FROM UNNEST($1::uuid[]) AS m(mbid)
        CROSS JOIN LATERAL (
            SELECT t.id, 0 AS rank, 0::bigint AS plays FROM tracks t WHERE t.mbid = m.mbid
            UNION ALL
            SELECT t.id, 1, t.scrobble_count FROM tracks t
            WHERE t.mbid_hint = m.mbid AND t.mbid IS NULL
              AND NOT EXISTS (SELECT 1 FROM track_mbid_rejections r
                              WHERE r.track_id = t.id AND r.mbid = m.mbid)
        ) c
        ORDER BY m.mbid, c.rank, c.plays DESC, c.id
        "#,
        mbids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.mbid, r.track_id)).collect())
}

/// What `track.getInfo` shows of a track: its catalog album, the longest
/// known length, and its play count only when at least `min_listeners`
/// users played it.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub id: i64,
    pub title: String,
    pub mbid: Option<Uuid>,
    pub duration_ms: Option<i32>,
    pub artist_name: String,
    pub artist_mbid: Option<Uuid>,
    pub album_title: Option<String>,
    pub album_mbid: Option<Uuid>,
    pub album_image: Option<String>,
    pub album_artist: Option<String>,
    pub scrobble_count: Option<i64>,
}

pub async fn track_info(
    pool: &PgPool,
    id: i64,
    min_listeners: i32,
) -> Result<Option<TrackInfo>, sqlx::Error> {
    sqlx::query_as!(
        TrackInfo,
        r#"
        SELECT t.id, t.title, t.mbid,
               COALESCE(t.duration_ms, t.mb_duration_ms, t.deezer_duration_ms) AS duration_ms,
               a.name AS artist_name, a.mbid AS artist_mbid,
               al.title AS "album_title?", al.mbid AS "album_mbid?",
               al.image_url AS "album_image?", aa.name AS "album_artist?",
               CASE WHEN track_has_listeners(t.id, $2) THEN t.scrobble_count END
                   AS "scrobble_count?"
        FROM tracks t
        JOIN artists a ON a.id = t.artist_id
        LEFT JOIN albums al ON al.id = t.album_id
        LEFT JOIN artists aa ON aa.id = al.artist_id
        WHERE t.id = $1
        "#,
        id,
        min_listeners,
    )
    .fetch_optional(pool)
    .await
}

/// Without counts: for lookups, not responses.
pub async fn find_artist_by_id(pool: &PgPool, id: i64) -> Result<Option<Artist>, sqlx::Error> {
    sqlx::query_as!(
        Artist,
        r#"
        SELECT id, name, name_normalized, mbid, image_url, bio,
               NULL::bigint AS "scrobble_count?", NULL::bigint AS "listener_count?", created_at
        FROM artists
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Its play count only when at least `min_listeners` users played it.
pub async fn public_artist(
    pool: &PgPool,
    id: i64,
    min_listeners: i32,
) -> Result<Option<Artist>, sqlx::Error> {
    sqlx::query_as!(
        Artist,
        r#"
        SELECT id, name, name_normalized, mbid, image_url, bio,
               CASE WHEN artist_has_listeners(id, $2) THEN scrobble_count END
                   AS "scrobble_count?",
               NULL::bigint AS "listener_count?", created_at
        FROM artists
        WHERE id = $1
        "#,
        id,
        min_listeners,
    )
    .fetch_optional(pool)
    .await
}

/// Searches artists by fuzzy name match using pg_trgm similarity, counts
/// as [`public_artist`] serves them.
///
/// Results are ranked by trigram similarity first, then by global popularity
/// (`scrobble_count`) as a tiebreaker. The `%` operator applies a minimum
/// similarity threshold (default 0.3) set via `pg_trgm.similarity_threshold`.
pub async fn search_artists(
    pool: &PgPool,
    query: &str,
    limit: i64,
    min_listeners: i32,
) -> Result<Vec<Artist>, sqlx::Error> {
    sqlx::query_as!(
        Artist,
        r#"
        SELECT id, name, name_normalized, mbid, image_url, bio,
               CASE WHEN artist_has_listeners(id, $3) THEN scrobble_count END
                   AS "scrobble_count?",
               NULL::bigint AS "listener_count?", created_at
        FROM (
            SELECT * FROM artists
            WHERE name % $1
            ORDER BY similarity(name, $1) DESC, scrobble_count DESC
            LIMIT $2
        ) a
        ORDER BY similarity(name, $1) DESC, a.scrobble_count DESC
        "#,
        query,
        limit,
        min_listeners,
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
                       duration_ms, created_at
                FROM tracks
                WHERE artist_id = $1 AND title_normalized = $4
            ), inserted AS (
                INSERT INTO tracks (artist_id, album_id, title, title_normalized, duration_ms)
                SELECT $1, $2, $3, $4, $5
                WHERE NOT EXISTS (SELECT 1 FROM found)
                ON CONFLICT (artist_id, title_normalized) DO NOTHING
                RETURNING id, artist_id, album_id, title, title_normalized, mbid,
                          duration_ms, created_at
            )
            SELECT id AS "id!", artist_id AS "artist_id!", album_id, title AS "title!",
                   title_normalized AS "title_normalized!", mbid, duration_ms,
                   NULL::bigint AS "scrobble_count?", created_at AS "created_at!"
            FROM found
            UNION ALL
            SELECT id, artist_id, album_id, title, title_normalized, mbid,
                   duration_ms, NULL, created_at
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
                  duration_ms, NULL::bigint AS "scrobble_count?", created_at
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

/// Without its count: for lookups, not responses.
pub async fn find_album_by_id(pool: &PgPool, id: i64) -> Result<Option<Album>, sqlx::Error> {
    sqlx::query_as!(
        Album,
        r#"
        SELECT al.id, al.artist_id, a.name AS artist_name, al.title, al.title_normalized,
               al.mbid, al.image_url, al.release_date,
               NULL::bigint AS "scrobble_count?", al.created_at
        FROM albums al
        JOIN artists a ON a.id = al.artist_id
        WHERE al.id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// With the plays of its tracks when at least `min_listeners` users played
/// them.
pub async fn public_album(
    pool: &PgPool,
    id: i64,
    min_listeners: i32,
) -> Result<Option<Album>, sqlx::Error> {
    sqlx::query_as!(
        Album,
        r#"
        SELECT al.id, al.artist_id, a.name AS artist_name, al.title, al.title_normalized,
               al.mbid, al.image_url, al.release_date,
               CASE WHEN album_has_listeners(al.id, $2)
                    THEN (SELECT COALESCE(sum(t.scrobble_count), 0)::bigint
                          FROM tracks t WHERE t.album_id = al.id) END AS "scrobble_count?",
               al.created_at
        FROM albums al
        JOIN artists a ON a.id = al.artist_id
        WHERE al.id = $1
        "#,
        id,
        min_listeners,
    )
    .fetch_optional(pool)
    .await
}

/// Without its count: for lookups, not responses.
pub async fn find_track_by_id(pool: &PgPool, id: i64) -> Result<Option<Track>, sqlx::Error> {
    sqlx::query_as!(
        Track,
        r#"
        SELECT id, artist_id, album_id, title, title_normalized, mbid,
               duration_ms, NULL::bigint AS "scrobble_count?", created_at
        FROM tracks
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Its play count only when at least `min_listeners` users played it.
pub async fn public_track(
    pool: &PgPool,
    id: i64,
    min_listeners: i32,
) -> Result<Option<Track>, sqlx::Error> {
    sqlx::query_as!(
        Track,
        r#"
        SELECT id, artist_id, album_id, title, title_normalized, mbid, duration_ms,
               CASE WHEN track_has_listeners(id, $2) THEN scrobble_count END
                   AS "scrobble_count?",
               created_at
        FROM tracks
        WHERE id = $1
        "#,
        id,
        min_listeners,
    )
    .fetch_optional(pool)
    .await
}

/// See [`search_artists`] — same ranking strategy applied to track titles.
pub async fn search_tracks(
    pool: &PgPool,
    query: &str,
    limit: i64,
    min_listeners: i32,
) -> Result<Vec<Track>, sqlx::Error> {
    sqlx::query_as!(
        Track,
        r#"
        SELECT id, artist_id, album_id, title, title_normalized, mbid, duration_ms,
               CASE WHEN track_has_listeners(id, $3) THEN scrobble_count END
                   AS "scrobble_count?",
               created_at
        FROM (
            SELECT * FROM tracks
            WHERE title % $1
            ORDER BY similarity(title, $1) DESC, scrobble_count DESC
            LIMIT $2
        ) t
        ORDER BY similarity(title, $1) DESC, t.scrobble_count DESC
        "#,
        query,
        limit,
        min_listeners,
    )
    .fetch_all(pool)
    .await
}

const TOP_TRACK_CANDIDATES: i64 = 200;

/// The artist's most-scrobbled tracks across all users, from the daily
/// aggregate, among its [`TOP_TRACK_CANDIDATES`] most played, leaving out
/// tracks fewer than `min_listeners` users played.
pub async fn artist_top_tracks(
    pool: &PgPool,
    artist_id: i64,
    limit: i64,
    min_listeners: i32,
) -> Result<Vec<TopTrack>, sqlx::Error> {
    sqlx::query_as!(
        TopTrack,
        r#"
        SELECT t.id            AS "track_id!",
               t.title         AS "track_title!",
               t.artist_id     AS "artist_id!",
               a.name          AS "artist_name!",
               al.image_url    AS "album_image?",
               s.play_count    AS "play_count!",
               NULL::BIGINT    AS "unverified_count?"
        FROM (
            SELECT c.track_id, c.play_count
            FROM (
                SELECT track_id, SUM(play_count)::BIGINT AS play_count
                FROM scrobbles_daily_by_track
                WHERE artist_id = $1
                GROUP BY track_id
                ORDER BY play_count DESC, track_id
                LIMIT $4
            ) c
            WHERE track_has_listeners(c.track_id, $3)
            ORDER BY c.play_count DESC, c.track_id
            LIMIT $2
        ) s
        JOIN tracks t       ON t.id = s.track_id
        JOIN artists a      ON a.id = t.artist_id
        LEFT JOIN albums al ON al.id = t.album_id
        ORDER BY s.play_count DESC, t.id
        "#,
        artist_id,
        limit,
        min_listeners,
        TOP_TRACK_CANDIDATES,
    )
    .fetch_all(pool)
    .await
}

/// Users who listen to this artist most, private profiles, accounts
/// waiting to be erased and accounts blocking `viewer_id` or blocked by it
/// excluded, from the daily aggregate.
pub async fn artist_listeners(
    pool: &PgPool,
    artist_id: i64,
    limit: i64,
    viewer_id: Option<i64>,
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
        WHERE NOT u.is_private AND u.delete_after IS NULL
          AND ($3::bigint IS NULL OR NOT EXISTS (
                  SELECT 1 FROM user_blocks b
                  WHERE (b.blocker_id = u.id AND b.blocked_id = $3)
                     OR (b.blocker_id = $3 AND b.blocked_id = u.id)))
        ORDER BY l.play_count DESC, u.id
        LIMIT $2
        "#,
        artist_id,
        limit,
        viewer_id,
    )
    .fetch_all(pool)
    .await
}

/// See [`artist_listeners`], scoped to a single track.
pub async fn track_listeners(
    pool: &PgPool,
    track_id: i64,
    limit: i64,
    viewer_id: Option<i64>,
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
        WHERE NOT u.is_private AND u.delete_after IS NULL
          AND ($3::bigint IS NULL OR NOT EXISTS (
                  SELECT 1 FROM user_blocks b
                  WHERE (b.blocker_id = u.id AND b.blocked_id = $3)
                     OR (b.blocker_id = $3 AND b.blocked_id = u.id)))
        ORDER BY l.play_count DESC, u.id
        LIMIT $2
        "#,
        track_id,
        limit,
        viewer_id,
    )
    .fetch_all(pool)
    .await
}
