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
use db::queries::users as users_db;
use shared::models::UserProfile;

#[derive(Debug, Serialize, JsonSchema)]
pub struct ProfileResponse {
    #[serde(flatten)]
    pub profile: UserProfile,
    pub is_following: Option<bool>, // None if not authenticated
}

/// GET /v1/user/:username
pub async fn get_profile(
    State(state): State<AppState>,
    Path(username): Path<String>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    let is_following =
        crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    Ok(Json(ProfileResponse {
        profile: user.into(),
        is_following,
    }))
}

pub fn _get_profile_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get user profile")
        .description("Returns the public profile for a user. If the viewer is authenticated, also includes `is_following`. Private profiles return 403 to non-owners.")
        .tag("Users")
        .response::<200, Json<ProfileResponse>>()
        .response_with::<403, (), _>(|r| r.description("Profile is private"))
        .response_with::<404, (), _>(|r| r.description("User not found"))
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FriendsResponse {
    pub followers: Vec<UserProfile>,
    pub following: Vec<UserProfile>,
}

/// GET /v1/user/me
pub async fn get_own_profile(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
) -> ApiResult<impl IntoApiResponse> {
    let user = users_db::find_by_id(&state.db, auth_user.id)
        .await?
        .ok_or(AppError::NotFound)?;

    Ok(Json(ProfileResponse {
        profile: user.into(),
        is_following: None, // you can't follow yourself
    }))
}

pub fn _get_own_profile_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get my own profile")
        .description("Alias for fetching the authenticated user's own profile, regardless of privacy settings.")
        .tag("Users")
        .response::<200, Json<ProfileResponse>>()
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

/// Omitted fields are left unchanged; sending an empty string clears the
/// field (JSON gives no way to tell `null` from absent here).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateSettingsRequest {
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub image_url: Option<String>,
    pub is_private: Option<bool>,
}

/// Maps "field present" to the DB patch semantics: empty/whitespace clears
/// the column, anything else sets the trimmed value.
fn patch_field(value: &Option<String>) -> Option<Option<&str>> {
    value.as_ref().map(|s| {
        let trimmed = s.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    })
}

/// PATCH /v1/user/me
pub async fn update_settings(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<UpdateSettingsRequest>,
) -> ApiResult<impl IntoApiResponse> {
    if let Some(name) = &body.display_name
        && name.trim().chars().count() > 100
    {
        return Err(AppError::BadRequest(
            "display name must be at most 100 characters".into(),
        ));
    }
    if let Some(bio) = &body.bio
        && bio.trim().chars().count() > 1000
    {
        return Err(AppError::BadRequest(
            "bio must be at most 1000 characters".into(),
        ));
    }
    if let Some(url) = &body.image_url {
        let url = url.trim();
        if !url.is_empty() {
            if !url.starts_with("https://") && !url.starts_with("http://") {
                return Err(AppError::BadRequest(
                    "image_url must be an http(s) URL".into(),
                ));
            }
            // Only the avatar-upload endpoint may set an /uploads/ URL, and it
            // always points at a file that user just created. Rejecting them
            // here stops a user aiming image_url at someone else's uploaded
            // file — which their next avatar upload's cleanup would delete.
            let uploads_prefix = format!("{}/uploads/", state.uploads.public_base_url);
            if url.starts_with(&uploads_prefix) {
                return Err(AppError::BadRequest(
                    "image_url cannot reference an uploaded file; use the avatar upload endpoint"
                        .into(),
                ));
            }
        }
    }

    let user = users_db::update_profile(
        &state.db,
        auth_user.id,
        &users_db::UpdateProfile {
            display_name: patch_field(&body.display_name),
            bio: patch_field(&body.bio),
            image_url: patch_field(&body.image_url),
            is_private: body.is_private,
        },
    )
    .await?;
    Ok(Json(UserProfile::from(user)))
}

pub fn _update_settings_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Update account settings")
        .description("Updates the authenticated user's own profile: display name, bio, avatar URL and privacy. Omitted fields are unchanged; an empty string clears the field.")
        .tag("Users")
        .response::<200, Json<UserProfile>>()
        .response_with::<400, (), _>(|r| r.description("Invalid field value"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

/// GET /v1/user/:username/friends
pub async fn get_friends(
    State(state): State<AppState>,
    Path(username): Path<String>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    let followers = users_db::get_followers(&state.db, user.id)
        .await?
        .into_iter()
        .map(UserProfile::from)
        .collect();

    let following = users_db::get_following(&state.db, user.id)
        .await?
        .into_iter()
        .map(UserProfile::from)
        .collect();

    Ok(Json(FriendsResponse {
        followers,
        following,
    }))
}

pub fn _get_friends_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get followers and following")
        .description("Returns both the follower list and the following list for a given user.")
        .tag("Users")
        .response::<200, Json<FriendsResponse>>()
        .response_with::<403, (), _>(|r| r.description("Profile is private"))
        .response_with::<404, (), _>(|r| r.description("User not found"))
}

/// POST /v1/user/:username/follow
pub async fn follow(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(username): Path<String>,
) -> ApiResult<StatusCode> {
    let target = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    if target.id == auth_user.id {
        return Err(AppError::BadRequest("cannot follow yourself".into()));
    }

    users_db::follow_user(&state.db, auth_user.id, target.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn _follow_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Follow a user")
        .description("Follows the specified user on behalf of the authenticated user. Returns 400 if attempting to follow yourself.")
        .tag("Users")
        .response_with::<204, (), _>(|r| r.description("Successfully followed"))
        .response_with::<400, (), _>(|r| r.description("Cannot follow yourself"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Target user not found"))
}

/// DELETE /v1/user/:username/follow
pub async fn unfollow(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(username): Path<String>,
) -> ApiResult<StatusCode> {
    let target = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    if target.id == auth_user.id {
        return Err(AppError::BadRequest("cannot unfollow yourself".into()));
    }

    users_db::unfollow_user(&state.db, auth_user.id, target.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn _unfollow_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Unfollow a user")
        .description("Unfollows the specified user on behalf of the authenticated user.")
        .tag("Users")
        .response_with::<204, (), _>(|r| r.description("Successfully unfollowed"))
        .response_with::<400, (), _>(|r| r.description("Cannot unfollow yourself"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
        .response_with::<404, (), _>(|r| r.description("Target user not found"))
}
