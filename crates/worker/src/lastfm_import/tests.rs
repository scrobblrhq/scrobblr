use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;

use super::{Importer, LEASE_SECS, SliceEnd};
use crate::enrichment::ratelimit::RateLimiter;
use crate::fake_lastfm::{API_KEY, Failure, FakeLastfm, FakePlay};
use crate::test_support::with_db;
use db::queries::imports::{self as imports_db, ImportPlay, NewImport, Page, PageOutcome};
use shared::lastfm::{Cursor, LastfmClient, LastfmError, RecentTracksQuery};
use shared::models::{ImportStatus, ScrobbleImport};

fn client(url: &str) -> LastfmClient {
    LastfmClient::new(reqwest::Client::new(), url, API_KEY, None)
}

fn importer(pool: &PgPool, url: &str, max_scrobbles: i64) -> Importer {
    Importer::new(
        pool.clone(),
        client(url),
        Arc::new(RateLimiter::new(Duration::ZERO)),
        max_scrobbles,
    )
}

/// 4,500 plays ten minutes apart, plus: 250 distinct tracks scrobbled in
/// the same second right across the first segment boundary, five exact
/// double scrobbles, and three plays without an artist.
fn history() -> Vec<FakePlay> {
    let newest = Utc::now().timestamp() - 60;
    let mut plays: Vec<FakePlay> = (0..4500)
        .map(|n| {
            let album = if n % 10 == 0 {
                String::new()
            } else {
                format!("Album {}", n % 53)
            };
            FakePlay::new(
                newest - n * 600,
                &format!("Artist {}", n % 37),
                &format!("Song {}", n % 300),
                &album,
            )
        })
        .collect();
    let burst = newest - 3900 * 600 + 300;
    plays.extend((0..250).map(|k| FakePlay::new(burst, "Burst", &format!("Burst {k}"), "")));
    plays.extend(plays[100..105].to_vec());
    plays.extend((0..3).map(|n| FakePlay::new(newest - n * 600 - 7, "", "Nameless", "")));
    plays
}

const UNIQUE_PLAYS: i64 = 4500 + 250;

async fn user(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash) VALUES ($1, $1 || '@test', 'x') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn new_import(pool: &PgPool, user_id: i64, lastfm: &str) -> i64 {
    let window_from = imports_db::reimport_from(pool, user_id, imports_db::PROVIDER_LASTFM, lastfm)
        .await
        .unwrap();
    imports_db::create(
        pool,
        &NewImport {
            user_id,
            provider: imports_db::PROVIDER_LASTFM,
            external_user: lastfm,
            verified: false,
            window_from,
        },
    )
    .await
    .unwrap()
    .id
}

/// Runs a job to the end the way the worker would, in short slices and
/// without waiting out retry delays.
async fn drive(importer: &Importer, pool: &PgPool, id: i64) -> (SliceEnd, ScrobbleImport) {
    loop {
        let job = imports_db::claim(pool, id, LEASE_SECS)
            .await
            .unwrap()
            .expect("claimable");
        match importer.process(job, 7).await.unwrap() {
            SliceEnd::Paused | SliceEnd::Deferred(_) => continue,
            end => return (end, imports_db::get(pool, id, None).await.unwrap().unwrap()),
        }
    }
}

async fn count(pool: &PgPool, sql: &str, id: i64) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn client_reads_pages_and_errors_from_the_fake_server() {
    let fake = FakeLastfm::default();
    fake.user("rj", history());
    fake.hidden_user("shy");
    let url = fake.start().await;
    let client = client(&url);
    let query = |user| RecentTracksQuery {
        user,
        from: None,
        to: None,
        page: 1,
        session_key: None,
    };

    let page = client.recent_tracks(&query("RJ")).await.unwrap();
    assert_eq!((page.dated(), page.invalid), (200, 3));
    assert_eq!(page.total, history().len() as u64);

    let error = client.recent_tracks(&query("nobody")).await.unwrap_err();
    assert_eq!(error.code(), Some(6));
    assert_eq!(
        client
            .recent_tracks(&query("shy"))
            .await
            .unwrap_err()
            .code(),
        Some(17)
    );

    fake.fail_next(&[Failure::Status(503), Failure::Html, Failure::Error(29)]);
    let busy = client.recent_tracks(&query("rj")).await.unwrap_err();
    assert!(matches!(busy, LastfmError::Status(503)) && busy.is_transient());
    let html = client.recent_tracks(&query("rj")).await.unwrap_err();
    assert!(matches!(html, LastfmError::Parse(_)) && html.is_transient());
    assert!(
        client
            .recent_tracks(&query("rj"))
            .await
            .unwrap_err()
            .is_rate_limited()
    );

    fake.length("Cher", "Believe", 239_000);
    assert_eq!(
        client.track_duration("cher", "believe").await.unwrap(),
        Some(239_000)
    );
    assert_eq!(
        client.track_duration("Cher", "Unknown").await.unwrap(),
        None
    );
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn imports_a_whole_history_and_a_reimport_adds_nothing() {
    with_db(|pool| async move {
        let fake = FakeLastfm::default();
        fake.user("rj", history());
        let url = fake.start().await;
        let importer = importer(&pool, &url, 1_000_000);
        let user_id = user(&pool, "alice").await;

        let id = new_import(&pool, user_id, "rj").await;
        let (end, done) = drive(&importer, &pool, id).await;
        assert_eq!(end, SliceEnd::Done);
        assert_eq!(done.status, ImportStatus::Done);
        assert_eq!(done.imported, UNIQUE_PLAYS);
        assert_eq!(done.skipped, 3);
        assert_eq!(done.total_expected, Some(history().len() as i64));
        assert!(done.duplicates >= 5);
        // About one request per 200 scrobbles, plus the segment overlaps.
        assert!(fake.requests() <= history().len() / 200 + 4, "{}", fake.requests());

        assert_eq!(count(&pool, "SELECT count(*) FROM scrobbles WHERE user_id = $1", user_id).await, UNIQUE_PLAYS);
        assert_eq!(count(&pool, "SELECT scrobble_count FROM users WHERE id = $1", user_id).await, UNIQUE_PLAYS);
        assert_eq!(
            count(&pool, "SELECT count(*) FROM scrobbles WHERE user_id = $1 AND source = 'lastfm_import' AND import_id IS NOT NULL", user_id).await,
            UNIQUE_PLAYS
        );
        // Checkpoints ran: aggregates refreshed, days queued for the
        // classifier, catalog queued for enrichment, nothing left pending.
        assert_eq!(
            count(&pool, "SELECT sum(scrobble_count)::bigint FROM user_activity_daily WHERE user_id = $1", user_id).await,
            UNIQUE_PLAYS
        );
        let days = count(&pool, "SELECT count(DISTINCT (played_at AT TIME ZONE 'UTC')::date) FROM scrobbles WHERE user_id = $1", user_id).await;
        assert!(count(&pool, "SELECT count(*) FROM classification_queue WHERE user_id = $1", user_id).await >= days);
        assert!(count(&pool, "SELECT count(*) FROM enrichment_jobs WHERE entity_type = 'track' AND priority BETWEEN $1 AND 29", 20).await >= 550);
        assert!(imports_db::pending_range(&pool, id).await.unwrap().is_none());

        // A second, incremental import only rescans the last two weeks…
        let again = new_import(&pool, user_id, "rj").await;
        let (_, incremental) = drive(&importer, &pool, again).await;
        assert_eq!(incremental.imported, 0);
        assert!(incremental.window_from.is_some());
        assert!(incremental.fetched < done.fetched / 2);
        // …and even a full rescan adds nothing.
        sqlx::query("UPDATE scrobble_imports SET window_from = NULL WHERE id = $1")
            .bind(new_import(&pool, user_id, "rj").await)
            .execute(&pool)
            .await
            .unwrap();
        let full = imports_db::active_for_user(&pool, user_id).await.unwrap().unwrap();
        let (_, full) = drive(&importer, &pool, full.id).await;
        assert_eq!((full.imported, full.status), (0, ImportStatus::Done));
        assert_eq!(count(&pool, "SELECT count(*) FROM scrobbles WHERE user_id = $1", user_id).await, UNIQUE_PLAYS);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn resumes_after_errors_and_a_dead_worker_without_duplicates() {
    with_db(|pool| async move {
        let fake = FakeLastfm::default();
        fake.user("rj", history());
        fake.to_exclusive();
        let url = fake.start().await;
        let importer = importer(&pool, &url, 1_000_000);
        let user_id = user(&pool, "bob").await;
        let id = new_import(&pool, user_id, "rj").await;

        let job = imports_db::claim(&pool, id, LEASE_SECS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(importer.process(job, 3).await.unwrap(), SliceEnd::Paused);

        fake.fail_next(&[Failure::Status(502), Failure::Error(8), Failure::Html]);
        for attempt in 1..=3 {
            let job = imports_db::claim(&pool, id, LEASE_SECS)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                importer.process(job, 3).await.unwrap(),
                SliceEnd::Deferred(_)
            ));
            let state = imports_db::get(&pool, id, None).await.unwrap().unwrap();
            assert!(state.retrying_at.is_some(), "attempt {attempt}");
        }

        // A worker that dies holding the lease: the job is claimable again
        // once the lease runs out, and picks up at the saved cursor.
        let dead = imports_db::claim(&pool, id, 0.0).await.unwrap().unwrap();
        let fetched_before = imports_db::get(&pool, id, None)
            .await
            .unwrap()
            .unwrap()
            .fetched;
        drop(dead);
        let (end, done) = drive(&importer, &pool, id).await;
        assert_eq!(end, SliceEnd::Done);
        assert_eq!(done.imported, UNIQUE_PLAYS);
        assert!(done.retrying_at.is_none());
        assert!(fetched_before >= 600);
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM scrobbles WHERE user_id = $1",
                user_id
            )
            .await,
            UNIQUE_PLAYS
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn unknown_hidden_and_empty_histories() {
    with_db(|pool| async move {
        let fake = FakeLastfm::default();
        fake.hidden_user("shy");
        fake.user("quiet", Vec::new());
        let url = fake.start().await;
        let importer = importer(&pool, &url, 1_000_000);

        for (name, lastfm, expected) in [
            ("a", "nobody", SliceEnd::Failed(super::USER_NOT_FOUND)),
            ("b", "shy", SliceEnd::Failed(super::HISTORY_HIDDEN)),
            ("c", "quiet", SliceEnd::Done),
        ] {
            let user_id = user(&pool, name).await;
            let id = new_import(&pool, user_id, lastfm).await;
            let (end, state) = drive(&importer, &pool, id).await;
            assert_eq!(end, expected, "{lastfm}");
            assert_eq!(state.imported, 0);
            assert!(state.finished_at.is_some());
            if lastfm == "quiet" {
                assert_eq!(state.total_expected, Some(0));
            } else {
                assert!(state.error_message.is_some());
            }
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn rate_limits_cost_no_attempts_and_the_cap_stops_an_import() {
    with_db(|pool| async move {
        let fake = FakeLastfm::default();
        fake.user("bot", history());
        let url = fake.start().await;
        let importer = importer(&pool, &url, 500)
            .with_rate_limit_cooldown(chrono::TimeDelta::milliseconds(50));
        let user_id = user(&pool, "dave").await;
        let id = new_import(&pool, user_id, "bot").await;

        fake.fail_next(&[Failure::Error(29)]);
        let job = imports_db::claim(&pool, id, LEASE_SECS)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            importer.process(job, 3).await.unwrap(),
            SliceEnd::Deferred(_)
        ));
        let job = imports_db::claim(&pool, id, LEASE_SECS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.attempts, 0);
        importer.process(job, 1).await.unwrap();

        let (end, capped) = drive(&importer, &pool, id).await;
        assert_eq!(end, SliceEnd::Failed(super::CAP_REACHED));
        assert!((500..700).contains(&capped.imported));
        assert!(
            imports_db::pending_range(&pool, id)
                .await
                .unwrap()
                .is_none()
        );

        // The cap is per user: starting over doesn't get past it.
        let again = new_import(&pool, user_id, "bot").await;
        let (end, _) = drive(&importer, &pool, again).await;
        assert_eq!(end, SliceEnd::Failed(super::CAP_REACHED));
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM scrobbles WHERE user_id = $1",
                user_id
            )
            .await,
            capped.imported
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_cancelled_import_still_checkpoints_what_it_imported() {
    with_db(|pool| async move {
        let importer = importer(&pool, "http://127.0.0.1:9/2.0/", 1_000_000);
        let user_id = user(&pool, "erin").await;
        let id = new_import(&pool, user_id, "rj").await;
        let job = imports_db::claim(&pool, id, LEASE_SECS)
            .await
            .unwrap()
            .unwrap();
        let plays = [ImportPlay {
            played_at: Utc::now() - chrono::TimeDelta::days(3),
            artist: "A".into(),
            track: "B".into(),
            album: None,
            track_mbid: None,
        }];
        let page = Page {
            plays: &plays,
            next: Cursor::start(0),
            fetched: 1,
            skipped: 0,
            total_expected: 1,
        };
        assert!(matches!(
            imports_db::record_page(&pool, &job, &page, LEASE_SECS)
                .await
                .unwrap(),
            PageOutcome::Recorded { imported: 1, .. }
        ));
        assert!(imports_db::cancel(&pool, id, Some(user_id)).await.unwrap());

        importer.checkpoint_leftovers().await;
        assert!(
            imports_db::pending_range(&pool, id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM classification_queue WHERE user_id = $1",
                user_id
            )
            .await,
            1
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn the_cli_imports_in_the_foreground() {
    with_db(|pool| async move {
        let fake = FakeLastfm::default();
        fake.user("rj", history());
        let url = fake.start().await;
        let importer = importer(&pool, &url, 1_000_000);
        let user_id = user(&pool, "alice").await;
        let args = |lastfm: &str| -> Vec<String> {
            ["--user", "Alice", "--lastfm", lastfm]
                .map(String::from)
                .to_vec()
        };

        super::cli::import(&pool, &importer, &args("rj"))
            .await
            .unwrap();
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM scrobbles WHERE user_id = $1",
                user_id
            )
            .await,
            UNIQUE_PLAYS
        );
        // Running it again makes an incremental import that adds nothing.
        super::cli::import(&pool, &importer, &args("RJ"))
            .await
            .unwrap();
        let listed = imports_db::list(&pool, Some(user_id), 10).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert!(
            listed
                .iter()
                .all(|(_, _, i)| i.status == ImportStatus::Done)
        );
        assert_eq!(listed[0].2.imported, 0);

        // One import at a time per user.
        new_import(&pool, user_id, "someone_else").await;
        let error = super::cli::import(&pool, &importer, &args("rj"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already importing"), "{error}");
    })
    .await;
}
