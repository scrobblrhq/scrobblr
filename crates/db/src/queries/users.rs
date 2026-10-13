use chrono::{DateTime, Utc};
use sqlx::PgPool;

use shared::cursor::Cursor;
use shared::models::{ScrobbleBreakdown, User};

pub async fn find_by_id(pool: &PgPool, id: i64) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as!(
        User,
        r#"
        SELECT id, username, email, password_hash, display_name, bio,
               image_url, website_url, country, scrobble_count,
               is_private, is_verified, last_seen_at, created_at, updated_at,
               delete_after
        FROM users
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Lookup is case-insensitive, served by `idx_users_username_lower`.
pub async fn find_by_username(pool: &PgPool, username: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as!(
        User,
        r#"
        SELECT id, username, email, password_hash, display_name, bio,
               image_url, website_url, country, scrobble_count,
               is_private, is_verified, last_seen_at, created_at, updated_at,
               delete_after
        FROM users
        WHERE lower(username) = lower($1)
        "#,
        username,
    )
    .fetch_optional(pool)
    .await
}

/// Case-insensitive, like [`find_by_username`].
pub async fn find_by_email(pool: &PgPool, email: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as!(
        User,
        r#"
        SELECT id, username, email, password_hash, display_name, bio,
               image_url, website_url, country, scrobble_count,
               is_private, is_verified, last_seen_at, created_at, updated_at,
               delete_after
        FROM users
        WHERE lower(email) = lower($1)
        "#,
        email,
    )
    .fetch_optional(pool)
    .await
}

pub struct CreateUser<'a> {
    pub username: &'a str,
    pub email: &'a str,
    pub password_hash: &'a str,
    pub display_name: Option<&'a str>,
}

pub async fn create_user(pool: &PgPool, input: &CreateUser<'_>) -> Result<User, sqlx::Error> {
    sqlx::query_as!(
        User,
        r#"
        INSERT INTO users (username, email, password_hash, display_name)
        VALUES ($1, $2, $3, $4)
        RETURNING id, username, email, password_hash, display_name, bio,
                  image_url, website_url, country, scrobble_count,
                  is_private, is_verified, last_seen_at, created_at, updated_at,
                  delete_after
        "#,
        input.username,
        input.email,
        input.password_hash,
        input.display_name,
    )
    .fetch_one(pool)
    .await
}

/// Substring match on username/display name, busiest scrobblers first.
/// `%`/`_` in the query are escaped so they match literally. Private
/// profiles are excluded — like the listener lists, they stay undiscoverable
/// — and so are accounts waiting to be erased.
pub async fn search_users(
    pool: &PgPool,
    query: &str,
    limit: i64,
) -> Result<Vec<User>, sqlx::Error> {
    let pattern = format!(
        "%{}%",
        query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    sqlx::query_as!(
        User,
        r#"
        SELECT id, username, email, password_hash, display_name, bio,
               image_url, website_url, country, scrobble_count,
               is_private, is_verified, last_seen_at, created_at, updated_at,
               delete_after
        FROM users
        WHERE NOT is_private AND delete_after IS NULL
          AND (username ILIKE $1 OR display_name ILIKE $1)
        ORDER BY scrobble_count DESC
        LIMIT $2
        "#,
        pattern,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// What a follow came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Followed {
    Now,
    Already,
    /// The follower already follows `max` accounts.
    AtLimit,
    /// The followee blocks the follower.
    BlockedBy,
    /// The follower blocks the followee.
    Blocking,
    /// The followee removed the follower, and its cooldown isn't over.
    Removed,
}

/// Follows `followee_id` unless `follower_id` already follows `max`
/// accounts (every follow counts, private and leaving accounts' too), or a
/// block or a removal between them stands. Serialized per follower, so
/// concurrent follows can't pass the cap together, and per pair, so no
/// follow lands beside a block made at the same time.
pub async fn follow_user(
    pool: &PgPool,
    follower_id: i64,
    followee_id: i64,
    max: i64,
) -> Result<Followed, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('follow:' || $1, 0))")
        .bind(follower_id.to_string())
        .execute(&mut *tx)
        .await?;
    crate::queries::blocks::lock_pair(&mut tx, follower_id, followee_id).await?;
    let r = sqlx::query!(
        r#"
        SELECT EXISTS (SELECT 1 FROM user_follows
                       WHERE follower_id = $1 AND followee_id = $2) AS "already!",
               (SELECT count(*) FROM user_follows WHERE follower_id = $1) AS "following!",
               EXISTS (SELECT 1 FROM user_blocks
                       WHERE blocker_id = $2 AND blocked_id = $1) AS "blocked_by!",
               EXISTS (SELECT 1 FROM user_blocks
                       WHERE blocker_id = $1 AND blocked_id = $2) AS "blocking!",
               EXISTS (SELECT 1 FROM follower_removals
                       WHERE user_id = $2 AND follower_id = $1 AND until > NOW()) AS "removed!"
        "#,
        follower_id,
        followee_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if r.blocked_by {
        return Ok(Followed::BlockedBy);
    }
    if r.blocking {
        return Ok(Followed::Blocking);
    }
    if r.already {
        return Ok(Followed::Already);
    }
    if r.removed {
        return Ok(Followed::Removed);
    }
    if r.following >= max {
        return Ok(Followed::AtLimit);
    }
    sqlx::query!(
        r#"
        INSERT INTO user_follows (follower_id, followee_id)
        VALUES ($1, $2)
        ON CONFLICT DO NOTHING
        "#,
        follower_id,
        followee_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Followed::Now)
}

pub async fn unfollow_user(
    pool: &PgPool,
    follower_id: i64,
    followee_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "DELETE FROM user_follows WHERE follower_id = $1 AND followee_id = $2",
        follower_id,
        followee_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Followers or following.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowList {
    Followers,
    Following,
}

/// An account in a follow list, and how it relates to the viewer (`None`
/// without one, or for the viewer's own entry).
#[derive(Debug, Clone)]
pub struct Follow {
    pub user: User,
    pub followed_at: DateTime<Utc>,
    pub is_following: Option<bool>,
    pub follows_you: Option<bool>,
}

macro_rules! follow {
    ($r:expr) => {
        Follow {
            user: User {
                id: $r.id,
                username: $r.username,
                email: $r.email,
                password_hash: $r.password_hash,
                display_name: $r.display_name,
                bio: $r.bio,
                image_url: $r.image_url,
                website_url: $r.website_url,
                country: $r.country,
                scrobble_count: $r.scrobble_count,
                is_private: $r.is_private,
                is_verified: $r.is_verified,
                last_seen_at: $r.last_seen_at,
                created_at: $r.created_at,
                updated_at: $r.updated_at,
                delete_after: $r.delete_after,
            },
            followed_at: $r.followed_at,
            is_following: $r.is_following,
            follows_you: $r.follows_you,
        }
    };
}

/// A page of `user_id`'s followers or following, newest follow first,
/// after `before` (the previous page's last follow time and account id).
/// Only accounts `viewer_id` may see: private ones are left out unless they
/// are the viewer's own, and accounts waiting to be erased always are.
pub async fn follow_page(
    pool: &PgPool,
    user_id: i64,
    list: FollowList,
    viewer_id: Option<i64>,
    before: Option<Cursor>,
    limit: i64,
) -> Result<Vec<Follow>, sqlx::Error> {
    let (before_at, before_id) = (before.map(|c| c.at), before.map_or(0, |c| c.id));
    Ok(match list {
        FollowList::Followers => sqlx::query!(
            r#"
        SELECT
               u.id, u.username, u.email, u.password_hash, u.display_name, u.bio,
               u.image_url, u.website_url, u.country, u.scrobble_count,
               u.is_private, u.is_verified, u.last_seen_at, u.created_at, u.updated_at,
               u.delete_after,
               f.created_at AS followed_at,
               CASE WHEN $2::bigint IS NULL OR u.id = $2 THEN NULL
                    ELSE EXISTS (SELECT 1 FROM user_follows v
                                 WHERE v.follower_id = $2 AND v.followee_id = u.id) END
                   AS is_following,
               CASE WHEN $2::bigint IS NULL OR u.id = $2 THEN NULL
                    ELSE EXISTS (SELECT 1 FROM user_follows v
                                 WHERE v.follower_id = u.id AND v.followee_id = $2) END
                   AS follows_you
        FROM user_follows f
        JOIN users u ON u.id = f.follower_id
        WHERE f.followee_id = $1 AND u.delete_after IS NULL
          AND (NOT u.is_private OR u.id = $2)
          AND ($3::timestamptz IS NULL OR (f.created_at, f.follower_id) < ($3, $4))
        ORDER BY f.created_at DESC, f.follower_id DESC
        LIMIT $5
        "#,
            user_id,
            viewer_id,
            before_at,
            before_id,
            limit,
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| follow!(r))
        .collect(),
        FollowList::Following => sqlx::query!(
            r#"
        SELECT
               u.id, u.username, u.email, u.password_hash, u.display_name, u.bio,
               u.image_url, u.website_url, u.country, u.scrobble_count,
               u.is_private, u.is_verified, u.last_seen_at, u.created_at, u.updated_at,
               u.delete_after,
               f.created_at AS followed_at,
               CASE WHEN $2::bigint IS NULL OR u.id = $2 THEN NULL
                    ELSE EXISTS (SELECT 1 FROM user_follows v
                                 WHERE v.follower_id = $2 AND v.followee_id = u.id) END
                   AS is_following,
               CASE WHEN $2::bigint IS NULL OR u.id = $2 THEN NULL
                    ELSE EXISTS (SELECT 1 FROM user_follows v
                                 WHERE v.follower_id = u.id AND v.followee_id = $2) END
                   AS follows_you
        FROM user_follows f
        JOIN users u ON u.id = f.followee_id
        WHERE f.follower_id = $1 AND u.delete_after IS NULL
          AND (NOT u.is_private OR u.id = $2)
          AND ($3::timestamptz IS NULL OR (f.created_at, f.followee_id) < ($3, $4))
        ORDER BY f.created_at DESC, f.followee_id DESC
        LIMIT $5
        "#,
            user_id,
            viewer_id,
            before_at,
            before_id,
            limit,
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| follow!(r))
        .collect(),
    })
}

/// Accounts following `user_id` and followed by it: every follow, private
/// accounts' and those waiting to be erased included, which lists don't
/// name.
pub async fn follow_counts(pool: &PgPool, user_id: i64) -> Result<(i64, i64), sqlx::Error> {
    let r = sqlx::query!(
        r#"
        SELECT (SELECT count(*) FROM user_follows WHERE followee_id = $1) AS "followers!",
               (SELECT count(*) FROM user_follows WHERE follower_id = $1) AS "following!"
        "#,
        user_id,
    )
    .fetch_one(pool)
    .await?;
    Ok((r.followers, r.following))
}

pub async fn is_following(
    pool: &PgPool,
    follower_id: i64,
    followee_id: i64,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query!(
        "SELECT 1 AS exists FROM user_follows WHERE follower_id = $1 AND followee_id = $2",
        follower_id,
        followee_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// Per-field patch semantics: `None` leaves the column unchanged,
/// `Some(None)` clears it to NULL, `Some(Some(v))` sets it.
#[derive(Debug, Default)]
pub struct UpdateProfile<'a> {
    pub display_name: Option<Option<&'a str>>,
    pub bio: Option<Option<&'a str>>,
    pub image_url: Option<Option<&'a str>>,
    pub is_private: Option<bool>,
}

/// Returns the updated user and the `image_url` it replaced. The row is
/// locked first, so concurrent updates each report the image they replaced.
pub async fn update_profile(
    pool: &PgPool,
    user_id: i64,
    update: &UpdateProfile<'_>,
) -> Result<(User, Option<String>), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let previous_image = sqlx::query_scalar!(
        "SELECT image_url FROM users WHERE id = $1 FOR UPDATE",
        user_id
    )
    .fetch_one(&mut *tx)
    .await?;
    let user = sqlx::query_as!(
        User,
        r#"
        UPDATE users SET
            display_name = CASE WHEN $2::bool THEN $3 ELSE display_name END,
            bio          = CASE WHEN $4::bool THEN $5 ELSE bio END,
            image_url    = CASE WHEN $6::bool THEN $7 ELSE image_url END,
            is_private   = COALESCE($8, is_private),
            updated_at   = NOW()
        WHERE id = $1
        RETURNING id, username, email, password_hash, display_name, bio,
                  image_url, website_url, country, scrobble_count,
                  is_private, is_verified, last_seen_at, created_at, updated_at,
                  delete_after
        "#,
        user_id,
        update.display_name.is_some(),
        update.display_name.flatten(),
        update.bio.is_some(),
        update.bio.flatten(),
        update.image_url.is_some(),
        update.image_url.flatten(),
        update.is_private,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((user, previous_image))
}

/// How the user's `scrobble_count` (which leaves classified duplicates out)
/// divides by label, from the totals the classification keeps
/// (`user_label_totals`).
pub async fn scrobble_breakdown(
    pool: &PgPool,
    user_id: i64,
) -> Result<ScrobbleBreakdown, sqlx::Error> {
    let r = sqlx::query!(
        r#"
        SELECT u.scrobble_count,
               COALESCE(t.classified, 0) AS "classified!", COALESCE(t.counted, 0) AS "counted!",
               COALESCE(t.suspect, 0) AS "suspect!", COALESCE(t.duplicate, 0) AS "duplicate!",
               COALESCE(t.no_data, 0) AS "no_data!"
        FROM users u
        LEFT JOIN user_label_totals t ON t.user_id = u.id
        WHERE u.id = $1
        "#,
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    let Some(r) = r else {
        return Ok(ScrobbleBreakdown::default());
    };
    let pending = (r.scrobble_count + r.duplicate - r.classified).max(0);
    Ok(ScrobbleBreakdown {
        verified: r.counted,
        unverified: r.suspect + r.no_data + pending,
        suspect: r.suspect,
        no_data: r.no_data,
        pending,
        duplicates: r.duplicate,
    })
}
