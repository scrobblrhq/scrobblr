//! Shadow-mode scrobble classification: ruleset registry, the per user-day
//! work ledger (`classification_days`) and the per-scrobble labels
//! (`scrobble_classifications`). See migrations/0007 for the schema notes.
//!
//! Ledger states:
//!
//! ```text
//!  (no row) ──mark──► DIRTY ──claim (settle passed)──► CLAIMED (lease)
//!                       ▲  ▲                              │
//!                       │  ├── gen changed meanwhile: due again after settle
//!                       │  └── error: attempts+1, backoff ┤
//!                       │                                 │ gen unchanged
//!   mark / reconcile /  │                                 ▼
//!   track refresh /     └──────────────────────────── CLEAN (ruleset_id, counts)
//!   outdated ruleset          (an expired lease makes CLAIMED claimable again)
//! ```
//!
//! Every "mark" (ingest, reclassify, reconcile, track refresh) uses the same
//! upsert: bump `dirty_gen`, keep the higher priority while already dirty,
//! and only start the settle clock (`next_attempt_at = NOW()`) when the row
//! wasn't dirty yet — so a stream of marks batches up instead of starving.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Ledger priorities; higher is claimed first.
pub const PRIORITY_BACKGROUND: i32 = 10; // outdated ruleset, backfill, track refresh
pub const PRIORITY_INGEST: i32 = 50; // new scrobbles, reconciliation
pub const PRIORITY_MANUAL: i32 = 100; // `classify reclassify`

/// Labels are upserted in chunks of this many rows per statement.
const UPSERT_CHUNK: usize = 10_000;

/// Registers (or re-activates) the worker's ruleset and returns its id.
/// `params` is the JSON-serialized thresholds; identical thresholds under the
/// same rules version map to the same id, so restarts don't fork rulesets.
pub async fn activate_ruleset(
    pool: &PgPool,
    rules_version: i32,
    params: &str,
) -> Result<i32, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        INSERT INTO classification_rulesets (rules_version, params, activated_at)
        VALUES ($1, $2::text::jsonb, NOW())
        ON CONFLICT (rules_version, params) DO UPDATE SET activated_at = NOW()
        RETURNING id
        "#,
        rules_version,
        params,
    )
    .fetch_one(pool)
    .await?;
    Ok(row.id)
}

/// The ruleset most recently activated by a worker, if any.
#[derive(Debug)]
pub struct ActiveRuleset {
    pub id: i32,
    pub rules_version: i32,
    pub params: String,
}

pub async fn active_ruleset(pool: &PgPool) -> Result<Option<ActiveRuleset>, sqlx::Error> {
    sqlx::query_as!(
        ActiveRuleset,
        r#"
        SELECT id, rules_version, params::text AS "params!"
        FROM classification_rulesets
        WHERE activated_at IS NOT NULL
        ORDER BY activated_at DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await
}

/// Marks the user-day of a newly ingested scrobble dirty, plus the next day
/// when the scrobble is within one window of midnight (it is in the lookback
/// of the next day's first scrobbles).
///
/// Runs inside `ingest_scrobble` — in the API process too, which has no
/// classifier config — so the window comes from the latest-activated ruleset
/// in the same statement. Before any worker has run there is none; then the
/// next day is always marked (the window can never exceed one day).
pub async fn mark_dirty(
    pool: &PgPool,
    user_id: i64,
    played_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO classification_days (user_id, day, priority)
        SELECT DISTINCT $1::bigint, d.day, $3::int
        FROM (
            SELECT COALESCE(
                (SELECT (params->>'window_secs')::int
                 FROM classification_rulesets
                 WHERE activated_at IS NOT NULL
                 ORDER BY activated_at DESC
                 LIMIT 1),
                86400) AS window_secs
        ) w
        CROSS JOIN LATERAL (VALUES
            (time_bucket('1 day', $2::timestamptz)),
            (time_bucket('1 day', $2::timestamptz + make_interval(secs => w.window_secs)))
        ) AS d(day)
        ON CONFLICT (user_id, day) DO UPDATE SET
            dirty_gen       = classification_days.dirty_gen + 1,
            priority        = CASE WHEN classification_days.dirty
                                   THEN GREATEST(classification_days.priority, EXCLUDED.priority)
                                   ELSE EXCLUDED.priority END,
            next_attempt_at = CASE WHEN classification_days.dirty
                                   THEN classification_days.next_attempt_at
                                   ELSE NOW() END,
            dirty           = TRUE
        "#,
        user_id,
        played_at,
        PRIORITY_INGEST,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A user-day claimed for classification.
#[derive(Debug, Clone)]
pub struct ClaimedDay {
    pub user_id: i64,
    pub day: DateTime<Utc>,
    /// `dirty_gen` at claim time; completion clears `dirty` only if unchanged.
    pub dirty_gen: i64,
    pub attempts: i32,
}

/// Claims up to `limit` dirty days whose settle delay has passed, highest
/// priority first. A claim is a lease: rows claimed more than `lease_secs`
/// ago (crashed worker) are claimable again. SKIP LOCKED keeps concurrent
/// claimers from double-processing.
pub async fn claim_dirty_days(
    pool: &PgPool,
    limit: i64,
    settle_secs: f64,
    lease_secs: f64,
) -> Result<Vec<ClaimedDay>, sqlx::Error> {
    sqlx::query_as!(
        ClaimedDay,
        r#"
        UPDATE classification_days
        SET claimed_at = NOW()
        WHERE (user_id, day) IN (
            SELECT user_id, day FROM classification_days
            WHERE dirty
              AND next_attempt_at <= NOW() - make_interval(secs => $2)
              AND (claimed_at IS NULL OR claimed_at < NOW() - make_interval(secs => $3))
            ORDER BY priority DESC, next_attempt_at
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING user_id, day, dirty_gen, attempts
        "#,
        limit,
        settle_secs,
        lease_secs,
    )
    .fetch_all(pool)
    .await
}

/// A scrobble as loaded for classification.
#[derive(Debug)]
pub struct WindowScrobble {
    pub id: i64,
    pub played_at: DateTime<Utc>,
    pub track_id: i64,
    pub source: String,
    /// Catalog duration first (shared across users, possibly MusicBrainz-
    /// backed), then the duration the client reported with this scrobble.
    pub duration_ms: Option<i32>,
}

/// A user's scrobbles with `after < played_at < before`, oldest first.
pub async fn load_user_window(
    pool: &PgPool,
    user_id: i64,
    after: DateTime<Utc>,
    before: DateTime<Utc>,
) -> Result<Vec<WindowScrobble>, sqlx::Error> {
    sqlx::query_as!(
        WindowScrobble,
        r#"
        SELECT s.id, s.played_at, s.track_id, s.source,
               COALESCE(t.duration_ms, s.duration_ms) AS "duration_ms?"
        FROM scrobbles s
        JOIN tracks t ON t.id = s.track_id
        WHERE s.user_id = $1
          AND s.played_at > $2
          AND s.played_at < $3
        ORDER BY s.played_at, s.id
        "#,
        user_id,
        after,
        before,
    )
    .fetch_all(pool)
    .await
}

/// Labels for every scrobble of one user-day, as parallel columns (they are
/// bound as Postgres arrays).
#[derive(Debug, Default)]
pub struct DayLabels {
    pub scrobble_ids: Vec<i64>,
    pub played_at: Vec<DateTime<Utc>>,
    pub track_ids: Vec<i64>,
    pub statuses: Vec<String>,
    pub reasons: Vec<String>,
    pub scores: Vec<Option<f32>>,
    pub counted: i32,
    pub suspect: i32,
    pub no_data: i32,
}

/// Writes a day's labels and completes its claim, in one transaction.
///
/// - Labels are upserted set-based (`UNNEST` over array binds) rather than
///   one statement per scrobble: a bot day can hold tens of thousands.
///   Unchanged labels are skipped (`IS DISTINCT FROM`), so they keep their
///   `classified_at` and a second run over the same data writes nothing.
/// - Labels in this day whose scrobble no longer exists are deleted. The
///   delete is scoped to the day itself, never to the lookback the caller
///   loaded, which belongs to the previous day's job.
/// - If the day was marked again since the claim (`dirty_gen` changed), it
///   stays dirty and becomes due again after the settle delay: the labels just
///   written may already be stale, and re-running immediately would hot-loop
///   on a user who keeps scrobbling.
///
/// Returns whether the day ended up clean.
pub async fn apply_day_results(
    pool: &PgPool,
    claimed: &ClaimedDay,
    ruleset_id: i32,
    labels: &DayLabels,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let n = labels.scrobble_ids.len();
    let mut start = 0;
    while start < n {
        let end = (start + UPSERT_CHUNK).min(n);
        sqlx::query!(
            r#"
            INSERT INTO scrobble_classifications
                (scrobble_id, played_at, user_id, track_id, status, reason, ruleset_id, score)
            SELECT u.scrobble_id, u.played_at, $1, u.track_id, u.status, u.reason, $2, u.score
            FROM UNNEST($3::bigint[], $4::timestamptz[], $5::bigint[], $6::text[], $7::text[], $8::real[])
                AS u(scrobble_id, played_at, track_id, status, reason, score)
            ON CONFLICT (scrobble_id, played_at) DO UPDATE SET
                user_id       = EXCLUDED.user_id,
                track_id      = EXCLUDED.track_id,
                status        = EXCLUDED.status,
                reason        = EXCLUDED.reason,
                ruleset_id    = EXCLUDED.ruleset_id,
                score         = EXCLUDED.score,
                classified_at = NOW()
            WHERE (scrobble_classifications.user_id, scrobble_classifications.track_id,
                   scrobble_classifications.status, scrobble_classifications.reason,
                   scrobble_classifications.ruleset_id, scrobble_classifications.score)
                  IS DISTINCT FROM
                  (EXCLUDED.user_id, EXCLUDED.track_id, EXCLUDED.status,
                   EXCLUDED.reason, EXCLUDED.ruleset_id, EXCLUDED.score)
            "#,
            claimed.user_id,
            ruleset_id,
            &labels.scrobble_ids[start..end],
            &labels.played_at[start..end],
            &labels.track_ids[start..end],
            &labels.statuses[start..end],
            &labels.reasons[start..end],
            &labels.scores[start..end] as &[Option<f32>],
        )
        .execute(&mut *tx)
        .await?;
        start = end;
    }

    sqlx::query!(
        r#"
        DELETE FROM scrobble_classifications
        WHERE user_id = $1
          AND played_at >= $2
          AND played_at <  $2 + INTERVAL '1 day'
          AND NOT (scrobble_id = ANY($3::bigint[]))
        "#,
        claimed.user_id,
        claimed.day,
        &labels.scrobble_ids,
    )
    .execute(&mut *tx)
    .await?;

    let clean = sqlx::query_scalar!(
        r#"
        UPDATE classification_days SET
            counted         = $3,
            suspect         = $4,
            no_data         = $5,
            ruleset_id      = $6,
            classified_at   = NOW(),
            claimed_at      = NULL,
            attempts        = 0,
            last_error      = NULL,
            dirty           = (dirty_gen <> $7),
            next_attempt_at = CASE WHEN dirty_gen <> $7 THEN NOW() ELSE next_attempt_at END
        WHERE user_id = $1 AND day = $2
        RETURNING NOT dirty AS "clean!"
        "#,
        claimed.user_id,
        claimed.day,
        labels.counted,
        labels.suspect,
        labels.no_data,
        ruleset_id,
        claimed.dirty_gen,
    )
    .fetch_optional(&mut *tx)
    .await?
    // Ledger row gone (user deleted mid-flight): nothing left to keep dirty.
    .unwrap_or(true);

    tx.commit().await?;
    Ok(clean)
}

/// Releases a claim after a failure and retries after `delay_secs`.
pub async fn reschedule_day(
    pool: &PgPool,
    user_id: i64,
    day: DateTime<Utc>,
    error: &str,
    delay_secs: f64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE classification_days
        SET claimed_at      = NULL,
            attempts        = attempts + 1,
            last_error      = $3,
            next_attempt_at = NOW() + make_interval(secs => $4)
        WHERE user_id = $1 AND day = $2
        "#,
        user_id,
        day,
        error,
        delay_secs,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Re-dirties every clean day last classified under a different ruleset, at
/// background priority (fresh ingest goes first). Returns the number of days.
pub async fn mark_outdated(pool: &PgPool, current_ruleset_id: i32) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        UPDATE classification_days
        SET dirty           = TRUE,
            dirty_gen       = dirty_gen + 1,
            priority        = $2,
            next_attempt_at = NOW()
        WHERE NOT dirty AND ruleset_id IS DISTINCT FROM $1
        "#,
        current_ruleset_id,
        PRIORITY_BACKGROUND,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Marks every user-day that has scrobbles matching the filters dirty
/// (`classify reclassify` / `classify backfill`). `None` means unbounded.
/// Returns the number of days marked.
pub async fn enqueue_range(
    pool: &PgPool,
    user_id: Option<i64>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    priority: i32,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        INSERT INTO classification_days (user_id, day, priority)
        SELECT DISTINCT s.user_id, time_bucket('1 day', s.played_at), $4::int
        FROM scrobbles s
        WHERE ($1::bigint IS NULL OR s.user_id = $1)
          AND ($2::timestamptz IS NULL OR s.played_at >= $2)
          AND ($3::timestamptz IS NULL OR s.played_at <  $3)
        ON CONFLICT (user_id, day) DO UPDATE SET
            dirty_gen       = classification_days.dirty_gen + 1,
            priority        = CASE WHEN classification_days.dirty
                                   THEN GREATEST(classification_days.priority, EXCLUDED.priority)
                                   ELSE EXCLUDED.priority END,
            next_attempt_at = CASE WHEN classification_days.dirty
                                   THEN classification_days.next_attempt_at
                                   ELSE NOW() END,
            dirty           = TRUE
        "#,
        user_id,
        from,
        to,
        priority,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Safety net for the best-effort ingest hook: compares per user-day scrobble
/// counts over the last `days` days (plus today) with the ledger and marks
/// days that are missing or whose label count doesn't match. Days already
/// dirty are left alone. Returns the number of days marked.
pub async fn reconcile_recent(pool: &PgPool, days: i32) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        INSERT INTO classification_days (user_id, day, priority)
        SELECT a.user_id, a.day, $2::int
        FROM (
            SELECT user_id, time_bucket('1 day', played_at) AS day, COUNT(*) AS n
            FROM scrobbles
            WHERE played_at >= time_bucket('1 day', NOW()) - make_interval(days => $1)
            GROUP BY 1, 2
        ) a
        LEFT JOIN classification_days c ON c.user_id = a.user_id AND c.day = a.day
        WHERE c.user_id IS NULL
           OR (NOT c.dirty AND c.counted + c.suspect + c.no_data <> a.n)
        ON CONFLICT (user_id, day) DO UPDATE SET
            dirty_gen       = classification_days.dirty_gen + 1,
            priority        = CASE WHEN classification_days.dirty
                                   THEN GREATEST(classification_days.priority, EXCLUDED.priority)
                                   ELSE EXCLUDED.priority END,
            next_attempt_at = CASE WHEN classification_days.dirty
                                   THEN classification_days.next_attempt_at
                                   ELSE NOW() END,
            dirty           = TRUE
        "#,
        days,
        PRIORITY_INGEST,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Drains up to `limit` queued track-duration fills and marks every user-day
/// holding a label for those tracks dirty (plus the next day, as in
/// [`mark_dirty`]). Returns `(tracks, days)`.
///
/// Only entries older than `min_age_secs` (the claim lease) are taken: a
/// classification that read the old NULL duration just before the fill
/// committed has then finished and written its labels, so the lookup below
/// sees them. Drain and marking share a transaction, so a crash loses nothing.
///
/// Affected days are found through `scrobble_classifications.track_id`, not
/// `scrobbles`: compressed scrobble chunks have no track index, so a lookup
/// there would decompress every user's history.
pub async fn drain_track_refresh(
    pool: &PgPool,
    min_age_secs: f64,
    limit: i64,
) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;

    let track_ids = sqlx::query_scalar!(
        r#"
        DELETE FROM classification_track_refresh
        WHERE track_id IN (
            SELECT track_id FROM classification_track_refresh
            WHERE enqueued_at < NOW() - make_interval(secs => $1)
            ORDER BY enqueued_at
            LIMIT $2
            FOR UPDATE SKIP LOCKED
        )
        RETURNING track_id
        "#,
        min_age_secs,
        limit,
    )
    .fetch_all(&mut *tx)
    .await?;

    if track_ids.is_empty() {
        tx.commit().await?;
        return Ok((0, 0));
    }

    let days = sqlx::query!(
        r#"
        INSERT INTO classification_days (user_id, day, priority)
        SELECT DISTINCT c.user_id, d.day, $2::int
        FROM scrobble_classifications c
        CROSS JOIN (
            SELECT COALESCE(
                (SELECT (params->>'window_secs')::int
                 FROM classification_rulesets
                 WHERE activated_at IS NOT NULL
                 ORDER BY activated_at DESC
                 LIMIT 1),
                86400) AS window_secs
        ) w
        CROSS JOIN LATERAL (VALUES
            (time_bucket('1 day', c.played_at)),
            (time_bucket('1 day', c.played_at + make_interval(secs => w.window_secs)))
        ) AS d(day)
        WHERE c.track_id = ANY($1::bigint[])
        ON CONFLICT (user_id, day) DO UPDATE SET
            dirty_gen       = classification_days.dirty_gen + 1,
            priority        = CASE WHEN classification_days.dirty
                                   THEN GREATEST(classification_days.priority, EXCLUDED.priority)
                                   ELSE EXCLUDED.priority END,
            next_attempt_at = CASE WHEN classification_days.dirty
                                   THEN classification_days.next_attempt_at
                                   ELSE NOW() END,
            dirty           = TRUE
        "#,
        &track_ids,
        PRIORITY_BACKGROUND,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    tx.commit().await?;
    Ok((track_ids.len() as u64, days))
}

// ---------------------------------------------------------------------------
// Report (internal review of shadow-mode results)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct StatusCount {
    pub ruleset_id: i32,
    pub status: String,
    pub reason: String,
    pub count: i64,
}

/// Label counts per ruleset, status and reason.
pub async fn status_distribution(pool: &PgPool) -> Result<Vec<StatusCount>, sqlx::Error> {
    sqlx::query_as!(
        StatusCount,
        r#"
        SELECT ruleset_id, status, reason, COUNT(*) AS "count!"
        FROM scrobble_classifications
        GROUP BY ruleset_id, status, reason
        ORDER BY ruleset_id, status, reason
        "#,
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug)]
pub struct LedgerSummary {
    /// Waiting to be (re)classified.
    pub dirty: i64,
    /// Clean, but classified under another ruleset than `current`.
    pub outdated: i64,
    /// Clean and classified under `current`.
    pub current: i64,
    /// Last attempt failed (will retry with backoff).
    pub erroring: i64,
}

pub async fn ledger_summary(
    pool: &PgPool,
    current_ruleset_id: Option<i32>,
) -> Result<LedgerSummary, sqlx::Error> {
    sqlx::query_as!(
        LedgerSummary,
        r#"
        SELECT
            COUNT(*) FILTER (WHERE dirty)                                        AS "dirty!",
            COUNT(*) FILTER (WHERE NOT dirty AND ruleset_id IS DISTINCT FROM $1) AS "outdated!",
            COUNT(*) FILTER (WHERE NOT dirty AND ruleset_id = $1)                AS "current!",
            COUNT(*) FILTER (WHERE last_error IS NOT NULL)                       AS "erroring!"
        FROM classification_days
        "#,
        current_ruleset_id,
    )
    .fetch_one(pool)
    .await
}

#[derive(Debug)]
pub struct TopSuspect {
    pub user_id: i64,
    pub username: String,
    pub is_private: bool,
    pub suspect: i64,
    pub total: i64,
    pub max_score: Option<f32>,
}

/// Users with the most suspect scrobbles. Private users are included: this
/// is an internal review tool, not a public surface.
pub async fn top_suspects(pool: &PgPool, limit: i64) -> Result<Vec<TopSuspect>, sqlx::Error> {
    sqlx::query_as!(
        TopSuspect,
        r#"
        SELECT d.user_id                                        AS "user_id!",
               u.username                                       AS "username!",
               u.is_private                                     AS "is_private!",
               SUM(d.suspect)::bigint                           AS "suspect!",
               SUM(d.counted + d.suspect + d.no_data)::bigint   AS "total!",
               (SELECT MAX(c.score)
                FROM scrobble_classifications c
                WHERE c.user_id = d.user_id AND c.status = 'suspect') AS "max_score?"
        FROM classification_days d
        JOIN users u ON u.id = d.user_id
        GROUP BY d.user_id, u.username, u.is_private
        HAVING SUM(d.suspect) > 0
        ORDER BY SUM(d.suspect) DESC, d.user_id
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}
