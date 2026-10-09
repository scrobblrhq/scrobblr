//! Scrobble classification storage (see `shared::classification` for the
//! rule and `migrations/0010_scrobble_classification.sql` for the schema).
//!
//! Work is per (user, UTC day): `classify_user_day` reads that day plus the
//! lookback before it from `scrobbles` and rewrites the day's row and flags.
//! Days reach `classification_queue` from ingest, imports, enrichment and a
//! periodic sweep that compares stored days with `user_activity_daily`, so a
//! lost queue entry (crashed worker), a deleted day or a changed ruleset is
//! picked up again.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use sqlx::{PgConnection, PgPool};

use crate::queries::rankings;
use shared::classification::{self as rules, BudgetParams, Label, Play, Status};

/// Queue priorities; higher runs first.
pub const PRIORITY_SWEEP: i32 = 10;
pub const PRIORITY_INGEST: i32 = 50;
pub const PRIORITY_IMPORT: i32 = 100;

/// Ingest marks wait this long, so a burst is classified once, not per play.
const INGEST_SETTLE_SECS: f64 = 30.0;

#[derive(Debug, Clone, Copy)]
pub struct Ruleset {
    pub id: i32,
    pub params: BudgetParams,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuedDay {
    pub user_id: i64,
    pub day: NaiveDate,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatusCounts {
    pub counted: i64,
    pub suspect: i64,
    pub duplicate: i64,
    pub no_data: i64,
}

impl StatusCounts {
    fn add(&mut self, status: Status) {
        match status {
            Status::Counted => self.counted += 1,
            Status::Suspect => self.suspect += 1,
            Status::Duplicate => self.duplicate += 1,
            Status::NoData => self.no_data += 1,
        }
    }

    pub fn total(&self) -> i64 {
        self.counted + self.suspect + self.duplicate + self.no_data
    }
}

#[derive(Debug, Default)]
pub struct DayOutcome {
    pub counts: StatusCounts,
    /// Scrobbles whose label changed, by (previous, new); previous is `None`
    /// when the scrobble had not been classified.
    pub changes: BTreeMap<(Option<Status>, Status), i64>,
}

pub(crate) fn parse_status(s: &str) -> Status {
    match s {
        "suspect" => Status::Suspect,
        "duplicate" => Status::Duplicate,
        "no_data" => Status::NoData,
        _ => Status::Counted,
    }
}

fn day_start(day: NaiveDate) -> DateTime<Utc> {
    day.and_time(NaiveTime::MIN).and_utc()
}

/// Records the ruleset (idempotent) and returns its id.
pub async fn register_ruleset(pool: &PgPool, params: BudgetParams) -> Result<Ruleset, sqlx::Error> {
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO classifier_rulesets (fingerprint, params)
        VALUES ($1, jsonb_build_object(
            'rules_version', $2::int,
            'window_ms',     $3::bigint,
            'max_ratio',     $4::bigint / 1000.0,
            'slack_ms',      $5::bigint,
            'budget_ms',     $6::bigint))
        ON CONFLICT (fingerprint) DO UPDATE SET fingerprint = EXCLUDED.fingerprint
        RETURNING id
        "#,
        params.fingerprint(),
        rules::RULES_VERSION as i32,
        params.window_ms,
        params.max_ratio_permille,
        params.slack_ms,
        params.budget_ms(),
    )
    .fetch_one(pool)
    .await?;
    Ok(Ruleset { id, params })
}

/// The stored id of `params`' ruleset, without registering it.
pub async fn find_ruleset(
    pool: &PgPool,
    params: &BudgetParams,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(
        "SELECT id FROM classifier_rulesets WHERE fingerprint = $1",
        params.fingerprint(),
    )
    .fetch_optional(pool)
    .await
}

/// Ingest hot path: one index probe when the day is already queued.
pub async fn mark_scrobble_dirty(
    pool: &PgPool,
    user_id: i64,
    played_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO classification_queue (user_id, day, priority, not_before)
        VALUES ($1, ($2::timestamptz AT TIME ZONE 'UTC')::date, $3, NOW() + make_interval(secs => $4))
        ON CONFLICT (user_id, day) DO NOTHING
        "#,
        user_id,
        played_at,
        PRIORITY_INGEST,
        INGEST_SETTLE_SECS,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Queues classification for every UTC day in `[from, to]` of one user's
/// history, the counterpart of `refresh_scrobble_aggregates` for backfills
/// such as a Last.fm import. Call it after the inserts commit; the worker
/// classifies the days shortly after. Returns the days queued.
pub async fn enqueue_scrobble_classification(
    pool: &PgPool,
    user_id: i64,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        INSERT INTO classification_queue (user_id, day, priority)
        SELECT $1, d::date, $4
        FROM generate_series(($2::timestamptz AT TIME ZONE 'UTC')::date,
                             ($3::timestamptz AT TIME ZONE 'UTC')::date,
                             INTERVAL '1 day') AS d
        ON CONFLICT (user_id, day) DO UPDATE
            SET priority   = GREATEST(classification_queue.priority, EXCLUDED.priority),
                not_before = LEAST(classification_queue.not_before, EXCLUDED.not_before)
        "#,
        user_id,
        from,
        to,
        PRIORITY_IMPORT,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// After enrichment gives a track a length: its counted scrobbles may now be
/// over budget. Only the uncompressed last 30 days are touched (the
/// compression policy's age); flagged scrobbles of any age are re-checked by
/// the sweep through `scrobble_flags`.
pub async fn mark_track_durations_changed(
    pool: &PgPool,
    track_id: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        INSERT INTO classification_queue (user_id, day, priority)
        SELECT DISTINCT user_id, (played_at AT TIME ZONE 'UTC')::date, $2::int
        FROM scrobbles
        WHERE track_id = $1 AND played_at > NOW() - INTERVAL '30 days'
        ON CONFLICT (user_id, day) DO NOTHING
        "#,
        track_id,
        PRIORITY_SWEEP,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Removes up to `limit` due days from the queue and returns them. The
/// removal commits before classification, so ingest never waits on it; a
/// crash before the day is written is repaired by the sweep.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<QueuedDay>, sqlx::Error> {
    sqlx::query_as!(
        QueuedDay,
        r#"
        DELETE FROM classification_queue
        WHERE (user_id, day) IN (
            SELECT user_id, day FROM classification_queue
            WHERE not_before <= NOW()
            ORDER BY priority DESC, not_before
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING user_id, day
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}

pub async fn requeue(
    pool: &PgPool,
    day: QueuedDay,
    priority: i32,
    delay_secs: f64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO classification_queue (user_id, day, priority, not_before)
        VALUES ($1, $2, $3, NOW() + make_interval(secs => $4))
        ON CONFLICT (user_id, day) DO NOTHING
        "#,
        day.user_id,
        day.day,
        priority,
        delay_secs,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Classifies one user's UTC day with `ruleset` and, unless `dry_run`,
/// replaces its stored row and flags. Reads `scrobbles`, never writes it.
pub async fn classify_user_day(
    pool: &PgPool,
    ruleset: &Ruleset,
    user_id: i64,
    day: NaiveDate,
    dry_run: bool,
) -> Result<DayOutcome, sqlx::Error> {
    let start = day_start(day);
    let end = start + TimeDelta::days(1);
    let lookback = ruleset.params.lookback();

    let mut tx = pool.begin().await?;
    // Serializes concurrent classifications of the same day (worker + CLI).
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('classify:' || $1 || ':' || $2, 0))",
    )
    .bind(user_id.to_string())
    .bind(day.to_string())
    .execute(&mut *tx)
    .await?;

    let previous = sqlx::query_scalar!(
        "SELECT max_scrobble_id FROM scrobble_classification_days WHERE user_id = $1 AND day = $2",
        user_id,
        day,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let previous_flags: HashMap<i64, Status> = sqlx::query!(
        r#"SELECT scrobble_id, status::text AS "status!" FROM scrobble_flags WHERE user_id = $1 AND day = $2"#,
        user_id,
        day,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| (r.scrobble_id, parse_status(&r.status)))
    .collect();

    let rows = sqlx::query!(
        r#"
        SELECT s.id, s.played_at, s.track_id, s.duration_ms, s.listened_ms,
               COALESCE(t.duration_ms, t.deezer_duration_ms) AS catalog_duration_ms,
               t.mb_duration_ms
        FROM scrobbles s
        JOIN tracks t ON t.id = s.track_id
        WHERE s.user_id = $1 AND s.played_at > $2 AND s.played_at < $3
        ORDER BY s.played_at, s.id
        "#,
        user_id,
        start - lookback,
        end,
    )
    .fetch_all(&mut *tx)
    .await?;

    let plays: Vec<Play> = rows
        .iter()
        .map(|r| Play {
            id: r.id,
            track_id: r.track_id,
            played_at: r.played_at,
            mb_duration_ms: r.mb_duration_ms,
            catalog_duration_ms: r.catalog_duration_ms,
            reported_duration_ms: r.duration_ms,
            listened_ms: r.listened_ms,
        })
        .collect();
    let labels = rules::classify(&plays, start, &ruleset.params);

    let mut outcome = DayOutcome::default();
    for label in &labels {
        outcome.counts.add(label.status);
        let before = previous.filter(|max_id| label.id <= *max_id).map(|_| {
            previous_flags
                .get(&label.id)
                .copied()
                .unwrap_or(Status::Counted)
        });
        if before != Some(label.status) {
            *outcome.changes.entry((before, label.status)).or_default() += 1;
        }
    }
    if dry_run {
        tx.rollback().await?;
        return Ok(outcome);
    }

    // The day's ranking weights are made from its labels.
    rankings::enqueue_day(&mut *tx, user_id, day).await?;
    if labels.is_empty() {
        sqlx::query!(
            "DELETE FROM scrobble_classification_days WHERE user_id = $1 AND day = $2",
            user_id,
            day,
        )
        .execute(&mut *tx)
        .await?;
    } else {
        let lookback_count = rows.iter().filter(|r| r.played_at < start).count();
        let track_ids: HashMap<i64, i64> = rows.iter().map(|r| (r.id, r.track_id)).collect();
        let played_at: HashMap<i64, DateTime<Utc>> =
            rows.iter().map(|r| (r.id, r.played_at)).collect();
        write_day(
            &mut tx,
            ruleset.id,
            user_id,
            day,
            &labels,
            &outcome.counts,
            lookback_count as i32,
            &track_ids,
            &played_at,
        )
        .await?;
    }

    // This day's tail is the next day's lookback.
    let tail = rows.iter().filter(|r| r.played_at > end - lookback).count() as i32;
    let next = day.succ_opt().unwrap_or(day);
    let next_lookback = sqlx::query_scalar!(
        "SELECT lookback_count FROM scrobble_classification_days WHERE user_id = $1 AND day = $2 AND ruleset_id = $3",
        user_id,
        next,
        ruleset.id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if next_lookback.is_some_and(|n| n != tail) {
        sqlx::query!(
            r#"
            INSERT INTO classification_queue (user_id, day, priority)
            VALUES ($1, $2, $3)
            ON CONFLICT (user_id, day) DO NOTHING
            "#,
            user_id,
            next,
            PRIORITY_INGEST,
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
async fn write_day(
    tx: &mut PgConnection,
    ruleset_id: i32,
    user_id: i64,
    day: NaiveDate,
    labels: &[Label],
    counts: &StatusCounts,
    lookback: i32,
    track_ids: &HashMap<i64, i64>,
    played_at: &HashMap<i64, DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    let max_id = labels.iter().map(|l| l.id).max().unwrap_or_default();
    sqlx::query!(
        r#"
        INSERT INTO scrobble_classification_days
            (user_id, day, ruleset_id, scrobble_count, lookback_count, max_scrobble_id,
             counted, suspect, duplicate, no_data)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (user_id, day) DO UPDATE
            SET ruleset_id      = EXCLUDED.ruleset_id,
                scrobble_count  = EXCLUDED.scrobble_count,
                lookback_count  = EXCLUDED.lookback_count,
                max_scrobble_id = EXCLUDED.max_scrobble_id,
                counted         = EXCLUDED.counted,
                suspect         = EXCLUDED.suspect,
                duplicate       = EXCLUDED.duplicate,
                no_data         = EXCLUDED.no_data,
                classified_at   = NOW()
        "#,
        user_id,
        day,
        ruleset_id,
        labels.len() as i32,
        lookback,
        max_id,
        counts.counted as i32,
        counts.suspect as i32,
        counts.duplicate as i32,
        counts.no_data as i32,
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        "DELETE FROM scrobble_flags WHERE user_id = $1 AND day = $2",
        user_id,
        day,
    )
    .execute(&mut *tx)
    .await?;

    let flagged: Vec<&Label> = labels
        .iter()
        .filter(|l| l.status != Status::Counted)
        .collect();
    if flagged.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = flagged.iter().map(|l| l.id).collect();
    let times: Vec<DateTime<Utc>> = ids.iter().map(|id| played_at[id]).collect();
    let tracks: Vec<i64> = ids.iter().map(|id| track_ids[id]).collect();
    let statuses: Vec<&str> = flagged.iter().map(|l| l.status.as_str()).collect();
    let reasons: Vec<&str> = flagged
        .iter()
        .map(|l| l.reason.unwrap_or_default())
        .collect();
    let sources: Vec<&str> = flagged
        .iter()
        .map(|l| l.duration_source.map_or("", |s| s.as_str()))
        .collect();
    let occupancy: Vec<i32> = flagged.iter().map(|l| l.occupancy_ms as i32).collect();
    let loads: Vec<i64> = flagged.iter().map(|l| l.load_ms).collect();

    sqlx::query!(
        r#"
        INSERT INTO scrobble_flags
            (scrobble_id, played_at, user_id, day, track_id, status, reason,
             duration_source, occupancy_ms, load_ms)
        SELECT f.id, f.played_at, $1, $2, f.track_id, f.status::scrobble_status, f.reason,
               NULLIF(f.source, ''),
               CASE WHEN f.status IN ('no_data', 'duplicate') THEN NULL ELSE f.occupancy END,
               f.load
        FROM UNNEST($3::bigint[], $4::timestamptz[], $5::bigint[], $6::text[], $7::text[],
                    $8::text[], $9::int[], $10::bigint[])
             AS f(id, played_at, track_id, status, reason, source, occupancy, load)
        "#,
        user_id,
        day,
        &ids,
        &times,
        &tracks,
        &statuses as &[&str],
        &reasons as &[&str],
        &sources as &[&str],
        &occupancy,
        &loads,
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Queues days whose stored classification is missing, made with another
/// ruleset, out of date (scrobble count differs from `user_activity_daily`,
/// or the day no longer has scrobbles), or holding `no_data` scrobbles whose
/// track has a length now. (A new length only ever adds to the listening
/// time charged, so it can't clear a suspect.) Newest days first, at most
/// `limit` per query (`None` = all). Returns the days queued.
pub async fn enqueue_stale(
    conn: &mut PgConnection,
    ruleset_id: i32,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    limit: Option<i64>,
    priority: i32,
) -> Result<u64, sqlx::Error> {
    let days = sqlx::query!(
        r#"
        WITH activity AS (
            SELECT user_id, (day AT TIME ZONE 'UTC')::date AS day, scrobble_count
            FROM user_activity_daily
            WHERE ($3::date IS NULL OR day >= $3::date::timestamp AT TIME ZONE 'UTC')
              AND ($4::date IS NULL OR day <= $4::date::timestamp AT TIME ZONE 'UTC')
        ), stored AS (
            SELECT user_id, day, ruleset_id, scrobble_count
            FROM scrobble_classification_days
            WHERE ($3::date IS NULL OR day >= $3) AND ($4::date IS NULL OR day <= $4)
        ), stale AS (
            SELECT user_id, day
            FROM activity a
            FULL JOIN stored d USING (user_id, day)
            WHERE a.user_id IS NULL
               OR d.user_id IS NULL
               OR d.ruleset_id <> $1
               OR d.scrobble_count <> a.scrobble_count
        )
        INSERT INTO classification_queue (user_id, day, priority)
        SELECT s.user_id, s.day, $2
        FROM stale s
        WHERE NOT EXISTS (
            SELECT 1 FROM classification_queue q WHERE q.user_id = s.user_id AND q.day = s.day
        )
        ORDER BY s.day DESC
        LIMIT $5
        ON CONFLICT (user_id, day) DO NOTHING
        "#,
        ruleset_id,
        priority,
        from,
        to,
        limit,
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();

    let improved = sqlx::query!(
        r#"
        INSERT INTO classification_queue (user_id, day, priority)
        SELECT DISTINCT f.user_id, f.day, $1::int
        FROM scrobble_flags f
        JOIN tracks t ON t.id = f.track_id
        WHERE f.status = 'no_data'
          AND (t.mb_duration_ms > 0 OR t.duration_ms > 0 OR t.deezer_duration_ms > 0)
          AND ($2::date IS NULL OR f.day >= $2) AND ($3::date IS NULL OR f.day <= $3)
          AND NOT EXISTS (
              SELECT 1 FROM classification_queue q WHERE q.user_id = f.user_id AND q.day = f.day
          )
        LIMIT $4
        ON CONFLICT (user_id, day) DO NOTHING
        "#,
        priority,
        from,
        to,
        limit,
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();

    Ok(days + improved)
}

/// Every (user, day) with scrobbles or a stored classification in range.
pub async fn list_days(
    pool: &PgPool,
    user_id: Option<i64>,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<Vec<QueuedDay>, sqlx::Error> {
    sqlx::query_as!(
        QueuedDay,
        r#"
        SELECT user_id AS "user_id!", day AS "day!"
        FROM (
            SELECT user_id, (day AT TIME ZONE 'UTC')::date AS day FROM user_activity_daily
            UNION
            SELECT user_id, day FROM scrobble_classification_days
        ) days
        WHERE ($1::bigint IS NULL OR user_id = $1)
          AND ($2::date IS NULL OR day >= $2)
          AND ($3::date IS NULL OR day <= $3)
        ORDER BY user_id, day
        "#,
        user_id,
        from,
        to,
    )
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------------
//  Reporting (worker CLI)
// ---------------------------------------------------------------------------

/// The first UTC day with scrobbles, for reports over all history.
pub async fn first_day(pool: &PgPool) -> Result<Option<NaiveDate>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT (min(day) AT TIME ZONE 'UTC')::date AS "day" FROM user_activity_daily"#
    )
    .fetch_one(pool)
    .await
}

#[derive(Debug)]
pub struct RulesetTotals {
    pub ruleset_id: i32,
    pub fingerprint: String,
    pub days: i64,
    pub counts: StatusCounts,
}

pub async fn totals_by_ruleset(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<RulesetTotals>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT r.id, r.fingerprint, count(*) AS "days!",
               sum(d.counted)::bigint AS "counted!", sum(d.suspect)::bigint AS "suspect!",
               sum(d.duplicate)::bigint AS "duplicate!", sum(d.no_data)::bigint AS "no_data!"
        FROM scrobble_classification_days d
        JOIN classifier_rulesets r ON r.id = d.ruleset_id
        WHERE d.day BETWEEN $1 AND $2
        GROUP BY r.id, r.fingerprint
        ORDER BY r.id
        "#,
        from,
        to,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RulesetTotals {
            ruleset_id: r.id,
            fingerprint: r.fingerprint,
            days: r.days,
            counts: StatusCounts {
                counted: r.counted,
                suspect: r.suspect,
                duplicate: r.duplicate,
                no_data: r.no_data,
            },
        })
        .collect())
}

#[derive(Debug)]
pub struct Coverage {
    /// User-days with scrobbles.
    pub user_days: i64,
    pub current: i64,
    /// Stored but made with another ruleset or out of date.
    pub stale: i64,
    pub missing: i64,
    pub queued: i64,
}

pub async fn coverage(
    pool: &PgPool,
    ruleset_id: i32,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Coverage, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        WITH activity AS (
            SELECT user_id, (day AT TIME ZONE 'UTC')::date AS day, scrobble_count
            FROM user_activity_daily
            WHERE day >= $2::date::timestamp AT TIME ZONE 'UTC'
              AND day <= $3::date::timestamp AT TIME ZONE 'UTC'
        ), stored AS (
            SELECT user_id, day, ruleset_id, scrobble_count
            FROM scrobble_classification_days
            WHERE day BETWEEN $2 AND $3
        )
        SELECT
            count(a.user_id) AS "user_days!",
            count(*) FILTER (WHERE a.user_id IS NOT NULL AND d.ruleset_id = $1
                                   AND d.scrobble_count = a.scrobble_count) AS "current!",
            count(*) FILTER (WHERE d.user_id IS NOT NULL
                                   AND (a.user_id IS NULL OR d.ruleset_id <> $1
                                        OR d.scrobble_count <> a.scrobble_count)) AS "stale!",
            count(*) FILTER (WHERE d.user_id IS NULL) AS "missing!",
            (SELECT count(*) FROM classification_queue WHERE day BETWEEN $2 AND $3) AS "queued!"
        FROM activity a
        FULL JOIN stored d USING (user_id, day)
        "#,
        ruleset_id,
        from,
        to,
    )
    .fetch_one(pool)
    .await?;
    Ok(Coverage {
        user_days: row.user_days,
        current: row.current,
        stale: row.stale,
        missing: row.missing,
        queued: row.queued,
    })
}

#[derive(Debug)]
pub struct UserTotals {
    pub user_id: i64,
    pub username: String,
    pub days: i64,
    pub counts: StatusCounts,
    pub peak_load_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopBy {
    Suspect,
    Duplicate,
}

/// Users with the most suspect (or duplicate) scrobbles under `ruleset_id`.
pub async fn top_users(
    pool: &PgPool,
    ruleset_id: i32,
    from: NaiveDate,
    to: NaiveDate,
    by: TopBy,
    limit: i64,
) -> Result<Vec<UserTotals>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT d.user_id, u.username, count(*) AS "days!",
               sum(d.counted)::bigint AS "counted!", sum(d.suspect)::bigint AS "suspect!",
               sum(d.duplicate)::bigint AS "duplicate!", sum(d.no_data)::bigint AS "no_data!",
               (SELECT max(f.load_ms) FROM scrobble_flags f
                WHERE f.user_id = d.user_id AND f.day BETWEEN $2 AND $3
                  AND f.status = 'suspect') AS peak_load_ms
        FROM scrobble_classification_days d
        JOIN users u ON u.id = d.user_id
        WHERE d.ruleset_id = $1 AND d.day BETWEEN $2 AND $3
        GROUP BY d.user_id, u.username
        HAVING sum(CASE WHEN $5 THEN d.duplicate ELSE d.suspect END) > 0
        ORDER BY sum(CASE WHEN $5 THEN d.duplicate ELSE d.suspect END) DESC, d.user_id
        LIMIT $4
        "#,
        ruleset_id,
        from,
        to,
        limit,
        by == TopBy::Duplicate,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| UserTotals {
            user_id: r.user_id,
            username: r.username,
            days: r.days,
            counts: StatusCounts {
                counted: r.counted,
                suspect: r.suspect,
                duplicate: r.duplicate,
                no_data: r.no_data,
            },
            peak_load_ms: r.peak_load_ms,
        })
        .collect())
}

#[derive(Debug)]
pub struct DayTotals {
    pub day: NaiveDate,
    pub ruleset_id: i32,
    pub counts: StatusCounts,
    pub peak_load_ms: Option<i64>,
    pub classified_at: DateTime<Utc>,
}

pub async fn user_days(
    pool: &PgPool,
    user_id: i64,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<DayTotals>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT d.day, d.ruleset_id, d.counted, d.suspect, d.duplicate, d.no_data,
               d.classified_at,
               (SELECT max(f.load_ms) FROM scrobble_flags f
                WHERE f.user_id = d.user_id AND f.day = d.day
                  AND f.status = 'suspect') AS peak_load_ms
        FROM scrobble_classification_days d
        WHERE d.user_id = $1 AND d.day BETWEEN $2 AND $3
        ORDER BY d.day
        "#,
        user_id,
        from,
        to,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| DayTotals {
            day: r.day,
            ruleset_id: r.ruleset_id,
            counts: StatusCounts {
                counted: r.counted.into(),
                suspect: r.suspect.into(),
                duplicate: r.duplicate.into(),
                no_data: r.no_data.into(),
            },
            peak_load_ms: r.peak_load_ms,
            classified_at: r.classified_at,
        })
        .collect())
}

#[derive(Debug)]
pub struct ShortReports {
    pub username: String,
    pub plays: i64,
    /// Plays whose reported length is under half the longest other one.
    pub short: i64,
}

/// Users whose clients report track lengths far below the catalog's or
/// MusicBrainz's, the signature of a bot claiming short tracks (which the
/// rule ignores, as it charges the longest). Scans the range's scrobbles.
pub async fn short_duration_reports(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    limit: i64,
) -> Result<Vec<ShortReports>, sqlx::Error> {
    let start = day_start(from);
    let end = day_start(to) + TimeDelta::days(1);
    sqlx::query_as!(
        ShortReports,
        r#"
        SELECT u.username, count(*) AS "plays!",
               count(*) FILTER (WHERE s.duration_ms * 2
                                      < GREATEST(t.mb_duration_ms, t.duration_ms)) AS "short!"
        FROM scrobbles s
        JOIN tracks t ON t.id = s.track_id
        JOIN users u ON u.id = s.user_id
        WHERE s.played_at >= $1 AND s.played_at < $2
          AND s.duration_ms > 0 AND GREATEST(t.mb_duration_ms, t.duration_ms) > 0
        GROUP BY u.username
        HAVING count(*) FILTER (WHERE s.duration_ms * 2
                                      < GREATEST(t.mb_duration_ms, t.duration_ms)) > 0
        ORDER BY 3 DESC
        LIMIT $3
        "#,
        start,
        end,
        limit,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug)]
pub struct DurationDisagreement {
    pub track_id: i64,
    pub artist_name: String,
    pub title: String,
    pub catalog_ms: i32,
    pub musicbrainz_ms: i32,
    pub scrobble_count: i64,
}

/// Tracks whose catalog length (the first client to report it, or Last.fm)
/// and MusicBrainz length differ by more than 2x. Either can be the wrong
/// one: a snippet, live take or medley matched on MusicBrainz, a bad
/// crowd-sourced value in the catalog.
pub async fn duration_disagreements(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<DurationDisagreement>, sqlx::Error> {
    sqlx::query_as!(
        DurationDisagreement,
        r#"
        SELECT t.id AS track_id, a.name AS artist_name, t.title,
               t.duration_ms AS "catalog_ms!", t.mb_duration_ms AS "musicbrainz_ms!",
               t.scrobble_count
        FROM tracks t
        JOIN artists a ON a.id = t.artist_id
        WHERE t.duration_ms > 0 AND t.mb_duration_ms > 0
          AND (t.duration_ms * 2 < t.mb_duration_ms OR t.mb_duration_ms * 2 < t.duration_ms)
        ORDER BY t.scrobble_count DESC, t.id
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}
