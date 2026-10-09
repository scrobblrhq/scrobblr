use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, RawQuery, State},
    http::StatusCode,
    response::Redirect,
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

    // Random single-use CSRF token, mapped to this user. The provider sends
    // the browser back to the web app with it, and the web app finishes the
    // link with the session it holds for that browser (`finish_connect`),
    // which must be this user's.
    let csrf_state = hex::encode(rand::random::<[u8; 16]>());
    let authorize_url = if provider == "lastfm" {
        // Last.fm has no `state` parameter, but keeps the callback's own
        // query string and appends `token` to it.
        let callback = format!(
            "{}/connect/lastfm/callback?state={csrf_state}",
            web_app(&state)?
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
    op.summary("Start linking an account")
        .description("Returns the provider's authorize URL to send the user's browser to: `spotify` (auto-scrobbling) or `lastfm` (proves ownership of the Last.fm account before importing its history). The provider sends the browser back to the web app's `/connect/{provider}/callback` (Spotify: to `SPOTIFY_REDIRECT_URI`), which finishes the link with `POST /v1/connect/{provider}/callback` and the same user's session within 10 minutes.")
        .tag("Connected accounts")
        .response::<200, Json<AuthorizeResponse>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Unsupported provider"))
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<503, ErrorJson, _>(|r| r.description("The provider isn't configured on this server, or (Last.fm) it has no web app to come back to (`WEB_APP_URL`)"))
}

/// Where the browser comes back to finish linking an account.
fn web_app(state: &AppState) -> ApiResult<&str> {
    state.web_app_url.as_deref().ok_or_else(|| {
        AppError::ServiceUnavailable(
            "accounts are linked through the web app, and this server has none (WEB_APP_URL)"
                .into(),
        )
    })
}

/// Burns a callback's CSRF `state`, checking that `user_id`, finishing the
/// link, is who started it: otherwise a page could send someone's browser
/// back with a `state` of the page owner's, linking their account to the
/// owner's.
async fn take_oauth_state(
    state: &AppState,
    provider: &str,
    csrf_state: &str,
    user_id: i64,
) -> ApiResult<()> {
    let invalid = || AppError::BadRequest("invalid or expired oauth state".into());
    if csrf_state.is_empty() || csrf_state.len() > 64 {
        return Err(invalid());
    }
    // Gone once read, so it can't be replayed.
    let started_by: Option<String> = state
        .redis
        .getdel(format!("oauth_state:{provider}:{csrf_state}"))
        .await
        .map_err(AppError::Redis)?;
    let started_by: i64 = started_by
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("corrupt oauth state value in redis")))?;
    if started_by != user_id {
        return Err(AppError::BadRequest(
            "this link was started by another account".into(),
        ));
    }
    Ok(())
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

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConnectResult {
    pub provider: String,
    pub connected: bool,
}

/// GET /v1/connect/{provider}/callback
///
/// Public: the provider sends the browser here, without the session, when
/// the redirect URI it knows is the API's (Spotify apps registered before
/// the web app). Links nothing: the web app's page does.
pub async fn forward_callback(
    State(state): State<AppState>,
    Path(ProviderPath { provider }): Path<ProviderPath>,
    RawQuery(query): RawQuery,
) -> ApiResult<Redirect> {
    ensure_supported_provider(&provider)?;
    Ok(Redirect::to(&format!(
        "{}/connect/{provider}/callback?{}",
        web_app(&state)?,
        query.unwrap_or_default()
    )))
}

pub fn _forward_callback_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Forward a provider's callback to the web app")
        .description("For a provider whose redirect URI is this API's (`SPOTIFY_REDIRECT_URI` pointing here): sends the browser on to `{WEB_APP_URL}/connect/{provider}/callback` with the provider's query, where the web app finishes the link with `POST /v1/connect/{provider}/callback`. Links nothing itself.")
        .tag("Connected accounts")
        .response_with::<303, (), _>(|r| r.description("To `{WEB_APP_URL}/connect/{provider}/callback?{query}`"))
        .response_with::<400, ErrorJson, _>(|r| r.description("Unsupported provider"))
        .response_with::<503, ErrorJson, _>(|r| r.description("This server has no web app (`WEB_APP_URL`)"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FinishConnectRequest {
    /// The `state` the provider sent back.
    pub state: String,
    /// Spotify's `code`.
    pub code: Option<String>,
    /// Last.fm's `token`.
    pub token: Option<String>,
}

/// POST /v1/connect/{provider}/callback
pub async fn finish_connect(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(ProviderPath { provider }): Path<ProviderPath>,
    Json(body): Json<FinishConnectRequest>,
) -> ApiResult<Json<ConnectResult>> {
    ensure_supported_provider(&provider)?;
    let (name, grant) = match provider.as_str() {
        "lastfm" => ("token", body.token),
        _ => ("code", body.code),
    };
    let grant = grant
        .filter(|g| !g.trim().is_empty())
        .ok_or_else(|| AppError::BadRequest(format!("missing {name}")))?;
    take_oauth_state(&state, &provider, &body.state, auth_user.id).await?;

    if provider == "lastfm" {
        link_lastfm(&state, auth_user.id, &grant).await?;
    } else {
        link_spotify(&state, auth_user.id, &grant).await?;
    }
    Ok(Json(ConnectResult {
        provider,
        connected: true,
    }))
}

pub fn _finish_connect_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Finish linking an account")
        .description("For the web app's page the provider sends the browser back to (`/connect/{provider}/callback`): the `state` from its query, and Spotify's `code` or Last.fm's `token`. The session must be that of the user who started the link with `GET /v1/connect/{provider}`, so a page can't have someone else's browser link their account to another user's. A linked Spotify account is polled for scrobbles; a linked Last.fm account is what lets the user import its history.")
        .tag("Connected accounts")
        .response::<200, Json<ConnectResult>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("A missing `code` or `token`; a `state` that's unknown, expired, used, or another user's; or the provider rejected the grant"))
        .response_with::<409, ErrorJson, _>(|r| r.description("That account is already linked to another Scrobblr user"))
        .response_with::<503, ErrorJson, _>(|r| r.description("The provider isn't configured on this server"))
}

/// Trades Spotify's authorization code for tokens and links the account.
async fn link_spotify(state: &AppState, user_id: i64, code: &str) -> ApiResult<()> {
    let client_id = spotify_client_id()?;
    let client_secret = spotify_client_secret()?;
    let redirect_uri = spotify_redirect_uri()?;

    let http = reqwest::Client::new();
    let tokens = spotify::exchange_code(&http, &client_id, &client_secret, code, &redirect_uri)
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
    Ok(())
}

/// Trades the token Last.fm appended to the callback for a session, which
/// names the account the user just authorized, and links it: what makes an
/// import "verified".
async fn link_lastfm(state: &AppState, user_id: i64, token: &str) -> ApiResult<()> {
    let session = lastfm_client()?
        .get_session(token)
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
    Ok(())
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

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode, header};
    use fred::interfaces::KeysInterface;
    use fred::types::Expiration;
    use serde_json::json;

    use crate::state::AppState;
    use crate::test_app::with_custom_app;

    /// A page can send someone's browser back with a `state` of the page
    /// owner's, from a link they started: that must link nothing.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn only_the_user_who_started_a_link_finishes_it() {
        let web = |state: &mut AppState| state.web_app_url = Some("https://web.test".into());
        with_custom_app(web, |app| async move {
            let key = |csrf_state: &str| format!("oauth_state:spotify:{csrf_state}");
            let started_by = async |user_id: i64| {
                let csrf_state = hex::encode(rand::random::<[u8; 16]>());
                let _: () = app
                    .redis
                    .set(
                        key(&csrf_state),
                        user_id.to_string(),
                        Some(Expiration::EX(600)),
                        None,
                        false,
                    )
                    .await
                    .unwrap();
                csrf_state
            };
            let finish = async |body| {
                let path = "/v1/connect/spotify/callback";
                let (status, body) = app.api(Method::POST, path, Some(body)).await;
                (
                    status,
                    body["error"].as_str().unwrap_or_default().to_string(),
                )
            };

            let theirs = started_by(app.user_id + 1).await;
            let attempt = json!({ "state": theirs, "code": "granted" });
            let (status, error) = finish(attempt.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(
                error,
                "bad request: this link was started by another account"
            );
            // And it's spent.
            let (_, error) = finish(attempt).await;
            assert_eq!(error, "bad request: invalid or expired oauth state");

            let ours = started_by(app.user_id).await;
            let (status, error) = finish(json!({ "state": ours })).await;
            assert_eq!(
                (status, error.as_str()),
                (StatusCode::BAD_REQUEST, "bad request: missing code")
            );
            // Past the state, to Spotify: unconfigured in tests, or refusing
            // a made-up code.
            let (_, error) = finish(json!({ "state": ours, "code": "granted" })).await;
            assert!(
                !error.contains("state") && !error.contains("another account"),
                "{error}"
            );
            let left: Option<String> = app.redis.get(key(&ours)).await.unwrap();
            assert_eq!(left, None);

            // A provider that knows the API's URL sends the browser there.
            let (status, headers, _) = app
                .get("/v1/connect/spotify/callback?code=granted&state=abc")
                .await;
            assert_eq!(status, StatusCode::SEE_OTHER);
            assert_eq!(
                headers[header::LOCATION],
                "https://web.test/connect/spotify/callback?code=granted&state=abc"
            );
            let (status, _, _) = app.get("/v1/connect/deezer/callback?code=x").await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        })
        .await;
    }
}
