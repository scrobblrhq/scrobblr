//! `worker rank …`: compare raw and filtered global rankings, and maintain
//! the weights. Params come from the same `RANKING_*` env vars as the
//! running worker; to preview others, `recompute` with them set, into a
//! scratch copy of the database.

use anyhow::{Context, bail};
use chrono::{NaiveDate, TimeDelta, Utc};
use sqlx::PgPool;

use db::queries::coordination as cdb;
use db::queries::rankings::{self as rdb, DayWeights, Kind, Period, Ranked, Ruleset};
use db::queries::users as users_db;
use shared::coordination::{Evidence, Signal};
use shared::ranking::{FULL, RankingParams, Reason};

pub const USAGE: &str = "       worker rank report    [--from DATE | --all] [--to DATE] [--limit N]
       worker rank recompute [--from DATE] [--to DATE] [--user NAME] [--dry-run]
       worker rank backfill  [--from DATE] [--to DATE] [--dry-run]
       worker rank refresh   [--period week|month|year]
       worker rank top       [--period week|month|year] [--kind artist|track] [--limit N]
       worker rank detect    [--dry-run] [--limit N] [--members]
       worker rank cohorts   [--dry-run] [--limit N] [--members]
                             report compares raw and filtered rankings, by default over
                             the last 7 days, and with and without coordination factors;
                             recompute and backfill cover all history; refresh recomputes
                             the stored rankings now (the worker does on a schedule) and
                             top prints one as a route would read it; detect looks for
                             coordinated accounts now and stores their factors (the worker
                             does every 6 hours); cohorts lists the latest run's cohorts,
                             or with --dry-run a fresh detection that stores nothing";

#[derive(Debug, Default)]
struct Options {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    user: Option<String>,
    limit: Option<i64>,
    period: Option<Period>,
    kind: Option<Kind>,
    all: bool,
    dry_run: bool,
    members: bool,
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
            "--period" => {
                let v = value()?;
                options.period =
                    Some(Period::parse(v).with_context(|| format!("unknown period `{v}`"))?)
            }
            "--kind" => {
                let v = value()?;
                options.kind = Some(Kind::parse(v).with_context(|| format!("unknown kind `{v}`"))?)
            }
            "--all" => options.all = true,
            "--dry-run" => options.dry_run = true,
            "--members" => options.members = true,
            other => bail!("unknown option `{other}` (see `worker --help`)"),
        }
    }
    Ok(options)
}

pub async fn run(db: &PgPool, args: &[String]) -> anyhow::Result<()> {
    let (command, rest) = args
        .split_first()
        .context("missing rank command (see `worker --help`)")?;
    let options = parse(rest)?;
    let params = super::params_from_env()?;
    match command.as_str() {
        "report" => report(db, params, options).await,
        "recompute" => recompute(db, params, options).await,
        "backfill" => backfill(db, params, options).await,
        "refresh" => refresh(db, params, options).await,
        "top" => top(db, options).await,
        "detect" => detect(db, options, true).await,
        "cohorts" => detect(db, options, false).await,
        other => bail!("unknown rank command `{other}` (see `worker --help`)"),
    }
}

/// The stored ruleset for `params`, registering it only when writing.
async fn ruleset(db: &PgPool, params: RankingParams, write: bool) -> anyhow::Result<Ruleset> {
    if write {
        return Ok(rdb::register_ruleset(db, &params).await?);
    }
    let id = rdb::find_ruleset(db, &params).await?.unwrap_or(0);
    Ok(Ruleset { id, params })
}

async fn report(db: &PgPool, params: RankingParams, o: Options) -> anyhow::Result<()> {
    let today = Utc::now().date_naive();
    let (from, to) = match (o.from, o.all) {
        (Some(from), _) => (from, o.to.unwrap_or(today)),
        (None, true) => {
            let (first, last) = rdb::weighed_range(db).await?.unwrap_or((today, today));
            (first, o.to.unwrap_or(last))
        }
        (None, false) => {
            let to = o.to.unwrap_or(today);
            (to - TimeDelta::days(6), to)
        }
    };
    let limit = o.limit.unwrap_or(20);
    let rules = ruleset(db, params, false).await?;

    println!("params      {}", rules.params.fingerprint());
    if rules.id == 0 {
        println!("            (no day weighed with these params yet)");
    }
    println!("range       {from} .. {to}");
    let c = rdb::coverage(db, rules.id, from, to).await?;
    println!(
        "coverage    {} classified user-days, {} weighed ({} with these params and the current \
         labels), {} queued\n",
        c.classified_days, c.weighed_days, c.current, c.queued
    );

    let totals = rdb::totals(db, from, to).await?;
    println!(
        "plays       {} raw · {} kept some weight · total weight {} ({:.1}% of raw)",
        totals.plays,
        totals.weighted,
        plays(totals.weight),
        percent(totals.weight, totals.plays * FULL)
    );
    println!("down-weighted, by reason (a play can have several):");
    for reason in Reason::ALL {
        let n = totals.reasons.get(reason);
        if n > 0 {
            println!(
                "  {:<15} {:>9}  {:>5.1}%",
                reason.as_str(),
                n,
                percent(n, totals.plays)
            );
        }
    }

    let listener_weight = rules.params.listener_weight();
    for (kind, title) in [(Kind::Artist, "artists"), (Kind::Track, "tracks")] {
        let ranked = rdb::rankings(db, kind, from, to, listener_weight, limit).await?;
        compare(
            &format!("{title} by listeners (filtered: listener credits, then weight)"),
            &ranked,
            limit,
            |r| r.raw_rank,
            |r| r.rank,
        );
        compare(
            &format!("{title} by plays (filtered: weight)"),
            &ranked,
            limit,
            |r| r.raw_play_rank,
            |r| r.weight_rank,
        );
    }

    coordination_section(db, from, to, limit, listener_weight).await?;

    let (users, contributors) = rdb::contributors(db, from, to, limit).await?;
    let moved = contributors
        .iter()
        .filter(|c| c.raw_rank <= limit && c.rank != c.raw_rank)
        .count();
    let affected = contributors
        .iter()
        .filter(|c| c.totals.weight < c.totals.plays * FULL)
        .count();
    println!(
        "\nusers: {users} scrobbled in range; of the top {limit} by plays, {moved} changed position \
         when ranked by weight ({affected} of those listed lost weight)"
    );
    println!("  raw  filt  user                     plays    weight   kept   reasons (plays)");
    for c in &contributors {
        println!(
            "  {:>4}  {:>4}  {:<22} {:>7}  {:>8}  {:>5.1}%  {}",
            c.raw_rank,
            c.rank,
            truncate(&c.username, 22),
            c.totals.plays,
            plays(c.totals.weight),
            percent(c.totals.weight, c.totals.plays * FULL),
            reasons(&c.totals)
        );
    }
    Ok(())
}

/// Prints the top `limit` by `filtered` beside their `raw` positions, then
/// what entered and left the top.
fn compare(
    title: &str,
    ranked: &[Ranked],
    limit: i64,
    raw: impl Fn(&Ranked) -> i64,
    filtered: impl Fn(&Ranked) -> i64,
) {
    let mut top: Vec<&Ranked> = ranked.iter().filter(|r| filtered(r) <= limit).collect();
    top.sort_by_key(|r| filtered(r));
    let moved = top.iter().filter(|r| raw(r) != filtered(r)).count();
    println!("\ntop {limit} {title}: {moved} changed position");
    println!(
        "  filt   raw  move   {:<40} raw listeners  raw plays  listeners    weight",
        "name"
    );
    for r in &top {
        println!(
            "  {:>4}  {:>4}  {:>4}   {:<40} {:>13}  {:>9}  {:>9}  {:>8}",
            filtered(r),
            raw(r),
            movement(raw(r), filtered(r)),
            truncate(&label(r), 40),
            r.raw_listeners,
            r.raw_plays,
            plays(r.listeners),
            plays(r.weight)
        );
    }
    let mut left: Vec<&Ranked> = ranked
        .iter()
        .filter(|r| raw(r) <= limit && filtered(r) > limit)
        .collect();
    left.sort_by_key(|r| raw(r));
    for r in left {
        println!(
            "  left the top: raw #{} {} -> #{} ({} raw listeners, {} raw plays; {} listeners, \
             {} weight)",
            raw(r),
            label(r),
            filtered(r),
            r.raw_listeners,
            r.raw_plays,
            plays(r.listeners),
            plays(r.weight)
        );
    }
}

fn label(r: &Ranked) -> String {
    match &r.artist_name {
        Some(artist) => format!("{artist} — {}", r.name),
        None => r.name.clone(),
    }
}

fn movement(raw: i64, filtered: i64) -> String {
    match raw - filtered {
        0 => "=".into(),
        d if d > 0 => format!("+{d}"),
        d => d.to_string(),
    }
}

async fn recompute(db: &PgPool, params: RankingParams, o: Options) -> anyhow::Result<()> {
    let user_id = match &o.user {
        Some(username) => Some(
            users_db::find_by_username(db, username)
                .await?
                .with_context(|| format!("no user named {username}"))?
                .id,
        ),
        None => None,
    };
    let rules = ruleset(db, params, !o.dry_run).await?;
    let days = rdb::list_days(db, user_id, o.from, o.to).await?;
    eprintln!(
        "{} {} days with {}",
        if o.dry_run {
            "dry run over"
        } else {
            "weighing"
        },
        days.len(),
        rules.params.fingerprint()
    );
    let mut totals = DayWeights::default();
    for (n, day) in days.iter().enumerate() {
        let weights = rdb::weigh_user_day(db, &rules, day.user_id, day.day, o.dry_run).await?;
        totals.add(&weights);
        if (n + 1) % 1000 == 0 {
            eprintln!("  {}/{} days", n + 1, days.len());
        }
    }
    println!(
        "{} plays · {} kept some weight · total weight {} ({:.1}%)",
        totals.plays,
        totals.weighted,
        plays(totals.weight),
        percent(totals.weight, totals.plays * FULL)
    );
    println!("  {}", reasons(&totals));
    if o.dry_run {
        println!("(dry run: nothing written)");
    }
    Ok(())
}

async fn backfill(db: &PgPool, params: RankingParams, o: Options) -> anyhow::Result<()> {
    if o.user.is_some() {
        bail!("backfill covers all users; use recompute --user for one");
    }
    let rules = ruleset(db, params, !o.dry_run).await?;
    let mut tx = db.begin().await?;
    let queued = rdb::enqueue_stale(&mut tx, rules.id, o.from, o.to, None).await?;
    if o.dry_run {
        tx.rollback().await?;
        println!("would queue {queued} days (dry run: nothing written)");
    } else {
        tx.commit().await?;
        println!("queued {queued} days; the running worker weighs them");
    }
    Ok(())
}

async fn refresh(db: &PgPool, params: RankingParams, o: Options) -> anyhow::Result<()> {
    let rules = ruleset(db, params, true).await?;
    let today = Utc::now().date_naive();
    for period in o.period.map_or(Period::ALL.to_vec(), |p| vec![p]) {
        for s in rdb::refresh_snapshots(db, &rules, period, today, super::SNAPSHOT_TOP).await? {
            println!(
                "{:<5} {:<6} {} .. {}  {:>7} ranked  {:>6} days pending  {:>6} ms",
                period.as_str(),
                s.kind.as_str(),
                s.from_day,
                s.to_day,
                s.ranked,
                s.pending_days,
                s.took_ms
            );
        }
    }
    Ok(())
}

async fn top(db: &PgPool, o: Options) -> anyhow::Result<()> {
    let period = o.period.unwrap_or(Period::Week);
    let kind = o.kind.unwrap_or(Kind::Artist);
    let stored = rdb::snapshots(db).await?;
    let Some(s) = stored.iter().find(|s| s.period == period && s.kind == kind) else {
        bail!(
            "no stored {} ranking yet (worker rank refresh)",
            period.as_str()
        );
    };
    let age = Utc::now() - s.computed_at;
    println!(
        "{} {}s, {} .. {}, computed {} min ago in {} ms ({} ranked, {} days were pending)",
        period.as_str(),
        kind.as_str(),
        s.from_day,
        s.to_day,
        age.num_minutes(),
        s.took_ms,
        s.ranked,
        s.pending_days
    );
    let started = std::time::Instant::now();
    let entries = rdb::snapshot_entries(db, period, kind, 0, o.limit.unwrap_or(20)).await?;
    let took = started.elapsed();
    println!(
        "   #  {:<40} listeners    weight  raw listeners  raw plays",
        "name"
    );
    for e in &entries {
        let name = match (&e.artist_name, &e.name) {
            (Some(artist), Some(name)) => format!("{artist} — {name}"),
            (None, Some(name)) => name.clone(),
            (_, None) => format!("(deleted #{})", e.entity_id),
        };
        println!(
            "{:>4}  {:<40} {:>9}  {:>8}  {:>13}  {:>9}",
            e.position,
            truncate(&name, 40),
            plays(e.listeners),
            plays(e.weight),
            e.raw_listeners,
            e.raw_plays
        );
    }
    println!("(read in {:.1} ms)", took.as_secs_f64() * 1000.0);
    Ok(())
}

/// The latest coordination run, and the rankings with its factors beside
/// the same without them.
async fn coordination_section(
    db: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    limit: i64,
    listener_weight: i64,
) -> anyhow::Result<()> {
    println!("\ncoordination");
    match cdb::latest_run(db).await? {
        None => println!("  no detection run yet (worker rank detect)"),
        Some(run) => println!(
            "  latest run #{} ({}) at {}, over {} .. {}: {} active accounts, {} flagged in {} \
             cohorts{}",
            run.id,
            run.mode,
            run.started_at.format("%Y-%m-%d %H:%M UTC"),
            run.window_from,
            run.window_to,
            run.accounts,
            run.flagged,
            run.cohorts,
            if run.too_small {
                " (too few active accounts: nothing judged)"
            } else {
                ""
            }
        ),
    }
    let t = rdb::coordination_totals(db, from, to).await?;
    println!(
        "  in range: {} accounts on {} user-days had a factor below 1; filtered weight {} -> {} \
         ({:.1}% kept)",
        t.accounts,
        t.user_days,
        plays(t.weight),
        plays(t.coordinated),
        percent(t.coordinated, t.weight)
    );
    for (kind, title) in [(Kind::Artist, "artists"), (Kind::Track, "tracks")] {
        let ranked = rdb::rankings(db, kind, from, to, listener_weight, limit).await?;
        let mut top: Vec<&Ranked> = ranked.iter().filter(|r| r.rank <= limit).collect();
        top.sort_by_key(|r| r.rank);
        let moved = top
            .iter()
            .filter(|r| r.rank != r.uncoordinated_rank)
            .count();
        println!(
            "\n  top {limit} {title} by listeners with coordination factors: {moved} changed position"
        );
        println!(
            "    with  without  move   {:<40} listeners without/with    weight without/with",
            "name"
        );
        for r in &top {
            println!(
                "    {:>4}  {:>7}  {:>4}   {:<40} {:>10} {:>9}  {:>10} {:>9}",
                r.rank,
                r.uncoordinated_rank,
                movement(r.uncoordinated_rank, r.rank),
                truncate(&label(r), 40),
                plays(r.uncoordinated_listeners),
                plays(r.listeners),
                plays(r.uncoordinated_weight),
                plays(r.weight)
            );
        }
        let mut left: Vec<&Ranked> = ranked
            .iter()
            .filter(|r| r.uncoordinated_rank <= limit && r.rank > limit)
            .collect();
        left.sort_by_key(|r| r.uncoordinated_rank);
        for r in left {
            println!(
                "    left the top: #{} {} -> #{} (listeners {} -> {}, weight {} -> {})",
                r.uncoordinated_rank,
                label(r),
                r.rank,
                plays(r.uncoordinated_listeners),
                plays(r.listeners),
                plays(r.uncoordinated_weight),
                plays(r.weight)
            );
        }
    }
    Ok(())
}

/// A cohort, stored or from a dry run.
struct CohortView {
    cohort: i32,
    kind: Kind,
    entity_id: i64,
    members: i32,
    score: i32,
    factor: i32,
    evidence: Evidence,
    detail: serde_json::Value,
}

/// `detect` runs detection now (and stores it unless `--dry-run`);
/// `cohorts` lists the latest stored run, or a fresh dry run.
async fn detect(db: &PgPool, o: Options, run: bool) -> anyhow::Result<()> {
    let params = super::coordination::params_from_env()?;
    let limit = o.limit.unwrap_or(20);
    let (cohorts, accounts) = if run || o.dry_run {
        if params.mode == shared::coordination::Mode::Off {
            bail!("RANKING_COORDINATION=off: nothing to detect");
        }
        let today = Utc::now().date_naive();
        let out = super::coordination::run_once(db, &params, today, o.dry_run).await?;
        let d = &out.detection;
        println!(
            "{} over {} .. {} (plays from {}): loaded {} (account, track) pairs and {} plays in \
             {:.1} s, detected in {:.1} s",
            match out.run_id {
                Some(id) => format!("run #{id}"),
                None => "dry run".to_string(),
            },
            out.window.from,
            out.window.to,
            out.window.sync_from,
            out.loaded.pairs,
            out.loaded.plays,
            out.load.as_secs_f64(),
            out.detect.as_secs_f64()
        );
        println!("params: {}", params.fingerprint());
        summary(d.active, d.groups, d.too_small);
        let ids: Vec<i64> = d.accounts.iter().map(|a| a.user_id).collect();
        let names = cdb::usernames(db, &ids).await?;
        let cohorts: Vec<CohortView> = d
            .cohorts
            .iter()
            .enumerate()
            .map(|(i, c)| CohortView {
                cohort: i as i32,
                kind: c.kind,
                entity_id: c.entity_id,
                members: c.members as i32,
                score: c.score as i32,
                factor: c.factor as i32,
                evidence: c.evidence,
                detail: cdb::detail_json(&c.detail),
            })
            .collect();
        let accounts: Vec<cdb::StoredAccount> = d
            .accounts
            .iter()
            .map(|a| {
                let (username, registered) = names.get(&a.user_id).cloned().unwrap_or_default();
                cdb::StoredAccount {
                    user_id: a.user_id,
                    username,
                    registered,
                    cohort: a.cohort as i32,
                    suspicion: a.suspicion as i32,
                    factor: a.factor as i32,
                    evidence: a.evidence,
                }
            })
            .collect();
        (cohorts, accounts)
    } else {
        let Some(run) = cdb::latest_run(db).await? else {
            bail!("no detection run yet (worker rank detect, or --dry-run)");
        };
        println!(
            "run #{} ({}) at {}, took {:.1} s, over {} .. {}",
            run.id,
            run.mode,
            run.started_at.format("%Y-%m-%d %H:%M UTC"),
            f64::from(run.took_ms) / 1000.0,
            run.window_from,
            run.window_to
        );
        println!("params: {}", run.fingerprint);
        summary(
            i64::from(run.accounts),
            i64::from(run.groups),
            run.too_small,
        );
        let cohorts = cdb::cohorts(db, run.id)
            .await?
            .into_iter()
            .map(|c| CohortView {
                cohort: c.cohort,
                kind: c.kind,
                entity_id: c.entity_id,
                members: c.members,
                score: c.score,
                factor: c.factor,
                evidence: c.evidence,
                detail: c.detail,
            })
            .collect();
        (cohorts, cdb::accounts(db, run.id).await?)
    };
    print_cohorts(db, &cohorts, &accounts, limit, o.members).await
}

fn summary(active: i64, groups: i64, too_small: bool) {
    if too_small {
        println!(
            "{active} active accounts: fewer than RANKING_COORDINATION_MIN_ACCOUNTS, nothing judged"
        );
    } else {
        println!("{active} active accounts, {groups} artists and tracks examined");
    }
}

async fn print_cohorts(
    db: &PgPool,
    cohorts: &[CohortView],
    accounts: &[cdb::StoredAccount],
    limit: i64,
    members: bool,
) -> anyhow::Result<()> {
    let zeroed = accounts.iter().filter(|a| a.factor == 0).count();
    println!(
        "{} accounts flagged ({} at factor 0, {} dented) in {} cohorts",
        accounts.len(),
        zeroed,
        accounts.len() - zeroed,
        cohorts.len()
    );
    let target = |t: &serde_json::Value| {
        Some((
            Kind::parse(t["kind"].as_str()?)?,
            t["entity_id"].as_i64()?,
            t["members"].as_i64()?,
            t["listeners"].as_i64()?,
            t["weight_share"].as_i64()?,
        ))
    };
    let shown: Vec<&CohortView> = cohorts.iter().take(limit.max(0) as usize).collect();
    let mut entities: Vec<(Kind, i64)> = Vec::new();
    for c in &shown {
        entities.push((c.kind, c.entity_id));
        if let Some(targets) = c.detail["targets"].as_array() {
            entities.extend(targets.iter().filter_map(target).map(|t| (t.0, t.1)));
        }
    }
    let names = cdb::entity_names(db, &entities).await?;
    let name = |kind: Kind, id: i64| {
        names
            .get(&(kind, id))
            .cloned()
            .unwrap_or_else(|| format!("#{id}"))
    };
    let share = |permille: i64| format!("{:.2}", permille as f64 / FULL as f64);
    for c in shown {
        println!(
            "\ncohort {} · {} {} · {} accounts · score {} · mean factor {}",
            c.cohort + 1,
            c.kind.as_str(),
            name(c.kind, c.entity_id),
            c.members,
            share(i64::from(c.score)),
            share(i64::from(c.factor))
        );
        let d = &c.detail;
        for signal in Signal::ALL {
            let value = c.evidence.get(signal);
            let what = match signal {
                Signal::Age => d["age"]
                    .as_array()
                    .map(|cells| {
                        cells
                            .iter()
                            .map(|w| {
                                format!(
                                    "registered {}..{}: {} accounts, purity {}",
                                    w["from"].as_str().unwrap_or("?"),
                                    w["to"].as_str().unwrap_or("?"),
                                    w["members"],
                                    share(w["purity"].as_i64().unwrap_or(0))
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default(),
                Signal::Origin | Signal::Client => {
                    let cell = &d[signal.as_str()];
                    if cell.is_null() {
                        String::new()
                    } else {
                        format!(
                            "{} ({} values): {} accounts, purity {}",
                            cell["value"].as_str().unwrap_or("?"),
                            cell["values"],
                            cell["members"],
                            share(cell["purity"].as_i64().unwrap_or(0))
                        )
                    }
                }
                Signal::Sync => {
                    let cell = &d["sync"];
                    if cell.is_null() {
                        String::new()
                    } else {
                        format!(
                            "lockstep: {} accounts, median {} days with crowds",
                            cell["members"], cell["median_days"]
                        )
                    }
                }
                Signal::Shape => String::new(),
                Signal::Burst => format!(
                    "new listeners: {}% of the group",
                    d["new_share"].as_i64().unwrap_or(0) / 10
                ),
            };
            if value == 0 && what.is_empty() {
                println!("  {:<7} -", signal.as_str());
            } else {
                println!("  {:<7} {}  {}", signal.as_str(), share(value), what);
            }
        }
        if let Some(targets) = d["targets"].as_array() {
            for (n, t) in targets.iter().filter_map(target).enumerate() {
                println!(
                    "  {} {} ({}): {} of {} listeners, {:.1}% of its weight",
                    if n == 0 { "targets" } else { "       " },
                    name(t.0, t.1),
                    t.0.as_str(),
                    t.2,
                    t.3,
                    t.4 as f64 / 10.0
                );
            }
        }
        if members {
            println!(
                "  {:<22} {:<10} {:>5} {:>6}  age   orig  cli   sync  shape burst",
                "user", "registered", "susp", "factor"
            );
            for a in accounts.iter().filter(|a| a.cohort == c.cohort) {
                let e: Vec<String> = Signal::ALL
                    .into_iter()
                    .map(|s| share(a.evidence.get(s)))
                    .collect();
                println!(
                    "  {:<22} {:<10} {:>5} {:>6}  {}",
                    truncate(&a.username, 22),
                    a.registered,
                    share(i64::from(a.suspicion)),
                    share(i64::from(a.factor)),
                    e.join("  ")
                );
            }
        }
    }
    if cohorts.len() as i64 > limit {
        println!(
            "\n({} more cohorts; --limit N)",
            cohorts.len() as i64 - limit
        );
    }
    Ok(())
}

fn reasons(w: &DayWeights) -> String {
    let listed: Vec<String> = Reason::ALL
        .into_iter()
        .filter(|r| w.reasons.get(*r) > 0)
        .map(|r| format!("{} {}", r.as_str(), w.reasons.get(r)))
        .collect();
    if listed.is_empty() {
        "-".into()
    } else {
        listed.join(", ")
    }
}

/// A sum of weights (or credits) in thousandths, as plays.
fn plays(permille: i64) -> String {
    format!("{:.1}", permille as f64 / FULL as f64)
}

fn percent(part: i64, total: i64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}
