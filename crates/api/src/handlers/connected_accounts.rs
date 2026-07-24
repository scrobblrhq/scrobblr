use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use fred::interfaces::KeysInterface;
use fred::types::Expiration;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    errors::{ApiResult, AppError},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::connected_accounts as connected_accounts_db;
use shared::spotify;

/// How long a CSRF `state` value is valid for. The user completes Spotify's
/// consent screen well within this window in practice.
const OAUTH_STATE_TTL_SECS: i64 = 600;

/// Only "spotify" is supported today — Deezer's public API has no
/// authenticated now-playing/recently-played endpoint, so there is nothing
/// for a worker poller to call. The path still takes `{provider}` (rather
/// than hardcoding "spotify" into the route) so adding a real provider later
/// doesn't require a route change.
fn ensure_supported_provider(provider: &str) -> ApiResult<()> {
    if provider == "spotify" {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "unsupported provider: {provider}"
        )))
    }
}

fn spotify_client_id() -> ApiResult<String> {
    std::env::var("SPOTIFY_CLIENT_ID")
        .map_err(|_| AppError::Internal(anyhow::anyhow!("SPOTIFY_CLIENT_ID is not configured")))
}

fn spotify_client_secret() -> ApiResult<String> {
    std::env::var("SPOTIFY_CLIENT_SECRET")
        .map_err(|_| AppError::Internal(anyhow::anyhow!("SPOTIFY_CLIENT_SECRET is not configured")))
}

fn spotify_redirect_uri() -> ApiResult<String> {
    std::env::var("SPOTIFY_REDIRECT_URI")
        .map_err(|_| AppError::Internal(anyhow::anyhow!("SPOTIFY_REDIRECT_URI is not configured")))
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AuthorizeResponse {
    pub authorize_url: String,
}

/// GET /v1/connect/{provider}
pub async fn connect_provider(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(provider): Path<String>,
) -> ApiResult<impl IntoApiResponse> {
    ensure_supported_provider(&provider)?;

    let client_id = spotify_client_id()?;
    let redirect_uri = spotify_redirect_uri()?;

    // Random single-use CSRF token, mapped to this user so the callback
    // (which Spotify calls with no Scrobblr session) knows who to link the
    // account to. Deleted on first use in `spotify_callback`.
    let csrf_state = hex::encode(rand::random::<[u8; 16]>());
    state
        .redis
        .set::<(), _, _>(
            format!("oauth_state:spotify:{csrf_state}"),
            auth_user.id.to_string(),
            Some(Expiration::EX(OAUTH_STATE_TTL_SECS)),
            None,
            false,
        )
        .await
        .map_err(AppError::Redis)?;

    let authorize_url = spotify::build_authorize_url(&client_id, &redirect_uri, &csrf_state);
    Ok(Json(AuthorizeResponse { authorize_url }))
}

pub fn _connect_provider_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Start a connected-account OAuth flow")
        .description("Returns the provider's authorize URL the client should redirect the user to in order to link their account. Currently only `spotify` is supported.")
        .tag("Connected accounts")
        .response::<200, Json<AuthorizeResponse>>()
        .response_with::<400, (), _>(|r| r.description("Unsupported provider"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpotifyCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConnectResult {
    pub provider: String,
    pub connected: bool,
}

/// GET /v1/connect/spotify/callback
///
/// Public route: Spotify redirects the user's browser here with no scrobblr
/// session attached, so the `state` CSRF token (stored in Redis by
/// `connect_provider`, keyed to the user who started the flow) is what
/// identifies which account to link — not request auth.
pub async fn spotify_callback(
    State(state): State<AppState>,
    Query(q): Query<SpotifyCallbackQuery>,
) -> ApiResult<impl IntoApiResponse> {
    if let Some(err) = q.error {
        return Err(AppError::BadRequest(format!(
            "spotify authorization denied: {err}"
        )));
    }
    let code = q
        .code
        .ok_or_else(|| AppError::BadRequest("missing code".into()))?;
    let csrf_state = q
        .state
        .ok_or_else(|| AppError::BadRequest("missing state".into()))?;

    let redis_key = format!("oauth_state:spotify:{csrf_state}");
    let user_id: Option<String> = state.redis.get(&redis_key).await.map_err(AppError::Redis)?;
    let user_id: i64 = user_id
        .ok_or_else(|| AppError::BadRequest("invalid or expired oauth state".into()))?
        .parse()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("corrupt oauth state value in redis")))?;
    // Single-use: remove immediately so the same state can't be replayed.
    let _: () = state.redis.del(&redis_key).await.map_err(AppError::Redis)?;

    let client_id = spotify_client_id()?;
    let client_secret = spotify_client_secret()?;
    let redirect_uri = spotify_redirect_uri()?;

    let http = reqwest::Client::new();
    let tokens = spotify::exchange_code(&http, &client_id, &client_secret, &code, &redirect_uri)
        .await
        .map_err(|e| match e {
            spotify::SpotifyError::Http(err) => AppError::Internal(anyhow::anyhow!(err)),
            spotify::SpotifyError::Unauthorized | spotify::SpotifyError::Api(_) => {
                AppError::BadRequest(format!("spotify token exchange failed: {e}"))
            }
        })?;

    let provider_user_id = spotify::get_current_user_id(&http, &tokens.access_token)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;

    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(tokens.expires_in);

    connected_accounts_db::upsert_connected_account(
        &state.db,
        &connected_accounts_db::UpsertConnectedAccount {
            user_id,
            provider: "spotify".into(),
            provider_user_id,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            token_type: tokens.token_type,
            scope: Some(tokens.scope),
            expires_at: Some(expires_at),
        },
    )
    .await?;

    Ok(Json(ConnectResult {
        provider: "spotify".into(),
        connected: true,
    }))
}

pub fn _spotify_callback_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Spotify OAuth callback")
        .description("Spotify redirects here after the user grants or denies access. Exchanges the authorization code for tokens and links the account to whichever user started the flow (identified via the `state` CSRF token, not request auth).")
        .tag("Connected accounts")
        .response::<200, Json<ConnectResult>>()
        .response_with::<400, (), _>(|r| r.description("Missing/invalid code or state, or the user denied access"))
}

/// GET /v1/connect
pub async fn list_connected_accounts(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
) -> ApiResult<impl IntoApiResponse> {
    let accounts = connected_accounts_db::list_connected_accounts(&state.db, auth_user.id).await?;
    Ok(Json(accounts))
}

pub fn _list_connected_accounts_doc(op: TransformOperation) -> TransformOperation {
    op.summary("List connected accounts")
        .description("Returns the authenticated user's connected third-party accounts (never including tokens).")
        .tag("Connected accounts")
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

/// DELETE /v1/connect/{provider}
pub async fn disconnect(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(provider): Path<String>,
) -> ApiResult<impl IntoApiResponse> {
    ensure_supported_provider(&provider)?;

    let deleted =
        connected_accounts_db::delete_connected_account(&state.db, auth_user.id, &provider).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

pub fn _disconnect_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Disconnect an account")
        .description("Removes a connected third-party account. The worker stops polling it immediately; a fresh OAuth flow is required to relink.")
        .tag("Connected accounts")
        .response_with::<204, (), _>(|r| r.description("Disconnected"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("No connection for this provider"))
}
