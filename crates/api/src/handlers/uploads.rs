//! User image uploads: avatars and custom artist/album artwork
//! (last.fm-style community art). Multipart endpoints shared by the mobile
//! app and the upcoming website.
//!
//! Every upload is decoded (rejecting non-images), downscaled (avatars to
//! 512 px, artwork to 1024 px) and re-encoded as JPEG by `media::image`,
//! then written under `UPLOAD_DIR` and served back from `/uploads/{file}`.
//! Artist/album images set `image_locked` so the enrichment worker never
//! overwrites community art.

use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Multipart, Path, State, multipart::MultipartError},
    http::StatusCode,
};
use uuid::Uuid;

use super::IdPath;
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    media,
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::{community as community_db, tracks as tracks_db, users as users_db};
use shared::models::{ImageCandidate, UserProfile};

/// Body limit of the upload routes (router.rs).
pub const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;
const AVATAR_SIDE: u32 = 512;
const ARTWORK_SIDE: u32 = 1024;

/// Reads the first file field out of the multipart body.
async fn read_image_field(multipart: &mut Multipart) -> ApiResult<Vec<u8>> {
    let rejection = |e: MultipartError| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::PayloadTooLarge(format!(
                "images can be at most {} MiB",
                MAX_UPLOAD_BYTES / 1024 / 1024
            ))
        } else {
            AppError::BadRequest(format!("invalid multipart body: {}", e.body_text()))
        }
    };
    while let Some(field) = multipart.next_field().await.map_err(rejection)? {
        // Accept the conventional field name plus anything carrying a file.
        if field.name() == Some("image") || field.file_name().is_some() {
            let bytes = field.bytes().await.map_err(rejection)?;
            if bytes.is_empty() {
                return Err(AppError::BadRequest("uploaded file is empty".into()));
            }
            return Ok(bytes.to_vec());
        }
    }
    Err(AppError::BadRequest(
        "multipart body must contain an `image` file field".into(),
    ))
}

/// Stores normalized bytes and returns the public URL.
async fn store_image(state: &AppState, bytes: &[u8]) -> ApiResult<String> {
    let file_name = format!("{}.jpg", Uuid::new_v4());
    let path = state.uploads.dir.join(&file_name);
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("could not store upload: {e}")))?;
    Ok(format!(
        "{}/uploads/{file_name}",
        state.uploads.public_base_url
    ))
}

/// Best-effort removal of a previously uploaded file when it is replaced.
async fn delete_if_owned(state: &AppState, old_url: Option<&str>) {
    let Some(old_url) = old_url else { return };
    let Some((_, file_name)) = old_url.split_once("/uploads/") else {
        return; // external URL (enrichment providers) — not ours to delete
    };
    // Guard against traversal; stored names are always `{uuid}.jpg`.
    if file_name.contains(['/', '\\']) || file_name.contains("..") {
        return;
    }
    let _ = tokio::fs::remove_file(state.uploads.dir.join(file_name)).await;
}

async fn process_upload(
    state: &AppState,
    multipart: &mut Multipart,
    max_side: u32,
) -> ApiResult<String> {
    let raw = read_image_field(multipart).await?;
    let normalized = media::normalize(raw, max_side).await?;
    store_image(state, &normalized).await
}

/// POST /v1/user/me/avatar
pub async fn upload_avatar(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    mut multipart: Multipart,
) -> ApiResult<impl IntoApiResponse> {
    let url = process_upload(&state, &mut multipart, AVATAR_SIDE).await?;

    let previous = users_db::find_by_id(&state.db, auth_user.id)
        .await?
        .and_then(|u| u.image_url);

    let user = users_db::update_profile(
        &state.db,
        auth_user.id,
        &users_db::UpdateProfile {
            image_url: Some(Some(&url)),
            ..Default::default()
        },
    )
    .await?;

    delete_if_owned(&state, previous.as_deref()).await;
    Ok(Json(UserProfile::from(user)))
}

/// The multipart body every upload takes, which aide can't describe.
fn image_body(mut op: TransformOperation) -> TransformOperation {
    op.inner_mut().request_body = Some(aide::openapi::ReferenceOr::Item(
        serde_json::from_value(serde_json::json!({
            "required": true,
            "content": { "multipart/form-data": { "schema": {
                "type": "object",
                "required": ["image"],
                "properties": { "image": {
                    "type": "string",
                    "contentMediaType": "application/octet-stream",
                    "description": "JPEG, PNG or WebP, at most 8 MiB, 12000 px a side and 40 megapixels"
                } }
            } } }
        }))
        .expect("valid request body"),
    ));
    op
}

pub fn _upload_avatar_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Upload avatar")
        .description("Sets the authenticated user's avatar from a multipart `image` file field (JPEG/PNG/WebP, re-encoded server-side as a JPEG of at most 512 px a side, max 8 MiB). Returns the updated profile.")
        .tag("Users")
        .response::<200, Json<UserProfile>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Not a valid image, or too many pixels"))
        .response_with::<413, ErrorJson, _>(|r| r.description("Larger than 8 MiB"))
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .with(image_body)
}

/// Adds a candidate for an artist/album and returns the refreshed candidate
/// list. Uploading only *proposes* an image; it becomes the displayed one
/// only once it wins the community vote (see `community` queries).
async fn add_candidate(
    state: &AppState,
    entity_type: &str,
    entity_id: i64,
    uploader: i64,
    multipart: &mut Multipart,
) -> ApiResult<Vec<ImageCandidate>> {
    let url = process_upload(state, multipart, ARTWORK_SIDE).await?;
    community_db::add_image_candidate(&state.db, entity_type, entity_id, &url, uploader).await?;
    // The uploader's auto-like may already push a first candidate to the
    // threshold on a tiny community.
    community_db::promote_winning_image(&state.db, entity_type, entity_id).await?;
    let candidates =
        community_db::list_image_candidates(&state.db, entity_type, entity_id, Some(uploader))
            .await?;
    Ok(candidates)
}

/// POST /v1/artist/{id}/image
pub async fn upload_artist_image(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(IdPath { id }): Path<IdPath>,
    mut multipart: Multipart,
) -> ApiResult<impl IntoApiResponse> {
    tracks_db::find_artist_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let candidates = add_candidate(&state, "artist", id, auth_user.id, &mut multipart).await?;
    Ok(Json(candidates))
}

pub fn _upload_artist_image_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Propose an artist image")
        .description("Adds a community image candidate for the artist (multipart `image` field). Uploads never replace the current image directly — the most-liked candidate becomes the displayed image once it clears the vote threshold. Returns the refreshed candidate list.")
        .tag("Catalog")
        .response::<200, Json<Vec<ImageCandidate>>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Not a valid image, or too many pixels"))
        .response_with::<413, ErrorJson, _>(|r| r.description("Larger than 8 MiB"))
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Artist not found"))
        .with(image_body)
}

/// POST /v1/album/{id}/image
pub async fn upload_album_image(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(IdPath { id }): Path<IdPath>,
    mut multipart: Multipart,
) -> ApiResult<impl IntoApiResponse> {
    tracks_db::find_album_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let candidates = add_candidate(&state, "album", id, auth_user.id, &mut multipart).await?;
    Ok(Json(candidates))
}

pub fn _upload_album_image_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Propose an album cover")
        .description("Adds a community cover candidate for the album (multipart `image` field). Uploads never replace the current cover directly — the most-liked candidate becomes the displayed cover once it clears the vote threshold. Returns the refreshed candidate list.")
        .tag("Catalog")
        .response::<200, Json<Vec<ImageCandidate>>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Not a valid image, or too many pixels"))
        .response_with::<413, ErrorJson, _>(|r| r.description("Larger than 8 MiB"))
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Album not found"))
        .with(image_body)
}
