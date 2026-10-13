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
    limits::{self, PairAction},
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
    if !limits::pair_attempt(&state, PairAction::Block, auth_user.id, target.id).await {
        return Err(AppError::RateLimited);
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
        .description(&format!("Blocks the account: the follows between the two end, and neither can follow the other while the block stands. The user disappears for the blocked account, which finds no such account wherever it looks while signed in (404 on the user's profile and everything under it, absent from lists of people, search, comments, image candidates, feeds and community charts) and is never told; a profile is public to anyone signed out, so a block hides it from the account, not from the person. The blocked account in turn leaves the user's lists of people, search, comments and image candidates; its profile stays visible to the user, with `blocking: true`. Blocking an account already blocked changes nothing. An account may block {MAX_BLOCKS} accounts at most, and block, unblock or remove followers {} times an hour ({} from one address), refused attempts included, and block the same account {} times a day.", limits::BLOCK_ACTIONS_PER_USER, limits::BLOCK_ACTIONS_PER_IP, limits::BLOCKS_PER_PAIR))
        .tag("Users")
        .response_with::<204, (), _>(|r| r.description("Blocked, now or before"))
        .response_with::<400, ErrorJson, _>(|r| r.description("Cannot block yourself"))
        .response_with::<404, ErrorJson, _>(|r| {
            r.description("User not found, waiting to be deleted, or blocking the user")
        })
        .response_with::<409, ErrorJson, _>(|r| {
            r.description(&format!("Already blocking {MAX_BLOCKS} accounts"))
        })
        .response_with::<429, ErrorJson, _>(|r| r.description(&block_limits(true)))
}

fn block_limits(per_pair: bool) -> String {
    let pair = if per_pair {
        format!(", {} blocks of this account today", limits::BLOCKS_PER_PAIR)
    } else {
        String::new()
    };
    format!(
        "Over {} blocks, unblocks and removed followers an hour for this account or {} from \
         this address{pair}, or over the request limit for this address",
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
        .response_with::<429, ErrorJson, _>(|r| r.description(&block_limits(false)))
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
        .response_with::<429, ErrorJson, _>(|r| r.description(&block_limits(false)))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use chrono::{TimeDelta, Utc};
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

    async fn scrobble(app: &TestApp, session: &str, minutes_ago: i64) {
        let play = json!({
            "track": TRACK,
            "artist": ARTIST,
            "album": "Shared Album",
            "played_at": (Utc::now() - TimeDelta::minutes(minutes_ago)).to_rfc3339(),
            "duration_ms": 200_000,
        });
        let (status, body) = app
            .api_as(session, Method::POST, "/v1/scrobble", Some(play))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

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

    /// The blocked account finds no trace of the blocker: every route of it
    /// answers as for an account that doesn't exist, and every list of
    /// people leaves it out. The blocker no longer sees the blocked account
    /// in lists, but may look at its profile; anyone else sees both.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn a_blocked_account_finds_no_trace_of_the_blocker() {
        with_custom_app(unlimited, |app| async move {
            let a = app.session.to_string();
            let a_name = app.username.clone();
            let (b_id, b_name, b) = app.other_user("blocked").await;
            let (_, c_name, c) = app.other_user("third").await;
            for (session, ago) in [(&a, 30), (&b, 20), (&c, 10)] {
                scrobble(&app, session, ago).await;
            }
            let (artist_id, track_id): (i64, i64) = sqlx::query_as(
                "SELECT t.artist_id, t.id FROM tracks t WHERE t.title = $1",
            )
            .bind(TRACK)
            .fetch_one(&app.pool)
            .await
            .unwrap();
            for session in [&a, &b] {
                for path in [
                    format!("/v1/artist/{artist_id}/comments"),
                    format!("/v1/track/{track_id}/comments"),
                ] {
                    let (status, _) = app
                        .api_as(session, Method::POST, &path, Some(json!({ "body": "hi" })))
                        .await;
                    assert_eq!(status, StatusCode::CREATED);
                }
            }
            sqlx::query(
                "INSERT INTO image_candidates (entity_type, entity_id, url, uploaded_by, vote_count)
                 SELECT 'artist', $1, 'https://img.test/' || id, id, 1 FROM users WHERE id = ANY($2)",
            )
            .bind(artist_id)
            .bind(vec![app.user_id, b_id])
            .execute(&app.pool)
            .await
            .unwrap();
            // Follows every way: both follow the third account, and each other.
            for (session, target) in [(&a, &c_name), (&b, &c_name), (&a, &b_name), (&b, &a_name)] {
                let (status, _) = app
                    .api_as(session, Method::POST, &format!("/v1/user/{target}/follow"), None)
                    .await;
                assert_eq!(status, StatusCode::NO_CONTENT);
            }
            let (_, token) = app
                .api_as(&b, Method::POST, "/v1/scrobbler/tokens", Some(json!({ "name": "pano" })))
                .await;
            let b_sk = token["token"].as_str().unwrap().to_string();

            let (status, _) = app
                .api(Method::POST, &format!("/v1/user/{b_name}/block"), None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            let follows: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM user_follows
                 WHERE (follower_id = $1 AND followee_id = $2) OR (follower_id = $2 AND followee_id = $1)",
            )
            .bind(app.user_id)
            .bind(b_id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
            assert_eq!(follows, 0, "the follows between them ended");

            // Every route of the blocker answers the blocked account as for
            // an account that doesn't exist; everyone else as before.
            let missing = get(&app, Some(&b), "/v1/user/nobody0here").await;
            assert_eq!(missing.0, StatusCode::NOT_FOUND);
            for suffix in [
                "",
                "/friends",
                "/followers",
                "/following",
                "/recent",
                "/top-artists",
                "/top-tracks",
                "/charts/artists",
                "/charts/tracks",
                "/charts/albums",
                "/heatmap",
                "/live",
            ] {
                let path = format!("/v1/user/{a_name}{suffix}");
                assert_eq!(get(&app, Some(&b), &path).await, missing, "{path}");
                if suffix != "/live" {
                    assert_eq!(get(&app, Some(&c), &path).await.0, StatusCode::OK, "{path}");
                    assert_eq!(get(&app, None, &path).await.0, StatusCode::OK, "{path}");
                }
            }
            let missing_write = app
                .api_as(&b, Method::POST, "/v1/user/nobody0here/follow", None)
                .await;
            for (method, path) in [
                (Method::POST, format!("/v1/user/{a_name}/follow")),
                (Method::DELETE, format!("/v1/user/{a_name}/follow")),
                (Method::POST, format!("/v1/user/{a_name}/block")),
                (Method::DELETE, format!("/v1/user/{a_name}/block")),
                (Method::DELETE, format!("/v1/user/me/followers/{a_name}")),
            ] {
                let answer = app.api_as(&b, method.clone(), &path, None).await;
                assert_eq!(answer, missing_write, "{method} {path}");
            }

            // Lists of people: the blocked account sees no blocker, the
            // blocker no blocked account, the others both.
            let lists = [
                format!("/v1/user/{c_name}/followers"),
                format!("/v1/user/{c_name}/friends"),
                format!("/v1/artist/{artist_id}/listeners"),
                format!("/v1/track/{track_id}/listeners"),
                format!("/v1/artist/{artist_id}/comments"),
                format!("/v1/track/{track_id}/comments"),
                format!("/v1/artist/{artist_id}/images"),
            ];
            let usernames = |body: &Value| -> Vec<String> {
                if let Some(followers) = body.get("followers") {
                    return names(followers);
                }
                match body.as_array() {
                    Some(list) if list.first().is_some_and(|e| e.get("uploaded_by").is_some()) => list
                        .iter()
                        .map(|e| e["uploaded_by"].as_str().unwrap().to_string())
                        .collect(),
                    _ => names(body),
                }
            };
            for path in &lists {
                let (status, body) = get(&app, Some(&b), path).await;
                assert_eq!(status, StatusCode::OK, "{path}");
                let seen = usernames(&body);
                assert!(!seen.contains(&a_name), "{path}: {seen:?}");
                let (_, body) = get(&app, Some(&a), path).await;
                let seen = usernames(&body);
                assert!(!seen.contains(&b_name), "{path}: {seen:?}");
                for viewer in [Some(c.as_str()), None] {
                    let (_, body) = get(&app, viewer, path).await;
                    let seen = usernames(&body);
                    assert!(seen.contains(&a_name) && seen.contains(&b_name), "{path}: {seen:?}");
                }
            }
            for (viewer, wanted, found) in [(&b, &a_name, false), (&a, &b_name, false), (&c, &a_name, true)] {
                let (_, body) = get(&app, Some(viewer), &format!("/v1/search?type=user&q={wanted}")).await;
                assert_eq!(names(&body).contains(wanted), found, "{body}");
            }
            let (_, body) = get(&app, None, &format!("/v1/search?type=user&q={a_name}")).await;
            assert_eq!(names(&body), vec![a_name.clone()]);

            // The Last.fm-compatible API, with the blocked account's key.
            let lastfm = |user: &str| {
                format!("/2.0/?method=user.getInfo&user={user}&api_key={}&sk={b_sk}&format=json", "0".repeat(32))
            };
            let (status, _, nobody) = app.get(&lastfm("nobody0here")).await;
            let (status_a, _, blocker) = app.get(&lastfm(&a_name)).await;
            assert_eq!((status_a, &blocker), (status, &nobody));
            assert!(blocker.contains("\"error\":6"), "{blocker}");
            let (status, _, body) = app.get(&lastfm(&c_name)).await;
            assert_eq!(status, StatusCode::OK, "{body}");

            // The blocker still sees the blocked profile, and that it blocks
            // it; it can't follow it while it does. Nobody sees a list of
            // who blocks them.
            let (status, profile) = get(&app, Some(&a), &format!("/v1/user/{b_name}")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                (&profile["blocking"], &profile["is_following"], &profile["follows_you"]),
                (&json!(true), &json!(false), &json!(false))
            );
            let (_, profile) = get(&app, Some(&c), &format!("/v1/user/{b_name}")).await;
            assert_eq!(profile["blocking"], json!(false));
            let (status, body) = app
                .api(Method::POST, &format!("/v1/user/{b_name}/follow"), None)
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            let (_, blocks) = get(&app, Some(&a), "/v1/user/me/blocks").await;
            assert_eq!(names(&blocks), vec![b_name.clone()]);
            assert_eq!(blocks["next"], json!(null));
            let (_, blocks) = get(&app, Some(&b), "/v1/user/me/blocks").await;
            assert_eq!(blocks["entries"], json!([]));
            let (_, own) = get(&app, Some(&b), "/v1/user/me").await;
            assert!(!own.to_string().contains(&a_name), "{own}");

            // Unblocked, each sees the other again; the follows stay ended.
            let (status, _) = app
                .api(Method::DELETE, &format!("/v1/user/{b_name}/block"), None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(get(&app, Some(&b), &format!("/v1/user/{a_name}")).await.0, StatusCode::OK);
            let (_, body) = get(&app, Some(&b), &format!("/v1/user/{c_name}/followers")).await;
            assert!(names(&body).contains(&a_name));
            let (_, profile) = get(&app, Some(&b), &format!("/v1/user/{a_name}")).await;
            assert_eq!(profile["is_following"], json!(false));
        })
        .await;
    }

    /// The feed and the community charts cover followed accounts, so a
    /// block takes each out of the other's at once, and with them out of
    /// the chart's cache key.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn a_block_takes_each_out_of_the_others_feed_and_community_charts() {
        with_app(Default::default(), |app| async move {
            let a = app.session.to_string();
            let (_, b_name, b) = app.other_user("blocked").await;
            scrobble(&app, &a, 30).await;
            scrobble(&app, &b, 20).await;
            for (session, target) in [(&a, &b_name), (&b, &app.username)] {
                app.api_as(
                    session,
                    Method::POST,
                    &format!("/v1/user/{target}/follow"),
                    None,
                )
                .await;
            }
            let feed_users = async |session: &str| -> Vec<String> {
                let (_, feed) = get(&app, Some(session), "/v1/user/me/feed").await;
                feed["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["user"]["username"].as_str().unwrap().to_string())
                    .collect()
            };
            assert_eq!(feed_users(&a).await, vec![b_name.clone()]);
            assert_eq!(feed_users(&b).await, vec![app.username.clone()]);
            let following = async |session: &str| {
                let (status, chart) =
                    get(&app, Some(session), "/v1/user/me/community/tracks").await;
                assert_eq!(status, StatusCode::OK, "{chart}");
                (chart["following"].clone(), chart["accounts"].clone())
            };
            assert_eq!(following(&a).await, (json!(1), json!(1)));

            app.api(Method::POST, &format!("/v1/user/{b_name}/block"), None)
                .await;
            assert!(feed_users(&a).await.is_empty());
            assert!(feed_users(&b).await.is_empty());
            assert_eq!(following(&a).await, (json!(0), json!(0)));
            assert_eq!(following(&b).await, (json!(0), json!(0)));
        })
        .await;
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
            let (status, _) = get(&app, Some(&blocked[0].2), &format!("/v1/user/{}", app.username)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
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

            // The same account three times a day: blocking and unblocking
            // can't keep flickering it.
            for _ in 1..crate::limits::BLOCKS_PER_PAIR {
                assert_eq!(
                    app.api(Method::POST, &block(&y), None).await.0,
                    StatusCode::NO_CONTENT
                );
                assert_eq!(
                    app.api(Method::DELETE, &block(&y), None).await.0,
                    StatusCode::NO_CONTENT
                );
            }
            // y counted once at the cap, so this is its fourth.
            assert_eq!(
                app.api(Method::POST, &block(&y), None).await.0,
                StatusCode::TOO_MANY_REQUESTS
            );

            // 3 + 2 × 2 + 1 actions so far; the hour's allowance ends at
            // 200, refused ones included.
            for _ in 8..crate::limits::BLOCK_ACTIONS_PER_USER {
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

    /// Following, unfollowing and following again keeps putting the
    /// follower first in the other's list: three new follows of one
    /// account a day.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn following_one_account_again_and_again_is_limited() {
        with_app(Default::default(), |app| async move {
            let (_, target, _) = app.other_user("target").await;
            let path = format!("/v1/user/{target}/follow");
            for _ in 0..crate::limits::FOLLOWS_PER_PAIR {
                assert_eq!(
                    app.api(Method::POST, &path, None).await.0,
                    StatusCode::NO_CONTENT
                );
                // Following what is followed already isn't a new follow.
                assert_eq!(
                    app.api(Method::POST, &path, None).await.0,
                    StatusCode::NO_CONTENT
                );
                assert_eq!(
                    app.api(Method::DELETE, &path, None).await.0,
                    StatusCode::NO_CONTENT
                );
            }
            assert_eq!(
                app.api(Method::POST, &path, None).await.0,
                StatusCode::TOO_MANY_REQUESTS
            );
        })
        .await;
    }
}
