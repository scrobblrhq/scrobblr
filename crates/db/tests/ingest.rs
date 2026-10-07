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
