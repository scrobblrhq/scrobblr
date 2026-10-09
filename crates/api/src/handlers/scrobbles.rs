use super::UsernamePath;
use crate::{
    errors::{ApiResult, AppError, ErrorJson},
    limits,
    middleware::{auth::AuthUser, rate_limit::ClientIp},
    state::AppState,
};
use aide::OperationOutput;
use aide::axum::IntoApiResponse;
use aide::transform::TransformOperation;
use axum::response::{IntoResponse, Response};
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
};
use chrono::{Duration, TimeDelta, Utc};
use db::queries::scrobble_clients::{ClientIdentity, PROTOCOL_SCROBBLR};
use db::queries::{scrobbles as scrobbles_db, tracks as tracks_db, users as users_db};
use fred::interfaces::PubsubInterface;
use futures_util::stream::{self, Stream, StreamExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use shared::models::{ActivityDay, NowPlayingRich, ScrobbleRich, TopArtist, TopTrack};
use shared::scrobble::{ScrobbleInput, plausible_duration_ms};
use std::convert::Infallible;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScrobbleRequest {
    pub track: String,
    pub artist: String,
    /// Collaborators credited on the track, in billing order. The primary
    /// artist is `artist`; repeating it here is ignored.
    #[serde(default)]
    pub featured_artists: Vec<String>,
    pub album: Option<String>,
    pub played_at: chrono::DateTime<Utc>,
    pub duration_ms: Option<i32>,
    pub listened_ms: Option<i32>,
    pub source: Option<String>,
    /// MusicBrainz recording id, if the client knows it: a hint for
    /// enrichment, checked against the title and artist before use.
    pub recording_mbid: Option<uuid::Uuid>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ScrobbleResponse {
    pub id: i64,
    pub played_at: chrono::DateTime<Utc>,
}

/// Older plays are refused: history comes in through the importer. Longer
/// than the scrobbler APIs' 14 days because the mobile app queues plays
/// while offline, and still short of the chunks compression has reached.
const MAX_AGE: TimeDelta = TimeDelta::days(30);

/// POST /v1/scrobble
pub async fn scrobble(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<ScrobbleRequest>,
) -> ApiResult<impl IntoApiResponse> {
    if body.played_at < Utc::now() - MAX_AGE {
        return Err(AppError::ScrobbleInvalid(format!(
            "played_at is more than {} days ago",
            MAX_AGE.num_days()
        )));
    }
    let quota = limits::reserve_scrobbles(&state, auth_user.id, 1).await;
    if quota.granted == 0 {
        return Err(AppError::DailyLimit);
    }

    let client = ClientIdentity::new(
        PROTOCOL_SCROBBLR,
        body.source.as_deref().unwrap_or("extension"),
        false,
    );
    let input = ScrobbleInput {
        track_title: body.track.clone(),
        artist_name: body.artist.clone(),
        featured_artists: body.featured_artists.clone(),
        album_title: body.album.clone(),
        played_at: body.played_at,
        duration_ms: body
            .duration_ms
            .and_then(|ms| plausible_duration_ms(ms.into())),
        listened_ms: body.listened_ms,
        source: client.name.clone(),
        client_id: state.clients.id(&state.db, &client).await,
        recording_mbid: body.recording_mbid,
    };

    // Validation, catalog resolution, dedup, and insertion all live in
    // `ingest_scrobble` so the worker's connected-accounts poller (Spotify)
    // goes through identical logic instead of duplicating it over HTTP.
    let ingested = scrobbles_db::ingest_scrobble(&state.db, auth_user.id, &input).await;
    if ingested.is_err() {
        quota.release(&state, 1).await;
    }
    let scrobble_id = ingested.map_err(|e| match e {
        scrobbles_db::IngestError::Validation(err) => AppError::ScrobbleInvalid(err.to_string()),
        scrobbles_db::IngestError::Duplicate => {
            AppError::ScrobbleInvalid("duplicate scrobble detected".into())
        }
        scrobbles_db::IngestError::Db(err) => AppError::Database(err),
    })?;

    Ok((
        StatusCode::CREATED,
        Json(ScrobbleResponse {
            id: scrobble_id,
            played_at: body.played_at,
        }),
    ))
}

pub fn _scrobble_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Scrobble a track")
        .description("Records a track listen for the authenticated user. Validates scrobble rules (e.g. minimum listen duration), deduplicates the same track within 30 seconds of another of the user's scrobbles, and refuses plays more than 30 days old (import older history instead). Each account may record `SCROBBLER_DAILY_LIMIT` scrobbles (default 3000) per UTC day, counted together with the scrobbler-compatible APIs.")
        .tag("Scrobbling")
        .response::<201, Json<ScrobbleResponse>>()
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
        .response_with::<422, ErrorJson, _>(|r| r.description("Invalid scrobble: failed validation, a duplicate, or more than 30 days old"))
        .response_with::<429, ErrorJson, _>(|r| r.description("The account's daily scrobble limit is reached; retry after midnight UTC"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NowPlayingRequest {
    pub track: String,
    pub artist: String,
    #[serde(default)]
    pub featured_artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<i32>,
    pub source: Option<String>,
    /// As for scrobbles.
    pub recording_mbid: Option<uuid::Uuid>,
}

/// POST /v1/now-playing
pub async fn update_now_playing(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<NowPlayingRequest>,
) -> ApiResult<StatusCode> {
    if body.featured_artists.len() > shared::scrobble::MAX_FEATURED_ARTISTS {
        return Err(AppError::BadRequest(
            "too many featured artists for one track".into(),
        ));
    }
    let source = ClientIdentity::new(
        PROTOCOL_SCROBBLR,
        body.source.as_deref().unwrap_or("extension"),
        false,
    )
    .name;
    set_now_playing(&state, auth_user.id, &body, &source).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Records what the user is playing and pushes it to their live listeners
/// (also for the scrobbler-compatible APIs). The entry expires when the
/// track would end, or after 5 minutes without a length.
pub(crate) async fn set_now_playing(
    state: &AppState,
    user_id: i64,
    playing: &NowPlayingRequest,
    source: &str,
) -> ApiResult<()> {
    let duration_ms = playing
        .duration_ms
        .and_then(|ms| plausible_duration_ms(ms.into()));
    let catalog = tracks_db::resolve(
        &state.db,
        &tracks_db::CatalogInput {
            artist: &playing.artist,
            featured_artists: &playing.featured_artists,
            album: playing.album.as_deref(),
            track: &playing.track,
            duration_ms,
            recording_mbid: playing.recording_mbid,
        },
    )
    .await?;

    let duration_ms = catalog.track.duration_ms.or(duration_ms).unwrap_or(300_000);
    let expires_at = Utc::now() + Duration::milliseconds(duration_ms as i64);

    scrobbles_db::upsert_now_playing(
        &state.db,
        &scrobbles_db::UpsertNowPlaying {
            user_id,
            track_id: catalog.track.id,
            artist_id: catalog.artist_id,
            album_id: catalog.album_id,
            source: source.to_string(),
            expires_at,
        },
    )
    .await?;

    // Live streams are a nicety: the update is stored either way.
    if let Some(rich) = scrobbles_db::get_now_playing(&state.db, user_id).await? {
        let payload =
            serde_json::to_string(&rich).map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
        if let Err(e) = state
            .redis
            .publish::<(), _, _>(crate::live::channel(user_id), payload)
            .await
        {
            tracing::warn!("failed to publish now playing: {e}");
        }
    }

    Ok(())
}

pub fn _update_now_playing_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Update now playing")
        .description("Sets the track currently being played by the authenticated user. The state expires automatically when the track duration elapses. Broadcasts the update to all SSE subscribers in real time via Redis.")
        .tag("Scrobbling")
        .response_with::<204, (), _>(|r| r.description("Now playing updated"))
        .response_with::<401, ErrorJson, _>(|r| r.description("Not authenticated"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecentQuery {
    pub limit: Option<i64>,
    pub before: Option<chrono::DateTime<Utc>>,
}

/// GET /v1/user/:username/recent
pub async fn recent_scrobbles(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    Query(q): Query<RecentQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    let limit = super::page_limit(q.limit, 50, 200);
    let scrobbles = scrobbles_db::get_recent_scrobbles(&state.db, user.id, limit, q.before).await?;

    Ok(Json(scrobbles))
}

pub fn _recent_scrobbles_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get recent scrobbles")
        .description("Returns the most recent scrobbles for a user, ordered by `played_at` descending. Maximum 200 per request. Supports cursor-based pagination via the `before` timestamp. Returns 403 for private profiles.")
        .tag("Scrobbles")
        .response_with::<200, Json<Vec<ScrobbleRich>>, _>(|r| r.description("Recent scrobbles, newest first"))
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| r.description("User not found"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PeriodQuery {
    /// "7days" | "1month" | "3months" | "6months" | "1year" | "overall"
    pub period: Option<String>,
    pub limit: Option<i64>,
}

fn period_to_since(period: &str) -> chrono::DateTime<Utc> {
    let now = Utc::now();
    match period {
        "7days" => now - Duration::days(7),
        "1month" => now - Duration::days(30),
        "3months" => now - Duration::days(90),
        "6months" => now - Duration::days(180),
        "1year" => now - Duration::days(365),
        _ => chrono::DateTime::from_timestamp(0, 0).unwrap_or(now), // overall
    }
}

/// GET /v1/user/:username/top-artists
pub async fn top_artists(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    Query(q): Query<PeriodQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    let since = period_to_since(q.period.as_deref().unwrap_or("overall"));
    let limit = super::page_limit(q.limit, 10, 50);

    let artists = scrobbles_db::get_top_artists(&state.db, user.id, since, limit).await?;
    Ok(Json(artists))
}

pub fn _top_artists_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get top artists")
        .description("Returns the most scrobbled artists for a user in a given period. Supported periods: `7days`, `1month`, `3months`, `6months`, `1year`, `overall` (default). Maximum 50 results.")
        .tag("Scrobbles")
        .response_with::<200, Json<Vec<TopArtist>>, _>(|r| r.description("Ranked list of top artists with scrobble counts"))
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| r.description("User not found"))
}

/// GET /v1/user/:username/top-tracks
pub async fn top_tracks(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    Query(q): Query<PeriodQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    let since = period_to_since(q.period.as_deref().unwrap_or("overall"));
    let limit = super::page_limit(q.limit, 10, 50);

    let tracks = scrobbles_db::get_top_tracks(&state.db, user.id, since, limit).await?;
    Ok(Json(tracks))
}

pub fn _top_tracks_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get top tracks")
        .description("Returns the most scrobbled tracks for a user in a given period. Supported periods: `7days`, `1month`, `3months`, `6months`, `1year`, `overall` (default). Maximum 50 results.")
        .tag("Scrobbles")
        .response_with::<200, Json<Vec<TopTrack>>, _>(|r| r.description("Ranked list of top tracks with scrobble counts"))
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| r.description("User not found"))
}

/// GET /v1/user/:username/heatmap
pub async fn activity_heatmap(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    let since = Utc::now() - Duration::days(365);
    let days = scrobbles_db::get_activity_heatmap(&state.db, user.id, since).await?;
    Ok(Json(days))
}

pub fn _activity_heatmap_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get activity heatmap")
        .description("Returns daily scrobble counts for the past 365 days, suitable for rendering a GitHub-style activity heatmap. Each entry contains a day (UTC midnight) and a scrobble count; days without scrobbles are left out.")
        .tag("Scrobbles")
        .response_with::<200, Json<Vec<ActivityDay>>, _>(|r| r.description("Days with scrobbles in the past year, oldest first"))
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| r.description("User not found"))
}

/// Documents a 200 declared as JSON as the event stream that carries it.
fn event_stream(mut op: TransformOperation) -> TransformOperation {
    if let Some(responses) = &mut op.inner_mut().responses
        && let Some(aide::openapi::ReferenceOr::Item(ok)) = responses
            .responses
            .get_mut(&aide::openapi::StatusCode::Code(200))
        && let Some(json) = ok.content.shift_remove("application/json")
    {
        ok.content.insert("text/event-stream".into(), json);
    }
    op
}

/// How often an idle live stream sends a comment, well within the idle
/// timeouts of proxies on the way (Cloudflare's is 100 s).
const KEEP_ALIVE: std::time::Duration = std::time::Duration::from_secs(15);

pub struct SseStream<S>(Sse<S>);

impl<S> IntoResponse for SseStream<S>
where
    Sse<S>: IntoResponse,
{
    fn into_response(self) -> Response {
        // nginx buffers a response unless told not to.
        ([("x-accel-buffering", "no")], self.0).into_response()
    }
}

impl<S> OperationOutput for SseStream<S> {
    type Inner = ();
}

pub async fn live_now_playing(
    State(state): State<AppState>,
    Path(UsernamePath { username }): Path<UsernamePath>,
    ClientIp(ip): ClientIp,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<SseStream<impl Stream<Item = Result<Event, Infallible>>>> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    // Listening first, so no update falls between it and the current state.
    let updates = state
        .live
        .listen(user.id, &ip, viewer_id)
        .ok_or(AppError::RateLimited)?;
    let current = match scrobbles_db::get_now_playing(&state.db, user.id).await? {
        Some(rich) => Event::default()
            .json_data(&rich)
            .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?,
        None => Event::default().data("null"),
    };
    let stream = stream::once(async { Ok(current) }).chain(stream::unfold(
        updates,
        |mut updates| async move {
            let payload = updates.recv().await?;
            Some((Ok(Event::default().data(payload)), updates))
        },
    ));
    Ok(SseStream(
        Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEP_ALIVE)),
    ))
}

pub fn _live_now_playing_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Live now playing (SSE)")
        .description(&format!(
            "Server-Sent Events stream of a user's now playing. The first event is the \
                 current state (`null` when nothing is playing), then one per update, each a JSON \
                 `NowPlayingRich`. An entry that expires sends no event: compare `expires_at` with \
                 the clock. An idle stream sends a comment every {} seconds, and tells proxies \
                 not to buffer it (`X-Accel-Buffering: no`).",
            KEEP_ALIVE.as_secs()
        ))
        .tag("Scrobbles")
        .response_with::<200, Json<Option<NowPlayingRich>>, _>(|r| {
            r.description("`text/event-stream`; each event's data is the JSON shown")
        })
        .with(event_stream)
        .response_with::<403, ErrorJson, _>(|r| r.description("Profile is private"))
        .response_with::<404, ErrorJson, _>(|r| r.description("User not found"))
        .response_with::<429, ErrorJson, _>(|r| {
            r.description(&format!(
                "This address already holds {} streams open, or this signed-in viewer {}; \
                 or over 60 requests a minute from this address",
                crate::live::MAX_STREAMS_PER_ADDRESS,
                crate::live::MAX_STREAMS_PER_VIEWER
            ))
        })
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use chrono::{TimeDelta, Utc};
    use serde_json::json;

    use crate::compat::CompatConfig;
    use crate::test_app::with_app;

    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn native_scrobbles_are_bounded_in_age_and_per_day() {
        let config = CompatConfig {
            daily_limit: 2,
            ..Default::default()
        };
        with_app(config, |app| async move {
            let scrobble = async |track: &str, ago: TimeDelta| {
                let play = json!({
                    "track": track,
                    "artist": "Someone",
                    "played_at": (Utc::now() - ago).to_rfc3339(),
                });
                app.api(Method::POST, "/v1/scrobble", Some(play)).await.0
            };
            let old = scrobble("Old", TimeDelta::days(31)).await;
            assert_eq!(old, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(
                scrobble("One", TimeDelta::minutes(9)).await,
                StatusCode::CREATED
            );
            // A duplicate takes nothing from the allowance.
            let again = scrobble("One", TimeDelta::minutes(9)).await;
            assert_eq!(again, StatusCode::UNPROCESSABLE_ENTITY);
            let late = scrobble("Two", TimeDelta::days(29)).await;
            assert_eq!(late, StatusCode::CREATED);
            let over = scrobble("Three", TimeDelta::minutes(1)).await;
            assert_eq!(over, StatusCode::TOO_MANY_REQUESTS);
        })
        .await;
    }
}
