//! Community contributions: image candidates with voting and comments.
//!
//! Image uploads add a candidate rather than replacing the entity image.
//! Voting promotes the most-liked candidate to the entity's displayed image
//! once it clears [`MIN_VOTES_TO_DEFAULT`] — promotion only, never demotion,
//! so the shown image never flickers away when a like is withdrawn.

use sqlx::PgPool;

use shared::models::{Comment, ImageCandidate};

/// Likes a candidate needs (and being the most-liked) to become the entity's
/// displayed image. Tunable; a low bar suits a small community.
pub const MIN_VOTES_TO_DEFAULT: i64 = 3;

/// Records a new image candidate and auto-likes it on the uploader's behalf,
/// then re-evaluates the winner. Returns the new candidate's id.
pub async fn add_image_candidate(
    pool: &PgPool,
    entity_type: &str,
    entity_id: i64,
    url: &str,
    uploaded_by: i64,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let row = sqlx::query!(
        r#"
        INSERT INTO image_candidates (entity_type, entity_id, url, uploaded_by, vote_count)
        VALUES ($1, $2, $3, $4, 1)
        RETURNING id
        "#,
        entity_type,
        entity_id,
        url,
        uploaded_by,
    )
    .fetch_one(&mut *tx)
    .await?;

    // The uploader implicitly likes their own submission.
    sqlx::query!(
        "INSERT INTO image_candidate_votes (candidate_id, user_id) VALUES ($1, $2)",
        row.id,
        uploaded_by,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(row.id)
}

/// Looks up which entity a candidate belongs to (used to scope votes and
/// recompute the winner). Returns `(entity_type, entity_id)`.
pub async fn candidate_entity(
    pool: &PgPool,
    candidate_id: i64,
) -> Result<Option<(String, i64)>, sqlx::Error> {
    let row = sqlx::query!(
        "SELECT entity_type, entity_id FROM image_candidates WHERE id = $1",
        candidate_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.entity_type, r.entity_id)))
}

/// Adds a like (idempotent) and returns the candidate's new vote count.
pub async fn vote_image(
    pool: &PgPool,
    candidate_id: i64,
    user_id: i64,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let inserted = sqlx::query!(
        r#"
        INSERT INTO image_candidate_votes (candidate_id, user_id)
        VALUES ($1, $2)
        ON CONFLICT DO NOTHING
        "#,
        candidate_id,
        user_id,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if inserted > 0 {
        sqlx::query!(
            "UPDATE image_candidates SET vote_count = vote_count + 1 WHERE id = $1",
            candidate_id,
        )
        .execute(&mut *tx)
        .await?;
    }

    let count = sqlx::query_scalar!(
        "SELECT vote_count FROM image_candidates WHERE id = $1",
        candidate_id,
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(count as i64)
}

/// Removes a like (idempotent) and returns the candidate's new vote count.
pub async fn unvote_image(
    pool: &PgPool,
    candidate_id: i64,
    user_id: i64,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let deleted = sqlx::query!(
        "DELETE FROM image_candidate_votes WHERE candidate_id = $1 AND user_id = $2",
        candidate_id,
        user_id,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if deleted > 0 {
        sqlx::query!(
            "UPDATE image_candidates SET vote_count = GREATEST(vote_count - 1, 0) WHERE id = $1",
            candidate_id,
        )
        .execute(&mut *tx)
        .await?;
    }

    let count = sqlx::query_scalar!(
        "SELECT vote_count FROM image_candidates WHERE id = $1",
        candidate_id,
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(count as i64)
}

/// If the entity's most-liked candidate clears the threshold, promotes its
/// image to the entity (locking it against enrichment). Promotion only: an
/// already-winning image is never reverted when likes drop.
pub async fn promote_winning_image(
    pool: &PgPool,
    entity_type: &str,
    entity_id: i64,
) -> Result<(), sqlx::Error> {
    let winner = sqlx::query!(
        r#"
        SELECT url, vote_count
        FROM image_candidates
        WHERE entity_type = $1 AND entity_id = $2
        ORDER BY vote_count DESC, created_at ASC
        LIMIT 1
        "#,
        entity_type,
        entity_id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(winner) = winner else { return Ok(()) };
    if (winner.vote_count as i64) < MIN_VOTES_TO_DEFAULT {
        return Ok(());
    }

    match entity_type {
        "artist" => {
            sqlx::query!(
                "UPDATE artists SET image_url = $2, image_locked = TRUE WHERE id = $1",
                entity_id,
                winner.url,
            )
            .execute(pool)
            .await?;
        }
        "album" => {
            sqlx::query!(
                "UPDATE albums SET image_url = $2, image_locked = TRUE WHERE id = $1",
                entity_id,
                winner.url,
            )
            .execute(pool)
            .await?;
        }
        _ => {}
    }
    Ok(())
}

/// Lists an entity's image candidates, most-liked first, flagging the
/// viewer's own votes and which candidate is the entity's current image.
pub async fn list_image_candidates(
    pool: &PgPool,
    entity_type: &str,
    entity_id: i64,
    viewer_id: Option<i64>,
) -> Result<Vec<ImageCandidate>, sqlx::Error> {
    // The entity's current image_url, to flag the default candidate.
    let current: Option<String> = match entity_type {
        "artist" => sqlx::query_scalar!("SELECT image_url FROM artists WHERE id = $1", entity_id)
            .fetch_optional(pool)
            .await?
            .flatten(),
        "album" => sqlx::query_scalar!("SELECT image_url FROM albums WHERE id = $1", entity_id)
            .fetch_optional(pool)
            .await?
            .flatten(),
        _ => None,
    };

    sqlx::query!(
        r#"
        SELECT c.id, c.url, c.vote_count, c.created_at,
               u.username AS uploaded_by,
               ($4::bigint IS NOT NULL AND v.user_id IS NOT NULL) AS "has_voted!"
        FROM image_candidates c
        JOIN users u ON u.id = c.uploaded_by
        LEFT JOIN image_candidate_votes v
               ON v.candidate_id = c.id AND v.user_id = $4
        WHERE c.entity_type = $1 AND c.entity_id = $2
        ORDER BY c.vote_count DESC, c.created_at ASC
        LIMIT $3
        "#,
        entity_type,
        entity_id,
        50_i64,
        viewer_id,
    )
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|r| ImageCandidate {
                id: r.id,
                is_default: current.as_deref() == Some(r.url.as_str()),
                url: r.url,
                uploaded_by: r.uploaded_by,
                vote_count: r.vote_count as i64,
                has_voted: r.has_voted,
                created_at: r.created_at,
            })
            .collect()
    })
}

pub async fn list_comments(
    pool: &PgPool,
    entity_type: &str,
    entity_id: i64,
    limit: i64,
) -> Result<Vec<Comment>, sqlx::Error> {
    sqlx::query_as!(
        Comment,
        r#"
        SELECT c.id, c.user_id, c.body, c.created_at,
               u.username     AS "username!",
               u.display_name AS "display_name?",
               u.image_url    AS "image_url?"
        FROM comments c
        JOIN users u ON u.id = c.user_id
        WHERE c.entity_type = $1 AND c.entity_id = $2
        ORDER BY c.created_at DESC
        LIMIT $3
        "#,
        entity_type,
        entity_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

pub async fn add_comment(
    pool: &PgPool,
    entity_type: &str,
    entity_id: i64,
    user_id: i64,
    body: &str,
) -> Result<Comment, sqlx::Error> {
    sqlx::query_as!(
        Comment,
        r#"
        WITH inserted AS (
            INSERT INTO comments (entity_type, entity_id, user_id, body)
            VALUES ($1, $2, $3, $4)
            RETURNING id, user_id, body, created_at
        )
        SELECT i.id, i.user_id, i.body, i.created_at,
               u.username     AS "username!",
               u.display_name AS "display_name?",
               u.image_url    AS "image_url?"
        FROM inserted i
        JOIN users u ON u.id = i.user_id
        "#,
        entity_type,
        entity_id,
        user_id,
        body,
    )
    .fetch_one(pool)
    .await
}

/// Deletes a comment only if it belongs to `user_id`. Returns whether a row
/// was removed (false = not found or not the owner).
pub async fn delete_comment(
    pool: &PgPool,
    comment_id: i64,
    user_id: i64,
) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query!(
        "DELETE FROM comments WHERE id = $1 AND user_id = $2",
        comment_id,
        user_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted > 0)
}
