//! Cover Art Archive — album covers, keyed by MusicBrainz release or
//! release-group MBID. No auth. Only useful after MusicBrainz has resolved an
//! MBID, which is why it sits behind MusicBrainz in the fallback chain.

use std::time::Duration;

use uuid::Uuid;

use super::{ProviderError, ProviderResult};
use crate::enrichment::ratelimit::RateLimiter;

#[derive(Debug, Clone, Copy)]
pub enum CoverKind {
    Release,
    ReleaseGroup,
}

impl CoverKind {
    fn segment(self) -> &'static str {
        match self {
            CoverKind::Release => "release",
            CoverKind::ReleaseGroup => "release-group",
        }
    }
}

/// Checks whether front cover art exists and returns its canonical CAA URL.
///
/// Uses a HEAD request against the stable `/front-500` endpoint instead of
/// fetching the image manifest: we store the canonical URL (which CAA keeps
/// redirecting to the current image) rather than a direct archive.org link
/// that can rot.
pub async fn front_cover_url(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    kind: CoverKind,
    mbid: Uuid,
) -> ProviderResult<String> {
    let url = format!(
        "https://coverartarchive.org/{}/{mbid}/front-500",
        kind.segment()
    );

    limiter.acquire().await;

    let response = client.head(&url).send().await.map_err(|e| {
        if e.is_timeout() || e.is_connect() || e.is_request() {
            ProviderError::Transient(format!("coverart: {e}"))
        } else {
            ProviderError::Fatal(format!("coverart: {e}"))
        }
    })?;

    let status = response.status();
    match status {
        reqwest::StatusCode::OK => Ok(Some(url)),
        reqwest::StatusCode::NOT_FOUND => Ok(None),
        reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::SERVICE_UNAVAILABLE => {
            limiter.penalize(Duration::from_secs(60)).await;
            Err(ProviderError::Transient(format!("coverart: HTTP {status}")))
        }
        s if s.is_server_error() => {
            Err(ProviderError::Transient(format!("coverart: HTTP {status}")))
        }
        _ => Ok(None), // e.g. 400 for a malformed mbid — nothing to gain by retrying
    }
}
