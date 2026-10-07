//! ListenBrainz's `POST /1/submit-listens` and `GET /1/validate-token`,
//! with a scrobbler token as the user token. Web Scrobbler and Pano
//! Scrobbler, among others, can send their ListenBrainz scrobbles to a
//! custom server.

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::DateTime;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Ignored, Params, Play, Playing};
use crate::state::AppState;
use db::queries::scrobble_clients::{ClientIdentity, PROTOCOL_LISTENBRAINZ};
use db::queries::scrobblers::Credential;

const MAX_IMPORT: usize = 1000;
/// ListenBrainz rejects earlier listens.
const LISTEN_MINIMUM_TS: i64 = 1_033_430_400;

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({ "code": status.as_u16(), "error": message })),
    )
        .into_response()
}

fn unavailable(e: impl std::fmt::Display) -> Response {
    tracing::error!("ListenBrainz API: {e}");
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "The service is temporarily unavailable, please try again.",
    )
}

/// `Authorization: Token …` (any case), or the deprecated `?token=`.
fn token(headers: &HeaderMap, params: &Params) -> Option<String> {
    let from_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("token"))
        .map(|(_, token)| token.trim().to_string());
    from_header.or_else(|| params.text("token").map(str::to_string))
}

/// GET /1/validate-token
pub async fn validate_token(State(state): State<AppState>, req: Request) -> Response {
    let params = Params::parse(req.uri().query(), &[]);
    let Some(token) = token(req.headers(), &params) else {
        return error(
            StatusCode::BAD_REQUEST,
            "You need to provide an Authorization token.",
        );
    };
    match super::authenticate(&state, &token).await {
        Ok(Some(credential)) => Json(json!({
            "code": 200,
            "message": "Token valid.",
            "valid": true,
            "user_name": credential.username,
        }))
        .into_response(),
        Ok(None) => Json(json!({ "code": 200, "message": "Token invalid.", "valid": false }))
            .into_response(),
        Err(e) => unavailable(e),
    }
}

#[derive(Deserialize)]
struct Submission {
    listen_type: String,
    #[serde(default)]
    payload: Vec<Listen>,
}

#[derive(Deserialize)]
struct Listen {
    listened_at: Option<Value>,
    #[serde(default)]
    track_metadata: TrackMetadata,
}

#[derive(Deserialize, Default)]
struct TrackMetadata {
    artist_name: Option<String>,
    track_name: Option<String>,
    release_name: Option<String>,
    #[serde(default)]
    additional_info: Map<String, Value>,
}

/// An integer, or a string holding one (some clients send those).
fn integer(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl TrackMetadata {
    fn info(&self, name: &str) -> Option<&str> {
        self.additional_info
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    }

    /// `duration_ms`, or `duration` in seconds (Web Scrobbler).
    fn duration_ms(&self) -> Option<i32> {
        integer(self.additional_info.get("duration_ms"))
            .or_else(|| {
                integer(self.additional_info.get("duration")).map(|s| s.saturating_mul(1000))
            })
            .and_then(super::plausible_duration_ms)
    }

    /// Who sent it: the submitting client, else the player.
    fn client(&self) -> String {
        let named = |name: &str, version: &str| {
            self.info(name)
                .map(|n| format!("{n} {}", self.info(version).unwrap_or_default()))
        };
        named("submission_client", "submission_client_version")
            .or_else(|| named("media_player", "media_player_version"))
            .unwrap_or_else(|| "unknown".into())
    }
}

/// POST /1/submit-listens
pub async fn submit_listens(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = match super::read_request(req).await {
        Ok(read) => read,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Payload too large."),
    };
    let params = Params::parse(parts.uri.query(), &[]);
    let credential = match token(&parts.headers, &params) {
        Some(token) => match super::authenticate(&state, &token).await {
            Ok(Some(credential)) => credential,
            Ok(None) => {
                return error(StatusCode::UNAUTHORIZED, "Invalid authorization token.");
            }
            Err(e) => return unavailable(e),
        },
        None => {
            return error(
                StatusCode::UNAUTHORIZED,
                "You need to provide an Authorization header.",
            );
        }
    };
    if !super::within_request_limit(&state, credential.user_id).await {
        return error(StatusCode::TOO_MANY_REQUESTS, "Too many requests.");
    }
    let submission: Submission = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {e}")),
    };
    accept(&state, &credential, submission).await
}

async fn accept(state: &AppState, credential: &Credential, s: Submission) -> Response {
    let bad = |message: &str| error(StatusCode::BAD_REQUEST, message);
    let (single, now) = match s.listen_type.as_str() {
        "single" => (true, false),
        "playing_now" => (true, true),
        "import" => (false, false),
        _ => return bad("listen_type must be single, playing_now or import."),
    };
    if s.payload.is_empty() {
        return bad("The payload is empty.");
    }
    if single && s.payload.len() > 1 {
        return bad("A single or playing_now submission has exactly one listen.");
    }
    if s.payload.len() > MAX_IMPORT {
        return bad("Too many listens in one request.");
    }
    for listen in &s.payload {
        let meta = &listen.track_metadata;
        let named = |n: &Option<String>| n.as_deref().is_some_and(|n| !n.trim().is_empty());
        if !named(&meta.artist_name) || !named(&meta.track_name) {
            return bad("Every listen needs an artist_name and a track_name.");
        }
        if !now && integer(listen.listened_at.as_ref()).is_none_or(|t| t < LISTEN_MINIMUM_TS) {
            return bad("Every listen needs a valid listened_at.");
        }
    }

    let ok = || Json(json!({ "status": "ok" })).into_response();
    let client = ClientIdentity::new(
        PROTOCOL_LISTENBRAINZ,
        &s.payload[0].track_metadata.client(),
        false,
    );
    if now {
        let meta = &s.payload[0].track_metadata;
        let playing = Playing {
            artist: meta.artist_name.clone().unwrap_or_default(),
            track: meta.track_name.clone().unwrap_or_default(),
            album: meta.release_name.clone(),
            duration_ms: meta.duration_ms(),
        };
        return match super::now_playing(state, credential, &client, &playing).await {
            Ok(_) => ok(),
            Err(e) => unavailable(e),
        };
    }

    let plays: Vec<Play> = s
        .payload
        .iter()
        .map(|listen| {
            let meta = &listen.track_metadata;
            Play {
                artist: meta.artist_name.clone().unwrap_or_default(),
                track: meta.track_name.clone().unwrap_or_default(),
                album: meta.release_name.clone(),
                played_at: integer(listen.listened_at.as_ref())
                    .and_then(|t| DateTime::from_timestamp(t, 0))
                    .unwrap_or(DateTime::UNIX_EPOCH),
                duration_ms: meta.duration_ms(),
            }
        })
        .collect();
    match super::submit(state, credential, &client, &plays).await {
        // Over the daily limit the client should retry later. Listens too
        // old or too far ahead are dropped quietly: ListenBrainz has no way
        // to report them one by one.
        Ok(outcomes) if outcomes.contains(&Err(Ignored::DailyLimit)) => error(
            StatusCode::TOO_MANY_REQUESTS,
            "Daily scrobble limit reached, try again later.",
        ),
        Ok(_) => ok(),
        Err(e) => unavailable(e),
    }
}
