//! Exporting and deleting the account: sessions only, so a leaked API token
//! can't carry the account away or delete it.

use std::sync::Arc;

use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, State},
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
};
use chrono::{DateTime, TimeDelta, Utc};
use fred::interfaces::KeysInterface;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    export, limits,
    middleware::auth::{AuthUser, Credential},
    middleware::rate_limit::ClientIp,
    state::AppState,
};
use db::queries::{accounts as accounts_db, users as users_db};
use shared::user::verify_password;
use shared::validation::PASSWORD_LOGIN_MAX_LEN;

/// Per API instance; each holds a database connection while it reads a page.
const EXPORTS_AT_ONCE: usize = 2;

#[derive(Clone)]
pub struct AccountConfig {
    pub deletion_grace: TimeDelta,
    pub exports: Arc<Semaphore>,
}

pub const DEFAULT_GRACE_DAYS: i64 = 7;
pub const MAX_GRACE_DAYS: i64 = 30;

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            deletion_grace: TimeDelta::days(DEFAULT_GRACE_DAYS),
            exports: Arc::new(Semaphore::new(EXPORTS_AT_ONCE)),
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
            ..Self::default()
        })
    }
}

/// GET /v1/user/me/export
pub async fn export_account(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
) -> ApiResult<impl IntoApiResponse> {
    let Ok(permit) = state.accounts.exports.clone().try_acquire_owned() else {
        return Err(AppError::ServiceUnavailable(
            "other exports are running, try again in a few minutes".into(),
        ));
    };
    if !limits::export_attempt(&state, auth_user.id, &ip).await? {
        return Err(AppError::RateLimited);
    }
    let user = users_db::find_by_id(&state.db, auth_user.id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let session = match auth_user.credential {
        Credential::Session { id } => Some(id),
        Credential::ApiToken { .. } => None,
    };
    let filename = format!(
        "scrobblr-{}-{}.zip",
        user.username,
        Utc::now().format("%Y%m%d")
    );
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .map_err(|e| AppError::Internal(e.into()))?;
    let body = export::stream(state, user, session, permit);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response())
}

pub fn _export_account_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Export my account")
        .description(&format!(
            "Downloads everything Scrobblr holds about the authenticated user as a ZIP archive (`{}`), streamed as it is read: `account.json` (profile and settings, follows, the accounts the user blocks and the followers it removed (never who blocks or removed it), comments, image uploads and likes, sessions without their ids, API tokens and scrobbler credentials without their values, linked accounts without their OAuth tokens, imports, now playing, and `not_included`: what is left out and why), `scrobbles.csv` and `scrobbles.json` (every scrobble, oldest first, with its source, client, import and the classifier's label, never ranking weights), and `uploads/` (the avatar and artwork the user uploaded). Nothing is stored on the server: a broken download is asked for again. Takes a session, never an API token. At most {} exports a day per account and {} per address, and {EXPORTS_AT_ONCE} at once on the server.",
            export::FORMAT,
            limits::EXPORTS_PER_USER,
            limits::EXPORTS_PER_IP,
        ))
        .tag("Account")
        .response_with::<200, (), _>(|mut r| {
            r.inner().content.insert(
                "application/zip".into(),
                aide::openapi::MediaType {
                    schema: Some(aide::openapi::SchemaObject {
                        json_schema: schemars::json_schema!({ "type": "string", "format": "binary" }),
                        external_docs: None,
                        example: None,
                    }),
                    ..Default::default()
                },
            );
            r.description("The archive, as an attachment")
        })
        .response_with::<429, ErrorJson, _>(|r| {
            r.description(&format!(
                "Over {} exports today for this account or {} from this address, or over the request limit for this address",
                limits::EXPORTS_PER_USER,
                limits::EXPORTS_PER_IP
            ))
        })
        .response_with::<503, ErrorJson, _>(|r| r.description("Other exports are running: try again in a few minutes"))
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
    use std::io::{Cursor, Read};

    use axum::body::Body;
    use axum::http::{Method, Request};
    use chrono::{TimeDelta, Utc};
    use serde_json::{Value, json};

    use super::*;
    use crate::compat::CompatConfig;
    use crate::media::tests::{multipart, png};
    use crate::test_app::{PASSWORD, TestApp, with_app};

    async fn export(app: &TestApp, bearer: &str) -> (StatusCode, Vec<u8>) {
        let req = Request::get("/v1/user/me/export")
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap();
        let (status, headers, body) = app.send_for_bytes(req).await;
        if status == StatusCode::OK {
            assert_eq!(headers[header::CONTENT_TYPE], "application/zip");
            assert!(
                headers[header::CONTENT_DISPOSITION]
                    .to_str()
                    .unwrap()
                    .starts_with("attachment; filename=\"scrobblr-")
            );
            assert!(headers.get(header::CONTENT_ENCODING).is_none());
        }
        (status, body)
    }

    fn unzip(bytes: Vec<u8>) -> Vec<(String, Vec<u8>)> {
        let mut archive = ::zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        (0..archive.len())
            .map(|i| {
                let mut file = archive.by_index(i).unwrap();
                let mut content = Vec::new();
                file.read_to_end(&mut content).unwrap();
                (file.name().unwrap().to_string(), content)
            })
            .collect()
    }

    /// The export holds the account and every scrobble, with labels and
    /// clients, the avatar, and none of the credentials' secrets.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn the_export_holds_the_account_and_its_history_but_no_secret() {
        with_app(CompatConfig::default(), |app| async move {
            for i in 0..3 {
                let play = json!({
                    "track": format!("Song {i}"),
                    "artist": "Band, \"The\"",
                    "album": "=cmd|' /C calc'!A0",
                    "played_at": (Utc::now() - TimeDelta::hours(i + 1)).to_rfc3339(),
                    "duration_ms": 200_000,
                    "source": "ytmusic",
                });
                let (status, _) = app.api(Method::POST, "/v1/scrobble", Some(play)).await;
                assert_eq!(status, StatusCode::CREATED);
            }
            let (status, _, _) = app
                .send(multipart(
                    "/v1/user/me/avatar",
                    &app.session.to_string(),
                    &png(),
                ))
                .await;
            assert_eq!(status, StatusCode::OK);
            let (_, token) = app
                .api(
                    Method::POST,
                    "/v1/auth/tokens",
                    Some(json!({ "name": "player", "scopes": ["scrobble", "read"] })),
                )
                .await;
            let token = token["token"].as_str().unwrap().to_string();
            let (_, scrobbler) = app
                .api(
                    Method::POST,
                    "/v1/scrobbler/tokens",
                    Some(json!({ "name": "pano" })),
                )
                .await;
            let scrobbler = scrobbler["token"].as_str().unwrap().to_string();
            // Whom the user blocks and removed is theirs; who blocks or
            // removed them isn't.
            let (_, blocked, _) = app.other_user("blockedqz").await;
            let (_, removed, removed_session) = app.other_user("removedqz").await;
            let (_, blocker, blocker_session) = app.other_user("blockerqz").await;
            let (_, remover, remover_session) = app.other_user("removerqz").await;
            app.api(Method::POST, &format!("/v1/user/{blocked}/block"), None)
                .await;
            for (session, target) in [
                (&removed_session, &app.username),
                (&app.session.to_string(), &remover),
            ] {
                app.api_as(
                    session,
                    Method::POST,
                    &format!("/v1/user/{target}/follow"),
                    None,
                )
                .await;
            }
            app.api(
                Method::DELETE,
                &format!("/v1/user/me/followers/{removed}"),
                None,
            )
            .await;
            app.api_as(
                &blocker_session,
                Method::POST,
                &format!("/v1/user/{}/block", app.username),
                None,
            )
            .await;
            app.api_as(
                &remover_session,
                Method::DELETE,
                &format!("/v1/user/me/followers/{}", app.username),
                None,
            )
            .await;

            // Not with an API token, even one that may read.
            let (status, _) = export(&app, &token).await;
            assert_eq!(status, StatusCode::FORBIDDEN);

            let (status, bytes) = export(&app, &app.session.to_string()).await;
            assert_eq!(status, StatusCode::OK);
            let text = String::from_utf8_lossy(&bytes).to_string();
            let files = unzip(bytes);
            let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(
                names[..4],
                [
                    "README.txt",
                    "account.json",
                    "scrobbles.csv",
                    "scrobbles.json"
                ]
            );
            assert!(names[4].starts_with("uploads/avatars/"), "{names:?}");
            assert_eq!(files[4].1[..2], [0xff, 0xd8], "the avatar, a JPEG");

            let file = |name: &str| {
                let (_, content) = files.iter().find(|(n, _)| n == name).unwrap();
                String::from_utf8(content.clone()).unwrap()
            };
            let account: Value = serde_json::from_str(&file("account.json")).unwrap();
            assert_eq!(account["format"], export::FORMAT);
            assert_eq!(account["profile"]["username"], app.username.as_str());
            assert_eq!(
                account["profile"]["email"],
                format!("{}@test", app.username)
            );
            assert_eq!(account["profile"]["image_file"], names[4]);
            assert_eq!(account["api_tokens"][0]["name"], "player");
            assert_eq!(account["scrobbler_credentials"][0]["name"], "pano");
            assert_eq!(account["sessions"][0]["current"], true);
            assert!(account["sessions"][0].get("id").is_none());
            assert!(account["not_included"].as_array().unwrap().len() >= 5);
            assert_eq!(account["blocks"][0]["username"], blocked.as_str());
            assert_eq!(account["blocks"].as_array().unwrap().len(), 1);
            assert_eq!(
                account["removed_followers"][0]["username"],
                removed.as_str()
            );
            assert!(account["removed_followers"][0]["until"].is_string());
            assert_eq!(account["following"], json!([]));
            // No secret, hashed or not, anywhere in the archive (stored
            // entries are searched as they are, deflated ones decompressed).
            let everything: String = files
                .iter()
                .map(|(_, c)| String::from_utf8_lossy(c).to_string())
                .collect::<Vec<_>>()
                .join("\n")
                + &text;
            for other in [&blocker, &remover] {
                assert!(
                    !everything.contains(other.as_str()),
                    "the export names {other}"
                );
            }
            for secret in [
                app.session.to_string(),
                token.clone(),
                db::queries::auth::hash_api_token(&token),
                scrobbler.clone(),
            ] {
                assert!(!everything.contains(&secret), "the export holds a secret");
            }
            let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
                .bind(app.user_id)
                .fetch_one(&app.pool)
                .await
                .unwrap();
            assert!(!everything.contains(&hash));

            let csv = file("scrobbles.csv");
            let lines: Vec<&str> = csv.lines().collect();
            assert_eq!(lines.len(), 4);
            assert!(lines[0].starts_with("played_at,artist,track,album,"));
            // Oldest first, quoted where needed, formulas defused.
            assert!(
                lines[1].contains(",\"Band, \"\"The\"\"\",Song 2,\"'=cmd|' /C calc'!A0\","),
                "{}",
                lines[1]
            );
            assert!(
                lines[1].contains(",ytmusic,scrobblr,ytmusic,false,"),
                "{}",
                lines[1]
            );
            let json: Vec<Value> = serde_json::from_str(&file("scrobbles.json")).unwrap();
            assert_eq!(json.len(), 3);
            assert_eq!(json[0]["track"], "Song 2");
            assert_eq!(json[0]["album"], "=cmd|' /C calc'!A0");
            assert_eq!(json[0]["client_protocol"], "scrobblr");
            assert!(json[0].get("weight").is_none());

            // Three a day.
            for _ in 1..limits::EXPORTS_PER_USER {
                assert_eq!(
                    export(&app, &app.session.to_string()).await.0,
                    StatusCode::OK
                );
            }
            assert_eq!(
                export(&app, &app.session.to_string()).await.0,
                StatusCode::TOO_MANY_REQUESTS
            );
        })
        .await;
    }

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
