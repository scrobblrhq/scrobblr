use chrono::{Duration, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use shared::models::{ApiToken, UserSession};

pub async fn create_session(
    pool: &PgPool,
    user_id: i64,
    ip_address: Option<&str>,
    user_agent: Option<&str>,
) -> Result<UserSession, sqlx::Error> {
    sqlx::query_as!(
        UserSession,
        r#"
        INSERT INTO user_sessions (user_id, ip_address, user_agent)
        VALUES ($1, $2::text::inet, $3)
        RETURNING id, user_id,
                  ip_address::TEXT, user_agent,
                  created_at, expires_at, last_used_at
        "#,
        user_id,
        ip_address,
        user_agent,
    )
    .fetch_one(pool)
    .await
}

pub async fn get_session(pool: &PgPool, id: Uuid) -> Result<Option<UserSession>, sqlx::Error> {
    sqlx::query_as!(
        UserSession,
        r#"
        SELECT id, user_id,
               ip_address::TEXT, user_agent,
               created_at, expires_at, last_used_at
        FROM user_sessions
        WHERE id = $1
          AND expires_at > NOW()
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

pub async fn touch_session(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE user_sessions SET last_used_at = NOW() WHERE id = $1",
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_session(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!("DELETE FROM user_sessions WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Deletes all sessions past their `expires_at` timestamp and returns the
/// number of rows removed. Intended to be called from a periodic cleanup job.
pub async fn delete_expired_sessions(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!("DELETE FROM user_sessions WHERE expires_at <= NOW()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Creates an API token with an optional expiry.
///
/// `token_hash` must be a pre-hashed value — the raw token is never stored.
/// Pass `expires_days: None` for a non-expiring token.
pub async fn create_api_token(
    pool: &PgPool,
    user_id: i64,
    name: &str,
    token_hash: &str,
    scopes: &[String],
    expires_days: Option<i64>,
) -> Result<ApiToken, sqlx::Error> {
    let expires_at = expires_days.map(|d| Utc::now() + Duration::days(d));

    sqlx::query_as!(
        ApiToken,
        r#"
        INSERT INTO api_tokens (user_id, name, token_hash, scopes, expires_at)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, user_id, name, token_hash, scopes, last_used_at, created_at, expires_at
        "#,
        user_id,
        name,
        token_hash,
        scopes,
        expires_at,
    )
    .fetch_one(pool)
    .await
}

pub async fn find_api_token_by_hash(
    pool: &PgPool,
    token_hash: &str,
) -> Result<Option<ApiToken>, sqlx::Error> {
    sqlx::query_as!(
        ApiToken,
        r#"
        SELECT id, user_id, name, token_hash, scopes, last_used_at, created_at, expires_at
        FROM api_tokens
        WHERE token_hash = $1
          AND (expires_at IS NULL OR expires_at > NOW())
        "#,
        token_hash,
    )
    .fetch_optional(pool)
    .await
}

pub async fn list_api_tokens(pool: &PgPool, user_id: i64) -> Result<Vec<ApiToken>, sqlx::Error> {
    sqlx::query_as!(
        ApiToken,
        r#"
        SELECT id, user_id, name, token_hash, scopes, last_used_at, created_at, expires_at
        FROM api_tokens
        WHERE user_id = $1
        ORDER BY created_at DESC
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

/// Deletes a token by ID, scoped to `user_id` to prevent cross-user deletion.
/// Returns `true` if a row was actually deleted.
pub async fn delete_api_token(pool: &PgPool, id: Uuid, user_id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        "DELETE FROM api_tokens WHERE id = $1 AND user_id = $2",
        id,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn touch_api_token(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE api_tokens SET last_used_at = NOW() WHERE id = $1",
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Hashes a raw API token for storage and lookup. This is the ONLY place
/// this hashing should happen — both token creation and request validation
/// must call this, or stored hashes and lookup hashes will diverge and
/// every token will silently fail to authenticate.
pub fn hash_api_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hex::encode(hasher.finalize())
}
