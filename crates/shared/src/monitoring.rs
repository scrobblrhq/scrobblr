//! Whether the worker's loops keep up, from the heartbeats they store
//! (`worker_heartbeats`).

use chrono::{DateTime, Utc};

/// A loop counts as stalled or failing after this many of its intervals.
pub const DEFAULT_STALL_FACTOR: f64 = 3.0;

/// `WORKER_STALL_FACTOR`, at least 1; blank is the default.
pub fn stall_factor_from_env() -> Result<f64, String> {
    match std::env::var("WORKER_STALL_FACTOR") {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite() && *f >= 1.0)
            .ok_or_else(|| format!("WORKER_STALL_FACTOR={v}: expected a number from 1 up")),
        _ => Ok(DEFAULT_STALL_FACTOR),
    }
}

#[derive(Debug, Clone)]
pub struct Heartbeat {
    pub loop_name: String,
    pub interval_secs: i32,
    pub started_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_ok_at: Option<DateTime<Utc>>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopState {
    Ok,
    /// Runs, but every run since the deadline failed.
    Failing,
    /// Hasn't finished a run within the deadline: hung, or the worker is
    /// gone.
    Stalled,
}

impl LoopState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failing => "failing",
            Self::Stalled => "stalled",
        }
    }
}

impl Heartbeat {
    /// Seconds since the loop last finished a run (or its worker started,
    /// if later).
    pub fn run_age_secs(&self, now: DateTime<Utc>) -> f64 {
        age(now, latest(self.started_at, self.last_run_at))
    }

    /// Seconds since the loop last ran without an error (or its worker
    /// started, if later).
    pub fn ok_age_secs(&self, now: DateTime<Utc>) -> f64 {
        age(now, latest(self.started_at, self.last_ok_at))
    }

    pub fn state(&self, now: DateTime<Utc>, factor: f64) -> LoopState {
        let deadline = f64::from(self.interval_secs) * factor;
        if self.run_age_secs(now) > deadline {
            LoopState::Stalled
        } else if self.ok_age_secs(now) > deadline {
            LoopState::Failing
        } else {
            LoopState::Ok
        }
    }
}

/// Healthy when there are heartbeats and every loop is [`LoopState::Ok`].
pub fn healthy(beats: &[Heartbeat], now: DateTime<Utc>, factor: f64) -> bool {
    !beats.is_empty() && beats.iter().all(|b| b.state(now, factor) == LoopState::Ok)
}

fn latest(started: DateTime<Utc>, at: Option<DateTime<Utc>>) -> DateTime<Utc> {
    at.map_or(started, |at| at.max(started))
}

fn age(now: DateTime<Utc>, at: DateTime<Utc>) -> f64 {
    ((now - at).num_milliseconds() as f64 / 1000.0).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn beat(started_ago: i64, run_ago: Option<i64>, ok_ago: Option<i64>) -> Heartbeat {
        let now = now();
        let ago = |s: i64| now - TimeDelta::seconds(s);
        Heartbeat {
            loop_name: "test".into(),
            interval_secs: 60,
            started_at: ago(started_ago),
            last_run_at: run_ago.map(ago),
            last_ok_at: ok_ago.map(ago),
            last_error_at: None,
            last_error: None,
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_791_600_000, 0).unwrap()
    }

    #[test]
    fn a_loop_is_judged_by_its_last_run_and_its_last_success() {
        let state = |b: Heartbeat| b.state(now(), 3.0);
        assert_eq!(state(beat(1000, Some(10), Some(10))), LoopState::Ok);
        assert_eq!(state(beat(1000, Some(181), Some(181))), LoopState::Stalled);
        assert_eq!(state(beat(1000, Some(10), Some(181))), LoopState::Failing);
        assert_eq!(state(beat(1000, Some(10), None)), LoopState::Failing);
        // A worker that just started has a deadline's grace.
        assert_eq!(state(beat(100, None, None)), LoopState::Ok);
        assert_eq!(state(beat(200, None, None)), LoopState::Stalled);
        // Runs from before a restart don't count against it.
        assert_eq!(state(beat(5, Some(5000), Some(5000))), LoopState::Ok);
    }

    #[test]
    fn healthy_needs_heartbeats_and_every_loop_ok() {
        assert!(!healthy(&[], now(), 3.0));
        assert!(healthy(&[beat(1000, Some(10), Some(10))], now(), 3.0));
        assert!(!healthy(
            &[
                beat(1000, Some(10), Some(10)),
                beat(1000, Some(500), Some(500))
            ],
            now(),
            3.0
        ));
        // A larger factor gives it longer.
        assert!(healthy(&[beat(1000, Some(500), Some(500))], now(), 10.0));
    }
}
