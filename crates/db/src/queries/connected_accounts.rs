use chrono::{DateTime, Utc};
use sqlx::PgPool;

use shared::models::ConnectedAccount;

pub struct UpsertConnectedAccount {
    pub user_id: i64,
    pub provider: String,
    pub provider_user_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub scope: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Creates or re-links a connected account. Re-authorizing (e.g. after a
/// revoked/expired refresh token forced `is_active` to false) simply
/// overwrites the stored tokens and reactivates the row.
pub async fn upsert_connected_account(
    pool: &PgPool,
    input: &UpsertConnectedAccount,
) -> Result<ConnectedAccount, sqlx::Error> {
    sqlx::query_as!(
        ConnectedAccount,
        r#"
        INSERT INTO connected_accounts
            (user_id, provider, provider_user_id, access_token, refresh_token, token_type, scope, expires_at, is_active)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, TRUE)
        ON CONFLICT (user_id, provider) DO UPDATE SET
            provider_user_id = EXCLUDED.provider_user_id,
            access_token     = EXCLUDED.access_token,
            refresh_token    = EXCLUDED.refresh_token,
            token_type       = EXCLUDED.token_type,
            scope            = EXCLUDED.scope,
            expires_at       = EXCLUDED.expires_at,
            is_active        = TRUE,
            last_error       = NULL
        RETURNING id, user_id, provider, provider_user_id, access_token, refresh_token,
                  token_type, scope, expires_at, last_polled_at, last_error, is_active,
                  created_at, updated_at
        "#,
        input.user_id,
        input.provider,
        input.provider_user_id,
        input.access_token,
        input.refresh_token,
        input.token_type,
        input.scope,
        input.expires_at,
    )
    .fetch_one(pool)
    .await
}

pub async fn list_connected_accounts(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<ConnectedAccount>, sqlx::Error> {
    sqlx::query_as!(
        ConnectedAccount,
        r#"
        SELECT id, user_id, provider, provider_user_id, access_token, refresh_token,
               token_type, scope, expires_at, last_polled_at, last_error, is_active,
               created_at, updated_at
        FROM connected_accounts
        WHERE user_id = $1
        ORDER BY created_at DESC
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

pub async fn delete_connected_account(
    pool: &PgPool,
    user_id: i64,
    provider: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        "DELETE FROM connected_accounts WHERE user_id = $1 AND provider = $2",
        user_id,
        provider,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Claims a batch of active accounts due for polling, oldest-polled (or
/// never-polled) first, so no single user's account starves the rest.
pub async fn list_accounts_to_poll(
    pool: &PgPool,
    provider: &str,
    batch_size: i64,
) -> Result<Vec<ConnectedAccount>, sqlx::Error> {
    sqlx::query_as!(
        ConnectedAccount,
        r#"
        SELECT id, user_id, provider, provider_user_id, access_token, refresh_token,
               token_type, scope, expires_at, last_polled_at, last_error, is_active,
               created_at, updated_at
        FROM connected_accounts
        WHERE provider = $1 AND is_active
        ORDER BY last_polled_at ASC NULLS FIRST
        LIMIT $2
        "#,
        provider,
        batch_size,
    )
    .fetch_all(pool)
    .await
}

/// Persists a refreshed access (and, if issued, refresh) token.
pub async fn update_tokens(
    pool: &PgPool,
    id: i64,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE connected_accounts
        SET access_token = $2,
            refresh_token = COALESCE($3, refresh_token),
            expires_at = $4
        WHERE id = $1
        "#,
        id,
        access_token,
        refresh_token,
        expires_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_polled(
    pool: &PgPool,
    id: i64,
    polled_at: DateTime<Utc>,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE connected_accounts SET last_polled_at = $2, last_error = $3 WHERE id = $1",
        id,
        polled_at,
        error,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Deactivates a connection (e.g. the refresh token was permanently
/// rejected — the user must re-authorize). Distinct from delete: keeps the
/// row (and `last_error`) so a settings UI can show a "reconnect" prompt
/// instead of the connection silently disappearing.
pub async fn deactivate(pool: &PgPool, id: i64, reason: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE connected_accounts SET is_active = FALSE, last_error = $2 WHERE id = $1",
        id,
        reason,
    )
    .execute(pool)
    .await?;
    Ok(())
}
