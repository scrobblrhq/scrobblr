//! Shadow-mode scrobble classification — Rule 1, the listening-time budget.
//!
//! Pure logic (no I/O) so every scenario is unit-testable offline. The worker
//! loads one user-day plus a window of lookback, calls [`classify`], and
//! stores one label per scrobble; nothing reads the labels yet.
//!
//! # The rule
//!
//! For each scrobble `i` (the *anchor*) with played_at `t_i` and known
//! duration `d_i`:
//!
//! ```text
//! listened_i = Σ min(d_j, W)   over scrobbles j with t_i − W < t_j ≤ t_i
//!                               (known durations only; cross-source copies count 0)
//! budget_i   = (W + min(d_i, W)) × margin_ratio + margin_slack
//! suspect    ⇔ listened_i > budget_i
//! ```
//!
//! - `played_at` is the play's start, so a continuous listener's window
//!   legitimately holds about `W + d_i` of music; that is the `+ d_i`.
//! - Each duration is clipped to `W`, so one 10-hour white-noise track cannot
//!   overflow a 1-hour window. A looped short track adds up to real time and
//!   passes too. Albums of very short tracks pass for the same reason: the
//!   budget is time, not play count.
//! - `margin_ratio` absorbs several devices playing at once (2.0 lets two
//!   full streams through); `margin_slack` absorbs imprecise durations.
//! - A scrobble with no known duration is `no_data` (never `suspect`) and
//!   adds nothing to its neighbours' sums: missing metadata is never punished.
//! - Within a spread-out burst the first plays that fit the budget stay
//!   `counted` and only the excess is flagged. Up to `W` of listening right
//!   after a burst can be flagged too, since the burst is still in its window.
//!
//! # Cross-source copies
//!
//! A user with both the extension and a connected Spotify account can get
//! every play recorded twice, timestamps about a track-length apart (Spotify
//! reports roughly the end, the extension the start), so ingest dedup misses
//! it. To the budget that looks like 2× listening and would use up the whole
//! multi-device margin. A greedy pass in time order pairs each scrobble with
//! at most one earlier, still-unpaired scrobble of the same track from a
//! *different* source that started within the earlier play's duration; the
//! later copy contributes 0. Pairing is one-to-one, so a genuine replay right
//! after a double-scrobble still counts. Same-source repeats (loops) are never
//! paired.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

/// Bump when the rule's logic changes. Together with [`TimeBudgetConfig`] it
/// identifies the ruleset stored with every label, so labels produced by an
/// older version are found and reclassified.
pub const RULES_VERSION: i32 = 1;

/// Upper bound for the window: the worker loads one day plus one window of
/// lookback, and the ingest hook only ever marks the next day.
pub const MAX_WINDOW_SECS: i32 = 86_400;

/// Thresholds for Rule 1. Serialized as the ruleset's `params`, so every
/// label records exactly which thresholds produced it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TimeBudgetConfig {
    pub window_secs: i32,
    pub margin_ratio: f64,
    pub margin_slack_secs: i32,
}

impl Default for TimeBudgetConfig {
    fn default() -> Self {
        Self {
            window_secs: 3_600,
            margin_ratio: 2.0,
            margin_slack_secs: 600,
        }
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum ConfigError {
    #[error("window must be between 1 and {MAX_WINDOW_SECS} seconds, got {0}")]
    Window(i32),
    #[error("margin ratio must be a finite number > 0, got {0}")]
    Ratio(f64),
    #[error("margin slack must be >= 0 seconds, got {0}")]
    Slack(i32),
}

impl TimeBudgetConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=MAX_WINDOW_SECS).contains(&self.window_secs) {
            return Err(ConfigError::Window(self.window_secs));
        }
        if !self.margin_ratio.is_finite() || self.margin_ratio <= 0.0 {
            return Err(ConfigError::Ratio(self.margin_ratio));
        }
        if self.margin_slack_secs < 0 {
            return Err(ConfigError::Slack(self.margin_slack_secs));
        }
        Ok(())
    }
}

/// One scrobble as the classifier sees it.
#[derive(Debug, Clone)]
pub struct ScrobbleSample {
    pub id: i64,
    pub played_at: DateTime<Utc>,
    pub track_id: i64,
    pub source: String,
    /// Track duration (catalog first, then client-reported). `None` or ≤ 0
    /// means unknown.
    pub duration_ms: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Counted,
    Suspect,
    NoData,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Counted => "counted",
            Status::Suspect => "suspect",
            Status::NoData => "no_data",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Ok,
    TimeBudgetExceeded,
    MissingDuration,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Ok => "ok",
            Reason::TimeBudgetExceeded => "time_budget_exceeded",
            Reason::MissingDuration => "missing_duration",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    pub id: i64,
    pub played_at: DateTime<Utc>,
    pub status: Status,
    pub reason: Reason,
    /// `listened / budget`; > 1.0 means suspect. `None` for no_data.
    pub score: Option<f32>,
}

/// Labels every sample. The output is sorted by `(played_at, id)` whatever
/// the input order, and is fully determined by the input: same samples and
/// config, same labels.
pub fn classify(samples: &[ScrobbleSample], cfg: &TimeBudgetConfig) -> Vec<Classification> {
    let mut sorted: Vec<&ScrobbleSample> = samples.iter().collect();
    sorted.sort_by_key(|s| (s.played_at, s.id));

    let window_ms = i64::from(cfg.window_secs) * 1_000;
    let slack_ms = i64::from(cfg.margin_slack_secs) * 1_000;
    let times: Vec<i64> = sorted
        .iter()
        .map(|s| s.played_at.timestamp_millis())
        .collect();
    let durations: Vec<Option<i64>> = sorted
        .iter()
        .map(|s| s.duration_ms.filter(|&d| d > 0).map(i64::from))
        .collect();

    let contributions = budget_contributions(&sorted, &times, &durations, window_ms);

    // prefix[k] = sum of contributions[..k], so any window sum is one subtraction.
    let mut prefix = Vec::with_capacity(sorted.len() + 1);
    prefix.push(0i64);
    for c in &contributions {
        prefix.push(prefix.last().copied().unwrap_or(0) + c);
    }

    // Two pointers over the sorted times, both only ever moving forward:
    //   hi = first index with t > t_i  (the window is inclusive of t_i, so it
    //        takes every scrobble sharing the anchor's timestamp, including
    //        later ids — a same-instant burst sees the whole burst);
    //   lo = first index with t > t_i − W  (exclusive lower bound).
    let mut lo = 0;
    let mut hi = 0;
    let mut out = Vec::with_capacity(sorted.len());
    for (i, sample) in sorted.iter().enumerate() {
        let t = times[i];
        while hi < sorted.len() && times[hi] <= t {
            hi += 1;
        }
        while times[lo] <= t - window_ms {
            lo += 1;
        }

        let (status, reason, score) = match durations[i] {
            None => (Status::NoData, Reason::MissingDuration, None),
            Some(d) => {
                let listened = prefix[hi] - prefix[lo];
                let budget =
                    (window_ms + d.min(window_ms)) as f64 * cfg.margin_ratio + slack_ms as f64;
                let score = listened as f64 / budget;
                if listened as f64 > budget {
                    (
                        Status::Suspect,
                        Reason::TimeBudgetExceeded,
                        Some(score as f32),
                    )
                } else {
                    (Status::Counted, Reason::Ok, Some(score as f32))
                }
            }
        };

        out.push(Classification {
            id: sample.id,
            played_at: sample.played_at,
            status,
            reason,
            score,
        });
    }
    out
}

/// How much each (sorted) scrobble adds to the window sums: its clipped
/// duration, or 0 when the duration is unknown or it is a cross-source copy
/// of an earlier play (see the module docs).
fn budget_contributions(
    sorted: &[&ScrobbleSample],
    times: &[i64],
    durations: &[Option<i64>],
    window_ms: i64,
) -> Vec<i64> {
    let mut contributions = vec![0i64; sorted.len()];
    // Per track: indices of earlier plays still available to absorb a copy.
    let mut unpaired: HashMap<i64, Vec<usize>> = HashMap::new();

    for (j, sample) in sorted.iter().enumerate() {
        let Some(d) = durations[j] else { continue };

        let candidates = unpaired.entry(sample.track_id).or_default();
        // Times only grow, so a play whose span has already ended can never
        // absorb a later copy; drop it for good.
        candidates.retain(|&k| times[j] - times[k] <= durations[k].unwrap_or(0));

        match candidates
            .iter()
            .position(|&k| sorted[k].source != sample.source)
        {
            Some(pos) => {
                // j is the other source's record of play k: count the play once.
                candidates.remove(pos);
            }
            None => {
                contributions[j] = d.min(window_ms);
                candidates.push(j);
            }
        }
    }
    contributions
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    const MIN: i64 = 60_000;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 14, 20, 0, 0).unwrap()
    }

    /// Builds samples from `(offset_ms, duration_ms, track_id, source)`.
    fn samples(rows: &[(i64, Option<i32>, i64, &str)]) -> Vec<ScrobbleSample> {
        rows.iter()
            .enumerate()
            .map(
                |(i, &(offset, duration_ms, track_id, source))| ScrobbleSample {
                    id: i as i64 + 1,
                    played_at: t0() + Duration::milliseconds(offset),
                    track_id,
                    source: source.to_string(),
                    duration_ms,
                },
            )
            .collect()
    }

    /// `count` back-to-back plays of `dur_ms` each, starting at `start_ms`,
    /// cycling through `distinct_tracks` track ids from `first_track`.
    fn back_to_back(
        start_ms: i64,
        count: usize,
        dur_ms: i32,
        first_track: i64,
        distinct_tracks: i64,
        source: &str,
    ) -> Vec<(i64, Option<i32>, i64, String)> {
        (0..count)
            .map(|k| {
                (
                    start_ms + k as i64 * i64::from(dur_ms),
                    Some(dur_ms),
                    first_track + (k as i64 % distinct_tracks),
                    source.to_string(),
                )
            })
            .collect()
    }

    fn owned(rows: &[(i64, Option<i32>, i64, String)]) -> Vec<ScrobbleSample> {
        let borrowed: Vec<(i64, Option<i32>, i64, &str)> = rows
            .iter()
            .map(|(o, d, t, s)| (*o, *d, *t, s.as_str()))
            .collect();
        samples(&borrowed)
    }

    fn count(out: &[Classification], status: Status) -> usize {
        out.iter().filter(|c| c.status == status).count()
    }

    /// A normal evening: ~15 tracks an hour, back to back, for three hours.
    #[test]
    fn normal_listening_is_counted() {
        let rows = back_to_back(0, 45, 4 * 60_000, 1, 30, "extension");
        let out = classify(&owned(&rows), &TimeBudgetConfig::default());
        assert_eq!(count(&out, Status::Counted), 45);
    }

    /// A naive bot: 3000 plays of 3.5-minute tracks inside one hour. Only the
    /// plays that still fit the budget at the start of the burst are counted.
    #[test]
    fn naive_bot_burst_is_suspect() {
        let rows: Vec<_> = (0..3000)
            .map(|k| {
                (
                    k as i64 * 1_200,
                    Some(210_000),
                    1 + k as i64 % 50,
                    "bot".to_string(),
                )
            })
            .collect();
        let out = classify(&owned(&rows), &TimeBudgetConfig::default());
        let suspect = count(&out, Status::Suspect);
        assert!(suspect >= 2_950, "only {suspect} of 3000 flagged");
        // Once the budget is exhausted nothing later in the burst is counted.
        let first_suspect = out
            .iter()
            .position(|c| c.status == Status::Suspect)
            .unwrap();
        assert!(
            out[first_suspect..]
                .iter()
                .all(|c| c.status == Status::Suspect)
        );
    }

    /// Same-instant burst: each play's window contains the whole burst, so
    /// every play is suspect regardless of id order (G1).
    /// Value: protects=a same-instant burst labels deterministically (all
    /// suspect); fails_when=equal-timestamp inclusion or tie-break changes so
    /// labels depend on id order.
    #[test]
    fn identical_timestamp_burst_is_all_suspect() {
        let rows: Vec<_> = (0..1000)
            .map(|k| (0, Some(180_000), 1 + k as i64 % 20, "bot".to_string()))
            .collect();
        let out = classify(&owned(&rows), &TimeBudgetConfig::default());
        assert_eq!(count(&out, Status::Suspect), 1000);
    }

    /// White noise for sleeping, as one 10-hour track: clipped to the window.
    #[test]
    fn single_long_track_is_counted() {
        let rows = samples(&[(0, Some(10 * 60 * 60_000), 7, "extension")]);
        let out = classify(&rows, &TimeBudgetConfig::default());
        assert_eq!(out[0].status, Status::Counted);
    }

    /// White noise as a 45-second track looped all night from one source:
    /// adds up to real time, and same-source repeats are never paired away.
    #[test]
    fn short_track_looped_all_night_is_counted() {
        let rows = back_to_back(0, 8 * 80, 45_000, 7, 1, "mobile");
        let out = classify(&owned(&rows), &TimeBudgetConfig::default());
        assert_eq!(count(&out, Status::Counted), 640);
    }

    /// An album of 30 twenty-second tracks played straight through (G2, a
    /// spec constraint).
    /// Value: protects=very short tracks stay counted; fails_when=the budget
    /// drifts toward play counts or clipping breaks.
    #[test]
    fn album_of_very_short_tracks_is_counted() {
        let rows = back_to_back(0, 30, 20_000, 100, 30, "extension");
        let out = classify(&owned(&rows), &TimeBudgetConfig::default());
        assert_eq!(count(&out, Status::Counted), 30);
    }

    /// A track without a known duration is no_data, never suspect, and adds
    /// nothing to its neighbours' sums — even when there are a lot of them.
    #[test]
    fn missing_duration_is_no_data_and_adds_nothing() {
        let mut rows: Vec<_> = (0..1000)
            .map(|k| (k as i64 * 1_000, None, 500 + k as i64, "bot".to_string()))
            .collect();
        rows.push((500_000, Some(240_000), 1, "extension".to_string()));
        rows.push((600_000, Some(0), 2, "extension".to_string())); // 0 = unknown too
        let out = classify(&owned(&rows), &TimeBudgetConfig::default());

        assert_eq!(count(&out, Status::NoData), 1001);
        assert_eq!(count(&out, Status::Suspect), 0);
        let real = out.iter().find(|c| c.status == Status::Counted).unwrap();
        // Only its own 4 minutes count: 240k / ((3.6M + 240k) × 2 + 600k).
        let expected = 240_000.0 / 8_280_000.0;
        assert!((real.score.unwrap() - expected as f32).abs() < 1e-6);
        assert!(
            out.iter()
                .filter(|c| c.status == Status::NoData)
                .all(|c| c.score.is_none())
        );
    }

    /// Two devices playing different music at the same time for an hour —
    /// full overlap, different track lengths — stays within the 2.0× margin.
    #[test]
    fn two_overlapping_devices_are_counted() {
        let mut rows = back_to_back(0, 15, 4 * 60_000, 1, 15, "extension");
        rows.extend(back_to_back(30_000, 20, 3 * 60_000, 100, 20, "mobile"));
        let out = classify(&owned(&rows), &TimeBudgetConfig::default());
        assert_eq!(count(&out, Status::Counted), 35);
    }

    /// Extension + Spotify recording the same continuous listening (each copy
    /// about a track-length later), plus a genuine second device on top: the
    /// copies are paired away, so the real second device still fits (R4a).
    #[test]
    fn cross_source_double_scrobble_plus_second_device_is_counted() {
        let mut rows = back_to_back(0, 15, 4 * 60_000, 1, 15, "extension");
        // Spotify's copy of each play, stamped near the play's end.
        rows.extend(back_to_back(
            4 * 60_000 - 5_000,
            15,
            4 * 60_000,
            1,
            15,
            "spotify",
        ));
        rows.extend(back_to_back(10_000, 20, 3 * 60_000, 200, 20, "mobile"));
        let out = classify(&owned(&rows), &TimeBudgetConfig::default());
        assert_eq!(count(&out, Status::Suspect), 0);
    }

    /// Without pairing the same input would have been flagged — this pins
    /// that the pairing (not a loose margin) is what keeps R4a counted.
    #[test]
    fn cross_source_copies_would_exceed_budget_without_pairing() {
        let mut rows = back_to_back(0, 15, 4 * 60_000, 1, 15, "extension");
        rows.extend(back_to_back(
            4 * 60_000 - 5_000,
            15,
            4 * 60_000,
            1,
            15,
            "web",
        ));
        rows.extend(back_to_back(10_000, 20, 3 * 60_000, 200, 20, "mobile"));
        // Re-label the copies with distinct track ids so nothing pairs.
        let mut unpaired = owned(&rows);
        for s in unpaired.iter_mut().filter(|s| s.source == "web") {
            s.track_id += 1_000;
        }
        let out = classify(&unpaired, &TimeBudgetConfig::default());
        assert!(count(&out, Status::Suspect) > 0);
    }

    /// Pairing is one-to-one: ext play, spotify copy, then a genuine ext
    /// replay of the same track — the replay still counts (R4c).
    #[test]
    fn replay_after_double_scrobble_still_counts() {
        let cfg = TimeBudgetConfig {
            margin_ratio: 1.0,
            margin_slack_secs: 0,
            ..Default::default()
        };
        let rows = samples(&[
            (0, Some(4 * 60_000), 1, "extension"),
            (4 * 60_000, Some(4 * 60_000), 1, "spotify"),
            (4 * 60_000, Some(4 * 60_000), 1, "extension"),
        ]);
        let out = classify(&rows, &cfg);
        // The replay's window holds the first play and itself: 8 minutes.
        let replay = out.iter().find(|c| c.id == 3).unwrap();
        let expected = (8 * MIN) as f64 / ((60 * MIN + 4 * MIN) as f64);
        assert!((replay.score.unwrap() - expected as f32).abs() < 1e-6);
    }

    /// A same-track play from another source that starts after the earlier
    /// play ended is a new play, not a copy (R4d); same-source repeats inside
    /// the span are never paired (R4b).
    #[test]
    fn copies_outside_the_span_or_same_source_are_not_paired() {
        let cfg = TimeBudgetConfig {
            margin_ratio: 1.0,
            margin_slack_secs: 0,
            ..Default::default()
        };
        let rows = samples(&[
            (0, Some(4 * 60_000), 1, "extension"),
            (4 * 60_000 + 1, Some(4 * 60_000), 1, "spotify"), // after the span
            (5 * 60_000, Some(4 * 60_000), 1, "spotify"),     // same source as #2
        ]);
        let out = classify(&rows, &cfg);
        let last = out.iter().find(|c| c.id == 3).unwrap();
        let expected = (12 * MIN) as f64 / ((60 * MIN + 4 * MIN) as f64);
        assert!((last.score.unwrap() - expected as f32).abs() < 1e-6);
    }

    /// A scrobble exactly W before the anchor is outside its window.
    #[test]
    fn window_lower_bound_is_exclusive() {
        let cfg = TimeBudgetConfig {
            window_secs: 600,
            margin_ratio: 1.0,
            margin_slack_secs: 0,
        };
        let rows = samples(&[
            (0, Some(600_000), 1, "extension"),
            (600_000, Some(300_000), 2, "extension"),
        ]);
        let out = classify(&rows, &cfg);
        let anchor = out.iter().find(|c| c.id == 2).unwrap();
        assert!((anchor.score.unwrap() - (300_000.0 / 900_000.0) as f32).abs() < 1e-6);

        // One millisecond later it is inside.
        let rows = samples(&[
            (1, Some(600_000), 1, "extension"),
            (600_000, Some(300_000), 2, "extension"),
        ]);
        let anchor = classify(&rows, &cfg)
            .into_iter()
            .find(|c| c.id == 2)
            .unwrap();
        assert!((anchor.score.unwrap() - 1.0).abs() < 1e-6);
    }

    /// Running the classifier twice, or on shuffled input, gives identical
    /// labels — the basis for idempotent reclassification.
    #[test]
    fn classification_is_deterministic() {
        let mut rows = back_to_back(0, 40, 200_000, 1, 10, "extension");
        rows.extend((0..500).map(|k| {
            (
                3_600_000 + k * 900,
                Some(200_000),
                1 + k % 7,
                "bot".to_string(),
            )
        }));
        rows.push((7_000_000, None, 99, "extension".to_string()));
        let input = owned(&rows);
        let cfg = TimeBudgetConfig::default();

        let first = classify(&input, &cfg);
        assert_eq!(first, classify(&input, &cfg));

        let mut shuffled = input.clone();
        shuffled.reverse();
        shuffled.swap(3, 300);
        assert_eq!(first, classify(&shuffled, &cfg));
    }

    /// Out-of-range thresholds are rejected before the worker starts (G3).
    /// Value: protects=W in 1..=86400, ratio > 0, slack >= 0; fails_when=the
    /// validation is removed, breaking the one-day lookback assumption.
    #[test]
    fn config_validation_rejects_out_of_range_values() {
        let ok = TimeBudgetConfig::default();
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(
            TimeBudgetConfig {
                window_secs: 0,
                ..ok
            }
            .validate(),
            Err(ConfigError::Window(0))
        );
        assert_eq!(
            TimeBudgetConfig {
                window_secs: 86_401,
                ..ok
            }
            .validate(),
            Err(ConfigError::Window(86_401))
        );
        assert_eq!(
            TimeBudgetConfig {
                margin_ratio: 0.0,
                ..ok
            }
            .validate(),
            Err(ConfigError::Ratio(0.0))
        );
        assert!(
            TimeBudgetConfig {
                margin_ratio: f64::NAN,
                ..ok
            }
            .validate()
            .is_err()
        );
        assert_eq!(
            TimeBudgetConfig {
                margin_slack_secs: -1,
                ..ok
            }
            .validate(),
            Err(ConfigError::Slack(-1))
        );
    }
}
