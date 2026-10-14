//! `worker similarity …`: recompute now, print what is stored for an artist
//! or a user, compare results with and without the anti-botting weights
//! and coordination factors, and evaluate recommendations offline. Params
//! come from the same `SIMILARITY_*` env vars as the running worker.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use anyhow::{Context, bail};
use chrono::{TimeDelta, Utc};
use sqlx::PgPool;

use db::queries::coordination as cdb;
use db::queries::similarity::{self as sdb, Source, Window};
use db::queries::users as users_db;
use shared::ranking::Kind;
use shared::similarity::{
    self as similarity, ArtistNeighbour, FULL, Input, Matrix, Recommendation, SimilarityParams,
    eval,
};

pub const USAGE: &str = "       worker similarity refresh [--dry-run]
       worker similarity artist  NAME [--limit N] [--fresh | --raw]
       worker similarity user    NAME [--limit N]
       worker similarity report  [--artist NAME]... [--limit N]
       worker similarity eval    [--holdout-days N] [--n N] [--min-plays N]
                                 refresh recomputes similar artists, recommendations and
                                 taste neighbours now (the worker does once a day); artist
                                 prints an artist's stored neighbours, or computed now from
                                 the weights (--fresh) or from every play (--raw); user prints
                                 what a user is recommended and their taste neighbours;
                                 report compares results with and without the anti-botting
                                 weights and coordination factors; eval holds out the last
                                 days (14) and scores recommendations made before them
                                 against a popularity baseline";

#[derive(Debug, Default)]
struct Options {
    name: Option<String>,
    artists: Vec<String>,
    limit: Option<usize>,
    holdout_days: Option<i64>,
    n: Option<usize>,
    min_plays: Option<i64>,
    dry_run: bool,
    fresh: bool,
    raw: bool,
}

fn parse(args: &[String]) -> anyhow::Result<Options> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--artist" => options.artists.push(value()?.clone()),
            "--limit" => {
                options.limit = Some(value()?.parse().context("--limit: expected a number")?)
            }
            "--holdout-days" => {
                options.holdout_days = Some(
                    value()?
                        .parse()
                        .context("--holdout-days: expected a number")?,
                )
            }
            "--n" => options.n = Some(value()?.parse().context("--n: expected a number")?),
            "--min-plays" => {
                options.min_plays =
                    Some(value()?.parse().context("--min-plays: expected a number")?)
            }
            "--dry-run" => options.dry_run = true,
            "--fresh" => options.fresh = true,
            "--raw" => options.raw = true,
            other if !other.starts_with("--") && options.name.is_none() => {
                options.name = Some(other.to_string())
            }
            other => bail!("unknown option `{other}` (see `worker --help`)"),
        }
    }
    Ok(options)
}

pub async fn run(db: &PgPool, args: &[String]) -> anyhow::Result<()> {
    let (command, rest) = args
        .split_first()
        .context("missing similarity command (see `worker --help`)")?;
    let options = parse(rest)?;
    let params = super::params_from_env()?;
    match command.as_str() {
        "refresh" => refresh(db, &params, options).await,
        "artist" => artist(db, &params, options).await,
        "user" => user(db, options).await,
        "report" => report(db, &params, options).await,
        "eval" => evaluate(db, &params, options).await,
        other => bail!("unknown similarity command `{other}` (see `worker --help`)"),
    }
}

async fn refresh(db: &PgPool, params: &SimilarityParams, o: Options) -> anyhow::Result<()> {
    let today = Utc::now().date_naive();
    let out = super::run_once(db, params, today, o.dry_run).await?;
    let pairs: usize = out.output.artists.values().map(Vec::len).sum();
    println!(
        "{} over {} .. {}",
        match out.run_id {
            Some(id) => format!("run #{id}"),
            None => "dry run".to_string(),
        },
        out.window.from,
        out.window.to
    );
    println!("params      {}", params.fingerprint());
    println!(
        "read        {} accounts, {} (account, artist) credits, {} history rows, {} users served",
        out.loaded.accounts, out.loaded.credits, out.loaded.history_rows, out.loaded.served
    );
    println!(
        "made        {} accounts with credit ({} listed), {} artists with {} neighbours, \
         {} users with recommendations, {} with taste neighbours",
        out.output.contributors,
        out.output.candidates,
        out.output.artists.len(),
        pairs,
        out.output.recommendations.len(),
        out.output.neighbours.len()
    );
    println!(
        "took        load {:.1} s, compute {:.1} s, store {:.1} s",
        out.load.as_secs_f64(),
        out.compute.as_secs_f64(),
        out.store.as_secs_f64()
    );
    Ok(())
}

async fn artist_id(db: &PgPool, name: &str) -> anyhow::Result<i64> {
    if let Ok(id) = name.parse::<i64>() {
        return Ok(id);
    }
    sdb::find_artist(db, name)
        .await?
        .with_context(|| format!("no artist called `{name}`"))
}

async fn artist(db: &PgPool, params: &SimilarityParams, o: Options) -> anyhow::Result<()> {
    let name = o.name.as_deref().context("artist: which one?")?;
    let id = artist_id(db, name).await?;
    let limit = o.limit.unwrap_or(20);
    let list: Vec<(i64, i64, i64)> = if o.fresh || o.raw {
        let source = if o.raw {
            Source::Plays
        } else {
            Source::Weights
        };
        let window = Window::new(Utc::now().date_naive(), params);
        let (input, _) = sdb::load(db, &window, params, source).await?;
        let m = Matrix::new(&input.contributors, &input.credits, params);
        println!(
            "computed now from {} over {} .. {}; audience {:.1} accounts",
            if o.raw { "every play" } else { "the weights" },
            window.from,
            window.to,
            m.audience(id)
        );
        similarity::artist_neighbours(&m, params)
            .remove(&id)
            .unwrap_or_default()
            .into_iter()
            .map(|n| (n.artist_id, n.score, n.listeners))
            .collect()
    } else {
        match sdb::latest_run(db).await? {
            Some(run) => println!("stored by run #{} at {}", run.id, run.finished_at),
            None => println!("no run stored yet"),
        }
        sdb::stored_neighbours(db, id)
            .await?
            .into_iter()
            .map(|n| (n.artist_id, n.score, n.listeners))
            .collect()
    };
    let ids: Vec<i64> = list.iter().map(|(a, _, _)| *a).collect();
    let names = sdb::artist_names(db, &ids).await?;
    println!("  pos  {:<40} score  listeners", "neighbour");
    for (i, (a, score, listeners)) in list.iter().take(limit).enumerate() {
        println!(
            "  {:>3}  {:<40} {:>5.3}  {:>9}",
            i + 1,
            truncate(names.get(a).map_or("?", String::as_str), 40),
            *score as f64 / FULL as f64,
            listeners
        );
    }
    if list.is_empty() {
        println!("  (none: too few listeners, or none with enough in common)");
    }
    Ok(())
}

async fn user(db: &PgPool, o: Options) -> anyhow::Result<()> {
    let name = o.name.as_deref().context("user: which one?")?;
    let user = users_db::find_by_username(db, name)
        .await?
        .with_context(|| format!("no user called `{name}`"))?;
    let limit = o.limit.unwrap_or(20);
    let setting = sdb::setting(db, user.id).await?.unwrap_or(sdb::Setting {
        mode: Default::default(),
        changed_at: None,
        is_private: false,
    });
    println!(
        "{} ({}): similarity {}",
        user.username,
        if user.is_private { "private" } else { "public" },
        setting.mode.as_str()
    );
    let Some(version) = sdb::version(db).await? else {
        println!("no run stored yet");
        return Ok(());
    };
    println!(
        "run #{} finished {} ({})",
        version.run_id,
        version.finished_at,
        if version.stale(Utc::now()) {
            "stale"
        } else {
            "fresh"
        }
    );
    let recs = sdb::recommendations(db, user.id, version.started_at).await?;
    println!("\nrecommended artists (as served):");
    for (i, r) in recs.iter().take(limit).enumerate() {
        println!(
            "  {:>3}  {:<36} {:>7.3}  because of {} (+{} more)",
            i + 1,
            truncate(&r.name, 36),
            r.score as f64 / FULL as f64,
            r.because_name,
            r.because_count - 1
        );
    }
    if recs.is_empty() {
        println!("  (none)");
    }
    let neighbours = sdb::taste_neighbours(db, user.id, Some(user.id)).await?;
    println!("\ntaste neighbours (as the user sees them):");
    for (i, n) in neighbours.iter().take(limit).enumerate() {
        let shared: Vec<&str> = n.top_shared.iter().map(|(_, s)| s.as_str()).collect();
        println!(
            "  {:>3}  {:<24} {:>5.3}  {:>4} artists shared: {}",
            i + 1,
            truncate(&n.username, 24),
            n.score as f64 / FULL as f64,
            n.shared_artists,
            shared.join(", ")
        );
    }
    if neighbours.is_empty() {
        println!("  (none)");
    }
    Ok(())
}

/// A variant's results for comparison.
struct Variant {
    name: &'static str,
    lists: BTreeMap<i64, Vec<ArtistNeighbour>>,
    recs: BTreeMap<i64, Vec<Recommendation>>,
}

fn variant(name: &'static str, input: &Input, params: &SimilarityParams, factors: bool) -> Variant {
    let mut contributors = input.contributors.clone();
    if !factors {
        for c in &mut contributors {
            c.factor = FULL;
        }
    }
    let m = Matrix::new(&contributors, &input.credits, params);
    let lists = similarity::artist_neighbours(&m, params);
    let recs = similarity::recommend(&lists, &input.histories, &input.skips, params);
    Variant { name, lists, recs }
}

fn appearances(v: &Variant, artist: i64, top: usize) -> (usize, usize) {
    let lists = v
        .lists
        .values()
        .filter(|l| l.iter().take(top).any(|n| n.artist_id == artist))
        .count();
    let users = v
        .recs
        .values()
        .filter(|r| r.iter().take(top).any(|r| r.artist_id == artist))
        .count();
    (lists, users)
}

async fn report(db: &PgPool, params: &SimilarityParams, o: Options) -> anyhow::Result<()> {
    let top = o.limit.unwrap_or(20);
    let window = Window::new(Utc::now().date_naive(), params);
    let started = Instant::now();
    let (weighted, loaded) = sdb::load(db, &window, params, Source::Weights).await?;
    let (raw, _) = sdb::load(db, &window, params, Source::Plays).await?;
    println!(
        "window {} .. {}: {} accounts, {} credits, {} users served; loaded in {:.1} s",
        window.from,
        window.to,
        loaded.accounts,
        loaded.credits,
        loaded.served,
        started.elapsed().as_secs_f64()
    );
    let factored = weighted
        .contributors
        .iter()
        .filter(|c| c.factor < FULL)
        .count();
    let excluded = weighted
        .contributors
        .iter()
        .filter(|c| c.factor < params.min_factor)
        .count();
    println!(
        "coordination: {factored} accounts with a factor below 1, {excluded} of them under the \
         minimum ({:.2}) and left out",
        params.min_factor as f64 / FULL as f64
    );
    let matrix = Matrix::new(&weighted.contributors, &weighted.credits, params);
    let mut cohesion = matrix.cohesion_by_artist(params);
    cohesion.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut values: Vec<f64> = cohesion.iter().map(|(_, c)| *c).collect();
    values.sort_by(f64::total_cmp);
    let at = |q: f64| {
        values
            .get(((values.len() as f64 - 1.0) * q).round() as usize)
            .copied()
    };
    if let (Some(p50), Some(p90), Some(p99)) = (at(0.5), at(0.9), at(0.99)) {
        println!(
            "cohesion of {} artists' listeners (how alike their other listening is): p50 {p50:.3}, \
             p90 {p90:.3}, p99 {p99:.3}, max {:.3}; over the maximum ({:.2}): {}",
            values.len(),
            values.last().copied().unwrap_or(0.0),
            params.max_cohesion as f64 / FULL as f64,
            values
                .iter()
                .filter(|c| **c > params.max_cohesion as f64 / FULL as f64)
                .count()
        );
    }
    let variants = [
        variant("weights and factors", &weighted, params, true),
        variant("weights, no factors", &weighted, params, false),
        variant("every play", &raw, params, false),
    ];

    println!(
        "\nartists with neighbours, and how much the first {top} agree with the first variant:"
    );
    let base = &variants[0];
    for v in &variants {
        let mut overlap = 0.0;
        let mut compared = 0usize;
        for (artist, list) in &v.lists {
            let theirs: HashSet<i64> = list.iter().take(top).map(|n| n.artist_id).collect();
            let ours: HashSet<i64> = base
                .lists
                .get(artist)
                .map(|l| l.iter().take(top).map(|n| n.artist_id).collect())
                .unwrap_or_default();
            let union = theirs.union(&ours).count();
            if union > 0 {
                overlap += theirs.intersection(&ours).count() as f64 / union as f64;
                compared += 1;
            }
        }
        println!(
            "  {:<22} {:>6} artists, {:>7} users recommended, mean Jaccard {:.3}",
            v.name,
            v.lists.len(),
            v.recs.len(),
            if compared > 0 {
                overlap / compared as f64
            } else {
                0.0
            }
        );
    }

    let mut followed: Vec<i64> = Vec::new();
    for name in &o.artists {
        followed.push(artist_id(db, name).await?);
    }
    if let Some(run) = cdb::latest_run(db).await? {
        for c in cdb::cohorts(db, run.id).await? {
            if c.kind == Kind::Artist && !followed.contains(&c.entity_id) {
                followed.push(c.entity_id);
            }
        }
    }
    let mut ids = followed.clone();
    for v in &variants {
        for a in &followed {
            if let Some(list) = v.lists.get(a) {
                ids.extend(list.iter().take(5).map(|n| n.artist_id));
            }
        }
    }
    // The artists the raw plays put in the most lists, beyond the first
    // variant: what the weights and factors keep out.
    let mut promoted: Vec<(i64, i64)> = Vec::new();
    {
        let count = |v: &Variant| {
            let mut n: HashMap<i64, i64> = HashMap::new();
            for list in v.lists.values() {
                for nb in list.iter().take(top) {
                    *n.entry(nb.artist_id).or_default() += 1;
                }
            }
            n
        };
        let (filtered, every) = (count(base), count(&variants[2]));
        for (artist, n) in every {
            let gained = n - filtered.get(&artist).copied().unwrap_or(0);
            if gained > 0 {
                promoted.push((artist, gained));
            }
        }
        promoted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        promoted.truncate(o.limit.unwrap_or(10));
        ids.extend(promoted.iter().map(|(a, _)| *a));
    }
    ids.extend(cohesion.iter().take(10).map(|(a, _)| *a));
    let names = sdb::artist_names(db, &ids).await?;
    let name = |a: &i64| names.get(a).cloned().unwrap_or_else(|| format!("#{a}"));
    println!("\nthe most cohesive audiences:");
    for (a, c) in cohesion.iter().take(10) {
        println!(
            "  {:<40} {c:.3}  ({:.0} accounts)",
            truncate(&name(a), 40),
            matrix.audience(*a)
        );
    }

    for a in &followed {
        println!("\n{}:", name(a));
        for v in &variants {
            let (lists, users) = appearances(v, *a, top);
            let own: Vec<String> = v
                .lists
                .get(a)
                .map(|l| l.iter().take(5).map(|n| name(&n.artist_id)).collect())
                .unwrap_or_default();
            println!(
                "  {:<22} in {lists} artists' first {top}, recommended to {users} users; \
                 its own: {}",
                v.name,
                if own.is_empty() {
                    "(none)".to_string()
                } else {
                    own.join(", ")
                }
            );
        }
    }
    println!("\nin more artists' first {top} with every play than with the weights and factors:");
    for (a, gained) in &promoted {
        let (lists, users) = appearances(base, *a, top);
        let (raw_lists, raw_users) = appearances(&variants[2], *a, top);
        println!(
            "  {:<40} +{gained:<5} lists {raw_lists} -> {lists}, recommended to {raw_users} -> {users}",
            truncate(&name(a), 40)
        );
    }
    Ok(())
}

async fn evaluate(db: &PgPool, params: &SimilarityParams, o: Options) -> anyhow::Result<()> {
    let holdout = o.holdout_days.unwrap_or(14);
    let n = o.n.unwrap_or(20);
    let min_plays = o.min_plays.unwrap_or(2);
    if !(1..=90).contains(&holdout) {
        bail!("--holdout-days: 1 to 90");
    }
    let today = Utc::now().date_naive();
    let cutoff = today - TimeDelta::days(holdout);
    let window = Window::new(cutoff, params);
    let started = Instant::now();
    let (weighted, loaded) = sdb::load(db, &window, params, Source::Weights).await?;
    let (raw, _) = sdb::load(db, &window, params, Source::Plays).await?;
    let held = sdb::played_between(db, cutoff + TimeDelta::days(1), today, min_plays).await?;
    println!(
        "trained on {} .. {} ({} accounts, {} users served), held out {} .. {} (artists new to \
         a user, {min_plays}+ plays with some weight); loaded in {:.1} s",
        window.from,
        window.to,
        loaded.accounts,
        loaded.served,
        cutoff + TimeDelta::days(1),
        today,
        started.elapsed().as_secs_f64()
    );

    let matrix = Matrix::new(&weighted.contributors, &weighted.credits, params);
    let by_popularity = matrix.by_popularity();
    let rank: HashMap<i64, usize> = by_popularity
        .iter()
        .enumerate()
        .map(|(i, a)| (*a, i))
        .collect();
    let listened: HashMap<i64, HashSet<i64>> = weighted
        .histories
        .iter()
        .map(|h| (h.user_id, h.artists.iter().map(|(a, _)| *a).collect()))
        .collect();
    let targets = |user: i64| -> HashSet<i64> {
        let before = listened.get(&user);
        held.get(&user)
            .map(|t| {
                t.iter()
                    .filter(|a| before.is_none_or(|b| !b.contains(a)))
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    };
    let cases = |recs: &BTreeMap<i64, Vec<Recommendation>>| -> Vec<eval::Case> {
        weighted
            .histories
            .iter()
            .map(|h| eval::Case {
                recommended: recs
                    .get(&h.user_id)
                    .map(|r| r.iter().map(|r| r.artist_id).collect())
                    .unwrap_or_default(),
                targets: targets(h.user_id),
            })
            .collect()
    };
    let with_targets = weighted
        .histories
        .iter()
        .filter(|h| !targets(h.user_id).is_empty())
        .count();
    println!(
        "{with_targets} of {} users served found a new artist in the hold-out; scored at {n}:",
        weighted.histories.len()
    );
    println!(
        "  {:<26} {:>6} {:>9} {:>7} {:>7} {:>9} {:>11} {:>8}",
        "", "users", "hit rate", "recall", "ndcg", "coverage", "popularity", "targets"
    );
    let print = |name: &str, m: eval::Metrics| {
        println!(
            "  {:<26} {:>6} {:>9.3} {:>7.3} {:>7.3} {:>9} {:>11.1} {:>8.1}",
            name,
            m.users,
            m.hit_rate,
            m.recall,
            m.ndcg,
            m.coverage,
            m.popularity,
            m.target_popularity
        );
    };
    for (name, input) in [("weights and factors", &weighted), ("every play", &raw)] {
        let computed = Instant::now();
        let v = variant("", input, params, true);
        let m = eval::evaluate(&cases(&v.recs), n, &rank, by_popularity.len());
        print(name, m);
        tracing::debug!("{name}: {:.1} s", computed.elapsed().as_secs_f64());
    }
    let popular: BTreeMap<i64, Vec<Recommendation>> = weighted
        .histories
        .iter()
        .map(|h| {
            let none = HashSet::new();
            let seen = listened.get(&h.user_id).unwrap_or(&none);
            let recs = eval::most_popular(&by_popularity, seen, n)
                .into_iter()
                .map(|artist_id| Recommendation {
                    artist_id,
                    score: 0,
                    because: 0,
                    because_count: 0,
                })
                .collect();
            (h.user_id, recs)
        })
        .collect();
    print(
        "popularity",
        eval::evaluate(&cases(&popular), n, &rank, by_popularity.len()),
    );
    println!(
        "  (popularity: mean percentile of the recommended artists and of the targets, 0 for \
         the most listened to of {} artists)",
        by_popularity.len()
    );
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}
