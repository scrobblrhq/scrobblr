//! What the web app needs for scrobblers: the user's scrobbler tokens
//! (list, create, revoke). Managing them takes the `write` scope, so a
//! session, not a scrobble token.

use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use schemars::JsonSchema;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    errors::{ApiResult, AppError},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::scrobblers::{self as scrobblers_db, KIND_TOKEN};
use shared::models::{CreatedScrobblerToken, ScrobblerCredential};

const NAME_MAX_CHARS: usize = 100;

fn require_write(user: &AuthUser) -> ApiResult<()> {
    if user.scopes.iter().any(|s| s == "write") {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// GET /v1/scrobbler/credentials
pub async fn list_credentials(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
) -> ApiResult<Json<Vec<ScrobblerCredential>>> {
    require_write(&auth_user)?;
    Ok(Json(
        scrobblers_db::list_credentials(&state.db, auth_user.id).await?,
    ))
}

pub fn _list_credentials_doc(op: TransformOperation) -> TransformOperation {
    op.summary("List scrobbler credentials")
        .description(
            "The tokens the user made for third-party scrobblers. Secrets are never returned.",
        )
        .tag("Scrobblers")
        .response::<200, Json<Vec<ScrobblerCredential>>>()
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<403, (), _>(|r| r.description("Needs a session (the `write` scope)"))
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
    require_write(&auth_user)?;
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
        .response_with::<403, (), _>(|r| r.description("Needs a session (the `write` scope)"))
}

/// DELETE /v1/scrobbler/credentials/{id}
pub async fn delete_credential(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_write(&auth_user)?;
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
        .response_with::<403, (), _>(|r| r.description("Needs a session (the `write` scope)"))
        .response_with::<404, (), _>(|r| r.description("No such credential of this user"))
}
