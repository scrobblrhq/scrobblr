use super::IdPath;
use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    state::AppState,
};
use db::queries::{enrichment as enrichment_db, tracks as tracks_db, users as users_db};
use shared::models::{Artist, TopListener, TopTrack, Track, TrackWithCredits, UserProfile};

/// GET /v1/track/:id
pub async fn get_track(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<Json<TrackWithCredits>> {
    let track = tracks_db::find_track_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let artists = tracks_db::track_credits(&state.db, track.id).await?;

    Ok(Json(TrackWithCredits { track, artists }))
}

pub fn _get_track_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get a track")
        .description("Returns catalog metadata for a single track by its internal ID, including title, album, duration and the full artist credit list (primary artist first, then featured collaborators in billing order).")
        .tag("Catalog")
        .response::<200, Json<TrackWithCredits>>()
        .response_with::<404, ErrorJson, _>(|r| r.description("Track not found"))
}

/// GET /v1/artist/:id
pub async fn get_artist(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
) -> ApiResult<Json<Artist>> {
    tracks_db::find_artist_by_id(&state.db, id)
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

pub fn _get_artist_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get an artist")
        .description("Returns catalog metadata for a single artist by its internal ID.")
        .tag("Catalog")
        .response::<200, Json<Artist>>()
        .response_with::<404, ErrorJson, _>(|r| r.description("Artist not found"))
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
        tracks_db::search_artists(&state.db, &q.q, limit).await?
    } else {
        vec![]
    };

    let tracks = if wants("track") {
        tracks_db::search_tracks(&state.db, &q.q, limit).await?
    } else {
        vec![]
    };

    let users = if wants("user") {
        users_db::search_users(&state.db, &q.q, limit)
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
        .description("Full-text search across artists, tracks and users. Use the `type` parameter to restrict results to `track`, `artist`, `user`, or `all` (default). Returns up to 30 results per type. Query must not be empty.")
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
    let tracks = tracks_db::artist_top_tracks(&state.db, id, section_limit(&q)).await?;
    Ok(Json(tracks))
}

pub fn _artist_top_tracks_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Artist's top tracks")
        .description(
            "Returns the artist's most-scrobbled tracks across all users, ranked by play count.",
        )
        .tag("Catalog")
        .response::<200, Json<Vec<TopTrack>>>()
        .response_with::<404, ErrorJson, _>(|r| r.description("Artist not found"))
}

/// GET /v1/artist/:id/listeners
pub async fn artist_listeners(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Query(q): Query<SectionQuery>,
) -> ApiResult<Json<Vec<TopListener>>> {
    tracks_db::find_artist_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let listeners = tracks_db::artist_listeners(&state.db, id, section_limit(&q)).await?;
    Ok(Json(listeners))
}

pub fn _artist_listeners_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Artist's top listeners")
        .description("Returns the users who scrobble this artist the most (public profiles only), ranked by play count.")
        .tag("Catalog")
        .response::<200, Json<Vec<TopListener>>>()
        .response_with::<404, ErrorJson, _>(|r| r.description("Artist not found"))
}

/// GET /v1/track/:id/listeners
pub async fn track_listeners(
    State(state): State<AppState>,
    Path(IdPath { id }): Path<IdPath>,
    Query(q): Query<SectionQuery>,
) -> ApiResult<Json<Vec<TopListener>>> {
    tracks_db::find_track_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let listeners = tracks_db::track_listeners(&state.db, id, section_limit(&q)).await?;
    Ok(Json(listeners))
}

pub fn _track_listeners_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Track's top listeners")
        .description("Returns the users who scrobble this track the most (public profiles only), ranked by play count.")
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
    use axum::http::Method;
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

            let (_, top) = app
                .api(
                    Method::GET,
                    &format!("/v1/artist/{artist_id}/top-tracks"),
                    None,
                )
                .await;
            assert_eq!(top[0]["track_title"], "Hit");
            assert_eq!(top[0]["play_count"], 2);
            assert_eq!(top[1]["play_count"], 1);
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
}
