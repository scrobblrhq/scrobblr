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
