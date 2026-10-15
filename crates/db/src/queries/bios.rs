//! Community bios of artists and albums (migration 0037; docs/social.md,
//! Bios): revisions, the current one, who may write them, and the edits
//! held for review.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};

use shared::community::{self, Editor};
use shared::models::{Bio, BioRevision, CatalogKind, RevisionKind, RevisionStatus, UserSummary};

/// What decides whether `user_id` may edit bios; `None` for no such user.
pub async fn editor(pool: &PgPool, user_id: i64) -> Result<Option<Editor>, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT u.created_at, u.delete_after IS NOT NULL AS "leaving!",
               EXISTS (SELECT 1 FROM editing_suspensions s
                       WHERE s.user_id = u.id AND (s.until IS NULL OR s.until > NOW()))
                   AS "suspended!",
               (SELECT min(d.factor)::bigint FROM coordination_days d
                WHERE d.user_id = u.id AND d.day >= CURRENT_DATE - $2::int) AS lowest_factor
        FROM users u
        WHERE u.id = $1
        "#,
        user_id,
        community::FACTOR_WINDOW_DAYS as i32,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Editor {
        created_at: r.created_at,
        leaving: r.leaving,
        suspended: r.suspended,
        lowest_factor: r.lowest_factor,
    }))
}

struct Row {
    id: i64,
    entity_type: String,
    entity_id: i64,
    parent_id: Option<i64>,
    kind: String,
    author_id: Option<i64>,
    author_username: Option<String>,
    author_display_name: Option<String>,
    author_image: Option<String>,
    summary: Option<String>,
    body: Option<String>,
    length: i64,
    reverted_to: Option<i64>,
    status: String,
    held_reason: Option<String>,
    hidden: bool,
    current: bool,
    created_at: DateTime<Utc>,
}

fn author(
    id: Option<i64>,
    username: Option<String>,
    display_name: Option<String>,
    image_url: Option<String>,
) -> Option<UserSummary> {
    Some(UserSummary {
        id: id?,
        username: username?,
        display_name,
        image_url,
    })
}

impl From<Row> for BioRevision {
    fn from(r: Row) -> Self {
        Self {
            id: r.id,
            entity_type: CatalogKind::parse(&r.entity_type).expect("a bio's entity type"),
            entity_id: r.entity_id,
            parent_id: r.parent_id,
            kind: RevisionKind::parse(&r.kind).expect("a revision kind"),
            author: author(
                r.author_id,
                r.author_username,
                r.author_display_name,
                r.author_image,
            ),
            summary: r.summary,
            body: r.body,
            length: r.length,
            reverted_to: r.reverted_to,
            status: RevisionStatus::parse(&r.status).expect("a revision status"),
            held_reason: r.held_reason,
            hidden: r.hidden,
            current: r.current,
            created_at: r.created_at,
        }
    }
}

/// The bio as it stands. Authors are named unless waiting to be erased, or
/// blocking `viewer` or blocked by it.
pub async fn current(
    pool: &PgPool,
    kind: CatalogKind,
    id: i64,
    viewer: Option<i64>,
) -> Result<Bio, sqlx::Error> {
    let r = sqlx::query!(
        r#"
        SELECT b.revision_id AS "revision_id?", r.body AS "body?", r.kind AS "kind?",
               r.created_at AS "updated_at?", COALESCE(b.locked_at IS NOT NULL, FALSE) AS "locked!",
               u.id AS "author_id?", u.username AS "author_username?",
               u.display_name AS "author_display_name?", u.image_url AS "author_image?",
               (SELECT count(*) FROM bio_revisions c
                WHERE c.entity_type = $1 AND c.entity_id = $2 AND c.status = 'published')
                   AS "revisions!"
        FROM (SELECT 1) one
        LEFT JOIN entity_bios b ON b.entity_type = $1 AND b.entity_id = $2
        LEFT JOIN bio_revisions r ON r.id = b.revision_id
        LEFT JOIN users u ON u.id = r.author_id AND u.delete_after IS NULL
              AND ($3::bigint IS NULL OR NOT EXISTS (
                      SELECT 1 FROM user_blocks k
                      WHERE (k.blocker_id = u.id AND k.blocked_id = $3)
                         OR (k.blocker_id = $3 AND k.blocked_id = u.id)))
        "#,
        kind.as_str(),
        id,
        viewer,
    )
    .fetch_one(pool)
    .await?;
    Ok(Bio {
        entity_type: kind,
        entity_id: id,
        revision_id: r.revision_id,
        body: r.body,
        kind: r.kind.as_deref().and_then(RevisionKind::parse),
        author: author(
            r.author_id,
            r.author_username,
            r.author_display_name,
            r.author_image,
        ),
        updated_at: r.updated_at,
        locked: r.locked,
        revisions: r.revisions,
    })
}

/// Published revisions before `before`, newest first, without their text.
pub async fn history(
    pool: &PgPool,
    kind: CatalogKind,
    id: i64,
    before: Option<i64>,
    limit: i64,
    viewer: Option<i64>,
) -> Result<Vec<BioRevision>, sqlx::Error> {
    let rows = sqlx::query_as!(
        Row,
        r#"
        SELECT r.id, r.entity_type, r.entity_id, r.parent_id, r.kind,
               u.id AS "author_id?", u.username AS "author_username?",
               u.display_name AS "author_display_name?", u.image_url AS "author_image?",
               CASE WHEN r.hidden_at IS NULL THEN r.summary END AS summary,
               NULL::text AS body, char_length(r.body)::bigint AS "length!", r.reverted_to,
               r.status, r.held_reason, r.hidden_at IS NOT NULL AS "hidden!",
               COALESCE(r.id = b.revision_id, FALSE) AS "current!", r.created_at
        FROM bio_revisions r
        LEFT JOIN entity_bios b ON b.entity_type = r.entity_type AND b.entity_id = r.entity_id
        LEFT JOIN users u ON u.id = r.author_id AND u.delete_after IS NULL
              AND ($5::bigint IS NULL OR NOT EXISTS (
                      SELECT 1 FROM user_blocks k
                      WHERE (k.blocker_id = u.id AND k.blocked_id = $5)
                         OR (k.blocker_id = $5 AND k.blocked_id = u.id)))
        WHERE r.entity_type = $1 AND r.entity_id = $2 AND r.status = 'published'
          AND ($3::bigint IS NULL OR r.id < $3)
        ORDER BY r.id DESC
        LIMIT $4
        "#,
        kind.as_str(),
        id,
        before,
        limit,
        viewer,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(BioRevision::from).collect())
}

/// One revision with its text: a published one for anyone, a held or
/// rejected one for its author only.
pub async fn revision(
    conn: &mut PgConnection,
    revision_id: i64,
    viewer: Option<i64>,
) -> Result<Option<BioRevision>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT r.id, r.entity_type, r.entity_id, r.parent_id, r.kind,
               u.id AS "author_id?", u.username AS "author_username?",
               u.display_name AS "author_display_name?", u.image_url AS "author_image?",
               CASE WHEN r.hidden_at IS NULL THEN r.summary END AS summary,
               CASE WHEN r.hidden_at IS NULL THEN r.body END AS body,
               char_length(r.body)::bigint AS "length!", r.reverted_to,
               r.status, r.held_reason, r.hidden_at IS NOT NULL AS "hidden!",
               COALESCE(r.id = b.revision_id, FALSE) AS "current!", r.created_at
        FROM bio_revisions r
        LEFT JOIN entity_bios b ON b.entity_type = r.entity_type AND b.entity_id = r.entity_id
        LEFT JOIN users u ON u.id = r.author_id AND u.delete_after IS NULL
              AND ($2::bigint IS NULL OR NOT EXISTS (
                      SELECT 1 FROM user_blocks k
                      WHERE (k.blocker_id = u.id AND k.blocked_id = $2)
                         OR (k.blocker_id = $2 AND k.blocked_id = u.id)))
        WHERE r.id = $1 AND (r.status = 'published' OR r.author_id = $2)
        "#,
        revision_id,
        viewer,
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.map(BioRevision::from))
}

/// What an edit or a revert asks for.
pub struct Change<'a> {
    pub kind: CatalogKind,
    pub entity_id: i64,
    pub author_id: i64,
    /// The revision the author saw; `None` when there was no bio.
    pub base: Option<i64>,
    pub summary: &'a str,
}

/// What came of an edit or a revert.
#[derive(Debug)]
pub enum Written {
    /// Published, or held for review: its status says.
    Revision(Box<BioRevision>),
    /// The bio isn't at `base` any more.
    Conflict {
        current: Option<i64>,
    },
    Locked,
    /// The text is the current one already.
    Unchanged,
    /// The author's revisions of the last 24 hours reached the cap.
    DailyCap,
    /// The author's revisions of this bio in the last 24 hours did.
    BioCap,
    /// A revert to a revision of another bio, not published, or hidden.
    BadTarget,
}

struct Head {
    revision_id: Option<i64>,
    body: Option<String>,
}

/// Locks the bio's row, and checks what every change checks.
async fn begin(
    conn: &mut PgConnection,
    change: &Change<'_>,
) -> Result<Result<Head, Written>, sqlx::Error> {
    let kind = change.kind.as_str();
    sqlx::query!(
        "INSERT INTO entity_bios (entity_type, entity_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        kind,
        change.entity_id,
    )
    .execute(&mut *conn)
    .await?;
    let head = sqlx::query!(
        r#"
        SELECT b.revision_id, b.locked_at IS NOT NULL AS "locked!", r.body AS "body?"
        FROM entity_bios b
        LEFT JOIN bio_revisions r ON r.id = b.revision_id
        WHERE b.entity_type = $1 AND b.entity_id = $2
        FOR UPDATE OF b
        "#,
        kind,
        change.entity_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    if head.locked {
        return Ok(Err(Written::Locked));
    }
    if head.revision_id != change.base {
        return Ok(Err(Written::Conflict {
            current: head.revision_id,
        }));
    }
    let made = sqlx::query!(
        r#"
        SELECT count(*) AS "day!",
               count(*) FILTER (WHERE entity_type = $2 AND entity_id = $3) AS "bio!"
        FROM bio_revisions
        WHERE author_id = $1 AND created_at > NOW() - INTERVAL '1 day'
        "#,
        change.author_id,
        kind,
        change.entity_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    if made.day >= community::EDITS_PER_DAY {
        return Ok(Err(Written::DailyCap));
    }
    if made.bio >= community::EDITS_PER_BIO_PER_DAY {
        return Ok(Err(Written::BioCap));
    }
    Ok(Ok(Head {
        revision_id: head.revision_id,
        body: head.body,
    }))
}

/// Makes `revision_id` the current text, and `artists.bio` follow it.
pub(crate) async fn set_current(
    conn: &mut PgConnection,
    kind: CatalogKind,
    entity_id: i64,
    revision_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        INSERT INTO entity_bios (entity_type, entity_id, revision_id) VALUES ($1, $2, $3)
        ON CONFLICT (entity_type, entity_id)
        DO UPDATE SET revision_id = EXCLUDED.revision_id, updated_at = NOW()
        "#,
        kind.as_str(),
        entity_id,
        revision_id,
    )
    .execute(&mut *conn)
    .await?;
    if kind == CatalogKind::Artist {
        sqlx::query!(
            "UPDATE artists SET bio = (SELECT body FROM bio_revisions WHERE id = $2) WHERE id = $1",
            entity_id,
            revision_id,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn added_hosts(body: &str, base: Option<&str>) -> Vec<String> {
    let before = base.map(community::link_hosts).unwrap_or_default();
    let mut added: Vec<String> = community::link_hosts(body)
        .into_iter()
        .filter(|h| !before.contains(h))
        .collect();
    added.sort();
    added.dedup();
    added
}

/// Why an edit should wait for a moderator, if it should.
async fn hold_reason(
    conn: &mut PgConnection,
    change: &Change<'_>,
    body: &str,
    added: &[String],
) -> Result<Option<&'static str>, sqlx::Error> {
    let hosts = community::link_hosts(body);
    if community::repeated_host(&hosts).is_some_and(|h| added.iter().any(|a| a == h)) {
        return Ok(Some("repeated_link"));
    }
    if !added.is_empty() {
        let spread = sqlx::query!(
            r#"
            SELECT count(DISTINCT (entity_type, entity_id))
                       FILTER (WHERE author_id = $3 AND created_at > NOW() - INTERVAL '7 days')
                       AS "by_author!",
                   count(DISTINCT (entity_type, entity_id))
                       FILTER (WHERE created_at > NOW() - INTERVAL '1 day') AS "by_anyone!"
            FROM bio_revisions
            WHERE added_hosts <> '{}' AND added_hosts && $4::text[] AND status <> 'rejected'
              AND NOT (entity_type = $1 AND entity_id = $2)
            "#,
            change.kind.as_str(),
            change.entity_id,
            change.author_id,
            added,
        )
        .fetch_one(&mut *conn)
        .await?;
        if spread.by_author >= community::LINK_BIOS_PER_AUTHOR
            || spread.by_anyone >= community::LINK_BIOS_PER_DAY
        {
            return Ok(Some("link_across_bios"));
        }
    }
    if body.chars().count() >= community::DUPLICATE_MIN_CHARS {
        let copied = sqlx::query_scalar!(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM bio_revisions
                WHERE body_hash = bio_text_hash($3) AND status <> 'rejected'
                  AND NOT (entity_type = $1 AND entity_id = $2)
            ) AS "copied!"
            "#,
            change.kind.as_str(),
            change.entity_id,
            body,
        )
        .fetch_one(&mut *conn)
        .await?;
        if copied {
            return Ok(Some("duplicate_text"));
        }
    }
    Ok(None)
}

/// Writes `body` (already through [`community::bio_text`]) over the bio,
/// or holds it for review when it looks like spam.
pub async fn edit(pool: &PgPool, change: &Change<'_>, body: &str) -> Result<Written, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let head = match begin(&mut tx, change).await? {
        Ok(head) => head,
        Err(refused) => return Ok(refused),
    };
    if head.body.as_deref() == Some(body) {
        return Ok(Written::Unchanged);
    }
    let added = added_hosts(body, head.body.as_deref());
    let held = hold_reason(&mut tx, change, body, &added).await?;
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO bio_revisions (entity_type, entity_id, parent_id, author_id, kind, body,
                                   summary, status, held_reason, added_hosts)
        VALUES ($1, $2, $3, $4, 'edit', $5, $6, $7, $8, $9)
        RETURNING id
        "#,
        change.kind.as_str(),
        change.entity_id,
        head.revision_id,
        change.author_id,
        body,
        change.summary,
        if held.is_some() { "held" } else { "published" },
        held,
        &added,
    )
    .fetch_one(&mut *tx)
    .await?;
    if held.is_none() {
        set_current(&mut tx, change.kind, change.entity_id, Some(id)).await?;
    }
    let written = revision(&mut tx, id, Some(change.author_id))
        .await?
        .expect("the revision just written");
    tx.commit().await?;
    Ok(Written::Revision(Box::new(written)))
}

/// Restores the text of `target`, an earlier published revision of the
/// same bio, as a new revision. Never held: the text was published once.
pub async fn revert(
    pool: &PgPool,
    change: &Change<'_>,
    target: i64,
) -> Result<Written, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let head = match begin(&mut tx, change).await? {
        Ok(head) => head,
        Err(refused) => return Ok(refused),
    };
    let Some(old) = sqlx::query!(
        r#"
        SELECT body FROM bio_revisions
        WHERE id = $1 AND entity_type = $2 AND entity_id = $3
          AND status = 'published' AND hidden_at IS NULL
        "#,
        target,
        change.kind.as_str(),
        change.entity_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Written::BadTarget);
    };
    if head.revision_id == Some(target) || head.body.as_deref() == Some(old.body.as_str()) {
        return Ok(Written::Unchanged);
    }
    let added = added_hosts(&old.body, head.body.as_deref());
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO bio_revisions (entity_type, entity_id, parent_id, author_id, kind,
                                   reverted_to, body, summary, status, added_hosts)
        VALUES ($1, $2, $3, $4, 'revert', $5, $6, $7, 'published', $8)
        RETURNING id
        "#,
        change.kind.as_str(),
        change.entity_id,
        head.revision_id,
        change.author_id,
        target,
        old.body,
        change.summary,
        &added,
    )
    .fetch_one(&mut *tx)
    .await?;
    set_current(&mut tx, change.kind, change.entity_id, Some(id)).await?;
    let written = revision(&mut tx, id, Some(change.author_id))
        .await?
        .expect("the revision just written");
    tx.commit().await?;
    Ok(Written::Revision(Box::new(written)))
}

/// Last.fm's bio of an artist, as an `import` revision: when it has no
/// bio, or over an earlier import with `force`; never once a person has
/// edited it. Text the bio rules refuse is left out. Whether it was written.
pub async fn import(
    pool: &PgPool,
    artist_id: i64,
    text: &str,
    force: bool,
) -> Result<bool, sqlx::Error> {
    let Ok(body) = community::bio_text(text) else {
        tracing::debug!(
            artist_id,
            "enrichment: Last.fm bio left out by the bio rules"
        );
        return Ok(false);
    };
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "INSERT INTO entity_bios (entity_type, entity_id) VALUES ('artist', $1) ON CONFLICT DO NOTHING",
        artist_id,
    )
    .execute(&mut *tx)
    .await?;
    let head = sqlx::query!(
        r#"
        SELECT b.revision_id, r.body AS "body?",
               EXISTS (SELECT 1 FROM bio_revisions p
                       WHERE p.entity_type = 'artist' AND p.entity_id = $1
                         AND p.kind <> 'import' AND p.status = 'published') AS "edited!"
        FROM entity_bios b
        LEFT JOIN bio_revisions r ON r.id = b.revision_id
        WHERE b.entity_type = 'artist' AND b.entity_id = $1
        FOR UPDATE OF b
        "#,
        artist_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if head.edited
        || (head.revision_id.is_some() && !force)
        || head.body.as_deref() == Some(body.as_str())
    {
        return Ok(false);
    }
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO bio_revisions (entity_type, entity_id, parent_id, kind, body, status)
        VALUES ('artist', $1, $2, 'import', $3, 'published')
        RETURNING id
        "#,
        artist_id,
        head.revision_id,
        body,
    )
    .fetch_one(&mut *tx)
    .await?;
    set_current(&mut tx, CatalogKind::Artist, artist_id, Some(id)).await?;
    tx.commit().await?;
    Ok(true)
}
