//! MusicBrainz — the canonical metadata source.
//!
//! Provides MBIDs (the identity anchor the schema is built around), recording
//! durations and release dates. No API key, but a strict 1 request/second
//! limit and a mandatory identifying User-Agent.

use uuid::Uuid;

use super::{ProviderResult, get_json};
use crate::enrichment::ratelimit::RateLimiter;

const BASE: &str = "https://musicbrainz.org/ws/2";

/// Minimum search score (0-100) to accept a match. Two-field queries
/// (title + artist) are reliable at 90; artist-only searches are noisier, so
/// they require 95.
const MIN_SCORE: i32 = 90;
const MIN_SCORE_ARTIST: i32 = 95;

#[derive(Debug, serde::Deserialize)]
pub struct Recording {
    pub id: Uuid,
    pub score: Option<i32>,
    /// Duration in milliseconds.
    pub length: Option<i64>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    pub releases: Vec<Release>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ArtistCredit {
    pub artist: CreditedArtist,
}

#[derive(Debug, serde::Deserialize)]
pub struct CreditedArtist {
    pub id: Uuid,
}

#[derive(Debug, serde::Deserialize)]
pub struct Release {
    pub id: Uuid,
    pub score: Option<i32>,
    pub title: String,
    /// "YYYY", "YYYY-MM" or "YYYY-MM-DD".
    pub date: Option<String>,
    pub status: Option<String>,
    #[serde(rename = "release-group")]
    pub release_group: Option<ReleaseGroup>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ReleaseGroup {
    pub id: Uuid,
}

#[derive(Debug, serde::Deserialize)]
struct RecordingSearch {
    #[serde(default)]
    recordings: Vec<Recording>,
}

#[derive(Debug, serde::Deserialize)]
struct ReleaseSearch {
    #[serde(default)]
    releases: Vec<Release>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ArtistMatch {
    pub id: Uuid,
    pub score: Option<i32>,
}

#[derive(Debug, serde::Deserialize)]
struct ArtistSearch {
    #[serde(default)]
    artists: Vec<ArtistMatch>,
}

/// Escapes a value for use inside a quoted Lucene phrase.
fn lucene_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn score_ok(score: Option<i32>, min: i32) -> bool {
    score.is_some_and(|s| s >= min)
}

/// Searches for a recording by title + artist name. Returns the best match at
/// or above the score threshold, with its artist credits and releases.
pub async fn search_recording(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    title: &str,
    artist: &str,
) -> ProviderResult<Recording> {
    let query = format!(
        r#"recording:"{}" AND artist:"{}""#,
        lucene_quote(title),
        lucene_quote(artist)
    );
    let url = format!("{BASE}/recording");
    let result: Option<RecordingSearch> = get_json(
        client,
        limiter,
        "musicbrainz",
        &url,
        &[("query", query.as_str()), ("fmt", "json"), ("limit", "5")],
    )
    .await?;

    Ok(result.and_then(|r| {
        r.recordings
            .into_iter()
            .find(|rec| score_ok(rec.score, MIN_SCORE))
    }))
}

/// Direct lookup of a recording we already have an MBID for — used to fill
/// missing duration/releases without a search. 404 (merged/deleted MBID)
/// yields `Ok(None)`.
pub async fn lookup_recording(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    mbid: Uuid,
) -> ProviderResult<Recording> {
    let url = format!("{BASE}/recording/{mbid}");
    get_json(
        client,
        limiter,
        "musicbrainz",
        &url,
        &[
            ("fmt", "json"),
            ("inc", "artist-credits releases release-groups"),
        ],
    )
    .await
}

/// Searches for an artist by name. Higher score bar than the two-field
/// searches — single-term artist queries are the easiest to mismatch.
pub async fn search_artist(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    name: &str,
) -> ProviderResult<ArtistMatch> {
    let query = format!(r#"artist:"{}""#, lucene_quote(name));
    let url = format!("{BASE}/artist");
    let result: Option<ArtistSearch> = get_json(
        client,
        limiter,
        "musicbrainz",
        &url,
        &[("query", query.as_str()), ("fmt", "json"), ("limit", "5")],
    )
    .await?;

    Ok(result.and_then(|r| {
        r.artists
            .into_iter()
            .find(|a| score_ok(a.score, MIN_SCORE_ARTIST))
    }))
}

/// Searches for a release by title + artist name.
pub async fn search_release(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    title: &str,
    artist: &str,
) -> ProviderResult<Release> {
    let query = format!(
        r#"release:"{}" AND artist:"{}""#,
        lucene_quote(title),
        lucene_quote(artist)
    );
    let url = format!("{BASE}/release");
    let result: Option<ReleaseSearch> = get_json(
        client,
        limiter,
        "musicbrainz",
        &url,
        &[("query", query.as_str()), ("fmt", "json"), ("limit", "5")],
    )
    .await?;

    Ok(result.and_then(|r| {
        let mut candidates: Vec<Release> = r
            .releases
            .into_iter()
            .filter(|rel| score_ok(rel.score, MIN_SCORE))
            .collect();
        // Prefer official releases with a date — those carry the metadata we
        // actually want and their cover art is the most likely to exist.
        candidates.sort_by_key(|rel| {
            let official = rel.status.as_deref() == Some("Official");
            let dated = rel.date.is_some();
            std::cmp::Reverse((official, dated))
        });
        candidates.into_iter().next()
    }))
}

/// Fetches the release-group MBID of a release — needed for the Cover Art
/// Archive release-group fallback when the album's mbid was already known and
/// no search (which would have carried the release-group) was performed.
pub async fn lookup_release_group(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    release_mbid: Uuid,
) -> ProviderResult<Uuid> {
    #[derive(serde::Deserialize)]
    struct ReleaseLookup {
        #[serde(rename = "release-group")]
        release_group: Option<ReleaseGroup>,
    }

    let url = format!("{BASE}/release/{release_mbid}");
    let result: Option<ReleaseLookup> = get_json(
        client,
        limiter,
        "musicbrainz",
        &url,
        &[("fmt", "json"), ("inc", "release-groups")],
    )
    .await?;

    Ok(result.and_then(|r| r.release_group.map(|rg| rg.id)))
}

/// Parses MusicBrainz's flexible date format ("2004", "2004-05",
/// "2004-05-12") into a date, defaulting missing parts to the first.
pub fn parse_mb_date(date: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .or_else(|_| chrono::NaiveDate::parse_from_str(&format!("{date}-01"), "%Y-%m-%d"))
        .or_else(|_| chrono::NaiveDate::parse_from_str(&format!("{date}-01-01"), "%Y-%m-%d"))
        .ok()
}
