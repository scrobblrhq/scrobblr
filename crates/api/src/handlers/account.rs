//! Deleting the account: sessions only, so a leaked API token can't.

use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, State},
    http::StatusCode,
};
use chrono::{DateTime, TimeDelta, Utc};
use fred::interfaces::KeysInterface;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    limits,
    middleware::auth::AuthUser,
    middleware::rate_limit::ClientIp,
    state::AppState,
};
use db::queries::{accounts as accounts_db, users as users_db};
use shared::user::verify_password;
use shared::validation::PASSWORD_LOGIN_MAX_LEN;

#[derive(Clone)]
pub struct AccountConfig {
    pub deletion_grace: TimeDelta,
}

pub const DEFAULT_GRACE_DAYS: i64 = 7;
pub const MAX_GRACE_DAYS: i64 = 30;

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            deletion_grace: TimeDelta::days(DEFAULT_GRACE_DAYS),
        }
    }
}

impl AccountConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let days = match std::env::var("ACCOUNT_DELETION_GRACE_DAYS") {
            Ok(v) if !v.trim().is_empty() => v
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|d| (0..=MAX_GRACE_DAYS).contains(d))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "ACCOUNT_DELETION_GRACE_DAYS={v}: expected a whole number of days from 0 to {MAX_GRACE_DAYS}"
                    )
                })?,
            _ => DEFAULT_GRACE_DAYS,
        };
        Ok(Self {
            deletion_grace: TimeDelta::days(days),
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteAccountRequest {
    /// The account's password, checked again.
    pub password: String,
    /// The account's username, typed out to confirm (case doesn't matter).
    pub confirm: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DeletionScheduled {
    /// When the worker erases the account. Logging in with the password
    /// before then restores it.
    pub delete_after: DateTime<Utc>,
}

/// POST /v1/user/me/delete
pub async fn delete_account(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Json(body): Json<DeleteAccountRequest>,
) -> ApiResult<(StatusCode, Json<DeletionScheduled>)> {
    if !limits::deletion_attempt(&state, auth_user.id, &ip).await? {
        return Err(AppError::RateLimited);
    }
    let user = users_db::find_by_id(&state.db, auth_user.id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    if !body.confirm.trim().eq_ignore_ascii_case(&user.username) {
        return Err(AppError::BadRequest(
            "confirm must be the account's username".into(),
        ));
    }
    if body.password.len() > PASSWORD_LOGIN_MAX_LEN
        || verify_password(&body.password, &user.password_hash).is_err()
    {
        return Err(AppError::WrongPassword);
    }

    let scheduled =
        accounts_db::schedule_deletion(&state.db, user.id, state.accounts.deletion_grace).await?;
    for session_id in scheduled.sessions {
        let cache_key = format!("session:{session_id}");
        if let Err(err) = state.redis.del::<i64, _>(&cache_key).await {
            // Until the entry expires, the session can still act.
            tracing::error!(%session_id, ?err, "failed to invalidate a deleted account's session cache");
        }
    }
    tracing::info!(
        user_id = user.id,
        delete_after = %scheduled.delete_after,
        "account deletion scheduled"
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(DeletionScheduled {
            delete_after: scheduled.delete_after,
        }),
    ))
}

pub fn _delete_account_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Delete my account")
        .description(&format!(
            "Deletes the authenticated user's account, given its password and its username typed out as `confirm`. At once: every session (this one included), API token, scrobbler credential and authorization, and linked account (with its stored OAuth tokens) is revoked, imports in progress stop, and the account disappears from profiles, search, listener lists, comments and follower lists. Its history stays for a grace period (`ACCOUNT_DELETION_GRACE_DAYS`, {DEFAULT_GRACE_DAYS} days by default) during which logging in with the password restores it (credentials and links have to be made again); after it the worker erases everything about the account, its plays from catalog counts and rankings included. Takes a session, never an API token. At most {} attempts an hour per account and {} per address.",
            limits::DELETION_ATTEMPTS_PER_USER,
            limits::DELETION_ATTEMPTS_PER_IP,
        ))
        .tag("Account")
        .response::<202, Json<DeletionScheduled>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("`confirm` isn't the account's username"))
        .response_with::<403, ErrorJson, _>(|r| r.description("Wrong password, or not a session"))
        .response_with::<429, ErrorJson, _>(|r| {
            r.description(&format!(
                "Over {} attempts this hour for this account or {} from this address, or over the request limit for this address",
                limits::DELETION_ATTEMPTS_PER_USER,
                limits::DELETION_ATTEMPTS_PER_IP
            ))
        })
}

#[cfg(test)]
mod tests {
    use axum::http::Method;
    use chrono::{TimeDelta, Utc};
    use serde_json::json;

    use super::*;
    use crate::compat::CompatConfig;
    use crate::test_app::{PASSWORD, with_app};

    /// Deleting takes the password and the username, revokes every
    /// credential, hides the account, and a login before the erase brings
    /// it back.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn deleting_revokes_everything_hides_the_account_and_a_login_restores_it() {
        with_app(CompatConfig::default(), |app| async move {
            let play = json!({
                "track": "Song",
                "artist": "Band",
                "played_at": (Utc::now() - TimeDelta::hours(1)).to_rfc3339(),
            });
            app.api(Method::POST, "/v1/scrobble", Some(play)).await;
            let (_, token) = app
                .api(
                    Method::POST,
                    "/v1/auth/tokens",
                    Some(json!({ "name": "all", "scopes": ["scrobble", "read", "write"] })),
                )
                .await;
            let token = token["token"].as_str().unwrap().to_string();
            let profile = format!("/v1/user/{}", app.username);
            let delete =
                |password: &str, confirm: &str| json!({ "password": password, "confirm": confirm });

            // Not with an API token, whatever its scopes.
            let (status, _) = app
                .api_as(
                    &token,
                    Method::POST,
                    "/v1/user/me/delete",
                    Some(delete(PASSWORD, &app.username)),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            let (status, _) = app
                .api(
                    Method::POST,
                    "/v1/user/me/delete",
                    Some(delete(PASSWORD, "someone")),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (status, body) = app
                .api(
                    Method::POST,
                    "/v1/user/me/delete",
                    Some(delete("wrong", &app.username)),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body["error"], "the password is wrong");
            // Still signed in after a wrong password.
            assert_eq!(
                app.api(Method::GET, "/v1/user/me", None).await.0,
                StatusCode::OK
            );

            let (status, body) = app
                .api(
                    Method::POST,
                    "/v1/user/me/delete",
                    Some(delete(PASSWORD, &app.username.to_uppercase())),
                )
                .await;
            assert_eq!(status, StatusCode::ACCEPTED);
            let delete_after: chrono::DateTime<Utc> =
                body["delete_after"].as_str().unwrap().parse().unwrap();
            assert!((delete_after - Utc::now() - TimeDelta::days(7)).abs() < TimeDelta::minutes(1));

            // Every credential is gone, the session (cached) included.
            assert_eq!(
                app.api(Method::GET, "/v1/user/me", None).await.0,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                app.api_as(&token, Method::GET, "/v1/user/me", None).await.0,
                StatusCode::UNAUTHORIZED
            );
            // Hidden everywhere.
            assert_eq!(app.get(&profile).await.0, StatusCode::NOT_FOUND);
            assert_eq!(
                app.get(&format!("{profile}/recent")).await.0,
                StatusCode::NOT_FOUND
            );
            let (_, _, search) = app
                .get(&format!("/v1/search?type=user&q={}", app.username))
                .await;
            assert!(!search.contains(&app.username));
            // Its name is still taken.
            let register = json!({
                "username": app.username,
                "email": "someone.else@example.com",
                "password": "another horse battery staple 42!",
            });
            let (status, _) = app
                .api_as("x", Method::POST, "/v1/auth/register", Some(register))
                .await;
            assert_eq!(status, StatusCode::CONFLICT);

            // A login brings it back, history and all, credentials not.
            let login = json!({ "username": app.username, "password": PASSWORD });
            let (status, body) = app
                .api_as("x", Method::POST, "/v1/auth/login", Some(login.clone()))
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["account_restored"], true);
            assert_eq!(app.get(&profile).await.0, StatusCode::OK);
            let session = body["token"].as_str().unwrap().to_string();
            let (_, me) = app.api_as(&session, Method::GET, "/v1/user/me", None).await;
            assert_eq!(me["scrobble_count"], 1);
            let (_, tokens) = app
                .api_as(&session, Method::GET, "/v1/auth/tokens", None)
                .await;
            assert_eq!(tokens, json!([]));
            let (_, body) = app
                .api_as("x", Method::POST, "/v1/auth/login", Some(login))
                .await;
            assert_eq!(body["account_restored"], false);

            // Once the erase has begun, a login is too late.
            let (status, _) = app
                .api_as(
                    &session,
                    Method::POST,
                    "/v1/user/me/delete",
                    Some(delete(PASSWORD, &app.username)),
                )
                .await;
            assert_eq!(status, StatusCode::ACCEPTED);
            sqlx::query(
                "UPDATE users SET delete_after = NOW(), erase_started_at = NOW() WHERE id = $1",
            )
            .bind(app.user_id)
            .execute(&app.pool)
            .await
            .unwrap();
            let login = json!({ "username": app.username, "password": PASSWORD });
            let (status, _) = app
                .api_as("x", Method::POST, "/v1/auth/login", Some(login))
                .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        })
        .await;
    }
}
