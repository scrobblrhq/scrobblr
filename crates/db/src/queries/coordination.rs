//! Cross-account coordination (see `shared::coordination` for
//! the detection and `migrations/0025_coordination.sql` for the schema).
//!
//! A run loads the window's weights from `ranking_daily` (never the
//! scrobbles hypertable by artist, whose compressed chunks are segmented by
//! user), the sync window's plays in one pass over recent scrobbles, and
//! the accounts' registration days and origins; it stores the cohorts, the
//! flagged accounts, and the factors of their active days in
//! `coordination_days`, which rankings join to `ranking_daily`.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use futures_util::TryStreamExt;
use sqlx::PgPool;

use shared::coordination::{
    self as coordination, Account, CohortDetail, CoordinationParams, Detection, Evidence, Group,
    Input, Member, Mode, Play, Signal,
};
use shared::origin::Origin;
use shared::ranking::Kind;

/// Records where an account registered from.
pub async fn record_origin(
    pool: &PgPool,
    user_id: i64,
    origin: &Origin,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO registration_origins (user_id, key_id, address, network)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (user_id) DO NOTHING
        "#,
        user_id,
        origin.key_id,
        origin.address,
        origin.network,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Deletes origins recorded more than `keep_days` ago; returns how many.
pub async fn purge_origins(pool: &PgPool, keep_days: i64) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "DELETE FROM registration_origins WHERE created_at < NOW() - make_interval(days => $1)",
        keep_days as i32,
    )
    .execute(pool)
    .await?
    .rows_affected())
}

/// The UTC days a run reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Groups are made over `[from, to]`...
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// ...and a member is new to an entity it weighed nothing for over the
    /// same number of days before.
    pub baseline_from: NaiveDate,
    /// Plays are read from `sync_from` to the end of `to`.
    pub sync_from: NaiveDate,
}

impl Window {
    pub fn new(today: NaiveDate, params: &CoordinationParams) -> Self {
        let from = today - TimeDelta::days(params.window_days - 1);
        Self {
            from,
            to: today,
            baseline_from: from - TimeDelta::days(params.window_days),
            sync_from: today - TimeDelta::days(params.sync_window_days - 1),
        }
    }
}

fn day_start(day: NaiveDate) -> DateTime<Utc> {
    day.and_time(NaiveTime::MIN).and_utc()
}

/// What [`load`] read, for the run's log line.
#[derive(Debug, Default, Clone, Copy)]
pub struct Loaded {
    pub pairs: usize,
    pub plays: usize,
}

/// Everything detection needs for `window`.
pub async fn load(pool: &PgPool, window: &Window) -> Result<(Input, Loaded), sqlx::Error> {
    let mut loaded = Loaded::default();
    // (user, track) -> (artist, weight) over the window.
    let mut pairs: HashMap<(i64, i64), (i64, i64)> = HashMap::new();
    let mut rows = sqlx::query!(
        r#"
        SELECT user_id, track_id, artist_id, sum(weight)::bigint AS "weight!"
        FROM ranking_daily
        WHERE day BETWEEN $1 AND $2
        GROUP BY user_id, track_id, artist_id
        HAVING sum(weight) > 0
        "#,
        window.from,
        window.to,
    )
    .fetch(pool);
    while let Some(r) = rows.try_next().await? {
        pairs.insert((r.user_id, r.track_id), (r.artist_id, r.weight));
    }
    drop(rows);
    loaded.pairs = pairs.len();

    let active: HashSet<i64> = pairs.keys().map(|(u, _)| *u).collect();
    let mut known_tracks: HashSet<(i64, i64)> = HashSet::new();
    let mut known_artists: HashSet<(i64, i64)> = HashSet::new();
    let mut rows = sqlx::query!(
        r#"
        SELECT DISTINCT user_id, track_id, artist_id
        FROM ranking_daily
        WHERE day BETWEEN $1 AND $2 AND weight > 0
        "#,
        window.baseline_from,
        window.from - TimeDelta::days(1),
    )
    .fetch(pool);
    while let Some(r) = rows.try_next().await? {
        if active.contains(&r.user_id) {
            known_tracks.insert((r.user_id, r.track_id));
            known_artists.insert((r.user_id, r.artist_id));
        }
    }
    drop(rows);

    let mut per_artist: BTreeMap<i64, BTreeMap<i64, i64>> = BTreeMap::new();
    let mut per_track: BTreeMap<i64, Vec<Member>> = BTreeMap::new();
    let mut per_user: HashMap<i64, HashMap<i64, i64>> = HashMap::new();
    for ((user, track), (artist, weight)) in &pairs {
        *per_artist
            .entry(*artist)
            .or_default()
            .entry(*user)
            .or_default() += weight;
        per_track.entry(*track).or_default().push(Member {
            user_id: *user,
            weight: *weight,
            new: !known_tracks.contains(&(*user, *track)),
        });
        *per_user
            .entry(*user)
            .or_default()
            .entry(*artist)
            .or_default() += weight;
    }
    drop(pairs);
    let mut groups: Vec<Group> = Vec::with_capacity(per_artist.len() + per_track.len());
    for (artist, members) in per_artist {
        groups.push(Group {
            kind: Kind::Artist,
            entity_id: artist,
            members: members
                .into_iter()
                .map(|(user, weight)| Member {
                    user_id: user,
                    weight,
                    new: !known_artists.contains(&(user, artist)),
                })
                .collect(),
        });
    }
    groups.extend(per_track.into_iter().map(|(track, members)| Group {
        kind: Kind::Track,
        entity_id: track,
        members,
    }));

    let ids: Vec<i64> = active.iter().copied().collect();
    let users = sqlx::query!(
        r#"
        SELECT u.id, (u.created_at AT TIME ZONE 'UTC')::date AS "registered!",
               o.key_id AS "key_id?", o.address AS "address?", o.network AS "network?"
        FROM users u
        LEFT JOIN registration_origins o ON o.user_id = u.id
        WHERE u.id = ANY($1)
        "#,
        &ids,
    )
    .fetch_all(pool)
    .await?;
    let accounts: Vec<Account> = users
        .into_iter()
        .map(|u| {
            let mut weights: Vec<i64> = per_user
                .get(&u.id)
                .map(|m| m.values().copied().collect())
                .unwrap_or_default();
            weights.sort_unstable_by(|a, b| b.cmp(a));
            Account {
                user_id: u.id,
                registered: u.registered,
                origin: match (u.key_id, u.address, u.network) {
                    (Some(key_id), Some(address), Some(network)) => Some(Origin {
                        key_id,
                        address,
                        network,
                    }),
                    _ => None,
                },
                weight: weights.iter().sum(),
                top3_weight: weights.iter().take(3).sum(),
            }
        })
        .collect();

    let mut clients: Vec<String> = Vec::new();
    let mut family_of: HashMap<i32, u32> = HashMap::new();
    let mut families: HashMap<String, u32> = HashMap::new();
    for c in sqlx::query!("SELECT id, protocol, name FROM scrobble_clients ORDER BY id")
        .fetch_all(pool)
        .await?
    {
        let family = coordination::client_family(&c.protocol, &c.name);
        let index = *families.entry(family.clone()).or_insert_with(|| {
            clients.push(family);
            clients.len() as u32 - 1
        });
        family_of.insert(c.id, index);
    }

    let mut plays = Vec::new();
    let mut rows = sqlx::query!(
        r#"
        SELECT user_id, track_id, artist_id, played_at, client_id
        FROM scrobbles
        WHERE played_at >= $1 AND played_at < $2 AND import_id IS NULL
        "#,
        day_start(window.sync_from),
        day_start(window.to) + TimeDelta::days(1),
    )
    .fetch(pool);
    while let Some(r) = rows.try_next().await? {
        if active.contains(&r.user_id) {
            plays.push(Play {
                user_id: r.user_id,
                track_id: r.track_id,
                artist_id: r.artist_id,
                at: r.played_at.timestamp(),
                client: r.client_id.and_then(|id| family_of.get(&id).copied()),
            });
        }
    }
    drop(rows);
    loaded.plays = plays.len();

    Ok((
        Input {
            accounts,
            groups,
            plays,
            clients,
            sync_from: Some(window.sync_from),
        },
        loaded,
    ))
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: i32,
    pub fingerprint: String,
    pub mode: String,
    pub window_from: NaiveDate,
    pub window_to: NaiveDate,
    pub accounts: i32,
    pub groups: i32,
    pub flagged: i32,
    pub cohorts: i32,
    pub too_small: bool,
    pub started_at: DateTime<Utc>,
    pub took_ms: i32,
    pub finished_at: DateTime<Utc>,
}

/// Stores a run and, in `apply` mode, replaces the factors of the window's
/// days with its own; in `report` mode, removes every stored factor. One
/// transaction, so rankings see either the previous factors or these.
pub async fn store(
    pool: &PgPool,
    params: &CoordinationParams,
    window: &Window,
    detection: &Detection,
    started_at: DateTime<Utc>,
    took_ms: i32,
) -> Result<i32, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('coordination', 0))")
        .execute(&mut *tx)
        .await?;
    let run_id = sqlx::query_scalar!(
        r#"
        INSERT INTO coordination_runs
            (fingerprint, mode, window_from, window_to, accounts, groups, flagged, cohorts,
             too_small, started_at, took_ms)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        RETURNING id
        "#,
        params.fingerprint(),
        params.mode.as_str(),
        window.from,
        window.to,
        detection.active as i32,
        detection.groups as i32,
        detection.accounts.len() as i32,
        detection.cohorts.len() as i32,
        detection.too_small,
        started_at,
        took_ms,
    )
    .fetch_one(&mut *tx)
    .await?;

    let signal = |e: &Evidence, s: Signal| e.get(s) as i32;
    let c = &detection.cohorts;
    let column = |s: Signal| -> Vec<i32> { c.iter().map(|c| signal(&c.evidence, s)).collect() };
    let details: Vec<sqlx::types::JsonValue> = c.iter().map(|c| detail_json(&c.detail)).collect();
    sqlx::query!(
        r#"
        INSERT INTO coordination_cohorts
            (run_id, cohort, kind, entity_id, members, score, factor,
             age, origin, client, sync, shape, burst, detail)
        SELECT $1, t.*
        FROM UNNEST($2::int[], $3::text[], $4::bigint[], $5::int[], $6::int[], $7::int[],
                    $8::int[], $9::int[], $10::int[], $11::int[], $12::int[], $13::int[],
                    $14::jsonb[]) AS t
        "#,
        run_id,
        &(0..c.len() as i32).collect::<Vec<_>>(),
        &c.iter()
            .map(|c| c.kind.as_str().to_string())
            .collect::<Vec<_>>(),
        &c.iter().map(|c| c.entity_id).collect::<Vec<_>>(),
        &c.iter().map(|c| c.members as i32).collect::<Vec<_>>(),
        &c.iter().map(|c| c.score as i32).collect::<Vec<_>>(),
        &c.iter().map(|c| c.factor as i32).collect::<Vec<_>>(),
        &column(Signal::Age),
        &column(Signal::Origin),
        &column(Signal::Client),
        &column(Signal::Sync),
        &column(Signal::Shape),
        &column(Signal::Burst),
        &details,
    )
    .execute(&mut *tx)
    .await?;

    let a = &detection.accounts;
    let column = |s: Signal| -> Vec<i32> { a.iter().map(|a| signal(&a.evidence, s)).collect() };
    let users: Vec<i64> = a.iter().map(|a| a.user_id).collect();
    let factors: Vec<i32> = a.iter().map(|a| a.factor as i32).collect();
    sqlx::query!(
        r#"
        INSERT INTO coordination_accounts
            (run_id, user_id, cohort, suspicion, factor, age, origin, client, sync, shape, burst)
        SELECT $1, t.*
        FROM UNNEST($2::bigint[], $3::int[], $4::int[], $5::int[], $6::int[], $7::int[],
                    $8::int[], $9::int[], $10::int[], $11::int[]) AS t
        "#,
        run_id,
        &users,
        &a.iter().map(|a| a.cohort as i32).collect::<Vec<_>>(),
        &a.iter().map(|a| a.suspicion as i32).collect::<Vec<_>>(),
        &factors,
        &column(Signal::Age),
        &column(Signal::Origin),
        &column(Signal::Client),
        &column(Signal::Sync),
        &column(Signal::Shape),
        &column(Signal::Burst),
    )
    .execute(&mut *tx)
    .await?;

    // Album votes weigh by their account's lowest factor: an account whose
    // factor this run changes has its tracks decided again.
    let previous: HashMap<i64, i32> = sqlx::query!(
        r#"
        SELECT user_id, min(factor) AS "factor!" FROM coordination_days
        WHERE ($1 OR day BETWEEN $2 AND $3) GROUP BY user_id
        "#,
        params.mode != Mode::Apply,
        window.from,
        window.to,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| (r.user_id, r.factor))
    .collect();
    let current: HashMap<i64, i32> = match params.mode {
        Mode::Apply => users.iter().copied().zip(factors.iter().copied()).collect(),
        Mode::Report | Mode::Off => HashMap::new(),
    };
    let changed: Vec<i64> = previous
        .keys()
        .chain(current.keys())
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|user| previous.get(user) != current.get(user))
        .collect();
    crate::queries::albums::enqueue_voted(&mut *tx, &changed).await?;

    match params.mode {
        Mode::Apply => {
            sqlx::query!(
                "DELETE FROM coordination_days WHERE day BETWEEN $1 AND $2",
                window.from,
                window.to,
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                r#"
                INSERT INTO coordination_days (user_id, day, factor, run_id)
                SELECT d.user_id, d.day, f.factor, $5
                FROM ranking_days d
                JOIN UNNEST($1::bigint[], $2::int[]) AS f(user_id, factor) USING (user_id)
                WHERE d.day BETWEEN $3 AND $4
                "#,
                &users,
                &factors,
                window.from,
                window.to,
                run_id,
            )
            .execute(&mut *tx)
            .await?;
        }
        Mode::Report | Mode::Off => {
            sqlx::query!("DELETE FROM coordination_days")
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(run_id)
}

/// Removes every stored factor (coordination turned off), queuing the
/// tracks their accounts voted on; returns how many (user, day) rows held
/// one.
pub async fn clear_factors(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut users = sqlx::query_scalar!("DELETE FROM coordination_days RETURNING user_id")
        .fetch_all(&mut *tx)
        .await?;
    let rows = users.len() as u64;
    users.sort_unstable();
    users.dedup();
    crate::queries::albums::enqueue_voted(&mut *tx, &users).await?;
    tx.commit().await?;
    Ok(rows)
}

/// Deletes runs finished more than `keep_days` ago, with their cohorts and
/// accounts, except the latest.
pub async fn purge_runs(pool: &PgPool, keep_days: i64) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        r#"
        DELETE FROM coordination_runs
        WHERE finished_at < NOW() - make_interval(days => $1)
          AND id <> (SELECT max(id) FROM coordination_runs)
        "#,
        keep_days as i32,
    )
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn latest_run(pool: &PgPool) -> Result<Option<Run>, sqlx::Error> {
    sqlx::query_as!(
        Run,
        r#"
        SELECT id, fingerprint, mode, window_from, window_to, accounts, groups, flagged,
               cohorts, too_small, started_at, took_ms, finished_at
        FROM coordination_runs
        ORDER BY id DESC
        LIMIT 1
        "#
    )
    .fetch_optional(pool)
    .await
}

/// A stored cohort, with its entity's name.
#[derive(Debug, Clone)]
pub struct StoredCohort {
    pub cohort: i32,
    pub kind: Kind,
    pub entity_id: i64,
    pub name: Option<String>,
    pub artist_name: Option<String>,
    pub members: i32,
    pub score: i32,
    pub factor: i32,
    pub evidence: Evidence,
    pub detail: sqlx::types::JsonValue,
}

fn evidence(values: [i32; 6]) -> Evidence {
    Evidence(values.map(i64::from))
}

pub async fn cohorts(pool: &PgPool, run_id: i32) -> Result<Vec<StoredCohort>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT c.cohort, c.kind, c.entity_id, c.members, c.score, c.factor,
               c.age, c.origin, c.client, c.sync, c.shape, c.burst, c.detail,
               COALESCE(t.title, a.name) AS "name?", ta.name AS "artist_name?"
        FROM coordination_cohorts c
        LEFT JOIN tracks t   ON c.kind = 'track' AND t.id = c.entity_id
        LEFT JOIN artists ta ON ta.id = t.artist_id
        LEFT JOIN artists a  ON c.kind = 'artist' AND a.id = c.entity_id
        WHERE c.run_id = $1
        ORDER BY c.cohort
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            Some(StoredCohort {
                cohort: r.cohort,
                kind: Kind::parse(&r.kind)?,
                entity_id: r.entity_id,
                name: r.name,
                artist_name: r.artist_name,
                members: r.members,
                score: r.score,
                factor: r.factor,
                evidence: evidence([r.age, r.origin, r.client, r.sync, r.shape, r.burst]),
                detail: r.detail,
            })
        })
        .collect())
}

/// A flagged account of a stored run.
#[derive(Debug, Clone)]
pub struct StoredAccount {
    pub user_id: i64,
    pub username: String,
    pub registered: NaiveDate,
    pub cohort: i32,
    pub suspicion: i32,
    pub factor: i32,
    pub evidence: Evidence,
}

pub async fn accounts(pool: &PgPool, run_id: i32) -> Result<Vec<StoredAccount>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT a.user_id, u.username, (u.created_at AT TIME ZONE 'UTC')::date AS "registered!",
               a.cohort, a.suspicion, a.factor,
               a.age, a.origin, a.client, a.sync, a.shape, a.burst
        FROM coordination_accounts a
        JOIN users u ON u.id = a.user_id
        WHERE a.run_id = $1
        ORDER BY a.cohort, a.suspicion DESC, a.user_id
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| StoredAccount {
            user_id: r.user_id,
            username: r.username,
            registered: r.registered,
            cohort: r.cohort,
            suspicion: r.suspicion,
            factor: r.factor,
            evidence: evidence([r.age, r.origin, r.client, r.sync, r.shape, r.burst]),
        })
        .collect())
}

/// Names for the entities `detail.targets` refer to, by kind and id.
pub async fn entity_names(
    pool: &PgPool,
    entities: &[(Kind, i64)],
) -> Result<HashMap<(Kind, i64), String>, sqlx::Error> {
    let ids = |kind: Kind| -> Vec<i64> {
        entities
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, id)| *id)
            .collect()
    };
    let mut names = HashMap::new();
    for r in sqlx::query!(
        "SELECT id, name FROM artists WHERE id = ANY($1)",
        &ids(Kind::Artist)
    )
    .fetch_all(pool)
    .await?
    {
        names.insert((Kind::Artist, r.id), r.name);
    }
    for r in sqlx::query!(
        r#"
        SELECT t.id, a.name || ' — ' || t.title AS "name!"
        FROM tracks t JOIN artists a ON a.id = t.artist_id
        WHERE t.id = ANY($1)
        "#,
        &ids(Kind::Track)
    )
    .fetch_all(pool)
    .await?
    {
        names.insert((Kind::Track, r.id), r.name);
    }
    Ok(names)
}

/// A cohort's detail as [`cohorts`] reads it back, for dry runs.
pub fn detail_json(detail: &CohortDetail) -> sqlx::types::JsonValue {
    serde_json::to_value(detail).unwrap_or_default()
}

/// Usernames and registration days, for dry runs.
pub async fn usernames(
    pool: &PgPool,
    ids: &[i64],
) -> Result<HashMap<i64, (String, NaiveDate)>, sqlx::Error> {
    Ok(sqlx::query!(
        r#"
        SELECT id, username, (created_at AT TIME ZONE 'UTC')::date AS "registered!"
        FROM users WHERE id = ANY($1)
        "#,
        ids,
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| (r.id, (r.username, r.registered)))
    .collect())
}
