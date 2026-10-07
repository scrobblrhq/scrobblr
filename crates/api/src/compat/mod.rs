//! Scrobbler-compatible APIs, so existing scrobblers work by changing only
//! the server URL: ListenBrainz (`/1/…`). What they submit goes through
//! `ingest_scrobble` like native scrobbles, recorded with the protocol and
//! client it came from.
//!
//! Clients authenticate with `scrobbler_credentials`, never the native
//! API's sessions or tokens: tokens a user makes to paste into a client.

pub mod credentials;
pub mod listenbrainz;
#[cfg(test)]
mod tests;

use axum::body::Bytes;
use axum::routing::{get, post};
use axum::{Router, extract::Request};
use chrono::{DateTime, TimeDelta, Utc};
use fred::interfaces::KeysInterface;

use crate::errors::{ApiResult, AppError};
use crate::handlers::scrobbles::{NowPlayingRequest, set_now_playing};
use crate::state::AppState;
use db::queries::scrobble_clients::ClientIdentity;
use db::queries::scrobblers::{self as scrobblers_db, Credential};
use db::queries::{auth as auth_db, scrobbles as scrobbles_db};
use shared::models::ScrobblerCredential;
use shared::scrobble::{MAX_CLOCK_SKEW, MAX_NAME_LEN, ScrobbleInput, ScrobbleValidationError};

/// Older scrobbles are ignored, as Last.fm does: history comes in through
/// the verified importer.
pub const MAX_SCROBBLE_AGE: TimeDelta = TimeDelta::days(14);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const REQUESTS_PER_USER_MINUTE: i64 = 120;
const DEFAULT_DAILY_LIMIT: i64 = 3_000;

#[derive(Debug, Clone)]
pub struct CompatConfig {
    /// Scrobbles a user may submit per UTC day (`SCROBBLER_DAILY_LIMIT`).
    pub daily_limit: i64,
}

impl Default for CompatConfig {
    fn default() -> Self {
        Self {
            daily_limit: DEFAULT_DAILY_LIMIT,
        }
    }
}

impl CompatConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let daily_limit = match std::env::var("SCROBBLER_DAILY_LIMIT")
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            Some(v) => v
                .trim()
                .parse()
                .map_err(|e| anyhow::anyhow!("SCROBBLER_DAILY_LIMIT={v}: {e}"))?,
            None => DEFAULT_DAILY_LIMIT,
        };
        Ok(Self { daily_limit })
    }
}

/// The compatibility endpoints, outside the OpenAPI spec: they speak other
/// services' protocols.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/1/validate-token", get(listenbrainz::validate_token))
        .route("/1/submit-listens", post(listenbrainz::submit_listens))
}

/// Form or query parameters, in the order sent.
#[derive(Debug, Default, Clone)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn parse(query: Option<&str>, body: &[u8]) -> Self {
        let mut pairs: Vec<(String, String)> = query
            .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
            .unwrap_or_default();
        pairs.extend(form_urlencoded::parse(body).into_owned());
        Self(pairs)
    }

    /// The last value sent under `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Non-empty, trimmed.
    pub fn text(&self, name: &str) -> Option<&str> {
        self.get(name).map(str::trim).filter(|v| !v.is_empty())
    }
}

/// The request's parts and body, read up to [`MAX_BODY_BYTES`].
pub async fn read_request(req: Request) -> ApiResult<(axum::http::request::Parts, Bytes)> {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| AppError::BadRequest("request body too large".into()))?;
    Ok((parts, body))
}

/// A random secret like Last.fm's session keys: 32 hex characters.
pub fn new_secret() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// The credential `secret` names, if any.
pub async fn authenticate(state: &AppState, secret: &str) -> ApiResult<Option<Credential>> {
    let secret = secret.trim();
    if secret.is_empty() || secret.len() > 256 {
        return Ok(None);
    }
    let credential =
        scrobblers_db::find_credential(&state.db, &auth_db::hash_api_token(secret)).await?;
    if let Some(c) = &credential {
        let (db, id) = (state.db.clone(), c.id);
        tokio::spawn(async move {
            let _ = scrobblers_db::touch_credential(&db, id).await;
        });
    }
    Ok(credential)
}

/// Fixed one-minute windows per user, across these APIs. A Redis failure
/// lets the request through: the global per-IP limit still applies.
pub async fn within_request_limit(state: &AppState, user_id: i64) -> bool {
    let key = format!("scrobbler_rl:{user_id}:{}", Utc::now().timestamp() / 60);
    match state.redis.incr::<i64, _>(&key).await {
        Ok(count) => {
            if count == 1 {
                let _ = state.redis.expire::<i64, _>(&key, 120, None).await;
            }
            count <= REQUESTS_PER_USER_MINUTE
        }
        Err(e) => {
            tracing::warn!("scrobbler request limit unavailable: {e}");
            true
        }
    }
}

/// One play as a compatibility protocol submitted it.
#[derive(Debug, Clone)]
pub struct Play {
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub played_at: DateTime<Utc>,
    pub duration_ms: Option<i32>,
}

/// Why a play wasn't recorded, in Last.fm's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ignored {
    Artist,
    Track,
    TooOld,
    TooNew,
    DailyLimit,
}

/// A client-reported length, unless implausible.
pub fn plausible_duration_ms(ms: i64) -> Option<i32> {
    (1..=24 * 3_600_000).contains(&ms).then_some(ms as i32)
}

fn precheck(play: &Play, now: DateTime<Utc>) -> Result<(), Ignored> {
    let too_long = |s: &str| s.chars().count() > MAX_NAME_LEN;
    if play.artist.trim().is_empty() || too_long(&play.artist) {
        return Err(Ignored::Artist);
    }
    if play.track.trim().is_empty()
        || too_long(&play.track)
        || play.album.as_deref().is_some_and(too_long)
    {
        return Err(Ignored::Track);
    }
    if play.played_at < now - MAX_SCROBBLE_AGE {
        return Err(Ignored::TooOld);
    }
    if play.played_at > now + MAX_CLOCK_SKEW {
        return Err(Ignored::TooNew);
    }
    Ok(())
}

/// Takes up to `wanted` scrobbles from the user's daily allowance and
/// returns how many it got. A Redis failure grants them all.
async fn reserve_quota(state: &AppState, user_id: i64, wanted: i64) -> (String, i64) {
    let key = format!("scrobbler_quota:{user_id}:{}", Utc::now().format("%Y%m%d"));
    if wanted == 0 {
        return (key, 0);
    }
    match state.redis.incr_by::<i64, _>(&key, wanted).await {
        Ok(count) => {
            if count == wanted {
                let _ = state.redis.expire::<i64, _>(&key, 2 * 86_400, None).await;
            }
            let over = (count - state.compat.daily_limit).clamp(0, wanted);
            (key, wanted - over)
        }
        Err(e) => {
            tracing::warn!("scrobbler daily limit unavailable: {e}");
            (key, wanted)
        }
    }
}

async fn release_quota(state: &AppState, key: &str, unused: i64) {
    if unused > 0 {
        let _ = state.redis.decr_by::<i64, _>(key, unused).await;
    }
}

/// Validates and ingests `plays`, in order, for the credential's user. A
/// retried play counts as recorded. `Err` only for a failure worth
/// retrying the request for; plays recorded before it stay recorded, and a
/// retry skips them as duplicates.
pub async fn submit(
    state: &AppState,
    credential: &Credential,
    client: &ClientIdentity,
    plays: &[Play],
) -> ApiResult<Vec<Result<(), Ignored>>> {
    let now = Utc::now();
    let mut outcomes: Vec<Result<(), Ignored>> = plays.iter().map(|p| precheck(p, now)).collect();
    let wanted = outcomes.iter().filter(|o| o.is_ok()).count() as i64;
    let (quota_key, granted) = reserve_quota(state, credential.user_id, wanted).await;

    let mut remaining = granted;
    for outcome in outcomes.iter_mut().filter(|o| o.is_ok()) {
        if remaining == 0 {
            *outcome = Err(Ignored::DailyLimit);
        } else {
            remaining -= 1;
        }
    }

    let client_id = state.clients.id(&state.db, client).await;
    let mut unused = 0;
    for (i, play) in plays.iter().enumerate() {
        if outcomes[i].is_err() {
            continue;
        }
        let input = ScrobbleInput {
            track_title: play.track.trim().to_string(),
            artist_name: play.artist.trim().to_string(),
            featured_artists: vec![],
            album_title: play
                .album
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(str::to_string),
            played_at: play.played_at,
            duration_ms: play.duration_ms,
            listened_ms: None,
            source: client.protocol.to_string(),
            client_id,
        };
        match scrobbles_db::ingest_scrobble(&state.db, credential.user_id, &input).await {
            Ok(_) => {}
            Err(scrobbles_db::IngestError::Duplicate) => unused += 1,
            Err(scrobbles_db::IngestError::Validation(e)) => {
                unused += 1;
                outcomes[i] = Err(match e {
                    ScrobbleValidationError::MissingArtist => Ignored::Artist,
                    ScrobbleValidationError::FutureTimestamp => Ignored::TooNew,
                    _ => Ignored::Track,
                });
            }
            Err(scrobbles_db::IngestError::Db(e)) => {
                let unstored = outcomes[i..].iter().filter(|o| o.is_ok()).count() as i64;
                release_quota(state, &quota_key, unused + unstored).await;
                return Err(AppError::Database(e));
            }
        }
    }
    release_quota(state, &quota_key, unused).await;
    Ok(outcomes)
}

/// What a client says is playing.
#[derive(Debug, Clone)]
pub struct Playing {
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub duration_ms: Option<i32>,
}

pub async fn now_playing(
    state: &AppState,
    credential: &Credential,
    client: &ClientIdentity,
    playing: &Playing,
) -> ApiResult<Result<(), Ignored>> {
    let check = Play {
        artist: playing.artist.clone(),
        track: playing.track.clone(),
        album: playing.album.clone(),
        played_at: Utc::now(),
        duration_ms: playing.duration_ms,
    };
    if let Err(ignored) = precheck(&check, Utc::now()) {
        return Ok(Err(ignored));
    }
    let request = NowPlayingRequest {
        track: playing.track.trim().to_string(),
        artist: playing.artist.trim().to_string(),
        featured_artists: vec![],
        album: playing
            .album
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string),
        duration_ms: playing.duration_ms,
        source: None,
    };
    set_now_playing(state, credential.user_id, &request, client.protocol).await?;
    Ok(Ok(()))
}

/// Stores a session or token for `user_id` and returns its secret.
pub async fn issue_credential(
    state: &AppState,
    user_id: i64,
    kind: &str,
    name: &str,
    api_key: Option<&str>,
) -> ApiResult<(String, ScrobblerCredential)> {
    let secret = new_secret();
    // Only tokens are typed into Audioscrobbler clients as passwords.
    let legacy_secret = if kind == scrobblers_db::KIND_TOKEN {
        match shared::crypto::encrypt(&shared::lastfm::md5_hex(&secret)) {
            Ok(enc) => Some(enc),
            Err(shared::crypto::CryptoError::MissingKey) => None,
            Err(e) => return Err(AppError::Internal(anyhow::anyhow!(e))),
        }
    } else {
        None
    };
    let created = scrobblers_db::create_credential(
        &state.db,
        &scrobblers_db::NewCredential {
            user_id,
            kind,
            name,
            key_hash: &auth_db::hash_api_token(&secret),
            legacy_secret: legacy_secret.as_deref(),
            api_key,
        },
    )
    .await?;
    Ok((secret, created))
}
