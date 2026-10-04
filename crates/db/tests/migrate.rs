mod common;

use common::with_db;
use db::migrate::{self, MigrateError};

fn all_versions() -> Vec<i64> {
    let mut versions = Vec::new();
    for entry in
        std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../migrations")).unwrap()
    {
        let name = entry.unwrap().file_name().into_string().unwrap();
        versions.push(name.split('_').next().unwrap().parse().unwrap());
    }
    versions.sort();
    versions
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn fresh_database_migrates_fully_and_reruns_as_noop() {
    with_db(false, |pool| async move {
        assert!(matches!(
            migrate::ensure_current(&pool).await,
            Err(MigrateError::Pending(..))
        ));

        assert_eq!(migrate::run(&pool).await.unwrap(), all_versions());
        migrate::ensure_current(&pool).await.unwrap();
        assert!(migrate::run(&pool).await.unwrap().is_empty());

        let policies: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM timescaledb_information.jobs \
             WHERE proc_name = 'policy_refresh_continuous_aggregate' AND config->'start_offset' = 'null'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(policies, 3);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn concurrent_runs_apply_each_migration_once() {
    with_db(false, |pool| async move {
        let (a, b) = tokio::join!(migrate::run(&pool), migrate::run(&pool));
        let mut applied = [a.unwrap(), b.unwrap()].concat();
        applied.sort();
        assert_eq!(applied, all_versions());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn modified_migration_is_rejected() {
    with_db(true, |pool| async move {
        sqlx::query("UPDATE schema_migrations SET checksum = '\\x00' WHERE version = 3")
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            migrate::ensure_current(&pool).await,
            Err(MigrateError::ChecksumMismatch(3))
        ));
        assert!(matches!(
            migrate::run(&pool).await,
            Err(MigrateError::ChecksumMismatch(3))
        ));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn baseline_adopts_a_hand_migrated_database() {
    with_db(true, |pool| async move {
        sqlx::query("DROP TABLE schema_migrations")
            .execute(&pool)
            .await
            .unwrap();
        let newest = *all_versions().last().unwrap();
        assert_eq!(
            migrate::baseline(&pool, newest).await.unwrap(),
            all_versions()
        );
        migrate::ensure_current(&pool).await.unwrap();
    })
    .await;
}
