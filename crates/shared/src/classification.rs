//! Scrobble classification rules (anti-botting, shadow mode).
//!
//! Pure: the worker loads one user's scrobbles for a UTC day plus the
//! lookback before it, and stores what [`classify`] returns. The result
//! depends only on those scrobbles and [`BudgetParams`], so reclassifying is
//! repeatable.
//!
//! **Duplicates.** A scrobble of the track the user scrobbled less than
//! [`repeat_window_ms`] before can't be a new listen: several scrobblers, or
//! one retrying, report the same play, often seconds or minutes apart. It is
//! `duplicate`: not counted and taking up no budget.
//!
//! **Listening-time budget.** Nobody can hear more music than real time
//! allows. Each scrobble occupies some listening time (see [`occupancy_ms`]);
//! a scrobble is `suspect` when the occupancy of every scrobble in the window
//! ending at it, itself included, exceeds `window * max_ratio + slack`. The
//! ratio tolerates several devices playing at once, the slack imprecise
//! durations. When the next scrobble starts before a play could have ended,
//! the play was skipped: from then on it occupies the time until that
//! scrobble, but never less than its [`scrobble_point_ms`], the least
//! listening a valid scrobble claims. A scrobble with no known track length
//! is `no_data` and takes up no budget, so it can never push another one over.
//!
//! **Lengths.** MusicBrainz, the catalog and the client each get some tracks
//! wrong (snippets, live versions, crowd-sourced values), so none is taken as
//! the truth: occupancy uses the longest known length, so a wrong short one
//! can't be exploited, and the repeat window the shortest, so a wrong long
//! one can't swallow real replays.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, TimeDelta, Utc};
use thiserror::Error;

use crate::scrobble::MIN_LISTEN_MS;

/// Bump when the rule's logic changes, so stored labels are reclassified.
pub const RULES_VERSION: u32 = 2;

pub const REASON_NO_DURATION: &str = "no_duration";
pub const REASON_LISTENING_BUDGET: &str = "listening_budget";
pub const REASON_REPEAT: &str = "repeat";

/// Last.fm's scrobble point is half the track, but at most 4 minutes in.
const SCROBBLE_POINT_CAP_MS: i64 = 240_000;
/// Replays sit right at the track length; the margin covers lengths and
/// timestamps that are a little off.
const REPEAT_PERMILLE: i64 = 900;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetParams {
    pub window_ms: i64,
    /// Allowed listening time per unit of real time, in thousandths.
    pub max_ratio_permille: i64,
    pub slack_ms: i64,
}

#[derive(Debug, Error)]
pub enum ParamsError {
    #[error("window must be between 60 s and 24 h")]
    Window,
    #[error("max ratio must be between 1 and 100")]
    Ratio,
    #[error("slack must be between 0 and 24 h")]
    Slack,
}

impl Default for BudgetParams {
    fn default() -> Self {
        Self {
            window_ms: 3_600_000,
            max_ratio_permille: 2_000,
            slack_ms: 900_000,
        }
    }
}

impl BudgetParams {
    pub fn new(window_secs: i64, max_ratio: f64, slack_secs: i64) -> Result<Self, ParamsError> {
        const DAY_SECS: i64 = 86_400;
        if !(60..=DAY_SECS).contains(&window_secs) {
            return Err(ParamsError::Window);
        }
        if !(1.0..=100.0).contains(&max_ratio) {
            return Err(ParamsError::Ratio);
        }
        if !(0..=DAY_SECS).contains(&slack_secs) {
            return Err(ParamsError::Slack);
        }
        Ok(Self {
            window_ms: window_secs * 1000,
            max_ratio_permille: (max_ratio * 1000.0).round() as i64,
            slack_ms: slack_secs * 1000,
        })
    }

    pub fn budget_ms(&self) -> i64 {
        self.window_ms * self.max_ratio_permille / 1000 + self.slack_ms
    }

    pub fn window(&self) -> TimeDelta {
        TimeDelta::milliseconds(self.window_ms)
    }

    /// How far before the first labelled scrobble [`classify`] must see:
    /// one window, plus a repeat window to tell its duplicates apart.
    pub fn lookback(&self) -> TimeDelta {
        TimeDelta::milliseconds(self.window_ms + SCROBBLE_POINT_CAP_MS)
    }

    /// Identifies the rule version and thresholds; stored labels made under
    /// another fingerprint are stale.
    pub fn fingerprint(&self) -> String {
        format!(
            "listening_budget/v{RULES_VERSION} window_ms={} max_ratio_permille={} slack_ms={}",
            self.window_ms, self.max_ratio_permille, self.slack_ms
        )
    }
}

/// One scrobble as the rule sees it.
#[derive(Debug, Clone)]
pub struct Play {
    pub id: i64,
    pub track_id: i64,
    pub played_at: DateTime<Utc>,
    pub mb_duration_ms: Option<i32>,
    /// `tracks.duration_ms`: whichever client reported the track first.
    pub catalog_duration_ms: Option<i32>,
    /// The track length this scrobble's client reported.
    pub reported_duration_ms: Option<i32>,
    pub listened_ms: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    Counted,
    Suspect,
    Duplicate,
    NoData,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Counted => "counted",
            Status::Suspect => "suspect",
            Status::Duplicate => "duplicate",
            Status::NoData => "no_data",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationSource {
    MusicBrainz,
    Catalog,
    Reported,
}

impl DurationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DurationSource::MusicBrainz => "musicbrainz",
            DurationSource::Catalog => "catalog",
            DurationSource::Reported => "reported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub id: i64,
    pub status: Status,
    pub reason: Option<&'static str>,
    /// Where the longest known length came from.
    pub duration_source: Option<DurationSource>,
    /// Listening time charged when the scrobble was labelled; 0 when it
    /// takes up no budget.
    pub occupancy_ms: i64,
    pub load_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lengths {
    pub longest: i64,
    pub source: DurationSource,
    pub shortest: i64,
}

/// The known lengths of the play's track. On a tie the source named is the
/// one a client can least choose: MusicBrainz, then the catalog (a bot can
/// only set it on tracks nobody reported before), then this play's own.
pub fn lengths(play: &Play) -> Option<Lengths> {
    let known = [
        (play.mb_duration_ms, DurationSource::MusicBrainz),
        (play.catalog_duration_ms, DurationSource::Catalog),
        (play.reported_duration_ms, DurationSource::Reported),
    ];
    known
        .into_iter()
        .filter_map(|(ms, source)| ms.filter(|d| *d > 0).map(|d| (i64::from(d), source)))
        .fold(None, |acc: Option<Lengths>, (ms, source)| {
            Some(match acc {
                None => Lengths {
                    longest: ms,
                    source,
                    shortest: ms,
                },
                Some(l) if ms > l.longest => Lengths {
                    longest: ms,
                    source,
                    ..l
                },
                Some(l) => Lengths {
                    shortest: l.shortest.min(ms),
                    ..l
                },
            })
        })
}

/// The least listening a valid scrobble of a track this long claims.
pub fn scrobble_point_ms(length_ms: i64) -> i64 {
    (length_ms / 2).min(SCROBBLE_POINT_CAP_MS)
}

/// A scrobble this soon after one of the same track is a duplicate: the
/// track can't have played again in between. Without a known length, the
/// least listening any valid scrobble needs.
pub fn repeat_window_ms(play: &Play) -> i64 {
    lengths(play).map_or(i64::from(MIN_LISTEN_MS), |l| {
        (l.shortest * REPEAT_PERMILLE / 1000).min(SCROBBLE_POINT_CAP_MS)
    })
}

/// Listening time a scrobble accounts for, or `None` without a length.
///
/// `listened_ms` (a skip after 40 s takes 40 s, not the whole track) is
/// clamped between the least a valid scrobble needs and the track length,
/// both measured against the longest known length rather than the client's.
/// No play occupies more than one window.
pub fn occupancy_ms(play: &Play, params: &BudgetParams) -> Option<(i64, DurationSource)> {
    let lengths = lengths(play)?;
    let length = lengths.longest;
    let occupancy = match play.listened_ms.filter(|l| *l >= 0) {
        Some(listened) => {
            let floor = i64::from(MIN_LISTEN_MS).min(length / 2);
            i64::from(listened).clamp(floor, length)
        }
        None => length,
    };
    Some((occupancy.min(params.window_ms), lengths.source))
}

/// Labels the plays at or after `from`; earlier plays only fill the window
/// (pass at least [`BudgetParams::lookback`] of them). Input order doesn't
/// matter: plays are ordered by `(played_at, id)`.
pub fn classify(plays: &[Play], from: DateTime<Utc>, params: &BudgetParams) -> Vec<Label> {
    let mut plays: Vec<&Play> = plays.iter().collect();
    plays.sort_by_key(|p| (p.played_at, p.id));

    let budget = params.budget_ms();
    let window = params.window();
    let mut last_of_track: HashMap<i64, DateTime<Utc>> = HashMap::new();
    let mut charge = vec![0_i64; plays.len()];
    let mut in_window: VecDeque<usize> = VecDeque::new();
    let mut load = 0;
    // The last play charged to the budget, with its length: the next play
    // that isn't a duplicate may have cut it short.
    let mut previous: Option<(usize, i64)> = None;

    let mut labels = Vec::new();
    for (i, play) in plays.iter().enumerate() {
        while let Some(&j) = in_window.front() {
            if plays[j].played_at > play.played_at - window {
                break;
            }
            load -= charge[j];
            in_window.pop_front();
        }

        let lengths = lengths(play);
        let repeat = last_of_track
            .insert(play.track_id, play.played_at)
            .is_some_and(|last| {
                (play.played_at - last).num_milliseconds() < repeat_window_ms(play)
            });

        let label = if repeat {
            Label {
                id: play.id,
                status: Status::Duplicate,
                reason: Some(REASON_REPEAT),
                duration_source: lengths.map(|l| l.source),
                occupancy_ms: 0,
                load_ms: load,
            }
        } else {
            if let Some((j, length)) = previous.take() {
                let gap = (play.played_at - plays[j].played_at).num_milliseconds();
                let cut = gap.max(scrobble_point_ms(length));
                if cut < charge[j] {
                    if plays[j].played_at > play.played_at - window {
                        load -= charge[j] - cut;
                    }
                    charge[j] = cut;
                }
            }
            match (occupancy_ms(play, params), lengths) {
                (Some((ms, source)), Some(lengths)) => {
                    charge[i] = ms;
                    in_window.push_back(i);
                    load += ms;
                    previous = Some((i, lengths.longest));
                    let over = load > budget;
                    Label {
                        id: play.id,
                        status: if over {
                            Status::Suspect
                        } else {
                            Status::Counted
                        },
                        reason: over.then_some(REASON_LISTENING_BUDGET),
                        duration_source: Some(source),
                        occupancy_ms: ms,
                        load_ms: load,
                    }
                }
                _ => Label {
                    id: play.id,
                    status: Status::NoData,
                    reason: Some(REASON_NO_DURATION),
                    duration_source: None,
                    occupancy_ms: 0,
                    load_ms: load,
                },
            }
        };
        if play.played_at >= from {
            labels.push(label);
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = 1000;
    const MIN: i64 = 60 * SEC;

    fn t0() -> DateTime<Utc> {
        "2026-09-01T00:00:00Z".parse().unwrap()
    }

    /// A play of its own track (`track_id` = `id`) with one known length.
    fn play(id: i64, at_ms: i64, duration_ms: Option<i32>) -> Play {
        Play {
            id,
            track_id: id,
            played_at: t0() + TimeDelta::milliseconds(at_ms),
            mb_duration_ms: None,
            catalog_duration_ms: duration_ms,
            reported_duration_ms: duration_ms,
            listened_ms: None,
        }
    }

    fn of_track(track_id: i64, play: Play) -> Play {
        Play { track_id, ..play }
    }

    /// Back-to-back plays of distinct tracks of `duration_ms`, from
    /// `start_ms` for `span_ms`.
    fn session(first_id: i64, start_ms: i64, span_ms: i64, duration_ms: i32) -> Vec<Play> {
        (0..span_ms / i64::from(duration_ms))
            .map(|n| {
                play(
                    first_id + n,
                    start_ms + n * i64::from(duration_ms),
                    Some(duration_ms),
                )
            })
            .collect()
    }

    /// Each play reported again by other scrobblers `offsets_ms` later.
    fn with_echoes(plays: &[Play], offsets_ms: &[i64]) -> Vec<Play> {
        let mut out = plays.to_vec();
        for p in plays {
            for (k, offset) in offsets_ms.iter().enumerate() {
                out.push(Play {
                    id: p.id * 100 + k as i64 + 1_000_000,
                    played_at: p.played_at + TimeDelta::milliseconds(*offset),
                    ..p.clone()
                });
            }
        }
        out
    }

    fn count(labels: &[Label], status: Status) -> usize {
        labels.iter().filter(|l| l.status == status).count()
    }

    fn run(plays: &[Play]) -> Vec<Label> {
        classify(plays, t0(), &BudgetParams::default())
    }

    fn status_of(labels: &[Label], id: i64) -> Status {
        labels.iter().find(|l| l.id == id).unwrap().status
    }

    #[test]
    fn normal_listening_is_counted() {
        let plays = session(1, 0, 10 * 60 * MIN, 213_000);
        let labels = run(&plays);
        assert_eq!(labels.len(), plays.len());
        assert_eq!(count(&labels, Status::Counted), plays.len());
    }

    #[test]
    fn naive_bot_is_suspect_beyond_the_budget() {
        // 3,000 plays of distinct 3-minute tracks within one hour: each is
        // cut to its scrobble point by the next, the newest counts in full.
        let plays: Vec<Play> = (0..3000)
            .map(|n| play(n, n * 1200, Some(180_000)))
            .collect();
        let labels = run(&plays);
        let budget = BudgetParams::default().budget_ms();
        let allowed = ((budget - 180_000) / 90_000 + 1) as usize;
        assert_eq!(count(&labels, Status::Counted), allowed);
        assert_eq!(count(&labels, Status::Suspect), 3000 - allowed);
        assert!(
            labels[..allowed]
                .iter()
                .all(|l| l.status == Status::Counted)
        );
    }

    #[test]
    fn one_track_spammed_counts_once() {
        let plays: Vec<Play> = (0..3000)
            .map(|n| of_track(7, play(n, n * 1200, Some(180_000))))
            .collect();
        let labels = run(&plays);
        assert_eq!(count(&labels, Status::Counted), 1);
        assert_eq!(count(&labels, Status::Duplicate), 2999);
        assert!(
            labels
                .iter()
                .filter(|l| l.status == Status::Duplicate)
                .all(|l| l.reason == Some(REASON_REPEAT) && l.occupancy_ms == 0)
        );
    }

    #[test]
    fn short_track_looped_all_night_is_counted() {
        let plays: Vec<Play> = (0..960)
            .map(|n| of_track(7, play(n, n * 30_000, Some(30_000))))
            .collect();
        assert_eq!(count(&run(&plays), Status::Counted), 960);
    }

    #[test]
    fn back_to_back_replays_are_counted() {
        // A 3-minute track on repeat, timestamps drifting a little short.
        let plays: Vec<Play> = (0..60)
            .map(|n| of_track(7, play(n, n * 176_000, Some(180_000))))
            .collect();
        assert_eq!(count(&run(&plays), Status::Counted), 60);
    }

    #[test]
    fn album_of_very_short_tracks_is_counted() {
        let plays = session(1, 0, 40 * MIN, 9_000);
        assert_eq!(count(&run(&plays), Status::Counted), plays.len());
    }

    #[test]
    fn echoes_from_several_scrobblers_are_duplicates() {
        // Seen in real histories: every play reported again 6 s and 9 s
        // later, or at fixed clock offsets of 145 s and 238 s.
        let listening = session(1, 0, 4 * 60 * MIN, 290_000);
        for offsets in [&[6_000, 9_000][..], &[145_000, 238_000]] {
            let plays = with_echoes(&listening, offsets);
            let labels = run(&plays);
            assert_eq!(count(&labels, Status::Counted), listening.len());
            assert_eq!(
                count(&labels, Status::Duplicate),
                listening.len() * offsets.len()
            );
            assert_eq!(count(&labels, Status::Suspect), 0);
        }
    }

    #[test]
    fn a_burst_doesnt_taint_the_listening_around_it() {
        // One track scrobbled every 1.5 s for 55 minutes, while real
        // listening goes on.
        let listening = session(1, 0, 2 * 60 * MIN, 240_000);
        let mut plays: Vec<Play> = (0..2200)
            .map(|n| of_track(99_999, play(10_000 + n, 30 * MIN + n * 1500, Some(384_000))))
            .collect();
        plays.extend(listening.iter().cloned());
        let labels = run(&plays);
        assert_eq!(count(&labels, Status::Suspect), 0);
        assert!(
            listening
                .iter()
                .all(|p| status_of(&labels, p.id) == Status::Counted)
        );
        assert_eq!(count(&labels, Status::Duplicate), 2199);
    }

    #[test]
    fn skipping_through_tracks_is_counted() {
        // Skips after 30 to 100 s through 4-minute tracks, ~50 an hour for
        // three hours: each costs its 2-minute scrobble point.
        let mut at = 0;
        let plays: Vec<Play> = (0..150)
            .map(|n| {
                let p = play(n, at, Some(240_000));
                at += [30, 60, 90, 100][n as usize % 4] * SEC;
                p
            })
            .collect();
        assert_eq!(count(&run(&plays), Status::Counted), 150);
    }

    #[test]
    fn the_ceiling_is_the_budget_in_scrobble_points() {
        // Distinct 4-minute tracks: a bot staying under budget / 2 min an
        // hour is counted, a faster one is not.
        let every = |secs: i64| -> Vec<Play> {
            (0..2000)
                .map(|n| play(n, n * secs * SEC, Some(240_000)))
                .collect()
        };
        assert_eq!(count(&run(&every(55)), Status::Suspect), 0);
        assert!(count(&run(&every(50)), Status::Suspect) > 1000);
    }

    #[test]
    fn missing_duration_is_no_data_and_never_suspect() {
        // Bot-rate plays without any duration, mixed into normal listening.
        let mut plays: Vec<Play> = (0..2000).map(|n| play(n, n * 1000, None)).collect();
        plays.extend(session(10_000, 0, 60 * MIN, 200_000));
        let labels = run(&plays);
        assert_eq!(count(&labels, Status::NoData), 2000);
        assert_eq!(count(&labels, Status::Suspect), 0);
        assert!(
            labels
                .iter()
                .filter(|l| l.status == Status::NoData)
                .all(|l| l.reason == Some(REASON_NO_DURATION))
        );
    }

    #[test]
    fn echoes_without_a_length_are_duplicates() {
        let plays = vec![
            of_track(7, play(1, 0, None)),
            of_track(7, play(2, 6 * SEC, None)),
            of_track(7, play(3, 9 * SEC, None)),
            of_track(7, play(4, 5 * MIN, None)),
        ];
        let labels = run(&plays);
        assert_eq!(count(&labels, Status::NoData), 2);
        assert_eq!(status_of(&labels, 2), Status::Duplicate);
        assert_eq!(status_of(&labels, 3), Status::Duplicate);
    }

    #[test]
    fn two_devices_at_once_are_counted() {
        // Durations run ~5 % longer than the gaps between plays, as when
        // clients report imprecise lengths.
        let mut plays = Vec::new();
        for (device, offset) in [(0, 0), (100_000, 37_000)] {
            plays.extend((0..90).map(|n| Play {
                reported_duration_ms: Some(210_000),
                catalog_duration_ms: Some(210_000),
                ..play(device + n, offset + n * 200_000, None)
            }));
        }
        assert_eq!(count(&run(&plays), Status::Counted), plays.len());
    }

    #[test]
    fn four_devices_at_once_exceed_the_budget() {
        let mut plays = Vec::new();
        for device in 0..4 {
            plays.extend(session(device * 1000, device * 1000, 3 * 60 * MIN, 200_000));
        }
        assert!(count(&run(&plays), Status::Suspect) > 0);
    }

    #[test]
    fn classification_is_deterministic_and_order_independent() {
        let mut plays: Vec<Play> = (0..500).map(|n| play(n, n * 2000, Some(150_000))).collect();
        plays.extend(session(1000, 0, 2 * 60 * MIN, 30_000));
        plays.extend(with_echoes(&session(5000, 0, 60 * MIN, 200_000), &[4_000]));
        let first = run(&plays);
        assert_eq!(first, run(&plays));
        plays.reverse();
        assert_eq!(first, run(&plays));
    }

    #[test]
    fn labels_depend_only_on_earlier_plays() {
        // What the worker relies on to classify one day with a lookback, and
        // live ingest to agree with a later reclassification.
        let mut plays = session(1, 0, 3 * 60 * MIN, 200_000);
        plays.extend((0..400).map(|n| play(10_000 + n, 60 * MIN + n * 3000, Some(180_000))));
        let all = run(&plays);
        let cutoff = t0() + TimeDelta::minutes(90);
        let earlier: Vec<Play> = plays
            .iter()
            .filter(|p| p.played_at < cutoff)
            .cloned()
            .collect();
        for label in run(&earlier) {
            assert_eq!(Some(&label), all.iter().find(|l| l.id == label.id));
        }
    }

    #[test]
    fn long_mix_alone_is_counted() {
        let plays = vec![
            play(1, 0, Some(3 * 60 * 60 * 1000)),
            play(2, 1000, Some(180_000)),
        ];
        assert_eq!(count(&run(&plays), Status::Counted), 2);
    }

    #[test]
    fn listened_ms_keeps_heavy_skipping_counted() {
        // 150 skips after ~35 s on 4-minute tracks within an hour.
        let plays: Vec<Play> = (0..150)
            .map(|n| Play {
                listened_ms: Some(35_000),
                ..play(n, n * 24_000, Some(240_000))
            })
            .collect();
        assert_eq!(count(&run(&plays), Status::Counted), 150);
        let without: Vec<Play> = plays
            .iter()
            .map(|p| Play {
                listened_ms: None,
                ..p.clone()
            })
            .collect();
        assert!(count(&run(&without), Status::Suspect) > 0);
    }

    #[test]
    fn the_longest_known_length_is_charged() {
        // A bot claims 10 s tracks and 5 s listens; MusicBrainz says 200 s.
        let claimed: Vec<Play> = (0..300)
            .map(|n| Play {
                listened_ms: Some(5_000),
                ..play(n, n * 10_000, Some(10_000))
            })
            .collect();
        assert_eq!(count(&run(&claimed), Status::Suspect), 0);
        let known: Vec<Play> = claimed
            .iter()
            .map(|p| Play {
                mb_duration_ms: Some(200_000),
                ..p.clone()
            })
            .collect();
        let labels = run(&known);
        assert!(count(&labels, Status::Suspect) > 0);
        assert!(
            labels
                .iter()
                .all(|l| l.duration_source == Some(DurationSource::MusicBrainz))
        );
        assert!(
            labels
                .iter()
                .all(|l| l.occupancy_ms == i64::from(MIN_LISTEN_MS))
        );
    }

    #[test]
    fn a_wrong_short_musicbrainz_length_cant_be_farmed() {
        // MusicBrainz matched a 30 s snippet of a 3:47 song.
        let plays: Vec<Play> = (0..300)
            .map(|n| Play {
                mb_duration_ms: Some(30_000),
                reported_duration_ms: None,
                ..play(n, n * 10_000, Some(227_000))
            })
            .collect();
        let labels = run(&plays);
        assert!(count(&labels, Status::Suspect) > 200);
        assert!(
            labels
                .iter()
                .all(|l| l.duration_source == Some(DurationSource::Catalog))
        );
    }

    #[test]
    fn a_wrong_long_length_doesnt_swallow_replays() {
        // MusicBrainz matched an 8:30 live version of a 2:27 song played on
        // repeat.
        let plays: Vec<Play> = (0..20)
            .map(|n| Play {
                mb_duration_ms: Some(510_000),
                ..of_track(7, play(n, n * 147_000, Some(147_000)))
            })
            .collect();
        let labels = run(&plays);
        assert_eq!(count(&labels, Status::Counted), 20);
    }

    #[test]
    fn lengths_pick_the_longest_and_the_shortest() {
        let p = Play {
            mb_duration_ms: Some(30_000),
            catalog_duration_ms: Some(227_000),
            reported_duration_ms: Some(0),
            ..play(1, 0, None)
        };
        assert_eq!(
            lengths(&p),
            Some(Lengths {
                longest: 227_000,
                source: DurationSource::Catalog,
                shortest: 30_000,
            })
        );
        let tie = Play {
            mb_duration_ms: Some(200_000),
            ..play(1, 0, Some(200_000))
        };
        assert_eq!(lengths(&tie).unwrap().source, DurationSource::MusicBrainz);
        assert_eq!(lengths(&play(1, 0, None)), None);
        assert_eq!(repeat_window_ms(&p), 27_000);
        assert_eq!(repeat_window_ms(&play(1, 0, Some(600_000))), 240_000);
    }

    #[test]
    fn previous_window_counts_but_is_not_labelled() {
        let from = t0() + TimeDelta::hours(24);
        let plays: Vec<Play> = (0..200)
            .map(|n| play(n, 24 * 60 * MIN - 10 * MIN + n * 5_000, Some(300_000)))
            .collect();
        let labels = classify(&plays, from, &BudgetParams::default());
        assert_eq!(
            labels.len(),
            plays.iter().filter(|p| p.played_at >= from).count()
        );
        assert!(labels.iter().all(|l| l.status == Status::Suspect));
    }

    #[test]
    fn params_are_validated_and_fingerprinted() {
        assert!(BudgetParams::new(30, 2.0, 0).is_err());
        assert!(BudgetParams::new(3600, 0.5, 0).is_err());
        let params = BudgetParams::new(3600, 2.0, 900).unwrap();
        assert_eq!(params, BudgetParams::default());
        assert_eq!(params.budget_ms(), 8_100_000);
        assert_ne!(
            params.fingerprint(),
            BudgetParams::new(3600, 2.5, 900).unwrap().fingerprint()
        );
        assert!(params.fingerprint().contains("/v2 "));
    }
}
