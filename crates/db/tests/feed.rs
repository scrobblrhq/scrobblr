mod common;

use std::collections::HashSet;

use chrono::{DateTime, TimeDelta, Utc};
use common::with_db;
use db::queries::feed;
use sqlx::PgPool;

/// Plays of the first track by `user` at the given times; their ids.
async fn plays(pool: &PgPool, user: i64, at: &[DateTime<Utc>]) -> Vec<i64> {
    sqlx::query_scalar(
        "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source)
         SELECT $1, t.id, t.artist_id, p, 'test'
         FROM unnest($2::timestamptz[]) p
         CROSS JOIN (SELECT id, artist_id FROM tracks ORDER BY id LIMIT 1) t
         RETURNING id",
    )
    .bind(user)
    .bind(at)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn the_feed_cursor_holds_while_plays_arrive() {
    with_db(true, |pool| async move {
        for sql in [
            "SELECT setseed(0.5)",
            "INSERT INTO users (username, email, password_hash)
             SELECT 'u' || n, 'u' || n || '@test', 'x' FROM generate_series(0, 30) n",
            "INSERT INTO artists (name, name_normalized) VALUES ('Band', 'band')",
            "INSERT INTO tracks (artist_id, title, title_normalized) SELECT id, 'Song', 'song' FROM artists",
            "INSERT INTO user_follows (follower_id, followee_id)
             SELECT (SELECT id FROM users WHERE username = 'u0'), id FROM users WHERE username <> 'u0'",
            // 400 plays over three days; some share a second.
            "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source)
             SELECT u.id, t.id, t.artist_id,
                    date_trunc('second', NOW() - make_interval(secs => floor(random() * 3 * 86400))), 'test'
             FROM generate_series(1, 400) n
             CROSS JOIN LATERAL (SELECT id FROM users WHERE username <> 'u0'
                                 ORDER BY random() + n * 0 LIMIT 1) u
             CROSS JOIN (SELECT id, artist_id FROM tracks) t",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let viewer: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'u0'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let followed: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM users WHERE username <> 'u0' ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let before: HashSet<i64> = sqlx::query_scalar("SELECT id FROM scrobbles")
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .collect();

        let mut seen: Vec<(DateTime<Utc>, i64)> = Vec::new();
        let mut above = HashSet::new();
        let mut late = HashSet::new();
        let mut cursor = None;
        let mut pages = 0;
        loop {
            let page = feed::feed_page(&pool, viewer, cursor, 7, Utc::now()).await.unwrap();
            seen.extend(page.entries.iter().map(|e| (e.played_at, e.id)));
            pages += 1;
            let Some(next) = page.next else { break };
            // Between pages: new plays above the cursor, which later pages
            // never show (one at its very time sorts above it by its newer
            // id), and late ones below it, which they must.
            let user = followed[pages % followed.len()];
            above.extend(plays(&pool, user, &[Utc::now(), next.at + TimeDelta::seconds(1), next.at]).await);
            if next.at - TimeDelta::hours(1) > Utc::now() - TimeDelta::days(3) {
                let below = [next.at - TimeDelta::hours(1), next.at - TimeDelta::seconds(1)];
                late.extend(plays(&pool, user, &below).await);
            }
            cursor = Some(next);
        }
        assert!(pages > 50, "{pages} pages");

        // Newest first, every entry once.
        for pair in seen.windows(2) {
            assert!(pair[0] > pair[1], "{pair:?}");
        }
        let ids: HashSet<i64> = seen.iter().map(|s| s.1).collect();
        assert_eq!(ids.len(), seen.len(), "an entry twice");
        // Every play there before the first page and every late one, and
        // none above a cursor once it was passed.
        assert!(late.len() > 50, "{} late", late.len());
        let expected: HashSet<i64> = before.union(&late).copied().collect();
        assert_eq!(ids, expected);
        assert!(above.is_disjoint(&ids));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_late_play_leaves_last_seen_at_the_latest() {
    with_db(true, |pool| async move {
        for sql in [
            "INSERT INTO users (username, email, password_hash) VALUES ('u', 'u@test', 'x')",
            "INSERT INTO artists (name, name_normalized) VALUES ('Band', 'band')",
            "INSERT INTO tracks (artist_id, title, title_normalized) SELECT id, 'Song', 'song' FROM artists",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let user: i64 = sqlx::query_scalar("SELECT id FROM users").fetch_one(&pool).await.unwrap();
        let latest = Utc::now() - TimeDelta::minutes(1);
        plays(&pool, user, &[latest]).await;
        // A phone's offline queue, flushed later.
        plays(&pool, user, &[latest - TimeDelta::days(2)]).await;
        let seen: DateTime<Utc> = sqlx::query_scalar("SELECT last_seen_at FROM users WHERE id = $1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(seen.timestamp_micros(), latest.timestamp_micros());
    })
    .await;
}
