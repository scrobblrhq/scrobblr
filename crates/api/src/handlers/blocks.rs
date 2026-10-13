use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use chrono::TimeDelta;
use fred::interfaces::PubsubInterface;

use super::UsernamePath;
use super::users::FollowListQuery;
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    limits,
    middleware::{auth::AuthUser, rate_limit::ClientIp},
    state::AppState,
};
use db::queries::blocks::{self as blocks_db, Blocked};
use db::queries::users as users_db;
use shared::cursor::Cursor;
use shared::models::{BlockEntry, BlockPage, User};

/// The most accounts one may block: past a few thousand, a block list is a
/// list of everyone.
pub const MAX_BLOCKS: i64 = 10_000;

/// How long a removed follower can't follow again.
pub const REMOVAL_COOLDOWN: TimeDelta = TimeDelta::days(30);

/// The account `username`, as the user may act on it: one that doesn't
/// exist, waits to be erased or blocks the user is not found alike.
async fn target(state: &AppState, user_id: i64, username: &str) -> ApiResult<User> {
    let target = users_db::find_by_username(&state.db, username)
        .await?
        .filter(|u| u.delete_after.is_none())
        .ok_or(AppError::NotFound)?;
    if target.id != user_id && blocks_db::blocks(&state.db, target.id, user_id).await? {
        return Err(AppError::NotFound);
    }
    Ok(target)
}

/// POST /v1/user/:username/block
pub async fn block(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(UsernamePath { username }): Path<UsernamePath>,
) -> ApiResult<StatusCode> {
    if !limits::block_attempt(&state, auth_user.id, &ip).await {
        return Err(AppError::RateLimited);
    }
    let target = target(&state, auth_user.id, &username).await?;
    if target.id == auth_user.id {
        return Err(AppError::BadRequest("cannot block yourself".into()));
    }
    if blocks_db::blocks(&state.db, auth_user.id, target.id).await? {
        return Ok(StatusCode::NO_CONTENT);
    }
    match blocks_db::block(&state.db, auth_user.id, target.id, MAX_BLOCKS).await? {
        Blocked::Now => {
            // Ends the live streams of the user's now playing the blocked
            // account holds open, on every API instance. Best-effort.
            let closed = state
                .redis
                .publish::<(), _, _>(
                    crate::live::channel(auth_user.id),
                    crate::live::close_for(target.id),
                )
                .await;
            if let Err(e) = closed {
                tracing::warn!("failed to close blocked live streams: {e}");
            }
            Ok(StatusCode::NO_CONTENT)
        }
        Blocked::Already => Ok(StatusCode::NO_CONTENT),
        Blocked::AtLimit => Err(AppError::Conflict(format!(
            "you already block {MAX_BLOCKS} accounts, the most allowed"
        ))),
    }
}

pub fn _block_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Block a user")
        .description(&format!("Blocks the account: the follows between the two end, and neither can follow the other while the block stands. The user disappears for the blocked account, which finds no such account wherever it looks while signed in (404 on the user's profile and everything under it, absent from lists of people, search, comments, image candidates, feeds and community charts) and is never told; a profile is public to anyone signed out, so a block hides it from the account, not from the person. The blocked account in turn leaves the user's lists of people, search, comments and image candidates; its profile stays visible to the user, with `blocking: true`. Blocking an account already blocked changes nothing. An account may block {MAX_BLOCKS} accounts at most, and block, unblock or remove followers {} times an hour ({} from one address), refused attempts included.", limits::BLOCK_ACTIONS_PER_USER, limits::BLOCK_ACTIONS_PER_IP))
        .tag("Users")
        .response_with::<204, (), _>(|r| r.description("Blocked, now or before"))
        .response_with::<400, ErrorJson, _>(|r| r.description("Cannot block yourself"))
        .response_with::<404, ErrorJson, _>(|r| {
            r.description("User not found, waiting to be deleted, or blocking the user")
        })
        .response_with::<409, ErrorJson, _>(|r| {
            r.description(&format!("Already blocking {MAX_BLOCKS} accounts"))
        })
        .response_with::<429, ErrorJson, _>(|r| r.description(&block_limits()))
}

fn block_limits() -> String {
    format!(
        "Over {} blocks, unblocks and removed followers an hour for this account or {} from \
         this address, or over the request limit for this address",
        limits::BLOCK_ACTIONS_PER_USER,
        limits::BLOCK_ACTIONS_PER_IP
    )
}

/// DELETE /v1/user/:username/block
pub async fn unblock(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(UsernamePath { username }): Path<UsernamePath>,
) -> ApiResult<StatusCode> {
    if !limits::block_attempt(&state, auth_user.id, &ip).await {
        return Err(AppError::RateLimited);
    }
    let target = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;
    if blocks_db::unblock(&state.db, auth_user.id, target.id).await? {
        return Ok(StatusCode::NO_CONTENT);
    }
    // Nothing to lift: answer as for any other account the user may see.
    self::target(&state, auth_user.id, &username).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn _unblock_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Unblock a user")
        .description("Lifts the user's block of the account; the follows it ended don't come back. Unblocking an account not blocked changes nothing. Counts against the same hourly limits as blocking.")
        .tag("Users")
        .response_with::<204, (), _>(|r| r.description("Not blocked any more"))
        .response_with::<404, ErrorJson, _>(|r| {
            r.description("User not found, or not blocked and waiting to be deleted or blocking the user")
        })
        .response_with::<429, ErrorJson, _>(|r| r.description(&block_limits()))
}

/// GET /v1/user/me/blocks
pub async fn list_blocks(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Query(q): Query<FollowListQuery>,
) -> ApiResult<Json<BlockPage>> {
    let before = q
        .before
        .as_deref()
        .map(|c| {
            Cursor::parse(c).ok_or_else(|| AppError::BadRequest("invalid `before` cursor".into()))
        })
        .transpose()?;
    let limit = super::page_limit(q.limit, 50, 200);
    let mut page = blocks_db::block_page(&state.db, auth_user.id, before, limit + 1).await?;
    let next = if page.len() as i64 > limit {
        page.truncate(limit as usize);
        page.last().map(|b| {
            Cursor {
                at: b.blocked_at,
                id: b.user.id,
            }
            .encode()
        })
    } else {
        None
    };
    Ok(Json(BlockPage {
        entries: page
            .into_iter()
            .map(|b| BlockEntry {
                user: b.user,
                blocked_at: b.blocked_at,
            })
            .collect(),
        next,
    }))
}

pub fn _list_blocks_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Accounts I block")
        .description("A page of the accounts the user blocks, newest block first: pass `next` as `before` for the following page; `next` is `null` on the last. A private account shows its username only (`display_name` and `image_url` are `null`); accounts waiting to be deleted are left out, so a page can hold fewer entries than `limit` while more follow. Nobody else can see this list, and nobody can see who blocks them.")
        .tag("Users")
        .response::<200, Json<BlockPage>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Invalid `before` cursor"))
}

/// DELETE /v1/user/me/followers/:username
pub async fn remove_follower(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(UsernamePath { username }): Path<UsernamePath>,
) -> ApiResult<StatusCode> {
    if !limits::block_attempt(&state, auth_user.id, &ip).await {
        return Err(AppError::RateLimited);
    }
    let follower = target(&state, auth_user.id, &username).await?;
    if follower.id == auth_user.id {
        return Err(AppError::BadRequest("cannot remove yourself".into()));
    }
    blocks_db::remove_follower(&state.db, auth_user.id, follower.id, REMOVAL_COOLDOWN).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn _remove_follower_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Remove a follower")
        .description(&format!("Ends the account's follow of the user, without blocking it: its profile and the user's stay visible to each other, and it can't follow the user again for {} days (it is told so if it tries). Removing an account that doesn't follow the user changes nothing. Counts against the same hourly limits as blocking.", REMOVAL_COOLDOWN.num_days()))
        .tag("Users")
        .response_with::<204, (), _>(|r| r.description("Not a follower any more"))
        .response_with::<400, ErrorJson, _>(|r| r.description("Cannot remove yourself"))
        .response_with::<404, ErrorJson, _>(|r| {
            r.description("User not found, waiting to be deleted, or blocking the user")
        })
        .response_with::<429, ErrorJson, _>(|r| r.description(&block_limits()))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};

    use crate::test_app::{TestApp, with_app, with_custom_app};

    /// Without the per-address request limit, which a test's many calls
    /// from one address reach.
    fn unlimited(state: &mut crate::state::AppState) {
        state.rate_limit = crate::middleware::rate_limit::RateLimit {
            requests: 0,
            window_secs: 60,
        }
    }

    const ARTIST: &str = "Shared Band";
    const TRACK: &str = "Shared Song";

    /// The usernames in a list (or a page's `entries`, or a search's `users`).
    fn names(body: &Value) -> Vec<String> {
        let list = body
            .get("entries")
            .or_else(|| body.get("users"))
            .unwrap_or(body);
        list.as_array()
            .unwrap_or_else(|| panic!("not a list: {body}"))
            .iter()
            .map(|e| e["username"].as_str().unwrap().to_string())
            .collect()
    }

    async fn get(app: &TestApp, bearer: Option<&str>, path: &str) -> (StatusCode, Value) {
        match bearer {
            Some(bearer) => app.api_as(bearer, Method::GET, path, None).await,
            None => {
                let (status, _, body) = app.get(path).await;
                (status, serde_json::from_str(&body).unwrap_or(Value::Null))
            }
        }
    }

    /// A blocked account watching the blocker's now playing is cut off when
    /// the block is made; one watching signed out isn't (a public profile is
    /// anyone's to see signed out).
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn a_block_ends_the_blocked_accounts_live_streams() {
        use axum::body::Body;
        use axum::http::{Request, header};
        use futures_util::StreamExt;
        use tower::ServiceExt;

        with_app(Default::default(), |app| async move {
            let (_, b_name, b) = app.other_user("blocked").await;
            let open = |bearer: Option<&str>| {
                let mut req = Request::builder().uri(format!("/v1/user/{}/live", app.username));
                if let Some(bearer) = bearer {
                    req = req.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
                }
                let mut req = req.body(Body::empty()).unwrap();
                req.extensions_mut()
                    .insert(axum::extract::ConnectInfo(app.ip));
                app.router.clone().oneshot(req)
            };
            let mut blocked = open(Some(&b)).await.unwrap().into_body().into_data_stream();
            let mut signed_out = open(None).await.unwrap().into_body().into_data_stream();
            for stream in [&mut blocked, &mut signed_out] {
                let first = stream.next().await.unwrap().unwrap();
                assert!(String::from_utf8_lossy(&first).contains("data: null"));
            }

            app.api(Method::POST, &format!("/v1/user/{b_name}/block"), None)
                .await;
            let ended = tokio::time::timeout(std::time::Duration::from_secs(5), blocked.next())
                .await
                .expect("the blocked stream wasn't closed");
            assert!(ended.is_none(), "{ended:?}");

            let play = json!({ "track": TRACK, "artist": ARTIST, "duration_ms": 200_000 });
            let (status, _) = app.api(Method::POST, "/v1/now-playing", Some(play)).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            let next = tokio::time::timeout(std::time::Duration::from_secs(5), signed_out.next())
                .await
                .expect("no update for the signed-out stream")
                .unwrap()
                .unwrap();
            assert!(String::from_utf8_lossy(&next).contains(TRACK));
        })
        .await;
    }

    /// The block list is the blocker's: paged newest first, a private
    /// account named only, an account waiting to be erased left out, but
    /// its block kept and liftable; a deletion and a restore keep blocks.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn blocks_list_newest_first_and_outlast_a_deletion_request() {
        with_app(Default::default(), |app| async move {
            let mut blocked = Vec::new();
            for n in 0..4 {
                let (id, name, session) = app.other_user(&format!("b{n}")).await;
                let (status, _) = app
                    .api(Method::POST, &format!("/v1/user/{name}/block"), None)
                    .await;
                assert_eq!(status, StatusCode::NO_CONTENT);
                sqlx::query(
                    "UPDATE user_blocks SET created_at = NOW() - make_interval(secs => $2)
                     WHERE blocked_id = $1",
                )
                .bind(id)
                .bind(100 - n)
                .execute(&app.pool)
                .await
                .unwrap();
                blocked.push((id, name, session));
            }
            sqlx::query(
                "UPDATE users SET is_private = TRUE, display_name = 'Hidden', image_url = 'https://img.test/x'
                 WHERE id = $1",
            )
            .bind(blocked[1].0)
            .execute(&app.pool)
            .await
            .unwrap();
            sqlx::query("UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE id = $1")
                .bind(blocked[2].0)
                .execute(&app.pool)
                .await
                .unwrap();
            // Blocking again changes nothing.
            let (status, _) = app
                .api(Method::POST, &format!("/v1/user/{}/block", blocked[0].1), None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT);

            let mut seen = Vec::new();
            let mut path = "/v1/user/me/blocks?limit=2".to_string();
            loop {
                let (status, page) = app.api(Method::GET, &path, None).await;
                assert_eq!(status, StatusCode::OK, "{page}");
                for entry in page["entries"].as_array().unwrap() {
                    if entry["username"] == blocked[1].1.as_str() {
                        assert_eq!((&entry["display_name"], &entry["image_url"]), (&json!(null), &json!(null)));
                    }
                    assert!(entry["blocked_at"].is_string());
                    seen.push(entry["username"].as_str().unwrap().to_string());
                }
                match page["next"].as_str() {
                    Some(next) => path = format!("/v1/user/me/blocks?limit=2&before={next}"),
                    None => break,
                }
            }
            let expected: Vec<String> = [3, 1, 0].iter().map(|&n| blocked[n].1.clone()).collect();
            assert_eq!(seen, expected);
            let (status, _) = app.api(Method::GET, "/v1/user/me/blocks?before=nonsense", None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            // Not blocked and waiting to be erased: as missing. Blocked: liftable.
            let (_, pending, _) = app.other_user("pending").await;
            sqlx::query("UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE username = $1")
                .bind(&pending)
                .execute(&app.pool)
                .await
                .unwrap();
            for path in [format!("/v1/user/{pending}/block"), "/v1/user/nobody0here/block".into()] {
                assert_eq!(app.api(Method::POST, &path, None).await.0, StatusCode::NOT_FOUND);
                assert_eq!(app.api(Method::DELETE, &path, None).await.0, StatusCode::NOT_FOUND);
            }
            let (status, _) = app
                .api(Method::DELETE, &format!("/v1/user/{}/block", blocked[2].1), None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            let (status, _) = app
                .api(Method::POST, &format!("/v1/user/{}/block", app.username), None)
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);

            // The blocker asks for deletion, then restores the account by
            // logging in: the blocks held all along.
            let (status, _) = app
                .api(
                    Method::POST,
                    "/v1/user/me/delete",
                    Some(json!({ "password": crate::test_app::PASSWORD, "confirm": app.username })),
                )
                .await;
            assert_eq!(status, StatusCode::ACCEPTED);
            let (status, login) = app
                .api_as(
                    "none",
                    Method::POST,
                    "/v1/auth/login",
                    Some(json!({ "username": app.username, "password": crate::test_app::PASSWORD })),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{login}");
            assert_eq!(login["account_restored"], json!(true));
            let session = login["token"].as_str().unwrap();
            let (_, page) = app.api_as(session, Method::GET, "/v1/user/me/blocks", None).await;
            assert_eq!(names(&page), vec![blocked[3].1.clone(), blocked[1].1.clone(), blocked[0].1.clone()]);
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn blocks_are_capped_and_rate_limited() {
        with_custom_app(unlimited, |app| async move {
            let base = format!("k{}", &app.username[4..]);
            sqlx::query(
                "WITH new AS (
                     INSERT INTO users (username, email, password_hash)
                     SELECT $1 || n, $1 || n || '@test', 'x' FROM generate_series(1, $2) n
                     RETURNING id)
                 INSERT INTO user_blocks (blocker_id, blocked_id) SELECT $3, id FROM new",
            )
            .bind(&base)
            .bind((super::MAX_BLOCKS - 1) as i32)
            .bind(app.user_id)
            .execute(&app.pool)
            .await
            .unwrap();
            let (_, x, _) = app.other_user("x").await;
            let (_, y, _) = app.other_user("y").await;
            let block = |name: &str| format!("/v1/user/{name}/block");
            assert_eq!(
                app.api(Method::POST, &block(&x), None).await.0,
                StatusCode::NO_CONTENT
            );
            let (status, body) = app.api(Method::POST, &block(&y), None).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(
                app.api(Method::DELETE, &block(&x), None).await.0,
                StatusCode::NO_CONTENT
            );

            // Three actions so far; the hour's allowance ends at 200,
            // refused ones included.
            for _ in 3..crate::limits::BLOCK_ACTIONS_PER_USER {
                assert_eq!(
                    app.api(Method::DELETE, &block(&x), None).await.0,
                    StatusCode::NO_CONTENT
                );
            }
            assert_eq!(
                app.api(Method::DELETE, &block(&x), None).await.0,
                StatusCode::TOO_MANY_REQUESTS
            );
            let (status, _) = app
                .api(Method::DELETE, &format!("/v1/user/me/followers/{x}"), None)
                .await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        })
        .await;
    }

    /// A removed follower stops following without being blocked, and can't
    /// follow again until the cooldown is over.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn a_removed_follower_waits_before_following_again() {
        with_app(Default::default(), |app| async move {
            let (fan_id, fan_name, fan) = app.other_user("fan").await;
            let follow = format!("/v1/user/{}/follow", app.username);
            assert_eq!(app.api_as(&fan, Method::POST, &follow, None).await.0, StatusCode::NO_CONTENT);
            let remove = format!("/v1/user/me/followers/{fan_name}");
            assert_eq!(app.api(Method::DELETE, &remove, None).await.0, StatusCode::NO_CONTENT);

            let (_, profile) = get(&app, Some(&fan), &format!("/v1/user/{}", app.username)).await;
            assert_eq!(profile["is_following"], json!(false), "{profile}");
            let (status, body) = app.api_as(&fan, Method::POST, &follow, None).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            // Removing again, or one who doesn't follow, changes nothing.
            assert_eq!(app.api(Method::DELETE, &remove, None).await.0, StatusCode::NO_CONTENT);
            let (_, other, _) = app.other_user("other").await;
            let (status, _) = app
                .api(Method::DELETE, &format!("/v1/user/me/followers/{other}"), None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            let removals: i64 = sqlx::query_scalar("SELECT count(*) FROM follower_removals")
                .fetch_one(&app.pool)
                .await
                .unwrap();
            assert_eq!(removals, 1);

            sqlx::query("UPDATE follower_removals SET until = NOW() - INTERVAL '1 second' WHERE follower_id = $1")
                .bind(fan_id)
                .execute(&app.pool)
                .await
                .unwrap();
            assert_eq!(app.api_as(&fan, Method::POST, &follow, None).await.0, StatusCode::NO_CONTENT);
            assert_eq!(db::queries::blocks::purge_expired_removals(&app.pool).await.unwrap(), 1);
        })
        .await;
    }
}
