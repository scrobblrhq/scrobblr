//! Deezer — image fallback. No API key required (~50 requests / 5 s per IP).
//!
//! Covers the two gaps the MBID-based chain leaves: artist images (which
//! MusicBrainz doesn't host at all) and album covers the Cover Art Archive
//! doesn't have. Matches are accepted only on a normalized name equality —
//! a wrong image is worse than no image.

use std::time::Duration;

use shared::scrobble::normalize_name;

use super::{ProviderError, ProviderResult, get_json};
use crate::enrichment::ratelimit::RateLimiter;

const BASE: &str = "https://api.deezer.com";

/// Deezer signals quota exhaustion with an error object in a 200 body.
const QUOTA_EXCEEDED: i32 = 4;

#[derive(Debug, serde::Deserialize)]
struct SearchResponse<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
    error: Option<DeezerError>,
}

#[derive(Debug, serde::Deserialize)]
struct DeezerError {
    code: i32,
    message: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ArtistHit {
    name: String,
    picture_xl: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct AlbumHit {
    title: String,
    cover_xl: Option<String>,
}

async fn check_error<T>(
    limiter: &RateLimiter,
    response: SearchResponse<T>,
) -> Result<Vec<T>, ProviderError> {
    if let Some(err) = response.error {
        let msg = err.message.unwrap_or_default();
        if err.code == QUOTA_EXCEEDED {
            limiter.penalize(Duration::from_secs(5)).await;
            return Err(ProviderError::Transient(format!("deezer: quota: {msg}")));
        }
        return Err(ProviderError::Fatal(format!(
            "deezer: error {}: {msg}",
            err.code
        )));
    }
    Ok(response.data)
}

fn non_empty(url: Option<String>) -> Option<String> {
    url.filter(|u| !u.trim().is_empty())
}

/// Finds an artist image by exact (normalized) name match.
pub async fn artist_image(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    name: &str,
) -> ProviderResult<String> {
    let url = format!("{BASE}/search/artist");
    let result: Option<SearchResponse<ArtistHit>> = get_json(
        client,
        limiter,
        "deezer",
        &url,
        &[("q", name), ("limit", "5")],
    )
    .await?;

    let Some(response) = result else {
        return Ok(None);
    };
    let hits = check_error(limiter, response).await?;

    let wanted = normalize_name(name);
    Ok(hits
        .into_iter()
        .find(|h| normalize_name(&h.name) == wanted)
        .and_then(|h| non_empty(h.picture_xl)))
}

/// Finds an album cover by artist + album title, exact (normalized) title match.
pub async fn album_cover(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    artist: &str,
    title: &str,
) -> ProviderResult<String> {
    let q = format!(r#"artist:"{artist}" album:"{title}""#);
    let url = format!("{BASE}/search/album");
    let result: Option<SearchResponse<AlbumHit>> = get_json(
        client,
        limiter,
        "deezer",
        &url,
        &[("q", q.as_str()), ("limit", "5")],
    )
    .await?;

    let Some(response) = result else {
        return Ok(None);
    };
    let hits = check_error(limiter, response).await?;

    let wanted = normalize_name(title);
    Ok(hits
        .into_iter()
        .find(|h| normalize_name(&h.title) == wanted)
        .and_then(|h| non_empty(h.cover_xl)))
}
