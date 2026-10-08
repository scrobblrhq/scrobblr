//! Which uploads the database still refers to.

use sqlx::PgPool;

/// Every image value that isn't an absolute URL: the upload keys some row
/// refers to (candidates included, voted on or not).
pub async fn referenced_keys(pool: &PgPool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT image_url AS "key!" FROM users
        WHERE image_url IS NOT NULL AND image_url !~ '^https?://'
        UNION
        SELECT image_url FROM artists
        WHERE image_url IS NOT NULL AND image_url !~ '^https?://'
        UNION
        SELECT image_url FROM albums
        WHERE image_url IS NOT NULL AND image_url !~ '^https?://'
        UNION
        SELECT url FROM image_candidates WHERE url !~ '^https?://'
        "#
    )
    .fetch_all(pool)
    .await
}
