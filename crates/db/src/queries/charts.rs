//! A user's charts: their top artists, tracks and albums over a range of
//! UTC days, from the daily aggregates (rows past the watermark, about the
//! last day, are read raw) and the classifier's flags, which take out
//! duplicates and say how many plays are unverified. Never the scrobbles
//! hypertable itself, whose compressed chunks would be decompressed.
//!
//! An album's plays are its tracks' (`tracks.album_id`), as in the album's
//! count. A page is cut from the counted entities before their names are
//! looked up: the planner otherwise looks up every entity of a long
//! history first.

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
            FROM (SELECT * FROM counted ORDER BY play_count DESC, artist_id LIMIT $6 OFFSET $7) k
            JOIN artists a ON a.id = k.artist_id
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
            FROM (SELECT * FROM counted ORDER BY play_count DESC, track_id LIMIT $6 OFFSET $7) k
            JOIN tracks t       ON t.id = k.track_id
            JOIN artists a      ON a.id = t.artist_id
            LEFT JOIN albums al ON al.id = t.album_id
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
            FROM (SELECT * FROM counted ORDER BY play_count DESC, album_id LIMIT $6 OFFSET $7) k
            JOIN albums al ON al.id = k.album_id
            JOIN artists a ON a.id = al.artist_id
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

/// The accounts `viewer_id` follows and may see (public, not waiting to be
/// erased), the most recently active first: how many there are, and the
/// first `max` of them.
pub async fn community_accounts(
    pool: &PgPool,
    viewer_id: i64,
    max: i64,
) -> Result<(i64, Vec<i64>), sqlx::Error> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT u.id
        FROM user_follows f
        JOIN users u ON u.id = f.followee_id
        WHERE f.follower_id = $1 AND NOT u.is_private AND u.delete_after IS NULL
        ORDER BY u.last_seen_at DESC NULLS LAST, u.id
        "#,
        viewer_id,
    )
    .fetch_all(pool)
    .await?;
    let following = ids.len() as i64;
    Ok((
        following,
        ids.into_iter().take(max.max(0) as usize).collect(),
    ))
}

/// An entity's place in a community chart, before names: listener credits
/// and weights in thousandths, coordination factors applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ranked {
    pub entity_id: i64,
    pub listeners: i64,
    pub weight: i64,
}

/// What a community chart ranks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommunityKind {
    Artists,
    Tracks,
    Albums,
}

/// `accounts`' artists, tracks or albums over the UTC days `[from, to]`,
/// weighed as the global rankings are (`rankings::rankings`): each
/// account's weight for an entity, its listener credit (that weight against
/// `listener_weight`, the latest snapshot's, at most one), both times the
/// account's coordination factor for the days. Only entities credited with
/// `min_listeners` (thousandths) or more, by listeners then weight, the
/// first `max_positions`. Reads `ranking_daily` by account and day.
pub async fn community(
    pool: &PgPool,
    kind: CommunityKind,
    accounts: &[i64],
    from: NaiveDate,
    to: NaiveDate,
    min_listeners: i64,
    max_positions: i64,
) -> Result<Vec<Ranked>, sqlx::Error> {
    let default_listener_weight = shared::ranking::RankingParams::default().listener_weight();
    let full = shared::ranking::FULL;
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL enable_mergejoin = off")
        .execute(&mut *tx)
        .await?;
    let rows = match kind {
        CommunityKind::Artists | CommunityKind::Tracks => sqlx::query_as!(
            Ranked,
            r#"
            WITH lw AS (
                SELECT GREATEST(1, COALESCE(
                    (SELECT round((r.params->>'no_listened')::numeric * $6::bigint)::bigint
                     FROM ranking_snapshots s JOIN ranking_rulesets r ON r.id = s.ruleset_id
                     ORDER BY s.computed_at DESC LIMIT 1), $7)) AS weight
            ), per_user AS (
                SELECT user_id,
                       CASE WHEN $4 THEN track_id ELSE artist_id END AS entity_id,
                       sum(weight)::bigint AS weight
                FROM ranking_daily
                WHERE user_id = ANY($1) AND day BETWEEN $2 AND $3
                GROUP BY 1, 2
            ), factors AS (
                SELECT c.user_id,
                       ($6::bigint - sum(r.weight::bigint * ($6::bigint - c.factor))::bigint
                                     / NULLIF(t.weight, 0))::bigint AS factor
                FROM coordination_days c
                JOIN ranking_days r ON r.user_id = c.user_id AND r.day = c.day
                JOIN (SELECT user_id, sum(weight)::bigint AS weight FROM ranking_days
                      WHERE user_id = ANY($1) AND day BETWEEN $2 AND $3 GROUP BY 1) t
                  ON t.user_id = c.user_id
                WHERE c.user_id = ANY($1) AND c.day BETWEEN $2 AND $3
                GROUP BY c.user_id, t.weight
            )
            SELECT p.entity_id AS "entity_id!",
                   sum(LEAST($6::bigint, p.weight * $6 / lw.weight) * COALESCE(f.factor, $6) / $6)::bigint
                       AS "listeners!",
                   sum(p.weight * COALESCE(f.factor, $6) / $6)::bigint AS "weight!"
            FROM per_user p
            CROSS JOIN lw
            LEFT JOIN factors f USING (user_id)
            GROUP BY p.entity_id
            HAVING sum(LEAST($6::bigint, p.weight * $6 / lw.weight) * COALESCE(f.factor, $6) / $6)::bigint
                   >= $5::bigint
            ORDER BY 2 DESC, 3 DESC, 1
            LIMIT $8
            "#,
            accounts,
            from,
            to,
            kind == CommunityKind::Tracks,
            min_listeners,
            full,
            default_listener_weight,
            max_positions,
        )
        .fetch_all(&mut *tx)
        .await?,
        CommunityKind::Albums => sqlx::query_as!(
            Ranked,
            r#"
            WITH lw AS (
                SELECT GREATEST(1, COALESCE(
                    (SELECT round((r.params->>'no_listened')::numeric * $5::bigint)::bigint
                     FROM ranking_snapshots s JOIN ranking_rulesets r ON r.id = s.ruleset_id
                     ORDER BY s.computed_at DESC LIMIT 1), $6)) AS weight
            ), per_track AS (
                SELECT user_id, track_id, sum(weight)::bigint AS weight
                FROM ranking_daily
                WHERE user_id = ANY($1) AND day BETWEEN $2 AND $3
                GROUP BY 1, 2
            ), per_user AS (
                SELECT p.user_id, t.album_id AS entity_id, sum(p.weight)::bigint AS weight
                FROM per_track p
                JOIN tracks t ON t.id = p.track_id
                WHERE t.album_id IS NOT NULL
                GROUP BY 1, 2
            ), factors AS (
                SELECT c.user_id,
                       ($5::bigint - sum(r.weight::bigint * ($5::bigint - c.factor))::bigint
                                     / NULLIF(t.weight, 0))::bigint AS factor
                FROM coordination_days c
                JOIN ranking_days r ON r.user_id = c.user_id AND r.day = c.day
                JOIN (SELECT user_id, sum(weight)::bigint AS weight FROM ranking_days
                      WHERE user_id = ANY($1) AND day BETWEEN $2 AND $3 GROUP BY 1) t
                  ON t.user_id = c.user_id
                WHERE c.user_id = ANY($1) AND c.day BETWEEN $2 AND $3
                GROUP BY c.user_id, t.weight
            )
            SELECT p.entity_id AS "entity_id!",
                   sum(LEAST($5::bigint, p.weight * $5 / lw.weight) * COALESCE(f.factor, $5) / $5)::bigint
                       AS "listeners!",
                   sum(p.weight * COALESCE(f.factor, $5) / $5)::bigint AS "weight!"
            FROM per_user p
            CROSS JOIN lw
            LEFT JOIN factors f USING (user_id)
            GROUP BY p.entity_id
            HAVING sum(LEAST($5::bigint, p.weight * $5 / lw.weight) * COALESCE(f.factor, $5) / $5)::bigint
                   >= $4::bigint
            ORDER BY 2 DESC, 3 DESC, 1
            LIMIT $7
            "#,
            accounts,
            from,
            to,
            min_listeners,
            full,
            default_listener_weight,
            max_positions,
        )
        .fetch_all(&mut *tx)
        .await?,
    };
    tx.commit().await?;
    Ok(rows)
}

/// Names and images of a community chart's entities, by id: for an artist
/// its own, for a track its artist's name and album's cover, for an album
/// its artist's name. Entities deleted since are missing.
#[derive(Debug, Clone)]
pub struct Named {
    pub entity_id: i64,
    pub name: String,
    pub artist_id: i64,
    pub artist_name: String,
    pub image: Option<String>,
}

pub async fn names(
    pool: &PgPool,
    kind: CommunityKind,
    ids: &[i64],
) -> Result<Vec<Named>, sqlx::Error> {
    match kind {
        CommunityKind::Artists => {
            sqlx::query_as!(
                Named,
                r#"
                SELECT id AS "entity_id!", name AS "name!", id AS "artist_id!",
                       name AS "artist_name!", image_url AS image
                FROM artists WHERE id = ANY($1)
                "#,
                ids,
            )
            .fetch_all(pool)
            .await
        }
        CommunityKind::Tracks => {
            sqlx::query_as!(
                Named,
                r#"
                SELECT t.id AS "entity_id!", t.title AS "name!", t.artist_id AS "artist_id!",
                       a.name AS "artist_name!", al.image_url AS "image?"
                FROM tracks t
                JOIN artists a ON a.id = t.artist_id
                LEFT JOIN albums al ON al.id = t.album_id
                WHERE t.id = ANY($1)
                "#,
                ids,
            )
            .fetch_all(pool)
            .await
        }
        CommunityKind::Albums => {
            sqlx::query_as!(
                Named,
                r#"
                SELECT al.id AS "entity_id!", al.title AS "name!", al.artist_id AS "artist_id!",
                       a.name AS "artist_name!", al.image_url AS image
                FROM albums al
                JOIN artists a ON a.id = al.artist_id
                WHERE al.id = ANY($1)
                "#,
                ids,
            )
            .fetch_all(pool)
            .await
        }
    }
}
