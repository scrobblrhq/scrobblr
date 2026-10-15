//! Reports and what an operator does about them (migration 0039;
//! `worker moderation`, docs/operations.md): reverting and locking bios,
//! hiding revisions, deciding held edits, suspending accounts from editing
//! and tagging, taking tags off and blocking them.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};

use crate::queries::bios::set_current;
use crate::queries::tags;
use shared::community;
use shared::models::CatalogKind;

/// What a report is about.
#[derive(Debug, Clone, Copy)]
pub enum Target {
    Revision(i64),
    /// A tag, on one entity or anywhere.
    Tag {
        tag_id: i64,
        on: Option<(CatalogKind, i64)>,
    },
}

/// Files a report; `false` when the reporter has one open on the target.
pub async fn report(
    pool: &PgPool,
    reporter_id: i64,
    target: Target,
    reason: &str,
    note: &str,
) -> Result<bool, sqlx::Error> {
    let (revision, tag, on) = match target {
        Target::Revision(id) => (Some(id), None, None),
        Target::Tag { tag_id, on } => (None, Some(tag_id), on),
    };
    Ok(sqlx::query!(
        r#"
        INSERT INTO reports (reporter_id, revision_id, tag_id, entity_type, entity_id, reason, note)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT DO NOTHING
        "#,
        reporter_id,
        revision,
        tag,
        on.map(|(k, _)| k.as_str()),
        on.map(|(_, id)| id),
        reason,
        note,
    )
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

#[derive(Debug)]
pub struct OpenReport {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub reporter: String,
    pub reason: String,
    pub note: String,
    pub revision_id: Option<i64>,
    pub tag: Option<String>,
    pub entity_type: Option<String>,
    pub entity_id: Option<i64>,
    /// The revision's author, or `NULL`.
    pub author: Option<String>,
    pub excerpt: Option<String>,
    /// Open reports on the same target.
    pub on_target: i64,
}

/// Open reports, oldest first.
pub async fn open_reports(pool: &PgPool, limit: i64) -> Result<Vec<OpenReport>, sqlx::Error> {
    sqlx::query_as!(
        OpenReport,
        r#"
        SELECT r.id, r.created_at, u.username AS reporter, r.reason, r.note, r.revision_id,
               t.name AS "tag?",
               COALESCE(r.entity_type, v.entity_type) AS entity_type,
               COALESCE(r.entity_id, v.entity_id) AS entity_id,
               a.username AS "author?",
               left(regexp_replace(v.body, '\s+', ' ', 'g'), 120) AS excerpt,
               count(*) OVER (PARTITION BY r.revision_id, r.tag_id, r.entity_type, r.entity_id)
                   AS "on_target!"
        FROM reports r
        JOIN users u ON u.id = r.reporter_id
        LEFT JOIN tags t ON t.id = r.tag_id
        LEFT JOIN bio_revisions v ON v.id = r.revision_id
        LEFT JOIN users a ON a.id = v.author_id
        WHERE r.status = 'open'
        ORDER BY r.created_at
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Closes open reports as resolved (or dismissed): how many.
pub async fn resolve(
    pool: &PgPool,
    ids: &[i64],
    dismissed: bool,
    resolution: &str,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        r#"
        UPDATE reports SET status = $2, resolution = $3, resolved_at = NOW()
        WHERE id = ANY($1) AND status = 'open'
        "#,
        ids,
        if dismissed { "dismissed" } else { "resolved" },
        resolution,
    )
    .execute(pool)
    .await?
    .rows_affected())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Moderated {
    Done,
    /// No such revision of that bio, or it isn't published and visible.
    NoSuchRevision,
    /// The held edit was written over a revision that isn't current now.
    Stale,
}

/// A moderator's revision restoring `to`'s text (`None`: no text), over
/// whatever the bio holds, lock or not.
async fn restore(
    conn: &mut PgConnection,
    kind: CatalogKind,
    entity_id: i64,
    to: Option<i64>,
    summary: &str,
) -> Result<Moderated, sqlx::Error> {
    sqlx::query!(
        "INSERT INTO entity_bios (entity_type, entity_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        kind.as_str(),
        entity_id,
    )
    .execute(&mut *conn)
    .await?;
    let head = sqlx::query_scalar!(
        "SELECT revision_id FROM entity_bios WHERE entity_type = $1 AND entity_id = $2 FOR UPDATE",
        kind.as_str(),
        entity_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    let Some(to) = to else {
        set_current(conn, kind, entity_id, None).await?;
        return Ok(Moderated::Done);
    };
    let Some(body) = sqlx::query_scalar!(
        r#"
        SELECT body FROM bio_revisions
        WHERE id = $1 AND entity_type = $2 AND entity_id = $3
          AND status = 'published' AND hidden_at IS NULL
        "#,
        to,
        kind.as_str(),
        entity_id,
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(Moderated::NoSuchRevision);
    };
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO bio_revisions (entity_type, entity_id, parent_id, kind, reverted_to, body,
                                   summary, status)
        VALUES ($1, $2, $3, 'moderation', $4, $5, $6, 'published')
        RETURNING id
        "#,
        kind.as_str(),
        entity_id,
        head,
        to,
        body,
        summary,
    )
    .fetch_one(&mut *conn)
    .await?;
    set_current(conn, kind, entity_id, Some(id)).await?;
    Ok(Moderated::Done)
}

/// `worker moderation revert`: the bio gets `to`'s text back.
pub async fn revert(
    pool: &PgPool,
    kind: CatalogKind,
    entity_id: i64,
    to: i64,
    summary: &str,
) -> Result<Moderated, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let done = restore(&mut tx, kind, entity_id, Some(to), summary).await?;
    tx.commit().await?;
    Ok(done)
}

/// Locks (with a reason) or unlocks the bio against edits and reverts.
pub async fn lock(
    pool: &PgPool,
    kind: CatalogKind,
    entity_id: i64,
    reason: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO entity_bios (entity_type, entity_id, locked_at, lock_reason)
        VALUES ($1, $2, CASE WHEN $3::bool THEN NOW() END, $4)
        ON CONFLICT (entity_type, entity_id) DO UPDATE
        SET locked_at = EXCLUDED.locked_at, lock_reason = EXCLUDED.lock_reason
        "#,
        kind.as_str(),
        entity_id,
        reason.is_some(),
        reason,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The latest published, visible revision of the bio before `before` not by
/// a suspended account; `None` when there is none.
async fn last_good(
    conn: &mut PgConnection,
    kind: &str,
    entity_id: i64,
    before: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT r.id FROM bio_revisions r
        WHERE r.entity_type = $1 AND r.entity_id = $2 AND r.id < $3
          AND r.status = 'published' AND r.hidden_at IS NULL
          AND NOT EXISTS (SELECT 1 FROM editing_suspensions s
                          WHERE s.user_id = r.author_id AND (s.until IS NULL OR s.until > NOW()))
        ORDER BY r.id DESC
        LIMIT 1
        "#,
        kind,
        entity_id,
        before,
    )
    .fetch_optional(&mut *conn)
    .await
}

/// Hides the revision's text, and every revision of the same bio with the
/// same text (reverts copy it); a bio showing it goes back to its latest
/// revision still visible, or to no text. How many revisions were hidden.
pub async fn hide(pool: &PgPool, revision_id: i64) -> Result<Option<u64>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(r) = sqlx::query!(
        "SELECT entity_type, entity_id FROM bio_revisions WHERE id = $1",
        revision_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    let hidden = sqlx::query!(
        r#"
        UPDATE bio_revisions SET hidden_at = COALESCE(hidden_at, NOW())
        WHERE entity_type = $1 AND entity_id = $2
          AND body_hash = (SELECT body_hash FROM bio_revisions WHERE id = $3)
        "#,
        r.entity_type,
        r.entity_id,
        revision_id,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let head = sqlx::query!(
        r#"
        SELECT b.revision_id AS "revision_id!" FROM entity_bios b
        JOIN bio_revisions h ON h.id = b.revision_id
        WHERE b.entity_type = $1 AND b.entity_id = $2 AND h.hidden_at IS NOT NULL
        "#,
        r.entity_type,
        r.entity_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(head) = head {
        let kind = CatalogKind::parse(&r.entity_type).expect("a bio's entity type");
        let to = last_good(&mut tx, &r.entity_type, r.entity_id, head.revision_id).await?;
        restore(&mut tx, kind, r.entity_id, to, "hidden by a moderator").await?;
    }
    tx.commit().await?;
    Ok(Some(hidden))
}

/// Shows a hidden revision's text again (the bio stays as it is).
pub async fn unhide(pool: &PgPool, revision_id: i64) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query!(
        "UPDATE bio_revisions SET hidden_at = NULL WHERE id = $1 AND hidden_at IS NOT NULL",
        revision_id
    )
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

#[derive(Debug)]
pub struct Held {
    pub id: i64,
    pub entity_type: String,
    pub entity_id: i64,
    pub author: Option<String>,
    pub held_reason: Option<String>,
    pub excerpt: String,
    pub created_at: DateTime<Utc>,
}

/// Edits waiting for review, oldest first.
pub async fn held(pool: &PgPool, limit: i64) -> Result<Vec<Held>, sqlx::Error> {
    sqlx::query_as!(
        Held,
        r#"
        SELECT r.id, r.entity_type, r.entity_id, u.username AS "author?", r.held_reason,
               left(regexp_replace(r.body, '\s+', ' ', 'g'), 160) AS "excerpt!", r.created_at
        FROM bio_revisions r
        LEFT JOIN users u ON u.id = r.author_id
        WHERE r.status = 'held'
        ORDER BY r.created_at
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await
}

/// Publishes a held edit, if the bio is still at the revision it was
/// written over (or `force`); or rejects it.
pub async fn decide(
    pool: &PgPool,
    revision_id: i64,
    approve: bool,
    force: bool,
) -> Result<Moderated, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(r) = sqlx::query!(
        r#"
        SELECT entity_type, entity_id, parent_id FROM bio_revisions
        WHERE id = $1 AND status = 'held'
        FOR UPDATE
        "#,
        revision_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Moderated::NoSuchRevision);
    };
    if approve {
        let head = sqlx::query_scalar!(
            "SELECT revision_id FROM entity_bios WHERE entity_type = $1 AND entity_id = $2 FOR UPDATE",
            r.entity_type,
            r.entity_id,
        )
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        if head != r.parent_id && !force {
            return Ok(Moderated::Stale);
        }
        let kind = CatalogKind::parse(&r.entity_type).expect("a bio's entity type");
        set_current(&mut tx, kind, r.entity_id, Some(revision_id)).await?;
    }
    sqlx::query!(
        "UPDATE bio_revisions SET status = $2, reviewed_at = NOW() WHERE id = $1",
        revision_id,
        if approve { "published" } else { "rejected" },
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Moderated::Done)
}

/// What suspending accounts undid.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Undone {
    /// Bios put back to their latest revision by an account in good standing.
    pub bios: u64,
    /// Held edits rejected.
    pub rejected: u64,
    /// Entities whose tag counts were recounted.
    pub tagged: u64,
}

/// Suspends the accounts from editing bios and tagging until `until`
/// (`None`: until lifted); their tags weigh nothing from the next recount.
/// With `undo`, every bio showing one of their revisions goes back to its
/// latest revision by an account not suspended, and their held edits are
/// rejected.
pub async fn suspend(
    pool: &PgPool,
    user_ids: &[i64],
    until: Option<DateTime<Utc>>,
    reason: &str,
    undo: bool,
) -> Result<Undone, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"
        INSERT INTO editing_suspensions (user_id, until, reason)
        SELECT UNNEST($1::bigint[]), $2, $3
        ON CONFLICT (user_id) DO UPDATE
        SET until = EXCLUDED.until, reason = EXCLUDED.reason, created_at = NOW()
        "#,
        user_ids,
        until,
        reason,
    )
    .execute(&mut *tx)
    .await?;
    let mut undone = Undone::default();
    for user_id in user_ids {
        undone.tagged += tags::enqueue_user(&mut *tx, *user_id).await?;
    }
    if undo {
        let shown = sqlx::query!(
            r#"
            SELECT b.entity_type, b.entity_id, b.revision_id AS "revision_id!"
            FROM entity_bios b
            JOIN bio_revisions r ON r.id = b.revision_id
            JOIN editing_suspensions s ON s.user_id = r.author_id
            WHERE s.until IS NULL OR s.until > NOW()
            ORDER BY 1, 2
            "#,
        )
        .fetch_all(&mut *tx)
        .await?;
        for bio in shown {
            let kind = CatalogKind::parse(&bio.entity_type).expect("a bio's entity type");
            let to = last_good(&mut tx, &bio.entity_type, bio.entity_id, bio.revision_id).await?;
            restore(&mut tx, kind, bio.entity_id, to, "undone by a moderator").await?;
            undone.bios += 1;
        }
        undone.rejected = sqlx::query!(
            r#"
            UPDATE bio_revisions SET status = 'rejected', reviewed_at = NOW()
            WHERE author_id = ANY($1) AND status = 'held'
            "#,
            user_ids,
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(undone)
}

/// Lifts suspensions: whether there were any.
pub async fn unsuspend(pool: &PgPool, user_ids: &[i64]) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let lifted = sqlx::query!(
        "DELETE FROM editing_suspensions WHERE user_id = ANY($1)",
        user_ids
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    for user_id in user_ids {
        tags::enqueue_user(&mut *tx, *user_id).await?;
    }
    tx.commit().await?;
    Ok(lifted > 0)
}

/// Takes the tag off one entity, or everywhere, for every account: how many
/// taggings went. The entities are queued for a recount.
pub async fn untag(
    pool: &PgPool,
    tag_id: i64,
    on: Option<(CatalogKind, i64)>,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let gone = sqlx::query!(
        r#"
        DELETE FROM user_tags
        WHERE tag_id = $1 AND ($2::text IS NULL OR (entity_type = $2 AND entity_id = $3))
        RETURNING entity_type, entity_id
        "#,
        tag_id,
        on.map(|(k, _)| k.as_str()),
        on.map_or(0, |(_, id)| id),
    )
    .fetch_all(&mut *tx)
    .await?;
    let kinds: Vec<String> = gone.iter().map(|g| g.entity_type.clone()).collect();
    let ids: Vec<i64> = gone.iter().map(|g| g.entity_id).collect();
    sqlx::query!(
        r#"
        INSERT INTO tag_queue (entity_type, entity_id)
        SELECT DISTINCT * FROM UNNEST($1::text[], $2::bigint[])
        ON CONFLICT (entity_type, entity_id) DO UPDATE SET due_at = LEAST(tag_queue.due_at, NOW())
        "#,
        &kinds,
        &ids,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(gone.len() as u64)
}

/// Blocks a term (normalized as a tag) in new tags and takes the tags it
/// matches off everything: how many taggings went.
pub async fn block_term(pool: &PgPool, term: &str) -> Result<u64, sqlx::Error> {
    sqlx::query!(
        "INSERT INTO tag_blocklist (term) VALUES ($1) ON CONFLICT DO NOTHING",
        term
    )
    .execute(pool)
    .await?;
    let names: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM tags")
        .fetch_all(pool)
        .await?;
    let terms = [term.to_string()];
    let mut gone = 0;
    for (id, name) in names {
        if community::is_blocked(&name, &terms) {
            gone += untag(pool, id, None).await?;
        }
    }
    Ok(gone)
}

pub async fn unblock_term(pool: &PgPool, term: &str) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query!("DELETE FROM tag_blocklist WHERE term = $1", term)
            .execute(pool)
            .await?
            .rows_affected()
            > 0,
    )
}
