//! User image uploads: avatars and custom artist/album artwork
//! (last.fm-style community art). Multipart endpoints shared by the mobile
//! app and the upcoming website.
//!
//! Every upload is decoded (rejecting non-images), downscaled to at most
//! 1024px and re-encoded as JPEG — which also strips EXIF metadata — then
//! written under `UPLOAD_DIR` and served back from `/uploads/{file}`.
//! Artist/album images set `image_locked` so the enrichment worker never
//! overwrites community art.

use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Multipart, Path, State},
};
use uuid::Uuid;

use crate::{
    errors::{ApiResult, AppError},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::{community as community_db, tracks as tracks_db, users as users_db};
use shared::models::{ImageCandidate, UserProfile};

const MAX_DIMENSION: u32 = 1024;
/// Hard cap on decoded source dimensions; anything larger is rejected before
/// decode to bound memory (guards against decompression bombs).
const MAX_SOURCE_DIMENSION: u32 = 12_000;
const JPEG_QUALITY: u8 = 85;

/// Reads the first file field out of the multipart body.
async fn read_image_field(multipart: &mut Multipart) -> ApiResult<Vec<u8>> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("invalid multipart body: {e}")))?
    {
        // Accept the conventional field name plus anything carrying a file.
        if field.name() == Some("image") || field.file_name().is_some() {
            let bytes = field
                .bytes()
                .await
                .map_err(|e| AppError::BadRequest(format!("could not read upload: {e}")))?;
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

/// Decode → clamp to [`MAX_DIMENSION`] → JPEG. CPU-bound, so it runs on the
/// blocking pool. Rejects anything that doesn't decode as an image.
async fn normalize_image(bytes: Vec<u8>) -> ApiResult<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        // Cap decoded dimensions before decoding: a small compressed file can
        // otherwise expand to a huge bitmap (decompression bomb) and OOM the
        // server, since the 8 MiB body limit only bounds the *compressed* input.
        let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|_| AppError::BadRequest("could not read image".into()))?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
        limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
        reader.limits(limits);

        let decoded = reader
            .decode()
            .map_err(|_| AppError::BadRequest("file is not a supported image".into()))?;

        let resized = if decoded.width() > MAX_DIMENSION || decoded.height() > MAX_DIMENSION {
            decoded.thumbnail(MAX_DIMENSION, MAX_DIMENSION)
        } else {
            decoded
        };

        let mut out = std::io::Cursor::new(Vec::new());
        let rgb = image::DynamicImage::ImageRgb8(resized.into_rgb8());
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)
            .encode_image(&rgb)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("jpeg encode failed: {e}")))?;
        Ok(out.into_inner())
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("image task panicked: {e}")))?
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

async fn process_upload(state: &AppState, multipart: &mut Multipart) -> ApiResult<String> {
    let raw = read_image_field(multipart).await?;
    let normalized = normalize_image(raw).await?;
    store_image(state, &normalized).await
}

/// POST /v1/user/me/avatar
pub async fn upload_avatar(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    mut multipart: Multipart,
) -> ApiResult<impl IntoApiResponse> {
    let url = process_upload(&state, &mut multipart).await?;

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

pub fn _upload_avatar_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Upload avatar")
        .description("Sets the authenticated user's avatar from a multipart `image` file field (JPEG/PNG/WebP, re-encoded server-side, max 8 MiB). Returns the updated profile.")
        .tag("Users")
        .response::<200, Json<UserProfile>>()
        .response_with::<400, (), _>(|r| r.description("Not a valid image"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
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
    let url = process_upload(state, multipart).await?;
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
    Path(id): Path<i64>,
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
        .response_with::<400, (), _>(|r| r.description("Not a valid image"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Artist not found"))
}

/// POST /v1/album/{id}/image
pub async fn upload_album_image(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(id): Path<i64>,
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
        .response_with::<400, (), _>(|r| r.description("Not a valid image"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Album not found"))
}
