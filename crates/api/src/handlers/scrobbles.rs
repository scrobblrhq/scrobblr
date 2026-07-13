use crate::{
    errors::{ApiResult, AppError},
    middleware::auth::AuthUser,
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
use chrono::{Duration, Utc};
use db::queries::{
    enrichment as enrichment_db, scrobbles as scrobbles_db, tracks as tracks_db, users as users_db,
};
use fred::interfaces::{EventInterface, PubsubInterface};
use futures_util::stream::Stream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use shared::scrobble::{self as scrobble_logic, ScrobbleInput};
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScrobbleRequest {
    pub track: String,
    pub artist: String,
    pub album: Option<String>,
    pub played_at: chrono::DateTime<Utc>,
    pub duration_ms: Option<i32>,
    pub listened_ms: Option<i32>,
    pub source: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ScrobbleResponse {
    pub id: i64,
    pub played_at: chrono::DateTime<Utc>,
}

/// POST /v1/scrobble
pub async fn scrobble(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<ScrobbleRequest>,
) -> ApiResult<impl IntoApiResponse> {
    let input = ScrobbleInput {
        track_title: body.track.clone(),
        artist_name: body.artist.clone(),
        album_title: body.album.clone(),
        played_at: body.played_at,
        duration_ms: body.duration_ms,
        listened_ms: body.listened_ms,
        source: body.source.clone().unwrap_or_else(|| "extension".into()),
    };

    // Validate scrobble rules
    scrobble_logic::validate(&input).map_err(|e| AppError::ScrobbleInvalid(e.to_string()))?;

    // Dedup: check against last scrobble
    if let Some(last) = scrobbles_db::get_last_scrobble(&state.db, auth_user.id).await? {
        let track_normalized = scrobble_logic::normalize_name(&body.track);
        // We need the track_id — do a quick look-up before dedup
        // For dedup we compare by title+artist name at the application level
        let last_artist = tracks_db::find_artist_by_id(&state.db, last.artist_id)
            .await?
            .map(|a| scrobble_logic::normalize_name(&a.name))
            .unwrap_or_default();
        let last_track = tracks_db::find_track_by_id(&state.db, last.track_id)
            .await?
            .map(|t| scrobble_logic::normalize_name(&t.title))
            .unwrap_or_default();

        let same_track = last_track == track_normalized
            && last_artist == scrobble_logic::normalize_name(&body.artist);
        let delta_secs = (body.played_at - last.played_at).num_seconds().abs();

        if same_track && delta_secs < 30 {
            return Err(AppError::ScrobbleInvalid(
                "duplicate scrobble detected".into(),
            ));
        }
    }

    // Resolve or create catalog entries
    let artist = tracks_db::find_or_create_artist(&state.db, &body.artist).await?;

    let album_id = if let Some(album_title) = &body.album {
        Some(tracks_db::find_or_create_album(&state.db, artist.id, album_title).await?)
    } else {
        None
    };

    let track = tracks_db::find_or_create_track(
        &state.db,
        artist.id,
        album_id,
        &body.track,
        body.duration_ms,
    )
    .await?;

    // Queue metadata enrichment for newly seen entities. Best-effort: a
    // failure here must never reject the scrobble.
    if let Err(e) =
        enrichment_db::enqueue_for_ingest(&state.db, artist.id, album_id, track.id).await
    {
        tracing::warn!("failed to enqueue enrichment for scrobble: {e}");
    }

    // Insert scrobble
    let scrobble_id = scrobbles_db::insert_scrobble(
        &state.db,
        &scrobbles_db::InsertScrobble {
            user_id: auth_user.id,
            track_id: track.id,
            artist_id: artist.id,
            album_id,
            played_at: body.played_at,
            source: input.source,
            duration_ms: body.duration_ms,
        },
    )
    .await?;

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
        .description("Records a track listen for the authenticated user. Validates scrobble rules (e.g. minimum listen duration) and deduplicates submissions within a 30-second window.")
        .tag("Scrobbling")
        .response::<201, Json<ScrobbleResponse>>()
        .response_with::<400, (), _>(|r| r.description("Invalid scrobble (failed validation or duplicate)"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NowPlayingRequest {
    pub track: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<i32>,
    pub source: Option<String>,
}

/// POST /v1/now-playing
pub async fn update_now_playing(
    State(state): State<AppState>,
    Extension(auth_user): Extension<AuthUser>,
    Json(body): Json<NowPlayingRequest>,
) -> ApiResult<StatusCode> {
    let artist = tracks_db::find_or_create_artist(&state.db, &body.artist).await?;

    let album_id = if let Some(album_title) = &body.album {
        Some(tracks_db::find_or_create_album(&state.db, artist.id, album_title).await?)
    } else {
        None
    };

    let track = tracks_db::find_or_create_track(
        &state.db,
        artist.id,
        album_id,
        &body.track,
        body.duration_ms,
    )
    .await?;

    if let Err(e) =
        enrichment_db::enqueue_for_ingest(&state.db, artist.id, album_id, track.id).await
    {
        tracing::warn!("failed to enqueue enrichment for now-playing: {e}");
    }

    let duration_ms = track.duration_ms.or(body.duration_ms).unwrap_or(300_000); // default 5 min
    let expires_at = Utc::now() + Duration::milliseconds(duration_ms as i64);

    scrobbles_db::upsert_now_playing(
        &state.db,
        &scrobbles_db::UpsertNowPlaying {
            user_id: auth_user.id,
            track_id: track.id,
            artist_id: artist.id,
            album_id,
            source: body.source.unwrap_or_else(|| "extension".into()),
            expires_at,
        },
    )
    .await?;

    // Query rich details to publish
    if let Some(rich) = scrobbles_db::get_now_playing(&state.db, auth_user.id).await? {
        let channel = format!("now_playing:{}", auth_user.id);
        let payload =
            serde_json::to_string(&rich).map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
        let _: () = state
            .redis
            .publish(channel, payload)
            .await
            .map_err(AppError::Redis)?;
    }

    Ok(StatusCode::NO_CONTENT)
}

pub fn _update_now_playing_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Update now playing")
        .description("Sets the track currently being played by the authenticated user. The state expires automatically when the track duration elapses. Broadcasts the update to all SSE subscribers in real time via Redis.")
        .tag("Scrobbling")
        .response_with::<204, (), _>(|r| r.description("Now playing updated"))
        .response_with::<401, (), _>(|r| r.description("Not authenticated"))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecentQuery {
    pub limit: Option<i64>,
    pub before: Option<chrono::DateTime<Utc>>,
}

/// GET /v1/user/:username/recent
pub async fn recent_scrobbles(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(q): Query<RecentQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    let limit = q.limit.unwrap_or(50).min(200);
    let scrobbles = scrobbles_db::get_recent_scrobbles(&state.db, user.id, limit, q.before).await?;

    Ok(Json(scrobbles))
}

pub fn _recent_scrobbles_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get recent scrobbles")
        .description("Returns the most recent scrobbles for a user, ordered by `played_at` descending. Maximum 200 per request. Supports cursor-based pagination via the `before` timestamp. Returns 403 for private profiles.")
        .tag("Scrobbles")
        .response_with::<200, (), _>(|r| r.description("List of recent scrobbles"))
        .response_with::<403, (), _>(|r| r.description("Profile is private"))
        .response_with::<404, (), _>(|r| r.description("User not found"))
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
    Path(username): Path<String>,
    Query(q): Query<PeriodQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    let since = period_to_since(q.period.as_deref().unwrap_or("overall"));
    let limit = q.limit.unwrap_or(10).min(50);

    let artists = scrobbles_db::get_top_artists(&state.db, user.id, since, limit).await?;
    Ok(Json(artists))
}

pub fn _top_artists_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get top artists")
        .description("Returns the most scrobbled artists for a user in a given period. Supported periods: `7days`, `1month`, `3months`, `6months`, `1year`, `overall` (default). Maximum 50 results.")
        .tag("Scrobbles")
        .response_with::<200, (), _>(|r| r.description("Ranked list of top artists with scrobble counts"))
        .response_with::<403, (), _>(|r| r.description("Profile is private"))
        .response_with::<404, (), _>(|r| r.description("User not found"))
}

/// GET /v1/user/:username/top-tracks
pub async fn top_tracks(
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(q): Query<PeriodQuery>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<impl IntoApiResponse> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    let since = period_to_since(q.period.as_deref().unwrap_or("overall"));
    let limit = q.limit.unwrap_or(10).min(50);

    let tracks = scrobbles_db::get_top_tracks(&state.db, user.id, since, limit).await?;
    Ok(Json(tracks))
}

pub fn _top_tracks_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Get top tracks")
        .description("Returns the most scrobbled tracks for a user in a given period. Supported periods: `7days`, `1month`, `3months`, `6months`, `1year`, `overall` (default). Maximum 50 results.")
        .tag("Scrobbles")
        .response_with::<200, (), _>(|r| r.description("Ranked list of top tracks with scrobble counts"))
        .response_with::<403, (), _>(|r| r.description("Profile is private"))
        .response_with::<404, (), _>(|r| r.description("User not found"))
}

/// GET /v1/user/:username/heatmap
pub async fn activity_heatmap(
    State(state): State<AppState>,
    Path(username): Path<String>,
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
        .description("Returns daily scrobble counts for the past 365 days, suitable for rendering a GitHub-style activity heatmap. Each entry contains a date and a listen count.")
        .tag("Scrobbles")
        .response_with::<200, (), _>(|r| r.description("Array of { date, count } objects for the past year"))
        .response_with::<403, (), _>(|r| r.description("Profile is private"))
        .response_with::<404, (), _>(|r| r.description("User not found"))
}

pub struct SseStream<S>(Sse<S>);

impl<S> IntoResponse for SseStream<S>
where
    Sse<S>: IntoResponse,
{
    fn into_response(self) -> Response {
        self.0.into_response()
    }
}

impl<S> OperationOutput for SseStream<S> {
    type Inner = ();
}

pub async fn live_now_playing(
    State(state): State<AppState>,
    Path(username): Path<String>,
    auth_user: Option<Extension<AuthUser>>,
) -> ApiResult<SseStream<impl Stream<Item = Result<Event, Infallible>>>> {
    let user = users_db::find_by_username(&state.db, &username)
        .await?
        .ok_or(AppError::NotFound)?;

    let viewer_id = auth_user.map(|Extension(a)| a.id);
    crate::middleware::visibility::ensure_profile_visible(&state, viewer_id, &user).await?;

    let (tx, rx) = mpsc::channel(10);

    let db_pool = state.db.clone();
    let redis_client = state.redis.clone();
    let user_id = user.id;

    tokio::spawn(async move {
        // 1. Send initial now_playing state from DB
        let initial_np = scrobbles_db::get_now_playing(&db_pool, user_id).await;
        match initial_np {
            Ok(Some(rich)) => {
                if let Ok(event) = Event::default().json_data(&rich)
                    && tx.send(Ok(event)).await.is_err()
                {
                    return; // client disconnected
                }
            }
            Ok(None) => {
                // Send explicit null to clear any playing state
                let event = Event::default().data("null");
                if tx.send(Ok(event)).await.is_err() {
                    return; // client disconnected
                }
            }
            Err(e) => {
                tracing::error!("failed to fetch initial now playing for SSE: {e}");
            }
        }

        // 2. Subscribe to Redis channel
        let channel_name = format!("now_playing:{}", user_id);
        let mut message_rx = redis_client.message_rx();
        if let Err(e) = redis_client.subscribe(&channel_name).await {
            tracing::error!("failed to subscribe to redis channel {channel_name} for SSE: {e}");
            return;
        }

        // 3. Listen to Redis messages and forward to client
        while let Ok(msg) = message_rx.recv().await {
            if msg.channel == channel_name
                && let Ok(value_str) = msg.value.convert::<String>()
            {
                let event = Event::default().data(value_str);
                if tx.send(Ok(event)).await.is_err() {
                    break; // client disconnected
                }
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    Ok(SseStream(Sse::new(stream).keep_alive(KeepAlive::default())))
}

pub fn _live_now_playing_doc(op: TransformOperation) -> TransformOperation {
    op.summary("Live now playing (SSE)")
        .description(
            "Server-Sent Events stream that pushes real-time now-playing updates for a user. \
             Emits the current state immediately on connect, then sends an event on every change. \
             Sends `null` when the user stops listening. \
             The connection is kept alive automatically via SSE keep-alive.",
        )
        .tag("Scrobbles")
        .response_with::<200, (), _>(|r| {
            r.description("SSE stream — content-type: text/event-stream")
        })
        .response_with::<403, (), _>(|r| r.description("Profile is private"))
        .response_with::<404, (), _>(|r| r.description("User not found"))
}
