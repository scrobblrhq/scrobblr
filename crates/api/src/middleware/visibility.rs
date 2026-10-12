use crate::{
    errors::{ApiResult, AppError},
    state::AppState,
};
use db::queries::users as users_db;
use shared::models::User;

/// Checks whether `viewer_id` is allowed to see `profile_user`'s data,
/// returning the follow relationship as a side effect since most callers
/// need it for their response anyway. A private profile is its owner's
/// alone: following takes no approval, so a follower is anyone who asked.
/// Errors with `Forbidden` for someone else's private profile, and with
/// `NotFound` for an account waiting to be erased.
pub async fn ensure_profile_visible(
    state: &AppState,
    viewer_id: Option<i64>,
    profile_user: &User,
) -> ApiResult<Option<bool>> {
    if profile_user.delete_after.is_some() {
        return Err(AppError::NotFound);
    }
    let is_owner = viewer_id == Some(profile_user.id);
    if profile_user.is_private && !is_owner {
        return Err(AppError::Forbidden);
    }
    match viewer_id {
        Some(vid) if !is_owner => Ok(Some(
            users_db::is_following(&state.db, vid, profile_user.id).await?,
        )),
        _ => Ok(None),
    }
}
