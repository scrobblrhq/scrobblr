mod common;

use chrono::{DateTime, TimeDelta, Utc};
use common::with_db;
use db::queries::imports::{self, ClaimedImport, ImportPlay, NewImport, Page, PageOutcome};
use db::queries::scrobbles as scrobbles_db;
use shared::lastfm::Cursor;
use shared::scrobble::ScrobbleInput;
use sqlx::PgPool;

const LEASE: f64 = 60.0;

async fn user(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash) VALUES ($1, $1 || '@test', 'x') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn start(pool: &PgPool, user_id: i64) -> ClaimedImport {
    let job = imports::create(
        pool,
        &NewImport {
            user_id,
            provider: imports::PROVIDER_LASTFM,
            external_user: "lastfm_name",
            verified: false,
            window_from: None,
        },
    )
    .await
    .unwrap();
    imports::claim(pool, job.id, LEASE).await.unwrap().unwrap()
}

fn days_ago(days: i64, minute: i64) -> DateTime<Utc> {
    let base = (Utc::now() - TimeDelta::days(days))
        .date_naive()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    base.and_utc() + TimeDelta::minutes(minute)
}

fn play(at: DateTime<Utc>, artist: &str, track: &str, album: Option<&str>) -> ImportPlay {
    ImportPlay {
        played_at: at,
        artist: artist.into(),
        track: track.into(),
        album: album.map(Into::into),
        track_mbid: None,
    }
}

async fn record(pool: &PgPool, job: &ClaimedImport, plays: &[ImportPlay]) -> PageOutcome {
    let page = Page {
        plays,
        next: Cursor {
            segment_to: 1_000,
            page: 2,
            segment_oldest: Some(999),
        },
        fetched: plays.len() as i64,
        skipped: 0,
        total_expected: 500,
    };
    imports::record_page(pool, job, &page, LEASE).await.unwrap()
}

async fn scalar(pool: &PgPool, sql: &str, id: i64) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_page_lands_with_catalog_counters_and_progress() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "alice").await;
        let job = start(&pool, user_id).await;
        let mbid: uuid::Uuid = "8f2bc1b0-9c33-4f25-8e65-d2dbd1c9a5b1".parse().unwrap();
        let plays = [
            ImportPlay { track_mbid: Some(mbid), ..play(days_ago(3, 9), "Radiohead", "Airbag", Some("OK Computer")) },
            play(days_ago(3, 4), "radiohead", "Airbag", Some("OK Computer")),
            play(days_ago(3, 0), "Björk", "Hyperballad", None),
        ];
        assert_eq!(
            record(&pool, &job, &plays).await,
            PageOutcome::Recorded { imported: 3, duplicates: 0 }
        );

        let rows: Vec<(String, Option<i64>, String)> = sqlx::query_as(
            "SELECT t.title, s.import_id, s.source FROM scrobbles s JOIN tracks t ON t.id = s.track_id ORDER BY s.played_at",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|(_, import_id, source)| *import_id == Some(job.id) && source == "lastfm_import"));

        // One artist row for both spellings, counters set-based.
        assert_eq!(scalar(&pool, "SELECT count(*) FROM artists WHERE id > $1", 0).await, 2);
        assert_eq!(scalar(&pool, "SELECT scrobble_count FROM users WHERE id = $1", user_id).await, 3);
        let airbag: (i64, i64, Option<uuid::Uuid>, Option<i64>) = sqlx::query_as(
            "SELECT id, scrobble_count, mbid_hint, album_id FROM tracks WHERE title = 'Airbag'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((airbag.1, airbag.2), (2, Some(mbid)));
        assert_eq!(scalar(&pool, "SELECT scrobble_count FROM albums WHERE id = $1", airbag.3.unwrap()).await, 2);
        assert_eq!(
            scalar(&pool, "SELECT count(*) FROM track_artists WHERE role = 'primary' AND track_id = $1", airbag.0).await,
            1
        );
        // History is older than now: last_seen_at is the newest play, not the oldest.
        let last_seen: DateTime<Utc> = sqlx::query_scalar("SELECT last_seen_at FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(last_seen, days_ago(3, 9));

        let listed = imports::get(&pool, job.id, Some(user_id)).await.unwrap().unwrap();
        assert_eq!((listed.fetched, listed.imported, listed.total_expected), (3, 3, Some(500)));
        assert_eq!(listed.oldest_played_at, Some(days_ago(3, 0)));
        assert_eq!(
            imports::pending_range(&pool, job.id).await.unwrap(),
            Some((days_ago(3, 0), days_ago(3, 9)))
        );
        let resumed = imports::claim(&pool, job.id, LEASE).await.unwrap();
        assert!(resumed.is_none(), "still leased");
        imports::release(&pool, &job).await.unwrap();
        let resumed = imports::claim(&pool, job.id, LEASE).await.unwrap().unwrap();
        assert_eq!(resumed.cursor, Cursor { segment_to: 1_000, page: 2, segment_oldest: Some(999) });
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn re_recording_a_page_adds_nothing() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "bob").await;
        let job = start(&pool, user_id).await;
        let plays: Vec<ImportPlay> = (0..50)
            .map(|n| {
                play(
                    days_ago(5, n * 4),
                    "Artist",
                    &format!("Song {}", n % 7),
                    Some("Album"),
                )
            })
            .collect();
        record(&pool, &job, &plays).await;
        assert_eq!(
            record(&pool, &job, &plays).await,
            PageOutcome::Recorded {
                imported: 0,
                duplicates: 50
            }
        );
        assert_eq!(
            scalar(
                &pool,
                "SELECT count(*) FROM scrobbles WHERE user_id = $1",
                user_id
            )
            .await,
            50
        );
        assert_eq!(
            scalar(
                &pool,
                "SELECT scrobble_count FROM users WHERE id = $1",
                user_id
            )
            .await,
            50
        );
        assert_eq!(
            scalar(
                &pool,
                "SELECT sum(scrobble_count)::bigint FROM tracks WHERE id > $1",
                0
            )
            .await,
            50
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn plays_already_scrobbled_live_are_skipped() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "carol").await;
        let input = ScrobbleInput {
            track_title: "Teardrop".into(),
            artist_name: "Massive Attack".into(),
            featured_artists: vec![],
            album_title: None,
            played_at: days_ago(2, 5),
            duration_ms: Some(330_000),
            listened_ms: None,
            source: "extension".into(),
            client_id: None,
            recording_mbid: None,
        };
        scrobbles_db::ingest_scrobble(&pool, user_id, &input)
            .await
            .unwrap();

        let job = start(&pool, user_id).await;
        let plays = [
            play(days_ago(2, 0), "Massive Attack", "Teardrop", None),
            play(days_ago(2, 30), "Massive Attack", "Teardrop", None),
        ];
        assert_eq!(
            record(&pool, &job, &plays).await,
            PageOutcome::Recorded {
                imported: 1,
                duplicates: 1
            }
        );
        // The live row still went through the trigger; the import did not double it.
        assert_eq!(
            scalar(
                &pool,
                "SELECT scrobble_count FROM users WHERE id = $1",
                user_id
            )
            .await,
            2
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn cancelling_stops_the_page_in_flight_and_one_import_runs_per_user() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "dave").await;
        let job = start(&pool, user_id).await;
        let again = imports::create(
            &pool,
            &NewImport {
                user_id,
                provider: imports::PROVIDER_LASTFM,
                external_user: "other",
                verified: true,
                window_from: None,
            },
        )
        .await;
        assert!(
            matches!(again, Err(imports::CreateImportError::AlreadyActive(id)) if id == job.id)
        );

        assert!(imports::cancel(&pool, job.id, Some(user_id)).await.unwrap());
        assert_eq!(
            record(&pool, &job, &[play(days_ago(1, 0), "A", "B", None)]).await,
            PageOutcome::LeaseLost
        );
        assert_eq!(
            scalar(
                &pool,
                "SELECT count(*) FROM scrobbles WHERE user_id = $1",
                user_id
            )
            .await,
            0
        );
        assert!(
            imports::active_for_user(&pool, user_id)
                .await
                .unwrap()
                .is_none()
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn deferred_jobs_wait_and_leases_expire() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "erin").await;
        let job = start(&pool, user_id).await;
        assert!(imports::claim_next(&pool, LEASE).await.unwrap().is_none());

        imports::defer(&pool, &job, 1, TimeDelta::minutes(5), "last.fm error 8")
            .await
            .unwrap();
        assert!(imports::claim_next(&pool, LEASE).await.unwrap().is_none());
        let listed = imports::get(&pool, job.id, None).await.unwrap().unwrap();
        assert!(listed.retrying_at.is_some());

        sqlx::query("UPDATE scrobble_imports SET next_attempt_at = NOW() WHERE id = $1")
            .bind(job.id)
            .execute(&pool)
            .await
            .unwrap();
        let claimed = imports::claim_next(&pool, 0.0).await.unwrap().unwrap();
        assert_ne!(claimed.lease_token, job.lease_token);
        // A zero-second lease is already expired: another worker may take over,
        // and the first one's writes no longer apply.
        let taken = imports::claim_next(&pool, LEASE).await.unwrap().unwrap();
        assert_eq!(
            record(&pool, &claimed, &[play(days_ago(1, 0), "A", "B", None)]).await,
            PageOutcome::LeaseLost
        );
        imports::finish(&pool, &taken).await.unwrap();
        let done = imports::get(&pool, job.id, None).await.unwrap().unwrap();
        assert_eq!(done.status, shared::models::ImportStatus::Done);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn old_history_goes_into_compressed_chunks_without_decompressing_them() {
    with_db(true, |pool| async move {
        // The policy's first run waits for the database's scheduler, which
        // can start mid-test and recompress the rows this test counts.
        sqlx::query(
            "SELECT alter_job(job_id, scheduled => false) FROM timescaledb_information.jobs
             WHERE proc_name = 'policy_compression'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let neighbour = user(&pool, "neighbour").await;
        let neighbour_job = start(&pool, neighbour).await;
        let old: Vec<ImportPlay> = (0..300)
            .map(|n| play(days_ago(120, n * 3), "Old Band", &format!("Track {}", n % 20), None))
            .collect();
        record(&pool, &neighbour_job, &old).await;
        let compressed: Vec<String> = sqlx::query_scalar(
            "SELECT compress_chunk(c)::text FROM show_chunks('scrobbles', older_than => INTERVAL '60 days') c",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(!compressed.is_empty());

        let user_id = user(&pool, "frank").await;
        let job = start(&pool, user_id).await;
        let plays: Vec<ImportPlay> = (0..200)
            .map(|n| play(days_ago(120, n * 4 + 1), "Old Band", &format!("Track {}", n % 20), None))
            .collect();
        assert_eq!(
            record(&pool, &job, &plays).await,
            PageOutcome::Recorded { imported: 200, duplicates: 0 }
        );
        assert_eq!(
            record(&pool, &job, &plays).await,
            PageOutcome::Recorded { imported: 0, duplicates: 200 }
        );

        let (compressed_chunks, still_compressed): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE is_compressed) FROM timescaledb_information.chunks
             WHERE hypertable_name = 'scrobbles' AND range_end < NOW() - INTERVAL '60 days'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(compressed_chunks, still_compressed);
        // The neighbour's rows stay compressed; only the new ones sit uncompressed.
        let uncompressed: i64 = sqlx::query_scalar(
            "SELECT sum((xpath('/row/n/text()', query_to_xml(format('SELECT count(*) AS n FROM ONLY %s', c), false, true, '')))[1]::text::bigint)::bigint
             FROM show_chunks('scrobbles', older_than => INTERVAL '60 days') c",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(uncompressed, 200);
        assert_eq!(scalar(&pool, "SELECT count(*) FROM scrobbles WHERE user_id = $1", user_id).await, 200);
        assert_eq!(scalar(&pool, "SELECT scrobble_count FROM users WHERE id = $1", user_id).await, 200);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn checkpoint_enrichment_puts_the_most_played_first_below_live_ingest() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "gina").await;
        let job = start(&pool, user_id).await;
        let mut plays: Vec<ImportPlay> = (0..64)
            .map(|n| play(days_ago(4, n * 4), "Favourite", "Anthem", Some("Hits")))
            .collect();
        plays.push(play(days_ago(4, 1000), "Stranger", "Once", None));
        record(&pool, &job, &plays).await;

        let range = imports::pending_range(&pool, job.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            imports::enqueue_enrichment(&pool, job.id, user_id, range)
                .await
                .unwrap(),
            5
        );
        imports::clear_pending(&pool, job.id, range).await.unwrap();
        assert!(
            imports::pending_range(&pool, job.id)
                .await
                .unwrap()
                .is_none()
        );

        let jobs: Vec<(String, i32)> = sqlx::query_as(
            "SELECT e.entity_type || ':' || COALESCE(t.title, a.name, al.title), e.priority
             FROM enrichment_jobs e
             LEFT JOIN tracks t ON e.entity_type = 'track' AND t.id = e.entity_id
             LEFT JOIN artists a ON e.entity_type = 'artist' AND a.id = e.entity_id
             LEFT JOIN albums al ON e.entity_type = 'album' AND al.id = e.entity_id
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            jobs,
            [
                ("album:Hits".into(), 26),
                ("artist:Favourite".into(), 26),
                ("artist:Stranger".into(), 20),
                ("track:Anthem".into(), 26),
                ("track:Once".into(), 20),
            ]
        );
    })
    .await;
}
