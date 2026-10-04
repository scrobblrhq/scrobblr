//! `worker classify …`: review and maintain classification results.
//! Thresholds come from the same env vars as the running worker; to preview
//! other thresholds, run `reclassify --dry-run` with them set.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use chrono::{NaiveDate, TimeDelta, Utc};
use sqlx::PgPool;

use db::queries::classification::{self as cdb, Ruleset, StatusCounts};
use db::queries::users as users_db;
use shared::classification::{BudgetParams, Status};

pub const USAGE: &str =
    "       worker classify report     [--from DATE] [--to DATE] [--user NAME] [--limit N]
       worker classify reclassify [--from DATE] [--to DATE] [--user NAME] [--dry-run]
       worker classify backfill   [--from DATE] [--to DATE] [--dry-run]
                                  DATE is YYYY-MM-DD (UTC); report defaults to the last
                                  30 days, reclassify and backfill to all history";

#[derive(Debug, Default)]
struct Options {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    user: Option<String>,
    limit: Option<i64>,
    dry_run: bool,
}

fn parse(args: &[String]) -> anyhow::Result<Options> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--from" => {
                options.from = Some(value()?.parse().context("--from: expected YYYY-MM-DD")?)
            }
            "--to" => options.to = Some(value()?.parse().context("--to: expected YYYY-MM-DD")?),
            "--user" => options.user = Some(value()?.clone()),
            "--limit" => {
                options.limit = Some(value()?.parse().context("--limit: expected a number")?)
            }
            "--dry-run" => options.dry_run = true,
            other => bail!("unknown option `{other}` (see `worker --help`)"),
        }
    }
    Ok(options)
}

pub async fn run(db: &PgPool, args: &[String]) -> anyhow::Result<()> {
    let (command, rest) = args
        .split_first()
        .context("missing classify command (see `worker --help`)")?;
    let options = parse(rest)?;
    let params = super::params_from_env()?;
    match command.as_str() {
        "report" => report(db, params, options).await,
        "reclassify" => reclassify(db, params, options).await,
        "backfill" => backfill(db, params, options).await,
        other => bail!("unknown classify command `{other}` (see `worker --help`)"),
    }
}

async fn user_id(db: &PgPool, username: &str) -> anyhow::Result<i64> {
    Ok(users_db::find_by_username(db, username)
        .await?
        .with_context(|| format!("no user named {username}"))?
        .id)
}

/// The stored ruleset for `params`, registering it only when writing.
async fn ruleset(db: &PgPool, params: BudgetParams, write: bool) -> anyhow::Result<Ruleset> {
    if write {
        return Ok(cdb::register_ruleset(db, params).await?);
    }
    let id = cdb::find_ruleset(db, &params).await?.unwrap_or(0);
    Ok(Ruleset { id, params })
}

async fn report(db: &PgPool, params: BudgetParams, o: Options) -> anyhow::Result<()> {
    let to = o.to.unwrap_or_else(|| Utc::now().date_naive());
    let from = o.from.unwrap_or(to - TimeDelta::days(30));
    let limit = o.limit.unwrap_or(20);
    let rules = ruleset(db, params, false).await?;

    println!("thresholds  {}", params.fingerprint());
    println!(
        "            budget {} of listening per {} window",
        duration(params.budget_ms()),
        duration(params.window_ms)
    );
    if rules.id == 0 {
        println!("            (no day classified with these thresholds yet)");
    }
    println!("range       {from} .. {to}\n");

    if let Some(username) = &o.user {
        let user_id = user_id(db, username).await?;
        println!("{username}, by day:");
        println!("  day         ruleset  counted  suspect  no_data  peak load  classified");
        for d in cdb::user_days(db, user_id, from, to).await? {
            let marker = if d.ruleset_id == rules.id { ' ' } else { '*' };
            println!(
                "  {}  {:>6}{marker}  {:>7}  {:>7}  {:>7}  {:>9}  {}",
                d.day,
                d.ruleset_id,
                d.counts.counted,
                d.counts.suspect,
                d.counts.no_data,
                load(d.peak_load_ms, &params),
                d.classified_at.format("%Y-%m-%d %H:%M")
            );
        }
        println!("  (* = classified with other thresholds)");
        return Ok(());
    }

    let c = cdb::coverage(db, rules.id, from, to).await?;
    println!(
        "coverage    {} user-days with scrobbles: {} current, {} missing; {} stored days stale; {} queued\n",
        c.user_days, c.current, c.missing, c.stale, c.queued
    );

    println!("by thresholds:");
    for r in cdb::totals_by_ruleset(db, from, to).await? {
        let current = if r.ruleset_id == rules.id {
            "  (current)"
        } else {
            ""
        };
        println!("  #{} {}{current}", r.ruleset_id, r.fingerprint);
        println!("      {} days  {}", r.days, counts(&r.counts));
    }

    println!("\nusers with most suspect scrobbles (current thresholds):");
    println!("  user                  days  counted  suspect  no_data  suspect%  peak load");
    for u in cdb::top_suspect_users(db, rules.id, from, to, limit).await? {
        let total = u.counts.counted + u.counts.suspect + u.counts.no_data;
        println!(
            "  {:<20} {:>5}  {:>7}  {:>7}  {:>7}  {:>7.1}%  {:>9}",
            u.username,
            u.days,
            u.counts.counted,
            u.counts.suspect,
            u.counts.no_data,
            percent(u.counts.suspect, total),
            load(u.peak_load_ms, &params)
        );
    }

    println!("\nusers reporting track lengths under half of MusicBrainz's:");
    println!("  user                  plays  short  short%");
    for s in cdb::short_duration_reports(db, from, to, limit).await? {
        println!(
            "  {:<20} {:>6}  {:>5}  {:>5.1}%",
            s.username,
            s.plays,
            s.short,
            percent(s.short, s.plays)
        );
    }

    println!("\ntracks whose catalog length is off from MusicBrainz's by over 2x:");
    for t in cdb::duration_disagreements(db, limit).await? {
        println!(
            "  #{:<8} {} — {}: catalog {}, musicbrainz {} ({} scrobbles)",
            t.track_id,
            t.artist_name,
            t.title,
            duration(t.catalog_ms.into()),
            duration(t.musicbrainz_ms.into()),
            t.scrobble_count
        );
    }
    Ok(())
}

async fn reclassify(db: &PgPool, params: BudgetParams, o: Options) -> anyhow::Result<()> {
    let user_id = match &o.user {
        Some(username) => Some(user_id(db, username).await?),
        None => None,
    };
    let rules = ruleset(db, params, !o.dry_run).await?;
    let days = cdb::list_days(db, user_id, o.from, o.to).await?;
    eprintln!(
        "{} {} days with {}",
        if o.dry_run {
            "dry run over"
        } else {
            "reclassifying"
        },
        days.len(),
        params.fingerprint()
    );

    let mut totals = StatusCounts::default();
    let mut changes: BTreeMap<(Option<Status>, Status), i64> = BTreeMap::new();
    for (n, day) in days.iter().enumerate() {
        let outcome = cdb::classify_user_day(db, &rules, day.user_id, day.day, o.dry_run).await?;
        totals.counted += outcome.counts.counted;
        totals.suspect += outcome.counts.suspect;
        totals.no_data += outcome.counts.no_data;
        for (key, count) in outcome.changes {
            *changes.entry(key).or_default() += count;
        }
        if (n + 1) % 1000 == 0 {
            eprintln!("  {}/{} days", n + 1, days.len());
        }
    }

    println!("{}", counts(&totals));
    if changes.is_empty() {
        println!("no label changes");
    }
    for ((before, after), count) in changes {
        let before = before.map_or("unclassified", Status::as_str);
        println!("  {before:>12} -> {:<8} {count}", after.as_str());
    }
    if o.dry_run {
        println!("(dry run: nothing written)");
    }
    Ok(())
}

async fn backfill(db: &PgPool, params: BudgetParams, o: Options) -> anyhow::Result<()> {
    if o.user.is_some() {
        bail!("backfill covers all users; use reclassify --user for one");
    }
    let rules = ruleset(db, params, !o.dry_run).await?;
    let mut tx = db.begin().await?;
    let queued =
        cdb::enqueue_stale(&mut tx, rules.id, o.from, o.to, None, cdb::PRIORITY_SWEEP).await?;
    if o.dry_run {
        tx.rollback().await?;
        println!("would queue {queued} days (dry run: nothing written)");
    } else {
        tx.commit().await?;
        println!("queued {queued} days; the running worker classifies them");
    }
    Ok(())
}

fn counts(c: &StatusCounts) -> String {
    let total = c.counted + c.suspect + c.no_data;
    format!(
        "counted {} · suspect {} ({:.2}%) · no_data {} ({:.2}%)",
        c.counted,
        c.suspect,
        percent(c.suspect, total),
        c.no_data,
        percent(c.no_data, total)
    )
}

fn percent(part: i64, total: i64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

/// Listening time in the window relative to the window's length.
fn load(load_ms: Option<i64>, params: &BudgetParams) -> String {
    load_ms.map_or("-".into(), |ms| {
        format!("{:.1}x", ms as f64 / params.window_ms as f64)
    })
}

fn duration(ms: i64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, secs % 3600 / 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}
