//! The follow feed: what the accounts a user follows played lately
//! (`db::queries::feed`).

use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Query, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;

use super::caching;
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::feed::{self as feed_db, FEED_DAYS};
use shared::cursor::Cursor;
use shared::models::FeedPage;

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 100;

/// Others' activity, which privacy settings can withdraw at any time.
const CACHE_CONTROL: &str = "private, no-store";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FeedQuery {
    /// Entries per page: 50 by default, 100 at most.
    pub limit: Option<i64>,
    /// The previous page's `next`, for the page after it.
    pub before: Option<String>,
}

/// GET /v1/user/me/feed
pub async fn feed(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Query(q): Query<FeedQuery>,
) -> ApiResult<Response> {
    let before = q
        .before
        .as_deref()
        .map(|c| {
            Cursor::parse(c).ok_or_else(|| AppError::BadRequest("invalid `before` cursor".into()))
        })
        .transpose()?;
    let limit = super::page_limit(q.limit, DEFAULT_LIMIT, MAX_LIMIT);
    let page = feed_db::feed_page(&state.db, auth_user.id, before, limit, Utc::now()).await?;
    let mut response = Json(FeedPage {
        entries: page.entries,
        next: page.next.map(|c| c.encode()),
    })
    .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    Ok(response)
}

pub fn _feed_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Feed of followed accounts")
        .description(&format!(
            "What the accounts the signed-in user follows played in the last {FEED_DAYS} days, \
             newest first: each entry a scrobble, with who played it and when, the track, its \
             artist and album, its label, and whether the account that played it loves the track (`loved`). Accounts anyone may see only: private ones and \
             those waiting to be deleted are left out, and so are the classifier's suspect plays \
             and duplicates (the same listen reported again); plays not classified yet are in, \
             with `status: null`.\n\n\
             Pages of `limit` entries (50 by default, 100 at most); pass `next` as `before` for \
             the next, older page, which stays where it was however many plays arrive meanwhile. \
             `next` is `null` once the page reaches the feed's oldest day; a page can hold fewer \
             entries than asked for and still have a next one. To catch up, poll the first page \
             and stop paging at an entry already seen (entries are unique by `id`). Nothing is \
             pushed: there is no stream."
        ))
        .tag("Social")
        .response_with::<200, Json<FeedPage>, _>(|r| {
            caching::cache_control(r, CACHE_CONTROL).description("A page of the feed")
        })
        .response_with::<400, ErrorJson, _>(|r| r.description("Invalid `before` cursor"))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use chrono::{DateTime, TimeDelta, Utc};
    use serde_json::{Value, json};

    use crate::test_app::{TestApp, with_app};
    use db::queries::classification as cdb;
    use shared::classification::BudgetParams;

    async fn play(app: &TestApp, user: i64, title: &str, at: DateTime<Utc>) {
        let artist = db::queries::tracks::find_or_create_artist(&app.pool, "Band")
            .await
            .unwrap();
        let track = db::queries::tracks::find_or_create_track(
            &app.pool,
            artist.id,
            None,
            title,
            Some(200_000),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms)
             VALUES ($1, $2, $3, $4, 'test', 200000)",
        )
        .bind(user)
        .bind(track.id)
        .bind(artist.id)
        .bind(at)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    fn titles(page: &Value) -> Vec<String> {
        page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                format!(
                    "{}:{}",
                    e["user"]["username"].as_str().unwrap(),
                    e["track_title"].as_str().unwrap()
                )
            })
            .collect()
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn the_feed_shows_visible_follows_recent_plays_newest_first() {
        with_app(Default::default(), |app| async move {
            let now = Utc::now();
            let (friend, friend_name, _) = app.other_user("friend").await;
            let (private, _, _) = app.other_user("private").await;
            let (leaving, _, _) = app.other_user("leaving").await;
            let (bot, bot_name, _) = app.other_user("bot").await;
            let (stranger, _, _) = app.other_user("stranger").await;
            for id in [friend, private, leaving, bot] {
                app.api(
                    Method::POST,
                    &format!("/v1/user/{}/follow", username(&app, id).await),
                    None,
                )
                .await;
            }
            sqlx::query("UPDATE users SET is_private = TRUE WHERE id = $1")
                .bind(private)
                .execute(&app.pool)
                .await
                .unwrap();
            sqlx::query("UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE id = $1")
                .bind(leaving)
                .execute(&app.pool)
                .await
                .unwrap();
            for n in 0..5 {
                play(
                    &app,
                    friend,
                    &format!("Song {n}"),
                    now - TimeDelta::minutes(10 * n),
                )
                .await;
            }
            play(&app, friend, "Old", now - TimeDelta::days(15)).await;
            play(&app, friend, "Week", now - TimeDelta::days(7)).await;
            for id in [private, leaving, stranger] {
                play(&app, id, "Hidden", now - TimeDelta::minutes(5)).await;
            }
            // The same listen twice, and far more plays than real time
            // allows: only plausible plays stay.
            let day = (now - TimeDelta::days(2)).date_naive();
            let at = |secs| day.and_hms_opt(0, 0, 0).unwrap().and_utc() + TimeDelta::seconds(secs);
            play(&app, friend, "Twice", at(43_200)).await;
            play(&app, friend, "Twice", at(43_205)).await;
            for n in 0..200 {
                play(&app, bot, &format!("Spam {n}"), at(3600 + n * 10)).await;
            }
            let rules = cdb::register_ruleset(&app.pool, BudgetParams::default())
                .await
                .unwrap();
            for user in [friend, bot] {
                cdb::classify_user_day(&app.pool, &rules, user, day, false)
                    .await
                    .unwrap();
            }

            let (status, page) = app.api(Method::GET, "/v1/user/me/feed?limit=3", None).await;
            assert_eq!(status, StatusCode::OK, "{page}");
            let first = titles(&page);
            assert_eq!(
                first,
                ["Song 0", "Song 1", "Song 2"].map(|t| format!("{friend_name}:{t}"))
            );
            assert_eq!(
                page["entries"][0]["status"],
                json!(null),
                "not classified yet"
            );
            assert_eq!(page["entries"][0]["user"]["id"], friend);
            let mut seen = first;
            let mut next = page["next"].as_str().unwrap().to_owned();
            loop {
                let (_, page) = app
                    .api(
                        Method::GET,
                        &format!("/v1/user/me/feed?limit=3&before={next}"),
                        None,
                    )
                    .await;
                seen.extend(titles(&page));
                match page["next"].as_str() {
                    Some(n) => next = n.to_owned(),
                    None => break,
                }
            }
            let bot_plays = seen.iter().filter(|t| t.starts_with(&bot_name)).count();
            assert!(
                (10..200).contains(&bot_plays),
                "{bot_plays} of the bot's plays"
            );
            let friends: Vec<&String> = seen
                .iter()
                .filter(|t| t.starts_with(&friend_name))
                .collect();
            let expected: Vec<String> = [
                "Song 0", "Song 1", "Song 2", "Song 3", "Song 4", "Twice", "Week",
            ]
            .map(|t| format!("{friend_name}:{t}"))
            .to_vec();
            assert_eq!(friends, expected.iter().collect::<Vec<_>>());
            assert!(!seen.iter().any(|t| t.ends_with(":Hidden")));

            let (status, _) = app
                .api(Method::GET, "/v1/user/me/feed?before=yesterday", None)
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            let (_, page) = app
                .api(Method::GET, "/v1/user/me/feed?limit=1000", None)
                .await;
            assert_eq!(
                page["entries"].as_array().unwrap().len(),
                seen.len().min(100)
            );
        })
        .await;
    }

    async fn username(app: &TestApp, id: i64) -> String {
        sqlx::query_scalar("SELECT username FROM users WHERE id = $1")
            .bind(id)
            .fetch_one(&app.pool)
            .await
            .unwrap()
    }
}
