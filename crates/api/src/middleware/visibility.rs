use crate::{
    errors::{ApiResult, AppError},
    state::AppState,
};
use db::queries::users as users_db;
use shared::models::User;

/// Checks whether `viewer_id` is allowed to see `profile_user`'s data,
/// returning the follow relationship as a side effect since most callers
/// need it for their response anyway. Errors with `Forbidden` if the
/// profile is private and the viewer is neither the owner nor a follower.
pub async fn ensure_profile_visible(
    state: &AppState,
    viewer_id: Option<i64>,
    profile_user: &User,
) -> ApiResult<Option<bool>> {
    let is_owner = viewer_id == Some(profile_user.id);
    let is_following = match viewer_id {
        Some(vid) if vid != profile_user.id => {
            Some(users_db::is_following(&state.db, vid, profile_user.id).await?)
        }
        _ => None,
    };

    if profile_user.is_private && !is_owner && !is_following.unwrap_or(false) {
        return Err(AppError::Forbidden);
    }

    Ok(is_following)
}
