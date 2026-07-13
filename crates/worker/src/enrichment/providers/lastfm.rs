//! Last.fm — artist biographies. Optional: enabled only when
//! `LASTFM_API_KEY` is set; without it the bio field simply stays NULL.

use std::time::Duration;

use uuid::Uuid;

use super::{ProviderError, ProviderResult, get_json};
use crate::enrichment::ratelimit::RateLimiter;

const BASE: &str = "https://ws.audioscrobbler.com/2.0/";

/// Last.fm error code for rate limiting.
const RATE_LIMITED: i32 = 29;
/// Last.fm error code for "artist not found".
const NOT_FOUND: i32 = 6;

#[derive(Debug, serde::Deserialize)]
struct ArtistInfoResponse {
    artist: Option<ArtistInfo>,
    error: Option<i32>,
    message: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ArtistInfo {
    bio: Option<Bio>,
}

#[derive(Debug, serde::Deserialize)]
struct Bio {
    summary: Option<String>,
}

/// Fetches an artist bio summary. Prefers MBID lookup when available (exact),
/// falling back to name matching on Last.fm's side.
pub async fn artist_bio(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    api_key: &str,
    name: &str,
    mbid: Option<Uuid>,
) -> ProviderResult<String> {
    let mbid_string = mbid.map(|m| m.to_string());
    let mut query: Vec<(&str, &str)> = vec![
        ("method", "artist.getinfo"),
        ("api_key", api_key),
        ("format", "json"),
        ("autocorrect", "1"),
    ];
    match &mbid_string {
        Some(m) => query.push(("mbid", m.as_str())),
        None => query.push(("artist", name)),
    }

    let result: Option<ArtistInfoResponse> =
        get_json(client, limiter, "lastfm", BASE, &query).await?;

    let Some(response) = result else {
        return Ok(None);
    };

    if let Some(code) = response.error {
        let msg = response.message.unwrap_or_default();
        return match code {
            NOT_FOUND => Ok(None),
            RATE_LIMITED => {
                limiter.penalize(Duration::from_secs(60)).await;
                Err(ProviderError::Transient(format!(
                    "lastfm: rate limited: {msg}"
                )))
            }
            _ => Err(ProviderError::Fatal(format!("lastfm: error {code}: {msg}"))),
        };
    }

    Ok(response
        .artist
        .and_then(|a| a.bio)
        .and_then(|b| b.summary)
        .and_then(clean_bio))
}

/// Last.fm summaries end with an HTML "Read more" link (and the licensing
/// blurb rides along after it). Cut at the first anchor and keep plain text.
fn clean_bio(summary: String) -> Option<String> {
    let cut = summary
        .find("<a href=")
        .map(|pos| &summary[..pos])
        .unwrap_or(&summary);
    let cleaned = cut.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}
