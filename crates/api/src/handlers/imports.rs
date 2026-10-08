use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use chrono::{TimeDelta, Utc};
use schemars::JsonSchema;
use serde::Deserialize;

use super::IdPath;
use crate::{
    errors::{ApiResult, AppError},
    handlers::connected_accounts::lastfm_client,
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::connected_accounts as connected_accounts_db;
use db::queries::imports::{self as imports_db, CreateImportError, NewImport};
use shared::models::ScrobbleImport;

/// One user-started import a day: each re-reads at least two weeks of
/// history from Last.fm on our API key.
const IMPORT_COOLDOWN: TimeDelta = TimeDelta::hours(24);

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct StartImportRequest {
    /// Rescan the whole history instead of only what's new since the last
    /// completed import.
    #[serde(default)]
    pub full: bool,
}

/// POST /v1/import/lastfm
pub async fn start_lastfm_import(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    body: Option<Json<StartImportRequest>>,
) -> ApiResult<impl IntoApiResponse> {
    let request = body.map(|Json(b)| b).unwrap_or_default();
    lastfm_client()?;

    let account = connected_accounts_db::list_connected_accounts(&state.db, auth_user.id)
        .await?
        .into_iter()
        .find(|a| a.provider == imports_db::PROVIDER_LASTFM)
        .ok_or_else(|| {
            AppError::BadRequest(
                "connect your Last.fm account first (GET /v1/connect/lastfm)".into(),
            )
        })?;

    if let Some(finished) = imports_db::last_verified_finish(&state.db, auth_user.id).await?
        && Utc::now() - finished < IMPORT_COOLDOWN
    {
        return Err(AppError::RateLimited);
    }

    let window_from = if request.full {
        None
    } else {
        imports_db::reimport_from(
            &state.db,
            auth_user.id,
            imports_db::PROVIDER_LASTFM,
            &account.provider_user_id,
        )
        .await?
    };
    let import = imports_db::create(
        &state.db,
        &NewImport {
            user_id: auth_user.id,
            provider: imports_db::PROVIDER_LASTFM,
            external_user: &account.provider_user_id,
            verified: true,
            window_from,
        },
    )
    .await
    .map_err(|e| match e {
        CreateImportError::AlreadyActive(id) => {
            AppError::Conflict(format!("import #{id} is still running"))
        }
        CreateImportError::Db(e) => AppError::Database(e),
    })?;

    Ok((StatusCode::ACCEPTED, Json(import)))
}

pub fn _start_lastfm_import_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Import Last.fm history")
        .description("Queues an import of the scrobble history of the Last.fm account linked through `GET /v1/connect/lastfm` (linking proves the user owns it). The worker runs it in the background; poll `GET /v1/imports/{id}` for progress. Imports are resumable and deduplicated: by default only scrobbles since the last completed import (minus two weeks, for late scrobbles) are fetched, and `full: true` rescans everything without creating duplicates. Scrobbles already sent to Scrobblr live are not imported twice. One import per user at a time, and one a day.")
        .tag("Imports")
        .response::<202, Json<ScrobbleImport>>()
        .response_with::<400, (), _>(|r| r.description("No Last.fm account connected"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<409, (), _>(|r| r.description("An import is already running"))
        .response_with::<429, (), _>(|r| r.description("An import finished less than a day ago"))
        .response_with::<503, (), _>(|r| r.description("Last.fm isn't configured on this server"))
}

/// GET /v1/imports
pub async fn list_imports(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
) -> ApiResult<impl IntoApiResponse> {
    let imports: Vec<ScrobbleImport> = imports_db::list(&state.db, Some(auth_user.id), 20)
        .await?
        .into_iter()
        .map(|(_, _, import)| import)
        .collect();
    Ok(Json(imports))
}

pub fn _list_imports_doc(op: TransformOperation) -> TransformOperation {
    op.summary("List imports")
        .description(
            "The authenticated user's 20 most recent history imports, newest first, with progress.",
        )
        .tag("Imports")
        .response::<200, Json<Vec<ScrobbleImport>>>()
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

/// GET /v1/imports/{id}
pub async fn get_import(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<impl IntoApiResponse> {
    let import = imports_db::get(&state.db, id, Some(auth_user.id))
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(import))
}

pub fn _get_import_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get an import")
        .description("Progress of one of the authenticated user's imports: scrobbles read against Last.fm's total, how many were new, duplicates and invalid entries skipped, how far back in time it has reached, and why it failed if it did.")
        .tag("Imports")
        .response::<200, Json<ScrobbleImport>>()
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("No such import for this user"))
}

/// DELETE /v1/imports/{id}
pub async fn cancel_import(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<impl IntoApiResponse> {
    if imports_db::cancel(&state.db, id, Some(auth_user.id)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

pub fn _cancel_import_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Cancel an import")
        .description("Stops a pending or running import after the page in flight. Scrobbles already imported stay.")
        .tag("Imports")
        .response_with::<204, (), _>(|r| r.description("Cancelled"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("No running import with this id for this user"))
}
