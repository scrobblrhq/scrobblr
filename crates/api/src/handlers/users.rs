use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::UsernamePath;
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
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
    Path(UsernamePath { username }): Path<UsernamePath>,
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
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| r.description("User not found"))
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
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
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
    let display_name = body
        .display_name
        .as_deref()
        .map(|name| shared::validation::sanitize_display_name(Some(name)))
        .transpose()?;
    if let Some(bio) = &body.bio
        && bio.trim().chars().count() > 1000
    {
        return Err(AppError::BadRequest(
            "bio must be at most 1000 characters".into(),
        ));
    }
    let mut image_url = patch_field(&body.image_url);
    if let Some(Some(url)) = image_url {
        let upload = is_upload_url(&state, url);
        if upload || !url.starts_with("https://") {
            // Clients send back the avatar URL they were given, which keeps
            // it, even an upload or an http URL from before this rule.
            // Otherwise uploads stay the upload endpoint's to set, so the
            // database never holds a URL of ours (nor a key, which isn't
            // https either).
            let current = users_db::find_by_id(&state.db, auth_user.id)
                .await?
                .and_then(|u| u.image_url);
            if current.as_deref().map(shared::media::public_url).as_deref() != Some(url) {
                return Err(AppError::BadRequest(if upload {
                    "image_url can't point at an uploaded image; use the avatar upload".into()
                } else {
                    "image_url must be an https URL".into()
                }));
            }
            image_url = None;
        }
    }

    let (user, previous_image) = users_db::update_profile(
        &state.db,
        auth_user.id,
        &users_db::UpdateProfile {
            display_name: display_name.as_ref().map(|name| name.as_deref()),
            bio: patch_field(&body.bio),
            image_url,
            is_private: body.is_private,
        },
    )
    .await?;
    state
        .media
        .delete_replaced_avatar(previous_image.as_deref(), user.image_url.as_deref())
        .await;
    Ok(Json(UserProfile::from(user)))
}

/// Whether `url` is under the uploads' public base or the API's own route.
fn is_upload_url(state: &AppState, url: &str) -> bool {
    let under = |base: &str| {
        url.strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    under(shared::media::public_base()) || under(&format!("{}/uploads", state.public_base_url))
}

pub fn _update_settings_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Update account settings")
        .description("Updates the authenticated user's own profile: display name (at most 40 characters, the registration rules), bio (1000), avatar URL and privacy. Omitted fields are unchanged; an empty string clears the field. The avatar URL is any https URL except an uploaded image's, which only `POST /v1/user/me/avatar` sets; sending back the current avatar's URL, whatever it is, leaves it as it is. A replaced uploaded avatar is deleted.")
        .tag("Users")
        .response::<200, Json<UserProfile>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Invalid field value"))
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
}

/// GET /v1/user/:username/friends
pub async fn get_friends(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
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
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| r.description("User not found"))
}

/// POST /v1/user/:username/follow
pub async fn follow(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(UsernamePath { username }): Path<UsernamePath>,
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
        .response_with::<400, ErrorJson, _>(|r| r.description("Cannot follow yourself"))
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Target user not found"))
}

/// DELETE /v1/user/:username/follow
pub async fn unfollow(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(UsernamePath { username }): Path<UsernamePath>,
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
        .response_with::<400, ErrorJson, _>(|r| r.description("Cannot unfollow yourself"))
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Target user not found"))
}
