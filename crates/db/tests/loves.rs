mod common;

use chrono::{DateTime, TimeDelta, Utc};
use common::with_db;
use db::queries::loves::{self as loves_db, LoveSource, Loved};
use db::queries::tracks as tracks_db;
use shared::cursor::Cursor;
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

async fn track(pool: &PgPool, title: &str) -> i64 {
    let artist = tracks_db::find_or_create_artist(pool, "Band")
        .await
        .unwrap();
    tracks_db::find_or_create_track(pool, artist.id, None, title, None)
        .await
        .unwrap()
        .id
}

async fn love_at(pool: &PgPool, user_id: i64, track_id: i64, at: DateTime<Utc>) {
    sqlx::query(
        "INSERT INTO loved_tracks (user_id, track_id, loved_at, source) VALUES ($1, $2, $3, 'scrobblr')",
    )
    .bind(user_id)
    .bind(track_id)
    .bind(at)
    .execute(pool)
    .await
    .unwrap();
}

/// Loving twice keeps the first love; unloving twice is no error; the cap
/// counts the user's own loves only, and a love taken back frees a place.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn loves_are_idempotent_and_capped_per_account() {
    with_db(true, |pool| async move {
        let (a, b) = (user(&pool, "a").await, user(&pool, "b").await);
        let tracks = [
            track(&pool, "One").await,
            track(&pool, "Two").await,
            track(&pool, "Three").await,
        ];

        let Loved::Now(first) = loves_db::love(&pool, a, tracks[0], LoveSource::NATIVE, 2)
            .await
            .unwrap()
        else {
            panic!("not loved");
        };
        assert_eq!(
            loves_db::love(&pool, a, tracks[0], LoveSource::NATIVE, 2)
                .await
                .unwrap(),
            Loved::Already(first)
        );
        assert!(matches!(
            loves_db::love(&pool, a, tracks[1], LoveSource::NATIVE, 2)
                .await
                .unwrap(),
            Loved::Now(_)
        ));
        assert_eq!(
            loves_db::love(&pool, a, tracks[2], LoveSource::NATIVE, 2)
                .await
                .unwrap(),
            Loved::AtLimit
        );
        // Already loved stays an answer at the cap.
        assert_eq!(
            loves_db::love(&pool, a, tracks[0], LoveSource::NATIVE, 2)
                .await
                .unwrap(),
            Loved::Already(first)
        );
        assert!(matches!(
            loves_db::love(&pool, b, tracks[2], LoveSource::NATIVE, 2)
                .await
                .unwrap(),
            Loved::Now(_)
        ));

        assert!(loves_db::unlove(&pool, a, tracks[1]).await.unwrap());
        assert!(!loves_db::unlove(&pool, a, tracks[1]).await.unwrap());
        assert_eq!(loves_db::count(&pool, a).await.unwrap(), 1);
        assert!(matches!(
            loves_db::love(&pool, a, tracks[2], LoveSource::NATIVE, 2)
                .await
                .unwrap(),
            Loved::Now(_)
        ));
        assert_eq!(
            loves_db::loved_at(&pool, a, tracks[0]).await.unwrap(),
            Some(first)
        );
        assert_eq!(loves_db::loved_at(&pool, a, tracks[1]).await.unwrap(), None);
    })
    .await;
}

/// Concurrent loves of different tracks can't pass the cap together.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn concurrent_loves_hold_the_cap() {
    with_db(true, |pool| async move {
        let a = user(&pool, "a").await;
        let mut ids = Vec::new();
        for i in 0..20 {
            ids.push(track(&pool, &format!("Song {i}")).await);
        }
        let attempts = ids.iter().map(|id| {
            let pool = pool.clone();
            let id = *id;
            async move { loves_db::love(&pool, a, id, LoveSource::NATIVE, 5).await }
        });
        let outcomes = futures_util::future::join_all(attempts).await;
        let loved = outcomes
            .iter()
            .filter(|o| matches!(o, Ok(Loved::Now(_))))
            .count();
        assert_eq!(loved, 5);
        assert_eq!(loves_db::count(&pool, a).await.unwrap(), 5);
    })
    .await;
}

/// Pages run newest love first, by (time, track) when two share a time,
/// with no entry repeated or skipped; the numbered pages the Last.fm API
/// serves agree. The album shown is the one the catalog files the track
/// under, whichever it becomes.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn pages_run_newest_first_and_follow_the_catalog() {
    with_db(true, |pool| async move {
        let a = user(&pool, "a").await;
        let other = user(&pool, "other").await;
        let now = Utc::now();
        let mut loved = Vec::new();
        for i in 0..25 {
            let id = track(&pool, &format!("Song {i}")).await;
            // Pairs share a time, to exercise the tie-break.
            love_at(&pool, a, id, now - TimeDelta::minutes(i / 2)).await;
            love_at(&pool, other, id, now).await;
            loved.push((i / 2, std::cmp::Reverse(id)));
        }
        loved.sort();
        let expected: Vec<i64> = loved.iter().map(|(_, id)| id.0).collect();

        let mut seen = Vec::new();
        let mut before = None;
        loop {
            let page = loves_db::page(&pool, a, before, 10).await.unwrap();
            seen.extend(page.iter().map(|l| l.track_id));
            if page.len() < 10 {
                break;
            }
            let last = page.last().unwrap();
            before = Some(Cursor {
                at: last.loved_at,
                id: last.track_id,
            });
        }
        assert_eq!(seen, expected);
        let mut numbered = Vec::new();
        for offset in [0, 10, 20] {
            numbered.extend(
                loves_db::page_at(&pool, a, 10, offset)
                    .await
                    .unwrap()
                    .iter()
                    .map(|l| l.track_id),
            );
        }
        assert_eq!(numbered, expected);
        assert_eq!(loves_db::count(&pool, a).await.unwrap(), 25);

        let artist = tracks_db::find_or_create_artist(&pool, "Band")
            .await
            .unwrap()
            .id;
        let album = tracks_db::find_or_create_album(&pool, artist, "Record")
            .await
            .unwrap();
        sqlx::query("UPDATE tracks SET album_id = $1 WHERE id = $2")
            .bind(album)
            .bind(expected[0])
            .execute(&pool)
            .await
            .unwrap();
        let first = &loves_db::page(&pool, a, None, 1).await.unwrap()[0];
        assert_eq!(
            (first.album_id, first.album_title.as_deref()),
            (Some(album), Some("Record"))
        );
    })
    .await;
}

/// A love goes with its track when the track leaves the catalog, and with
/// its user when the account is deleted; nothing else holds loves.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn loves_go_with_their_track_or_their_user() {
    with_db(true, |pool| async move {
        let (a, b) = (user(&pool, "a").await, user(&pool, "b").await);
        let (kept, removed) = (track(&pool, "Kept").await, track(&pool, "Removed").await);
        for (u, t) in [(a, kept), (a, removed), (b, kept), (b, removed)] {
            love_at(&pool, u, t, Utc::now()).await;
        }
        sqlx::query("DELETE FROM track_artists WHERE track_id = $1")
            .bind(removed)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM tracks WHERE id = $1")
            .bind(removed)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(loves_db::count(&pool, a).await.unwrap(), 1);
        assert_eq!(loves_db::count(&pool, b).await.unwrap(), 1);

        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
        let left: Vec<(i64, i64)> = sqlx::query_as("SELECT user_id, track_id FROM loved_tracks")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, [(b, kept)]);
    })
    .await;
}
