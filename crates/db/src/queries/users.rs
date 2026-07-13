use sqlx::PgPool;

use shared::models::User;

pub async fn find_by_id(pool: &PgPool, id: i64) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as!(
        User,
        r#"
        SELECT id, username, email, password_hash, display_name, bio,
               image_url, website_url, country, scrobble_count,
               is_private, is_verified, last_seen_at, created_at, updated_at
        FROM users
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// Lookup is case-insensitive. The `idx_users_username` index is on the raw
/// column, so this query falls back to a sequential scan — consider adding a
/// `lower(username)` functional index if this becomes a hot path.
pub async fn find_by_username(pool: &PgPool, username: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as!(
        User,
        r#"
        SELECT id, username, email, password_hash, display_name, bio,
               image_url, website_url, country, scrobble_count,
               is_private, is_verified, last_seen_at, created_at, updated_at
        FROM users
        WHERE lower(username) = lower($1)
        "#,
        username,
    )
    .fetch_optional(pool)
    .await
}

/// See [`find_by_username`] — same case-insensitive caveat applies.
pub async fn find_by_email(pool: &PgPool, email: &str) -> Result<Option<User>, sqlx::Error> {
    sqlx::query_as!(
        User,
        r#"
        SELECT id, username, email, password_hash, display_name, bio,
               image_url, website_url, country, scrobble_count,
               is_private, is_verified, last_seen_at, created_at, updated_at
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
                  is_private, is_verified, last_seen_at, created_at, updated_at
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
/// profiles are excluded — like the listener lists, they stay undiscoverable.
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
               is_private, is_verified, last_seen_at, created_at, updated_at
        FROM users
        WHERE NOT is_private AND (username ILIKE $1 OR display_name ILIKE $1)
        ORDER BY scrobble_count DESC
        LIMIT $2
        "#,
        pattern,
        limit,
    )
    .fetch_all(pool)
    .await
}

pub async fn follow_user(
    pool: &PgPool,
    follower_id: i64,
    followee_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO user_follows (follower_id, followee_id)
        VALUES ($1, $2)
        ON CONFLICT DO NOTHING
        "#,
        follower_id,
        followee_id,
    )
    .execute(pool)
    .await?;
    Ok(())
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

pub async fn get_followers(pool: &PgPool, user_id: i64) -> Result<Vec<User>, sqlx::Error> {
    sqlx::query_as!(
        User,
        r#"
        SELECT u.id, u.username, u.email, u.password_hash, u.display_name, u.bio,
               u.image_url, u.website_url, u.country, u.scrobble_count,
               u.is_private, u.is_verified, u.last_seen_at, u.created_at, u.updated_at
        FROM user_follows f
        JOIN users u ON u.id = f.follower_id
        WHERE f.followee_id = $1
        ORDER BY f.created_at DESC
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

pub async fn get_following(pool: &PgPool, user_id: i64) -> Result<Vec<User>, sqlx::Error> {
    sqlx::query_as!(
        User,
        r#"
        SELECT u.id, u.username, u.email, u.password_hash, u.display_name, u.bio,
               u.image_url, u.website_url, u.country, u.scrobble_count,
               u.is_private, u.is_verified, u.last_seen_at, u.created_at, u.updated_at
        FROM user_follows f
        JOIN users u ON u.id = f.followee_id
        WHERE f.follower_id = $1
        ORDER BY f.created_at DESC
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
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

pub async fn update_profile(
    pool: &PgPool,
    user_id: i64,
    update: &UpdateProfile<'_>,
) -> Result<User, sqlx::Error> {
    sqlx::query_as!(
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
                  is_private, is_verified, last_seen_at, created_at, updated_at
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
    .fetch_one(pool)
    .await
}
