//! Community endpoints: image-candidate voting and comments.
//!
//! Image uploads live in `handlers::uploads`; this module lists candidates,
//! records votes (which can promote a candidate to the entity's displayed
//! image), and handles comments on artists and tracks.

use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    errors::{ApiResult, AppError},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::{community as community_db, tracks as tracks_db};
use shared::models::{Comment, ImageCandidate};

const COMMENT_MAX_LEN: usize = 2000;
const COMMENTS_PAGE: i64 = 100;

async fn ensure_image_entity(state: &AppState, entity_type: &str, id: i64) -> ApiResult<()> {
    let exists = match entity_type {
        "artist" => tracks_db::find_artist_by_id(&state.db, id).await?.is_some(),
        "album" => tracks_db::find_album_by_id(&state.db, id).await?.is_some(),
        _ => false,
    };
    exists.then_some(()).ok_or(AppError::NotFound)
}

/// GET /v1/{artist,album}/{id}/images
async fn list_images(
    state: &AppState,
    entity_type: &str,
    id: i64,
    viewer: Option<i64>,
) -> ApiResult<Json<Vec<ImageCandidate>>> {
    ensure_image_entity(state, entity_type, id).await?;
    let candidates =
        community_db::list_image_candidates(&state.db, entity_type, id, viewer).await?;
    Ok(Json(candidates))
}

pub async fn list_artist_images(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    list_images(&state, "artist", id, auth_user.map(|Extension(a)| a.id)).await
}

pub fn _list_artist_images_doc(op: TransformOperation) -> TransformOperation {
    op.summary("List artist image candidates")
        .description("Returns the community image candidates for an artist, most-liked first. `has_voted` reflects the authenticated viewer; `is_default` marks the currently displayed image.")
        .tag("Catalog")
        .response::<200, Json<Vec<ImageCandidate>>>()
        .response_with::<404, (), _>(|r| r.description("Artist not found"))
}

pub async fn list_album_images(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    list_images(&state, "album", id, auth_user.map(|Extension(a)| a.id)).await
}

pub fn _list_album_images_doc(op: TransformOperation) -> TransformOperation {
    op.summary("List album cover candidates")
        .description("Returns the community cover candidates for an album, most-liked first.")
        .tag("Catalog")
        .response::<200, Json<Vec<ImageCandidate>>>()
        .response_with::<404, (), _>(|r| r.description("Album not found"))
}

/// POST /v1/image/{candidate_id}/vote
pub async fn vote_image(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(candidate_id): Path<i64>,
) -> ApiResult<impl IntoApiResponse> {
    let (entity_type, entity_id) = community_db::candidate_entity(&state.db, candidate_id)
        .await?
        .ok_or(AppError::NotFound)?;
    community_db::vote_image(&state.db, candidate_id, auth_user.id).await?;
    community_db::promote_winning_image(&state.db, &entity_type, entity_id).await?;
    let candidates =
        community_db::list_image_candidates(&state.db, &entity_type, entity_id, Some(auth_user.id))
            .await?;
    Ok(Json(candidates))
}

pub fn _vote_image_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Like an image candidate")
        .description("Records the authenticated user's like for a candidate (idempotent). If it becomes the most-liked candidate past the threshold, it is promoted to the entity's displayed image. Returns the refreshed candidate list.")
        .tag("Catalog")
        .response::<200, Json<Vec<ImageCandidate>>>()
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Candidate not found"))
}

/// DELETE /v1/image/{candidate_id}/vote
pub async fn unvote_image(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(candidate_id): Path<i64>,
) -> ApiResult<impl IntoApiResponse> {
    let (entity_type, entity_id) = community_db::candidate_entity(&state.db, candidate_id)
        .await?
        .ok_or(AppError::NotFound)?;
    community_db::unvote_image(&state.db, candidate_id, auth_user.id).await?;
    // Promotion only — a withdrawn like never demotes the displayed image.
    let candidates =
        community_db::list_image_candidates(&state.db, &entity_type, entity_id, Some(auth_user.id))
            .await?;
    Ok(Json(candidates))
}

pub fn _unvote_image_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Remove a like from an image candidate")
        .description("Withdraws the authenticated user's like (idempotent). The currently displayed image is never reverted by this. Returns the refreshed candidate list.")
        .tag("Catalog")
        .response::<200, Json<Vec<ImageCandidate>>>()
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Candidate not found"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NewCommentRequest {
    pub body: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DeletedResponse {
    pub deleted: bool,
}

async fn list_comments(
    state: &AppState,
    entity_type: &str,
    id: i64,
) -> ApiResult<Json<Vec<Comment>>> {
    ensure_comment_entity(state, entity_type, id).await?;
    let comments = community_db::list_comments(&state.db, entity_type, id, COMMENTS_PAGE).await?;
    Ok(Json(comments))
}

async fn ensure_comment_entity(state: &AppState, entity_type: &str, id: i64) -> ApiResult<()> {
    let exists = match entity_type {
        "artist" => tracks_db::find_artist_by_id(&state.db, id).await?.is_some(),
        "track" => tracks_db::find_track_by_id(&state.db, id).await?.is_some(),
        _ => false,
    };
    exists.then_some(()).ok_or(AppError::NotFound)
}

async fn create_comment(
    state: &AppState,
    entity_type: &str,
    id: i64,
    user_id: i64,
    body: String,
) -> ApiResult<(StatusCode, Json<Comment>)> {
    ensure_comment_entity(state, entity_type, id).await?;
    let body = body.trim();
    if body.is_empty() {
        return Err(AppError::BadRequest("comment cannot be empty".into()));
    }
    if body.chars().count() > COMMENT_MAX_LEN {
        return Err(AppError::BadRequest(format!(
            "comment must be at most {COMMENT_MAX_LEN} characters"
        )));
    }
    let comment = community_db::add_comment(&state.db, entity_type, id, user_id, body).await?;
    Ok((StatusCode::CREATED, Json(comment)))
}

pub async fn list_artist_comments(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoApiResponse> {
    list_comments(&state, "artist", id).await
}

pub fn _list_artist_comments_doc(op: TransformOperation) -> TransformOperation {
    op.summary("List artist comments")
        .description("Returns comments on an artist, newest first.")
        .tag("Catalog")
        .response::<200, Json<Vec<Comment>>>()
        .response_with::<404, (), _>(|r| r.description("Artist not found"))
}

pub async fn add_artist_comment(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(id): Path<i64>,
    Json(body): Json<NewCommentRequest>,
) -> ApiResult<impl IntoApiResponse> {
    create_comment(&state, "artist", id, auth_user.id, body.body).await
}

pub fn _add_artist_comment_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Comment on an artist")
        .description("Posts a comment on an artist as the authenticated user.")
        .tag("Catalog")
        .response::<201, Json<Comment>>()
        .response_with::<400, (), _>(|r| r.description("Empty or too-long comment"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Artist not found"))
}

pub async fn list_track_comments(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoApiResponse> {
    list_comments(&state, "track", id).await
}

pub fn _list_track_comments_doc(op: TransformOperation) -> TransformOperation {
    op.summary("List track comments")
        .description("Returns comments on a track, newest first.")
        .tag("Catalog")
        .response::<200, Json<Vec<Comment>>>()
        .response_with::<404, (), _>(|r| r.description("Track not found"))
}

pub async fn add_track_comment(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(id): Path<i64>,
    Json(body): Json<NewCommentRequest>,
) -> ApiResult<impl IntoApiResponse> {
    create_comment(&state, "track", id, auth_user.id, body.body).await
}

pub fn _add_track_comment_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Comment on a track")
        .description("Posts a comment on a track as the authenticated user.")
        .tag("Catalog")
        .response::<201, Json<Comment>>()
        .response_with::<400, (), _>(|r| r.description("Empty or too-long comment"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Track not found"))
}

/// DELETE /v1/comments/{id}
pub async fn delete_comment(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(id): Path<i64>,
) -> ApiResult<impl IntoApiResponse> {
    let deleted = community_db::delete_comment(&state.db, id, auth_user.id).await?;
    if !deleted {
        // Either the comment doesn't exist or it isn't the caller's.
        return Err(AppError::NotFound);
    }
    Ok(Json(DeletedResponse { deleted }))
}

pub fn _delete_comment_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Delete a comment")
        .description("Deletes one of the authenticated user's own comments.")
        .tag("Catalog")
        .response::<200, Json<DeletedResponse>>()
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Comment not found or not owned"))
}
