//! `worker status`: whether the worker's loops keep up, and the work
//! waiting in its queues.

use chrono::Utc;
use sqlx::PgPool;

use db::queries::monitoring as mdb;
use shared::monitoring::{self, LoopState};

pub const USAGE: &str = "\
       worker status               the worker's loops and queues; exits 1
                                   when a loop is stalled or failing";

/// Prints the report; `Ok(false)` when unhealthy.
pub async fn run(db: &PgPool, args: &[String]) -> anyhow::Result<bool> {
    if !args.is_empty() {
        anyhow::bail!("usage:\n{USAGE}");
    }
    let factor = monitoring::stall_factor_from_env().map_err(anyhow::Error::msg)?;
    let beats = mdb::heartbeats(db).await?;
    let queues = mdb::queue_depths(db).await?;
    let now = Utc::now();

    println!("loops (stalled or failing after {factor}x their interval):");
    println!(
        "  {:<26} {:<8} {:>10} {:>10} {:>9}  last error",
        "loop", "state", "last run", "last ok", "interval"
    );
    for b in &beats {
        let state = b.state(now, factor);
        let last_ok = b.last_ok_at.map(|_| ago(b.ok_age_secs(now)));
        let error = match (b.last_error_at, &b.last_error) {
            (Some(at), Some(e)) => format!("{} ago: {e}", age((now - at).num_seconds() as f64)),
            _ => String::new(),
        };
        println!(
            "  {:<26} {:<8} {:>10} {:>10} {:>9}  {}",
            b.loop_name,
            state.as_str(),
            b.last_run_at
                .map_or_else(|| "never".into(), |_| ago(b.run_age_secs(now))),
            last_ok.unwrap_or_else(|| "never".into()),
            age(f64::from(b.interval_secs)),
            error
        );
    }
    if beats.is_empty() {
        println!("  no heartbeats: the worker has never run against this database");
    }

    println!("\nqueues (due now):");
    for q in &queues {
        let oldest = q
            .oldest_due_secs
            .map(|s| format!(", oldest waiting {}", age(s)))
            .unwrap_or_default();
        println!("  {:<15} {:>8}{oldest}", q.queue, q.due);
    }

    let healthy = monitoring::healthy(&beats, now, factor);
    let bad: Vec<&str> = beats
        .iter()
        .filter(|b| b.state(now, factor) != LoopState::Ok)
        .map(|b| b.loop_name.as_str())
        .collect();
    if healthy {
        println!("\nstatus: healthy");
    } else if bad.is_empty() {
        println!("\nstatus: unhealthy (no heartbeats)");
    } else {
        println!("\nstatus: unhealthy ({})", bad.join(", "));
    }
    Ok(healthy)
}

fn ago(secs: f64) -> String {
    format!("{} ago", age(secs))
}

fn age(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d{:02}h", s / 86400, s % 86400 / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_are_short() {
        assert_eq!(age(5.4), "5s");
        assert_eq!(age(300.0), "5m");
        assert_eq!(age(3.0 * 3600.0 + 120.0), "3h02m");
        assert_eq!(age(2.0 * 86400.0 + 3600.0), "2d01h");
    }
}
