//! Each supervised loop reports its runs to `worker_heartbeats`, so
//! `worker status`, /health/worker and /metrics can tell when one stalls or
//! keeps failing.

use std::fmt::Display;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;
use tokio::time::Instant;

use db::queries::monitoring as mdb;

/// Every loop the worker supervises, with how long it may take between two
/// runs: its pause plus a slow batch.
pub const LOOPS: &[(&str, Duration)] = &[
    ("cleanup", Duration::from_secs(300)),
    ("enrichment", Duration::from_secs(300)),
    ("enrichment maintenance", Duration::from_secs(1800)),
    ("connected-accounts poller", Duration::from_secs(300)),
    ("length backfill", Duration::from_secs(300)),
    ("deezer lengths", Duration::from_secs(300)),
    ("import", Duration::from_secs(600)),
    ("classification", Duration::from_secs(300)),
    ("classification sweep", Duration::from_secs(600)),
    ("rankings", Duration::from_secs(300)),
    ("rankings sweep", Duration::from_secs(600)),
    ("ranking snapshots", Duration::from_secs(900)),
];

/// A run is written at most this often, unless the loop starts or stops
/// failing, or waits longer than before.
const WRITE_EVERY: Duration = Duration::from_secs(30);
const MAX_ERROR_LEN: usize = 500;

pub async fn register(db: &PgPool) -> Result<(), sqlx::Error> {
    let loops: Vec<(&str, i32)> = LOOPS.iter().map(|(n, i)| (*n, secs(*i))).collect();
    mdb::register_loops(db, &loops).await
}

/// A loop's handle on its heartbeat. Writing one is best-effort: a failure
/// is logged at debug level, since the database being away shows elsewhere.
#[derive(Clone)]
pub struct Beat {
    db: PgPool,
    name: &'static str,
    interval: Duration,
    last_write: Arc<Mutex<Option<Written>>>,
}

#[derive(Clone, Copy, PartialEq)]
struct Written {
    at: Instant,
    ok: bool,
    interval_secs: i32,
}

impl Beat {
    /// Panics on a name missing from [`LOOPS`].
    pub fn new(db: PgPool, name: &'static str) -> Self {
        let interval = LOOPS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, i)| *i)
            .unwrap_or_else(|| panic!("loop `{name}` missing from heartbeat::LOOPS"));
        Self {
            db,
            name,
            interval,
            last_write: Default::default(),
        }
    }

    pub async fn ok(&self) {
        self.record(None, self.interval).await
    }

    /// Ran fine, and pauses for `wait` before the next run.
    pub async fn ok_then_wait(&self, wait: Duration) {
        self.record(None, self.interval.max(wait)).await
    }

    pub async fn failed(&self, error: impl Display) {
        self.record(Some(error.to_string()), self.interval).await
    }

    pub async fn failed_then_wait(&self, error: impl Display, wait: Duration) {
        self.record(Some(error.to_string()), self.interval.max(wait))
            .await
    }

    /// The loop isn't configured to run here: it has no heartbeat to check.
    pub async fn disable(&self) {
        if let Err(e) = mdb::remove_loop(&self.db, self.name).await {
            tracing::debug!("heartbeat {}: {e}", self.name);
        }
    }

    async fn record(&self, error: Option<String>, interval: Duration) {
        let now = Instant::now();
        let this = Written {
            at: now,
            ok: error.is_none(),
            interval_secs: secs(interval),
        };
        {
            let mut last = self.last_write.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(last) = *last
                && last.ok == this.ok
                && last.interval_secs >= this.interval_secs
                && now - last.at < WRITE_EVERY
            {
                return;
            }
            *last = Some(this);
        }
        let error = error.map(|e| truncate(e, MAX_ERROR_LEN));
        if let Err(e) =
            mdb::record_run(&self.db, self.name, this.interval_secs, error.as_deref()).await
        {
            tracing::debug!("heartbeat {}: {e}", self.name);
            *self.last_write.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
}

fn secs(d: Duration) -> i32 {
    i32::try_from(d.as_secs()).unwrap_or(i32::MAX).max(1)
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_names_are_unique() {
        let mut names: Vec<&str> = LOOPS.iter().map(|(n, _)| *n).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), LOOPS.len());
    }

    #[test]
    fn errors_are_cut_on_a_char_boundary() {
        assert_eq!(truncate("añb".into(), 2), "a");
        assert_eq!(truncate("ab".into(), 5), "ab");
    }
}
