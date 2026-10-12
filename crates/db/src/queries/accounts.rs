//! Account deletion: the request, its restore by a login, and the erase.

use std::collections::HashMap;
use std::time::Instant;

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use shared::media::{UploadKey, UploadKind};

pub struct DeletionScheduled {
    pub delete_after: DateTime<Utc>,
    /// Ended sessions, whose Redis cache entries the caller removes.
    pub sessions: Vec<Uuid>,
}

/// Schedules the erase `grace` from now (an earlier request's date stands)
/// and revokes everything that acts as the user; the history stays until
/// the erase.
pub async fn schedule_deletion(
    pool: &PgPool,
    user_id: i64,
    grace: TimeDelta,
) -> Result<DeletionScheduled, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let delete_after = sqlx::query_scalar!(
        r#"
        UPDATE users
        SET delete_after = COALESCE(delete_after, NOW() + make_interval(secs => $2))
        WHERE id = $1
        RETURNING delete_after AS "delete_after!"
        "#,
        user_id,
        grace.num_seconds() as f64,
    )
    .fetch_one(&mut *tx)
    .await?;
    let sessions = sqlx::query_scalar!(
        "DELETE FROM user_sessions WHERE user_id = $1 RETURNING id",
        user_id
    )
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM api_tokens WHERE user_id = $1", user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM scrobbler_credentials WHERE user_id = $1",
        user_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM scrobbler_authorizations WHERE user_id = $1",
        user_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM connected_accounts WHERE user_id = $1", user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM now_playing WHERE user_id = $1", user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        r#"
        UPDATE scrobble_imports
        SET status = 'cancelled', finished_at = NOW(), lease_token = NULL, leased_until = NULL
        WHERE user_id = $1 AND status IN ('pending', 'running')
        "#,
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(DeletionScheduled {
        delete_after,
        sessions,
    })
}

/// `false` when nothing was scheduled or the erase has begun.
pub async fn restore(pool: &PgPool, user_id: i64) -> Result<bool, sqlx::Error> {
    let restored = sqlx::query!(
        r#"
        UPDATE users SET delete_after = NULL
        WHERE id = $1 AND delete_after IS NOT NULL AND erase_started_at IS NULL
        "#,
        user_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(restored > 0)
}

/// After this, a claimed erase is taken as failed and claimed again.
const ERASE_LEASE: TimeDelta = TimeDelta::minutes(15);

/// The next account due, marked so a login can no longer restore it.
pub async fn claim_due(pool: &PgPool) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        UPDATE users SET erase_started_at = NOW()
        WHERE id = (
            SELECT id FROM users
            WHERE delete_after <= NOW()
              AND (erase_started_at IS NULL OR erase_started_at < NOW() - make_interval(secs => $1))
            ORDER BY delete_after
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id
        "#,
        ERASE_LEASE.num_seconds() as f64,
    )
    .fetch_optional(pool)
    .await
}

/// For `worker accounts erase`: due now, and no longer restorable.
pub async fn mark_erasing(pool: &PgPool, user_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE users SET delete_after = NOW(), erase_started_at = NOW() WHERE id = $1",
        user_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub struct Pending {
    pub user_id: i64,
    pub username: String,
    pub delete_after: DateTime<Utc>,
    pub erase_started_at: Option<DateTime<Utc>>,
}

pub async fn pending(pool: &PgPool) -> Result<Vec<Pending>, sqlx::Error> {
    sqlx::query_as!(
        Pending,
        r#"
        SELECT id AS user_id, username, delete_after AS "delete_after!", erase_started_at
        FROM users WHERE delete_after IS NOT NULL
        ORDER BY delete_after
        "#
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Default)]
pub struct Erased {
    pub scrobbles: u64,
    pub chunks: u32,
    pub aggregate_rows: u64,
    /// An aggregate isn't grouped by user: its policy refresh cleans it.
    pub aggregates_by_refresh: bool,
    /// The user's uploads no row refers to any more, for the caller to delete.
    pub uploads: Vec<UploadKey>,
    pub took_ms: u128,
}

/// Erases a user; every step is safe to repeat.
///
/// Deleting from `scrobbles` would invalidate the aggregates over the
/// user's whole history, and the refresh would recompute it for every user.
/// The aggregates are grouped by user, so the deletes skip invalidation and
/// the user's materialized rows are deleted instead. The guard transaction
/// keeps refreshes from writing those rows back meanwhile.
pub async fn erase(pool: &PgPool, user_id: i64) -> Result<Erased, sqlx::Error> {
    let started = Instant::now();
    let mut erased = Erased::default();

    let aggregates = scrobble_aggregates(pool).await?;
    let direct = aggregates.iter().all(|a| a.per_user);
    erased.aggregates_by_refresh = !direct;
    let mut guard = if direct {
        let mut guard = pool.begin().await?;
        sqlx::query("SET LOCAL lock_timeout = '5min'")
            .execute(&mut *guard)
            .await?;
        if !aggregates.is_empty() {
            let tables: Vec<&str> = aggregates.iter().map(|a| a.table.as_str()).collect();
            sqlx::query(&format!(
                "LOCK TABLE {} IN SHARE ROW EXCLUSIVE MODE",
                tables.join(", ")
            ))
            .execute(&mut *guard)
            .await?;
        }
        Some(guard)
    } else {
        None
    };

    let chunks: Vec<String> =
        sqlx::query_scalar("SELECT c::text FROM show_chunks('scrobbles') AS c")
            .fetch_all(pool)
            .await?;
    for chunk in &chunks {
        let deleted = with_retries(|| erase_chunk(pool, chunk, user_id, direct)).await?;
        if deleted > 0 {
            erased.scrobbles += deleted;
            erased.chunks += 1;
        }
    }

    tracing::debug!(
        user_id,
        scrobbles = erased.scrobbles,
        ms = started.elapsed().as_millis() as u64,
        "erase: scrobbles deleted"
    );
    if let Some(mut guard) = guard.take() {
        for aggregate in &aggregates {
            erased.aggregate_rows += sqlx::query(&format!(
                "DELETE FROM {} WHERE user_id = $1",
                aggregate.table
            ))
            .bind(user_id)
            .execute(&mut *guard)
            .await?
            .rows_affected();
        }
        guard.commit().await?;
    }
    tracing::debug!(
        user_id,
        rows = erased.aggregate_rows,
        ms = started.elapsed().as_millis() as u64,
        "erase: aggregates cleaned"
    );

    // Marked before and after the weights go: a refresh may run between.
    sqlx::query!(
        r#"
        UPDATE ranking_snapshots SET invalidated = TRUE
        WHERE EXISTS (SELECT 1 FROM ranking_days WHERE user_id = $1)
        "#,
        user_id,
    )
    .execute(pool)
    .await?;
    let mut weighed = 0;
    for table in [
        "scrobble_classification_days",
        "ranking_days",
        "coordination_days",
    ] {
        loop {
            let deleted = sqlx::query(&format!(
                "DELETE FROM {table} WHERE (user_id, day) IN \
                 (SELECT user_id, day FROM {table} WHERE user_id = $1 LIMIT 100)"
            ))
            .bind(user_id)
            .execute(pool)
            .await?
            .rows_affected();
            if table == "ranking_days" {
                weighed += deleted;
            }
            if deleted == 0 {
                break;
            }
        }
    }

    let mut tx = pool.begin().await?;
    erased.uploads = delete_user_row(&mut tx, user_id, weighed > 0).await?;
    tx.commit().await?;
    erased.uploads = unreferenced(pool, std::mem::take(&mut erased.uploads)).await?;
    erased.took_ms = started.elapsed().as_millis();
    Ok(erased)
}

struct Aggregate {
    /// The materialization hypertable, quoted.
    table: String,
    per_user: bool,
}

async fn scrobble_aggregates(pool: &PgPool) -> Result<Vec<Aggregate>, sqlx::Error> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        r#"
        SELECT format('%I.%I', ca.materialization_hypertable_schema,
                      ca.materialization_hypertable_name),
               EXISTS (SELECT 1 FROM information_schema.columns c
                       WHERE c.table_schema = ca.view_schema AND c.table_name = ca.view_name
                         AND c.column_name = 'user_id')
        FROM timescaledb_information.continuous_aggregates ca
        WHERE ca.hypertable_schema = 'public' AND ca.hypertable_name = 'scrobbles'
        ORDER BY 1
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(table, per_user)| Aggregate { table, per_user })
        .collect())
}

/// The user's rows in one chunk, and the counters they added to.
///
/// TimescaleDB's direct batch delete on compressed chunks logs
/// invalidations even when told to skip them, so it is turned off: the
/// user's batches are decompressed and deleted, and the compression policy
/// recompresses the chunk later.
async fn erase_chunk(
    pool: &PgPool,
    chunk: &str,
    user_id: i64,
    skip_invalidation: bool,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if skip_invalidation {
        for setting in [
            "SET LOCAL timescaledb.skip_cagg_invalidation = on",
            "SET LOCAL timescaledb.enable_compressed_direct_batch_delete = off",
            // A week of a farm's account can pass the default 100,000.
            "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0",
        ] {
            sqlx::query(setting).execute(&mut *tx).await?;
        }
    }
    let plays: Vec<Plays> = match sqlx::query_as(&format!(
        "SELECT track_id, artist_id, album_id, count(*) AS plays FROM {chunk} \
         WHERE user_id = $1 GROUP BY 1, 2, 3"
    ))
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await
    {
        Ok(plays) => plays,
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42P01") => return Ok(0),
        Err(e) => return Err(e),
    };
    if plays.is_empty() {
        return Ok(0);
    }
    let deleted = sqlx::query(&format!("DELETE FROM {chunk} WHERE user_id = $1"))
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    // Last, so ingest waits on these rows as briefly as possible.
    take_off_counters(&mut tx, &plays).await?;
    tx.commit().await?;
    Ok(deleted)
}

#[derive(sqlx::FromRow)]
struct Plays {
    track_id: i64,
    artist_id: i64,
    album_id: Option<i64>,
    plays: i64,
}

/// In the row trigger's table order, ids sorted, to wait on ingest rather
/// than deadlock with it.
async fn take_off_counters(
    tx: &mut Transaction<'_, Postgres>,
    plays: &[Plays],
) -> Result<(), sqlx::Error> {
    let sum = |key: fn(&Plays) -> Option<i64>| {
        let mut counts: HashMap<i64, i64> = HashMap::new();
        for play in plays {
            if let Some(id) = key(play) {
                *counts.entry(id).or_default() += play.plays;
            }
        }
        let mut pairs: Vec<(i64, i64)> = counts.into_iter().collect();
        pairs.sort_unstable();
        pairs.into_iter().unzip::<i64, i64, Vec<i64>, Vec<i64>>()
    };
    let (ids, counts) = sum(|p| Some(p.track_id));
    sqlx::query!(
        r#"
        UPDATE tracks t SET scrobble_count = GREATEST(t.scrobble_count - c.n, 0)
        FROM UNNEST($1::bigint[], $2::bigint[]) AS c(id, n)
        WHERE t.id = c.id
        "#,
        &ids,
        &counts,
    )
    .execute(&mut **tx)
    .await?;
    let (ids, counts) = sum(|p| Some(p.artist_id));
    sqlx::query!(
        r#"
        UPDATE artists a SET scrobble_count = GREATEST(a.scrobble_count - c.n, 0)
        FROM UNNEST($1::bigint[], $2::bigint[]) AS c(id, n)
        WHERE a.id = c.id
        "#,
        &ids,
        &counts,
    )
    .execute(&mut **tx)
    .await?;
    let (ids, counts) = sum(|p| p.album_id);
    if !ids.is_empty() {
        sqlx::query!(
            r#"
            UPDATE albums al SET scrobble_count = GREATEST(al.scrobble_count - c.n, 0)
            FROM UNNEST($1::bigint[], $2::bigint[]) AS c(id, n)
            WHERE al.id = c.id
            "#,
            &ids,
            &counts,
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Returns the uploads the user made.
async fn delete_user_row(
    tx: &mut Transaction<'_, Postgres>,
    user_id: i64,
    had_weights: bool,
) -> Result<Vec<UploadKey>, sqlx::Error> {
    sqlx::query!(
        "DELETE FROM classification_queue WHERE user_id = $1",
        user_id
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!("DELETE FROM ranking_queue WHERE user_id = $1", user_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query!(
        r#"
        UPDATE image_candidates c SET vote_count = GREATEST(c.vote_count - 1, 0)
        FROM image_candidate_votes v
        WHERE v.candidate_id = c.id AND v.user_id = $1 AND c.uploaded_by <> $1
        "#,
        user_id,
    )
    .execute(&mut **tx)
    .await?;
    // A kindless key may come from a URL the user typed: not theirs to delete.
    let avatar = sqlx::query_scalar!("SELECT image_url FROM users WHERE id = $1", user_id)
        .fetch_optional(&mut **tx)
        .await?
        .flatten()
        .and_then(|url| UploadKey::parse(&url))
        .filter(|key| key.kind() == Some(UploadKind::Avatar));
    let candidates = sqlx::query_scalar!(
        "SELECT url FROM image_candidates WHERE uploaded_by = $1",
        user_id
    )
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query!("DELETE FROM users WHERE id = $1", user_id)
        .execute(&mut **tx)
        .await?;
    if had_weights {
        sqlx::query!("UPDATE ranking_snapshots SET invalidated = TRUE")
            .execute(&mut **tx)
            .await?;
    }
    Ok(avatar
        .into_iter()
        .chain(candidates.iter().filter_map(|url| UploadKey::parse(url)))
        .collect())
}

async fn unreferenced(pool: &PgPool, keys: Vec<UploadKey>) -> Result<Vec<UploadKey>, sqlx::Error> {
    if keys.is_empty() {
        return Ok(keys);
    }
    let values: Vec<String> = keys.iter().map(|k| k.as_str().to_string()).collect();
    let referenced = crate::queries::uploads::referenced_among(pool, &values).await?;
    Ok(keys
        .into_iter()
        .filter(|k| !referenced.iter().any(|r| r == k.as_str()))
        .collect())
}

/// Retries deadlocks with ingest and imports on the counters' rows.
async fn with_retries<F, Fut, T>(mut step: F) -> Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, sqlx::Error>>,
{
    let mut attempt = 0;
    loop {
        match step().await {
            Err(sqlx::Error::Database(e))
                if attempt < 3 && matches!(e.code().as_deref(), Some("40P01" | "40001")) =>
            {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100 * attempt)).await;
            }
            result => return result,
        }
    }
}
