//! Blocks and removed followers (docs/social.md, Blocking).
//!
//! Everything that changes who may follow whom takes the pair's advisory
//! lock ([`lock_pair`]), follows included, so a follow can't land beside a
//! block made at the same moment.

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{PgConnection, PgPool};

use shared::cursor::Cursor;
use shared::models::UserSummary;

/// Serializes changes between two accounts, whichever acts.
pub(crate) async fn lock_pair(tx: &mut PgConnection, a: i64, b: i64) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('pair:' || $1 || ':' || $2, 0))")
        .bind(a.min(b).to_string())
        .bind(a.max(b).to_string())
        .execute(tx)
        .await?;
    Ok(())
}

/// How a viewer and another account stand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Relation {
    pub following: bool,
    pub follows_you: bool,
    /// The viewer blocks the other account.
    pub blocking: bool,
    /// The other account blocks the viewer.
    pub blocked_by: bool,
}

pub async fn relation(pool: &PgPool, viewer: i64, other: i64) -> Result<Relation, sqlx::Error> {
    let r = sqlx::query!(
        r#"
        SELECT EXISTS (SELECT 1 FROM user_follows WHERE follower_id = $1 AND followee_id = $2)
                   AS "following!",
               EXISTS (SELECT 1 FROM user_follows WHERE follower_id = $2 AND followee_id = $1)
                   AS "follows_you!",
               EXISTS (SELECT 1 FROM user_blocks WHERE blocker_id = $1 AND blocked_id = $2)
                   AS "blocking!",
               EXISTS (SELECT 1 FROM user_blocks WHERE blocker_id = $2 AND blocked_id = $1)
                   AS "blocked_by!"
        "#,
        viewer,
        other,
    )
    .fetch_one(pool)
    .await?;
    Ok(Relation {
        following: r.following,
        follows_you: r.follows_you,
        blocking: r.blocking,
        blocked_by: r.blocked_by,
    })
}

/// Whether `blocker_id` blocks `blocked_id`.
pub async fn blocks(pool: &PgPool, blocker_id: i64, blocked_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM user_blocks WHERE blocker_id = $1 AND blocked_id = $2) AS "blocks!""#,
        blocker_id,
        blocked_id,
    )
    .fetch_one(pool)
    .await
}

/// What a block came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    Now,
    Already,
    /// The blocker already blocks `max` accounts.
    AtLimit,
}

/// Blocks `blocked_id` unless `blocker_id` already blocks `max` accounts,
/// and ends the follows between them.
pub async fn block(
    pool: &PgPool,
    blocker_id: i64,
    blocked_id: i64,
    max: i64,
) -> Result<Blocked, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('block:' || $1, 0))")
        .bind(blocker_id.to_string())
        .execute(&mut *tx)
        .await?;
    lock_pair(&mut tx, blocker_id, blocked_id).await?;
    let r = sqlx::query!(
        r#"
        SELECT EXISTS (SELECT 1 FROM user_blocks WHERE blocker_id = $1 AND blocked_id = $2)
                   AS "already!",
               (SELECT count(*) FROM user_blocks WHERE blocker_id = $1) AS "blocking!"
        "#,
        blocker_id,
        blocked_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if r.already {
        return Ok(Blocked::Already);
    }
    if r.blocking >= max {
        return Ok(Blocked::AtLimit);
    }
    sqlx::query!(
        "INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1, $2)",
        blocker_id,
        blocked_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"
        DELETE FROM user_follows
        WHERE (follower_id = $1 AND followee_id = $2) OR (follower_id = $2 AND followee_id = $1)
        "#,
        blocker_id,
        blocked_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Blocked::Now)
}

/// Whether there was a block to lift.
pub async fn unblock(pool: &PgPool, blocker_id: i64, blocked_id: i64) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query!(
        "DELETE FROM user_blocks WHERE blocker_id = $1 AND blocked_id = $2",
        blocker_id,
        blocked_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted > 0)
}

/// An account the user blocks.
#[derive(Debug, Clone)]
pub struct Block {
    pub user: UserSummary,
    pub blocked_at: DateTime<Utc>,
}

/// A page of the accounts `blocker_id` blocks, newest block first, after
/// `before`. Accounts waiting to be erased are left out, as everywhere; a
/// private account shows its username only.
pub async fn block_page(
    pool: &PgPool,
    blocker_id: i64,
    before: Option<Cursor>,
    limit: i64,
) -> Result<Vec<Block>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT u.id, u.username,
               CASE WHEN u.is_private THEN NULL ELSE u.display_name END AS display_name,
               CASE WHEN u.is_private THEN NULL ELSE u.image_url END AS image_url,
               b.created_at AS blocked_at
        FROM user_blocks b
        JOIN users u ON u.id = b.blocked_id
        WHERE b.blocker_id = $1 AND u.delete_after IS NULL
          AND ($2::timestamptz IS NULL OR (b.created_at, b.blocked_id) < ($2, $3))
        ORDER BY b.created_at DESC, b.blocked_id DESC
        LIMIT $4
        "#,
        blocker_id,
        before.map(|c| c.at),
        before.map_or(0, |c| c.id),
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Block {
            user: UserSummary {
                id: r.id,
                username: r.username,
                display_name: r.display_name,
                image_url: r.image_url,
            },
            blocked_at: r.blocked_at,
        })
        .collect())
}

/// Ends `follower_id`'s follow of `user_id`, and keeps it from following
/// again for `cooldown`. Whether it was following.
pub async fn remove_follower(
    pool: &PgPool,
    user_id: i64,
    follower_id: i64,
    cooldown: TimeDelta,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_pair(&mut tx, user_id, follower_id).await?;
    let removed = sqlx::query!(
        "DELETE FROM user_follows WHERE follower_id = $1 AND followee_id = $2",
        follower_id,
        user_id,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if removed {
        sqlx::query!(
            r#"
            INSERT INTO follower_removals (user_id, follower_id, until)
            VALUES ($1, $2, NOW() + make_interval(secs => $3))
            ON CONFLICT (user_id, follower_id) DO UPDATE SET until = EXCLUDED.until
            "#,
            user_id,
            follower_id,
            cooldown.num_seconds() as f64,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(removed)
}

/// Removals whose cooldown is over.
pub async fn purge_expired_removals(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query!("DELETE FROM follower_removals WHERE until <= NOW()")
            .execute(pool)
            .await?
            .rows_affected(),
    )
}
