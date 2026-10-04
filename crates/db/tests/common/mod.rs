//! Database test harness. Tests using it are `#[ignore]`d so the default
//! `cargo test` needs no Postgres; run them with `just test-db`.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection, PgPool};

/// Creates a throwaway database (migrated through the runner when `migrate`
/// is set), runs `test` against it, and drops it even if the test panics.
pub async fn with_db<F, Fut>(migrate: bool, test: F)
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
    if migrate {
        db::migrate::run(&pool).await.unwrap();
    }

    let result = AssertUnwindSafe(test(pool.clone())).catch_unwind().await;

    pool.close().await;
    conn.execute(format!("DROP DATABASE {name} WITH (FORCE)").as_str())
        .await
        .unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
