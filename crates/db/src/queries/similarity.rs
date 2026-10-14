//! Artist similarity, recommendations and taste neighbours (see
//! `shared::similarity` for the computation and
//! `migrations/0033_similarity.sql` for the schema).
//!
//! A run reads the window's weights from `ranking_daily` (never the
//! scrobbles hypertable by artist), each account's coordination factor for
//! the window, and the whole history of the users it serves from the daily
//! aggregate, imports included; it replaces every stored result in one
//! transaction. Readers filter what changed since: blocks, privacy, the
//! setting, skips and plays.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use futures_util::TryStreamExt;
use sqlx::{PgPool, Postgres, Transaction};

use shared::similarity::{
    Contributor, Credit, FULL, History, Input, Output, SimilarityMode, SimilarityParams,
};

/// Rows a statement inserts at most.
const BATCH: usize = 20_000;
/// Runs kept for review.
const RUNS_KEPT_DAYS: i32 = 30;
/// Skips kept per account; older ones go first.
pub const MAX_SKIPS: i64 = 1000;
/// A recommendation played since the run read the history is left out;
/// plays this much older than the run arriving late aren't looked for.
pub const LATE_PLAYS: TimeDelta = TimeDelta::days(14);
/// How long past its next run's due time a run is still fresh.
pub const STALE_GRACE: TimeDelta = TimeDelta::hours(1);

fn day_start(day: NaiveDate) -> DateTime<Utc> {
    day.and_time(NaiveTime::MIN).and_utc()
}

/// The UTC days of weights a run reads, both included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

impl Window {
    pub fn new(to: NaiveDate, params: &SimilarityParams) -> Self {
        Self {
            from: to - TimeDelta::days(params.window_days - 1),
            to,
        }
    }
}

/// What credits are made of: the ranking weights with coordination factors
/// (what runs use), or every play at full weight, for comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Weights,
    Plays,
}

/// What [`load`] read.
#[derive(Debug, Default, Clone, Copy)]
pub struct Loaded {
    pub accounts: usize,
    pub credits: usize,
    pub history_rows: usize,
    pub served: usize,
}

struct Played {
    artists: Vec<(i64, f64)>,
    last: DateTime<Utc>,
}

/// The run's input: accounts that may count (not off, not waiting to be
/// erased) with their factor, their credits over `window`, and the history
/// up to its last day of the ones among them who played in it.
pub async fn load(
    pool: &PgPool,
    window: &Window,
    params: &SimilarityParams,
    source: Source,
) -> Result<(Input, Loaded), sqlx::Error> {
    let mut loaded = Loaded::default();
    let accounts = sqlx::query!(
        r#"
        WITH totals AS (
            SELECT user_id, sum(weight)::bigint AS weight FROM ranking_days
            WHERE day BETWEEN $1 AND $2 GROUP BY 1
        ), factors AS (
            SELECT c.user_id,
                   ($3::bigint - sum(r.weight::bigint * ($3::bigint - c.factor))::bigint
                                 / NULLIF(t.weight, 0))::bigint AS factor
            FROM coordination_days c
            JOIN ranking_days r ON r.user_id = c.user_id AND r.day = c.day
            JOIN totals t ON t.user_id = c.user_id
            WHERE c.day BETWEEN $1 AND $2
            GROUP BY c.user_id, t.weight
        )
        SELECT u.id, (u.similarity = 'listed' AND NOT u.is_private) AS "listed!",
               COALESCE(f.factor, $3) AS "factor!"
        FROM users u
        LEFT JOIN factors f ON f.user_id = u.id
        WHERE u.similarity <> 'off' AND u.delete_after IS NULL
        "#,
        window.from,
        window.to,
        FULL,
    )
    .fetch_all(pool)
    .await?;
    let contributors: Vec<Contributor> = accounts
        .into_iter()
        .map(|a| Contributor {
            user_id: a.id,
            factor: match source {
                Source::Weights => a.factor.clamp(0, FULL),
                Source::Plays => FULL,
            },
            listed: a.listed,
        })
        .collect();
    loaded.accounts = contributors.len();
    let known: HashSet<i64> = contributors.iter().map(|c| c.user_id).collect();

    let mut credits = Vec::new();
    let mut rows = sqlx::query!(
        r#"
        SELECT user_id, artist_id, sum(weight)::bigint AS "weight!", sum(plays)::bigint AS "plays!"
        FROM ranking_daily
        WHERE day BETWEEN $1 AND $2
        GROUP BY 1, 2
        "#,
        window.from,
        window.to,
    )
    .fetch(pool);
    while let Some(r) = rows.try_next().await? {
        if !known.contains(&r.user_id) {
            continue;
        }
        let weight = match source {
            Source::Weights => r.weight,
            Source::Plays => r.plays * FULL,
        };
        if weight > 0 {
            credits.push(Credit {
                user_id: r.user_id,
                artist_id: r.artist_id,
                weight,
            });
        }
    }
    drop(rows);
    loaded.credits = credits.len();

    let end = day_start(window.to) + TimeDelta::days(1);
    let mut per_user: HashMap<i64, Played> = HashMap::new();
    let mut rows = sqlx::query!(
        r#"
        SELECT user_id AS "user_id!", artist_id AS "artist_id!",
               sum(play_count * power(0.5, extract(epoch FROM $1::timestamptz - day)
                                           / 86400.0 / $2::float8))::float8 AS "plays!",
               max(day) AS "last!"
        FROM scrobbles_daily_by_artist
        WHERE day < $1
        GROUP BY 1, 2
        "#,
        end,
        params.half_life_days as f64,
    )
    .fetch(pool);
    while let Some(r) = rows.try_next().await? {
        loaded.history_rows += 1;
        if !known.contains(&r.user_id) {
            continue;
        }
        let entry = per_user.entry(r.user_id).or_insert_with(|| Played {
            artists: Vec::new(),
            last: r.last,
        });
        entry.artists.push((r.artist_id, r.plays));
        entry.last = entry.last.max(r.last);
    }
    drop(rows);
    let first = day_start(window.from);
    let mut histories: Vec<History> = per_user
        .into_iter()
        .filter(|(_, played)| played.last >= first)
        .map(|(user_id, played)| History {
            user_id,
            artists: played.artists,
        })
        .collect();
    histories.sort_by_key(|h| h.user_id);
    loaded.served = histories.len();

    let skips =
        sqlx::query!("SELECT user_id, artist_id FROM recommendation_skips WHERE until > NOW()")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|r| (r.user_id, r.artist_id))
            .collect();

    Ok((
        Input {
            contributors,
            credits,
            histories,
            skips,
        },
        loaded,
    ))
}

/// The artists each user played between `from` and `to` with at least
/// `min_plays` plays, some weight among them: what an offline evaluation
/// checks recommendations against.
pub async fn played_between(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    min_plays: i64,
) -> Result<HashMap<i64, HashSet<i64>>, sqlx::Error> {
    let mut played: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut rows = sqlx::query!(
        r#"
        SELECT user_id, artist_id
        FROM ranking_daily
        WHERE day BETWEEN $1 AND $2
        GROUP BY 1, 2
        HAVING sum(weight) > 0 AND sum(plays) >= $3
        "#,
        from,
        to,
        min_plays,
    )
    .fetch(pool);
    while let Some(r) = rows.try_next().await? {
        played.entry(r.user_id).or_default().insert(r.artist_id);
    }
    Ok(played)
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: i32,
    pub fingerprint: String,
    pub window_from: NaiveDate,
    pub window_to: NaiveDate,
    pub coordination_run_id: Option<i32>,
    pub accounts: i32,
    pub candidates: i32,
    pub artists: i32,
    pub neighbours: i32,
    pub recommended_users: i32,
    pub neighboured_users: i32,
    pub refresh_secs: i32,
    pub invalidated: bool,
    pub started_at: DateTime<Utc>,
    pub took_ms: i32,
    pub finished_at: DateTime<Utc>,
}

pub async fn latest_run(pool: &PgPool) -> Result<Option<Run>, sqlx::Error> {
    sqlx::query_as!(
        Run,
        r#"
        SELECT id, fingerprint, window_from, window_to, coordination_run_id, accounts, candidates,
               artists, neighbours, recommended_users, neighboured_users, refresh_secs,
               invalidated, started_at, took_ms, finished_at
        FROM similarity_runs
        ORDER BY id DESC
        LIMIT 1
        "#
    )
    .fetch_optional(pool)
    .await
}

/// Inserts rows from parallel arrays, a batch at a time.
macro_rules! in_batches {
    ($n:expr, |$range:ident| $insert:expr) => {{
        let mut start = 0;
        while start < $n {
            let $range = start..($n).min(start + BATCH);
            $insert;
            start += BATCH;
        }
    }};
}

/// Stores a run, replacing every result, in one transaction: readers see
/// the previous results or these. Accounts that opted out or asked to be
/// deleted since the run read its input are left out, and neighbours no
/// longer listed.
pub async fn store(
    pool: &PgPool,
    params: &SimilarityParams,
    window: &Window,
    output: &Output,
    coordination_run_id: Option<i32>,
    started_at: DateTime<Utc>,
    took_ms: i32,
) -> Result<i32, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '10min'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('similarity', 0))")
        .execute(&mut *tx)
        .await?;
    let pairs: usize = output.artists.values().map(Vec::len).sum();
    let run_id = sqlx::query_scalar!(
        r#"
        INSERT INTO similarity_runs
            (fingerprint, window_from, window_to, coordination_run_id, accounts, candidates,
             artists, neighbours, recommended_users, neighboured_users, refresh_secs,
             started_at, took_ms)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        RETURNING id
        "#,
        params.fingerprint(),
        window.from,
        window.to,
        coordination_run_id,
        output.contributors as i32,
        output.candidates as i32,
        output.artists.len() as i32,
        pairs as i32,
        output.recommendations.len() as i32,
        output.neighbours.len() as i32,
        (params.refresh_hours * 3600) as i32,
        started_at,
        took_ms,
    )
    .fetch_one(&mut *tx)
    .await?;

    store_artists(&mut tx, output).await?;
    store_recommendations(&mut tx, output).await?;
    store_neighbours(&mut tx, output).await?;
    sqlx::query!(
        r#"
        DELETE FROM similarity_runs
        WHERE finished_at < NOW() - make_interval(days => $1) AND id <> $2
        "#,
        RUNS_KEPT_DAYS,
        run_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(run_id)
}

async fn store_artists(
    tx: &mut Transaction<'_, Postgres>,
    output: &Output,
) -> Result<(), sqlx::Error> {
    sqlx::query!("DELETE FROM artist_neighbours")
        .execute(&mut **tx)
        .await?;
    let (mut artists, mut positions, mut neighbours, mut scores, mut listeners) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (artist, list) in &output.artists {
        for (i, n) in list.iter().enumerate() {
            artists.push(*artist);
            positions.push(i as i16 + 1);
            neighbours.push(n.artist_id);
            scores.push(n.score as i32);
            listeners.push(n.listeners as i32);
        }
    }
    in_batches!(artists.len(), |r| sqlx::query!(
        r#"
        INSERT INTO artist_neighbours (artist_id, position, neighbour_id, score, listeners)
        SELECT * FROM UNNEST($1::bigint[], $2::smallint[], $3::bigint[], $4::int[], $5::int[])
        "#,
        &artists[r.clone()],
        &positions[r.clone()],
        &neighbours[r.clone()],
        &scores[r.clone()],
        &listeners[r],
    )
    .execute(&mut **tx)
    .await?);
    Ok(())
}

async fn store_recommendations(
    tx: &mut Transaction<'_, Postgres>,
    output: &Output,
) -> Result<(), sqlx::Error> {
    sqlx::query!("DELETE FROM user_recommendations")
        .execute(&mut **tx)
        .await?;
    let (mut users, mut positions, mut artists, mut scores, mut because, mut counts) = (
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    for (user, list) in &output.recommendations {
        for (i, r) in list.iter().enumerate() {
            users.push(*user);
            positions.push(i as i32);
            artists.push(r.artist_id);
            scores.push(r.score.min(i32::MAX as i64) as i32);
            because.push(r.because);
            counts.push(r.because_count as i32);
        }
    }
    // A user's entries make one row: a batch ends between two users.
    let mut start = 0;
    while start < users.len() {
        let mut end = (start + BATCH).min(users.len());
        while end < users.len() && users[end] == users[end - 1] {
            end += 1;
        }
        let r = start..end;
        sqlx::query!(
            r#"
            INSERT INTO user_recommendations
                (user_id, artist_ids, scores, because_artist_ids, because_counts)
            SELECT t.user_id, array_agg(t.artist_id ORDER BY t.position),
                   array_agg(t.score ORDER BY t.position),
                   array_agg(t.because_artist_id ORDER BY t.position),
                   array_agg(t.because_count ORDER BY t.position)
            FROM UNNEST($1::bigint[], $2::int[], $3::bigint[], $4::int[], $5::bigint[], $6::int[])
                 AS t(user_id, position, artist_id, score, because_artist_id, because_count)
            JOIN users u ON u.id = t.user_id
            WHERE u.similarity <> 'off' AND u.delete_after IS NULL
            GROUP BY t.user_id
            "#,
            &users[r.clone()],
            &positions[r.clone()],
            &artists[r.clone()],
            &scores[r.clone()],
            &because[r.clone()],
            &counts[r],
        )
        .execute(&mut **tx)
        .await?;
        start = end;
    }
    Ok(())
}

async fn store_neighbours(
    tx: &mut Transaction<'_, Postgres>,
    output: &Output,
) -> Result<(), sqlx::Error> {
    sqlx::query!("DELETE FROM user_neighbours")
        .execute(&mut **tx)
        .await?;
    let mut users = Vec::new();
    let mut positions = Vec::new();
    let mut neighbours = Vec::new();
    let mut scores = Vec::new();
    let mut shared = Vec::new();
    let mut top: [Vec<Option<i64>>; 3] = Default::default();
    for (user, list) in &output.neighbours {
        for (i, n) in list.iter().enumerate() {
            users.push(*user);
            positions.push(i as i16 + 1);
            neighbours.push(n.user_id);
            scores.push(n.score as i32);
            shared.push(n.shared as i32);
            for (k, column) in top.iter_mut().enumerate() {
                column.push(n.top_shared.get(k).copied());
            }
        }
    }
    in_batches!(users.len(), |r| sqlx::query!(
        r#"
        INSERT INTO user_neighbours
            (user_id, position, neighbour_user_id, score, shared_artists, top_shared)
        SELECT t.user_id, t.position, t.neighbour_user_id, t.score, t.shared_artists,
               array_remove(ARRAY[t.s1, t.s2, t.s3], NULL)
        FROM UNNEST($1::bigint[], $2::smallint[], $3::bigint[], $4::int[], $5::int[],
                    $6::bigint[], $7::bigint[], $8::bigint[])
             AS t(user_id, position, neighbour_user_id, score, shared_artists, s1, s2, s3)
        JOIN users u ON u.id = t.user_id
        JOIN users n ON n.id = t.neighbour_user_id
        WHERE u.similarity <> 'off' AND u.delete_after IS NULL
          AND n.similarity = 'listed' AND n.delete_after IS NULL
        "#,
        &users[r.clone()],
        &positions[r.clone()],
        &neighbours[r.clone()],
        &scores[r.clone()],
        &shared[r.clone()],
        &top[0][r.clone()] as &[Option<i64>],
        &top[1][r.clone()] as &[Option<i64>],
        &top[2][r] as &[Option<i64>],
    )
    .execute(&mut **tx)
    .await?);
    Ok(())
}

/// Which results a route serves, and how fresh they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub run_id: i32,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub refresh_secs: i32,
}

impl Version {
    /// Due again `refresh_secs` after it finished; late an hour after that.
    pub fn stale(&self, now: DateTime<Utc>) -> bool {
        now > self.finished_at + TimeDelta::seconds(i64::from(self.refresh_secs)) + STALE_GRACE
    }
}

pub async fn version(pool: &PgPool) -> Result<Option<Version>, sqlx::Error> {
    sqlx::query_as!(
        Version,
        r#"
        SELECT id AS run_id, started_at, finished_at, refresh_secs
        FROM similarity_runs ORDER BY id DESC LIMIT 1
        "#
    )
    .fetch_optional(pool)
    .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeighbourEntry {
    pub position: i32,
    pub artist_id: i64,
    pub name: String,
    pub image_url: Option<String>,
    /// Thousandths.
    pub score: i64,
    pub listeners: i64,
}

#[derive(Debug, Clone)]
pub struct ArtistPage {
    pub version: Version,
    /// Neighbours the artist has.
    pub total: i64,
    pub entries: Vec<NeighbourEntry>,
}

/// Positions after `offset` of an artist's neighbours, with the run they
/// come from, in one statement; `None` before the first run.
pub async fn artist_page(
    pool: &PgPool,
    artist_id: i64,
    offset: i64,
    limit: i64,
) -> Result<Option<ArtistPage>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT r.id AS run_id, r.started_at, r.finished_at, r.refresh_secs, c.total AS "total!",
               e.position AS "position?", e.neighbour_id AS "neighbour_id?", e.name AS "name?",
               e.image_url AS "image_url?", e.score AS "score?", e.listeners AS "listeners?"
        FROM (SELECT * FROM similarity_runs ORDER BY id DESC LIMIT 1) r
        CROSS JOIN LATERAL (
            SELECT count(*) AS total FROM artist_neighbours WHERE artist_id = $1
        ) c
        LEFT JOIN LATERAL (
            SELECT n.position, n.neighbour_id, a.name, a.image_url, n.score, n.listeners
            FROM artist_neighbours n
            JOIN artists a ON a.id = n.neighbour_id
            WHERE n.artist_id = $1 AND n.position > $2
            ORDER BY n.position
            LIMIT $3
        ) e ON true
        ORDER BY e.position
        "#,
        artist_id,
        offset.clamp(0, i16::MAX as i64) as i16,
        limit,
    )
    .fetch_all(pool)
    .await?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let version = Version {
        run_id: first.run_id,
        started_at: first.started_at,
        finished_at: first.finished_at,
        refresh_secs: first.refresh_secs,
    };
    let total = first.total;
    let entries = rows
        .into_iter()
        .filter_map(|r| {
            Some(NeighbourEntry {
                position: i32::from(r.position?),
                artist_id: r.neighbour_id?,
                name: r.name?,
                image_url: r.image_url,
                score: i64::from(r.score?),
                listeners: i64::from(r.listeners?),
            })
        })
        .collect();
    Ok(Some(ArtistPage {
        version,
        total,
        entries,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TasteEntry {
    pub user_id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub image_url: Option<String>,
    /// Thousandths.
    pub score: i64,
    pub shared_artists: i64,
    pub top_shared: Vec<(i64, String)>,
}

/// `user_id`'s taste neighbours as a viewer may see them: still listed,
/// public and not waiting to be erased, and no block between them and the
/// user, nor the viewer. In order.
pub async fn taste_neighbours(
    pool: &PgPool,
    user_id: i64,
    viewer_id: Option<i64>,
) -> Result<Vec<TasteEntry>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT u.id, u.username, u.display_name, u.image_url, n.score, n.shared_artists,
               COALESCE(s.ids, '{}') AS "shared_ids!", COALESCE(s.names, '{}') AS "shared_names!"
        FROM user_neighbours n
        JOIN users u ON u.id = n.neighbour_user_id
        LEFT JOIN LATERAL (
            SELECT array_agg(a.id ORDER BY t.ord) AS ids, array_agg(a.name ORDER BY t.ord) AS names
            FROM unnest(n.top_shared) WITH ORDINALITY AS t(id, ord)
            JOIN artists a ON a.id = t.id
        ) s ON true
        WHERE n.user_id = $1
          AND u.similarity = 'listed' AND NOT u.is_private AND u.delete_after IS NULL
          AND NOT EXISTS (SELECT 1 FROM user_blocks b
                          WHERE (b.blocker_id = u.id AND b.blocked_id = $1)
                             OR (b.blocker_id = $1 AND b.blocked_id = u.id))
          AND ($2::bigint IS NULL OR NOT EXISTS (
                  SELECT 1 FROM user_blocks b
                  WHERE (b.blocker_id = u.id AND b.blocked_id = $2)
                     OR (b.blocker_id = $2 AND b.blocked_id = u.id)))
        ORDER BY n.position
        "#,
        user_id,
        viewer_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| TasteEntry {
            user_id: r.id,
            username: r.username,
            display_name: r.display_name,
            image_url: r.image_url,
            score: i64::from(r.score),
            shared_artists: i64::from(r.shared_artists),
            top_shared: r.shared_ids.into_iter().zip(r.shared_names).collect(),
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecommendationEntry {
    pub artist_id: i64,
    pub name: String,
    pub image_url: Option<String>,
    /// Thousandths.
    pub score: i64,
    pub because_id: i64,
    pub because_name: String,
    pub because_count: i64,
}

/// `user_id`'s recommendations without the artists skipped since, or
/// played since the run read the history (as far back as [`LATE_PLAYS`]
/// before it). In order.
pub async fn recommendations(
    pool: &PgPool,
    user_id: i64,
    run_started_at: DateTime<Utc>,
) -> Result<Vec<RecommendationEntry>, sqlx::Error> {
    sqlx::query_as!(
        RecommendationEntry,
        r#"
        SELECT e.artist_id AS "artist_id!", a.name, a.image_url, e.score::bigint AS "score!",
               e.because_id AS "because_id!", b.name AS because_name,
               e.because_count::bigint AS "because_count!"
        FROM user_recommendations r
        CROSS JOIN LATERAL unnest(r.artist_ids, r.scores, r.because_artist_ids, r.because_counts)
            WITH ORDINALITY AS e(artist_id, score, because_id, because_count, position)
        JOIN artists a ON a.id = e.artist_id
        JOIN artists b ON b.id = e.because_id
        WHERE r.user_id = $1
          AND NOT EXISTS (SELECT 1 FROM recommendation_skips k
                          WHERE k.user_id = $1 AND k.artist_id = e.artist_id AND k.until > NOW())
          AND NOT EXISTS (SELECT 1 FROM scrobbles s
                          WHERE s.user_id = $1 AND s.artist_id = e.artist_id AND s.played_at >= $2)
        ORDER BY e.position
        "#,
        user_id,
        run_started_at - LATE_PLAYS,
    )
    .fetch_all(pool)
    .await
}

/// What a user's recommendations depend on besides the run: their latest
/// play and their skips.
pub async fn recommendation_state(
    pool: &PgPool,
    user_id: i64,
) -> Result<(Option<DateTime<Utc>>, i64, Option<DateTime<Utc>>), sqlx::Error> {
    let r = sqlx::query!(
        r#"
        SELECT (SELECT last_seen_at FROM users WHERE id = $1) AS last_seen_at,
               (SELECT count(*) FROM recommendation_skips WHERE user_id = $1) AS "skips!",
               (SELECT max(skipped_at) FROM recommendation_skips WHERE user_id = $1) AS last_skip
        "#,
        user_id,
    )
    .fetch_one(pool)
    .await?;
    Ok((r.last_seen_at, r.skips, r.last_skip))
}

/// Whether the user has stored recommendations at all.
pub async fn has_recommendations(pool: &PgPool, user_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM user_recommendations WHERE user_id = $1) AS "has!""#,
        user_id,
    )
    .fetch_one(pool)
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    pub mode: SimilarityMode,
    pub changed_at: Option<DateTime<Utc>>,
    pub is_private: bool,
}

fn parse_mode(value: &str) -> SimilarityMode {
    SimilarityMode::parse(value).unwrap_or_default()
}

pub async fn setting(pool: &PgPool, user_id: i64) -> Result<Option<Setting>, sqlx::Error> {
    Ok(sqlx::query!(
        "SELECT similarity, similarity_changed_at, is_private FROM users WHERE id = $1",
        user_id
    )
    .fetch_optional(pool)
    .await?
    .map(|r| Setting {
        mode: parse_mode(&r.similarity),
        changed_at: r.similarity_changed_at,
        is_private: r.is_private,
    }))
}

/// Sets the user's mode and, at once, removes what it no longer allows: off
/// takes their recommendations, their neighbours and their place in
/// others' neighbours; anonymous their place in others'. Their anonymous
/// part in artist neighbours goes at the next run.
pub async fn set_setting(
    pool: &PgPool,
    user_id: i64,
    mode: SimilarityMode,
) -> Result<Setting, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let r = sqlx::query!(
        r#"
        UPDATE users
        SET similarity = $2,
            similarity_changed_at = CASE WHEN similarity = $2 THEN similarity_changed_at
                                         ELSE NOW() END
        WHERE id = $1
        RETURNING similarity_changed_at, is_private
        "#,
        user_id,
        mode.as_str(),
    )
    .fetch_one(&mut *tx)
    .await?;
    if mode != SimilarityMode::Listed {
        sqlx::query!(
            "DELETE FROM user_neighbours WHERE neighbour_user_id = $1",
            user_id
        )
        .execute(&mut *tx)
        .await?;
    }
    if mode == SimilarityMode::Off {
        sqlx::query!("DELETE FROM user_neighbours WHERE user_id = $1", user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query!(
            "DELETE FROM user_recommendations WHERE user_id = $1",
            user_id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Setting {
        mode,
        changed_at: r.similarity_changed_at,
        is_private: r.is_private,
    })
}

/// Hides an artist from the user's recommendations for `days`, keeping
/// the newest [`MAX_SKIPS`]. `false` when the artist doesn't exist.
pub async fn skip(
    pool: &PgPool,
    user_id: i64,
    artist_id: i64,
    days: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query!(
        r#"
        INSERT INTO recommendation_skips (user_id, artist_id, until)
        SELECT $1, id, NOW() + make_interval(days => $3) FROM artists WHERE id = $2
        ON CONFLICT (user_id, artist_id) DO UPDATE
            SET skipped_at = NOW(), until = EXCLUDED.until
        "#,
        user_id,
        artist_id,
        days as i32,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    sqlx::query!(
        r#"
        DELETE FROM recommendation_skips
        WHERE user_id = $1 AND artist_id IN (
            SELECT artist_id FROM recommendation_skips WHERE user_id = $1
            ORDER BY skipped_at DESC, artist_id OFFSET $2
        )
        "#,
        user_id,
        MAX_SKIPS,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(inserted)
}

pub async fn unskip(pool: &PgPool, user_id: i64, artist_id: i64) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query!(
        "DELETE FROM recommendation_skips WHERE user_id = $1 AND artist_id = $2",
        user_id,
        artist_id,
    )
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// Skips past their date.
pub async fn purge_skips(pool: &PgPool) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query!("DELETE FROM recommendation_skips WHERE until <= NOW()")
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Skip {
    pub artist_id: i64,
    pub artist_name: String,
    pub skipped_at: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

pub async fn skips(pool: &PgPool, user_id: i64) -> Result<Vec<Skip>, sqlx::Error> {
    sqlx::query_as!(
        Skip,
        r#"
        SELECT k.artist_id, a.name AS artist_name, k.skipped_at, k.until
        FROM recommendation_skips k
        JOIN artists a ON a.id = k.artist_id
        WHERE k.user_id = $1
        ORDER BY k.skipped_at DESC, k.artist_id
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await
}

/// A stored neighbour list as the worker CLI prints it: every entry, with
/// what readers would leave out.
#[derive(Debug, Clone)]
pub struct StoredNeighbour {
    pub artist_id: i64,
    pub name: String,
    pub score: i64,
    pub listeners: i64,
}

pub async fn stored_neighbours(
    pool: &PgPool,
    artist_id: i64,
) -> Result<Vec<StoredNeighbour>, sqlx::Error> {
    sqlx::query_as!(
        StoredNeighbour,
        r#"
        SELECT n.neighbour_id AS artist_id, a.name, n.score::bigint AS "score!",
               n.listeners::bigint AS "listeners!"
        FROM artist_neighbours n JOIN artists a ON a.id = n.neighbour_id
        WHERE n.artist_id = $1
        ORDER BY n.position
        "#,
        artist_id,
    )
    .fetch_all(pool)
    .await
}

/// Artist names by id, for the CLI.
pub async fn artist_names(pool: &PgPool, ids: &[i64]) -> Result<HashMap<i64, String>, sqlx::Error> {
    Ok(
        sqlx::query!("SELECT id, name FROM artists WHERE id = ANY($1)", ids)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|r| (r.id, r.name))
            .collect(),
    )
}

/// An artist by exact name (case-insensitive), the most played first.
pub async fn find_artist(pool: &PgPool, name: &str) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT id FROM artists WHERE lower(name) = lower($1)
        ORDER BY scrobble_count DESC, id LIMIT 1
        "#,
        name,
    )
    .fetch_optional(pool)
    .await
}
