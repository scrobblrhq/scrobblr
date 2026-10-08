mod common;

use common::with_db;
use db::queries::{tracks as tracks_db, users as users_db};
use sqlx::PgPool;

const MIGRATION: &str = include_str!("../../../migrations/0017_upload_keys.sql");
const LEGACY: &str = "0b4e7a0e-5c2f-4a8d-9f3e-2d1c0b9a8f7e.jpg";

async fn user(pool: &PgPool, name: &str, image_url: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash, image_url) \
         VALUES ($1, $1 || '@test', 'x', $2) RETURNING id",
    )
    .bind(name)
    .bind(image_url)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn image_of(pool: &PgPool, table: &str, id: i64) -> String {
    let column = if table == "image_candidates" {
        "url"
    } else {
        "image_url"
    };
    sqlx::query_scalar(&format!("SELECT {column} FROM {table} WHERE id = $1"))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn upload_urls_become_keys_and_other_urls_stay() {
    with_db(true, |pool| async move {
        let unchanged = [
            "https://lastfm.freetls.fastly.net/i/u/300x300/2a96cbd8b46e442fc41c2b86b821562f.png",
            "https://example.com/uploads/me.jpg",
            "https://example.com/uploads/0b4e7a0e-5c2f-4a8d-9f3e-2d1c0b9a8f7e.png",
            "https://example.com/uploads/0b4e7a0e-5c2f-4a8d-9f3e-2d1c0b9a8f7e.jpg?size=1",
            "avatars/0b/0b4e7a0e5c2f4a8d9f3e2d1c0b9a8f7e.jpg",
        ];
        let mut kept = Vec::new();
        for (i, url) in unchanged.iter().enumerate() {
            kept.push((user(&pool, &format!("kept{i}"), url).await, *url));
        }
        let converted = [
            user(
                &pool,
                "local",
                &format!("http://localhost:8080/uploads/{LEGACY}"),
            )
            .await,
            user(
                &pool,
                "prefixed",
                &format!("https://scrobblr.example/api/uploads/{LEGACY}"),
            )
            .await,
        ];

        let artist = tracks_db::find_or_create_artist(&pool, "Artist")
            .await
            .unwrap();
        let album = tracks_db::find_or_create_album(&pool, artist.id, "Album")
            .await
            .unwrap();
        let deezer = tracks_db::find_or_create_artist(&pool, "Other")
            .await
            .unwrap();
        let legacy_url = format!("https://api.scrobblr.example/uploads/{LEGACY}");
        for (table, id, url) in [
            ("artists", artist.id, legacy_url.as_str()),
            ("albums", album, legacy_url.as_str()),
            (
                "artists",
                deezer.id,
                "https://cdn-images.dzcdn.net/images/artist/x/500x500.jpg",
            ),
        ] {
            sqlx::query(&format!("UPDATE {table} SET image_url = $2 WHERE id = $1"))
                .bind(id)
                .bind(url)
                .execute(&pool)
                .await
                .unwrap();
        }
        let candidate: i64 = sqlx::query_scalar(
            "INSERT INTO image_candidates (entity_type, entity_id, url, uploaded_by) \
             VALUES ('artist', $1, $2, $3) RETURNING id",
        )
        .bind(artist.id)
        .bind(&legacy_url)
        .bind(converted[0])
        .fetch_one(&pool)
        .await
        .unwrap();

        // The migration already ran on the empty database; it is idempotent.
        sqlx::raw_sql(MIGRATION).execute(&pool).await.unwrap();

        for id in converted {
            assert_eq!(image_of(&pool, "users", id).await, LEGACY);
        }
        for (id, url) in kept {
            assert_eq!(image_of(&pool, "users", id).await, url);
        }
        assert_eq!(image_of(&pool, "artists", artist.id).await, LEGACY);
        assert_eq!(image_of(&pool, "albums", album).await, LEGACY);
        assert_eq!(image_of(&pool, "image_candidates", candidate).await, LEGACY);
        assert!(
            image_of(&pool, "artists", deezer.id)
                .await
                .starts_with("https://cdn-images")
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn update_profile_reports_the_image_it_replaced() {
    with_db(true, |pool| async move {
        let id = user(&pool, "someone", "avatars/ab/old.jpg").await;
        let set = |image_url| users_db::UpdateProfile {
            image_url: Some(image_url),
            ..Default::default()
        };

        let (user, previous) =
            users_db::update_profile(&pool, id, &set(Some("avatars/cd/new.jpg")))
                .await
                .unwrap();
        assert_eq!(previous.as_deref(), Some("avatars/ab/old.jpg"));
        assert_eq!(user.image_url.as_deref(), Some("avatars/cd/new.jpg"));

        let (user, previous) = users_db::update_profile(&pool, id, &set(None))
            .await
            .unwrap();
        assert_eq!(previous.as_deref(), Some("avatars/cd/new.jpg"));
        assert_eq!(user.image_url, None);

        let untouched = users_db::UpdateProfile::default();
        let (_, previous) = users_db::update_profile(&pool, id, &untouched)
            .await
            .unwrap();
        assert_eq!(previous, None);
    })
    .await;
}
