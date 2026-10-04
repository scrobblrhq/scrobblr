mod common;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use common::with_db;
use db::queries::classification::{self as cdb, QueuedDay, Ruleset};
use db::queries::{scrobbles as scrobbles_db, tracks as tracks_db};
use shared::classification::{BudgetParams, Status};
use shared::scrobble::ScrobbleInput;
use sqlx::PgPool;

fn days_ago(n: i64) -> NaiveDate {
    (Utc::now() - TimeDelta::days(n)).date_naive()
}

fn at(day: NaiveDate, secs: i64) -> DateTime<Utc> {
    day.and_hms_opt(0, 0, 0).unwrap().and_utc() + TimeDelta::seconds(secs)
}

async fn user(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash) VALUES ($1, $1 || '@test', 'x') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Returns `(track_id, artist_id)`.
async fn track(pool: &PgPool, title: &str, duration_ms: Option<i32>) -> (i64, i64) {
    let artist = tracks_db::find_or_create_artist(pool, "Artist")
        .await
        .unwrap();
    let track = tracks_db::find_or_create_track(pool, artist.id, None, title, duration_ms)
        .await
        .unwrap();
    (track.id, artist.id)
}

async fn scrobble(
    pool: &PgPool,
    user_id: i64,
    (track_id, artist_id): (i64, i64),
    played_at: DateTime<Utc>,
    duration_ms: Option<i32>,
) {
    scrobbles_db::insert_scrobble(
        pool,
        &scrobbles_db::InsertScrobble {
            user_id,
            track_id,
            artist_id,
            album_id: None,
            played_at,
            source: "test".into(),
            duration_ms,
            listened_ms: None,
        },
    )
    .await
    .unwrap();
}

async fn ruleset(pool: &PgPool) -> Ruleset {
    cdb::register_ruleset(pool, BudgetParams::default())
        .await
        .unwrap()
}

/// `(scrobble_id, status, reason)` of every flag, ordered.
async fn flags(pool: &PgPool, user_id: i64) -> Vec<(i64, String, String)> {
    sqlx::query_as(
        "SELECT scrobble_id, status::text, reason FROM scrobble_flags WHERE user_id = $1 ORDER BY scrobble_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn label_counts(pool: &PgPool, user_id: i64) -> Vec<(Option<String>, i64)> {
    sqlx::query_as(
        "SELECT status::text, count(*) FROM scrobble_labels WHERE user_id = $1 GROUP BY 1 ORDER BY 1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn queued(pool: &PgPool) -> Vec<(i64, NaiveDate, i32)> {
    sqlx::query_as("SELECT user_id, day, priority FROM classification_queue ORDER BY 1, 2")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Refreshes the daily aggregates first, as an import would: the sweep reads
/// `user_activity_daily`, which only sees backfilled rows after a refresh.
async fn sweep(pool: &PgPool, ruleset_id: i32) -> u64 {
    scrobbles_db::refresh_scrobble_aggregates(pool, Utc::now() - TimeDelta::days(365), Utc::now())
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    cdb::enqueue_stale(&mut conn, ruleset_id, None, None, None, cdb::PRIORITY_SWEEP)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn ingest_queues_the_day_and_classification_labels_it() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "alice").await;
        let day = days_ago(3);
        for n in 0..5 {
            let input = ScrobbleInput {
                track_title: format!("Song {n}"),
                artist_name: "Artist".into(),
                featured_artists: vec![],
                album_title: None,
                played_at: at(day, 3600 + n * 240),
                duration_ms: Some(230_000),
                listened_ms: Some(200_000),
                source: "test".into(),
            };
            scrobbles_db::ingest_scrobble(&pool, user_id, &input)
                .await
                .unwrap();
        }
        assert_eq!(queued(&pool).await, [(user_id, day, cdb::PRIORITY_INGEST)]);
        // Ingest marks settle before they are due.
        assert!(cdb::claim_due(&pool, 10).await.unwrap().is_empty());

        let stored: Vec<Option<i32>> =
            sqlx::query_scalar("SELECT listened_ms FROM scrobbles ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(stored.iter().all(|l| *l == Some(200_000)));

        assert_eq!(label_counts(&pool, user_id).await, [(None, 5)]);
        let outcome = cdb::classify_user_day(&pool, &ruleset(&pool).await, user_id, day, false)
            .await
            .unwrap();
        assert_eq!(outcome.counts.counted, 5);
        assert_eq!(outcome.changes.get(&(None, Status::Counted)), Some(&5));
        assert_eq!(
            label_counts(&pool, user_id).await,
            [(Some("counted".into()), 5)]
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn bot_hour_is_flagged_identically_on_rerun_and_charts_are_unchanged() {
    with_db(true, |pool| async move {
        let bot = user(&pool, "bot").await;
        let song = track(&pool, "Song", Some(180_000)).await;
        let day = days_ago(5);
        for n in 0..600 {
            scrobble(&pool, bot, song, at(day, 7200 + n * 6), None).await;
        }
        let rules = ruleset(&pool).await;

        let first = cdb::classify_user_day(&pool, &rules, bot, day, false)
            .await
            .unwrap();
        let allowed = rules.params.budget_ms() / 180_000;
        assert_eq!(first.counts.counted, allowed);
        assert_eq!(first.counts.suspect, 600 - allowed);
        let stored = flags(&pool, bot).await;
        assert_eq!(stored.len() as i64, first.counts.suspect);
        assert!(
            stored
                .iter()
                .all(|(_, s, r)| s == "suspect" && r == "listening_budget")
        );

        let second = cdb::classify_user_day(&pool, &rules, bot, day, false)
            .await
            .unwrap();
        assert!(second.changes.is_empty());
        assert_eq!(second.counts, first.counts);
        assert_eq!(flags(&pool, bot).await, stored);
        assert_eq!(
            label_counts(&pool, bot).await,
            [
                (Some("counted".into()), allowed),
                (Some("suspect".into()), 600 - allowed)
            ]
        );

        // Shadow mode: charts still count every scrobble.
        scrobbles_db::refresh_scrobble_aggregates(&pool, at(day, 0), at(day, 0))
            .await
            .unwrap();
        let top = scrobbles_db::get_top_tracks(&pool, bot, at(day, 0), 10)
            .await
            .unwrap();
        assert_eq!(top[0].play_count, 600);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn dry_run_reports_without_writing() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "carol").await;
        let unknown = track(&pool, "Unknown", None).await;
        let day = days_ago(2);
        scrobble(&pool, user_id, unknown, at(day, 100), None).await;

        let outcome = cdb::classify_user_day(&pool, &ruleset(&pool).await, user_id, day, true)
            .await
            .unwrap();
        assert_eq!(outcome.counts.no_data, 1);
        assert_eq!(outcome.changes.get(&(None, Status::NoData)), Some(&1));
        assert_eq!(label_counts(&pool, user_id).await, [(None, 1)]);
        assert!(flags(&pool, user_id).await.is_empty());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn sweep_repairs_lost_dropped_and_outdated_days() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "dave").await;
        let song = track(&pool, "Song", Some(200_000)).await;
        let (d1, d2, d3) = (days_ago(9), days_ago(8), days_ago(7));
        for day in [d1, d2, d3] {
            scrobble(&pool, user_id, song, at(day, 600), None).await;
        }
        let rules = ruleset(&pool).await;
        for day in [d1, d2, d3] {
            cdb::classify_user_day(&pool, &rules, user_id, day, false)
                .await
                .unwrap();
        }
        assert_eq!(sweep(&pool, rules.id).await, 0);

        // Dropped classification.
        sqlx::query("DELETE FROM scrobble_classification_days WHERE day = $1")
            .bind(d1)
            .execute(&pool)
            .await
            .unwrap();
        // A scrobble whose queue entry was claimed by a worker that crashed.
        scrobble(&pool, user_id, song, at(d2, 7200), None).await;
        cdb::enqueue_scrobble_classification(&pool, user_id, at(d2, 7200), at(d2, 7200))
            .await
            .unwrap();
        assert_eq!(
            cdb::claim_due(&pool, 10).await.unwrap(),
            [QueuedDay { user_id, day: d2 }]
        );

        assert_eq!(sweep(&pool, rules.id).await, 2);
        let mut days: Vec<NaiveDate> = queued(&pool).await.iter().map(|q| q.1).collect();
        days.sort();
        assert_eq!(days, [d1, d2]);

        for q in cdb::claim_due(&pool, 10).await.unwrap() {
            cdb::classify_user_day(&pool, &rules, q.user_id, q.day, false)
                .await
                .unwrap();
        }
        assert_eq!(sweep(&pool, rules.id).await, 0);

        // Changed thresholds make every day stale.
        let stricter = cdb::register_ruleset(&pool, BudgetParams::new(1800, 1.5, 0).unwrap())
            .await
            .unwrap();
        assert_ne!(stricter.id, rules.id);
        assert_eq!(sweep(&pool, stricter.id).await, 3);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn late_scrobble_before_midnight_requeues_the_next_day() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "erin").await;
        let song = track(&pool, "Song", Some(200_000)).await;
        let (day, next) = (days_ago(4), days_ago(3));
        scrobble(&pool, user_id, song, at(next, 300), None).await;
        let rules = ruleset(&pool).await;
        cdb::classify_user_day(&pool, &rules, user_id, next, false)
            .await
            .unwrap();

        scrobble(&pool, user_id, song, at(day, 86_400 - 600), None).await;
        cdb::classify_user_day(&pool, &rules, user_id, day, false)
            .await
            .unwrap();
        assert_eq!(queued(&pool).await, [(user_id, next, cdb::PRIORITY_INGEST)]);

        // Once the next day has seen the new lookback, nothing is requeued.
        sqlx::query("DELETE FROM classification_queue")
            .execute(&pool)
            .await
            .unwrap();
        cdb::classify_user_day(&pool, &rules, user_id, next, false)
            .await
            .unwrap();
        cdb::classify_user_day(&pool, &rules, user_id, day, false)
            .await
            .unwrap();
        assert!(queued(&pool).await.is_empty());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn enrichment_turns_no_data_into_a_verdict() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "frank").await;
        let song = track(&pool, "Untimed", None).await;
        let day = days_ago(6);
        for n in 0..3 {
            scrobble(&pool, user_id, song, at(day, n * 300), None).await;
        }
        let rules = ruleset(&pool).await;
        cdb::classify_user_day(&pool, &rules, user_id, day, false)
            .await
            .unwrap();
        assert_eq!(
            label_counts(&pool, user_id).await,
            [(Some("no_data".into()), 3)]
        );
        assert_eq!(sweep(&pool, rules.id).await, 0);

        sqlx::query("UPDATE tracks SET mb_duration_ms = 240000 WHERE id = $1")
            .bind(song.0)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(sweep(&pool, rules.id).await, 1);
        let outcome = cdb::classify_user_day(&pool, &rules, user_id, day, false)
            .await
            .unwrap();
        assert_eq!(
            outcome
                .changes
                .get(&(Some(Status::NoData), Status::Counted)),
            Some(&3)
        );

        sqlx::query("DELETE FROM classification_queue")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            cdb::mark_track_durations_changed(&pool, song.0)
                .await
                .unwrap(),
            1
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn import_hook_queues_every_touched_day() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "gina").await;
        let (from, to) = (at(days_ago(12), 3600), at(days_ago(10), 60));
        assert_eq!(
            cdb::enqueue_scrobble_classification(&pool, user_id, from, to)
                .await
                .unwrap(),
            3
        );
        assert!(
            queued(&pool)
                .await
                .iter()
                .all(|q| q.2 == cdb::PRIORITY_IMPORT)
        );
        assert_eq!(cdb::claim_due(&pool, 10).await.unwrap().len(), 3);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn reclassifying_compressed_history_leaves_chunks_compressed() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "hank").await;
        let song = track(&pool, "Old", Some(180_000)).await;
        let day = days_ago(90);
        for n in 0..100 {
            scrobble(&pool, user_id, song, at(day, n * 20), None).await;
        }
        let compressed: Vec<String> = sqlx::query_scalar(
            "SELECT compress_chunk(c)::text FROM show_chunks('scrobbles', older_than => INTERVAL '60 days') c",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(compressed.len(), 1);

        let rules = ruleset(&pool).await;
        let outcome = cdb::classify_user_day(&pool, &rules, user_id, day, false).await.unwrap();
        assert!(outcome.counts.suspect > 0);
        cdb::classify_user_day(&pool, &rules, user_id, day, false).await.unwrap();

        let still: bool = sqlx::query_scalar(
            "SELECT bool_and(is_compressed) FROM timescaledb_information.chunks WHERE hypertable_name = 'scrobbles'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(still);
    })
    .await;
}
