//! Deezer — image fallback and track lengths. No API key required (~50
//! requests / 5 s per IP).
//!
//! Covers the two gaps the MBID-based chain leaves: artist images (which
//! MusicBrainz doesn't host at all) and album covers the Cover Art Archive
//! doesn't have; and lengths for tracks no other source has one for.
//! Matches are accepted only on a normalized name equality — a wrong image
//! or length is worse than none.

use std::time::Duration;

use shared::scrobble::normalize_name;

use super::musicbrainz::undecorated_title;
use super::{ProviderError, ProviderResult, get_json};
use crate::enrichment::ratelimit::RateLimiter;

pub const BASE: &str = "https://api.deezer.com";

/// How long Deezer's previews are.
const PREVIEW_SECS: i64 = 30;

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
struct TrackHit {
    title: String,
    #[serde(default)]
    title_short: String,
    /// "(Acoustic)", "(Remastered 2011)"…
    #[serde(default)]
    title_version: String,
    /// Seconds.
    duration: Option<i64>,
    artist: TrackHitArtist,
}

#[derive(Debug, serde::Deserialize)]
struct TrackHitArtist {
    name: String,
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

/// A track's length, from the first search hit whose artist and title
/// match ([`same_title`], [`same_artist`]), searching again without notes
/// that leave the recording unchanged (" - Remastered 2011", " Ft. X") when
/// the full title finds nothing. `base` is [`BASE`] outside tests.
pub async fn track_length(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    base: &str,
    artist: &str,
    title: &str,
) -> ProviderResult<i32> {
    if let Some(length) = search_track_length(client, limiter, base, artist, title, title).await? {
        return Ok(Some(length));
    }
    match undecorated_title(title) {
        Some(plain) => search_track_length(client, limiter, base, artist, &plain, title).await,
        None => Ok(None),
    }
}

async fn search_track_length(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    base: &str,
    artist: &str,
    query_title: &str,
    title: &str,
) -> ProviderResult<i32> {
    // Track search ignores the `artist:` filter (it finds nothing, even
    // alone); plain words find the track, and the hits are checked below.
    let q = format!("{artist} {query_title}");
    let url = format!("{base}/search/track");
    let result: Option<SearchResponse<TrackHit>> = get_json(
        client,
        limiter,
        "deezer",
        &url,
        &[("q", q.as_str()), ("limit", "10")],
    )
    .await?;

    let Some(response) = result else {
        return Ok(None);
    };
    let hits = check_error(limiter, response).await?;
    Ok(hits
        .into_iter()
        // Deezer lists some tracks only as their 30-second preview, and so
        // does MusicBrainz now and then: a length that short is no length.
        .filter(|h| h.duration.is_some_and(|secs| secs > PREVIEW_SECS))
        .filter(|h| same_artist(artist, &h.artist.name))
        .find(|h| {
            same_title(title, &h.title)
                || same_title(title, &format!("{} {}", h.title_short, h.title_version))
        })
        .and_then(|h| h.duration)
        .and_then(|secs| shared::scrobble::plausible_duration_ms(secs.saturating_mul(1000))))
}

/// Titles as words: "Song - Acoustic", "Song (Acoustic)" and
/// "Song [Acoustic]" are one title.
fn title_words(title: &str) -> String {
    normalize_name(title)
        .replace(" - ", " ")
        .replace(['(', ')', '[', ']'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The same title, written either way, or once notes that leave the
/// recording unchanged are dropped from both. Version notes ("Acoustic",
/// "Sped Up", "Live") must match: they are other recordings.
fn same_title(ours: &str, theirs: &str) -> bool {
    let forms = |title: &str| {
        let mut forms = vec![title_words(title)];
        forms.extend(undecorated_title(title).map(|t| title_words(&t)));
        forms
    };
    let ours = forms(ours);
    forms(theirs)
        .iter()
        .any(|t| !t.is_empty() && ours.contains(t))
}

/// Their artist is ours, or the first of the artists ours credits ("A, B",
/// "A & B", "A feat. B"…).
fn same_artist(ours: &str, theirs: &str) -> bool {
    let (ours, theirs) = (normalize_name(ours), normalize_name(theirs));
    if theirs.is_empty() {
        return false;
    }
    ours == theirs
        || ours.strip_prefix(&theirs).is_some_and(|rest| {
            [",", " &", " y ", " x ", " and ", " feat", " ft.", " with "]
                .iter()
                .any(|sep| rest.starts_with(sep))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_match_written_either_way_but_not_other_versions() {
        assert!(same_title("Tek It - Acoustic", "Tek It (Acoustic)"));
        assert!(same_title("Song", "SONG"));
        assert!(same_title("Going Under - Remastered 2023", "Going Under"));
        assert!(same_title("Everybody Ft. Ty Dolla $ign", "Everybody"));
        assert!(same_title(
            "Black Sheep - Brie Larson Vocal Version",
            "Black Sheep (Brie Larson Vocal Version)"
        ));
        assert!(!same_title("Cats - Sped Up", "Cats"));
        assert!(!same_title("Duvet - acoustic", "Duvet"));
        assert!(!same_title("Song", "Other Song"));
    }

    #[test]
    fn artists_match_ours_or_the_first_we_credit() {
        assert!(same_artist("Radiohead", "radiohead"));
        assert!(same_artist(
            "No Te Va Gustar, NICKI NICOLE",
            "No Te Va Gustar"
        ));
        assert!(same_artist(
            "Black Eyed Peas, Shakira y David Guetta",
            "Black Eyed Peas"
        ));
        assert!(same_artist("Rihanna feat. Mikky Ekko", "Rihanna"));
        assert!(!same_artist("Radiohead", "Radio"));
        assert!(!same_artist("Shakira", "Black Eyed Peas"));
        assert!(!same_artist("Radiohead", ""));
    }
}
