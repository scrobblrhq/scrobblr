//! Which album a track belongs to (docs/social.md, Albums).
//!
//! Every account has one say per track: the album its latest play naming
//! one named. An album's weight is the sum of its accounts' weights (one
//! each, in thousandths: nothing for an account too young or waiting to be
//! erased, its coordination factor for a flagged one); MusicBrainz listing
//! the track's recording on a release of that title doubles it. The album
//! with the most weight takes the track only by half again the current
//! album's weight and one account more, so a close race doesn't flip it
//! back and forth, and an album needs accounts behind it to take a track.

use crate::ranking::FULL;

/// An album a track was played from, as its accounts weigh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub album_id: i64,
    /// Thousandths of an account.
    pub weight: i64,
    /// MusicBrainz lists the recording on a release of the album's title.
    pub confirmed: bool,
}

/// A new album's score must reach the current one's times this, in percent.
pub const SWITCH_RATIO_PCT: i64 = 150;
/// And lead it by this much (one account).
pub const MIN_LEAD: i64 = FULL;
/// What a release MusicBrainz confirms multiplies its weight by.
pub const CONFIRMED_FACTOR: i64 = 2;

pub fn score(c: &Candidate) -> i64 {
    if c.confirmed {
        c.weight * CONFIRMED_FACTOR
    } else {
        c.weight
    }
}

/// The album `current` should give way to, if any: the best scored
/// candidate (then the most weight, then the oldest album), when it beats
/// the current album's score by [`SWITCH_RATIO_PCT`] and [`MIN_LEAD`]. A
/// track without an album takes the best one with any weight.
pub fn choose(current: Option<i64>, candidates: &[Candidate]) -> Option<i64> {
    let best = candidates
        .iter()
        .filter(|c| c.weight > 0)
        .max_by_key(|c| (score(c), c.weight, std::cmp::Reverse(c.album_id)))?;
    let Some(current) = current else {
        return Some(best.album_id);
    };
    if best.album_id == current {
        return None;
    }
    let held = candidates
        .iter()
        .find(|c| c.album_id == current)
        .map_or(0, score);
    let wins = score(best) * 100 >= held * SWITCH_RATIO_PCT && score(best) - held >= MIN_LEAD;
    wins.then_some(best.album_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(album_id: i64, accounts: i64) -> Candidate {
        Candidate {
            album_id,
            weight: accounts * FULL,
            confirmed: false,
        }
    }

    fn confirmed(album_id: i64, accounts: i64) -> Candidate {
        Candidate {
            confirmed: true,
            ..album(album_id, accounts)
        }
    }

    #[test]
    fn a_track_without_an_album_takes_the_one_most_accounts_named() {
        assert_eq!(choose(None, &[album(1, 2), album(2, 3)]), Some(2));
        assert_eq!(
            choose(None, &[album(1, 2), album(2, 2)]),
            Some(1),
            "a tie goes to the oldest album"
        );
        assert_eq!(
            choose(None, &[album(1, 0)]),
            None,
            "accounts that don't count choose nothing"
        );
        assert_eq!(choose(None, &[]), None);
    }

    #[test]
    fn a_new_album_needs_half_again_the_current_ones_weight_and_one_account_more() {
        assert_eq!(choose(Some(1), &[album(1, 10), album(2, 14)]), None);
        assert_eq!(choose(Some(1), &[album(1, 10), album(2, 15)]), Some(2));
        // Small numbers: the lead matters.
        assert_eq!(choose(Some(1), &[album(1, 1), album(2, 1)]), None);
        assert_eq!(choose(Some(1), &[album(1, 1), album(2, 2)]), Some(2));
        assert_eq!(choose(Some(1), &[album(1, 2), album(2, 3)]), Some(2));
        assert_eq!(choose(Some(1), &[album(1, 3), album(2, 4)]), None);
        // An album with no account left behind it gives way to any.
        assert_eq!(choose(Some(1), &[album(2, 1)]), Some(2));
        assert_eq!(choose(Some(1), &[album(1, 0), album(2, 1)]), Some(2));
        assert_eq!(choose(Some(1), &[album(2, 0)]), None);
        assert_eq!(choose(Some(1), &[album(1, 5)]), None);
    }

    #[test]
    fn once_switched_it_takes_as_much_again_to_switch_back() {
        let mut current = Some(1);
        let mut votes = [album(1, 10), album(2, 10)];
        for _ in 0..5 {
            votes[1].weight += FULL;
            if let Some(next) = choose(current, &votes) {
                current = Some(next);
            }
        }
        assert_eq!(current, Some(2), "15 against 10");
        votes[0].weight += 5 * FULL;
        assert_eq!(choose(current, &votes), None, "15 against 15 stays");
        votes[0].weight += 7 * FULL;
        assert_eq!(choose(current, &votes), None, "22 against 15 stays");
        votes[0].weight += FULL;
        assert_eq!(choose(current, &votes), Some(1), "23 against 15 goes back");
    }

    #[test]
    fn a_release_musicbrainz_confirms_counts_double() {
        assert_eq!(choose(Some(1), &[confirmed(1, 10), album(2, 29)]), None);
        assert_eq!(choose(Some(1), &[confirmed(1, 10), album(2, 30)]), Some(2));
        assert_eq!(choose(Some(2), &[confirmed(1, 8), album(2, 10)]), Some(1));
        assert_eq!(choose(None, &[confirmed(1, 3), album(2, 5)]), Some(1));
    }

    #[test]
    fn fractions_of_accounts_count_as_fractions() {
        let flagged = Candidate {
            album_id: 2,
            weight: 40 * 250,
            confirmed: false,
        };
        assert_eq!(
            choose(Some(1), &[album(1, 10), flagged]),
            None,
            "40 accounts at a quarter each are 10"
        );
    }
}
