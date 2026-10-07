//! Last.fm API 2.0 at `/2.0/`: the methods scrobblers call, answering in
//! XML, or JSON with `format=json`, shaped like Last.fm's.
//!
//! A request is verified when signed with a secret the server knows: one
//! configured in `SCROBBLER_API_KEYS`, or the api_key itself, which GNU FM
//! clients such as Pano Scrobbler sign with. A wrong signature for a known
//! key is rejected. Other keys are accepted, recorded as unverified,
//! unless `SCROBBLER_STRICT_API_KEYS` is set. Either way the session key,
//! not the signature, is what authenticates the user.

use std::collections::BTreeSet;

use axum::{
    Json,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use super::{CompatConfig, Ignored, Params, Play, Playing};
use crate::errors::AppError;
use crate::handlers::auth::verify_password_login;
use crate::state::AppState;
use db::queries::scrobble_clients::{ClientIdentity, PROTOCOL_LASTFM};
use db::queries::scrobblers::{self as scrobblers_db, Credential, KIND_SESSION, KIND_TOKEN};
use db::queries::{scrobbles as scrobbles_db, users as users_db};
use shared::lastfm::signature;
use shared::models::User;

const MAX_BATCH: usize = 50;
const AUTH_TOKEN_TTL: TimeDelta = TimeDelta::hours(1);
const SCROBBLE_FIELDS: [&str; 10] = [
    "artist",
    "track",
    "timestamp",
    "album",
    "albumArtist",
    "duration",
    "trackNumber",
    "mbid",
    "chosenByUser",
    "context",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Xml,
    Json,
}

#[derive(Debug)]
struct LfmError {
    code: u16,
    message: String,
}

impl LfmError {
    fn new(code: u16, message: &str) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }

    fn invalid_parameters(detail: &str) -> Self {
        Self::new(6, &format!("Invalid parameters - {detail}"))
    }

    fn invalid_api_key() -> Self {
        Self::new(
            10,
            "Invalid API key - You must be granted a valid key by last.fm",
        )
    }

    fn invalid_session() -> Self {
        Self::new(9, "Invalid session key - Please re-authenticate")
    }

    fn rate_limited() -> Self {
        Self::new(
            29,
            "Rate Limit Exceeded - Your IP has made too many requests in a short period",
        )
    }

    fn status(&self) -> StatusCode {
        match self.code {
            2 | 3 | 5 | 6 | 7 => StatusCode::BAD_REQUEST,
            4 | 9 | 10 | 13 | 14 | 15 | 17 | 26 => StatusCode::FORBIDDEN,
            29 => StatusCode::TOO_MANY_REQUESTS,
            11 | 16 => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<AppError> for LfmError {
    fn from(e: AppError) -> Self {
        match e {
            AppError::RateLimited => Self::rate_limited(),
            AppError::BadRequest(detail) => Self::invalid_parameters(&detail),
            AppError::Database(_) | AppError::Redis(_) | AppError::Internal(_) => {
                tracing::error!("Last.fm API: {e}");
                Self::new(
                    16,
                    "The service is temporarily unavailable, please try again.",
                )
            }
            _ => Self::new(8, "Operation failed - Please try again."),
        }
    }
}

impl From<sqlx::Error> for LfmError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Database(e).into()
    }
}

/// A success body in both formats.
struct Body {
    xml: String,
    json: Value,
}

type LfmResult = Result<Body, LfmError>;

/// Escapes text for XML, dropping characters XML can't carry.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

fn respond(format: Format, result: LfmResult) -> Response {
    let xml = |status: StatusCode, inner: String| {
        (
            status,
            [(header::CONTENT_TYPE, "text/xml; charset=utf-8")],
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{inner}\n"),
        )
            .into_response()
    };
    match (format, result) {
        (Format::Xml, Ok(body)) => xml(
            StatusCode::OK,
            format!("<lfm status=\"ok\">{}</lfm>", body.xml),
        ),
        (Format::Json, Ok(body)) => (StatusCode::OK, Json(body.json)).into_response(),
        (Format::Xml, Err(e)) => xml(
            e.status(),
            format!(
                "<lfm status=\"failed\"><error code=\"{}\">{}</error></lfm>",
                e.code,
                xml_escape(&e.message)
            ),
        ),
        (Format::Json, Err(e)) => (
            e.status(),
            Json(json!({ "error": e.code, "message": e.message })),
        )
            .into_response(),
    }
}

/// GET or POST /2.0/
pub async fn api(State(state): State<AppState>, req: Request) -> Response {
    let http_method = req.method().clone();
    let (parts, body) = match super::read_request(req).await {
        Ok(read) => read,
        Err(_) => {
            return respond(
                Format::Xml,
                Err(LfmError::invalid_parameters("request too large")),
            );
        }
    };
    let params = Params::parse(parts.uri.query(), &body);
    let format = match params.get("format") {
        Some("json") => Format::Json,
        _ => Format::Xml,
    };
    let ip = super::request_ip(&state, &parts);
    respond(format, dispatch(&state, &http_method, &params, &ip).await)
}

async fn dispatch(state: &AppState, http_method: &Method, params: &Params, ip: &str) -> LfmResult {
    let method = params
        .text("method")
        .ok_or_else(|| LfmError::invalid_parameters("method is required"))?
        .to_ascii_lowercase();
    let posted = || {
        if *http_method == Method::POST {
            Ok(())
        } else {
            Err(LfmError::invalid_parameters(
                "this method needs an HTTP POST",
            ))
        }
    };
    match method.as_str() {
        "auth.gettoken" => get_token(state, params).await,
        "auth.getsession" => get_session(state, params).await,
        "auth.getmobilesession" => {
            posted()?;
            get_mobile_session(state, params, ip).await
        }
        "track.scrobble" => {
            posted()?;
            scrobble(state, params).await
        }
        "track.updatenowplaying" => {
            posted()?;
            update_now_playing(state, params).await
        }
        "user.getinfo" => user_info(state, params).await,
        "user.getrecenttracks" => recent_tracks(state, params).await,
        _ => Err(LfmError::new(
            3,
            "Invalid Method - No method with that name in this package",
        )),
    }
}

/// Signed with a secret the server knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signed {
    Verified,
    Unverified,
}

/// Checks the api_key and, where the server knows its secret, the
/// signature, which methods that change something must carry.
fn check_api_key(
    config: &CompatConfig,
    params: &Params,
    needs_signature: bool,
) -> Result<(String, Signed), LfmError> {
    let api_key = params
        .text("api_key")
        .filter(|k| k.len() <= 64)
        .ok_or_else(LfmError::invalid_api_key)?;
    let sig = params.text("api_sig");
    let signed_with = |secret: &str| sig.is_some_and(|sig| signature_matches(params, secret, sig));
    let signed = match config.api_keys.get(api_key) {
        Some(secret) if signed_with(secret) => Signed::Verified,
        Some(_) if sig.is_some() || needs_signature => {
            return Err(LfmError::new(13, "Invalid method signature supplied"));
        }
        Some(_) => Signed::Unverified,
        None if config.strict_api_keys => return Err(LfmError::invalid_api_key()),
        None if signed_with(api_key) => Signed::Verified,
        None => Signed::Unverified,
    };
    Ok((api_key.to_string(), signed))
}

/// Clients differ on whether empty parameters are signed, so both forms
/// count.
fn signature_matches(params: &Params, secret: &str, sig: &str) -> bool {
    let signed: Vec<(&str, &str)> = params.pairs().filter(|(n, _)| *n != "api_sig").collect();
    let without_empty: Vec<(&str, &str)> = signed
        .iter()
        .copied()
        .filter(|(_, v)| !v.is_empty())
        .collect();
    let sig = sig.to_ascii_lowercase();
    [
        signature(&signed, secret),
        signature(&without_empty, secret),
    ]
    .iter()
    .any(|expected| bool::from(expected.as_bytes().ct_eq(sig.as_bytes())))
}

/// The session the request's `sk` names, if it may be used with `api_key`.
async fn session(state: &AppState, params: &Params, api_key: &str) -> Result<Credential, LfmError> {
    let sk = params.text("sk").ok_or_else(LfmError::invalid_session)?;
    let credential = super::authenticate(state, sk)
        .await?
        .ok_or_else(LfmError::invalid_session)?;
    if credential.kind == KIND_SESSION && credential.api_key.as_deref() != Some(api_key) {
        return Err(LfmError::invalid_session());
    }
    if !super::within_request_limit(state, credential.user_id).await {
        return Err(LfmError::rate_limited());
    }
    Ok(credential)
}

fn session_name(api_key: &str) -> String {
    format!("{api_key} (Last.fm API)")
}

fn session_body(username: &str, key: &str) -> Body {
    Body {
        xml: format!(
            "<session><name>{}</name><key>{}</key><subscriber>0</subscriber></session>",
            xml_escape(username),
            xml_escape(key)
        ),
        json: json!({ "session": { "name": username, "key": key, "subscriber": 0 } }),
    }
}

async fn get_token(state: &AppState, params: &Params) -> LfmResult {
    let (api_key, _) = check_api_key(&state.compat, params, true)?;
    let token = super::new_secret();
    scrobblers_db::create_authorization(&state.db, &token, &api_key, None, AUTH_TOKEN_TTL).await?;
    Ok(Body {
        xml: format!("<token>{token}</token>"),
        json: json!({ "token": token }),
    })
}

async fn get_session(state: &AppState, params: &Params) -> LfmResult {
    let (api_key, _) = check_api_key(&state.compat, params, true)?;
    let token = params
        .text("token")
        .ok_or_else(|| LfmError::invalid_parameters("token is required"))?;
    let invalid = || LfmError::new(4, "Invalid authentication token supplied");

    let Some(user_id) = scrobblers_db::take_authorization(&state.db, token, &api_key).await? else {
        return Err(
            match scrobblers_db::get_authorization(&state.db, token).await? {
                Some(a) if a.api_key != api_key => invalid(),
                Some(a) if a.expires_at <= Utc::now() => {
                    LfmError::new(15, "This token has expired")
                }
                Some(a) if a.user_id.is_none() => {
                    LfmError::new(14, "This token has not been authorized")
                }
                _ => invalid(),
            },
        );
    };
    let user = users_db::find_by_id(&state.db, user_id)
        .await?
        .ok_or_else(invalid)?;
    let (key, _) = super::issue_credential(
        state,
        user.id,
        KIND_SESSION,
        &session_name(&api_key),
        Some(&api_key),
    )
    .await?;
    Ok(session_body(&user.username, &key))
}

/// Username and password, where the password may be a scrobbler token
/// (always) or the account password (when `SCROBBLER_PASSWORD_LOGIN`
/// allows it). Guarded like `/v1/auth/login`, and then some: attempts are
/// limited per IP and per username, and a missing account costs the same
/// Argon2 time as a wrong password.
async fn get_mobile_session(state: &AppState, params: &Params, ip: &str) -> LfmResult {
    let (api_key, _) = check_api_key(&state.compat, params, true)?;
    let username = params
        .text("username")
        .ok_or_else(|| LfmError::invalid_parameters("username is required"))?;
    let password = params
        .get("password")
        .filter(|p| !p.is_empty())
        .ok_or_else(|| LfmError::invalid_parameters("password is required"))?;

    if !super::login_attempt(state, ip, username).await? {
        return Err(LfmError::new(
            29,
            "Rate Limit Exceeded - Too many login attempts, try again later",
        ));
    }
    match password_login(state, username, password, &api_key).await? {
        Some((name, key)) => {
            super::login_succeeded(state, username).await;
            Ok(session_body(&name, &key))
        }
        None => Err(LfmError::new(
            4,
            "Authentication Failed - Invalid username or password",
        )),
    }
}

/// A token password is returned as the session key itself, so revoking
/// the token ends the session; an account password gets a new session
/// bound to the api_key.
async fn password_login(
    state: &AppState,
    username: &str,
    password: &str,
    api_key: &str,
) -> Result<Option<(String, String)>, LfmError> {
    if let Some(credential) = super::authenticate(state, password).await?
        && credential.kind == KIND_TOKEN
        && credential.username.eq_ignore_ascii_case(username.trim())
    {
        return Ok(Some((credential.username, password.trim().to_string())));
    }
    if !state.compat.password_login {
        return Ok(None);
    }
    let Some(user) = verify_password_login(&state.db, username, password, true).await? else {
        return Ok(None);
    };
    let (key, _) = super::issue_credential(
        state,
        user.id,
        KIND_SESSION,
        &session_name(api_key),
        Some(api_key),
    )
    .await?;
    Ok(Some((user.username, key)))
}

/// One `track.scrobble` entry, as sent.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    artist: String,
    track: String,
    album: String,
    album_artist: String,
    timestamp: i64,
    duration_ms: Option<i32>,
}

/// The batch form (`artist[0]`, … up to 50) or the single one (`artist`).
fn parse_scrobbles(params: &Params) -> Result<Vec<Entry>, LfmError> {
    let mut indices = BTreeSet::new();
    for (name, _) in params.pairs() {
        if let Some((field, index)) = name.strip_suffix(']').and_then(|n| n.split_once('['))
            && SCROBBLE_FIELDS.contains(&field)
        {
            let index: usize = index
                .parse()
                .ok()
                .filter(|i| *i < 1000)
                .ok_or_else(|| LfmError::invalid_parameters("bad scrobble index"))?;
            indices.insert(index);
        }
    }
    if indices.len() > MAX_BATCH {
        return Err(LfmError::invalid_parameters(
            "at most 50 scrobbles per request",
        ));
    }
    let slots: Vec<Option<usize>> = if indices.is_empty() {
        vec![None]
    } else {
        indices.into_iter().map(Some).collect()
    };

    let field = |name: &str, slot: Option<usize>| -> String {
        match slot {
            Some(i) => params.get(&format!("{name}[{i}]")),
            None => params.get(name),
        }
        .unwrap_or_default()
        .to_string()
    };
    slots
        .into_iter()
        .map(|slot| {
            let timestamp = field("timestamp", slot)
                .trim()
                .parse()
                .map_err(|_| LfmError::invalid_parameters("timestamp is required"))?;
            Ok(Entry {
                artist: field("artist", slot),
                track: field("track", slot),
                album: field("album", slot),
                album_artist: field("albumArtist", slot),
                timestamp,
                duration_ms: seconds_to_ms(&field("duration", slot)),
            })
        })
        .collect()
}

fn seconds_to_ms(value: &str) -> Option<i32> {
    value
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(|s| super::plausible_duration_ms(s.saturating_mul(1000)))
}

fn ignored_message(outcome: Result<(), Ignored>) -> (u8, &'static str) {
    match outcome {
        Ok(()) => (0, ""),
        Err(Ignored::Artist) => (1, "Artist was ignored"),
        Err(Ignored::Track) => (2, "Track was ignored"),
        Err(Ignored::TooOld) => (3, "Timestamp was too old"),
        Err(Ignored::TooNew) => (4, "Timestamp was too new"),
        Err(Ignored::DailyLimit) => (5, "Daily scrobble limit exceeded"),
    }
}

fn corrected_xml(tag: &str, value: &str) -> String {
    format!("<{tag} corrected=\"0\">{}</{tag}>", xml_escape(value))
}

fn corrected_json(value: &str) -> Value {
    json!({ "corrected": "0", "#text": value })
}

fn ignored_xml(outcome: Result<(), Ignored>) -> String {
    let (code, message) = ignored_message(outcome);
    format!("<ignoredMessage code=\"{code}\">{message}</ignoredMessage>")
}

fn ignored_json(outcome: Result<(), Ignored>) -> Value {
    let (code, message) = ignored_message(outcome);
    json!({ "code": code.to_string(), "#text": message })
}

async fn scrobble(state: &AppState, params: &Params) -> LfmResult {
    let (api_key, signed) = check_api_key(&state.compat, params, true)?;
    let credential = session(state, params, &api_key).await?;
    let entries = parse_scrobbles(params)?;
    let plays: Vec<Play> = entries
        .iter()
        .map(|e| Play {
            artist: e.artist.clone(),
            track: e.track.clone(),
            album: Some(e.album.clone()),
            played_at: DateTime::from_timestamp(e.timestamp, 0).unwrap_or(DateTime::UNIX_EPOCH),
            duration_ms: e.duration_ms,
        })
        .collect();
    let client = ClientIdentity::new(PROTOCOL_LASTFM, &api_key, signed == Signed::Verified);
    let outcomes = super::submit(state, &credential, &client, &plays).await?;

    let accepted = outcomes.iter().filter(|o| o.is_ok()).count();
    let ignored = outcomes.len() - accepted;
    let mut xml = format!("<scrobbles accepted=\"{accepted}\" ignored=\"{ignored}\">");
    let mut json_scrobbles = Vec::with_capacity(entries.len());
    for (entry, outcome) in entries.iter().zip(&outcomes) {
        xml.push_str(&format!(
            "<scrobble>{}{}{}{}<timestamp>{}</timestamp>{}</scrobble>",
            corrected_xml("track", &entry.track),
            corrected_xml("artist", &entry.artist),
            corrected_xml("album", &entry.album),
            corrected_xml("albumArtist", &entry.album_artist),
            entry.timestamp,
            ignored_xml(*outcome),
        ));
        json_scrobbles.push(json!({
            "track": corrected_json(&entry.track),
            "artist": corrected_json(&entry.artist),
            "album": corrected_json(&entry.album),
            "albumArtist": corrected_json(&entry.album_artist),
            "timestamp": entry.timestamp.to_string(),
            "ignoredMessage": ignored_json(*outcome),
        }));
    }
    xml.push_str("</scrobbles>");
    // Like Last.fm: an object for one scrobble, an array for several.
    let scrobble = match json_scrobbles.len() {
        1 => json_scrobbles.pop().unwrap_or_default(),
        _ => Value::Array(json_scrobbles),
    };
    Ok(Body {
        xml,
        json: json!({
            "scrobbles": {
                "scrobble": scrobble,
                "@attr": { "accepted": accepted, "ignored": ignored },
            }
        }),
    })
}

async fn update_now_playing(state: &AppState, params: &Params) -> LfmResult {
    let (api_key, signed) = check_api_key(&state.compat, params, true)?;
    let credential = session(state, params, &api_key).await?;
    let text = |name: &str| params.get(name).unwrap_or_default().to_string();
    let (artist, track, album, album_artist) = (
        text("artist"),
        text("track"),
        text("album"),
        text("albumArtist"),
    );
    let client = ClientIdentity::new(PROTOCOL_LASTFM, &api_key, signed == Signed::Verified);
    let outcome = super::now_playing(
        state,
        &credential,
        &client,
        &Playing {
            artist: artist.clone(),
            track: track.clone(),
            album: Some(album.clone()),
            duration_ms: seconds_to_ms(&text("duration")),
        },
    )
    .await?;
    Ok(Body {
        xml: format!(
            "<nowplaying>{}{}{}{}{}</nowplaying>",
            corrected_xml("track", &track),
            corrected_xml("artist", &artist),
            corrected_xml("album", &album),
            corrected_xml("albumArtist", &album_artist),
            ignored_xml(outcome),
        ),
        json: json!({
            "nowplaying": {
                "track": corrected_json(&track),
                "artist": corrected_json(&artist),
                "album": corrected_json(&album),
                "albumArtist": corrected_json(&album_artist),
                "ignoredMessage": ignored_json(outcome),
            }
        }),
    })
}

/// The user named by `user`, or the session's, if the requester may see
/// them: a private profile only to its owner.
async fn visible_user(state: &AppState, params: &Params) -> Result<User, LfmError> {
    let (api_key, _) = check_api_key(&state.compat, params, false)?;
    let viewer = match params.text("sk") {
        Some(_) => Some(session(state, params, &api_key).await?),
        None => None,
    };
    let name = params
        .text("user")
        .map(str::to_string)
        .or_else(|| viewer.as_ref().map(|v| v.username.clone()))
        .ok_or_else(|| LfmError::invalid_parameters("user is required"))?;
    let user = users_db::find_by_username(&state.db, &name)
        .await?
        .ok_or_else(|| LfmError::new(6, "User not found"))?;
    if user.is_private && viewer.map(|v| v.user_id) != Some(user.id) {
        return Err(LfmError::new(17, "Login: User requires to be logged in"));
    }
    Ok(user)
}

fn profile_url(state: &AppState, username: &str) -> String {
    match &state.compat.web_app_url {
        Some(web) => format!("{web}/user/{username}"),
        None => format!("{}/v1/user/{username}", state.uploads.public_base_url),
    }
}

fn images(url: Option<&str>) -> (String, Value) {
    let url = url.unwrap_or_default();
    let sizes = ["small", "medium", "large", "extralarge"];
    (
        sizes
            .iter()
            .map(|size| format!("<image size=\"{size}\">{}</image>", xml_escape(url)))
            .collect(),
        Value::Array(
            sizes
                .iter()
                .map(|size| json!({ "size": size, "#text": url }))
                .collect(),
        ),
    )
}

async fn user_info(state: &AppState, params: &Params) -> LfmResult {
    let user = visible_user(state, params).await?;
    let url = profile_url(state, &user.username);
    let (image_xml, image_json) = images(user.image_url.as_deref());
    let realname = user.display_name.clone().unwrap_or_default();
    let country = user.country.clone().unwrap_or_default();
    let registered = user.created_at.timestamp();
    Ok(Body {
        xml: format!(
            "<user><name>{name}</name><realname>{realname}</realname>{image_xml}<url>{url}</url>\
             <country>{country}</country><subscriber>0</subscriber><playcount>{plays}</playcount>\
             <registered unixtime=\"{registered}\">{date}</registered><type>user</type></user>",
            name = xml_escape(&user.username),
            realname = xml_escape(&realname),
            url = xml_escape(&url),
            country = xml_escape(&country),
            plays = user.scrobble_count,
            date = user.created_at.format("%Y-%m-%d %H:%M"),
        ),
        json: json!({
            "user": {
                "name": user.username,
                "realname": realname,
                "image": image_json,
                "url": url,
                "country": country,
                "subscriber": "0",
                "playcount": user.scrobble_count.to_string(),
                "registered": { "unixtime": registered.to_string(), "#text": registered },
                "type": "user",
            }
        }),
    })
}

fn unix_param(params: &Params, name: &str) -> Result<Option<DateTime<Utc>>, LfmError> {
    params
        .text(name)
        .map(|v| {
            v.parse::<i64>()
                .ok()
                .and_then(|secs| DateTime::from_timestamp(secs, 0))
                .ok_or_else(|| LfmError::invalid_parameters(&format!("bad {name}")))
        })
        .transpose()
}

struct RecentEntry {
    artist: String,
    track: String,
    album: String,
    image: Option<String>,
    played_at: Option<DateTime<Utc>>,
}

async fn recent_tracks(state: &AppState, params: &Params) -> LfmResult {
    let user = visible_user(state, params).await?;
    let limit = params
        .text("limit")
        .and_then(|l| l.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    let page = params
        .text("page")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let (from, to) = (unix_param(params, "from")?, unix_param(params, "to")?);

    let total = match (from, to) {
        (None, None) => user.scrobble_count,
        _ => scrobbles_db::count_scrobbles(&state.db, user.id, from, to).await?,
    };
    let offset = (page - 1).saturating_mul(limit);
    let page_scrobbles = if offset < total {
        scrobbles_db::recent_scrobbles_page(&state.db, user.id, from, to, limit, offset).await?
    } else {
        Vec::new()
    };

    let mut entries = Vec::new();
    if page == 1
        && to.is_none()
        && let Some(np) = scrobbles_db::get_now_playing(&state.db, user.id).await?
    {
        entries.push(RecentEntry {
            artist: np.artist_name,
            track: np.track_title,
            album: np.album_title.unwrap_or_default(),
            image: np.album_image.or(np.artist_image),
            played_at: None,
        });
    }
    entries.extend(page_scrobbles.into_iter().map(|s| RecentEntry {
        artist: s.artist_name,
        track: s.track_title,
        album: s.album_title.unwrap_or_default(),
        image: s.album_image,
        played_at: Some(s.played_at),
    }));

    let total_pages = (total + limit - 1) / limit;
    let mut xml = format!(
        "<recenttracks user=\"{}\" page=\"{page}\" perPage=\"{limit}\" totalPages=\"{total_pages}\" total=\"{total}\">",
        xml_escape(&user.username)
    );
    let mut tracks = Vec::with_capacity(entries.len());
    for e in &entries {
        let (image_xml, image_json) = images(e.image.as_deref());
        let (date_xml, date_json, attr) = match e.played_at {
            Some(at) => (
                format!(
                    "<date uts=\"{}\">{}</date>",
                    at.timestamp(),
                    at.format("%d %b %Y, %H:%M")
                ),
                json!({ "uts": at.timestamp().to_string(), "#text": at.format("%d %b %Y, %H:%M").to_string() }),
                None,
            ),
            None => (
                String::new(),
                Value::Null,
                Some(json!({ "nowplaying": "true" })),
            ),
        };
        xml.push_str(&format!(
            "<track{np}><artist mbid=\"\">{artist}</artist><name>{track}</name><mbid></mbid>\
             <album mbid=\"\">{album}</album><url></url>{date_xml}<streamable>0</streamable>{image_xml}</track>",
            np = if attr.is_some() { " nowplaying=\"true\"" } else { "" },
            artist = xml_escape(&e.artist),
            track = xml_escape(&e.track),
            album = xml_escape(&e.album),
        ));
        let mut track = json!({
            "artist": { "mbid": "", "#text": e.artist },
            "name": e.track,
            "mbid": "",
            "album": { "mbid": "", "#text": e.album },
            "url": "",
            "streamable": "0",
            "image": image_json,
        });
        if !date_json.is_null() {
            track["date"] = date_json;
        }
        if let Some(attr) = attr {
            track["@attr"] = attr;
        }
        tracks.push(track);
    }
    xml.push_str("</recenttracks>");
    Ok(Body {
        xml,
        json: json!({
            "recenttracks": {
                "track": tracks,
                "@attr": {
                    "user": user.username,
                    "page": page.to_string(),
                    "perPage": limit.to_string(),
                    "totalPages": total_pages.to_string(),
                    "total": total.to_string(),
                },
            }
        }),
    })
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

    fn signed(pairs: &[(&str, &str)], secret: &str) -> Params {
        let mut all: Vec<(&str, &str)> = pairs.to_vec();
        let sig = signature(pairs, secret);
        all.push(("api_sig", &sig));
        params(&all)
    }

    fn config(keys: &[(&str, &str)], strict: bool) -> CompatConfig {
        CompatConfig {
            api_keys: keys
                .iter()
                .map(|(k, s)| (k.to_string(), s.to_string()))
                .collect(),
            strict_api_keys: strict,
            ..Default::default()
        }
    }

    #[test]
    fn signatures_are_checked_where_the_secret_is_known() {
        let base = [
            ("method", "track.scrobble"),
            ("api_key", "known"),
            ("sk", "s"),
        ];
        let cfg = config(&[("known", "secret")], false);
        assert_eq!(
            check_api_key(&cfg, &signed(&base, "secret"), true)
                .unwrap()
                .1,
            Signed::Verified
        );
        assert_eq!(
            check_api_key(&cfg, &signed(&base, "wrong"), true)
                .unwrap_err()
                .code,
            13
        );
        assert_eq!(
            check_api_key(&cfg, &params(&base), true).unwrap_err().code,
            13
        );
        // Reads need no signature.
        assert_eq!(
            check_api_key(&cfg, &params(&base), false).unwrap().1,
            Signed::Unverified
        );
    }

    #[test]
    fn unknown_keys_pass_unverified_unless_strict() {
        let base = [
            ("method", "track.scrobble"),
            ("api_key", "mpris"),
            ("sk", "s"),
        ];
        assert_eq!(
            check_api_key(&config(&[], false), &signed(&base, "their-secret"), true)
                .unwrap()
                .1,
            Signed::Unverified
        );
        assert_eq!(
            check_api_key(&config(&[], true), &signed(&base, "their-secret"), true)
                .unwrap_err()
                .code,
            10
        );
        assert_eq!(
            check_api_key(&config(&[], false), &params(&[]), false)
                .unwrap_err()
                .code,
            10
        );
    }

    #[test]
    fn gnu_fm_clients_signing_with_their_key_are_verified() {
        let base = [
            ("method", "track.scrobble"),
            ("api_key", "panoScrobbler"),
            ("sk", "s"),
        ];
        assert_eq!(
            check_api_key(&config(&[], false), &signed(&base, "panoScrobbler"), true)
                .unwrap()
                .1,
            Signed::Verified
        );
    }

    #[test]
    fn empty_parameters_may_or_may_not_be_signed() {
        let sent = [
            ("method", "track.scrobble"),
            ("api_key", "k"),
            ("album", ""),
            ("sk", "s"),
        ];
        let sig_without_empty = signature(
            &[("method", "track.scrobble"), ("api_key", "k"), ("sk", "s")],
            "x",
        );
        let mut with_sig = sent.to_vec();
        with_sig.push(("api_sig", &sig_without_empty));
        assert!(signature_matches(
            &params(&with_sig),
            "x",
            &sig_without_empty
        ));
        let sig_with_empty = signature(&sent, "x");
        assert!(signature_matches(
            &params(&sent),
            "x",
            &sig_with_empty.to_uppercase()
        ));
    }

    #[test]
    fn batches_and_single_scrobbles_parse() {
        let single = parse_scrobbles(&params(&[
            ("artist", "A"),
            ("track", "T"),
            ("timestamp", "1700000000"),
            ("duration", "215"),
        ]))
        .unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].duration_ms, Some(215_000));

        let batch = parse_scrobbles(&params(&[
            ("artist[1]", "B"),
            ("track[1]", "U"),
            ("timestamp[1]", "1700000300"),
            ("artist[0]", "A"),
            ("track[0]", "T"),
            ("timestamp[0]", "1700000000"),
            ("duration[0]", "999999999"),
        ]))
        .unwrap();
        assert_eq!(
            batch.iter().map(|e| e.artist.as_str()).collect::<Vec<_>>(),
            ["A", "B"]
        );
        assert_eq!(batch[0].duration_ms, None);

        let missing = parse_scrobbles(&params(&[("artist[0]", "A"), ("track[0]", "T")]));
        assert_eq!(missing.unwrap_err().code, 6);
        let mut too_many = Vec::new();
        let names: Vec<(String, String)> = (0..51)
            .flat_map(|i| {
                [
                    (format!("artist[{i}]"), "A".to_string()),
                    (format!("timestamp[{i}]"), "1700000000".to_string()),
                ]
            })
            .collect();
        too_many.extend(names.iter().map(|(n, v)| (n.as_str(), v.as_str())));
        assert_eq!(parse_scrobbles(&params(&too_many)).unwrap_err().code, 6);
    }

    #[test]
    fn xml_text_is_escaped_and_cleaned() {
        assert_eq!(
            xml_escape("R&B <\"live\"> it's\u{1}"),
            "R&amp;B &lt;&quot;live&quot;&gt; it&apos;s"
        );
    }
}
