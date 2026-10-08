//! Audioscrobbler 1.2 (Last.fm's old submissions protocol), still spoken by
//! players such as mpdscribe, Quod Libet and DeaDBeeF: a GET handshake at
//! the URL the user configures (`/` or `/1.2/`), then form POSTs to the
//! now-playing and submission URLs it hands out. Answers are plain text.
//!
//! The standard handshake proves knowledge of md5(password), which an
//! Argon2 hash can't check, so the password must be a scrobbler token,
//! whose md5 the server keeps encrypted. The web-services variant
//! (`api_key` + `sk`) takes a Last.fm session key instead.

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use fred::interfaces::KeysInterface;
use fred::types::{Expiration, SetOptions};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use super::{Ignored, Params, Play, Playing};
use crate::errors::ApiResult;
use crate::state::AppState;
use db::queries::scrobble_clients::{ClientIdentity, PROTOCOL_AUDIOSCROBBLER};
use db::queries::scrobblers::{self as scrobblers_db, Credential};
use shared::lastfm::md5_hex;

/// How far the handshake's timestamp may be from the server's clock; its
/// auth token can't be replayed outside this window, nor inside it twice.
const MAX_SKEW_SECS: i64 = 300;
const SESSION_TTL_SECS: i64 = 24 * 3600;
const MAX_SUBMISSIONS: usize = 50;

#[derive(Serialize, Deserialize)]
struct Session {
    credential_id: Uuid,
    client: String,
}

fn text(body: &str) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("{body}\n"),
    )
        .into_response()
}

fn failed(e: impl std::fmt::Display) -> Response {
    tracing::error!("Audioscrobbler API: {e}");
    text("FAILED Temporary server error")
}

/// GET /?hs=true&p=1.2.1&c=…&v=…&u=…&t=…&a=…
pub async fn handshake(State(state): State<AppState>, req: Request) -> Response {
    let (parts, _) = match super::read_request(req).await {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let params = Params::parse(parts.uri.query(), &[]);
    if params.get("hs") != Some("true") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ip = super::request_ip(&state, &parts);
    match handshake_reply(&state, &params, &ip).await {
        Ok(reply) => text(&reply),
        Err(e) => failed(e),
    }
}

async fn handshake_reply(state: &AppState, params: &Params, ip: &str) -> ApiResult<String> {
    if !params.get("p").is_some_and(|p| p.starts_with("1.2")) {
        return Ok("FAILED Unsupported protocol version".into());
    }
    let (Some(username), Some(t), Some(auth)) =
        (params.text("u"), params.text("t"), params.text("a"))
    else {
        return Ok("FAILED Missing parameters".into());
    };
    let skewed = t
        .parse::<i64>()
        .map_or(true, |t| (Utc::now().timestamp() - t).abs() > MAX_SKEW_SECS);
    if skewed {
        return Ok("BADTIME".into());
    }
    let client = format!(
        "{} {}",
        params.text("c").unwrap_or("unknown"),
        params.text("v").unwrap_or_default()
    );

    let credential = match (params.text("api_key"), params.text("sk")) {
        // Web-services authentication: the session key is the credential.
        (Some(_), Some(sk)) => super::authenticate(state, sk)
            .await?
            .filter(|c| c.username.eq_ignore_ascii_case(username)),
        _ => {
            if !crate::limits::login_attempt(state, ip, username).await? {
                return Ok("FAILED Too many login attempts, try again later".into());
            }
            let found = token_for(state, username, t, auth).await?;
            if found.is_none() {
                return Ok("BADAUTH".into());
            }
            if !first_use(state, auth).await? {
                return Ok("BADAUTH".into());
            }
            crate::limits::login_succeeded(state, username).await;
            found
        }
    };
    let Some(credential) = credential else {
        return Ok("BADAUTH".into());
    };

    let id = super::new_secret();
    let session = serde_json::to_string(&Session {
        credential_id: credential.id,
        client,
    })
    .map_err(anyhow::Error::from)?;
    let _: () = state
        .redis
        .set(
            format!("as12:session:{id}"),
            session,
            Some(Expiration::EX(SESSION_TTL_SECS)),
            None,
            false,
        )
        .await?;
    let base = &state.uploads.public_base_url;
    Ok(format!(
        "OK\n{id}\n{base}/1.2/nowplaying\n{base}/1.2/submissions"
    ))
}

/// The user's token whose md5, followed by `t`, hashes to `auth`. Every
/// candidate is checked, so the time taken doesn't say which matched.
async fn token_for(
    state: &AppState,
    username: &str,
    t: &str,
    auth: &str,
) -> ApiResult<Option<Credential>> {
    let auth = auth.to_ascii_lowercase();
    let mut found = None;
    for candidate in scrobblers_db::legacy_candidates(&state.db, username).await? {
        let Ok(token_md5) = shared::crypto::decrypt(&candidate.legacy_secret) else {
            continue;
        };
        let expected = md5_hex(&format!("{token_md5}{t}"));
        if bool::from(expected.as_bytes().ct_eq(auth.as_bytes())) {
            found = Some(candidate.credential);
        }
    }
    Ok(found)
}

/// Burns a handshake's auth token, so a captured one can't be replayed.
/// Fails closed, like the app-signature nonces.
async fn first_use(state: &AppState, auth: &str) -> ApiResult<bool> {
    let previous: Option<String> = state
        .redis
        .set(
            format!("as12:handshake:{}", auth.to_ascii_lowercase()),
            1,
            Some(Expiration::EX(MAX_SKEW_SECS * 2)),
            Some(SetOptions::NX),
            true,
        )
        .await?;
    Ok(previous.is_none())
}

/// The session `s` names, while its credential exists: revoking the
/// credential ends the sessions it opened.
async fn session(state: &AppState, params: &Params) -> ApiResult<Option<(Credential, String)>> {
    let Some(id) = params.text("s") else {
        return Ok(None);
    };
    let key = format!("as12:session:{id}");
    let stored: Option<String> = state.redis.get(&key).await?;
    let Some(session) = stored.and_then(|s| serde_json::from_str::<Session>(&s).ok()) else {
        return Ok(None);
    };
    let Some(credential) =
        scrobblers_db::find_credential_by_id(&state.db, session.credential_id).await?
    else {
        return Ok(None);
    };
    let _ = state
        .redis
        .expire::<i64, _>(&key, SESSION_TTL_SECS, None)
        .await;
    Ok(Some((credential, session.client)))
}

fn seconds(value: Option<&str>) -> Option<i32> {
    value
        .and_then(|v| v.trim().parse::<i64>().ok())
        .and_then(|s| shared::scrobble::plausible_duration_ms(s.saturating_mul(1000)))
}

async fn form(req: Request) -> ApiResult<Params> {
    let (parts, body) = super::read_request(req).await?;
    Ok(Params::parse(parts.uri.query(), &body))
}

/// POST /1.2/nowplaying: s, a, t, b, l, n, m
pub async fn now_playing(State(state): State<AppState>, req: Request) -> Response {
    let reply = async {
        let params = form(req).await?;
        let Some((credential, client)) = session(&state, &params).await? else {
            return Ok("BADSESSION");
        };
        if !super::within_request_limit(&state, credential.user_id).await {
            return Ok("FAILED Too many requests, slow down");
        }
        let client = ClientIdentity::new(PROTOCOL_AUDIOSCROBBLER, &client, false);
        let playing = Playing {
            artist: params.get("a").unwrap_or_default().to_string(),
            track: params.get("t").unwrap_or_default().to_string(),
            album: params.get("b").map(str::to_string),
            duration_ms: seconds(params.get("l")),
        };
        // Unusable names are dropped quietly, as the protocol allows.
        let _ignored = super::now_playing(&state, &credential, &client, &playing).await?;
        ApiResult::Ok("OK")
    };
    match reply.await {
        Ok(reply) => text(reply),
        Err(e) => failed(e),
    }
}

/// The submission's tracks, in index order, minus those rated as skipped
/// or banned (Last.fm radio's ratings, which mean it wasn't listened to).
fn plays(params: &Params) -> Result<Vec<Play>, &'static str> {
    let mut indices: Vec<usize> = params
        .pairs()
        .filter_map(|(name, _)| {
            name.strip_prefix("a[")
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(|i| i.parse().ok())
        })
        .filter(|i| *i < 1000)
        .collect();
    indices.sort_unstable();
    indices.dedup();
    if indices.len() > MAX_SUBMISSIONS {
        return Err("FAILED Too many tracks in one submission");
    }
    Ok(indices
        .into_iter()
        .filter_map(|i| {
            let field = |name: &str| params.get(&format!("{name}[{i}]"));
            if matches!(field("r").map(str::trim), Some("B" | "S")) {
                return None;
            }
            let played_at = field("i")
                .and_then(|t| t.trim().parse::<i64>().ok())
                .and_then(|t| DateTime::from_timestamp(t, 0))?;
            Some(Play {
                artist: field("a").unwrap_or_default().to_string(),
                track: field("t").unwrap_or_default().to_string(),
                album: field("b").map(str::to_string),
                played_at,
                duration_ms: seconds(field("l")),
            })
        })
        .collect())
}

/// POST /1.2/submissions: s, then a[i], t[i], i[i], o[i], r[i], l[i], b[i],
/// n[i], m[i] for up to 50 tracks.
pub async fn submissions(State(state): State<AppState>, req: Request) -> Response {
    let reply = async {
        let params = form(req).await?;
        let Some((credential, client)) = session(&state, &params).await? else {
            return Ok("BADSESSION");
        };
        if !super::within_request_limit(&state, credential.user_id).await {
            return Ok("FAILED Too many requests, slow down");
        }
        let plays = match plays(&params) {
            Ok(plays) => plays,
            Err(reply) => return Ok(reply),
        };
        let client = ClientIdentity::new(PROTOCOL_AUDIOSCROBBLER, &client, false);
        let outcomes = super::submit(&state, &credential, &client, &plays).await?;
        // Only the daily limit is worth a retry (tomorrow); the protocol lets
        // a server drop plays it won't record, such as malformed ones.
        if outcomes.contains(&Err(Ignored::DailyLimit)) {
            return Ok("FAILED Daily scrobble limit reached, try again later");
        }
        ApiResult::Ok("OK")
    };
    match reply.await {
        Ok(reply) => text(reply),
        Err(e) => failed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Params {
        let body: String = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        Params::parse(None, body.as_bytes())
    }

    #[test]
    fn submissions_parse_in_order_without_skips_and_bans() {
        let p = params(&[
            ("s", "x"),
            ("a[1]", "B"),
            ("t[1]", "Two"),
            ("i[1]", "1700000300"),
            ("o[1]", "P"),
            ("l[1]", "200"),
            ("a[0]", "A"),
            ("t[0]", "One"),
            ("i[0]", "1700000000"),
            ("o[0]", "P"),
            ("a[2]", "C"),
            ("t[2]", "Skipped"),
            ("i[2]", "1700000600"),
            ("o[2]", "L1b48a"),
            ("r[2]", "S"),
            ("a[3]", "D"),
            ("t[3]", "No time"),
        ]);
        let plays = plays(&p).unwrap();
        assert_eq!(
            plays.iter().map(|p| p.track.as_str()).collect::<Vec<_>>(),
            ["One", "Two"]
        );
        assert_eq!(plays[1].duration_ms, Some(200_000));
        assert_eq!(plays[0].duration_ms, None);
    }
}
