mod common;

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveTime, TimeDelta, Utc};
use common::with_db;
use db::queries::charts::{self as charts_db, Page};
use db::queries::classification as cdb;
use db::queries::scrobbles as scrobbles_db;
use shared::charts::{ChartPeriod, ChartRange};
use shared::classification::BudgetParams;
use sqlx::PgPool;

/// 6 users over 50 days of 10 artists' 30 tracks, two thirds of them on an
/// album, a tenth without a length, with a tenth of plays reported again
/// seconds later and one user scrobbling far faster than real time on one
/// day; every day classified, the aggregates refreshed.
async fn history(pool: &PgPool) -> Vec<i64> {
    for sql in [
        "SELECT setseed(0.7)",
        "INSERT INTO users (username, email, password_hash)
         SELECT 'u' || n, 'u' || n || '@test', 'x' FROM generate_series(1, 6) n",
        "INSERT INTO artists (name, name_normalized)
         SELECT 'Artist ' || n, 'artist ' || n FROM generate_series(1, 10) n",
        "INSERT INTO albums (artist_id, title, title_normalized)
         SELECT id, 'Album of ' || name, 'album of ' || name_normalized FROM artists",
        "INSERT INTO tracks (artist_id, album_id, title, title_normalized, duration_ms)
         SELECT a.id, CASE WHEN n % 3 <> 0 THEN al.id END, 'Track ' || a.id || '.' || n,
                'track ' || a.id || '.' || n, CASE WHEN n <> 1 OR a.id % 3 <> 0 THEN 200000 END
         FROM artists a JOIN albums al ON al.artist_id = a.id
         CROSS JOIN generate_series(1, 3) n",
        "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source)
         SELECT u.id, t.id, t.artist_id,
                date_trunc('day', NOW() - make_interval(days => d)) + make_interval(secs => 36000 + p * 300),
                'test'
         FROM users u
         CROSS JOIN generate_series(0, 49) d
         CROSS JOIN generate_series(0, 9) p
         CROSS JOIN LATERAL (SELECT id, artist_id FROM tracks
                             ORDER BY random() + u.id * 0 + d * 0 + p * 0 LIMIT 1) t
         WHERE random() < 0.6",
        "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source)
         SELECT user_id, track_id, artist_id, played_at + INTERVAL '5 seconds', 'test'
         FROM scrobbles WHERE random() < 0.1",
        "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source)
         SELECT (SELECT min(id) FROM users), t.id, t.artist_id,
                date_trunc('day', NOW() - INTERVAL '12 days') + make_interval(secs => 3600 + p * 31),
                'test'
         FROM generate_series(0, 299) p
         CROSS JOIN LATERAL (SELECT id, artist_id FROM tracks
                             WHERE duration_ms IS NOT NULL ORDER BY random() + p * 0 LIMIT 1) t",
    ] {
        sqlx::query(sql).execute(pool).await.unwrap();
    }
    let rules = cdb::register_ruleset(pool, BudgetParams::default())
        .await
        .unwrap();
    let days: Vec<(i64, NaiveDate)> = sqlx::query_as(
        "SELECT DISTINCT user_id, (played_at AT TIME ZONE 'UTC')::date FROM scrobbles ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    for (user, day) in days {
        cdb::classify_user_day(pool, &rules, user, day, false)
            .await
            .unwrap();
    }
    scrobbles_db::refresh_scrobble_aggregates(pool, Utc::now() - TimeDelta::days(60), Utc::now())
        .await
        .unwrap();
    sqlx::query_scalar("SELECT id FROM users ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Plays and unverified plays by entity, recounted from the scrobbles and
/// their flags: `entity` is `s.artist_id`, `s.track_id` or `t.album_id`.
async fn recount(
    pool: &PgPool,
    user: i64,
    range: &ChartRange,
    entity: &str,
) -> BTreeMap<i64, (i64, i64)> {
    let start = range.from.map(|d| d.and_time(NaiveTime::MIN).and_utc());
    let end = (range.to + TimeDelta::days(1))
        .and_time(NaiveTime::MIN)
        .and_utc();
    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(&format!(
        "SELECT {entity}, count(*),
                count(*) FILTER (WHERE f.status IN ('suspect', 'no_data'))
         FROM scrobbles s
         JOIN tracks t ON t.id = s.track_id
         LEFT JOIN scrobble_flags f ON f.scrobble_id = s.id AND f.played_at = s.played_at
         WHERE s.user_id = $1 AND ($2::timestamptz IS NULL OR s.played_at >= $2)
           AND s.played_at < $3 AND f.status IS DISTINCT FROM 'duplicate'
           AND {entity} IS NOT NULL
         GROUP BY 1"
    ))
    .bind(user)
    .bind(start)
    .bind(end)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter()
        .map(|(id, plays, unverified)| (id, (plays, unverified)))
        .collect()
}

/// Every page of a chart, checked to be ordered by plays, then id.
fn collect<T>(
    pages: Vec<Page<T>>,
    key: impl Fn(&T) -> (i64, i64, i64),
) -> BTreeMap<i64, (i64, i64)> {
    let total = pages[0].total;
    let entries: Vec<(i64, i64, i64)> = pages
        .iter()
        .flat_map(|p| p.entries.iter().map(&key))
        .collect();
    assert_eq!(entries.len() as i64, total);
    for pair in entries.windows(2) {
        assert!(
            (-pair[0].1, pair[0].0) < (-pair[1].1, pair[1].0),
            "{pair:?}"
        );
    }
    entries
        .into_iter()
        .map(|(id, plays, unverified)| (id, (plays, unverified)))
        .collect()
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn personal_charts_equal_a_recount_of_the_scrobbles() {
    with_db(true, |pool| async move {
        let users = history(&pool).await;
        let flagged: (i64, i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE status = 'duplicate'), count(*) FILTER (WHERE status = 'suspect'),
                    count(*) FILTER (WHERE status = 'no_data') FROM scrobble_flags",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(flagged.0 > 50 && flagged.1 > 50 && flagged.2 > 50, "{flagged:?}");

        let today = Utc::now().date_naive();
        let mut ranges = vec![ChartRange {
            period: None,
            from: Some(today - TimeDelta::days(40)),
            to: today - TimeDelta::days(10),
        }];
        for period in [ChartPeriod::Days7, ChartPeriod::Days30, ChartPeriod::All] {
            ranges.push(ChartRange {
                period: Some(period),
                from: period.days().map(|d| today - TimeDelta::days(d - 1)),
                to: today,
            });
        }
        for user in users {
            for range in &ranges {
                let mut artists = Vec::new();
                let mut tracks = Vec::new();
                let mut albums = Vec::new();
                for offset in [0, 7, 14, 21, 28] {
                    artists.push(charts_db::user_artists(&pool, user, range, 7, offset).await.unwrap());
                    tracks.push(charts_db::user_tracks(&pool, user, range, 7, offset).await.unwrap());
                    albums.push(charts_db::user_albums(&pool, user, range, 7, offset).await.unwrap());
                }
                assert_eq!(
                    collect(artists, |e| (e.artist_id, e.play_count, e.unverified_count)),
                    recount(&pool, user, range, "s.artist_id").await,
                    "artists, user {user}, {range:?}"
                );
                assert_eq!(
                    collect(tracks, |e| (e.track_id, e.play_count, e.unverified_count.unwrap())),
                    recount(&pool, user, range, "s.track_id").await,
                    "tracks, user {user}, {range:?}"
                );
                assert_eq!(
                    collect(albums, |e| (e.album_id, e.play_count, e.unverified_count)),
                    recount(&pool, user, range, "t.album_id").await,
                    "albums, user {user}, {range:?}"
                );
            }
        }
    })
    .await;
}
