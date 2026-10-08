//! Credentials of third-party scrobblers and Last.fm browser authorizations
//! (migration 0014). The API's compatibility endpoints are the only reader.

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use shared::models::ScrobblerCredential;

pub const KIND_TOKEN: &str = "token";
pub const KIND_SESSION: &str = "session";

/// A credential resolved for a request, with its owner.
#[derive(Debug, Clone)]
pub struct Credential {
    pub id: Uuid,
    pub user_id: i64,
    pub username: String,
    pub kind: String,
    pub api_key: Option<String>,
}

pub struct NewCredential<'a> {
    pub user_id: i64,
    pub kind: &'a str,
    pub name: &'a str,
    pub key_hash: &'a str,
    pub legacy_secret: Option<&'a str>,
    pub api_key: Option<&'a str>,
}

pub async fn create_credential(
    pool: &PgPool,
    c: &NewCredential<'_>,
) -> Result<ScrobblerCredential, sqlx::Error> {
    sqlx::query_as!(
        ScrobblerCredential,
        r#"
        INSERT INTO scrobbler_credentials (user_id, kind, name, key_hash, legacy_secret, api_key)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, kind, name, api_key, legacy_secret IS NOT NULL AS "legacy_auth!",
                  created_at, last_used_at
        "#,
        c.user_id,
        c.kind,
        c.name,
        c.key_hash,
        c.legacy_secret,
        c.api_key,
    )
    .fetch_one(pool)
    .await
}

pub async fn find_credential(
    pool: &PgPool,
    key_hash: &str,
) -> Result<Option<Credential>, sqlx::Error> {
    sqlx::query_as!(
        Credential,
        r#"
        SELECT c.id, c.user_id, u.username, c.kind, c.api_key
        FROM scrobbler_credentials c
        JOIN users u ON u.id = c.user_id
        WHERE c.key_hash = $1
        "#,
        key_hash,
    )
    .fetch_optional(pool)
    .await
}

pub async fn find_credential_by_id(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<Credential>, sqlx::Error> {
    sqlx::query_as!(
        Credential,
        r#"
        SELECT c.id, c.user_id, u.username, c.kind, c.api_key
        FROM scrobbler_credentials c
        JOIN users u ON u.id = c.user_id
        WHERE c.id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await
}

/// A token holding a legacy secret, for logins that prove knowledge of it.
pub struct LegacyCandidate {
    pub credential: Credential,
    pub legacy_secret: String,
}

/// The legacy-capable tokens of the user named `username` (matched like
/// `users::find_by_username`), in one query whether or not the user exists.
pub async fn legacy_candidates(
    pool: &PgPool,
    username: &str,
) -> Result<Vec<LegacyCandidate>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT c.id, c.user_id, u.username, c.kind, c.api_key,
               c.legacy_secret AS "legacy_secret!"
        FROM scrobbler_credentials c
        JOIN users u ON u.id = c.user_id
        WHERE lower(u.username) = lower($1) AND c.kind = 'token' AND c.legacy_secret IS NOT NULL
        "#,
        username,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LegacyCandidate {
            credential: Credential {
                id: r.id,
                user_id: r.user_id,
                username: r.username,
                kind: r.kind,
                api_key: r.api_key,
            },
            legacy_secret: r.legacy_secret,
        })
        .collect())
}

pub async fn list_credentials(
    pool: &PgPool,
    user_id: i64,
) -> Result<Vec<ScrobblerCredential>, sqlx::Error> {
    sqlx::query_as!(
        ScrobblerCredential,
        r#"
        SELECT id, kind, name, api_key, legacy_secret IS NOT NULL AS "legacy_auth!",
               created_at, last_used_at
        FROM scrobbler_credentials
        WHERE user_id = $1
        ORDER BY created_at DESC
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

/// Scoped to `user_id`; `true` if the credential existed.
pub async fn delete_credential(pool: &PgPool, id: Uuid, user_id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        "DELETE FROM scrobbler_credentials WHERE id = $1 AND user_id = $2",
        id,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Records a credential's use, at most every 5 minutes (see
/// `auth::touch_session`).
pub async fn touch_credential(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE scrobbler_credentials SET last_used_at = NOW()
        WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < NOW() - INTERVAL '5 minutes')
        "#,
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Authorization {
    pub token: String,
    pub api_key: String,
    pub user_id: Option<i64>,
    pub expires_at: DateTime<Utc>,
}

/// `user_id` set: already approved (the web flow approves as it creates).
pub async fn create_authorization(
    pool: &PgPool,
    token: &str,
    api_key: &str,
    user_id: Option<i64>,
    ttl: TimeDelta,
) -> Result<Authorization, sqlx::Error> {
    sqlx::query_as!(
        Authorization,
        r#"
        INSERT INTO scrobbler_authorizations (token, api_key, user_id, expires_at)
        VALUES ($1, $2, $3, NOW() + make_interval(secs => $4))
        RETURNING token, api_key, user_id, expires_at
        "#,
        token,
        api_key,
        user_id,
        ttl.num_seconds() as f64,
    )
    .fetch_one(pool)
    .await
}

pub async fn get_authorization(
    pool: &PgPool,
    token: &str,
) -> Result<Option<Authorization>, sqlx::Error> {
    sqlx::query_as!(
        Authorization,
        "SELECT token, api_key, user_id, expires_at FROM scrobbler_authorizations WHERE token = $1",
        token,
    )
    .fetch_optional(pool)
    .await
}

/// Approves a pending, unexpired authorization for `user_id`. `false` when
/// there is none to approve (unknown, expired, or approved already).
pub async fn approve_authorization(
    pool: &PgPool,
    token: &str,
    user_id: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        UPDATE scrobbler_authorizations SET user_id = $2
        WHERE token = $1 AND user_id IS NULL AND expires_at > NOW()
        "#,
        token,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Consumes an approved, unexpired authorization issued to `api_key` and
/// returns who approved it. Atomic, so a token yields one session at most.
pub async fn take_authorization(
    pool: &PgPool,
    token: &str,
    api_key: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        DELETE FROM scrobbler_authorizations
        WHERE token = $1 AND api_key = $2 AND user_id IS NOT NULL AND expires_at > NOW()
        RETURNING user_id AS "user_id!"
        "#,
        token,
        api_key,
    )
    .fetch_optional(pool)
    .await
}

pub async fn delete_expired_authorizations(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!("DELETE FROM scrobbler_authorizations WHERE expires_at <= NOW()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
