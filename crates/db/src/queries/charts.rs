//! A user's charts: their top artists, tracks and albums over a range of
//! UTC days, from the daily aggregates (rows past the watermark, about the
//! last day, are read raw) and the classifier's flags, which take out
//! duplicates and say how many plays are unverified. Never the scrobbles
//! hypertable itself, whose compressed chunks would be decompressed.
//!
//! An album's plays are its tracks' (`tracks.album_id`), as in the album's
//! count.

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use sqlx::PgPool;

use shared::charts::ChartRange;
use shared::models::{TopAlbum, TopArtist, TopTrack};

/// A page of a chart, and how many entries the whole chart has.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub total: i64,
    pub entries: Vec<T>,
}

/// The aggregates' buckets the range covers: `[from, to)`, `from` `None`
/// for all of history.
fn bounds(range: &ChartRange) -> (Option<DateTime<Utc>>, DateTime<Utc>) {
    let start = |day: NaiveDate| day.and_time(NaiveTime::MIN).and_utc();
    (range.from.map(start), start(range.to) + TimeDelta::days(1))
}

pub async fn user_artists(
    pool: &PgPool,
    user_id: i64,
    range: &ChartRange,
    limit: i64,
    offset: i64,
) -> Result<Page<TopArtist>, sqlx::Error> {
    let (start, end) = bounds(range);
    let rows = sqlx::query!(
        r#"
        WITH flagged AS (
            SELECT t.artist_id,
                   count(*) FILTER (WHERE f.status = 'duplicate') AS duplicates,
                   count(*) FILTER (WHERE f.status IN ('suspect', 'no_data')) AS unverified
            FROM scrobble_flags f
            JOIN tracks t ON t.id = f.track_id
            WHERE f.user_id = $1 AND ($2::date IS NULL OR f.day >= $2) AND f.day <= $3
            GROUP BY t.artist_id
        ), plays AS (
            SELECT artist_id, sum(play_count)::bigint AS play_count
            FROM scrobbles_daily_by_artist
            WHERE user_id = $1 AND ($4::timestamptz IS NULL OR day >= $4) AND day < $5
            GROUP BY artist_id
        ), counted AS (
            SELECT p.artist_id, p.play_count - COALESCE(fl.duplicates, 0) AS play_count,
                   COALESCE(fl.unverified, 0) AS unverified
            FROM plays p
            LEFT JOIN flagged fl USING (artist_id)
            WHERE p.play_count > COALESCE(fl.duplicates, 0)
        )
        SELECT c.total AS "total!", e.artist_id AS "artist_id?", e.name AS "name?",
               e.image_url AS "image_url?", e.play_count AS "play_count?",
               e.unverified AS "unverified?"
        FROM (SELECT count(*) AS total FROM counted) c
        LEFT JOIN LATERAL (
            SELECT k.artist_id, a.name, a.image_url, k.play_count::bigint, k.unverified::bigint
            FROM counted k
            JOIN artists a ON a.id = k.artist_id
            ORDER BY k.play_count DESC, k.artist_id
            LIMIT $6 OFFSET $7
        ) e ON true
        ORDER BY e.play_count DESC, e.artist_id
        "#,
        user_id,
        range.from,
        range.to,
        start,
        end,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(Page {
        total: rows.first().map_or(0, |r| r.total),
        entries: rows
            .into_iter()
            .filter_map(|r| {
                Some(TopArtist {
                    artist_id: r.artist_id?,
                    artist_name: r.name?,
                    image_url: r.image_url,
                    play_count: r.play_count?,
                    unverified_count: r.unverified?,
                })
            })
            .collect(),
    })
}

/// A track's image is its album's cover.
pub async fn user_tracks(
    pool: &PgPool,
    user_id: i64,
    range: &ChartRange,
    limit: i64,
    offset: i64,
) -> Result<Page<TopTrack>, sqlx::Error> {
    let (start, end) = bounds(range);
    let rows = sqlx::query!(
        r#"
        WITH flagged AS (
            SELECT track_id,
                   count(*) FILTER (WHERE status = 'duplicate') AS duplicates,
                   count(*) FILTER (WHERE status IN ('suspect', 'no_data')) AS unverified
            FROM scrobble_flags
            WHERE user_id = $1 AND ($2::date IS NULL OR day >= $2) AND day <= $3
            GROUP BY track_id
        ), plays AS (
            SELECT track_id, sum(play_count)::bigint AS play_count
            FROM scrobbles_daily_by_track
            WHERE user_id = $1 AND ($4::timestamptz IS NULL OR day >= $4) AND day < $5
            GROUP BY track_id
        ), counted AS (
            SELECT p.track_id, p.play_count - COALESCE(fl.duplicates, 0) AS play_count,
                   COALESCE(fl.unverified, 0) AS unverified
            FROM plays p
            LEFT JOIN flagged fl USING (track_id)
            WHERE p.play_count > COALESCE(fl.duplicates, 0)
        )
        SELECT c.total AS "total!", e.track_id AS "track_id?", e.title AS "title?",
               e.artist_id AS "artist_id?", e.artist_name AS "artist_name?",
               e.album_image AS "album_image?", e.play_count AS "play_count?",
               e.unverified AS "unverified?"
        FROM (SELECT count(*) AS total FROM counted) c
        LEFT JOIN LATERAL (
            SELECT k.track_id, t.title, t.artist_id, a.name AS artist_name,
                   al.image_url AS album_image, k.play_count::bigint, k.unverified::bigint
            FROM counted k
            JOIN tracks t       ON t.id = k.track_id
            JOIN artists a      ON a.id = t.artist_id
            LEFT JOIN albums al ON al.id = t.album_id
            ORDER BY k.play_count DESC, k.track_id
            LIMIT $6 OFFSET $7
        ) e ON true
        ORDER BY e.play_count DESC, e.track_id
        "#,
        user_id,
        range.from,
        range.to,
        start,
        end,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(Page {
        total: rows.first().map_or(0, |r| r.total),
        entries: rows
            .into_iter()
            .filter_map(|r| {
                Some(TopTrack {
                    track_id: r.track_id?,
                    track_title: r.title?,
                    artist_id: r.artist_id?,
                    artist_name: r.artist_name?,
                    album_image: r.album_image,
                    play_count: r.play_count?,
                    unverified_count: r.unverified,
                })
            })
            .collect(),
    })
}

/// Albums by their tracks' plays; tracks the catalog files under no album
/// are left out.
pub async fn user_albums(
    pool: &PgPool,
    user_id: i64,
    range: &ChartRange,
    limit: i64,
    offset: i64,
) -> Result<Page<TopAlbum>, sqlx::Error> {
    let (start, end) = bounds(range);
    let rows = sqlx::query!(
        r#"
        WITH flagged AS (
            SELECT t.album_id,
                   count(*) FILTER (WHERE f.status = 'duplicate') AS duplicates,
                   count(*) FILTER (WHERE f.status IN ('suspect', 'no_data')) AS unverified
            FROM scrobble_flags f
            JOIN tracks t ON t.id = f.track_id
            WHERE f.user_id = $1 AND ($2::date IS NULL OR f.day >= $2) AND f.day <= $3
              AND t.album_id IS NOT NULL
            GROUP BY t.album_id
        ), per_track AS (
            SELECT track_id, sum(play_count)::bigint AS play_count
            FROM scrobbles_daily_by_track
            WHERE user_id = $1 AND ($4::timestamptz IS NULL OR day >= $4) AND day < $5
            GROUP BY track_id
        ), plays AS (
            SELECT t.album_id, sum(p.play_count)::bigint AS play_count
            FROM per_track p
            JOIN tracks t ON t.id = p.track_id
            WHERE t.album_id IS NOT NULL
            GROUP BY t.album_id
        ), counted AS (
            SELECT p.album_id, p.play_count - COALESCE(fl.duplicates, 0) AS play_count,
                   COALESCE(fl.unverified, 0) AS unverified
            FROM plays p
            LEFT JOIN flagged fl USING (album_id)
            WHERE p.play_count > COALESCE(fl.duplicates, 0)
        )
        SELECT c.total AS "total!", e.album_id AS "album_id?", e.title AS "title?",
               e.artist_id AS "artist_id?", e.artist_name AS "artist_name?",
               e.image_url AS "image_url?", e.play_count AS "play_count?",
               e.unverified AS "unverified?"
        FROM (SELECT count(*) AS total FROM counted) c
        LEFT JOIN LATERAL (
            SELECT k.album_id, al.title, al.artist_id, a.name AS artist_name, al.image_url,
                   k.play_count::bigint, k.unverified::bigint
            FROM counted k
            JOIN albums al ON al.id = k.album_id
            JOIN artists a ON a.id = al.artist_id
            ORDER BY k.play_count DESC, k.album_id
            LIMIT $6 OFFSET $7
        ) e ON true
        ORDER BY e.play_count DESC, e.album_id
        "#,
        user_id,
        range.from,
        range.to,
        start,
        end,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    Ok(Page {
        total: rows.first().map_or(0, |r| r.total),
        entries: rows
            .into_iter()
            .filter_map(|r| {
                Some(TopAlbum {
                    album_id: r.album_id?,
                    album_title: r.title?,
                    artist_id: r.artist_id?,
                    artist_name: r.artist_name?,
                    image_url: r.image_url,
                    play_count: r.play_count?,
                    unverified_count: r.unverified?,
                })
            })
            .collect(),
    })
}
