//! Deduplication for listening-history imports.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, TimeDelta, Utc};

/// Source recorded on imported scrobbles (`scrobbles.import_id` is the
/// field rules should trust: clients can send any source).
pub const LASTFM_SOURCE: &str = "lastfm_import";

/// How far apart an imported play and a live scrobble of the same track can
/// be and still be the same listen, reported to both services. Clients stamp
/// the start or the end of a play, so this spans a track.
pub const LIVE_OVERLAP: TimeDelta = TimeDelta::minutes(10);

/// Re-imports start this far before the previous import's window ended:
/// Last.fm accepts scrobbles up to two weeks late.
pub const REIMPORT_OVERLAP: TimeDelta = TimeDelta::days(14);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Play {
    pub played_at: DateTime<Utc>,
    pub track_id: i64,
}

/// A scrobble already stored for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stored {
    pub played_at: DateTime<Utc>,
    pub track_id: i64,
    pub imported: bool,
}

/// Indices of the `incoming` plays to insert, in input order.
///
/// Dropped: repeats within `incoming`; plays an earlier import already
/// stored (same second, same track); and plays matching a live scrobble of
/// the same track within `live_overlap`, each live scrobble absorbing at
/// most one play so a looped track keeps all its repeats.
pub fn new_plays(incoming: &[Play], stored: &[Stored], live_overlap: TimeDelta) -> Vec<usize> {
    let imported: HashSet<(DateTime<Utc>, i64)> = stored
        .iter()
        .filter(|s| s.imported)
        .map(|s| (s.played_at, s.track_id))
        .collect();
    let mut live: HashMap<i64, Vec<(DateTime<Utc>, bool)>> = HashMap::new();
    for s in stored.iter().filter(|s| !s.imported) {
        live.entry(s.track_id)
            .or_default()
            .push((s.played_at, false));
    }

    let mut order: Vec<usize> = (0..incoming.len()).collect();
    order.sort_by_key(|&i| (incoming[i].played_at, incoming[i].track_id, i));

    let mut seen = HashSet::new();
    let mut keep = vec![false; incoming.len()];
    for i in order {
        let play = incoming[i];
        let key = (play.played_at, play.track_id);
        if !seen.insert(key) || imported.contains(&key) {
            continue;
        }
        let nearest = live.get_mut(&play.track_id).and_then(|rows| {
            rows.iter_mut()
                .filter(|(at, used)| !*used && (*at - play.played_at).abs() <= live_overlap)
                .min_by_key(|(at, _)| (*at - play.played_at).abs())
        });
        match nearest {
            Some(row) => row.1 = true,
            None => keep[i] = true,
        }
    }
    (0..incoming.len()).filter(|&i| keep[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: i64) -> DateTime<Utc> {
        "2020-05-01T12:00:00Z".parse::<DateTime<Utc>>().unwrap() + TimeDelta::minutes(minute)
    }

    fn play(minute: i64, track_id: i64) -> Play {
        Play {
            played_at: at(minute),
            track_id,
        }
    }

    fn stored(minute: i64, track_id: i64, imported: bool) -> Stored {
        Stored {
            played_at: at(minute),
            track_id,
            imported,
        }
    }

    #[test]
    fn first_import_keeps_everything() {
        let incoming = [play(0, 1), play(4, 2), play(8, 1)];
        assert_eq!(new_plays(&incoming, &[], LIVE_OVERLAP), [0, 1, 2]);
    }

    #[test]
    fn reimport_adds_only_what_is_missing() {
        let incoming = [play(0, 1), play(4, 2), play(8, 1), play(12, 3)];
        let stored = [stored(0, 1, true), stored(4, 2, true), stored(8, 1, true)];
        assert_eq!(new_plays(&incoming, &stored, LIVE_OVERLAP), [3]);
    }

    #[test]
    fn repeats_within_a_batch_are_kept_once_but_simultaneous_tracks_are_not_repeats() {
        let incoming = [play(0, 1), play(0, 1), play(0, 2)];
        assert_eq!(new_plays(&incoming, &[], LIVE_OVERLAP), [0, 2]);
    }

    #[test]
    fn an_earlier_import_only_matches_exactly() {
        let incoming = [play(3, 1)];
        assert_eq!(
            new_plays(&incoming, &[stored(0, 1, true)], LIVE_OVERLAP),
            [0]
        );
    }

    #[test]
    fn plays_scrobbled_live_too_are_skipped_one_for_one() {
        // A 3-minute track looped three times, scrobbled live at the end of
        // each play and to Last.fm at the start, plus a fourth play that
        // only Last.fm saw.
        let stored = [
            stored(3, 7, false),
            stored(6, 7, false),
            stored(9, 7, false),
        ];
        let incoming = [play(0, 7), play(3, 7), play(6, 7), play(9, 7)];
        assert_eq!(new_plays(&incoming, &stored, LIVE_OVERLAP).len(), 1);
    }

    #[test]
    fn live_scrobbles_of_other_tracks_or_far_away_do_not_match() {
        let stored = [stored(0, 8, false), stored(30, 7, false)];
        let incoming = [play(0, 7), play(11, 7)];
        assert_eq!(new_plays(&incoming, &stored, LIVE_OVERLAP), [0, 1]);
    }
}
