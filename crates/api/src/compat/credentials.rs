//! What the web app needs for scrobblers: the user's scrobbler tokens and
//! Last.fm sessions (list, create, revoke), and the browser authorization
//! a Last.fm-API client sends the user to (`/api/auth/`). Managing them
//! takes a session (`Access::Session` in `router.rs`), never an API token.

use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, RawQuery, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::{TimeDelta, Utc};
use schemars::JsonSchema;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    errors::{ApiResult, AppError},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::scrobblers::{self as scrobblers_db, KIND_TOKEN};
use shared::models::{
    CreatedScrobblerToken, ScrobblerAuthorization, ScrobblerAuthorizationRedirect,
    ScrobblerCredential,
};

const NAME_MAX_CHARS: usize = 100;
const CALLBACK_MAX_LEN: usize = 2048;
const WEB_FLOW_TOKEN_TTL: TimeDelta = TimeDelta::minutes(10);

/// GET /api/auth/?api_key=…&token=… (desktop flow) or ?api_key=…&cb=… (web
/// flow), where Last.fm-API clients send the user's browser. The approval
/// page is the web app's; the query goes along unchanged.
pub async fn browser_authorization(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Response {
    match &state.compat.web_app_url {
        Some(web) => Redirect::to(&format!(
            "{web}/scrobbler/authorize?{}",
            query.unwrap_or_default()
        ))
        .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Html(
                "<!doctype html><meta charset=\"utf-8\"><title>Scrobblr</title>\
                 <p>This server isn't set up for signing in from the browser. In your \
                 player, sign in with your Scrobblr username and a scrobbler token as the \
                 password instead.</p>",
            ),
        )
            .into_response(),
    }
}

/// GET /v1/scrobbler/credentials
pub async fn list_credentials(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
) -> ApiResult<Json<Vec<ScrobblerCredential>>> {
    Ok(Json(
        scrobblers_db::list_credentials(&state.db, auth_user.id).await?,
    ))
}

pub fn _list_credentials_doc(op: TransformOperation) -> TransformOperation {
    op.summary("List scrobbler credentials")
        .description("The tokens the user made for third-party scrobblers and the Last.fm sessions clients obtained with them or by logging in. Secrets are never returned.")
        .tag("Scrobblers")
        .response::<200, Json<Vec<ScrobblerCredential>>>()
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateScrobblerTokenRequest {
    /// Which client it's for, e.g. "Pano Scrobbler on my phone".
    pub name: String,
}

/// POST /v1/scrobbler/tokens
pub async fn create_token(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<CreateScrobblerTokenRequest>,
) -> ApiResult<(StatusCode, Json<CreatedScrobblerToken>)> {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > NAME_MAX_CHARS {
        return Err(AppError::BadRequest(format!(
            "name must be 1 to {NAME_MAX_CHARS} characters"
        )));
    }
    let (token, credential) =
        super::issue_credential(&state, auth_user.id, KIND_TOKEN, name, None).await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedScrobblerToken { credential, token }),
    ))
}

pub fn _create_token_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Create a scrobbler token")
        .description("A secret to paste into a third-party scrobbler: the user token of a ListenBrainz client, the password of an Audioscrobbler 1.2 client, or the password of a Last.fm-API client (instead of the account password). It can scrobble and nothing else, and is shown only in this response. `legacy_auth` is false when the server has no TOKEN_ENCRYPTION_KEY, which Audioscrobbler 1.2 needs.")
        .tag("Scrobblers")
        .response::<201, Json<CreatedScrobblerToken>>()
        .response_with::<400, (), _>(|r| r.description("Missing or overlong name"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

/// DELETE /v1/scrobbler/credentials/{id}
pub async fn delete_credential(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    if scrobblers_db::delete_credential(&state.db, id, auth_user.id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

pub fn _delete_credential_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Revoke a scrobbler credential")
        .description("Clients using it are signed out at once, including Audioscrobbler 1.2 sessions it opened.")
        .tag("Scrobblers")
        .response_with::<204, (), _>(|r| r.description("Revoked"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("No such credential of this user"))
}

/// GET /v1/scrobbler/authorizations/{token}
pub async fn get_authorization(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Json<ScrobblerAuthorization>> {
    let a = scrobblers_db::get_authorization(&state.db, &token)
        .await?
        .ok_or(AppError::NotFound)?;
    let status = if a.expires_at <= Utc::now() {
        "expired"
    } else if a.user_id.is_some() {
        "approved"
    } else {
        "pending"
    };
    Ok(Json(ScrobblerAuthorization {
        api_key: a.api_key,
        status: status.into(),
        expires_at: a.expires_at,
    }))
}

pub fn _get_authorization_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Show a scrobbler authorization request")
        .description("For the page a Last.fm-API client sends the user to (`/api/auth/?api_key=…&token=…`, forwarded to the web app's `/scrobbler/authorize`): which client asks, and whether the request is still pending.")
        .tag("Scrobblers")
        .response::<200, Json<ScrobblerAuthorization>>()
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Unknown token"))
}

/// POST /v1/scrobbler/authorizations/{token}/approve
pub async fn approve_authorization(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(token): Path<String>,
) -> ApiResult<StatusCode> {
    if scrobblers_db::approve_authorization(&state.db, &token, auth_user.id).await? {
        return Ok(StatusCode::NO_CONTENT);
    }
    match scrobblers_db::get_authorization(&state.db, &token).await? {
        None => Err(AppError::NotFound),
        Some(a) if a.expires_at <= Utc::now() => {
            Err(AppError::BadRequest("this request has expired".into()))
        }
        Some(a) if a.user_id == Some(auth_user.id) => Ok(StatusCode::NO_CONTENT),
        Some(_) => Err(AppError::Conflict(
            "another user approved this request".into(),
        )),
    }
}

pub fn _approve_authorization_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Approve a scrobbler authorization request")
        .description("Lets the Last.fm-API client that holds the token scrobble for the user: its next `auth.getSession` gets a session key. Desktop flow.")
        .tag("Scrobblers")
        .response_with::<204, (), _>(|r| r.description("Approved"))
        .response_with::<400, (), _>(|r| r.description("The request expired (they last an hour)"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Unknown token"))
        .response_with::<409, (), _>(|r| r.description("Approved by another user already"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuthorizeCallbackRequest {
    pub api_key: String,
    /// The client's `cb` parameter: where the browser goes with the token.
    pub callback: String,
}

/// POST /v1/scrobbler/authorizations
pub async fn authorize_callback(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<AuthorizeCallbackRequest>,
) -> ApiResult<Json<ScrobblerAuthorizationRedirect>> {
    let api_key = body.api_key.trim();
    if api_key.is_empty() || api_key.len() > 64 {
        return Err(AppError::BadRequest("invalid api_key".into()));
    }
    let callback = body.callback.trim();
    let valid = callback.len() <= CALLBACK_MAX_LEN
        && reqwest::Url::parse(callback).is_ok_and(|u| matches!(u.scheme(), "http" | "https"));
    if !valid {
        return Err(AppError::BadRequest(
            "callback must be an http(s) URL".into(),
        ));
    }
    let token = super::new_secret();
    scrobblers_db::create_authorization(
        &state.db,
        &token,
        api_key,
        Some(auth_user.id),
        WEB_FLOW_TOKEN_TTL,
    )
    .await?;
    let separator = if callback.contains('?') { '&' } else { '?' };
    Ok(Json(ScrobblerAuthorizationRedirect {
        redirect_url: format!("{callback}{separator}token={token}"),
        token,
    }))
}

pub fn _authorize_callback_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Authorize a web-flow scrobbler")
        .description("For a Last.fm-API client that sent the user with a callback (`/api/auth/?api_key=…&cb=…`): approves it at once and returns the callback URL with the token, which the client trades for a session key with `auth.getSession` within 10 minutes.")
        .tag("Scrobblers")
        .response::<200, Json<ScrobblerAuthorizationRedirect>>()
        .response_with::<400, (), _>(|r| r.description("Invalid api_key or callback"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}
