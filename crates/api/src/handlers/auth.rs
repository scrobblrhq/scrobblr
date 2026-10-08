use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, State},
    http::StatusCode,
};
use chrono::Utc;
use fred::interfaces::KeysInterface;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::UuidPath;
use crate::{
    errors::{ApiResult, AppError},
    limits,
    middleware::auth::{AuthUser, Credential, Scope},
    middleware::rate_limit::ClientIp,
    state::AppState,
};
use db::queries::{auth as auth_db, users as users_db};
use shared::user::{hash_password, verify_password};
use shared::validation::{
    self, PASSWORD_LOGIN_MAX_LEN, USERNAME_MAX_LEN, ValidationError, sanitize_display_name,
    validate_email, validate_password, validate_username,
};

// Reserved usernames that cannot be registered (e.g. "me" for /user/me)
const RESERVED_USERNAMES: &[&str] = &["me", "settings", "admin", "api"];

const API_TOKEN_NAME_MAX_CHARS: usize = 100;

/// The longest an API token can be set to last: ten years. One that should
/// never expire is created without `expires_days`.
const API_TOKEN_MAX_DAYS: i64 = 3650;

/// Hash of a value nobody can supply, verified against when no user
/// matches so a missing account costs the same Argon2 work as a real one.
/// Without it, response latency turns login into a username oracle.
pub(crate) fn decoy_password_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        let filler = hex::encode(rand::random::<[u8; 32]>());
        hash_password(&filler).expect("hashing a generated value cannot fail")
    })
}

impl From<ValidationError> for AppError {
    fn from(e: ValidationError) -> Self {
        AppError::BadRequest(e.to_string())
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegisterRequest {
    pub username: String,
    pub email: String,
    pub password: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AuthResponse {
    pub token: Uuid,
    pub user_id: i64,
    pub username: String,
}

/// POST /v1/register
pub async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> ApiResult<(StatusCode, Json<AuthResponse>)> {
    let username = validation::normalize_username(&body.username);
    validate_username(username, RESERVED_USERNAMES)?;

    let email = validate_email(&body.email)?;
    validate_password(&body.password, username, &email)?;
    let display_name = sanitize_display_name(body.display_name.as_deref())?;

    if users_db::find_by_username(&state.db, username)
        .await?
        .is_some()
    {
        return Err(AppError::UsernameTaken);
    }
    if users_db::find_by_email(&state.db, &email).await?.is_some() {
        return Err(AppError::EmailTaken);
    }

    let password_hash =
        hash_password(&body.password).map_err(|e| AppError::BadRequest(e.to_string()))?;

    let user = users_db::create_user(
        &state.db,
        &users_db::CreateUser {
            username,
            email: &email,
            password_hash: &password_hash,
            display_name: display_name.as_deref(),
        },
    )
    .await
    .map_err(|e| match &e {
        // The pre-checks above race with concurrent registrations; the unique
        // constraints are the source of truth, so map their violations to 409s.
        sqlx::Error::Database(db) if db.constraint() == Some("users_username_key") => {
            AppError::UsernameTaken
        }
        sqlx::Error::Database(db) if db.constraint() == Some("users_email_key") => {
            AppError::EmailTaken
        }
        _ => AppError::Database(e),
    })?;

    let session = auth_db::create_session(&state.db, user.id, None, None).await?;

    Ok((
        StatusCode::CREATED,
        Json(AuthResponse {
            token: session.id,
            user_id: user.id,
            username: user.username,
        }),
    ))
}

pub fn _register_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Register a new user")
        .description("Creates a new user account and returns a session token. Usernames are 2-30 characters of ASCII letters, digits and the separators `_ - .`, starting and ending alphanumeric. Passwords are 12-128 characters combining at least three of lowercase, uppercase, digits and symbols, and may not contain the username or email.")
        .tag("Auth")
        .response::<201, Json<AuthResponse>>()
        .response_with::<400, (), _>(|r| r.description("Validation error on username, email, password or display name"))
        .response_with::<401, (), _>(|r| r.description("Missing or invalid first-party app signature"))
        .response_with::<409, (), _>(|r| r.description("Username or email already taken"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Checks a username and password the way every password login must: only
/// the bounds that keep a hostile body away from Argon2 (not the
/// registration rules, which would lock out accounts made before them),
/// then Argon2 against the user's hash or, when no user matches, a decoy,
/// so a missing account costs the same time as a wrong password.
/// `with_email` also accepts the account's email address as the username.
pub(crate) async fn verify_password_login(
    db: &sqlx::PgPool,
    username: &str,
    password: &str,
    with_email: bool,
) -> ApiResult<Option<shared::models::User>> {
    let username = username.trim();
    let max_len = if with_email {
        validation::EMAIL_MAX_LEN
    } else {
        USERNAME_MAX_LEN
    };
    if username.is_empty()
        || username.chars().count() > max_len
        || password.len() > PASSWORD_LOGIN_MAX_LEN
    {
        return Ok(None);
    }

    let user = if with_email && username.contains('@') {
        users_db::find_by_email(db, username).await?
    } else {
        users_db::find_by_username(db, username).await?
    };

    let password_hash = user
        .as_ref()
        .map(|u| u.password_hash.as_str())
        .unwrap_or_else(|| decoy_password_hash());
    let verified = verify_password(password, password_hash).is_ok();
    Ok(user.filter(|_| verified))
}

/// POST /v1/login
pub async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<LoginRequest>,
) -> ApiResult<Json<AuthResponse>> {
    if !limits::login_attempt(&state, &ip, &body.username).await? {
        return Err(AppError::RateLimited);
    }
    let user = verify_password_login(&state.db, &body.username, &body.password, false)
        .await?
        .ok_or(AppError::InvalidCredentials)?;
    limits::login_succeeded(&state, &body.username).await;

    let session = auth_db::create_session(&state.db, user.id, None, None).await?;

    Ok(Json(AuthResponse {
        token: session.id,
        user_id: user.id,
        username: user.username,
    }))
}

pub fn _login_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Log in")
        .description("Authenticates a user with username and password. Returns a session token to be used as `Bearer` in the `Authorization` header. Attempts are limited per address and per username, together with the scrobbler APIs' password logins; a success resets the username's count.")
        .tag("Auth")
        .response::<200, Json<AuthResponse>>()
        .response_with::<401, (), _>(|r| r.description("Invalid credentials, or missing/invalid first-party app signature"))
        .response_with::<429, (), _>(|r| r.description("More than 20 attempts from this address or 10 for this username in 15 minutes"))
}

/// POST /v1/logout
pub async fn logout(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
) -> ApiResult<StatusCode> {
    let deleted_ids = sqlx::query_scalar!(
        "DELETE FROM user_sessions WHERE user_id = $1 RETURNING id",
        auth_user.id,
    )
    .fetch_all(&state.db)
    .await?;

    for session_id in deleted_ids {
        let cache_key = format!("session:{session_id}");
        if let Err(err) = state.redis.del::<i64, _>(&cache_key).await {
            tracing::warn!(%session_id, ?err, "failed to invalidate session cache on logout");
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

pub fn _logout_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Log out")
        .description("Invalidates all active sessions for the authenticated user (logout from all devices). Requires a valid session token.")
        .tag("Auth")
        .response_with::<204, (), _>(|r| r.description("Successfully logged out"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTokenRequest {
    /// 1 to 100 characters, e.g. the client it's for.
    pub name: String,
    /// Any of `scrobble`, `read` and `write`; `["scrobble"]` when omitted.
    pub scopes: Option<Vec<String>>,
    /// Days until the token expires, 1 to 3650; it never does when omitted.
    pub expires_days: Option<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CreateTokenResponse {
    pub id: Uuid,
    pub name: String,
    pub token: String, // raw token — shown only once
    pub scopes: Vec<String>,
    pub expires_at: Option<chrono::DateTime<Utc>>,
    pub created_at: chrono::DateTime<Utc>,
}

/// POST /v1/auth/tokens
pub async fn create_api_token(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<CreateTokenRequest>,
) -> ApiResult<impl IntoApiResponse> {
    check_expires_days(body.expires_days)?;
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > API_TOKEN_NAME_MAX_CHARS {
        return Err(AppError::BadRequest(format!(
            "name must be 1 to {API_TOKEN_NAME_MAX_CHARS} characters"
        )));
    }

    // Generate a cryptographically random 32-byte token
    let raw_bytes: [u8; 32] = rand::random();
    let raw_token = hex::encode(raw_bytes);

    // Only the SHA-256 hash is stored; the raw token appears once in the
    // response and can never be recovered afterwards.
    let token_hash = auth_db::hash_api_token(&raw_token);

    let scopes: Vec<String> = match &body.scopes {
        Some(names) => parse_scopes(names)?,
        None => vec![Scope::Scrobble],
    }
    .into_iter()
    .map(|scope| scope.as_str().to_string())
    .collect();

    let api_token = auth_db::create_api_token(
        &state.db,
        auth_user.id,
        name,
        &token_hash,
        &scopes,
        body.expires_days,
    )
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateTokenResponse {
            id: api_token.id,
            name: api_token.name,
            token: raw_token, // only time the raw token is shown
            scopes: api_token.scopes,
            expires_at: api_token.expires_at,
            created_at: api_token.created_at,
        }),
    ))
}

/// The scopes a new token asks for, deduplicated in canonical order. A name
/// this server doesn't know is an error rather than stored, and a token
/// needs at least one scope.
fn parse_scopes(names: &[String]) -> ApiResult<Vec<Scope>> {
    let known = || Scope::ALL.map(Scope::as_str).join(", ");
    if let Some(unknown) = names.iter().find(|name| Scope::parse(name).is_none()) {
        return Err(AppError::BadRequest(format!(
            "unknown scope `{unknown}` (the scopes are {})",
            known()
        )));
    }
    let scopes: Vec<Scope> = Scope::ALL
        .into_iter()
        .filter(|scope| names.iter().any(|name| name == scope.as_str()))
        .collect();
    if scopes.is_empty() {
        return Err(AppError::BadRequest(format!(
            "a token needs at least one scope ({})",
            known()
        )));
    }
    Ok(scopes)
}

/// Refuses an `expires_days` outside 1 to [`API_TOKEN_MAX_DAYS`]: zero or
/// less would create a token that's already expired, and too many days
/// overflow the expiry date, which panics.
fn check_expires_days(expires_days: Option<i64>) -> ApiResult<()> {
    if let Some(days) = expires_days
        && !(1..=API_TOKEN_MAX_DAYS).contains(&days)
    {
        return Err(AppError::BadRequest(format!(
            "expires_days must be 1 to {API_TOKEN_MAX_DAYS}, or left out for a token that never expires"
        )));
    }
    Ok(())
}

pub fn _create_api_token_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Create an API token")
        .description("Generates a new long-lived API token for programmatic access (e.g. scrobbling from a music player). The raw token is only shown once — store it securely. `scopes` (default `[\"scrobble\"]`) says what it may do, and scopes don't imply one another: `scrobble` submits scrobbles and now playing, `read` reads the account's own data (profile, imports, connected accounts), and `write` changes the account and posts as it (profile, follows, comments, votes, uploads, imports, catalog refreshes). `expires_days`, 1 to 3650 (ten years), sets when it expires; without it, it never does. Only a session can create, list all or revoke tokens, log out, manage scrobbler credentials and connect accounts.")
        .tag("Auth")
        .response::<201, Json<CreateTokenResponse>>()
        .response_with::<400, (), _>(|r| r.description("A name that's empty or over 100 characters, an unknown scope or none, or `expires_days` outside 1 to 3650"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

/// GET /v1/auth/tokens
pub async fn list_api_tokens(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
) -> ApiResult<impl IntoApiResponse> {
    let mut tokens = auth_db::list_api_tokens(&state.db, auth_user.id).await?;
    // An API token sees only itself: enough for a client to check the token
    // it was given, without learning of the account's others.
    if let Credential::ApiToken { id, .. } = auth_user.credential {
        tokens.retain(|token| token.id == id);
    }
    Ok(Json(tokens))
}

pub fn _list_api_tokens_doc(op: TransformOperation) -> TransformOperation {
    op.summary("List API tokens")
        .description("With a session, returns every API token of the authenticated user; with an API token, only that token, so a client can check the token it was given (its scopes and expiry). The raw token value is never returned here — only metadata.")
        .tag("Auth")
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

/// DELETE /v1/auth/tokens/:token_id
pub async fn delete_api_token(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    axum::extract::Path(UuidPath { id: token_id }): axum::extract::Path<UuidPath>,
) -> ApiResult<impl IntoApiResponse> {
    let deleted = auth_db::delete_api_token(&state.db, token_id, auth_user.id).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

pub fn _delete_api_token_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Revoke an API token")
        .description("Permanently deletes an API token by its ID. Only the owner of the token can revoke it.")
        .tag("Auth")
        .response_with::<204, (), _>(|r| r.description("Token successfully revoked"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Token not found or not owned by the authenticated user"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(names: &[&str]) -> ApiResult<Vec<Scope>> {
        parse_scopes(&names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn requested_scopes_are_deduplicated_in_order() {
        assert_eq!(parse(&["scrobble"]).unwrap(), [Scope::Scrobble]);
        assert_eq!(
            parse(&["write", "read", "write"]).unwrap(),
            [Scope::Read, Scope::Write]
        );
    }

    #[test]
    fn unknown_or_missing_scopes_are_refused() {
        assert!(matches!(
            parse(&["scrobble", "admin"]),
            Err(AppError::BadRequest(m)) if m.contains("`admin`")
        ));
        assert!(matches!(parse(&["Write"]), Err(AppError::BadRequest(_))));
        assert!(matches!(parse(&[]), Err(AppError::BadRequest(_))));
    }

    #[test]
    fn tokens_expire_in_a_day_to_ten_years_or_never() {
        for days in [None, Some(1), Some(365), Some(API_TOKEN_MAX_DAYS)] {
            assert!(check_expires_days(days).is_ok(), "{days:?}");
        }
        // Already expired, or past the expiry chrono can compute (it panics).
        for days in [
            0,
            -1,
            API_TOKEN_MAX_DAYS + 1,
            100_000_000_000,
            i64::MAX,
            i64::MIN,
        ] {
            assert!(
                matches!(check_expires_days(Some(days)), Err(AppError::BadRequest(m)) if m.contains("expires_days")),
                "{days}"
            );
        }
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn password_logins_are_limited_per_username() {
        use crate::test_app::{PASSWORD, with_app};
        use axum::http::Method;
        use serde_json::json;

        with_app(Default::default(), |app| async move {
            let login = |password: &str| json!({ "username": app.username, "password": password });
            let (status, _) = app
                .api(Method::POST, "/v1/auth/login", Some(login(PASSWORD)))
                .await;
            assert_eq!(status, StatusCode::OK);
            for _ in 0..limits::LOGIN_ATTEMPTS_PER_USER {
                let (status, _) = app
                    .api(Method::POST, "/v1/auth/login", Some(login("wrong")))
                    .await;
                assert_eq!(status, StatusCode::UNAUTHORIZED);
            }
            let (status, _) = app
                .api(Method::POST, "/v1/auth/login", Some(login(PASSWORD)))
                .await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        })
        .await;
    }
}
