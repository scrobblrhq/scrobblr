mod common;

use chrono::{TimeDelta, Utc};
use common::with_db;
use db::queries::blocks as blocks_db;
use db::queries::rankings as rdb;
use db::queries::similarity::{self as sdb, Source, Window};
use shared::ranking::RankingParams;
use shared::similarity::{self as similarity, SimilarityMode, SimilarityParams};
use sqlx::PgPool;

fn params() -> SimilarityParams {
    SimilarityParams {
        min_listeners: 5,
        min_support: 5,
        min_user_artists: 2,
        min_shared: 2,
        ..Default::default()
    }
}

/// Two genres of three bands, each with 20 fans (`x1`… and `y1`…, the odd
/// ones listed) who also play "Everyone"; `me`, playing Band 1 and Band 2.
/// Each account plays each of its bands 5 times on each of the last 3
/// days, as scrobbles (the history) and as their weights.
async fn population(pool: &PgPool) {
    let ruleset = rdb::register_ruleset(pool, &RankingParams::default())
        .await
        .unwrap()
        .id;
    for sql in [
        "INSERT INTO users (username, email, password_hash, created_at, similarity)
         SELECT g || n, g || n || '@test', 'x', NOW() - INTERVAL '100 days',
                CASE WHEN n % 2 = 1 THEN 'listed' ELSE 'anonymous' END
         FROM unnest(ARRAY['x', 'y']) g CROSS JOIN generate_series(1, 20) n
         UNION ALL SELECT 'me', 'me@test', 'x', NOW() - INTERVAL '100 days', 'anonymous'",
        "INSERT INTO artists (name, name_normalized)
         SELECT 'Band ' || n, 'band ' || n FROM generate_series(1, 6) n
         UNION ALL SELECT 'Everyone', 'everyone'",
        "INSERT INTO tracks (artist_id, title, title_normalized) SELECT id, 'Song', 'song' FROM artists",
        "CREATE TABLE fixture_plays AS
         SELECT u.id AS user_id, t.id AS track_id, t.artist_id, d
         FROM users u
         JOIN artists a ON a.name = 'Everyone'
             OR (u.username LIKE 'x%' AND a.name IN ('Band 1', 'Band 2', 'Band 3'))
             OR (u.username LIKE 'y%' AND a.name IN ('Band 4', 'Band 5', 'Band 6'))
             OR (u.username = 'me' AND a.name IN ('Band 1', 'Band 2'))
         JOIN tracks t ON t.artist_id = a.id
         CROSS JOIN generate_series(1, 3) d",
        "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source)
         SELECT user_id, track_id, artist_id,
                (CURRENT_DATE - d)::timestamp AT TIME ZONE 'UTC' + make_interval(mins => k * 4 + artist_id::int * 30),
                'test'
         FROM fixture_plays CROSS JOIN generate_series(1, 5) k",
        "INSERT INTO ranking_days (user_id, day, ruleset_id, classified_at, plays, weighted,
             weight, unclassified, suspect, duplicate, no_data, imported, new_account,
             short_listen, no_listened, unknown_client, track_cap, artist_cap)
         SELECT DISTINCT user_id, CURRENT_DATE - d, {ruleset}, NOW(), 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
         FROM fixture_plays",
        "INSERT INTO ranking_daily (user_id, day, track_id, artist_id, plays, weight)
         SELECT user_id, CURRENT_DATE - d, track_id, artist_id, 5, 5000 FROM fixture_plays",
        "UPDATE users SET last_seen_at = NOW() - INTERVAL '1 day'",
    ] {
        sqlx::query(&sql.replace("{ruleset}", &ruleset.to_string()))
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn id(pool: &PgPool, table: &str, column: &str, value: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT id FROM {table} WHERE {column} = $1"))
        .bind(value)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Loads, computes and stores a run as the worker does.
async fn run(pool: &PgPool, p: &SimilarityParams) -> i32 {
    let window = Window::new(Utc::now().date_naive(), p);
    let (input, loaded) = sdb::load(pool, &window, p, Source::Weights).await.unwrap();
    assert!(loaded.credits > 0);
    let output = similarity::compute(&input, p);
    sdb::store(pool, p, &window, &output, None, Utc::now(), 1)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_run_stores_neighbours_recommendations_and_listed_taste_neighbours() {
    with_db(true, |pool| async move {
        population(&pool).await;
        let p = params();
        let first = run(&pool, &p).await;
        let band = |n: i64| {
            let pool = pool.clone();
            async move { id(&pool, "artists", "name", &format!("Band {n}")).await }
        };
        let (b1, b2, b3) = (band(1).await, band(2).await, band(3).await);
        let me = id(&pool, "users", "username", "me").await;

        let page = sdb::artist_page(&pool, b1, 0, 10).await.unwrap().unwrap();
        assert_eq!(page.version.run_id, first);
        let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Band 2", "Band 3"]);
        assert_eq!(page.total, 2);
        assert_eq!(page.entries[0].listeners, 21);
        let page = sdb::artist_page(&pool, b1, 1, 10).await.unwrap().unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].position, 2);

        let run_at = sdb::version(&pool).await.unwrap().unwrap().started_at;
        let recs = sdb::recommendations(&pool, me, run_at).await.unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].artist_id, b3);
        assert!(recs[0].because_id == b1 || recs[0].because_id == b2);

        // Listed fans of the same bands, never anonymous ones.
        let neighbours = sdb::taste_neighbours(&pool, me, None).await.unwrap();
        assert_eq!(neighbours.len(), 10);
        assert!(neighbours.iter().all(|n| n.username.starts_with('x')));
        let odd = |n: &sdb::TasteEntry| n.username[1..].parse::<i64>().unwrap() % 2 == 1;
        assert!(neighbours.iter().all(odd));
        assert_eq!(neighbours[0].shared_artists, 2);
        let shared: Vec<i64> = neighbours[0].top_shared.iter().map(|(a, _)| *a).collect();
        assert!(shared.contains(&b1) && shared.contains(&b2));

        // A skip hides it until lifted; a play since the run too.
        assert!(
            sdb::skip(&pool, me, b3, similarity::SKIP_DAYS)
                .await
                .unwrap()
        );
        assert!(
            sdb::recommendations(&pool, me, run_at)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !sdb::skip(&pool, me, 0, similarity::SKIP_DAYS)
                .await
                .unwrap()
        );
        assert!(sdb::unskip(&pool, me, b3).await.unwrap());
        assert_eq!(
            sdb::recommendations(&pool, me, run_at).await.unwrap().len(),
            1
        );
        sqlx::query(
            "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source)
             SELECT $1, id, artist_id, NOW(), 'test' FROM tracks WHERE artist_id = $2",
        )
        .bind(me)
        .bind(b3)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            sdb::recommendations(&pool, me, run_at)
                .await
                .unwrap()
                .is_empty()
        );

        // Blocks, privacy and the setting take effect at once.
        let x1 = id(&pool, "users", "username", "x1").await;
        let x3 = id(&pool, "users", "username", "x3").await;
        let x5 = id(&pool, "users", "username", "x5").await;
        let x7 = id(&pool, "users", "username", "x7").await;
        blocks_db::block(&pool, me, x1, 100).await.unwrap();
        sqlx::query("UPDATE users SET is_private = TRUE WHERE id = $1")
            .bind(x3)
            .execute(&pool)
            .await
            .unwrap();
        sdb::set_setting(&pool, x5, SimilarityMode::Anonymous)
            .await
            .unwrap();
        let seen = |viewer: Option<i64>| {
            let pool = pool.clone();
            async move {
                sdb::taste_neighbours(&pool, me, viewer)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|n| n.user_id)
                    .collect::<Vec<i64>>()
            }
        };
        let now = seen(None).await;
        assert_eq!(now.len(), 7);
        for gone in [x1, x3, x5] {
            assert!(!now.contains(&gone));
        }
        // An account blocking the viewer is gone for that viewer only.
        let x9 = id(&pool, "users", "username", "x9").await;
        blocks_db::block(&pool, x9, x7, 100).await.unwrap();
        let for_x7 = seen(Some(x7)).await;
        assert!(!for_x7.contains(&x9) && for_x7.len() == 6, "{for_x7:?}");
        let (ended, others): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE (user_id = $1 AND neighbour_user_id = $2)
                                        OR neighbour_user_id = $3),
                    count(*) FILTER (WHERE neighbour_user_id = $2 AND user_id <> $1)
             FROM user_neighbours",
        )
        .bind(me)
        .bind(x1)
        .bind(x5)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ended, 0, "rows a block or the setting ended are gone");
        assert!(others > 0, "a block ends the pair's rows only");

        // Off removes everything about the account.
        let setting = sdb::set_setting(&pool, me, SimilarityMode::Off)
            .await
            .unwrap();
        assert_eq!(setting.mode, SimilarityMode::Off);
        assert!(setting.changed_at.is_some());
        for table in ["user_recommendations", "user_neighbours"] {
            let n: i64 =
                sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE user_id = $1"))
                    .bind(me)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        // And the next run leaves it out, as it does accounts leaving.
        sqlx::query(
            "UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE username = 'y2'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let window = Window::new(Utc::now().date_naive(), &p);
        let (input, _) = sdb::load(&pool, &window, &p, Source::Weights)
            .await
            .unwrap();
        let y2 = id(&pool, "users", "username", "y2").await;
        assert!(
            input
                .contributors
                .iter()
                .all(|c| c.user_id != me && c.user_id != y2)
        );
        assert!(input.histories.iter().all(|h| h.user_id != me));
        let second = run(&pool, &p).await;
        assert!(second > first);
        assert!(
            sdb::recommendations(&pool, me, Utc::now())
                .await
                .unwrap()
                .is_empty()
        );
    })
    .await;
}

/// A run reads its input, then takes a while: accounts opting out or
/// leaving meanwhile, or no longer listed, aren't stored.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_run_leaves_out_accounts_that_changed_their_mind_meanwhile() {
    with_db(true, |pool| async move {
        population(&pool).await;
        let p = params();
        let window = Window::new(Utc::now().date_naive(), &p);
        let (input, _) = sdb::load(&pool, &window, &p, Source::Weights)
            .await
            .unwrap();
        let output = similarity::compute(&input, &p);
        let x1 = id(&pool, "users", "username", "x1").await;
        let x2 = id(&pool, "users", "username", "x2").await;
        assert!(output.recommendations.contains_key(&x2) || output.neighbours.contains_key(&x2));
        sdb::set_setting(&pool, x1, SimilarityMode::Anonymous)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET delete_after = NOW() + INTERVAL '7 days' WHERE id = $1")
            .bind(x2)
            .execute(&pool)
            .await
            .unwrap();
        sdb::store(&pool, &p, &window, &output, None, Utc::now(), 1)
            .await
            .unwrap();
        let (about_x1, about_x2): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM user_neighbours WHERE neighbour_user_id = $1),
                    (SELECT count(*) FROM user_neighbours WHERE user_id = $2)
                    + (SELECT count(*) FROM user_recommendations WHERE user_id = $2)",
        )
        .bind(x1)
        .bind(x2)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((about_x1, about_x2), (0, 0));
        let run = sdb::latest_run(&pool).await.unwrap().unwrap();
        assert!(run.artists > 0 && !run.invalidated);
        assert_eq!(run.refresh_secs, 24 * 3600);
    })
    .await;
}

/// Skips expire and are capped per account; old runs go after 30 days.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn skips_expire_and_are_capped() {
    with_db(true, |pool| async move {
        population(&pool).await;
        let me = id(&pool, "users", "username", "me").await;
        let band = id(&pool, "artists", "name", "Band 6").await;
        assert!(sdb::skip(&pool, me, band, 0).await.unwrap());
        assert_eq!(sdb::skips(&pool, me).await.unwrap().len(), 1);
        assert_eq!(sdb::purge_skips(&pool).await.unwrap(), 1);
        sqlx::query(
            "INSERT INTO artists (name, name_normalized)
             SELECT 'Skipped ' || n, 'skipped ' || n FROM generate_series(1, 1005) n",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO recommendation_skips (user_id, artist_id, skipped_at, until)
             SELECT $1, id, NOW() - make_interval(secs => id), NOW() + INTERVAL '1 day'
             FROM artists WHERE name LIKE 'Skipped %'",
        )
        .bind(me)
        .execute(&pool)
        .await
        .unwrap();
        assert!(sdb::skip(&pool, me, band, 90).await.unwrap());
        let skips = sdb::skips(&pool, me).await.unwrap();
        assert_eq!(skips.len() as i64, sdb::MAX_SKIPS);
        assert_eq!(skips[0].artist_id, band);
        assert!(skips[0].until > Utc::now() + TimeDelta::days(89));
    })
    .await;
}
