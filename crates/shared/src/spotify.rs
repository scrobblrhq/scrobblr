//! Spotify Web API client: OAuth Authorization Code flow (authorize URL,
//! token exchange, refresh) and the `recently-played` endpoint used to
//! auto-scrobble. Shared between `crates/api` (the OAuth HTTP dance) and
//! `crates/worker` (the polling loop + token refresh), so both sides talk to
//! Spotify the same way.
//!
//! Deliberately hand-rolled with `reqwest` rather than the `oauth2` crate:
//! the flow is small (one authorize-URL builder, one token POST), and this
//! keeps the same HTTP-calling style already used by the metadata-enrichment
//! providers in `crates/worker/src/enrichment/providers/`.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use thiserror::Error;

const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const RECENTLY_PLAYED_URL: &str = "https://api.spotify.com/v1/me/player/recently-played";
const CURRENTLY_PLAYING_URL: &str = "https://api.spotify.com/v1/me/player/currently-playing";
const ME_URL: &str = "https://api.spotify.com/v1/me";

/// Read-only listening history (scrobbles) plus live playback state (now
/// playing). Accounts connected before `user-read-currently-playing` was
/// added here only granted the first scope — they need to disconnect and
/// reconnect once for the live now-playing widget to start working.
pub const SCOPES: &str = "user-read-recently-played user-read-currently-playing";

#[derive(Debug, Error)]
pub enum SpotifyError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("spotify access token is invalid or expired")]
    Unauthorized,
    #[error("spotify returned an error: {0}")]
    Api(String),
}

#[derive(Debug, Clone)]
pub struct SpotifyTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Seconds until `access_token` expires, per Spotify's response.
    pub expires_in: i64,
    pub scope: String,
    pub token_type: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    scope: String,
    expires_in: i64,
    refresh_token: Option<String>,
}

/// Builds the URL to redirect the user to so they can grant access.
/// `csrf_state` should be a random, single-use, short-lived value the
/// caller stores (e.g. in Redis) and verifies on callback.
pub fn build_authorize_url(client_id: &str, redirect_uri: &str, csrf_state: &str) -> String {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL is a valid static URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("state", csrf_state);
    url.to_string()
}

/// Exchanges an authorization `code` (from the OAuth callback) for an
/// access/refresh token pair.
pub async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<SpotifyTokens, SpotifyError> {
    let resp = client
        .post(TOKEN_URL)
        .basic_auth(client_id, Some(client_secret))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?;

    parse_token_response(resp).await
}

/// Exchanges a stored `refresh_token` for a fresh access token. Spotify may
/// or may not include a new `refresh_token` in the response — callers should
/// keep the old one when it doesn't.
pub async fn refresh_access_token(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<SpotifyTokens, SpotifyError> {
    let resp = client
        .post(TOKEN_URL)
        .basic_auth(client_id, Some(client_secret))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?;

    parse_token_response(resp).await
}

async fn parse_token_response(resp: reqwest::Response) -> Result<SpotifyTokens, SpotifyError> {
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED
        || resp.status() == reqwest::StatusCode::BAD_REQUEST
    {
        // Spotify returns 400 with an `invalid_grant` body for a revoked or
        // expired refresh token — treat both as "needs re-authorization".
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if body.contains("invalid_grant") || status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SpotifyError::Unauthorized);
        }
        return Err(SpotifyError::Api(format!("{status}: {body}")));
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(SpotifyError::Api(format!("{status}: {body}")));
    }

    let parsed: TokenResponse = resp.json().await?;
    Ok(SpotifyTokens {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expires_in: parsed.expires_in,
        scope: parsed.scope,
        token_type: parsed.token_type,
    })
}

/// Fetches the Spotify user id of the account behind `access_token`, used
/// as `provider_user_id` so the same external account can't be linked to
/// two different Scrobblr users.
pub async fn get_current_user_id(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<String, SpotifyError> {
    #[derive(Deserialize)]
    struct Me {
        id: String,
    }

    let resp = client.get(ME_URL).bearer_auth(access_token).send().await?;
    check_status(&resp)?;
    let me: Me = resp.json().await?;
    Ok(me.id)
}

/// A single already-completed play, ready to become a scrobble. Unlike the
/// browser extension (which has to watch playback live and re-derive listen
/// thresholds itself), Spotify's `recently-played` endpoint only reports
/// plays it already considers significant — so every item here is ingested
/// as-is, with no threshold logic needed on our side. It also already
/// separates collaborating artists into a real list (`track.artists`), so
/// picking `artists[0]` here is not a heuristic the way the YT Music DOM
/// string-splitting fallback is — it's reading structured data directly.
#[derive(Debug, Clone)]
pub struct RecentlyPlayedItem {
    pub track_title: String,
    pub artist_name: String,
    pub album_title: Option<String>,
    pub duration_ms: i32,
    pub played_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct RecentlyPlayedResponse {
    items: Vec<RecentlyPlayedItemRaw>,
}

#[derive(Debug, Deserialize)]
struct RecentlyPlayedItemRaw {
    track: SpotifyTrack,
    played_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct SpotifyTrack {
    name: String,
    artists: Vec<SpotifyArtist>,
    album: SpotifyAlbum,
    duration_ms: i32,
}

#[derive(Debug, Deserialize)]
struct SpotifyArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyAlbum {
    name: String,
}

/// Fetches tracks played after `after_ms` (unix milliseconds), up to
/// Spotify's max of 50 per call. Pass `None` on an account's first poll.
pub async fn get_recently_played(
    client: &reqwest::Client,
    access_token: &str,
    after_ms: Option<i64>,
) -> Result<Vec<RecentlyPlayedItem>, SpotifyError> {
    let mut req = client
        .get(RECENTLY_PLAYED_URL)
        .bearer_auth(access_token)
        .query(&[("limit", "50")]);
    if let Some(after) = after_ms {
        req = req.query(&[("after", after.to_string())]);
    }

    let resp = req.send().await?;
    check_status(&resp)?;

    let parsed: RecentlyPlayedResponse = resp.json().await?;
    Ok(parsed
        .items
        .into_iter()
        .map(|item| RecentlyPlayedItem {
            track_title: item.track.name,
            artist_name: item
                .track
                .artists
                .first()
                .map(|a| a.name.clone())
                .unwrap_or_default(),
            album_title: Some(item.track.album.name),
            duration_ms: item.track.duration_ms,
            played_at: item.played_at,
        })
        .collect())
}

/// What the user is playing right now, for the live now-playing widget —
/// distinct from [`RecentlyPlayedItem`], which is already-finished history.
#[derive(Debug, Clone)]
pub struct CurrentlyPlaying {
    pub track_title: String,
    pub artist_name: String,
    pub album_title: Option<String>,
    pub duration_ms: i32,
    pub progress_ms: i32,
}

#[derive(Debug, Deserialize)]
struct CurrentlyPlayingResponse {
    is_playing: bool,
    progress_ms: Option<i32>,
    item: Option<SpotifyTrack>,
    currently_playing_type: String,
}

/// Fetches what the user is playing right now. Returns `None` when nothing
/// is playing (Spotify's 204 response), playback is paused, or the current
/// item isn't a track (e.g. a podcast episode) — none of those are worth
/// showing in a "now playing" widget.
pub async fn get_currently_playing(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Option<CurrentlyPlaying>, SpotifyError> {
    let resp = client
        .get(CURRENTLY_PLAYING_URL)
        .bearer_auth(access_token)
        .send()
        .await?;

    if resp.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }
    check_status(&resp)?;

    let parsed: CurrentlyPlayingResponse = resp.json().await?;
    if !parsed.is_playing || parsed.currently_playing_type != "track" {
        return Ok(None);
    }
    let Some(item) = parsed.item else {
        return Ok(None);
    };

    Ok(Some(CurrentlyPlaying {
        track_title: item.name,
        artist_name: item
            .artists
            .first()
            .map(|a| a.name.clone())
            .unwrap_or_default(),
        album_title: Some(item.album.name),
        duration_ms: item.duration_ms,
        progress_ms: parsed.progress_ms.unwrap_or(0),
    }))
}

fn check_status(resp: &reqwest::Response) -> Result<(), SpotifyError> {
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(SpotifyError::Unauthorized);
    }
    if !resp.status().is_success() {
        return Err(SpotifyError::Api(format!(
            "unexpected status {}",
            resp.status()
        )));
    }
    Ok(())
}
