//! Loved tracks (`db::queries::loves`; docs/social.md, Loved tracks).

use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{IdPath, UsernamePath};
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    limits,
    middleware::{auth::AuthUser, rate_limit::ClientIp},
    state::AppState,
};
use db::queries::loves::{self as loves_db, LoveSource, Loved, MAX_LOVED_TRACKS};
use db::queries::{tracks as tracks_db, users as users_db};
use shared::cursor::Cursor;
use shared::models::{LovedTrack, LovedTrackPage};

/// POST /v1/track/:id/love
pub async fn love(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<StatusCode> {
    if !limits::love_attempt(&state, auth_user.id, &ip).await {
        return Err(AppError::RateLimited);
    }
    tracks_db::find_track_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    match loves_db::love(
        &state.db,
        auth_user.id,
        id,
        LoveSource::NATIVE,
        MAX_LOVED_TRACKS,
    )
    .await?
    {
        Loved::Now(_) | Loved::Already(_) => Ok(StatusCode::NO_CONTENT),
        Loved::AtLimit => Err(at_limit()),
    }
}

pub fn at_limit() -> AppError {
    AppError::Conflict(format!(
        "you already love {MAX_LOVED_TRACKS} tracks, the most allowed"
    ))
}

fn love_limits() -> String {
    format!(
        "Over {} loves and unloves an hour for this account or {} from this address, \
         or over the request limit for this address",
        limits::LOVES_PER_USER,
        limits::LOVES_PER_IP
    )
}

pub fn _love_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Love a track")
        .description(&format!("Adds the track to the user's loved tracks; loving a track already loved changes nothing (its first `loved_at` stands). An account may love {MAX_LOVED_TRACKS} tracks at most, and love or unlove {} times an hour ({} from one address), through this API and the Last.fm-compatible one together, refused attempts included. Loves are shown with the profile, under its privacy, and feed nothing else: rankings, recommendations, similarity and album attribution don't read them, and no count of a track's loves is published.", limits::LOVES_PER_USER, limits::LOVES_PER_IP))
        .tag("Loved tracks")
        .response_with::<204, (), _>(|r| r.description("Loved, now or before"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Track not found"))
        .response_with::<409, ErrorJson, _>(|r| {
            r.description(&format!("Already loving {MAX_LOVED_TRACKS} tracks"))
        })
        .response_with::<429, ErrorJson, _>(|r| r.description(&love_limits()))
}

/// DELETE /v1/track/:id/love
pub async fn unlove(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    ClientIp(ip): ClientIp,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<StatusCode> {
    if !limits::love_attempt(&state, auth_user.id, &ip).await {
        return Err(AppError::RateLimited);
    }
    if !loves_db::unlove(&state.db, auth_user.id, id).await? {
        tracks_db::find_track_by_id(&state.db, id)
            .await?
            .ok_or(AppError::NotFound)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub fn _unlove_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Unlove a track")
        .description("Takes the track out of the user's loved tracks; unloving a track not loved changes nothing. Counts against the same hourly limits as loving.")
        .tag("Loved tracks")
        .response_with::<204, (), _>(|r| r.description("Not loved any more"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Track not found"))
        .response_with::<429, ErrorJson, _>(|r| r.description(&love_limits()))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LovedQuery {
    /// Entries per page: 50 by default, 200 at most.
    pub limit: Option<i64>,
    /// The previous page's `next`, for the page after it.
    pub before: Option<String>,
}

async fn loved_page(state: &AppState, user_id: i64, q: &LovedQuery) -> ApiResult<LovedTrackPage> {
    let before = q
        .before
        .as_deref()
        .map(|c| {
            Cursor::parse(c).ok_or_else(|| AppError::BadRequest("invalid `before` cursor".into()))
        })
        .transpose()?;
    let limit = super::page_limit(q.limit, 50, 200);
    let mut page = loves_db::page(&state.db, user_id, before, limit + 1).await?;
    let next = if page.len() as i64 > limit {
        page.truncate(limit as usize);
        page.last().map(|l| {
            Cursor {
                at: l.loved_at,
                id: l.track_id,
            }
            .encode()
        })
    } else {
        None
    };
    Ok(LovedTrackPage {
        total: loves_db::count(&state.db, user_id).await?,
        entries: page
            .into_iter()
            .map(|l| LovedTrack {
                track_id: l.track_id,
                track_title: l.track_title,
                artist_id: l.artist_id,
                artist_name: l.artist_name,
                album_id: l.album_id,
                album_title: l.album_title,
                album_image: l.album_image,
                artist_image: l.artist_image,
                loved_at: l.loved_at,
            })
            .collect(),
        next,
    })
}

const PAGES: &str = "Newest love first, `limit` entries a page (50 by default, 200 at most): pass `next` as `before` for the following page; `next` is `null` on the last. `total` counts every track the user loves. Each entry shows the track with its artist and the album the catalog files it under (`album_image`, else `artist_image`, for artwork).";

/// GET /v1/user/:username/loved
pub async fn user_loved(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    Query(q): Query<LovedQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<LovedTrackPage>> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;
    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;
    Ok(Json(loved_page(&state, user.id, &q).await?))
}

pub fn _user_loved_doc(op: TransformOperation) -> TransformOperation {
    op.summary("A user's loved tracks")
        .description(&format!("{PAGES} They are as visible as the profile: a private profile answers 403 to everyone but its owner, and an account waiting to be deleted or blocking the signed-in viewer 404, as one that doesn't exist."))
        .tag("Loved tracks")
        .response::<200, Json<LovedTrackPage>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Invalid `before` cursor"))
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| {
            r.description("User not found, waiting to be deleted, or blocking the viewer")
        })
}

/// GET /v1/user/me/loved
pub async fn own_loved(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Query(q): Query<LovedQuery>,
) -> ApiResult<Json<LovedTrackPage>> {
    Ok(Json(loved_page(&state, auth_user.id, &q).await?))
}

pub fn _own_loved_doc(op: TransformOperation) -> TransformOperation {
    op.summary("My loved tracks")
        .description(&format!(
            "The signed-in user's loved tracks, whatever the profile's privacy. {PAGES}"
        ))
        .tag("Loved tracks")
        .response::<200, Json<LovedTrackPage>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Invalid `before` cursor"))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use chrono::Utc;
    use fred::interfaces::KeysInterface;
    use serde_json::Value;

    use crate::test_app::{TestApp, with_app};
    use db::queries::loves::MAX_LOVED_TRACKS;
    use db::queries::tracks as tracks_db;

    async fn track(app: &TestApp, title: &str) -> i64 {
        let artist = tracks_db::find_or_create_artist(&app.pool, "Band")
            .await
            .unwrap();
        let album = tracks_db::find_or_create_album(&app.pool, artist.id, "Record")
            .await
            .unwrap();
        tracks_db::find_or_create_track(&app.pool, artist.id, Some(album), title, None)
            .await
            .unwrap()
            .id
    }

    async fn signed_out(app: &TestApp, path: &str) -> (StatusCode, Value) {
        let (status, _, body) = app.get(path).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn loving_is_idempotent_paged_capped_and_limited() {
        with_app(Default::default(), |app| async move {
            let first = track(&app, "First").await;
            let second = track(&app, "Second").await;
            for id in [first, first, second] {
                let (status, body) = app
                    .api(Method::POST, &format!("/v1/track/{id}/love"), None)
                    .await;
                assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
            }
            let (status, page) = app
                .api(Method::GET, "/v1/user/me/loved?limit=1", None)
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["total"], 2);
            assert_eq!(page["entries"][0]["track_id"], second);
            assert_eq!(page["entries"][0]["artist_name"], "Band");
            assert_eq!(page["entries"][0]["album_title"], "Record");
            let next = page["next"].as_str().unwrap();
            let (_, rest) = app
                .api(
                    Method::GET,
                    &format!("/v1/user/me/loved?before={next}"),
                    None,
                )
                .await;
            assert_eq!(rest["entries"].as_array().unwrap().len(), 1);
            assert_eq!(rest["entries"][0]["track_id"], first);
            assert_eq!(rest["next"], Value::Null);
            let (status, _) = app
                .api(Method::GET, "/v1/user/me/loved?before=nope", None)
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);

            for _ in 0..2 {
                let (status, _) = app
                    .api(Method::DELETE, &format!("/v1/track/{first}/love"), None)
                    .await;
                assert_eq!(status, StatusCode::NO_CONTENT);
            }
            let (_, page) = app.api(Method::GET, "/v1/user/me/loved", None).await;
            assert_eq!(page["total"], 1);
            for method in [Method::POST, Method::DELETE] {
                let (status, _) = app.api(method, "/v1/track/987654321/love", None).await;
                assert_eq!(status, StatusCode::NOT_FOUND);
            }

            // At the cap, a new love is refused and a loved track still answers.
            sqlx::query(
                "WITH artist AS (INSERT INTO artists (name, name_normalized)
                                 VALUES ('Many', 'many') RETURNING id),
                      made AS (INSERT INTO tracks (artist_id, title, title_normalized)
                               SELECT artist.id, 'T' || g, 't' || g
                               FROM artist, generate_series(1, $2 - 1) g RETURNING id)
                 INSERT INTO loved_tracks (user_id, track_id, source)
                 SELECT $1, id, 'scrobblr' FROM made",
            )
            .bind(app.user_id)
            .bind(MAX_LOVED_TRACKS)
            .execute(&app.pool)
            .await
            .unwrap();
            let (status, body) = app
                .api(Method::POST, &format!("/v1/track/{first}/love"), None)
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            let (status, _) = app
                .api(Method::POST, &format!("/v1/track/{second}/love"), None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT);

            // Past the hourly limit, loves and unloves alike are refused.
            let window = Utc::now().timestamp() / 3600;
            let _: () = app
                .redis
                .set(
                    format!("love:user:{}:{window}", app.user_id),
                    crate::limits::LOVES_PER_USER,
                    None,
                    None,
                    false,
                )
                .await
                .unwrap();
            for method in [Method::POST, Method::DELETE] {
                let (status, _) = app
                    .api(method, &format!("/v1/track/{second}/love"), None)
                    .await;
                assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
            }
        })
        .await;
    }

    /// Loved tracks are seen where the profile is: by anyone while public,
    /// by the owner alone while private, by no one while the account waits
    /// to be erased, and not by an account it blocks.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn loved_tracks_follow_the_profile_rules() {
        with_app(Default::default(), |app| async move {
            let id = track(&app, "Loved").await;
            let (status, _) = app
                .api(Method::POST, &format!("/v1/track/{id}/love"), None)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            let (viewer, _, viewer_session) = app.other_user("viewer").await;
            let path = format!("/v1/user/{}/loved", app.username);
            let set = |sql: &'static str| {
                let pool = app.pool.clone();
                async move { sqlx::query(sql).execute(&pool).await.unwrap() }
            };

            let (status, page) = signed_out(&app, &path).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["entries"][0]["track_id"], id);
            let (status, _) = app.api_as(&viewer_session, Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::OK);

            set("UPDATE users SET is_private = TRUE").await;
            assert_eq!(signed_out(&app, &path).await.0, StatusCode::FORBIDDEN);
            let (status, _) = app.api_as(&viewer_session, Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            let (status, page) = app.api(Method::GET, &path, None).await;
            assert_eq!((status, page["total"].as_i64()), (StatusCode::OK, Some(1)));
            set("UPDATE users SET is_private = FALSE").await;

            sqlx::query("INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1, $2)")
                .bind(app.user_id)
                .bind(viewer)
                .execute(&app.pool)
                .await
                .unwrap();
            let (status, _) = app.api_as(&viewer_session, Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(signed_out(&app, &path).await.0, StatusCode::OK);
            set("DELETE FROM user_blocks").await;

            sqlx::query("UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE id = $1")
                .bind(app.user_id)
                .execute(&app.pool)
                .await
                .unwrap();
            assert_eq!(signed_out(&app, &path).await.0, StatusCode::NOT_FOUND);
            let (status, _) = app.api_as(&viewer_session, Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            // Kept through the grace period, and back with the account.
            set("UPDATE users SET delete_after = NULL").await;
            let (status, page) = signed_out(&app, &path).await;
            assert_eq!((status, page["total"].as_i64()), (StatusCode::OK, Some(1)));

            let (status, _) = signed_out(&app, "/v1/user/nobody_here/loved").await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
    }
}
