//! Worker heartbeats and queue depths, for `worker status`, /health/worker
//! and /metrics.

use sqlx::PgPool;

use shared::monitoring::Heartbeat;

/// Makes `loops` the worker's loops, as of now: their rows start over and
/// rows of loops it no longer runs are deleted.
pub async fn register_loops(pool: &PgPool, loops: &[(&str, i32)]) -> Result<(), sqlx::Error> {
    let names: Vec<String> = loops.iter().map(|(n, _)| n.to_string()).collect();
    let intervals: Vec<i32> = loops.iter().map(|(_, i)| *i).collect();
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "DELETE FROM worker_heartbeats WHERE loop_name <> ALL($1)",
        &names
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"
        INSERT INTO worker_heartbeats (loop_name, interval_secs, started_at)
        SELECT name, secs, NOW() FROM unnest($1::text[], $2::int[]) AS l (name, secs)
        ON CONFLICT (loop_name) DO UPDATE
        SET interval_secs = EXCLUDED.interval_secs, started_at = EXCLUDED.started_at
        "#,
        &names,
        &intervals,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// A finished run: fine when `error` is `None`.
pub async fn record_run(
    pool: &PgPool,
    loop_name: &str,
    interval_secs: i32,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO worker_heartbeats
            (loop_name, interval_secs, started_at, last_run_at, last_ok_at, last_error_at, last_error)
        VALUES ($1, $2, NOW(), NOW(),
                CASE WHEN $3::text IS NULL THEN NOW() END,
                CASE WHEN $3::text IS NOT NULL THEN NOW() END,
                $3)
        ON CONFLICT (loop_name) DO UPDATE
        SET interval_secs = EXCLUDED.interval_secs,
            last_run_at = NOW(),
            last_ok_at = COALESCE(EXCLUDED.last_ok_at, worker_heartbeats.last_ok_at),
            last_error_at = COALESCE(EXCLUDED.last_error_at, worker_heartbeats.last_error_at),
            last_error = COALESCE(EXCLUDED.last_error, worker_heartbeats.last_error)
        "#,
        loop_name,
        interval_secs,
        error,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A loop this worker doesn't run (not configured).
pub async fn remove_loop(pool: &PgPool, loop_name: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "DELETE FROM worker_heartbeats WHERE loop_name = $1",
        loop_name
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn heartbeats(pool: &PgPool) -> Result<Vec<Heartbeat>, sqlx::Error> {
    sqlx::query_as!(
        Heartbeat,
        r#"
        SELECT loop_name, interval_secs, started_at, last_run_at, last_ok_at,
               last_error_at, last_error
        FROM worker_heartbeats
        ORDER BY loop_name
        "#
    )
    .fetch_all(pool)
    .await
}

/// Work waiting in one of the worker's queues.
#[derive(Debug, Clone)]
pub struct QueueDepth {
    pub queue: String,
    /// Items due now.
    pub due: i64,
    /// How long the oldest due item has waited, in seconds.
    pub oldest_due_secs: Option<f64>,
}

/// Classification and ranking days, enrichment jobs and history imports
/// waiting to run.
pub async fn queue_depths(pool: &PgPool) -> Result<Vec<QueueDepth>, sqlx::Error> {
    sqlx::query_as!(
        QueueDepth,
        r#"
        SELECT 'classification' AS "queue!", count(*) AS "due!",
               extract(epoch FROM NOW() - min(not_before))::float8 AS oldest_due_secs
        FROM classification_queue WHERE not_before <= NOW()
        UNION ALL
        SELECT 'ranking', count(*), extract(epoch FROM NOW() - min(enqueued_at))::float8
        FROM ranking_queue
        UNION ALL
        SELECT 'enrichment', count(*), extract(epoch FROM NOW() - min(next_attempt_at))::float8
        FROM enrichment_jobs WHERE status = 'pending' AND next_attempt_at <= NOW()
        UNION ALL
        SELECT 'import', count(*), extract(epoch FROM NOW() - min(created_at))::float8
        FROM scrobble_imports WHERE status IN ('pending', 'running')
        "#
    )
    .fetch_all(pool)
    .await
}
