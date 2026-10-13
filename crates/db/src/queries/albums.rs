//! Which album a track belongs to (docs/social.md, Albums): accounts'
//! votes, the queue of tracks to decide, and what the worker decides from
//! (`shared::albums`).

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use sqlx::{PgConnection, PgExecutor, PgPool};

/// A track with new votes waits this long, so a burst of them costs one
/// decision.
pub const SETTLE: TimeDelta = TimeDelta::seconds(30);

/// Records the album `user_id` played `track_id` from. It becomes the
/// account's vote unless a later play named another; a changed vote queues
/// the track. Reads before writing: the usual case, the same album again,
/// writes nothing.
pub async fn record_vote(
    pool: &PgPool,
    user_id: i64,
    track_id: i64,
    album_id: i64,
    played_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        WITH current AS (
            SELECT album_id, played_at FROM track_album_votes
            WHERE track_id = $1 AND user_id = $2
        ), inserted AS (
            INSERT INTO track_album_votes (track_id, user_id, album_id, played_at)
            SELECT $1, $2, $3, $4
            WHERE NOT EXISTS (SELECT 1 FROM current)
            ON CONFLICT (track_id, user_id) DO NOTHING
            RETURNING track_id
        ), moved AS (
            UPDATE track_album_votes SET album_id = $3, played_at = $4
            WHERE track_id = $1 AND user_id = $2
              AND EXISTS (SELECT 1 FROM current WHERE album_id <> $3 AND played_at < $4)
            RETURNING track_id
        )
        INSERT INTO album_attribution_queue (track_id, due_at)
        SELECT track_id, NOW() + make_interval(secs => $5)
        FROM (SELECT track_id FROM inserted UNION ALL SELECT track_id FROM moved) changed
        ON CONFLICT (track_id) DO UPDATE SET due_at = EXCLUDED.due_at
            WHERE album_attribution_queue.due_at > EXCLUDED.due_at
        "#,
        track_id,
        user_id,
        album_id,
        played_at,
        SETTLE.num_seconds() as f64,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// [`record_vote`] for a page of imported plays (`album_ids` 0 for none),
/// the latest play of each track voting.
pub async fn record_votes(
    tx: &mut PgConnection,
    user_id: i64,
    track_ids: &[i64],
    album_ids: &[i64],
    played_at: &[DateTime<Utc>],
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        WITH latest AS (
            SELECT DISTINCT ON (track_id) track_id, album_id, played_at
            FROM UNNEST($2::bigint[], $3::bigint[], $4::timestamptz[]) AS p(track_id, album_id, played_at)
            WHERE album_id <> 0
            ORDER BY track_id, played_at DESC
        ), changed AS (
            INSERT INTO track_album_votes (track_id, user_id, album_id, played_at)
            SELECT track_id, $1, album_id, played_at FROM latest
            ON CONFLICT (track_id, user_id) DO UPDATE
                SET album_id = EXCLUDED.album_id, played_at = EXCLUDED.played_at
                WHERE track_album_votes.album_id <> EXCLUDED.album_id
                  AND track_album_votes.played_at < EXCLUDED.played_at
            RETURNING track_id
        )
        INSERT INTO album_attribution_queue (track_id, due_at)
        SELECT track_id, NOW() + make_interval(secs => $5) FROM changed ORDER BY track_id
        ON CONFLICT (track_id) DO UPDATE SET due_at = EXCLUDED.due_at
            WHERE album_attribution_queue.due_at > EXCLUDED.due_at
        "#,
        user_id,
        track_ids,
        album_ids,
        played_at,
        SETTLE.num_seconds() as f64,
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Queues every track `user_id` voted on, for when its votes start or stop
/// counting (a deletion, a restore, an erase).
pub async fn enqueue_voted(executor: impl PgExecutor<'_>, user_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO album_attribution_queue (track_id, due_at)
        SELECT track_id, NOW() FROM track_album_votes WHERE user_id = $1 ORDER BY track_id
        ON CONFLICT (track_id) DO UPDATE SET due_at = EXCLUDED.due_at
            WHERE album_attribution_queue.due_at > EXCLUDED.due_at
        "#,
        user_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Queues `track_ids` to be decided at `due_at`, unless they are due
/// earlier already.
pub async fn enqueue(
    executor: impl PgExecutor<'_>,
    track_ids: &[i64],
    due_at: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO album_attribution_queue (track_id, due_at)
        SELECT id, $2 FROM UNNEST($1::bigint[]) AS id ORDER BY id
        ON CONFLICT (track_id) DO UPDATE SET due_at = EXCLUDED.due_at
            WHERE album_attribution_queue.due_at > EXCLUDED.due_at
        "#,
        track_ids,
        due_at,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Up to `limit` tracks due, locked until the transaction ends.
pub async fn claim_due(tx: &mut PgConnection, limit: i64) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT track_id FROM album_attribution_queue
        WHERE due_at <= NOW()
        ORDER BY due_at
        LIMIT $1
        FOR UPDATE SKIP LOCKED
        "#,
        limit,
    )
    .fetch_all(&mut *tx)
    .await
}

pub async fn dequeue(tx: &mut PgConnection, track_ids: &[i64]) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "DELETE FROM album_attribution_queue WHERE track_id = ANY($1)",
        track_ids,
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// An album a track was played from, with the weight of the accounts
/// behind it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CandidateRow {
    pub track_id: i64,
    pub album_id: i64,
    /// Thousandths of an account: an account old enough and not waiting to
    /// be erased weighs its lowest coordination factor since `factor_since`.
    pub weight: i64,
    pub accounts: i64,
    /// MusicBrainz lists the recording on a release of this title.
    pub confirmed: bool,
    /// The earliest an account too young to count was created.
    pub youngest_since: Option<DateTime<Utc>>,
}

/// Who counts in a decision.
#[derive(Debug, Clone, Copy)]
pub struct Eligibility {
    /// Accounts created later don't count yet.
    pub created_before: DateTime<Utc>,
    /// A flagged account's coordination days from this one on.
    pub factor_since: NaiveDate,
}

/// The candidates of `track_ids`, from `votes`: `track_album_votes`, or a
/// table of the same shape on this connection (the backfill's).
pub async fn candidates(
    conn: &mut PgConnection,
    votes: &str,
    track_ids: &[i64],
    who: Eligibility,
) -> Result<Vec<CandidateRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        r#"
        SELECT v.track_id, v.album_id,
               sum(CASE WHEN u.created_at <= $2 AND u.delete_after IS NULL
                        THEN COALESCE(c.factor, $3) ELSE 0 END)::bigint AS weight,
               count(*) AS accounts,
               COALESCE(al.title_normalized = ANY(t.mb_release_titles), FALSE) AS confirmed,
               min(u.created_at) FILTER (WHERE u.created_at > $2 AND u.delete_after IS NULL)
                   AS youngest_since
        FROM {votes} v
        JOIN users u ON u.id = v.user_id
        JOIN albums al ON al.id = v.album_id
        JOIN tracks t ON t.id = v.track_id
        LEFT JOIN LATERAL (
            SELECT min(d.factor)::bigint AS factor FROM coordination_days d
            WHERE d.user_id = v.user_id AND d.day >= $4
        ) c ON TRUE
        WHERE v.track_id = ANY($1)
        GROUP BY v.track_id, v.album_id, al.title_normalized, t.mb_release_titles
        "#
    ))
    .bind(track_ids)
    .bind(who.created_before)
    .bind(shared::ranking::FULL)
    .bind(who.factor_since)
    .fetch_all(&mut *conn)
    .await
}

/// The albums `track_ids` belong to now.
pub async fn current(
    conn: &mut PgConnection,
    track_ids: &[i64],
) -> Result<Vec<(i64, Option<i64>)>, sqlx::Error> {
    let rows = sqlx::query!(
        "SELECT id, album_id FROM tracks WHERE id = ANY($1)",
        track_ids,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.album_id)).collect())
}

/// A track moving to another album.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move {
    pub track_id: i64,
    pub from: Option<i64>,
    pub to: i64,
}

/// Moves each track whose album is still `from`; how many moved.
pub async fn apply(conn: &mut PgConnection, moves: &[Move]) -> Result<u64, sqlx::Error> {
    let mut moves = moves.to_vec();
    moves.sort_by_key(|m| m.track_id);
    let ids: Vec<i64> = moves.iter().map(|m| m.track_id).collect();
    let from: Vec<i64> = moves.iter().map(|m| m.from.unwrap_or(0)).collect();
    let to: Vec<i64> = moves.iter().map(|m| m.to).collect();
    Ok(sqlx::query!(
        r#"
        UPDATE tracks t SET album_id = m.to_id
        FROM UNNEST($1::bigint[], $2::bigint[], $3::bigint[]) AS m(id, from_id, to_id)
        WHERE t.id = m.id AND t.album_id IS NOT DISTINCT FROM NULLIF(m.from_id, 0)
        "#,
        &ids,
        &from,
        &to,
    )
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Stores the release titles MusicBrainz lists `mbid`'s recording on, if
/// it is still the track's, and queues the track when they changed.
pub async fn record_releases(
    pool: &PgPool,
    track_id: i64,
    mbid: uuid::Uuid,
    titles: &[String],
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        WITH stored AS (
            UPDATE tracks SET mb_release_titles = $3
            WHERE id = $1 AND mbid = $2 AND mb_release_titles IS DISTINCT FROM $3
            RETURNING id
        )
        INSERT INTO album_attribution_queue (track_id, due_at)
        SELECT id, NOW() FROM stored
        ON CONFLICT (track_id) DO UPDATE SET due_at = EXCLUDED.due_at
            WHERE album_attribution_queue.due_at > EXCLUDED.due_at
        "#,
        track_id,
        mbid,
        titles,
    )
    .execute(pool)
    .await?;
    Ok(())
}
