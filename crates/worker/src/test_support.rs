//! Database harness for the worker's `#[ignore]`d tests (`just test-db`);
//! mirrors `crates/db/tests/common`.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection, PgPool};

/// Runs `test` against a throwaway, fully migrated database, dropped
/// afterwards even if the test panics.
pub async fn with_db<F, Fut>(test: F)
where
    F: FnOnce(PgPool) -> Fut,
    Fut: Future<Output = ()>,
{
    dotenvy::dotenv().ok();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required for DB tests");
    let admin: PgConnectOptions = url.parse().expect("invalid DATABASE_URL");
    let name = format!("scrobblr_test_{}", uuid::Uuid::new_v4().simple());

    let mut conn = PgConnection::connect_with(&admin).await.unwrap();
    conn.execute(format!("CREATE DATABASE {name}").as_str())
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(admin.clone().database(&name))
        .await
        .unwrap();
    db::migrate::run(&pool).await.unwrap();

    let result = AssertUnwindSafe(test(pool.clone())).catch_unwind().await;

    pool.close().await;
    conn.execute(format!("DROP DATABASE {name} WITH (FORCE)").as_str())
        .await
        .unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
