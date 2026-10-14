//! ListenBrainz's `POST /1/submit-listens`, `GET /1/validate-token` and
//! the feedback API (loves), with a scrobbler token as the user token. Web
//! Scrobbler and Pano Scrobbler, among others, can send their ListenBrainz
//! scrobbles to a custom server.
//!
//! A recording MSID this server hands out names a catalog track: a
//! version-8 UUID holding the track id after a fixed prefix, so nothing is
//! stored for it and no MSID from listenbrainz.org is taken for one.

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::DateTime;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{Ignored, Params, Play, Playing};
use crate::state::AppState;
use db::queries::loves::{
    self as loves_db, LoveSource, Loved, MAX_LOVED_TRACKS, SOURCE_LISTENBRAINZ,
};
use db::queries::scrobble_clients::{ClientIdentity, PROTOCOL_LISTENBRAINZ};
use db::queries::scrobblers::Credential;
use db::queries::{scrobbles as scrobbles_db, tracks as tracks_db};

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

const MSID_HIGH: u64 = 0x5c40_bb1e_7e57_8001;
const MSID_VARIANT: u64 = 0b10 << 62;

pub fn msid(track_id: i64) -> Uuid {
    Uuid::from_u64_pair(MSID_HIGH, MSID_VARIANT | track_id as u64)
}

/// The track an MSID of ours names; `None` for anyone else's.
pub fn msid_track(msid: Uuid) -> Option<i64> {
    let (high, low) = msid.as_u64_pair();
    (high == MSID_HIGH && low >> 62 == 0b10).then_some((low & !(0b11 << 62)) as i64)
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
            .and_then(shared::scrobble::plausible_duration_ms)
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
    let credential = match authorize(&state, &parts.headers, &params).await {
        Ok(credential) => credential,
        Err(response) => return *response,
    };
    let submission: Submission = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {e}")),
    };
    // As ListenBrainz: any value but an empty one asks for it.
    let return_msid = params.get("return_msid").is_some_and(|v| !v.is_empty());
    accept(&state, &credential, submission, return_msid).await
}

/// The credential a request's token names: 401 as ListenBrainz answers
/// without one, and within the per-user request limit.
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    params: &Params,
) -> Result<Credential, Box<Response>> {
    let credential = match token(headers, params) {
        Some(token) => match super::authenticate(state, &token).await {
            Ok(Some(credential)) => credential,
            Ok(None) => {
                return Err(Box::new(error(
                    StatusCode::UNAUTHORIZED,
                    "Invalid authorization token.",
                )));
            }
            Err(e) => return Err(Box::new(unavailable(e))),
        },
        None => {
            return Err(Box::new(error(
                StatusCode::UNAUTHORIZED,
                "You need to provide an Authorization header.",
            )));
        }
    };
    if !super::within_request_limit(state, credential.user_id).await {
        return Err(Box::new(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests.",
        )));
    }
    Ok(credential)
}

async fn accept(
    state: &AppState,
    credential: &Credential,
    s: Submission,
    return_msid: bool,
) -> Response {
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
            recording_mbid: super::mbid(meta.info("recording_mbid")),
        };
        if let Err(e) = super::now_playing(state, credential, &client, &playing).await {
            return unavailable(e);
        }
        // For loving what is playing (Pano Scrobbler keeps it for that).
        if return_msid {
            match scrobbles_db::get_now_playing(&state.db, credential.user_id).await {
                Ok(Some(playing)) => {
                    return Json(
                        json!({ "status": "ok", "recording_msid": msid(playing.track_id) }),
                    )
                    .into_response();
                }
                Ok(None) => {}
                Err(e) => return unavailable(e),
            }
        }
        return ok();
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
                recording_mbid: super::mbid(meta.info("recording_mbid")),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Feedback {
    recording_msid: Option<String>,
    recording_mbid: Option<String>,
    score: Value,
}

/// A recording a feedback request named, parsed.
struct Recording {
    mbid: Option<Uuid>,
    msid: Option<Uuid>,
}

fn uuid(value: Option<&str>, name: &str) -> Result<Option<Uuid>, String> {
    value
        .map(|v| Uuid::parse_str(v.trim()).map_err(|_| format!("{name} is not a valid UUID.")))
        .transpose()
}

/// The catalog track a recording names: the MBID's, else our MSID's.
async fn recording_track(
    state: &AppState,
    recording: &Recording,
) -> Result<Option<i64>, sqlx::Error> {
    if let Some(mbid) = recording.mbid
        && let Some((_, id)) = tracks_db::find_by_recording_mbids(&state.db, &[mbid])
            .await?
            .first()
    {
        return Ok(Some(*id));
    }
    match recording.msid.and_then(msid_track) {
        Some(id) => Ok(tracks_db::find_track_by_id(&state.db, id)
            .await?
            .map(|t| t.id)),
        None => Ok(None),
    }
}

/// POST /1/feedback/recording-feedback: score 1 loves the recording's
/// track, 0 takes the love back; -1 (hate) takes it back too, as a
/// recording has one score on ListenBrainz, and isn't stored. Counted
/// against the love limits whatever the score.
pub async fn recording_feedback(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = match super::read_request(req).await {
        Ok(read) => read,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Payload too large."),
    };
    let params = Params::parse(parts.uri.query(), &[]);
    let credential = match authorize(&state, &parts.headers, &params).await {
        Ok(credential) => credential,
        Err(response) => return *response,
    };
    let bad = |message: &str| error(StatusCode::BAD_REQUEST, message);
    let feedback: Feedback = match serde_json::from_slice(&body) {
        Ok(f) => f,
        Err(e) => {
            return bad(&format!(
                "JSON document must contain recording_msid or recording_mbid, and score, and nothing else: {e}"
            ));
        }
    };
    let score = match feedback.score.as_i64() {
        Some(score @ -1..=1) => score,
        _ => return bad("Score can have a value of 1, 0 or -1."),
    };
    let recording = match (
        uuid(feedback.recording_mbid.as_deref(), "recording_mbid"),
        uuid(feedback.recording_msid.as_deref(), "recording_msid"),
    ) {
        (Ok(mbid), Ok(msid)) if mbid.is_some() || msid.is_some() => Recording { mbid, msid },
        (Err(message), _) | (_, Err(message)) => return bad(&message),
        _ => return bad("JSON document must contain either recording_msid or recording_mbid."),
    };

    let ip = super::request_ip(&state, &parts);
    if !crate::limits::love_attempt(&state, credential.user_id, &ip).await {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many loves and unloves, try again later.",
        );
    }
    let track = match recording_track(&state, &recording).await {
        Ok(track) => track,
        Err(e) => return unavailable(e),
    };
    let ok = || Json(json!({ "status": "ok" })).into_response();
    let Some(track_id) = track else {
        return match score {
            1 => error(
                StatusCode::NOT_FOUND,
                "No track in this server's catalog has that recording; scrobble it first.",
            ),
            _ => ok(),
        };
    };
    if score != 1 {
        return match loves_db::unlove(&state.db, credential.user_id, track_id).await {
            Ok(_) => ok(),
            Err(e) => unavailable(e),
        };
    }
    let from = LoveSource {
        source: SOURCE_LISTENBRAINZ,
        client_id: None,
    };
    match loves_db::love(
        &state.db,
        credential.user_id,
        track_id,
        from,
        MAX_LOVED_TRACKS,
    )
    .await
    {
        Ok(Loved::Now(_) | Loved::Already(_)) => ok(),
        Ok(Loved::AtLimit) => error(
            StatusCode::CONFLICT,
            &format!("You already love {MAX_LOVED_TRACKS} tracks, the most allowed."),
        ),
        Err(e) => unavailable(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_msids_name_tracks_and_no_one_elses_do() {
        for id in [1, 42, 7_000_000_000, (1 << 62) - 1] {
            let m = msid(id);
            assert_eq!(m.get_version_num(), 8);
            assert_eq!(m.get_variant(), uuid::Variant::RFC4122);
            assert_eq!(msid_track(m), Some(id));
        }
        assert_eq!(msid(1).to_string(), "5c40bb1e-7e57-8001-8000-000000000001");
        for other in [
            Uuid::nil(),
            Uuid::new_v4(),
            Uuid::from_u64_pair(MSID_HIGH, 1),
        ] {
            assert_eq!(msid_track(other), None, "{other}");
        }
    }
}
