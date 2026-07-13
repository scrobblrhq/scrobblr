use sqlx::{PgPool, postgres::PgPoolOptions};

/// Create and return a PostgreSQL connection pool.
/// Reads pool sizing from env vars if set, otherwise uses sensible defaults.
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let max_connections = std::env::var("DB_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20u32);

    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
}
