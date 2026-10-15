//! Tag counts (`db::queries::tags`): the loop recounting the entities whose
//! tags changed, and all of them every 6 hours, since weights change on
//! their own (accounts coming of age, coordination runs, suspensions
//! ending); and `worker tags`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::bail;
use chrono::Utc;
use sqlx::PgPool;

use crate::heartbeat::Beat;
use db::queries::tags::{self as tags_db, Weighing};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const FULL_EVERY: Duration = Duration::from_secs(6 * 3600);
const BATCH: i64 = 500;

pub const USAGE: &str =
    "       worker tags refresh         recount every artist's, album's and track's tags now
       worker tags report [--limit N]
                                  the N (20) most used tags, with how many taggings
                                  they have, how many entities show them, and what
                                  the weights took off";

pub struct Counter {
    db: PgPool,
}

impl Counter {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }

    pub async fn run(self: Arc<Self>, beat: Beat) {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_full: Option<Instant> = None;
        loop {
            interval.tick().await;
            let full = last_full.is_none_or(|at| at.elapsed() >= FULL_EVERY);
            match self.pass(full).await {
                Ok(n) => {
                    if full {
                        last_full = Some(Instant::now());
                        tracing::info!("tags: recounted {n} entities");
                    }
                    beat.ok().await;
                }
                Err(e) => {
                    tracing::error!("tags: recount failed, retrying later: {e}");
                    beat.failed(e).await;
                }
            }
        }
    }

    async fn pass(&self, full: bool) -> Result<usize, sqlx::Error> {
        if full {
            tags_db::enqueue_all(&self.db).await?;
            tags_db::purge_unused(&self.db).await?;
        }
        drain(&self.db).await
    }
}

/// Recounts every due entity: how many.
pub async fn drain(db: &PgPool) -> Result<usize, sqlx::Error> {
    let mut done = 0;
    loop {
        let n = tags_db::recount_due(db, Weighing::at(Utc::now()), BATCH).await?;
        done += n;
        if n < BATCH as usize {
            return Ok(done);
        }
    }
}

pub async fn run(db: &PgPool, args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("refresh") => {
            let started = Instant::now();
            tags_db::enqueue_all(db).await?;
            let n = drain(db).await?;
            println!(
                "recounted {n} entities in {:.1} s",
                started.elapsed().as_secs_f64()
            );
            Ok(())
        }
        Some("report") => {
            let limit = match args.get(1).map(String::as_str) {
                Some("--limit") => args.get(2).and_then(|n| n.parse().ok()).unwrap_or(20),
                _ => 20,
            };
            report(db, limit).await
        }
        _ => bail!("unknown tags command (see `worker --help`)"),
    }
}

async fn report(db: &PgPool, limit: i64) -> anyhow::Result<()> {
    let min = i64::from(
        shared::ranking::parse_min_listeners(
            crate::non_empty_env("RANKING_MIN_LISTENERS").as_deref(),
        )
        .map_err(anyhow::Error::msg)?,
    ) * shared::ranking::FULL;
    let rows: Vec<(String, i64, i64, i64, i64, i64)> = sqlx::query_as(
        r#"
        SELECT t.name, sum(c.taggers)::bigint, count(*),
               count(*) FILTER (WHERE c.weight >= $1),
               sum(c.taggers)::bigint * 1000 - sum(c.weight)::bigint,
               max(c.weight)
        FROM tag_counts c JOIN tags t ON t.id = c.tag_id
        GROUP BY t.name
        ORDER BY sum(c.taggers) DESC, t.name
        LIMIT $2
        "#,
    )
    .bind(min)
    .bind(limit)
    .fetch_all(db)
    .await?;
    println!(
        "{:<30} {:>9} {:>9} {:>7} {:>13} {:>10}",
        "tag", "taggings", "entities", "shown", "weight off", "best"
    );
    for (name, taggings, entities, shown, off, best) in rows {
        println!(
            "{name:<30} {taggings:>9} {entities:>9} {shown:>7} {:>13.1} {:>10.1}",
            off as f64 / 1000.0,
            best as f64 / 1000.0
        );
    }
    Ok(())
}
