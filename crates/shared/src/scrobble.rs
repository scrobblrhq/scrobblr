use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::models::Scrobble;

/// Minimum listened duration to count as a valid scrobble (ms)
pub const MIN_LISTEN_MS: i32 = 30_000; // 30 seconds

pub const MAX_FEATURED_ARTISTS: usize = 16;
pub const MAX_NAME_LEN: usize = 300;

#[derive(Debug, Error)]
pub enum ScrobbleValidationError {
    #[error("track title is required")]
    MissingTitle,
    #[error("artist name is required")]
    MissingArtist,
    #[error("played_at timestamp is in the future")]
    FutureTimestamp,
    #[error("listen duration too short to scrobble")]
    TooShort,
    #[error("track, artist and album names may be at most {MAX_NAME_LEN} characters")]
    NameTooLong,
    #[error("a track may credit at most {MAX_FEATURED_ARTISTS} featured artists")]
    TooManyFeaturedArtists,
}

/// Input coming from the client before any DB look-ups
#[derive(Debug, Clone)]
pub struct ScrobbleInput {
    pub track_title: String,
    /// The primary artist, mirrored into `tracks.artist_id`.
    pub artist_name: String,
    /// Collaborators credited on the track, in billing order.
    pub featured_artists: Vec<String>,
    pub album_title: Option<String>,
    pub played_at: DateTime<Utc>,
    pub duration_ms: Option<i32>,
    /// How long the user actually listened (may be less than full track)
    pub listened_ms: Option<i32>,
    pub source: String,
}

/// Trims, drops blanks, removes the primary artist and de-duplicates
/// case-insensitively, preserving the order the client sent.
pub fn normalize_featured_artists(primary: &str, featured: &[String]) -> Vec<String> {
    let primary = normalize_name(primary);
    let mut seen = vec![primary];
    let mut result = Vec::new();

    for name in featured {
        let trimmed = name.trim();
        let normalized = normalize_name(trimmed);
        if normalized.is_empty() || seen.contains(&normalized) {
            continue;
        }
        seen.push(normalized);
        result.push(trimmed.to_string());
    }

    result
}

/// Validates raw scrobble input from the client.
/// Returns `Ok(())` when the scrobble meets last.fm-style rules:
///   - Track & artist must be non-empty
///   - `played_at` must not be in the future
///   - Listened at least 30 s **or** ≥ 50 % of the track duration
pub fn validate(input: &ScrobbleInput) -> Result<(), ScrobbleValidationError> {
    if input.track_title.trim().is_empty() {
        return Err(ScrobbleValidationError::MissingTitle);
    }
    if input.artist_name.trim().is_empty() {
        return Err(ScrobbleValidationError::MissingArtist);
    }
    if input.played_at > Utc::now() {
        return Err(ScrobbleValidationError::FutureTimestamp);
    }

    let too_long = |value: &str| value.chars().count() > MAX_NAME_LEN;
    if too_long(&input.track_title)
        || too_long(&input.artist_name)
        || input.album_title.as_deref().is_some_and(too_long)
        || input.featured_artists.iter().any(|a| too_long(a))
    {
        return Err(ScrobbleValidationError::NameTooLong);
    }
    if input.featured_artists.len() > MAX_FEATURED_ARTISTS {
        return Err(ScrobbleValidationError::TooManyFeaturedArtists);
    }

    // Duration check only when we know how long they listened
    if let Some(listened_ms) = input.listened_ms {
        let passes_absolute = listened_ms >= MIN_LISTEN_MS;
        let passes_relative = input
            .duration_ms
            .map(|d| d > 0 && listened_ms * 2 >= d)
            .unwrap_or(false);

        if !passes_absolute && !passes_relative {
            return Err(ScrobbleValidationError::TooShort);
        }
    }

    Ok(())
}

/// Returns `true` if the new scrobble is a duplicate of `previous`.
/// Duplicate = same track within 30 seconds of the previous one.
pub fn is_duplicate(previous: &Scrobble, new_track_id: i64, new_played_at: DateTime<Utc>) -> bool {
    if previous.track_id != new_track_id {
        return false;
    }
    let delta = (new_played_at - previous.played_at).num_seconds().abs();
    delta < 30
}

/// Normalize a name for deduplication: lowercase + remove diacritics placeholder.
/// Real unaccent happens in Postgres; this mirrors the logic for in-process checks.
pub fn normalize_name(name: &str) -> String {
    name.trim().to_lowercase()
}
