use crate::{
    errors::{ApiResult, AppError},
    state::AppState,
};
use db::queries::blocks::{self as blocks_db, Relation};
use shared::models::User;

/// Checks whether `viewer_id` is allowed to see `profile_user`'s data,
/// returning how the two stand (`None` signed out or on the viewer's own
/// profile), since some callers answer with it. A private profile is its
/// owner's alone: following takes no approval, so a follower is anyone who
/// asked. Errors with `Forbidden` for someone else's private profile, and
/// with `NotFound` for an account waiting to be erased or blocking the
/// viewer, as for one that doesn't exist.
pub async fn ensure_profile_visible(
    state: &AppState,
    viewer_id: Option<i64>,
    profile_user: &User,
) -> ApiResult<Option<Relation>> {
    if profile_user.delete_after.is_some() {
        return Err(AppError::NotFound);
    }
    let relation = match viewer_id {
        Some(viewer) if viewer != profile_user.id => {
            Some(blocks_db::relation(&state.db, viewer, profile_user.id).await?)
        }
        _ => None,
    };
    if relation.is_some_and(|r| r.blocked_by) {
        return Err(AppError::NotFound);
    }
    if profile_user.is_private && viewer_id != Some(profile_user.id) {
        return Err(AppError::Forbidden);
    }
    Ok(relation)
}
