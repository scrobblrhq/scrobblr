mod common;

use chrono::{NaiveDate, TimeDelta, Utc};
use common::with_db;
use db::queries::coordination::{self as cdb, Window};
use db::queries::rankings::{self as rdb, Kind, Period};
use shared::coordination::{self as coordination, CoordinationParams, Mode};
use shared::ranking::{FULL, RankingParams};
use sqlx::PgPool;

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

fn params() -> CoordinationParams {
    CoordinationParams {
        min_accounts: 100,
        ..Default::default()
    }
}

/// 240 honest accounts registered over two years, each from its own
/// address, each weighing 10 of 60 bands on each of the last 10 days; and
/// `farm` accounts registered 40 days ago from two addresses, pushing
/// "Farm Band" with a little filler. Rows go straight into the weights'
/// tables, as weighing would write them.
async fn population(pool: &PgPool, farm: i64) -> i32 {
    let ruleset = rdb::register_ruleset(pool, &RankingParams::default())
        .await
        .unwrap()
        .id;
    for sql in [
        "INSERT INTO users (username, email, password_hash, created_at)
         SELECT 'honest' || n, 'honest' || n || '@test', 'x',
                NOW() - make_interval(days => 30 + (n * 37) % 700)
         FROM generate_series(1, 240) n",
        "INSERT INTO users (username, email, password_hash, created_at)
         SELECT 'farm' || n, 'farm' || n || '@test', 'x', NOW() - make_interval(days => 40 + n % 2)
         FROM generate_series(1, {farm}) n",
        "INSERT INTO registration_origins (user_id, key_id, address, network)
         SELECT id, 1, CASE WHEN username LIKE 'farm%' THEN 7 + id % 2 ELSE id + 1000 END,
                CASE WHEN username LIKE 'farm%' THEN 7 ELSE id + 1000 END
         FROM users",
        "INSERT INTO artists (name, name_normalized)
         SELECT 'Band ' || n, 'band ' || n FROM generate_series(1, 60) n
         UNION ALL SELECT 'Farm Band', 'farm band'",
        "INSERT INTO tracks (artist_id, title, title_normalized)
         SELECT id, 'Song', 'song' FROM artists",
        "INSERT INTO ranking_days (user_id, day, ruleset_id, classified_at, plays, weighted,
             weight, unclassified, suspect, duplicate, no_data, imported, new_account,
             short_listen, no_listened, unknown_client, track_cap, artist_cap)
         SELECT u.id, CURRENT_DATE - d, {ruleset}, NOW(), 10, 10, 10000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
         FROM users u CROSS JOIN generate_series(0, 9) d",
        "INSERT INTO ranking_daily (user_id, day, track_id, artist_id, plays, weight)
         SELECT u.id, CURRENT_DATE - d, t.id, t.artist_id, 1, 1000
         FROM users u
         CROSS JOIN generate_series(0, 9) d
         CROSS JOIN generate_series(0, 9) k
         JOIN artists a ON a.name = 'Band ' || (1 + (u.id * 7 + k * 13) % 60)
         JOIN tracks t ON t.artist_id = a.id
         WHERE u.username LIKE 'honest%'",
        "INSERT INTO ranking_daily (user_id, day, track_id, artist_id, plays, weight)
         SELECT u.id, CURRENT_DATE - d, t.id, t.artist_id, CASE WHEN a.name = 'Farm Band' THEN 8 ELSE 2 END,
                CASE WHEN a.name = 'Farm Band' THEN 4000 ELSE 1000 END
         FROM users u
         CROSS JOIN generate_series(0, 9) d
         JOIN artists a ON a.name IN ('Farm Band', 'Band 1')
         JOIN tracks t ON t.artist_id = a.id
         WHERE u.username LIKE 'farm%'",
    ] {
        let sql = sql
            .replace("{farm}", &farm.to_string())
            .replace("{ruleset}", &ruleset.to_string());
        sqlx::query(&sql).execute(pool).await.unwrap();
    }
    ruleset
}

async fn run(pool: &PgPool, params: &CoordinationParams) -> (i32, coordination::Detection) {
    let window = Window::new(today(), params);
    let (input, _) = cdb::load(pool, &window).await.unwrap();
    let detection = coordination::detect(&input, params);
    let id = cdb::store(pool, params, &window, &detection, Utc::now(), 1)
        .await
        .unwrap();
    (id, detection)
}

async fn factor_rows(pool: &PgPool, like: &str) -> Vec<(String, i32)> {
    sqlx::query_as(
        "SELECT u.username, c.factor FROM coordination_days c JOIN users u ON u.id = c.user_id
         WHERE u.username LIKE $1 ORDER BY 1, c.day",
    )
    .bind(like)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_farm_gets_factors_and_falls_out_of_the_rankings() {
    with_db(true, |pool| async move {
        let ruleset = population(&pool, 50).await;
        let (run_id, detection) = run(&pool, &params()).await;
        assert!(!detection.too_small);
        assert_eq!(detection.accounts.len(), 50, "{:?}", detection.cohorts);
        assert!(detection.accounts.iter().all(|a| a.factor == 0));

        // Every active day of every farm account, nothing for anyone else.
        let farm = factor_rows(&pool, "farm%").await;
        assert_eq!(farm.len(), 50 * 10);
        assert!(farm.iter().all(|(_, f)| *f == 0));
        assert!(factor_rows(&pool, "honest%").await.is_empty());

        let cohorts = cdb::cohorts(&pool, run_id).await.unwrap();
        assert_eq!(cohorts.len(), 1);
        assert_eq!(cohorts[0].members, 50);
        assert_eq!(cohorts[0].kind, Kind::Artist);
        assert_eq!(cohorts[0].name.as_deref(), Some("Farm Band"));
        assert_eq!(cohorts[0].detail["origin"]["values"], 2);
        assert_eq!(cdb::accounts(&pool, run_id).await.unwrap().len(), 50);

        let lw = RankingParams::default().listener_weight();
        let (from, to) = Period::Week.range(today());
        let ranked = rdb::rankings(&pool, Kind::Artist, from, to, lw, 100)
            .await
            .unwrap();
        let farm_band = ranked.iter().find(|r| r.name == "Farm Band").unwrap();
        // Behind Band 1 only, which the farm's filler lifts too.
        assert_eq!(farm_band.uncoordinated_rank, 2);
        assert_eq!(farm_band.uncoordinated_listeners, 50 * FULL);
        assert_eq!((farm_band.listeners, farm_band.weight), (0, 0));
        // The farm's filler leaves Band 1 where honest listening puts it.
        let band1 = ranked.iter().find(|r| r.name == "Band 1").unwrap();
        assert_eq!(band1.uncoordinated_listeners - band1.listeners, 50 * FULL);
        let honest: Vec<_> = ranked.iter().filter(|r| r.name != "Farm Band").collect();
        assert!(honest.iter().all(|r| r.name == "Band 1"
            || (r.listeners, r.weight) == (r.uncoordinated_listeners, r.uncoordinated_weight)));
        let totals = rdb::coordination_totals(&pool, from, to).await.unwrap();
        assert_eq!(totals.accounts, 50);
        assert_eq!(totals.weight - totals.coordinated, 50 * 7 * 5000);

        // Snapshots apply the factors and record whose they are.
        let weights = rdb::Ruleset {
            id: ruleset,
            params: RankingParams::default(),
        };
        let snapshots = rdb::refresh_snapshots(&pool, &weights, Period::Week, today(), 1000)
            .await
            .unwrap();
        assert!(
            snapshots
                .iter()
                .all(|s| s.coordination_run_id == Some(run_id))
        );
        let entries = rdb::snapshot_entries(&pool, Period::Week, Kind::Artist, 0, 1000)
            .await
            .unwrap();
        assert!(
            entries
                .iter()
                .all(|e| e.name.as_deref() != Some("Farm Band"))
        );
        assert_eq!(entries.len(), 60);

        // Report mode lists the cohort but takes every factor away.
        let report = CoordinationParams {
            mode: Mode::Report,
            ..params()
        };
        let (report_run, detection) = run(&pool, &report).await;
        assert_eq!(detection.accounts.len(), 50);
        assert!(factor_rows(&pool, "%").await.is_empty());
        assert_eq!(cdb::cohorts(&pool, report_run).await.unwrap().len(), 1);
        let ranked = rdb::rankings(&pool, Kind::Artist, from, to, lw, 100)
            .await
            .unwrap();
        let farm_band = ranked.iter().find(|r| r.name == "Farm Band").unwrap();
        assert_eq!((farm_band.rank, farm_band.listeners), (2, 50 * FULL));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn small_populations_and_honest_ones_get_no_factor() {
    with_db(true, |pool| async move {
        population(&pool, 0).await;
        let (_, detection) = run(&pool, &params()).await;
        assert!(!detection.too_small);
        assert!(detection.accounts.is_empty());
        let strict = CoordinationParams {
            min_accounts: 500,
            ..params()
        };
        let (id, detection) = run(&pool, &strict).await;
        assert!(detection.too_small);
        let latest = cdb::latest_run(&pool).await.unwrap().unwrap();
        assert_eq!(
            (latest.id, latest.too_small, latest.accounts),
            (id, true, 240)
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_run_rewrites_its_window_and_keeps_earlier_days() {
    with_db(true, |pool| async move {
        population(&pool, 50).await;
        let p = params();
        let window = Window::new(today(), &p);
        let (input, _) = cdb::load(&pool, &window).await.unwrap();
        let detection = coordination::detect(&input, &p);
        cdb::store(&pool, &p, &window, &detection, Utc::now(), 1)
            .await
            .unwrap();
        assert_eq!(factor_rows(&pool, "farm%").await.len(), 500);

        // A later run whose window starts 5 days ago and flags nobody
        // clears its days and leaves the five before as they were.
        let later = Window {
            from: today() - TimeDelta::days(4),
            ..window
        };
        let nobody = coordination::Detection::default();
        cdb::store(&pool, &p, &later, &nobody, Utc::now(), 1)
            .await
            .unwrap();
        let days: Vec<NaiveDate> = sqlx::query_scalar("SELECT DISTINCT day FROM coordination_days")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(days.len(), 5);
        assert!(days.iter().all(|d| *d < later.from));

        // They go when the weights do.
        rdb::purge_expired(&pool, today() + TimeDelta::days(rdb::RETAINED_DAYS))
            .await
            .unwrap();
        assert!(factor_rows(&pool, "%").await.is_empty());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn registration_origins_are_kept_for_their_retention() {
    with_db(true, |pool| async move {
        let ids: Vec<i64> = sqlx::query_scalar(
            "INSERT INTO users (username, email, password_hash)
             SELECT 'u' || n, 'u' || n || '@test', 'x' FROM generate_series(1, 3) n RETURNING id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let origin = shared::origin::Origin {
            key_id: 1,
            address: 2,
            network: 3,
        };
        for id in &ids {
            cdb::record_origin(&pool, *id, &origin).await.unwrap();
        }
        // Recording again keeps the first.
        cdb::record_origin(
            &pool,
            ids[0],
            &shared::origin::Origin {
                address: 9,
                ..origin
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE registration_origins SET created_at = NOW() - INTERVAL '91 days'
             WHERE user_id = $1",
        )
        .bind(ids[1])
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(cdb::purge_origins(&pool, 90).await.unwrap(), 1);
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(ids[2])
            .execute(&pool)
            .await
            .unwrap();
        let left: Vec<(i64, i64)> =
            sqlx::query_as("SELECT user_id, address FROM registration_origins")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(left, [(ids[0], 2)]);
    })
    .await;
}
