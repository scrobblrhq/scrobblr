use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::UsernamePath;
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    limits,
    middleware::{auth::AuthUser, rate_limit::ClientIp},
    state::AppState,
};
use db::queries::users::{self as users_db, FollowList, Followed};
use shared::cursor::Cursor;
use shared::models::{FollowEntry, FollowPage, ScrobbleBreakdown, UserProfile};

#[derive(Debug, Serialize, JsonSchema)]
pub struct ProfileResponse {
    #[serde(flatten)]
    pub profile: UserProfile,
    /// Whether the signed-in viewer follows this account; `null` when
    /// signed out, and on the viewer's own profile.
    pub is_following: Option<bool>,
    /// Whether this account follows the signed-in viewer; `null` likewise.
    pub follows_you: Option<bool>,
    /// Accounts following this one, private ones included (lists don't
    /// name them).
    pub followers_count: i64,
    /// Accounts this one follows, private ones included.
    pub following_count: i64,
    /// `scrobble_count` (duplicates left out) by what the classifier made
    /// of it.
    pub scrobble_breakdown: ScrobbleBreakdown,
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
    let follows_you = match viewer_id {
        Some(viewer) if viewer != user.id => {
            Some(users_db::is_following(&state.db, user.id, viewer).await?)
        }
        _ => None,
    };
    let (followers_count, following_count) = users_db::follow_counts(&state.db, user.id).await?;
    let scrobble_breakdown = users_db::scrobble_breakdown(&state.db, user.id).await?;

    Ok(Json(ProfileResponse {
        profile: user.into(),
        is_following,
        follows_you,
        followers_count,
        following_count,
        scrobble_breakdown,
    }))
}

pub fn _get_profile_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get user profile")
        .description("Returns the public profile for a user, with how many accounts follow it and it follows (`followers_count`, `following_count`: every follow, private accounts' included). A signed-in viewer also gets `is_following` (the viewer follows this account) and `follows_you` (it follows the viewer); both are `null` otherwise. `scrobble_count` leaves out the scrobbles the classifier found to be the same listen reported again (duplicates); `scrobble_breakdown` divides it into verified plays and unverified ones (over the listening time real time allows, of tracks with no known length, or not classified yet). Private profiles return 403 to everyone but their owner, followers included (following takes no approval).")
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
    let (followers_count, following_count) = users_db::follow_counts(&state.db, user.id).await?;
    let scrobble_breakdown = users_db::scrobble_breakdown(&state.db, user.id).await?;

    Ok(Json(ProfileResponse {
        profile: user.into(),
        is_following: None,
        follows_you: None,
        followers_count,
        following_count,
        scrobble_breakdown,
    }))
}

pub fn _get_own_profile_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get my own profile")
        .description("Alias for fetching the authenticated user's own profile, regardless of privacy settings, with the same counts and `scrobble_breakdown`; `is_following` and `follows_you` are `null`.")
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

/// How many of each list `/friends` returns.
const FRIENDS_LIMIT: i64 = 100;

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

    let list = async |list| -> ApiResult<Vec<UserProfile>> {
        Ok(
            users_db::follow_page(&state.db, user.id, list, viewer_id, None, FRIENDS_LIMIT)
                .await?
                .into_iter()
                .map(|f| UserProfile::from(f.user))
                .collect(),
        )
    };
    Ok(Json(FriendsResponse {
        followers: list(FollowList::Followers).await?,
        following: list(FollowList::Following).await?,
    }))
}

pub fn _get_friends_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get followers and following")
        .description(&format!("Returns the {FRIENDS_LIMIT} most recent followers and the {FRIENDS_LIMIT} most recent accounts followed, newest follow first; `GET /v1/user/{{username}}/followers` and `/following` page through all of them. Private accounts are left out of both, except the viewer's own, and so are accounts waiting to be deleted. Private profiles return 403 to everyone but their owner."))
        .tag("Users")
        .response::<200, Json<FriendsResponse>>()
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| r.description("User not found"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FollowListQuery {
    /// Entries per page: 50 by default, 200 at most.
    pub limit: Option<i64>,
    /// The previous page's `next`, for the page after it.
    pub before: Option<String>,
}

/// GET /v1/user/:username/followers
pub async fn followers(
    state: State<AppState>,
    path: Path<UsernamePath>,
    query: Query<FollowListQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<FollowPage>> {
    follow_list(state, path, query, auth_user, FollowList::Followers).await
}

pub fn _followers_doc(op: TransformOperation) -> TransformOperation {
    follow_list_doc(op, "Followers", "the accounts following the user")
}

/// GET /v1/user/:username/following
pub async fn following(
    state: State<AppState>,
    path: Path<UsernamePath>,
    query: Query<FollowListQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<FollowPage>> {
    follow_list(state, path, query, auth_user, FollowList::Following).await
}

pub fn _following_doc(op: TransformOperation) -> TransformOperation {
    follow_list_doc(op, "Following", "the accounts the user follows")
}

async fn follow_list(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    Query(q): Query<FollowListQuery>,
    auth_user: Option<Extension<AuthUser>>,
    list: FollowList,
) -> ApiResult<Json<FollowPage>> {
    let before = q
        .before
        .as_deref()
        .map(|c| {
            Cursor::parse(c).ok_or_else(|| AppError::BadRequest("invalid `before` cursor".into()))
        })
        .transpose()?;
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;
    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    let limit = super::page_limit(q.limit, 50, 200);
    let mut page =
        users_db::follow_page(&state.db, user.id, list, viewer_id, before, limit + 1).await?;
    let next = if page.len() as i64 > limit {
        page.truncate(limit as usize);
        page.last().map(|f| {
            Cursor {
                at: f.followed_at,
                id: f.user.id,
            }
            .encode()
        })
    } else {
        None
    };
    Ok(Json(FollowPage {
        entries: page
            .into_iter()
            .map(|f| FollowEntry {
                user: f.user.into(),
                followed_at: f.followed_at,
                is_following: f.is_following,
                follows_you: f.follows_you,
            })
            .collect(),
        next,
    }))
}

fn follow_list_doc<'t>(
    op: TransformOperation<'t>,
    summary: &str,
    what: &str,
) -> TransformOperation<'t> {
    op.summary(summary)
        .description(&format!("A page of {what}, newest follow first: pass `next` as `before` for the following page; `next` is `null` on the last. Private accounts are left out, except the viewer's own, and so are accounts waiting to be deleted, so a page can hold fewer entries than `limit` while more follow. A signed-in viewer gets `is_following` and `follows_you` for each account. Private profiles return 403 to everyone but their owner."))
        .tag("Users")
        .response::<200, Json<FollowPage>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Invalid `before` cursor"))
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| r.description("User not found"))
}

/// The most accounts one may follow: a feed of more is noise, and a cap
/// bounds what a follow farm's accounts cost the server.
pub const MAX_FOLLOWING: i64 = 2000;

/// POST /v1/user/:username/follow
pub async fn follow(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(UsernamePath { username }): Path<UsernamePath>,
) -> ApiResult<StatusCode> {
    if !limits::follow_attempt(&state, auth_user.id, &ip).await {
        return Err(AppError::RateLimited);
    }
    let target = users_db::find_by_username(&state.db, &username)
        .await?
        .filter(|u| u.delete_after.is_none())
        .ok_or(AppError::NotFound)?;

    if target.id == auth_user.id {
        return Err(AppError::BadRequest("cannot follow yourself".into()));
    }

    match users_db::follow_user(&state.db, auth_user.id, target.id, MAX_FOLLOWING).await? {
        Followed::Now | Followed::Already => Ok(StatusCode::NO_CONTENT),
        Followed::AtLimit => Err(AppError::Conflict(format!(
            "you already follow {MAX_FOLLOWING} accounts, the most allowed"
        ))),
    }
}

pub fn _follow_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Follow a user")
        .description(&format!("Follows the specified user on behalf of the authenticated user; following an account already followed changes nothing. Following takes no approval, and opens nothing a private profile hides: its owner alone sees it. An account may follow {MAX_FOLLOWING} accounts at most, and follow {} times an hour ({} from one address), refused attempts included; unfollowing is free.", limits::FOLLOWS_PER_USER, limits::FOLLOWS_PER_IP))
        .tag("Users")
        .response_with::<204, (), _>(|r| r.description("Followed, now or before"))
        .response_with::<400, ErrorJson, _>(|r| r.description("Cannot follow yourself"))
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Target user not found"))
        .response_with::<409, ErrorJson, _>(|r| {
            r.description(&format!("Already following {MAX_FOLLOWING} accounts"))
        })
        .response_with::<429, ErrorJson, _>(|r| {
            r.description(&format!(
                "Over {} follows an hour for this account or {} from this address, or over the request limit for this address",
                limits::FOLLOWS_PER_USER,
                limits::FOLLOWS_PER_IP
            ))
        })
}

/// DELETE /v1/user/:username/follow
pub async fn unfollow(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Path(UsernamePath { username }): Path<UsernamePath>,
) -> ApiResult<StatusCode> {
    let target = users_db::find_by_username(&state.db, &username)
        .await?
        .filter(|u| u.delete_after.is_none())
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

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::json;

    use crate::test_app::{with_app, with_custom_app};

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn follow_lists_page_newest_first_with_counts_and_mutual_follows() {
        with_app(Default::default(), |app| async move {
            // Seven followers, a second apart; one private, one leaving.
            let mut fans = Vec::new();
            for n in 0..7 {
                let (id, name, session) = app.other_user(&format!("fan{n}")).await;
                sqlx::query(
                    "INSERT INTO user_follows (follower_id, followee_id, created_at)
                     VALUES ($1, $2, NOW() - make_interval(secs => $3))",
                )
                .bind(id)
                .bind(app.user_id)
                .bind(100 - n)
                .execute(&app.pool)
                .await
                .unwrap();
                fans.push((id, name, session));
            }
            sqlx::query("UPDATE users SET is_private = TRUE WHERE id = $1")
                .bind(fans[2].0)
                .execute(&app.pool)
                .await
                .unwrap();
            sqlx::query("UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE id = $1")
                .bind(fans[3].0)
                .execute(&app.pool)
                .await
                .unwrap();
            app.api(
                Method::POST,
                &format!("/v1/user/{}/follow", fans[6].1),
                None,
            )
            .await;

            let profile = format!("/v1/user/{}", app.username);
            let (_, body) = app.api_as(&fans[6].2, Method::GET, &profile, None).await;
            assert_eq!(
                body["followers_count"], 7,
                "private and leaving ones counted"
            );
            assert_eq!(body["following_count"], 1);
            assert_eq!(
                (&body["is_following"], &body["follows_you"]),
                (&json!(true), &json!(true))
            );
            let (_, body) = app.api_as(&fans[0].2, Method::GET, &profile, None).await;
            assert_eq!(
                (&body["is_following"], &body["follows_you"]),
                (&json!(true), &json!(false))
            );
            let body: serde_json::Value = serde_json::from_str(&app.get(&profile).await.2).unwrap();
            assert_eq!(
                (&body["is_following"], &body["follows_you"]),
                (&json!(null), &json!(null))
            );

            // Pages of two, newest first, without the private or leaving
            // fan; the viewer's own entry has no relation to itself.
            let mut seen = Vec::new();
            let mut path = format!("{profile}/followers?limit=2");
            loop {
                let (status, page) = app.api_as(&fans[6].2, Method::GET, &path, None).await;
                assert_eq!(status, StatusCode::OK, "{page}");
                for entry in page["entries"].as_array().unwrap() {
                    seen.push(entry["username"].as_str().unwrap().to_owned());
                    if entry["username"] == fans[6].1.as_str() {
                        assert_eq!(entry["is_following"], json!(null));
                    } else {
                        assert_eq!(entry["is_following"], json!(false));
                        assert_eq!(entry["follows_you"], json!(false));
                    }
                }
                match page["next"].as_str() {
                    Some(next) => path = format!("{profile}/followers?limit=2&before={next}"),
                    None => break,
                }
            }
            let expected: Vec<String> =
                [6, 5, 4, 1, 0].iter().map(|&n| fans[n].1.clone()).collect();
            assert_eq!(seen, expected);
            // The private fan sees itself.
            let (_, page) = app
                .api_as(
                    &fans[2].2,
                    Method::GET,
                    &format!("{profile}/followers?limit=200"),
                    None,
                )
                .await;
            assert_eq!(page["entries"].as_array().unwrap().len(), 6);

            let (_, page) = app
                .api(Method::GET, &format!("{profile}/following"), None)
                .await;
            assert_eq!(page["entries"][0]["username"], fans[6].1.as_str());
            assert_eq!(page["entries"][0]["follows_you"], json!(true));
            assert_eq!(page["next"], json!(null));

            let (status, _) = app
                .api(
                    Method::GET,
                    &format!("{profile}/followers?before=nonsense"),
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (status, page) = app
                .api(Method::GET, &format!("{profile}/followers?limit=0"), None)
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["entries"].as_array().unwrap().len(), 1);
            let gone = format!("/v1/user/{}/followers", fans[3].1);
            assert_eq!(app.get(&gone).await.0, StatusCode::NOT_FOUND);
            let private = format!("/v1/user/{}/following", fans[2].1);
            assert_eq!(app.get(&private).await.0, StatusCode::FORBIDDEN);
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn follows_are_capped_and_rate_limited() {
        let unlimited = |state: &mut crate::state::AppState| {
            state.rate_limit = crate::middleware::rate_limit::RateLimit {
                requests: 0,
                window_secs: 60,
            }
        };
        with_custom_app(unlimited, |app| async move {
            // One short of the cap, followed directly.
            let base = format!("c{}", &app.username[4..]);
            sqlx::query(
                "WITH new AS (
                     INSERT INTO users (username, email, password_hash)
                     SELECT $1 || n, $1 || n || '@test', 'x' FROM generate_series(1, $2) n
                     RETURNING id)
                 INSERT INTO user_follows (follower_id, followee_id) SELECT $3, id FROM new",
            )
            .bind(&base)
            .bind((super::MAX_FOLLOWING - 1) as i32)
            .bind(app.user_id)
            .execute(&app.pool)
            .await
            .unwrap();
            let (_, a, _) = app.other_user("a").await;
            let (_, b, _) = app.other_user("b").await;
            let follow = |name: &str| format!("/v1/user/{name}/follow");
            assert_eq!(
                app.api(Method::POST, &follow(&a), None).await.0,
                StatusCode::NO_CONTENT
            );
            let (status, body) = app.api(Method::POST, &follow(&b), None).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            // Following again is no new follow; unfollowing makes room.
            assert_eq!(
                app.api(Method::POST, &follow(&a), None).await.0,
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                app.api(Method::DELETE, &follow(&a), None).await.0,
                StatusCode::NO_CONTENT
            );
            assert_eq!(
                app.api(Method::POST, &follow(&b), None).await.0,
                StatusCode::NO_CONTENT
            );

            // Four attempts so far; the hour's allowance ends at 100,
            // refused ones included, and unfollows don't count.
            for _ in 4..crate::limits::FOLLOWS_PER_USER {
                app.api(Method::DELETE, &follow(&b), None).await;
                let (status, _) = app.api(Method::POST, &follow(&b), None).await;
                assert_eq!(status, StatusCode::NO_CONTENT);
            }
            let (status, _) = app.api(Method::POST, &follow(&a), None).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
            let (status, _) = app.api(Method::DELETE, &follow(&b), None).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn a_private_profile_is_its_owners_alone() {
        with_app(Default::default(), |app| async move {
            let (_, follower, follower_session) = app.other_user("follower").await;
            let (_, public, _) = app.other_user("public").await;
            let follow = format!("/v1/user/{}/follow", app.username);
            let (status, _) = app
                .api_as(&follower_session, Method::POST, &follow, None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            app.api(Method::POST, &format!("/v1/user/{public}/follow"), None)
                .await;
            let (status, _) = app
                .api(
                    Method::PATCH,
                    "/v1/user/me",
                    Some(json!({ "is_private": true })),
                )
                .await;
            assert_eq!(status, StatusCode::OK);

            // Following took no approval, so it opens nothing.
            let profile = format!("/v1/user/{}", app.username);
            for path in [
                profile.clone(),
                format!("{profile}/friends"),
                format!("{profile}/recent"),
                format!("{profile}/top-artists"),
            ] {
                let (status, _) = app
                    .api_as(&follower_session, Method::GET, &path, None)
                    .await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
                assert_eq!(app.get(&path).await.0, StatusCode::FORBIDDEN, "{path}");
                assert_eq!(
                    app.api(Method::GET, &path, None).await.0,
                    StatusCode::OK,
                    "{path}"
                );
            }

            // Nor is the private account named in anyone's follow lists, but
            // to itself.
            let lists = format!("/v1/user/{public}/friends");
            let (_, friends) = app
                .api_as(&follower_session, Method::GET, &lists, None)
                .await;
            assert_eq!(friends["followers"], json!([]));
            let (_, friends) = app.api(Method::GET, &lists, None).await;
            assert_eq!(friends["followers"][0]["username"], app.username.as_str());
            let (_, friends) = app
                .api_as(
                    &follower_session,
                    Method::GET,
                    &format!("/v1/user/{follower}/friends"),
                    None,
                )
                .await;
            assert_eq!(friends["following"], json!([]));
        })
        .await;
    }
}
