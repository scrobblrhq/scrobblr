//! Which of a track's lengths is wrong, when three sources disagree.
//!
//! MusicBrainz matches snippets, live takes and medleys; the catalog's
//! length (the first client to report the track, or Last.fm) is sometimes
//! crowd-sourced nonsense; Deezer's comes from the audio it streams. None
//! is the truth, so a source is called wrong only when the other two agree
//! (within 10 %) and it differs from both by 1.5x or more. Two sources that
//! disagree on their own decide nothing.

/// Within 10 % of each other.
pub fn agree(a: i64, b: i64) -> bool {
    (a - b).abs() * 10 <= a.max(b)
}

/// One at least 1.5 times the other.
pub fn disagree(a: i64, b: i64) -> bool {
    a.max(b) * 2 >= a.min(b) * 3
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outlier {
    MusicBrainz,
    Catalog,
    Deezer,
}

impl Outlier {
    pub fn as_str(self) -> &'static str {
        match self {
            Outlier::MusicBrainz => "musicbrainz",
            Outlier::Catalog => "catalog",
            Outlier::Deezer => "deezer",
        }
    }
}

/// The source the two others contradict, if any. Lengths in ms, positive.
pub fn outlier(musicbrainz: i64, catalog: i64, deezer: i64) -> Option<Outlier> {
    let odd_one = |odd: i64, a: i64, b: i64| agree(a, b) && disagree(odd, a) && disagree(odd, b);
    if odd_one(musicbrainz, catalog, deezer) {
        Some(Outlier::MusicBrainz)
    } else if odd_one(catalog, musicbrainz, deezer) {
        Some(Outlier::Catalog)
    } else if odd_one(deezer, musicbrainz, catalog) {
        Some(Outlier::Deezer)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1000;

    #[test]
    fn a_source_two_others_contradict_is_the_outlier() {
        // Seen in a real catalog: MusicBrainz matched a 2:08 snippet of
        // "Feel Good Inc." (3:42), an 8:30 live take of "Runaway Baby"
        // (2:27); Last.fm said 2:22 for "Bohemian Rhapsody" (5:55).
        assert_eq!(
            outlier(128 * S, 236 * S, 222 * S),
            Some(Outlier::MusicBrainz)
        );
        assert_eq!(
            outlier(510 * S, 147 * S, 148 * S),
            Some(Outlier::MusicBrainz)
        );
        assert_eq!(outlier(356 * S, 142 * S, 355 * S), Some(Outlier::Catalog));
        assert_eq!(outlier(200 * S, 201 * S, 30 * S), Some(Outlier::Deezer));
    }

    #[test]
    fn without_two_agreeing_sources_nothing_is_decided() {
        // "I Wanna Be Yours": 2:28, 4:55 and 3:03 all apart.
        assert_eq!(outlier(295 * S, 148 * S, 183 * S), None);
        // Close enough everywhere: a radio edit, rounding.
        assert_eq!(outlier(250 * S, 210 * S, 212 * S), None);
        assert_eq!(outlier(200 * S, 200 * S, 200 * S), None);
    }

    #[test]
    fn thresholds() {
        assert!(agree(200 * S, 220 * S));
        assert!(!agree(200 * S, 223 * S));
        assert!(disagree(200 * S, 300 * S));
        assert!(!disagree(200 * S, 299 * S));
    }
}
