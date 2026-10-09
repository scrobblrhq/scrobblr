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
    #[serde(default)]
    pub title: String,
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
    /// The name as credited on this recording.
    #[serde(default)]
    pub name: String,
    pub artist: CreditedArtist,
}

#[derive(Debug, serde::Deserialize)]
pub struct CreditedArtist {
    pub id: Uuid,
    #[serde(default)]
    pub name: String,
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
/// or above the score threshold that isn't in `skip`, with its artist
/// credits and releases.
pub async fn search_recording(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    title: &str,
    artist: &str,
    skip: &[Uuid],
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
            .find(|rec| score_ok(rec.score, MIN_SCORE) && !skip.contains(&rec.id))
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

/// Whether a recording someone else pointed us at (Last.fm's mbid) is
/// plausibly this track: the same title, give or take a trailing
/// " (…)" or " - …" qualifier, and a credited artist named like ours.
pub fn matches_track(recording: &Recording, title: &str, artist: &str) -> bool {
    fn base(s: &str) -> String {
        let s = s.trim().to_lowercase();
        let cut = [" (", " [", " - "]
            .iter()
            .filter_map(|sep| s.find(sep))
            .min()
            .unwrap_or(s.len());
        s[..cut].trim().to_string()
    }
    let artist = artist.trim().to_lowercase();
    let same_title = base(&recording.title) == base(title) && !base(title).is_empty();
    let same_artist = recording.artist_credit.iter().any(|credit| {
        [&credit.name, &credit.artist.name].into_iter().any(|name| {
            let name = name.trim().to_lowercase();
            !name.is_empty() && (name == artist || artist.contains(&name))
        })
    });
    same_title && same_artist
}

/// The title without trailing notes that leave the recording unchanged, as
/// streaming services add them ("Song - Remastered 2011", "Song (feat. X)",
/// "Song (From \"Film\")"), or `None` if it has none. MusicBrainz titles
/// carry no such notes. Versions ("Acoustic", "Sped Up", "Live") are kept:
/// they are other recordings.
pub fn undecorated_title(title: &str) -> Option<String> {
    fn same_recording(note: &str) -> bool {
        let note = note.trim().to_ascii_lowercase();
        note.contains("remaster")
            || [
                "feat.",
                "feat ",
                "ft.",
                "ft ",
                "featuring ",
                "with ",
                "con ",
                "from ",
            ]
            .iter()
            .any(|prefix| note.starts_with(prefix))
    }

    let mut base = title.trim();
    loop {
        // A trailing "(…)" or "[…]" group, else a trailing " - …" note.
        let note = match base.chars().last() {
            Some(close @ (')' | ']')) => {
                let open = if close == ')' { '(' } else { '[' };
                base.rfind(open).map(|i| (i, &base[i + 1..base.len() - 1]))
            }
            _ => base.rfind(" - ").map(|i| (i, &base[i + 3..])),
        };
        match note {
            Some((i, note)) if same_recording(note) && !base[..i].trim().is_empty() => {
                base = base[..i].trim_end();
            }
            _ => break,
        }
    }
    // An inline credit: "Song Ft. X".
    let lower = base.to_ascii_lowercase();
    if let Some(i) = [" feat. ", " ft. ", " featuring "]
        .iter()
        .filter_map(|credit| lower.find(credit))
        .min()
    {
        base = base[..i].trim_end();
    }
    (base != title.trim()).then(|| base.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(title: &str, credits: &[&str]) -> Recording {
        Recording {
            id: Uuid::nil(),
            title: title.into(),
            score: None,
            length: Some(240_000),
            artist_credit: credits
                .iter()
                .map(|name| ArtistCredit {
                    name: name.to_string(),
                    artist: CreditedArtist {
                        id: Uuid::nil(),
                        name: name.to_string(),
                    },
                })
                .collect(),
            releases: Vec::new(),
        }
    }

    #[test]
    fn decorations_that_keep_the_recording_are_dropped() {
        for (title, base) in [
            ("Going Under - Remastered 2023", "Going Under"),
            ("Mil Horas - 1994 Remastered Version", "Mil Horas"),
            (
                "Una Nube Cuelga Sobre Mí - Remasterizado",
                "Una Nube Cuelga Sobre Mí",
            ),
            ("Roundabout - 2024 Remaster", "Roundabout"),
            ("BAND4BAND (feat. Lil Baby)", "BAND4BAND"),
            ("Everybody Ft. Ty Dolla $ign", "Everybody"),
            ("Baila Conmigo (with Rauw Alejandro)", "Baila Conmigo"),
            ("Feel It (From “Invincible”)", "Feel It"),
            (
                "Black Sheep (Brie Larson Vocal Version) (con Brie Larson)",
                "Black Sheep (Brie Larson Vocal Version)",
            ),
            ("Song [Remastered] (feat. X)", "Song"),
        ] {
            assert_eq!(undecorated_title(title).as_deref(), Some(base), "{title}");
        }
        for title in [
            "Tek It - Acoustic",
            "Cats - Sped Up",
            "Last Friday Night (T.G.I.F.)",
            "D>E>A>T>H>M>E>T>A>L",
            "(feat. Nobody)",
            "Song",
        ] {
            assert_eq!(undecorated_title(title), None, "{title}");
        }
    }

    #[test]
    fn a_hinted_recording_must_match_title_and_artist() {
        let rec = recording("Airbag (Remastered)", &["Radiohead"]);
        assert!(matches_track(&rec, "Airbag", "radiohead"));
        assert!(matches_track(
            &recording("Airbag", &["Radiohead"]),
            "Airbag - 2009 Remaster",
            "Radiohead"
        ));
        assert!(matches_track(
            &recording("Stay", &["Rihanna", "Mikky Ekko"]),
            "Stay",
            "Rihanna feat. Mikky Ekko"
        ));
        assert!(!matches_track(&rec, "Lucky", "Radiohead"));
        assert!(!matches_track(&rec, "Airbag", "Muse"));
        assert!(!matches_track(
            &recording("", &["Radiohead"]),
            "",
            "Radiohead"
        ));
    }
}
