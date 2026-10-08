mod common;

use chrono::{DateTime, TimeDelta, Utc};
use common::with_db;
use db::queries::scrobble_clients::{self as clients_db, ClientIdentity, PROTOCOL_LISTENBRAINZ};
use db::queries::scrobbles as scrobbles_db;
use shared::scrobble::ScrobbleInput;
use sqlx::PgPool;

async fn user(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash) VALUES ($1, $1 || '@test', 'x') RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn input(title: &str, played_at: DateTime<Utc>, client_id: Option<i32>) -> ScrobbleInput {
    ScrobbleInput {
        track_title: title.into(),
        artist_name: "Artist".into(),
        featured_artists: vec![],
        album_title: None,
        played_at,
        duration_ms: Some(200_000),
        listened_ms: None,
        source: "test".into(),
        client_id,
        recording_mbid: None,
    }
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn clients_are_registered_once_and_recorded_on_scrobbles() {
    with_db(true, |pool| async move {
        let web_scrobbler =
            ClientIdentity::new(PROTOCOL_LISTENBRAINZ, "  Web Scrobbler 3.10  ", false);
        assert_eq!(web_scrobbler.name, "Web Scrobbler 3.10");
        let id = clients_db::resolve_client(&pool, &web_scrobbler)
            .await
            .unwrap();
        assert_eq!(
            clients_db::resolve_client(&pool, &web_scrobbler)
                .await
                .unwrap(),
            id
        );
        let verified = ClientIdentity {
            verified: true,
            ..web_scrobbler.clone()
        };
        assert_ne!(
            clients_db::resolve_client(&pool, &verified).await.unwrap(),
            id
        );
        assert_eq!(
            ClientIdentity::new(PROTOCOL_LISTENBRAINZ, " ", false).name,
            "unknown"
        );
        assert_eq!(
            ClientIdentity::new(PROTOCOL_LISTENBRAINZ, &"x".repeat(500), false)
                .name
                .chars()
                .count(),
            clients_db::MAX_NAME_CHARS
        );

        let user_id = user(&pool, "kim").await;
        let at = Utc::now() - TimeDelta::hours(1);
        scrobbles_db::ingest_scrobble(&pool, user_id, &input("Song", at, Some(id)))
            .await
            .unwrap();
        let stored: Option<i32> = sqlx::query_scalar("SELECT client_id FROM scrobbles")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, Some(id));
    })
    .await;
}

async fn count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM scrobbles")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn retried_batches_never_duplicate_in_any_order() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "lee").await;
        let start = Utc::now() - TimeDelta::days(3);
        let batch: Vec<ScrobbleInput> = (0..5)
            .map(|n| {
                input(
                    &format!("Song {n}"),
                    start + TimeDelta::minutes(4 * n),
                    None,
                )
            })
            .collect();
        for play in &batch {
            scrobbles_db::ingest_scrobble(&pool, user_id, play)
                .await
                .unwrap();
        }
        // A newer scrobble lands, then the whole batch is retried, reversed.
        scrobbles_db::ingest_scrobble(&pool, user_id, &input("Later", Utc::now(), None))
            .await
            .unwrap();
        for play in batch.iter().rev() {
            assert!(matches!(
                scrobbles_db::ingest_scrobble(&pool, user_id, play).await,
                Err(scrobbles_db::IngestError::Duplicate)
            ));
        }
        // A client clock a few seconds off still matches.
        let shifted = ScrobbleInput {
            played_at: batch[2].played_at + TimeDelta::seconds(7),
            ..batch[2].clone()
        };
        assert!(matches!(
            scrobbles_db::ingest_scrobble(&pool, user_id, &shifted).await,
            Err(scrobbles_db::IngestError::Duplicate)
        ));
        assert_eq!(count(&pool).await, 6);

        // Another track at the same moment, or the same one later, is new.
        let other = input("Other", batch[2].played_at, None);
        scrobbles_db::ingest_scrobble(&pool, user_id, &other)
            .await
            .unwrap();
        let replay = ScrobbleInput {
            played_at: batch[2].played_at + TimeDelta::minutes(4),
            ..batch[2].clone()
        };
        scrobbles_db::ingest_scrobble(&pool, user_id, &replay)
            .await
            .unwrap();
        assert_eq!(count(&pool).await, 8);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn concurrent_copies_of_one_submission_insert_once() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "max").await;
        let play = input("Song", Utc::now() - TimeDelta::hours(2), None);
        // Resolve the catalog first so both copies race on the insert.
        scrobbles_db::ingest_scrobble(
            &pool,
            user_id,
            &input("Song", Utc::now() - TimeDelta::days(1), None),
        )
        .await
        .unwrap();
        let results = futures_util::future::join_all(
            (0..8).map(|_| scrobbles_db::ingest_scrobble(&pool, user_id, &play)),
        )
        .await;
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(count(&pool).await, 2);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn now_playing_without_an_album_reads_back() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "nia").await;
        let artist = db::queries::tracks::find_or_create_artist(&pool, "Artist")
            .await
            .unwrap();
        let track =
            db::queries::tracks::find_or_create_track(&pool, artist.id, None, "Single", None)
                .await
                .unwrap();
        scrobbles_db::upsert_now_playing(
            &pool,
            &scrobbles_db::UpsertNowPlaying {
                user_id,
                track_id: track.id,
                artist_id: artist.id,
                album_id: None,
                source: "test".into(),
                expires_at: Utc::now() + TimeDelta::minutes(3),
            },
        )
        .await
        .unwrap();
        let playing = scrobbles_db::get_now_playing(&pool, user_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (playing.track_title.as_str(), playing.album_title),
            ("Single", None)
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn client_mbids_become_hints_and_the_first_one_wins() {
    with_db(true, |pool| async move {
        let user_id = user(&pool, "hinter").await;
        let first: uuid::Uuid = "8f2bc1b0-9c33-4f25-8e65-d2dbd1c9a5b1".parse().unwrap();
        let other: uuid::Uuid = "0b2a7c71-4f0d-4c3e-9f6b-1f0c2d3e4a5b".parse().unwrap();
        let play = |title: &str, minutes: i64, mbid: Option<uuid::Uuid>| ScrobbleInput {
            recording_mbid: mbid,
            ..input(title, Utc::now() - TimeDelta::minutes(minutes), None)
        };
        let hint = async |title: &str| -> (i64, Option<uuid::Uuid>) {
            sqlx::query_as("SELECT id, mbid_hint FROM tracks WHERE title = $1")
                .bind(title)
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        scrobbles_db::ingest_scrobble(&pool, user_id, &play("Song", 40, Some(first)))
            .await
            .unwrap();
        scrobbles_db::ingest_scrobble(&pool, user_id, &play("Song", 30, Some(other)))
            .await
            .unwrap();
        assert_eq!(hint("Song").await.1, Some(first));

        // A track enriched without a match gets one more look once a hint
        // arrives.
        scrobbles_db::ingest_scrobble(&pool, user_id, &play("Later", 20, None))
            .await
            .unwrap();
        let (later, _) = hint("Later").await;
        sqlx::query("UPDATE tracks SET enriched_at = NOW() WHERE id = $1")
            .bind(later)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE enrichment_jobs SET status = 'done' WHERE entity_type = 'track' AND entity_id = $1")
            .bind(later)
            .execute(&pool)
            .await
            .unwrap();
        scrobbles_db::ingest_scrobble(&pool, user_id, &play("Later", 10, Some(other)))
            .await
            .unwrap();
        assert_eq!(hint("Later").await.1, Some(other));
        let status: String = sqlx::query_scalar(
            "SELECT status FROM enrichment_jobs WHERE entity_type = 'track' AND entity_id = $1",
        )
        .bind(later)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "pending");
    })
    .await;
}
