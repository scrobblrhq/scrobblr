mod common;

use chrono::Utc;
use common::with_db;
use db::queries::tags::{self as tags_db, Added, TagSource, Weighing};
use db::queries::tracks as tracks_db;
use shared::community;
use shared::models::CatalogKind;
use sqlx::PgPool;

async fn user(pool: &PgPool, name: &str, days_old: i64) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash, created_at)
         VALUES ($1, $1 || '@test', 'x', NOW() - make_interval(days => $2::int)) RETURNING id",
    )
    .bind(name)
    .bind(days_old as i32)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn artist(pool: &PgPool, name: &str) -> i64 {
    tracks_db::find_or_create_artist(pool, name)
        .await
        .unwrap()
        .id
}

fn names(tags: &[&str]) -> Vec<String> {
    tags.iter().map(|t| t.to_string()).collect()
}

async fn tag(pool: &PgPool, user_id: i64, kind: CatalogKind, id: i64, tags: &[&str]) -> Added {
    tags_db::add(pool, user_id, kind, id, &names(tags), TagSource::NATIVE)
        .await
        .unwrap()
}

/// Every due entity recounted now.
async fn recount(pool: &PgPool) {
    sqlx::query("UPDATE tag_queue SET due_at = NOW()")
        .execute(pool)
        .await
        .unwrap();
    while tags_db::recount_due(pool, Weighing::at(Utc::now()), 100)
        .await
        .unwrap()
        > 0
    {}
}

/// Adding is idempotent and capped per account and entity, per entity and
/// per account; removing frees the place.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn tags_are_idempotent_and_capped() {
    with_db(true, |pool| async move {
        let alice = user(&pool, "alice", 30).await;
        let band = artist(&pool, "Band").await;
        let added = tag(&pool, alice, CatalogKind::Artist, band, &["rock", "post rock", "rock"]).await;
        assert_eq!(added, Added::Tags(names(&["post rock", "rock"])));
        let again = tag(&pool, alice, CatalogKind::Artist, band, &["rock"]).await;
        assert_eq!(again, Added::Tags(names(&["post rock", "rock"])));

        let many: Vec<String> = (0..community::MAX_TAGS_PER_ENTITY - 1)
            .map(|i| format!("tag {i}"))
            .collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(tag(&pool, alice, CatalogKind::Artist, band, &many).await, Added::TooManyHere);
        assert!(tags_db::remove(&pool, alice, CatalogKind::Artist, band, "rock").await.unwrap());
        assert!(!tags_db::remove(&pool, alice, CatalogKind::Artist, band, "rock").await.unwrap());
        assert!(matches!(tag(&pool, alice, CatalogKind::Artist, band, &many).await, Added::Tags(t) if t.len() == 10));

        // An entity full of distinct tags takes only those it has.
        let full = artist(&pool, "Full").await;
        sqlx::query(
            "WITH t AS (INSERT INTO tags (name) SELECT 'filler ' || g FROM generate_series(1, $2) g RETURNING id)
             INSERT INTO user_tags (user_id, entity_type, entity_id, tag_id, source)
             SELECT $1, 'artist', $3, id, 'scrobblr' FROM t",
        )
        .bind(alice)
        .bind(community::MAX_ENTITY_TAGS as i32)
        .bind(full)
        .execute(&pool)
        .await
        .unwrap();
        let bob = user(&pool, "bob", 30).await;
        assert_eq!(tag(&pool, bob, CatalogKind::Artist, full, &["new one"]).await, Added::EntityFull);
        assert!(matches!(tag(&pool, bob, CatalogKind::Artist, full, &["filler 1"]).await, Added::Tags(_)));

        // An account holding the most taggings.
        let carol = user(&pool, "carol", 30).await;
        sqlx::query(
            "INSERT INTO user_tags (user_id, entity_type, entity_id, tag_id, source)
             SELECT $1, 'track', g, (SELECT min(id) FROM tags), 'scrobblr'
             FROM generate_series(1, $2) g",
        )
        .bind(carol)
        .bind(community::MAX_TAGGINGS as i32)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(tag(&pool, carol, CatalogKind::Artist, band, &["one more"]).await, Added::AtLimit);
    })
    .await;
}

/// The counts are what a recount from the taggings gives: every tagger in
/// `taggers`; in `weight`, a week-old account at its lowest coordination
/// factor of the window, nothing for new, suspended or leaving accounts.
/// Only weights past the minimum are served.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn counts_weigh_taggers_like_the_rankings() {
    with_db(true, |pool| async move {
        let band = artist(&pool, "Band").await;
        let other = artist(&pool, "Other").await;
        let mut aged = Vec::new();
        for i in 0..6 {
            aged.push(user(&pool, &format!("fan{i}"), 30).await);
        }
        let young = user(&pool, "young", 2).await;
        let flagged = user(&pool, "flagged", 30).await;
        let suspended = user(&pool, "suspended", 30).await;
        let leaving = user(&pool, "leaving", 30).await;
        sqlx::query(
            "INSERT INTO coordination_days (user_id, day, factor, run_id) VALUES
             ($1, CURRENT_DATE - 3, 250, 1), ($1, CURRENT_DATE - 50, 0, 1)",
        )
        .bind(flagged)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO editing_suspensions (user_id) VALUES ($1)")
            .bind(suspended)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE id = $1")
            .bind(leaving)
            .execute(&pool)
            .await
            .unwrap();

        for u in aged.iter().take(4).chain([&young, &flagged, &suspended, &leaving]) {
            tag(&pool, *u, CatalogKind::Artist, band, &["shoegaze", "noise"]).await;
        }
        tag(&pool, aged[4], CatalogKind::Artist, band, &["shoegaze"]).await;
        for u in &aged {
            tag(&pool, *u, CatalogKind::Artist, other, &["shoegaze"]).await;
        }
        recount(&pool).await;

        let counts: Vec<(i64, String, i32, i64)> = sqlx::query_as(
            "SELECT c.entity_id, t.name, c.taggers, c.weight FROM tag_counts c
             JOIN tags t ON t.id = c.tag_id ORDER BY 1, 2",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            counts,
            [
                (band, "noise".into(), 8, 4_250),
                (band, "shoegaze".into(), 9, 5_250),
                (other, "shoegaze".into(), 6, 6_000),
            ]
        );
        let recounted: Vec<(i64, String, i64)> = sqlx::query_as(
            "SELECT ut.entity_id, t.name, count(*) FROM user_tags ut JOIN tags t ON t.id = ut.tag_id
             GROUP BY 1, 2 ORDER BY 1, 2",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            recounted,
            counts.iter().map(|(e, n, t, _)| (*e, n.clone(), *t as i64)).collect::<Vec<_>>()
        );

        let min = 5 * shared::ranking::FULL;
        let top = tags_db::top_tags(&pool, CatalogKind::Artist, band, min, 10).await.unwrap();
        assert_eq!(top, [("shoegaze".to_string(), 5_250)]);
        let shoegaze = tags_db::tag_id(&pool, "shoegaze").await.unwrap().unwrap();
        let (entries, total) = tags_db::top_entities(&pool, shoegaze, CatalogKind::Artist, min, 1, 0)
            .await
            .unwrap();
        assert_eq!(total, 2);
        assert_eq!((entries[0].id, entries[0].taggers, entries[0].position), (other, 6, 1));
        let (entries, _) = tags_db::top_entities(&pool, shoegaze, CatalogKind::Artist, min, 1, 1)
            .await
            .unwrap();
        assert_eq!((entries[0].id, entries[0].taggers, entries[0].position), (band, 5, 2));

        // A removal queues the entity; the young account coming of age is
        // seen at the next full recount.
        tags_db::remove(&pool, aged[0], CatalogKind::Artist, band, "noise").await.unwrap();
        sqlx::query("UPDATE users SET created_at = NOW() - INTERVAL '8 days' WHERE id = $1")
            .bind(young)
            .execute(&pool)
            .await
            .unwrap();
        tags_db::enqueue_all(&pool).await.unwrap();
        recount(&pool).await;
        let noise: (i32, i64) = sqlx::query_as(
            "SELECT taggers, weight FROM tag_counts WHERE entity_id = $1 AND tag_id = (SELECT id FROM tags WHERE name = 'noise')",
        )
        .bind(band)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(noise, (7, 4_250));
    })
    .await;
}
