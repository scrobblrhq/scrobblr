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

use super::ProviderPath;
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::connected_accounts::{self as connected_accounts_db, ConnectedAccountError};
use shared::lastfm::LastfmClient;
use shared::spotify;

/// How long a CSRF `state` value is valid for. The user completes Spotify's
/// consent screen well within this window in practice.
const OAUTH_STATE_TTL_SECS: i64 = 600;

/// Spotify (polled for scrobbles) and Last.fm (connected to prove
/// ownership before importing its history). Deezer's public API has no
/// authenticated now-playing/recently-played endpoint, so there is nothing
/// for a worker poller to call. The path still takes `{provider}` so adding
/// one later doesn't require a route change.
fn ensure_supported_provider(provider: &str) -> ApiResult<()> {
    if matches!(provider, "spotify" | "lastfm") {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "unsupported provider: {provider}"
        )))
    }
}

/// The Last.fm client, if this server can run the auth flow (which signs
/// requests with the shared secret).
pub fn lastfm_client() -> ApiResult<LastfmClient> {
    LastfmClient::from_env(reqwest::Client::new())
        .filter(LastfmClient::can_sign)
        .ok_or_else(|| {
            AppError::ServiceUnavailable(
                "Last.fm isn't configured on this server (LASTFM_API_KEY, LASTFM_SHARED_SECRET)"
                    .into(),
            )
        })
}

/// A Spotify setting; unset or blank means this server doesn't link
/// Spotify accounts.
fn spotify_setting(name: &str) -> ApiResult<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            AppError::ServiceUnavailable(format!(
                "Spotify isn't configured on this server ({name})"
            ))
        })
}

fn spotify_client_id() -> ApiResult<String> {
    spotify_setting("SPOTIFY_CLIENT_ID")
}

fn spotify_client_secret() -> ApiResult<String> {
    spotify_setting("SPOTIFY_CLIENT_SECRET")
}

fn spotify_redirect_uri() -> ApiResult<String> {
    spotify_setting("SPOTIFY_REDIRECT_URI")
}

/// Splits Spotify failures by who can act on them. A rejected code, a
/// redirect-URI mismatch or a revoked token are all things the *client* can
/// fix by restarting the connect flow, so they're 400s; only a transport
/// failure talking to Spotify is genuinely our problem and worth a 500.
fn spotify_error(context: &str, e: spotify::SpotifyError) -> AppError {
    match e {
        spotify::SpotifyError::Http(err) => AppError::Internal(anyhow::anyhow!(err)),
        e @ (spotify::SpotifyError::Unauthorized | spotify::SpotifyError::Api(_)) => {
            AppError::BadRequest(format!("{context}: {e}"))
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AuthorizeResponse {
    pub authorize_url: String,
}

/// GET /v1/connect/{provider}
pub async fn connect_provider(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(ProviderPath { provider }): Path<ProviderPath>,
) -> ApiResult<impl IntoApiResponse> {
    ensure_supported_provider(&provider)?;

    // Random single-use CSRF token, mapped to this user so the callback
    // (which the provider calls with no Scrobblr session) knows who to link
    // the account to. Deleted on first use in the callback.
    let csrf_state = hex::encode(rand::random::<[u8; 16]>());
    let authorize_url = if provider == "lastfm" {
        // Last.fm has no `state` parameter, but keeps the callback's own
        // query string and appends `token` to it.
        let callback = format!(
            "{}/v1/connect/lastfm/callback?state={csrf_state}",
            state.public_base_url
        );
        lastfm_client()?.auth_url(&callback)
    } else {
        spotify::build_authorize_url(&spotify_client_id()?, &spotify_redirect_uri()?, &csrf_state)
    };

    state
        .redis
        .set::<(), _, _>(
            format!("oauth_state:{provider}:{csrf_state}"),
            auth_user.id.to_string(),
            Some(Expiration::EX(OAUTH_STATE_TTL_SECS)),
            None,
            false,
        )
        .await
        .map_err(AppError::Redis)?;

    Ok(Json(AuthorizeResponse { authorize_url }))
}

pub fn _connect_provider_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Start a connected-account OAuth flow")
        .description("Returns the provider's authorize URL the client should redirect the user to in order to link their account: `spotify` (auto-scrobbling) or `lastfm` (proves ownership of the Last.fm account before importing its history).")
        .tag("Connected accounts")
        .response::<200, Json<AuthorizeResponse>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Unsupported provider"))
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<503, ErrorJson, _>(|r| r.description("The provider isn't configured on this server"))
}

/// Resolves and burns a callback's CSRF `state`, returning the user who
/// started the flow.
async fn take_oauth_state(state: &AppState, provider: &str, csrf_state: &str) -> ApiResult<i64> {
    let redis_key = format!("oauth_state:{provider}:{csrf_state}");
    let user_id: Option<String> = state.redis.get(&redis_key).await.map_err(AppError::Redis)?;
    let user_id: i64 = user_id
        .ok_or_else(|| AppError::BadRequest("invalid or expired oauth state".into()))?
        .parse()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("corrupt oauth state value in redis")))?;
    // Single-use: remove immediately so the same state can't be replayed.
    let _: () = state.redis.del(&redis_key).await.map_err(AppError::Redis)?;
    Ok(user_id)
}

/// The external account is already linked to a different Scrobblr user:
/// theirs to resolve (unlink it there first), not a server fault.
fn link_error(e: ConnectedAccountError) -> AppError {
    match e {
        ConnectedAccountError::Db(sqlx::Error::Database(db))
            if db.constraint()
                == Some(connected_accounts_db::PROVIDER_ACCOUNT_TAKEN_CONSTRAINT) =>
        {
            AppError::ProviderAccountTaken
        }
        other => other.into(),
    }
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

    let user_id = take_oauth_state(&state, "spotify", &csrf_state).await?;

    let client_id = spotify_client_id()?;
    let client_secret = spotify_client_secret()?;
    let redirect_uri = spotify_redirect_uri()?;

    let http = reqwest::Client::new();
    let tokens = spotify::exchange_code(&http, &client_id, &client_secret, &code, &redirect_uri)
        .await
        .map_err(|e| spotify_error("spotify token exchange failed", e))?;

    let provider_user_id = spotify::get_current_user_id(&http, &tokens.access_token)
        .await
        .map_err(|e| spotify_error("could not read the spotify profile", e))?;

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
    .await
    .map_err(link_error)?;

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
        .response_with::<400, ErrorJson, _>(|r| r.description("Missing/invalid code or state, the user denied access, or Spotify rejected the exchange"))
        .response_with::<409, ErrorJson, _>(|r| r.description("That Spotify account is already linked to another Scrobblr user"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LastfmCallbackQuery {
    pub token: Option<String>,
    pub state: Option<String>,
}

/// GET /v1/connect/lastfm/callback
///
/// Public for the same reason as the Spotify callback: the `state` token
/// identifies the user. The session Last.fm returns names the account the
/// user just authorized, which is what makes an import "verified".
pub async fn lastfm_callback(
    State(state): State<AppState>,
    Query(q): Query<LastfmCallbackQuery>,
) -> ApiResult<impl IntoApiResponse> {
    let csrf_state = q
        .state
        .ok_or_else(|| AppError::BadRequest("missing state".into()))?;
    let token = q
        .token
        .ok_or_else(|| AppError::BadRequest("missing token".into()))?;
    let user_id = take_oauth_state(&state, "lastfm", &csrf_state).await?;

    let session = lastfm_client()?
        .get_session(&token)
        .await
        .map_err(|e| match e {
            e if e.is_transient() => AppError::Internal(anyhow::anyhow!(e)),
            e => AppError::BadRequest(format!("Last.fm rejected the authorization: {e}")),
        })?;

    connected_accounts_db::upsert_connected_account(
        &state.db,
        &connected_accounts_db::UpsertConnectedAccount {
            user_id,
            provider: "lastfm".into(),
            provider_user_id: session.name,
            access_token: session.key,
            refresh_token: None,
            token_type: "session".into(),
            scope: None,
            expires_at: None,
        },
    )
    .await
    .map_err(link_error)?;

    Ok(Json(ConnectResult {
        provider: "lastfm".into(),
        connected: true,
    }))
}

pub fn _lastfm_callback_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Last.fm auth callback")
        .description("Last.fm redirects here after the user grants access. Exchanges the token for a session and links the Last.fm account it names to whichever user started the flow (identified via the `state` CSRF token, not request auth). A linked account is what lets that user import its history.")
        .tag("Connected accounts")
        .response::<200, Json<ConnectResult>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Missing/invalid token or state, or Last.fm rejected the token"))
        .response_with::<409, ErrorJson, _>(|r| r.description("That Last.fm account is already linked to another Scrobblr user"))
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
        .description("Returns the authenticated user's connected third-party accounts. Tokens are not part of this response type at all — the query never selects them.")
        .tag("Connected accounts")
        .response::<200, Json<Vec<shared::models::ConnectedAccountSummary>>>()
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
}

/// DELETE /v1/connect/{provider}
pub async fn disconnect(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(ProviderPath { provider }): Path<ProviderPath>,
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
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<404, ErrorJson, _>(|r| r.description("No connection for this provider"))
}
