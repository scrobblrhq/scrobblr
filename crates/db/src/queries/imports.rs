//! Listening-history import jobs (`scrobble_imports`, migration 0011).
//!
//! A worker claims a job with a lease, then records it page by page: each
//! [`record_page`] resolves the page's catalog entries set-based, inserts
//! the scrobbles that aren't stored yet, bumps the denormalized counters
//! (the row trigger skips imported rows) and advances the job's cursor, all
//! in one transaction. Downstream work (aggregates, classification,
//! enrichment) is batched per checkpoint through the job's pending range.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{PgConnection, PgPool};
use thiserror::Error;
use uuid::Uuid;

use crate::queries::enrichment::PRIORITY_IMPORT;
use shared::import::{self as import_logic, LIVE_OVERLAP};
use shared::lastfm::Cursor;
use shared::models::{ImportStatus, ScrobbleImport};

pub const PROVIDER_LASTFM: &str = "lastfm";

#[derive(Debug, Error)]
pub enum CreateImportError {
    #[error("import #{0} is still running for this user")]
    AlreadyActive(i64),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

struct ImportRow {
    id: i64,
    provider: String,
    external_user: String,
    verified: bool,
    status: String,
    error_code: Option<String>,
    error_message: Option<String>,
    window_from: Option<DateTime<Utc>>,
    window_to: Option<DateTime<Utc>>,
    total_expected: Option<i64>,
    fetched: i64,
    imported: i64,
    duplicates: i64,
    skipped: i64,
    oldest_played_at: Option<DateTime<Utc>>,
    attempts: i32,
    next_attempt_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

impl From<ImportRow> for ScrobbleImport {
    fn from(r: ImportRow) -> Self {
        let status = ImportStatus::parse(&r.status).unwrap_or(ImportStatus::Failed);
        Self {
            id: r.id,
            provider: r.provider,
            external_user: r.external_user,
            verified: r.verified,
            status,
            error_code: r.error_code,
            error_message: r.error_message,
            window_from: r.window_from,
            window_to: r.window_to,
            total_expected: r.total_expected,
            fetched: r.fetched,
            imported: r.imported,
            duplicates: r.duplicates,
            skipped: r.skipped,
            oldest_played_at: r.oldest_played_at,
            retrying_at: (status.is_active() && r.attempts > 0).then_some(r.next_attempt_at),
            created_at: r.created_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
        }
    }
}

pub struct NewImport<'a> {
    pub user_id: i64,
    pub provider: &'a str,
    pub external_user: &'a str,
    pub verified: bool,
    pub window_from: Option<DateTime<Utc>>,
}

pub async fn create(
    pool: &PgPool,
    new: &NewImport<'_>,
) -> Result<ScrobbleImport, CreateImportError> {
    let row = sqlx::query_as!(
        ImportRow,
        r#"
        INSERT INTO scrobble_imports (user_id, provider, external_user, verified, window_from)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, provider, external_user, verified, status, error_code, error_message,
                  window_from, window_to, total_expected, fetched, imported, duplicates, skipped,
                  oldest_played_at, attempts, next_attempt_at, created_at, started_at, finished_at
        "#,
        new.user_id,
        new.provider,
        new.external_user,
        new.verified,
        new.window_from,
    )
    .fetch_one(pool)
    .await;

    match row {
        Ok(row) => Ok(row.into()),
        Err(sqlx::Error::Database(e)) if e.constraint() == Some("uq_scrobble_imports_active") => {
            let active = active_for_user(pool, new.user_id).await?;
            Err(CreateImportError::AlreadyActive(active.map_or(0, |a| a.id)))
        }
        Err(e) => Err(e.into()),
    }
}

/// Where a re-import of `external_user` should start: shortly before the
/// window of the last completed import, so late scrobbles are caught.
pub async fn reimport_from(
    pool: &PgPool,
    user_id: i64,
    provider: &str,
    external_user: &str,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let window_to = sqlx::query_scalar!(
        r#"
        SELECT max(window_to) FROM scrobble_imports
        WHERE user_id = $1 AND provider = $2 AND lower(external_user) = lower($3)
          AND status = 'done'
        "#,
        user_id,
        provider,
        external_user,
    )
    .fetch_one(pool)
    .await?;
    Ok(window_to.map(|t| t - import_logic::REIMPORT_OVERLAP))
}

/// When `user_id` last finished an import they started themselves.
pub async fn last_verified_finish(
    pool: &PgPool,
    user_id: i64,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    sqlx::query_scalar!(
        "SELECT max(finished_at) FROM scrobble_imports WHERE user_id = $1 AND verified",
        user_id,
    )
    .fetch_one(pool)
    .await
}

pub async fn active_for_user(
    pool: &PgPool,
    user_id: i64,
) -> Result<Option<ScrobbleImport>, sqlx::Error> {
    let row = sqlx::query_as!(
        ImportRow,
        r#"
        SELECT id, provider, external_user, verified, status, error_code, error_message,
               window_from, window_to, total_expected, fetched, imported, duplicates, skipped,
               oldest_played_at, attempts, next_attempt_at, created_at, started_at, finished_at
        FROM scrobble_imports
        WHERE user_id = $1 AND status IN ('pending', 'running')
        "#,
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// `(username, import)`, newest first; `user_id = None` lists everyone's
/// (operator CLI).
pub async fn list(
    pool: &PgPool,
    user_id: Option<i64>,
    limit: i64,
) -> Result<Vec<(String, ScrobbleImport)>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT u.username, i.id, i.provider, i.external_user, i.verified, i.status,
               i.error_code, i.error_message, i.window_from, i.window_to, i.total_expected,
               i.fetched, i.imported, i.duplicates, i.skipped, i.oldest_played_at, i.attempts,
               i.next_attempt_at, i.created_at, i.started_at, i.finished_at
        FROM scrobble_imports i
        JOIN users u ON u.id = i.user_id
        WHERE $1::bigint IS NULL OR i.user_id = $1
        ORDER BY i.created_at DESC, i.id DESC
        LIMIT $2
        "#,
        user_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let import = ImportRow {
                id: r.id,
                provider: r.provider,
                external_user: r.external_user,
                verified: r.verified,
                status: r.status,
                error_code: r.error_code,
                error_message: r.error_message,
                window_from: r.window_from,
                window_to: r.window_to,
                total_expected: r.total_expected,
                fetched: r.fetched,
                imported: r.imported,
                duplicates: r.duplicates,
                skipped: r.skipped,
                oldest_played_at: r.oldest_played_at,
                attempts: r.attempts,
                next_attempt_at: r.next_attempt_at,
                created_at: r.created_at,
                started_at: r.started_at,
                finished_at: r.finished_at,
            };
            (r.username, import.into())
        })
        .collect())
}

/// The import, if it exists and (when `user_id` is given) belongs to them.
pub async fn get(
    pool: &PgPool,
    id: i64,
    user_id: Option<i64>,
) -> Result<Option<ScrobbleImport>, sqlx::Error> {
    let row = sqlx::query_as!(
        ImportRow,
        r#"
        SELECT id, provider, external_user, verified, status, error_code, error_message,
               window_from, window_to, total_expected, fetched, imported, duplicates, skipped,
               oldest_played_at, attempts, next_attempt_at, created_at, started_at, finished_at
        FROM scrobble_imports
        WHERE id = $1 AND ($2::bigint IS NULL OR user_id = $2)
        "#,
        id,
        user_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Stops a pending or running import after the page in flight. Scrobbles
/// already imported stay.
pub async fn cancel(pool: &PgPool, id: i64, user_id: Option<i64>) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        UPDATE scrobble_imports
        SET status = 'cancelled', finished_at = NOW(), lease_token = NULL, leased_until = NULL
        WHERE id = $1 AND ($2::bigint IS NULL OR user_id = $2) AND status IN ('pending', 'running')
        "#,
        id,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// A job leased to one worker; every write below checks `lease_token`.
#[derive(Debug, Clone)]
pub struct ClaimedImport {
    pub id: i64,
    pub user_id: i64,
    pub external_user: String,
    pub verified: bool,
    pub window_from: Option<DateTime<Utc>>,
    pub window_to: DateTime<Utc>,
    pub cursor: Cursor,
    pub imported: i64,
    pub attempts: i32,
    pub lease_token: Uuid,
}

struct ClaimRow {
    id: i64,
    user_id: i64,
    external_user: String,
    verified: bool,
    window_from: Option<DateTime<Utc>>,
    window_to: DateTime<Utc>,
    segment_to: Option<i64>,
    segment_page: Option<i32>,
    segment_oldest: Option<i64>,
    imported: i64,
    attempts: i32,
    lease_token: Uuid,
}

impl From<ClaimRow> for ClaimedImport {
    fn from(r: ClaimRow) -> Self {
        let cursor = match (r.segment_to, r.segment_page) {
            (Some(segment_to), Some(page)) => Cursor {
                segment_to,
                page: page.max(1) as u32,
                segment_oldest: r.segment_oldest,
            },
            _ => Cursor::start(r.window_to.timestamp()),
        };
        Self {
            id: r.id,
            user_id: r.user_id,
            external_user: r.external_user,
            verified: r.verified,
            window_from: r.window_from,
            window_to: r.window_to,
            cursor,
            imported: r.imported,
            attempts: r.attempts,
            lease_token: r.lease_token,
        }
    }
}

/// Leases the due job touched least recently, so concurrent imports take
/// turns. A crashed worker's lease simply expires.
pub async fn claim_next(
    pool: &PgPool,
    lease_secs: f64,
) -> Result<Option<ClaimedImport>, sqlx::Error> {
    let row = sqlx::query_as!(
        ClaimRow,
        r#"
        UPDATE scrobble_imports
        SET status       = 'running',
            started_at   = COALESCE(started_at, NOW()),
            window_to    = COALESCE(window_to, NOW()),
            lease_token  = $2,
            leased_until = NOW() + make_interval(secs => $1)
        WHERE id = (
            SELECT id FROM scrobble_imports
            WHERE status IN ('pending', 'running') AND next_attempt_at <= NOW()
              AND (leased_until IS NULL OR leased_until < NOW())
            ORDER BY updated_at, id
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING id, user_id, external_user, verified, window_from,
                  window_to AS "window_to!", segment_to, segment_page, segment_oldest,
                  imported, attempts, lease_token AS "lease_token!"
        "#,
        lease_secs,
        Uuid::new_v4(),
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// Leases one specific job, ignoring its retry delay (operator CLI).
pub async fn claim(
    pool: &PgPool,
    id: i64,
    lease_secs: f64,
) -> Result<Option<ClaimedImport>, sqlx::Error> {
    let row = sqlx::query_as!(
        ClaimRow,
        r#"
        UPDATE scrobble_imports
        SET status       = 'running',
            started_at   = COALESCE(started_at, NOW()),
            window_to    = COALESCE(window_to, NOW()),
            lease_token  = $3,
            leased_until = NOW() + make_interval(secs => $2)
        WHERE id = $1 AND status IN ('pending', 'running')
          AND (leased_until IS NULL OR leased_until < NOW())
        RETURNING id, user_id, external_user, verified, window_from,
                  window_to AS "window_to!", segment_to, segment_page, segment_oldest,
                  imported, attempts, lease_token AS "lease_token!"
        "#,
        id,
        lease_secs,
        Uuid::new_v4(),
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn release(pool: &PgPool, job: &ClaimedImport) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE scrobble_imports SET lease_token = NULL, leased_until = NULL WHERE id = $1 AND lease_token = $2",
        job.id,
        job.lease_token,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Backs off after a provider failure; the job stays running.
pub async fn defer(
    pool: &PgPool,
    job: &ClaimedImport,
    attempts: i32,
    delay: TimeDelta,
    error: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE scrobble_imports
        SET attempts = $3, next_attempt_at = NOW() + make_interval(secs => $4),
            error_message = $5, lease_token = NULL, leased_until = NULL
        WHERE id = $1 AND lease_token = $2
        "#,
        job.id,
        job.lease_token,
        attempts,
        delay.as_seconds_f64(),
        error,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn fail(
    pool: &PgPool,
    job: &ClaimedImport,
    code: &str,
    message: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE scrobble_imports
        SET status = 'failed', error_code = $3, error_message = $4, finished_at = NOW(),
            lease_token = NULL, leased_until = NULL
        WHERE id = $1 AND lease_token = $2
        "#,
        job.id,
        job.lease_token,
        code,
        message,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn finish(pool: &PgPool, job: &ClaimedImport) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE scrobble_imports
        SET status = 'done', error_code = NULL, error_message = NULL, finished_at = NOW(),
            lease_token = NULL, leased_until = NULL
        WHERE id = $1 AND lease_token = $2
        "#,
        job.id,
        job.lease_token,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// One scrobble to import, as the provider reported it.
#[derive(Debug, Clone)]
pub struct ImportPlay {
    pub played_at: DateTime<Utc>,
    pub artist: String,
    pub track: String,
    pub album: Option<String>,
    pub track_mbid: Option<Uuid>,
}

/// A fetched page: its plays plus what the cursor and progress become.
pub struct Page<'a> {
    pub plays: &'a [ImportPlay],
    pub next: Cursor,
    /// Dated scrobbles on the page, valid or not.
    pub fetched: i64,
    pub skipped: i64,
    pub total_expected: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageOutcome {
    Recorded {
        imported: i64,
        duplicates: i64,
    },
    /// The job was cancelled or its lease taken over; nothing was written.
    LeaseLost,
}

fn normalize(name: &str) -> String {
    name.trim().to_lowercase()
}

pub async fn record_page(
    pool: &PgPool,
    job: &ClaimedImport,
    page: &Page<'_>,
    lease_secs: f64,
) -> Result<PageOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let leased = sqlx::query_scalar!(
        r#"
        SELECT id FROM scrobble_imports
        WHERE id = $1 AND lease_token = $2 AND status = 'running'
        FOR UPDATE
        "#,
        job.id,
        job.lease_token,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if leased.is_none() {
        tx.rollback().await?;
        return Ok(PageOutcome::LeaseLost);
    }

    let inserted = if page.plays.is_empty() {
        Vec::new()
    } else {
        insert_plays(&mut tx, job, page.plays).await?
    };
    let imported = inserted.len() as i64;
    let duplicates = page.plays.len() as i64 - imported;

    sqlx::query!(
        r#"
        UPDATE scrobble_imports SET
            segment_to       = $2,
            segment_page     = $3,
            segment_oldest   = $4,
            total_expected   = COALESCE(total_expected, $5),
            fetched          = fetched + $6,
            imported         = imported + $7,
            duplicates       = duplicates + $8,
            skipped          = skipped + $9,
            oldest_played_at = LEAST(oldest_played_at, $10),
            pending_from     = LEAST(pending_from, $11),
            pending_to       = GREATEST(pending_to, $12),
            attempts         = 0,
            error_message    = NULL,
            leased_until     = NOW() + make_interval(secs => $13)
        WHERE id = $1
        "#,
        job.id,
        page.next.segment_to,
        page.next.page as i32,
        page.next.segment_oldest,
        page.total_expected,
        page.fetched,
        imported,
        duplicates,
        page.skipped,
        page.plays.iter().map(|p| p.played_at).min(),
        inserted.iter().min().copied(),
        inserted.iter().max().copied(),
        lease_secs,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(PageOutcome::Recorded {
        imported,
        duplicates,
    })
}

/// Resolves the plays' catalog entries, inserts the ones not already
/// stored and updates the counters. Returns the inserted `played_at`s.
async fn insert_plays(
    tx: &mut PgConnection,
    job: &ClaimedImport,
    plays: &[ImportPlay],
) -> Result<Vec<DateTime<Utc>>, sqlx::Error> {
    let artists = upsert_artists(tx, plays).await?;
    let albums = upsert_albums(tx, plays, &artists).await?;
    let tracks = upsert_tracks(tx, plays, &artists, &albums).await?;

    struct Resolved {
        played_at: DateTime<Utc>,
        track_id: i64,
        artist_id: i64,
        album_id: Option<i64>,
    }
    let resolved: Vec<Resolved> = plays
        .iter()
        .map(|p| {
            let artist_id = artists[&normalize(&p.artist)];
            Resolved {
                played_at: p.played_at,
                track_id: tracks[&(artist_id, normalize(&p.track))],
                artist_id,
                album_id: p.album.as_ref().map(|a| albums[&(artist_id, normalize(a))]),
            }
        })
        .collect();

    let earliest = resolved
        .iter()
        .map(|r| r.played_at)
        .min()
        .unwrap_or_default();
    let latest = resolved
        .iter()
        .map(|r| r.played_at)
        .max()
        .unwrap_or_default();
    let stored: Vec<import_logic::Stored> = sqlx::query!(
        r#"
        SELECT played_at, track_id, import_id IS NOT NULL AS "imported!"
        FROM scrobbles
        WHERE user_id = $1 AND played_at BETWEEN $2 AND $3
        "#,
        job.user_id,
        earliest - LIVE_OVERLAP,
        latest + LIVE_OVERLAP,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| import_logic::Stored {
        played_at: r.played_at,
        track_id: r.track_id,
        imported: r.imported,
    })
    .collect();

    let incoming: Vec<import_logic::Play> = resolved
        .iter()
        .map(|r| import_logic::Play {
            played_at: r.played_at,
            track_id: r.track_id,
        })
        .collect();
    let fresh: Vec<&Resolved> = import_logic::new_plays(&incoming, &stored, LIVE_OVERLAP)
        .into_iter()
        .map(|i| &resolved[i])
        .collect();
    if fresh.is_empty() {
        return Ok(Vec::new());
    }

    let track_ids: Vec<i64> = fresh.iter().map(|r| r.track_id).collect();
    let artist_ids: Vec<i64> = fresh.iter().map(|r| r.artist_id).collect();
    let album_ids: Vec<i64> = fresh.iter().map(|r| r.album_id.unwrap_or(0)).collect();
    let played_at: Vec<DateTime<Utc>> = fresh.iter().map(|r| r.played_at).collect();
    sqlx::query!(
        r#"
        INSERT INTO scrobbles (user_id, track_id, artist_id, album_id, played_at, source, import_id)
        SELECT $1, i.track_id, i.artist_id, NULLIF(i.album_id, 0), i.played_at, $2, $3
        FROM UNNEST($4::bigint[], $5::bigint[], $6::bigint[], $7::timestamptz[])
             AS i(track_id, artist_id, album_id, played_at)
        "#,
        job.user_id,
        import_logic::LASTFM_SOURCE,
        job.id,
        &track_ids,
        &artist_ids,
        &album_ids,
        &played_at,
    )
    .execute(&mut *tx)
    .await?;

    // Same table order as the row trigger (tracks, artists, users, albums),
    // so a live scrobble's transaction can't deadlock with this one.
    let (ids, counts) = tally(&track_ids);
    sqlx::query!(
        r#"
        UPDATE tracks t SET scrobble_count = t.scrobble_count + c.n
        FROM UNNEST($1::bigint[], $2::bigint[]) AS c(id, n)
        WHERE t.id = c.id
        "#,
        &ids,
        &counts,
    )
    .execute(&mut *tx)
    .await?;
    let (ids, counts) = tally(&artist_ids);
    sqlx::query!(
        r#"
        UPDATE artists a SET scrobble_count = a.scrobble_count + c.n
        FROM UNNEST($1::bigint[], $2::bigint[]) AS c(id, n)
        WHERE a.id = c.id
        "#,
        &ids,
        &counts,
    )
    .execute(&mut *tx)
    .await?;
    // GREATEST, not the trigger's plain assignment: history is older than
    // whatever the user scrobbled last.
    sqlx::query!(
        r#"
        UPDATE users
        SET scrobble_count = scrobble_count + $2,
            last_seen_at   = GREATEST(last_seen_at, $3)
        WHERE id = $1
        "#,
        job.user_id,
        played_at.len() as i64,
        played_at.iter().max().copied(),
    )
    .execute(&mut *tx)
    .await?;
    let albums_played: Vec<i64> = album_ids.iter().copied().filter(|id| *id != 0).collect();
    if !albums_played.is_empty() {
        let (ids, counts) = tally(&albums_played);
        sqlx::query!(
            r#"
            UPDATE albums al SET scrobble_count = al.scrobble_count + c.n
            FROM UNNEST($1::bigint[], $2::bigint[]) AS c(id, n)
            WHERE al.id = c.id
            "#,
            &ids,
            &counts,
        )
        .execute(&mut *tx)
        .await?;
    }

    Ok(played_at)
}

/// Sorted distinct ids with their counts; sorted so concurrent batches
/// lock rows in the same order.
fn tally(ids: &[i64]) -> (Vec<i64>, Vec<i64>) {
    let mut counts: HashMap<i64, i64> = HashMap::new();
    for id in ids {
        *counts.entry(*id).or_default() += 1;
    }
    let mut pairs: Vec<(i64, i64)> = counts.into_iter().collect();
    pairs.sort_unstable();
    pairs.into_iter().unzip()
}

/// First spelling per normalized key, in page order (first one wins, as with
/// live ingest).
fn distinct<K: std::hash::Hash + Eq + Clone, V>(
    items: impl Iterator<Item = (K, V)>,
) -> Vec<(K, V)> {
    let mut seen = std::collections::HashSet::new();
    items.filter(|(k, _)| seen.insert(k.clone())).collect()
}

async fn upsert_artists(
    tx: &mut PgConnection,
    plays: &[ImportPlay],
) -> Result<HashMap<String, i64>, sqlx::Error> {
    let wanted = distinct(
        plays
            .iter()
            .map(|p| (normalize(&p.artist), p.artist.trim().to_string())),
    );
    let names: Vec<String> = wanted.iter().map(|(_, n)| n.clone()).collect();
    let keys: Vec<String> = wanted.iter().map(|(k, _)| k.clone()).collect();

    // Rows a concurrent ingest commits mid-statement are neither inserted
    // nor visible to the statement's snapshot; the loop's second pass reads
    // them with a fresh one.
    let mut found = HashMap::new();
    for _ in 0..2 {
        let rows = sqlx::query!(
            r#"
            WITH input AS (
                SELECT * FROM UNNEST($1::text[], $2::text[]) AS i(name, name_normalized)
            ), inserted AS (
                INSERT INTO artists (name, name_normalized)
                SELECT name, name_normalized FROM input
                ON CONFLICT (name_normalized) DO NOTHING
                RETURNING id, name_normalized
            )
            SELECT id AS "id!", name_normalized AS "name_normalized!" FROM inserted
            UNION ALL
            SELECT a.id, a.name_normalized FROM artists a JOIN input i USING (name_normalized)
            "#,
            &names,
            &keys,
        )
        .fetch_all(&mut *tx)
        .await?;
        found.extend(rows.into_iter().map(|r| (r.name_normalized, r.id)));
        if keys.iter().all(|k| found.contains_key(k)) {
            return Ok(found);
        }
    }
    Err(sqlx::Error::RowNotFound)
}

async fn upsert_albums(
    tx: &mut PgConnection,
    plays: &[ImportPlay],
    artists: &HashMap<String, i64>,
) -> Result<HashMap<(i64, String), i64>, sqlx::Error> {
    let wanted = distinct(plays.iter().filter_map(|p| {
        let title = p.album.as_ref()?;
        Some((
            (artists[&normalize(&p.artist)], normalize(title)),
            title.trim().to_string(),
        ))
    }));
    let mut found = HashMap::new();
    if wanted.is_empty() {
        return Ok(found);
    }
    let artist_ids: Vec<i64> = wanted.iter().map(|((a, _), _)| *a).collect();
    let titles: Vec<String> = wanted.iter().map(|(_, t)| t.clone()).collect();
    let keys: Vec<String> = wanted.iter().map(|((_, k), _)| k.clone()).collect();

    for _ in 0..2 {
        let rows = sqlx::query!(
            r#"
            WITH input AS (
                SELECT * FROM UNNEST($1::bigint[], $2::text[], $3::text[])
                    AS i(artist_id, title, title_normalized)
            ), inserted AS (
                INSERT INTO albums (artist_id, title, title_normalized)
                SELECT artist_id, title, title_normalized FROM input
                ON CONFLICT (artist_id, title_normalized) DO NOTHING
                RETURNING id, artist_id, title_normalized
            )
            SELECT id AS "id!", artist_id AS "artist_id!", title_normalized AS "title_normalized!"
            FROM inserted
            UNION ALL
            SELECT al.id, al.artist_id, al.title_normalized
            FROM albums al JOIN input i USING (artist_id, title_normalized)
            "#,
            &artist_ids,
            &titles,
            &keys,
        )
        .fetch_all(&mut *tx)
        .await?;
        found.extend(
            rows.into_iter()
                .map(|r| ((r.artist_id, r.title_normalized), r.id)),
        );
        if wanted.iter().all(|(k, _)| found.contains_key(k)) {
            return Ok(found);
        }
    }
    Err(sqlx::Error::RowNotFound)
}

async fn upsert_tracks(
    tx: &mut PgConnection,
    plays: &[ImportPlay],
    artists: &HashMap<String, i64>,
    albums: &HashMap<(i64, String), i64>,
) -> Result<HashMap<(i64, String), i64>, sqlx::Error> {
    let wanted = distinct(plays.iter().map(|p| {
        let artist_id = artists[&normalize(&p.artist)];
        let album_id = p.album.as_ref().map(|a| albums[&(artist_id, normalize(a))]);
        (
            (artist_id, normalize(&p.track)),
            (p.track.trim().to_string(), album_id, p.track_mbid),
        )
    }));
    let artist_ids: Vec<i64> = wanted.iter().map(|((a, _), _)| *a).collect();
    let titles: Vec<String> = wanted.iter().map(|(_, (t, _, _))| t.clone()).collect();
    let keys: Vec<String> = wanted.iter().map(|((_, k), _)| k.clone()).collect();
    let album_ids: Vec<i64> = wanted.iter().map(|(_, (_, a, _))| a.unwrap_or(0)).collect();
    let hints: Vec<String> = wanted
        .iter()
        .map(|(_, (_, _, h))| h.map(|h| h.to_string()).unwrap_or_default())
        .collect();

    let mut found = HashMap::new();
    let mut created = Vec::new();
    for _ in 0..2 {
        let rows = sqlx::query!(
            r#"
            WITH input AS (
                SELECT * FROM UNNEST($1::bigint[], $2::text[], $3::text[], $4::bigint[], $5::text[])
                    AS i(artist_id, title, title_normalized, album_id, hint)
            ), inserted AS (
                INSERT INTO tracks (artist_id, album_id, title, title_normalized, mbid_hint)
                SELECT artist_id, NULLIF(album_id, 0), title, title_normalized,
                       NULLIF(hint, '')::uuid
                FROM input
                ON CONFLICT (artist_id, title_normalized) DO NOTHING
                RETURNING id, artist_id, title_normalized
            )
            SELECT id AS "id!", artist_id AS "artist_id!", title_normalized AS "title_normalized!",
                   TRUE AS "created!"
            FROM inserted
            UNION ALL
            SELECT t.id, t.artist_id, t.title_normalized, FALSE
            FROM tracks t JOIN input i USING (artist_id, title_normalized)
            "#,
            &artist_ids,
            &titles,
            &keys,
            &album_ids,
            &hints,
        )
        .fetch_all(&mut *tx)
        .await?;
        for r in rows {
            if r.created {
                created.push((r.id, r.artist_id));
            }
            found.insert((r.artist_id, r.title_normalized), r.id);
        }
        if wanted.iter().all(|(k, _)| found.contains_key(k)) {
            break;
        }
    }
    if !wanted.iter().all(|(k, _)| found.contains_key(k)) {
        return Err(sqlx::Error::RowNotFound);
    }

    // Like find_or_create_track: an existing track only gains what it lacks.
    let ids: Vec<i64> = wanted.iter().map(|(k, _)| found[k]).collect();
    sqlx::query!(
        r#"
        UPDATE tracks t
        SET album_id  = COALESCE(t.album_id, NULLIF(i.album_id, 0)),
            mbid_hint = CASE WHEN t.mbid IS NULL THEN COALESCE(t.mbid_hint, NULLIF(i.hint, '')::uuid)
                             ELSE t.mbid_hint END
        FROM UNNEST($1::bigint[], $2::bigint[], $3::text[]) AS i(id, album_id, hint)
        WHERE t.id = i.id
          AND ((t.album_id IS NULL AND i.album_id <> 0)
            OR (t.mbid IS NULL AND t.mbid_hint IS NULL AND i.hint <> ''))
        "#,
        &ids,
        &album_ids,
        &hints,
    )
    .execute(&mut *tx)
    .await?;

    // New tracks need their primary credit (track_artists invariant).
    if !created.is_empty() {
        let (track_ids, artist_ids): (Vec<i64>, Vec<i64>) = created.into_iter().unzip();
        sqlx::query!(
            r#"
            INSERT INTO track_artists (track_id, artist_id, role, position)
            SELECT c.track_id, c.artist_id, 'primary', 0
            FROM UNNEST($1::bigint[], $2::bigint[]) AS c(track_id, artist_id)
            ON CONFLICT DO NOTHING
            "#,
            &track_ids,
            &artist_ids,
        )
        .execute(&mut *tx)
        .await?;
    }

    Ok(found)
}

/// Imported but not yet checkpointed, if anything.
pub async fn pending_range(
    pool: &PgPool,
    id: i64,
) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>, sqlx::Error> {
    let row = sqlx::query!(
        "SELECT pending_from, pending_to FROM scrobble_imports WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| r.pending_from.zip(r.pending_to)))
}

/// Clears the pending range if it is still the one just checkpointed.
pub async fn clear_pending(
    pool: &PgPool,
    id: i64,
    (from, to): (DateTime<Utc>, DateTime<Utc>),
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE scrobble_imports SET pending_from = NULL, pending_to = NULL
        WHERE id = $1 AND pending_from = $2 AND pending_to = $3
        "#,
        id,
        from,
        to,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Finished, failed or cancelled jobs whose last pages never reached a
/// checkpoint (e.g. cancelled mid-segment): `(id, user_id)`.
pub async fn unfinished_checkpoints(pool: &PgPool) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT id, user_id FROM scrobble_imports
        WHERE status IN ('done', 'failed', 'cancelled') AND pending_from IS NOT NULL
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.user_id)).collect())
}

/// Queues enrichment for the never-enriched tracks, artists and albums the
/// import touched in `[from, to]`, below live ingest and most-played first.
pub async fn enqueue_enrichment(
    pool: &PgPool,
    import_id: i64,
    user_id: i64,
    (from, to): (DateTime<Utc>, DateTime<Utc>),
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        r#"
        WITH plays AS (
            SELECT track_id, artist_id, album_id FROM scrobbles
            WHERE user_id = $1 AND import_id = $2 AND played_at BETWEEN $3 AND $4
        ), counts AS (
            SELECT 'track' AS entity_type, track_id AS entity_id, count(*) AS n
            FROM plays GROUP BY track_id
            UNION ALL
            SELECT 'artist', artist_id, count(*) FROM plays GROUP BY artist_id
            UNION ALL
            SELECT 'album', album_id, count(*) FROM plays WHERE album_id IS NOT NULL GROUP BY album_id
        )
        INSERT INTO enrichment_jobs (entity_type, entity_id, priority)
        SELECT c.entity_type, c.entity_id,
               $5 + LEAST(9, floor(log(2, c.n::numeric)))::int
        FROM counts c
        WHERE (c.entity_type = 'track'
               AND EXISTS (SELECT 1 FROM tracks t WHERE t.id = c.entity_id AND t.enriched_at IS NULL))
           OR (c.entity_type = 'artist'
               AND EXISTS (SELECT 1 FROM artists a WHERE a.id = c.entity_id AND a.enriched_at IS NULL))
           OR (c.entity_type = 'album'
               AND EXISTS (SELECT 1 FROM albums al WHERE al.id = c.entity_id AND al.enriched_at IS NULL))
        ON CONFLICT (entity_type, entity_id) DO UPDATE
            SET priority = GREATEST(enrichment_jobs.priority, EXCLUDED.priority)
            WHERE enrichment_jobs.status = 'pending'
        "#,
        user_id,
        import_id,
        from,
        to,
        PRIORITY_IMPORT,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// `(imported scrobbles, those whose track has a known length)`.
pub async fn length_coverage(
    pool: &PgPool,
    import_id: i64,
    user_id: i64,
) -> Result<(i64, i64), sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT count(*) AS "total!",
               count(*) FILTER (WHERE t.mb_duration_ms > 0 OR t.duration_ms > 0) AS "with_length!"
        FROM scrobbles s
        JOIN tracks t ON t.id = s.track_id
        WHERE s.user_id = $1 AND s.import_id = $2
        "#,
        user_id,
        import_id,
    )
    .fetch_one(pool)
    .await?;
    Ok((row.total, row.with_length))
}
