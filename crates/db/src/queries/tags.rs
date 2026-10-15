//! Tags (migration 0038; docs/social.md, Tags): a user's own, and the
//! weighted counts the worker keeps for everyone to see. Nothing else
//! reads tags: rankings, similarity and recommendations ignore them.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgExecutor, PgPool};

use shared::community;
use shared::cursor::Cursor;
use shared::models::{CatalogKind, TaggedEntity, UserTag, UserTagCount};

pub const SOURCE_SCROBBLR: &str = "scrobblr";
pub const SOURCE_LASTFM: &str = "lastfm";

/// A changed entity waits this long, so a burst of tags costs one pass.
pub const SETTLE_SECS: f64 = 30.0;

pub async fn blocklist(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar!("SELECT term FROM tag_blocklist ORDER BY term")
        .fetch_all(pool)
        .await
}

pub async fn tag_id(pool: &PgPool, name: &str) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar!("SELECT id FROM tags WHERE name = $1", name)
        .fetch_optional(pool)
        .await
}

pub(crate) async fn enqueue<'e>(
    db: impl PgExecutor<'e>,
    kind: &str,
    entity_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO tag_queue (entity_type, entity_id, due_at)
        VALUES ($1, $2, NOW() + make_interval(secs => $3))
        ON CONFLICT (entity_type, entity_id) DO NOTHING
        "#,
        kind,
        entity_id,
        SETTLE_SECS,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Queues what `user_id` tagged: its weight changes (a deletion request, a
/// restore, an erase, a suspension).
pub async fn enqueue_user<'e>(db: impl PgExecutor<'e>, user_id: i64) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        r#"
        INSERT INTO tag_queue (entity_type, entity_id)
        SELECT DISTINCT entity_type, entity_id FROM user_tags WHERE user_id = $1
        ON CONFLICT (entity_type, entity_id) DO UPDATE SET due_at = LEAST(tag_queue.due_at, NOW())
        "#,
        user_id,
    )
    .execute(db)
    .await?
    .rows_affected())
}

/// Queues every entity with tags or counts, for a full recount.
pub async fn enqueue_all(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        r#"
        INSERT INTO tag_queue (entity_type, entity_id)
        SELECT entity_type, entity_id FROM user_tags
        UNION
        SELECT entity_type, entity_id FROM tag_counts
        ON CONFLICT (entity_type, entity_id) DO UPDATE SET due_at = LEAST(tag_queue.due_at, NOW())
        "#,
    )
    .execute(pool)
    .await?
    .rows_affected())
}

/// The user's tags on the entity, by name.
pub async fn of_entity<'e>(
    db: impl PgExecutor<'e>,
    user_id: i64,
    kind: CatalogKind,
    entity_id: i64,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT t.name FROM user_tags ut JOIN tags t ON t.id = ut.tag_id
        WHERE ut.user_id = $1 AND ut.entity_type = $2 AND ut.entity_id = $3
        ORDER BY t.name
        "#,
        user_id,
        kind.as_str(),
        entity_id,
    )
    .fetch_all(db)
    .await
}

/// Where a tagging came from.
#[derive(Debug, Clone, Copy)]
pub struct TagSource {
    pub source: &'static str,
    pub client_id: Option<i32>,
}

impl TagSource {
    pub const NATIVE: TagSource = TagSource {
        source: SOURCE_SCROBBLR,
        client_id: None,
    };
}

/// What adding tags came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Added {
    /// The user's tags on the entity now.
    Tags(Vec<String>),
    /// More than [`community::MAX_TAGS_PER_ENTITY`] of the user's on it.
    TooManyHere,
    /// The entity would carry more than [`community::MAX_ENTITY_TAGS`]
    /// distinct tags.
    EntityFull,
    /// The user would hold more than [`community::MAX_TAGGINGS`].
    AtLimit,
}

/// Adds `names` (normalized, not blocked) to the user's tags on the entity;
/// tags already there change nothing.
pub async fn add(
    pool: &PgPool,
    user_id: i64,
    kind: CatalogKind,
    entity_id: i64,
    names: &[String],
    from: TagSource,
) -> Result<Added, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // The user's caps, then the entity's, always in that order.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('tags:' || $1, 0))")
        .bind(user_id.to_string())
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('tags:' || $1, 0))")
        .bind(format!("{}:{entity_id}", kind.as_str()))
        .execute(&mut *tx)
        .await?;
    let mine = of_entity(&mut *tx, user_id, kind, entity_id).await?;
    let mut new: Vec<&String> = names.iter().filter(|n| !mine.contains(n)).collect();
    new.sort();
    new.dedup();
    if new.is_empty() {
        return Ok(Added::Tags(mine));
    }
    if (mine.len() + new.len()) as i64 > community::MAX_TAGS_PER_ENTITY {
        return Ok(Added::TooManyHere);
    }
    let counts = sqlx::query!(
        r#"
        SELECT (SELECT count(*) FROM user_tags WHERE user_id = $1) AS "held!",
               (SELECT count(DISTINCT tag_id) FROM user_tags
                WHERE entity_type = $2 AND entity_id = $3) AS "on_entity!",
               (SELECT count(*) FROM UNNEST($4::text[]) AS n(name)
                WHERE NOT EXISTS (SELECT 1 FROM user_tags ut JOIN tags t ON t.id = ut.tag_id
                                  WHERE ut.entity_type = $2 AND ut.entity_id = $3
                                    AND t.name = n.name)) AS "new_here!"
        "#,
        user_id,
        kind.as_str(),
        entity_id,
        &new.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
    )
    .fetch_one(&mut *tx)
    .await?;
    if counts.held + new.len() as i64 > community::MAX_TAGGINGS {
        return Ok(Added::AtLimit);
    }
    if counts.on_entity + counts.new_here > community::MAX_ENTITY_TAGS {
        return Ok(Added::EntityFull);
    }
    let new: Vec<String> = new.into_iter().cloned().collect();
    sqlx::query!(
        r#"
        WITH named AS (
            INSERT INTO tags (name) SELECT UNNEST($1::text[])
            ON CONFLICT (name) DO NOTHING
            RETURNING id
        )
        INSERT INTO user_tags (user_id, entity_type, entity_id, tag_id, source, client_id)
        SELECT $2, $3, $4, id, $5, $6
        FROM (SELECT id FROM named UNION SELECT id FROM tags WHERE name = ANY($1)) ids
        ON CONFLICT DO NOTHING
        "#,
        &new,
        user_id,
        kind.as_str(),
        entity_id,
        from.source,
        from.client_id,
    )
    .execute(&mut *tx)
    .await?;
    enqueue(&mut *tx, kind.as_str(), entity_id).await?;
    let mine = of_entity(&mut *tx, user_id, kind, entity_id).await?;
    tx.commit().await?;
    Ok(Added::Tags(mine))
}

/// Whether the user had the tag there to take off.
pub async fn remove(
    pool: &PgPool,
    user_id: i64,
    kind: CatalogKind,
    entity_id: i64,
    name: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query!(
        r#"
        DELETE FROM user_tags
        WHERE user_id = $1 AND entity_type = $2 AND entity_id = $3
          AND tag_id = (SELECT id FROM tags WHERE name = $4)
        "#,
        user_id,
        kind.as_str(),
        entity_id,
        name,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if removed {
        enqueue(&mut *tx, kind.as_str(), entity_id).await?;
    }
    tx.commit().await?;
    Ok(removed)
}

struct UserTagRow {
    id: i64,
    tag: String,
    entity_type: String,
    entity_id: i64,
    name: Option<String>,
    artist_id: Option<i64>,
    artist_name: Option<String>,
    image_url: Option<String>,
    tagged_at: DateTime<Utc>,
}

/// The user's tags before `before`, newest first, on one entity if `on`
/// names it. With each, the cursor of its entry.
pub async fn user_page(
    pool: &PgPool,
    user_id: i64,
    on: Option<(CatalogKind, i64)>,
    before: Option<Cursor>,
    limit: i64,
) -> Result<Vec<(UserTag, Cursor)>, sqlx::Error> {
    let rows = sqlx::query_as!(
        UserTagRow,
        r#"
        SELECT ut.id, t.name AS tag, ut.entity_type, ut.entity_id,
               COALESCE(a.name, al.title, tr.title) AS name,
               COALESCE(al.artist_id, tr.artist_id) AS artist_id,
               COALESCE(ala.name, tra.name) AS artist_name,
               COALESCE(a.image_url, al.image_url, tal.image_url, tra.image_url) AS image_url,
               ut.created_at AS tagged_at
        FROM user_tags ut
        JOIN tags t ON t.id = ut.tag_id
        LEFT JOIN artists a  ON ut.entity_type = 'artist' AND a.id = ut.entity_id
        LEFT JOIN albums al  ON ut.entity_type = 'album' AND al.id = ut.entity_id
        LEFT JOIN artists ala ON ala.id = al.artist_id
        LEFT JOIN tracks tr  ON ut.entity_type = 'track' AND tr.id = ut.entity_id
        LEFT JOIN artists tra ON tra.id = tr.artist_id
        LEFT JOIN albums tal ON tal.id = tr.album_id
        WHERE ut.user_id = $1
          AND ($2::text IS NULL OR (ut.entity_type = $2 AND ut.entity_id = $3))
          AND ($4::timestamptz IS NULL OR (ut.created_at, ut.id) < ($4, $5))
        ORDER BY ut.created_at DESC, ut.id DESC
        LIMIT $6
        "#,
        user_id,
        on.map(|(k, _)| k.as_str()),
        on.map_or(0, |(_, id)| id),
        before.map(|c| c.at),
        before.map_or(0, |c| c.id),
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let cursor = Cursor {
                at: r.tagged_at,
                id: r.id,
            };
            Some((
                UserTag {
                    tag: r.tag,
                    entity_type: CatalogKind::parse(&r.entity_type)?,
                    entity_id: r.entity_id,
                    name: r.name?,
                    artist_id: r.artist_id,
                    artist_name: r.artist_name,
                    image_url: r.image_url,
                    tagged_at: r.tagged_at,
                },
                cursor,
            ))
        })
        .collect())
}

/// The tags the user uses, most used first.
pub async fn user_counts(
    pool: &PgPool,
    user_id: i64,
    limit: i64,
) -> Result<Vec<UserTagCount>, sqlx::Error> {
    sqlx::query_as!(
        UserTagCount,
        r#"
        SELECT t.name, c.count AS "count!"
        FROM (SELECT tag_id, count(*) AS count FROM user_tags WHERE user_id = $1
              GROUP BY tag_id ORDER BY count(*) DESC LIMIT $2) c
        JOIN tags t ON t.id = c.tag_id
        ORDER BY c.count DESC, t.name
        "#,
        user_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// The entity's tags with at least `min_weight`, heaviest first: name and
/// weight in thousandths.
pub async fn top_tags(
    pool: &PgPool,
    kind: CatalogKind,
    entity_id: i64,
    min_weight: i64,
    limit: i64,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT t.name, c.weight
        FROM tag_counts c JOIN tags t ON t.id = c.tag_id
        WHERE c.entity_type = $1 AND c.entity_id = $2 AND c.weight >= $3
        ORDER BY c.weight DESC, c.tag_id
        LIMIT $4
        "#,
        kind.as_str(),
        entity_id,
        min_weight,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.name, r.weight)).collect())
}

/// The entities of `kind` carrying the tag with at least `min_weight`,
/// heaviest first, and how many there are.
pub async fn top_entities(
    pool: &PgPool,
    tag_id: i64,
    kind: CatalogKind,
    min_weight: i64,
    limit: i64,
    offset: i64,
) -> Result<(Vec<TaggedEntity>, i64), sqlx::Error> {
    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "n!" FROM tag_counts
        WHERE tag_id = $1 AND entity_type = $2 AND weight >= $3
        "#,
        tag_id,
        kind.as_str(),
        min_weight,
    )
    .fetch_one(pool)
    .await?;
    let rows = sqlx::query!(
        r#"
        SELECT c.entity_id, c.weight,
               COALESCE(a.name, al.title, tr.title) AS name,
               COALESCE(al.artist_id, tr.artist_id) AS artist_id,
               COALESCE(ala.name, tra.name) AS artist_name,
               COALESCE(a.image_url, al.image_url, tal.image_url, tra.image_url) AS image_url
        FROM (
            SELECT entity_id, weight FROM tag_counts
            WHERE tag_id = $1 AND entity_type = $2 AND weight >= $3
            ORDER BY weight DESC, entity_id
            LIMIT $4 OFFSET $5
        ) c
        LEFT JOIN artists a  ON $2 = 'artist' AND a.id = c.entity_id
        LEFT JOIN albums al  ON $2 = 'album' AND al.id = c.entity_id
        LEFT JOIN artists ala ON ala.id = al.artist_id
        LEFT JOIN tracks tr  ON $2 = 'track' AND tr.id = c.entity_id
        LEFT JOIN artists tra ON tra.id = tr.artist_id
        LEFT JOIN albums tal ON tal.id = tr.album_id
        ORDER BY c.weight DESC, c.entity_id
        "#,
        tag_id,
        kind.as_str(),
        min_weight,
        limit,
        offset,
    )
    .fetch_all(pool)
    .await?;
    let entries = rows
        .into_iter()
        .zip(offset + 1..)
        .filter_map(|(r, position)| {
            Some(TaggedEntity {
                position,
                entity_type: kind,
                id: r.entity_id,
                name: r.name?,
                artist_id: r.artist_id,
                artist_name: r.artist_name,
                image_url: r.image_url,
                taggers: shared::ranking::whole(r.weight),
            })
        })
        .collect();
    Ok((entries, total))
}

/// Who weighs in the counts: accounts created before `created_before`, at
/// their lowest coordination factor since `factor_since`.
#[derive(Debug, Clone, Copy)]
pub struct Weighing {
    pub created_before: DateTime<Utc>,
    pub factor_since: chrono::NaiveDate,
}

impl Weighing {
    pub fn at(now: DateTime<Utc>) -> Self {
        Self {
            created_before: now - chrono::TimeDelta::days(community::MIN_ACCOUNT_DAYS),
            factor_since: (now - chrono::TimeDelta::days(community::FACTOR_WINDOW_DAYS))
                .date_naive(),
        }
    }
}

/// Takes up to `limit` due entities off the queue and recounts their tags,
/// in one transaction (a failure leaves them queued). How many it took.
pub async fn recount_due(pool: &PgPool, w: Weighing, limit: i64) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let due = sqlx::query!(
        r#"
        DELETE FROM tag_queue
        WHERE (entity_type, entity_id) IN (
            SELECT entity_type, entity_id FROM tag_queue
            WHERE due_at <= NOW()
            ORDER BY due_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING entity_type, entity_id
        "#,
        limit,
    )
    .fetch_all(&mut *tx)
    .await?;
    if due.is_empty() {
        return Ok(0);
    }
    let kinds: Vec<String> = due.iter().map(|d| d.entity_type.clone()).collect();
    let ids: Vec<i64> = due.iter().map(|d| d.entity_id).collect();
    recount(&mut tx, &kinds, &ids, w).await?;
    tx.commit().await?;
    Ok(due.len())
}

/// Replaces the counts of these entities with what their tags weigh now.
pub async fn recount(
    conn: &mut PgConnection,
    kinds: &[String],
    ids: &[i64],
    w: Weighing,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        DELETE FROM tag_counts c
        USING UNNEST($1::text[], $2::bigint[]) AS b(entity_type, entity_id)
        WHERE c.entity_type = b.entity_type AND c.entity_id = b.entity_id
        "#,
        kinds,
        ids,
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        r#"
        INSERT INTO tag_counts (entity_type, entity_id, tag_id, taggers, weight)
        SELECT ut.entity_type, ut.entity_id, ut.tag_id, count(*),
               sum(CASE WHEN u.created_at <= $3 AND u.delete_after IS NULL AND s.user_id IS NULL
                        THEN COALESCE(f.factor, $5) ELSE 0 END)
        FROM UNNEST($1::text[], $2::bigint[]) AS b(entity_type, entity_id)
        JOIN user_tags ut ON ut.entity_type = b.entity_type AND ut.entity_id = b.entity_id
        JOIN users u ON u.id = ut.user_id
        LEFT JOIN editing_suspensions s ON s.user_id = u.id AND (s.until IS NULL OR s.until > NOW())
        LEFT JOIN LATERAL (
            SELECT min(d.factor)::bigint AS factor FROM coordination_days d
            WHERE d.user_id = u.id AND d.day >= $4
        ) f ON TRUE
        GROUP BY 1, 2, 3
        "#,
        kinds,
        ids,
        w.created_before,
        w.factor_since,
        shared::ranking::FULL,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Deletes tags nobody uses any more (a day after they were made).
pub async fn purge_unused(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        r#"
        DELETE FROM tags t
        WHERE t.created_at < NOW() - INTERVAL '1 day'
          AND NOT EXISTS (SELECT 1 FROM user_tags ut WHERE ut.tag_id = t.id)
          AND NOT EXISTS (SELECT 1 FROM tag_counts c WHERE c.tag_id = t.id)
          AND NOT EXISTS (SELECT 1 FROM reports r WHERE r.tag_id = t.id)
        "#,
    )
    .execute(pool)
    .await?
    .rows_affected())
}
