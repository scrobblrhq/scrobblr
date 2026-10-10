mod common;

use chrono::Utc;
use common::with_db;
use db::queries::monitoring as mdb;
use shared::monitoring::LoopState;

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn heartbeats_record_runs_and_registration_starts_over() {
    with_db(true, |pool| async move {
        mdb::register_loops(&pool, &[("old", 60)]).await.unwrap();
        mdb::register_loops(&pool, &[("a", 60), ("b", 120)])
            .await
            .unwrap();
        let beats = mdb::heartbeats(&pool).await.unwrap();
        let names: Vec<&str> = beats.iter().map(|b| b.loop_name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert!(beats.iter().all(|b| b.last_run_at.is_none()));

        mdb::record_run(&pool, "a", 60, None).await.unwrap();
        mdb::record_run(&pool, "b", 300, Some("boom"))
            .await
            .unwrap();
        let beats = mdb::heartbeats(&pool).await.unwrap();
        let (a, b) = (&beats[0], &beats[1]);
        assert!(a.last_ok_at.is_some() && a.last_error_at.is_none());
        assert_eq!(b.interval_secs, 300);
        assert!(b.last_ok_at.is_none());
        assert_eq!(b.last_error.as_deref(), Some("boom"));

        // A success keeps the last error for the record.
        mdb::record_run(&pool, "b", 120, None).await.unwrap();
        let b = &mdb::heartbeats(&pool).await.unwrap()[1];
        assert!(b.last_ok_at.is_some());
        assert_eq!(b.last_error.as_deref(), Some("boom"));
        assert_eq!(b.state(Utc::now(), 3.0), LoopState::Ok);

        mdb::remove_loop(&pool, "b").await.unwrap();
        assert_eq!(mdb::heartbeats(&pool).await.unwrap().len(), 1);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn queue_depths_count_due_work() {
    with_db(true, |pool| async move {
        sqlx::query(
            "INSERT INTO classification_queue (user_id, day, not_before) VALUES \
             (1, '2026-10-01', NOW() - interval '10 minutes'), \
             (2, '2026-10-01', NOW() + interval '1 hour')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO enrichment_jobs (entity_type, entity_id, status) VALUES ('artist', 1, 'pending'), ('artist', 2, 'failed')")
            .execute(&pool)
            .await
            .unwrap();
        let depths = mdb::queue_depths(&pool).await.unwrap();
        let depth = |q: &str| depths.iter().find(|d| d.queue == q).unwrap().clone();
        assert_eq!(depth("classification").due, 1);
        assert!(depth("classification").oldest_due_secs.unwrap() >= 600.0);
        assert_eq!(depth("ranking").due, 0);
        assert_eq!(depth("ranking").oldest_due_secs, None);
        assert_eq!(depth("enrichment").due, 1);
        assert_eq!(depth("import").due, 0);
    })
    .await;
}
