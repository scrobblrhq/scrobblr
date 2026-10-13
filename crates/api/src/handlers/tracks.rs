use super::IdPath;
use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    middleware::auth::AuthUser,
    state::AppState,
};
use db::queries::{enrichment as enrichment_db, tracks as tracks_db, users as users_db};
use shared::models::{Album, Artist, TopListener, TopTrack, Track, TrackWithCredits, UserProfile};

fn min_listeners(state: &AppState) -> i32 {
    i32::try_from(state.rankings.min_listeners).unwrap_or(i32::MAX)
}

const COUNTS_RULE: &str = "Catalog-wide counts include every user's plays, private profiles' too, so they are served only for entities at least `RANKING_MIN_LISTENERS` (5 by default) distinct users have played, counted from the daily aggregates (a new listener counts from their next refresh, within the hour; today's plays from tomorrow): a count is never one or two people's listening.";

/// GET /v1/track/:id
pub async fn get_track(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<Json<TrackWithCredits>> {
    let track = tracks_db::public_track(&state.db, id, min_listeners(&state))
        .await?
        .ok_or(AppError::NotFound)?;
    let artists = tracks_db::track_credits(&state.db, track.id).await?;

    Ok(Json(TrackWithCredits { track, artists }))
}

pub fn _get_track_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get a track")
        .description(&format!("Returns catalog metadata for a single track by its internal ID, including title, album, duration and the full artist credit list (primary artist first, then featured collaborators in billing order). `scrobble_count` is `null` for a track fewer than the minimum of listeners played. {COUNTS_RULE}"))
        .tag("Catalog")
        .response::<200, Json<TrackWithCredits>>()
        .response_with::<404, ErrorJson, _>(|r| r.description("Track not found"))
}

/// GET /v1/artist/:id
pub async fn get_artist(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<Json<Artist>> {
    tracks_db::public_artist(&state.db, id, min_listeners(&state))
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

pub fn _get_artist_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get an artist")
        .description(&format!("Returns catalog metadata for a single artist by its internal ID. `scrobble_count` is `null` for an artist fewer than the minimum of listeners played; `listener_count` is always `null` (it was never kept). {COUNTS_RULE}"))
        .tag("Catalog")
        .response::<200, Json<Artist>>()
        .response_with::<404, ErrorJson, _>(|r| r.description("Artist not found"))
}

/// GET /v1/album/:id
pub async fn get_album(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<Json<Album>> {
    tracks_db::public_album(&state.db, id, min_listeners(&state))
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

pub fn _get_album_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get an album")
        .description(&format!("Returns catalog metadata for a single album by its internal ID, with its artist's name. Each track belongs to one album, the one the accounts playing it named (one say per account at least a week old, a release MusicBrainz lists the recording on counting double; another album needs half again the current one's accounts to take the track), and its plays count for that album here and in charts, from whichever release they were played. `scrobble_count` (the plays of its tracks) is `null` for an album fewer than the minimum of listeners played. {COUNTS_RULE}"))
        .tag("Catalog")
        .response::<200, Json<Album>>()
        .response_with::<404, ErrorJson, _>(|r| r.description("Album not found"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchQuery {
    pub q: String,
    pub r#type: Option<String>, // "track" | "artist" | "all" (default)
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchResponse {
    pub artists: Vec<Artist>,
    pub tracks: Vec<Track>,
    pub users: Vec<UserProfile>,
}

const SEARCH_MAX_CHARS: usize = 200;

/// GET /v1/search
pub async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    if q.q.trim().is_empty() {
        return Err(AppError::BadRequest("search query cannot be empty".into()));
    }
    if q.q.chars().count() > SEARCH_MAX_CHARS {
        return Err(AppError::BadRequest(format!(
            "search query may be at most {SEARCH_MAX_CHARS} characters"
        )));
    }

    let limit = super::page_limit(q.limit, 10, 30);
    let kind = q.r#type.as_deref().unwrap_or("all");
    let wants = |k: &str| kind == "all" || kind == k;

    let artists = if wants("artist") {
        tracks_db::search_artists(&state.db, &q.q, limit, min_listeners(&state)).await?
    } else {
        vec![]
    };

    let tracks = if wants("track") {
        tracks_db::search_tracks(&state.db, &q.q, limit, min_listeners(&state)).await?
    } else {
        vec![]
    };

    let users = if wants("user") {
        users_db::search_users(&state.db, &q.q, limit, auth_user.map(|Extension(a)| a.id))
            .await?
            .into_iter()
            .map(UserProfile::from)
            .collect()
    } else {
        vec![]
    };

    Ok(Json(SearchResponse {
        artists,
        tracks,
        users,
    }))
}

pub fn _search_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Search catalog and users")
        .description(&format!("Full-text search across artists, tracks and users. Use the `type` parameter to restrict results to `track`, `artist`, `user`, or `all` (default). Returns up to 30 results per type. Query must not be empty. Users are public profiles only, never accounts waiting to be deleted nor, for a signed-in viewer, accounts blocking it or blocked by it. Artists' and tracks' `scrobble_count` are as `GET /v1/artist/{{id}}` and `GET /v1/track/{{id}}` serve them. {COUNTS_RULE}"))
        .tag("Catalog")
        .response::<200, Json<SearchResponse>>()
        .response_with::<400, ErrorJson, _>(|r| r.description("Empty or overlong search query"))
}

/// Shared query params for the listener/top-track sections.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SectionQuery {
    pub limit: Option<i64>,
}

fn section_limit(q: &SectionQuery) -> i64 {
    super::page_limit(q.limit, 10, 50)
}

/// GET /v1/artist/:id/top-tracks
pub async fn artist_top_tracks(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Query(q): Query<SectionQuery>,
) -> ApiResult<Json<Vec<TopTrack>>> {
    tracks_db::find_artist_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let tracks =
        tracks_db::artist_top_tracks(&state.db, id, section_limit(&q), min_listeners(&state))
            .await?;
    Ok(Json(tracks))
}

pub fn _artist_top_tracks_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Artist's top tracks")
        .description(&format!(
            "Returns the artist's most-scrobbled tracks across all users, ranked by play count, leaving out tracks fewer than the minimum of listeners played (of the 200 most played). {COUNTS_RULE}"
        ))
        .tag("Catalog")
        .response::<200, Json<Vec<TopTrack>>>()
        .response_with::<404, ErrorJson, _>(|r| r.description("Artist not found"))
}

/// GET /v1/artist/:id/listeners
pub async fn artist_listeners(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Query(q): Query<SectionQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<Vec<TopListener>>> {
    tracks_db::find_artist_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let viewer = auth_user.map(|Extension(a)| a.id);
    let listeners = tracks_db::artist_listeners(&state.db, id, section_limit(&q), viewer).await?;
    Ok(Json(listeners))
}

pub fn _artist_listeners_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Artist's top listeners")
        .description("Returns the users who scrobble this artist the most (public profiles only, never accounts waiting to be deleted nor, for a signed-in viewer, accounts blocking it or blocked by it), ranked by play count: what their public profiles already show.")
        .tag("Catalog")
        .response::<200, Json<Vec<TopListener>>>()
        .response_with::<404, ErrorJson, _>(|r| r.description("Artist not found"))
}

/// GET /v1/track/:id/listeners
pub async fn track_listeners(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Query(q): Query<SectionQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<Json<Vec<TopListener>>> {
    tracks_db::find_track_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let viewer = auth_user.map(|Extension(a)| a.id);
    let listeners = tracks_db::track_listeners(&state.db, id, section_limit(&q), viewer).await?;
    Ok(Json(listeners))
}

pub fn _track_listeners_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Track's top listeners")
        .description("Returns the users who scrobble this track the most (public profiles only, never accounts waiting to be deleted nor, for a signed-in viewer, accounts blocking it or blocked by it), ranked by play count: what their public profiles already show.")
        .tag("Catalog")
        .response::<200, Json<Vec<TopListener>>>()
        .response_with::<404, ErrorJson, _>(|r| r.description("Track not found"))
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RefreshResponse {
    pub status: String,
}

async fn queue_refresh(
    state: &AppState,
    entity_type: &str,
    id: i64,
) -> ApiResult<(StatusCode, Json<RefreshResponse>)> {
    enrichment_db::request_refresh(&state.db, entity_type, id).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(RefreshResponse {
            status: "queued".into(),
        }),
    ))
}

/// POST /v1/track/:id/refresh
pub async fn refresh_track(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<impl IntoApiResponse> {
    tracks_db::find_track_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    queue_refresh(&state, "track", id).await
}

pub fn _refresh_track_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Refresh track metadata")
        .description("Queues a forced metadata re-enrichment for the track (MusicBrainz ID, duration). Processed asynchronously by the background worker.")
        .tag("Catalog")
        .response::<202, Json<RefreshResponse>>()
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Track not found"))
}

/// POST /v1/artist/:id/refresh
pub async fn refresh_artist(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<impl IntoApiResponse> {
    tracks_db::find_artist_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    queue_refresh(&state, "artist", id).await
}

pub fn _refresh_artist_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Refresh artist metadata")
        .description("Queues a forced metadata re-enrichment for the artist (MusicBrainz ID, image, bio). Overwrites provider-sourced fields; processed asynchronously by the background worker.")
        .tag("Catalog")
        .response::<202, Json<RefreshResponse>>()
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Artist not found"))
}

/// POST /v1/album/:id/refresh
pub async fn refresh_album(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<impl IntoApiResponse> {
    tracks_db::find_album_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    queue_refresh(&state, "album", id).await
}

pub fn _refresh_album_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Refresh album metadata")
        .description("Queues a forced metadata re-enrichment for the album (MusicBrainz ID, cover art, release date). Overwrites provider-sourced fields; processed asynchronously by the background worker.")
        .tag("Catalog")
        .response::<202, Json<RefreshResponse>>()
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<404, ErrorJson, _>(|r| r.description("Album not found"))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use chrono::{TimeDelta, Utc};
    use serde_json::json;

    use crate::test_app::with_app;

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn sections_count_fresh_scrobbles_of_public_listeners() {
        with_app(Default::default(), |app| async move {
            for (i, track) in ["Hit", "Hit", "Other"].iter().enumerate() {
                let play = json!({
                    "track": track,
                    "artist": "Band",
                    "played_at": (Utc::now() - TimeDelta::minutes(10 * i as i64)).to_rfc3339(),
                });
                app.api(Method::POST, "/v1/scrobble", Some(play)).await;
            }
            let (track_id, artist_id): (i64, i64) =
                sqlx::query_as("SELECT id, artist_id FROM tracks WHERE title = 'Hit'")
                    .fetch_one(&app.pool)
                    .await
                    .unwrap();

            // One listener: no catalog-wide count of theirs is served.
            let (_, top) = app
                .api(
                    Method::GET,
                    &format!("/v1/artist/{artist_id}/top-tracks"),
                    None,
                )
                .await;
            assert_eq!(top, json!([]));
            let (_, listeners) = app
                .api(
                    Method::GET,
                    &format!("/v1/track/{track_id}/listeners"),
                    None,
                )
                .await;
            assert_eq!(listeners[0]["username"], app.username.as_str());
            assert_eq!(listeners[0]["play_count"], 2);

            let private = json!({ "is_private": true });
            app.api(Method::PATCH, "/v1/user/me", Some(private)).await;
            let (_, listeners) = app
                .api(
                    Method::GET,
                    &format!("/v1/artist/{artist_id}/listeners"),
                    None,
                )
                .await;
            assert_eq!(listeners, json!([]));
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn catalog_counts_need_the_minimum_of_listeners() {
        with_app(Default::default(), |app| async move {
            let day = Utc::now() - TimeDelta::days(2);
            let listen = async |user: &str, title: &str, plays: i64| {
                let user_id: i64 = sqlx::query_scalar(
                    "INSERT INTO users (username, email, password_hash) VALUES ($1, $1 || '@t', 'x')
                     ON CONFLICT (username) DO UPDATE SET username = EXCLUDED.username RETURNING id",
                )
                .bind(user)
                .fetch_one(&app.pool)
                .await
                .unwrap();
                let artist = db::queries::tracks::find_or_create_artist(&app.pool, "Quiet Band")
                    .await
                    .unwrap();
                let track = db::queries::tracks::find_or_create_track(
                    &app.pool, artist.id, None, title, None,
                )
                .await
                .unwrap();
                for n in 0..plays {
                    sqlx::query(
                        "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source)
                         VALUES ($1, $2, $3, $4, 'test')",
                    )
                    .bind(user_id)
                    .bind(track.id)
                    .bind(artist.id)
                    .bind(day + TimeDelta::minutes(n * 4))
                    .execute(&app.pool)
                    .await
                    .unwrap();
                }
                db::queries::scrobbles::refresh_scrobble_aggregates(&app.pool, day, day + TimeDelta::days(1))
                    .await
                    .unwrap();
                (track.id, artist.id)
            };
            // One fan plays the rare track a lot; three others the hit.
            let (rare, artist) = listen("fan0", "Rare", 40).await;
            let mut hit = 0;
            for n in 1..=3 {
                hit = listen(&format!("fan{n}"), "Hit", 2).await.0;
            }
            let count = async |path: String| app.get(&path).await.2;
            let track = |id| format!("/v1/track/{id}");
            let served = |body: String| serde_json::from_str::<serde_json::Value>(&body).unwrap();
            assert_eq!(served(count(track(hit)).await)["scrobble_count"], json!(null));
            assert_eq!(
                served(count(format!("/v1/artist/{artist}")).await)["scrobble_count"],
                json!(null),
                "four listeners aren't enough"
            );

            // A fifth listener of the artist: its count is served, not yet
            // the hit's (four).
            listen("fan4", "Hit", 2).await;
            assert_eq!(
                served(count(format!("/v1/artist/{artist}")).await)["scrobble_count"],
                48
            );
            assert_eq!(served(count(track(hit)).await)["scrobble_count"], json!(null));
            assert_eq!(
                served(count(format!("/v1/artist/{artist}/top-tracks")).await),
                json!([])
            );

            listen("fan5", "Hit", 1).await;
            assert_eq!(served(count(track(hit)).await)["scrobble_count"], 9);
            let artist_body = served(count(format!("/v1/artist/{artist}")).await);
            assert_eq!(artist_body["scrobble_count"], 49);
            assert_eq!(artist_body["listener_count"], json!(null));
            // The rare track's 40 plays are one person's: still not served.
            assert_eq!(served(count(track(rare)).await)["scrobble_count"], json!(null));
            let top = served(count(format!("/v1/artist/{artist}/top-tracks")).await);
            assert_eq!(top.as_array().unwrap().len(), 1);
            assert_eq!(top[0]["track_title"], "Hit");
            assert_eq!(top[0]["play_count"], 9);
            let search = served(count("/v1/search?type=track&q=Rare".into()).await);
            assert_eq!(search["tracks"][0]["scrobble_count"], json!(null));
            let search = served(count("/v1/search?type=artist&q=Quiet%20Band".into()).await);
            assert_eq!(search["artists"][0]["scrobble_count"], 49);
        })
        .await;
    }

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn albums_count_their_tracks_plays_past_the_minimum_of_listeners() {
        with_app(Default::default(), |app| async move {
            let artist = db::queries::tracks::find_or_create_artist(&app.pool, "Band")
                .await
                .unwrap();
            let album = db::queries::tracks::find_or_create_album(&app.pool, artist.id, "LP")
                .await
                .unwrap();
            let mut tracks = Vec::new();
            for title in ["Side A", "Side B"] {
                tracks.push(
                    db::queries::tracks::find_or_create_track(
                        &app.pool,
                        artist.id,
                        Some(album),
                        title,
                        None,
                    )
                    .await
                    .unwrap()
                    .id,
                );
            }
            let day = Utc::now() - TimeDelta::days(2);
            // Each fan plays one side, so only the album has them all.
            let listen = async |n: i64| {
                let user: i64 = sqlx::query_scalar(
                    "INSERT INTO users (username, email, password_hash)
                     VALUES ('fan' || $1, 'fan' || $1 || '@t', 'x') RETURNING id",
                )
                .bind(n)
                .fetch_one(&app.pool)
                .await
                .unwrap();
                sqlx::query(
                    "INSERT INTO scrobbles (user_id, track_id, artist_id, album_id, played_at, source)
                     VALUES ($1, $2, $3, $4, $5, 'test')",
                )
                .bind(user)
                .bind(tracks[n as usize % 2])
                .bind(artist.id)
                .bind(album)
                .bind(day + TimeDelta::minutes(n))
                .execute(&app.pool)
                .await
                .unwrap();
                db::queries::scrobbles::refresh_scrobble_aggregates(&app.pool, day, day)
                    .await
                    .unwrap();
            };
            for n in 0..4 {
                listen(n).await;
            }
            let get = async || {
                let (status, _, body) = app.get(&format!("/v1/album/{album}")).await;
                assert_eq!(status, StatusCode::OK);
                serde_json::from_str::<serde_json::Value>(&body).unwrap()
            };
            let body = get().await;
            assert_eq!(body["title"], "LP");
            assert_eq!(body["artist_name"], "Band");
            assert_eq!(body["scrobble_count"], json!(null));
            listen(4).await;
            assert_eq!(get().await["scrobble_count"], 5);
            assert_eq!(app.get("/v1/album/0").await.0, StatusCode::NOT_FOUND);
        })
        .await;
    }
}
