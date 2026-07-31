//! Connected third-party accounts (OAuth). This module is the encryption
//! boundary for provider tokens: `access_token`/`refresh_token` are stored
//! ciphertext (see `shared::crypto`) and every function here takes and
//! returns plaintext, so no caller has to remember to encrypt. Nothing
//! outside this module should read those two columns.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;

use shared::crypto::{self, CryptoError};
use shared::models::{ConnectedAccount, ConnectedAccountSummary};

/// The `UNIQUE (provider, provider_user_id)` constraint from
/// `migrations/0006_connected_accounts.sql`, under the name Postgres
/// generates for it. Callers match on this to tell "this Spotify account is
/// already linked to somebody else" apart from a real database failure.
pub const PROVIDER_ACCOUNT_TAKEN_CONSTRAINT: &str =
    "connected_accounts_provider_provider_user_id_key";

#[derive(Debug, Error)]
pub enum ConnectedAccountError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("token encryption error: {0}")]
    Crypto(#[from] CryptoError),
}

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
///
/// Fails with a [`ConnectedAccountError::Db`] carrying
/// [`PROVIDER_ACCOUNT_TAKEN_CONSTRAINT`] when the provider account is
/// already linked to a *different* Scrobblr user.
pub async fn upsert_connected_account(
    pool: &PgPool,
    input: &UpsertConnectedAccount,
) -> Result<ConnectedAccount, ConnectedAccountError> {
    let access_token = crypto::encrypt(&input.access_token)?;
    let refresh_token = crypto::encrypt_opt(input.refresh_token.as_deref())?;

    let account = sqlx::query_as!(
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
                  token_type, scope, expires_at, last_polled_at, history_cursor_at,
                  last_error, is_active, created_at, updated_at
        "#,
        input.user_id,
        input.provider,
        input.provider_user_id,
        access_token,
        refresh_token,
        input.token_type,
        input.scope,
        input.expires_at,
    )
    .fetch_one(pool)
    .await?;

    decrypt_tokens(account)
}

/// The user-facing listing. Returns [`ConnectedAccountSummary`], which has
/// no token columns — so this path neither selects nor decrypts token
/// material, and a settings page keeps working even if
/// `TOKEN_ENCRYPTION_KEY` is misconfigured.
pub async fn list_connected_accounts(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<ConnectedAccountSummary>, sqlx::Error> {
    sqlx::query_as!(
        ConnectedAccountSummary,
        r#"
        SELECT id, user_id, provider, provider_user_id, token_type, scope, expires_at,
               last_polled_at, last_error, is_active, created_at, updated_at
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

/// Lists a batch of active accounts due for polling, oldest-polled (or
/// never-polled) first, so no single user's account starves the rest.
///
/// A plain read, not a claim: rows are not locked or marked in-flight, so
/// two workers polling concurrently would hand each other the same batch.
/// That is fine today because the worker is single-instance by design (the
/// enrichment rate limiters are in-process too) — running more than one
/// would need a `FOR UPDATE SKIP LOCKED` claim here first.
pub async fn list_accounts_to_poll(
    pool: &PgPool,
    provider: &str,
    batch_size: i64,
) -> Result<Vec<ConnectedAccount>, ConnectedAccountError> {
    let accounts = sqlx::query_as!(
        ConnectedAccount,
        r#"
        SELECT id, user_id, provider, provider_user_id, access_token, refresh_token,
               token_type, scope, expires_at, last_polled_at, history_cursor_at,
               last_error, is_active, created_at, updated_at
        FROM connected_accounts
        WHERE provider = $1 AND is_active
        ORDER BY last_polled_at ASC NULLS FIRST
        LIMIT $2
        "#,
        provider,
        batch_size,
    )
    .fetch_all(pool)
    .await?;

    accounts.into_iter().map(decrypt_tokens).collect()
}

/// Persists a refreshed access (and, if issued, refresh) token.
pub async fn update_tokens(
    pool: &PgPool,
    id: i64,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<(), ConnectedAccountError> {
    let access_token = crypto::encrypt(access_token)?;
    let refresh_token = crypto::encrypt_opt(refresh_token)?;

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

/// Records the outcome of a poll attempt.
///
/// `history_cursor_at` only ever moves forward to `cursor_at` — the
/// `played_at` of the newest play actually ingested. Pass `None` when the
/// poll returned nothing (or failed) so the cursor stays where it is: a
/// play the provider hadn't reported yet must still be picked up next tick,
/// which a wall-clock cursor would silently skip past.
pub async fn mark_polled(
    pool: &PgPool,
    id: i64,
    polled_at: DateTime<Utc>,
    cursor_at: Option<DateTime<Utc>>,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE connected_accounts
        SET last_polled_at = $2,
            history_cursor_at = GREATEST(history_cursor_at, $3),
            last_error = $4
        WHERE id = $1
        "#,
        id,
        polled_at,
        cursor_at,
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

fn decrypt_tokens(
    mut account: ConnectedAccount,
) -> Result<ConnectedAccount, ConnectedAccountError> {
    account.access_token = crypto::decrypt(&account.access_token)?;
    account.refresh_token = crypto::decrypt_opt(account.refresh_token.as_deref())?;
    Ok(account)
}
