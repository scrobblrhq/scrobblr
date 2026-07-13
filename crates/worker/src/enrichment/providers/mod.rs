pub mod coverart;
pub mod deezer;
pub mod lastfm;
pub mod musicbrainz;

use std::time::Duration;

use serde::de::DeserializeOwned;

use super::ratelimit::RateLimiter;

/// How a provider call failed.
///
/// `Transient` failures (network, 5xx, 429) make the job retry later with
/// backoff. `Fatal` failures (malformed query, unexpected body) are logged and
/// treated as "this provider contributed nothing" — they never block the job,
/// since retrying won't change the outcome.
#[derive(Debug)]
pub enum ProviderError {
    Transient(String),
    Fatal(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Transient(msg) => write!(f, "transient: {msg}"),
            ProviderError::Fatal(msg) => write!(f, "fatal: {msg}"),
        }
    }
}

/// `Ok(None)` means the provider answered but had no (confident) match — a
/// valid final result, not an error.
pub type ProviderResult<T> = Result<Option<T>, ProviderError>;

const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

/// Shared GET-and-parse-JSON helper with rate limiting and 429/503 handling.
///
/// A 404 is returned as `Ok(None)`; callers that treat 404 as "no match"
/// (Cover Art Archive, MusicBrainz lookups) get that for free.
pub async fn get_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    provider: &str,
    url: &str,
    query: &[(&str, &str)],
) -> ProviderResult<T> {
    limiter.acquire().await;

    let response = client
        .get(url)
        .query(query)
        .send()
        .await
        .map_err(|e| classify_reqwest_error(provider, &e))?;

    let status = response.status();

    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        let cooldown = retry_after(&response).unwrap_or(DEFAULT_COOLDOWN);
        limiter.penalize(cooldown).await;
        return Err(ProviderError::Transient(format!(
            "{provider}: HTTP {status}, cooling down {}s",
            cooldown.as_secs()
        )));
    }

    if status.is_server_error() {
        return Err(ProviderError::Transient(format!(
            "{provider}: HTTP {status}"
        )));
    }

    if !status.is_success() {
        return Err(ProviderError::Fatal(format!("{provider}: HTTP {status}")));
    }

    match response.json::<T>().await {
        Ok(parsed) => Ok(Some(parsed)),
        Err(e) => Err(ProviderError::Fatal(format!(
            "{provider}: bad response body: {e}"
        ))),
    }
}

fn classify_reqwest_error(provider: &str, e: &reqwest::Error) -> ProviderError {
    // Connectivity and timeout problems are worth retrying; anything else
    // (builder misuse, redirect loops) won't fix itself.
    if e.is_timeout() || e.is_connect() || e.is_request() {
        ProviderError::Transient(format!("{provider}: {e}"))
    } else {
        ProviderError::Fatal(format!("{provider}: {e}"))
    }
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}
