mod common;

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveTime, TimeDelta, Utc};
use common::with_db;
use db::queries::charts::{self as charts_db, CommunityKind, Page, Ranked};
use db::queries::classification as cdb;
use db::queries::rankings as rdb;
use db::queries::scrobbles as scrobbles_db;
use shared::charts::{ChartPeriod, ChartRange};
use shared::classification::BudgetParams;
use shared::ranking::{FULL, RankingParams};
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

/// `accounts`' entities over `[from, to]` recounted from the stored weights
/// and coordination factors, as `community` ranks them.
async fn recount_weights(
    pool: &PgPool,
    accounts: &[i64],
    from: NaiveDate,
    to: NaiveDate,
    kind: CommunityKind,
    min_listeners: i64,
) -> Vec<Ranked> {
    let full = FULL;
    let listener_weight = RankingParams::default().listener_weight();
    let rows: Vec<(i64, i64, i64, Option<i64>, i32)> = sqlx::query_as(
        "SELECT r.user_id, r.track_id, r.artist_id, t.album_id, r.weight
         FROM ranking_daily r JOIN tracks t ON t.id = r.track_id
         WHERE r.user_id = ANY($1) AND r.day BETWEEN $2 AND $3",
    )
    .bind(accounts)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .unwrap();
    let days: Vec<(i64, NaiveDate, i64, Option<i32>)> = sqlx::query_as(
        "SELECT r.user_id, r.day, r.weight, c.factor
         FROM ranking_days r
         LEFT JOIN coordination_days c ON c.user_id = r.user_id AND c.day = r.day
         WHERE r.user_id = ANY($1) AND r.day BETWEEN $2 AND $3",
    )
    .bind(accounts)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut factors: BTreeMap<i64, i64> = BTreeMap::new();
    for user in accounts {
        let mine: Vec<_> = days.iter().filter(|d| d.0 == *user).collect();
        let total: i64 = mine.iter().map(|d| d.2).sum();
        let dented: i64 = mine
            .iter()
            .filter_map(|d| d.3.map(|f| d.2 * (full - i64::from(f))))
            .sum();
        if mine.iter().any(|d| d.3.is_some()) && total > 0 {
            factors.insert(*user, full - dented / total);
        }
    }
    let mut per_user: BTreeMap<(i64, i64), i64> = BTreeMap::new();
    for (user, track, artist, album, weight) in rows {
        let entity = match kind {
            CommunityKind::Artists => Some(artist),
            CommunityKind::Tracks => Some(track),
            CommunityKind::Albums => album,
        };
        if let Some(entity) = entity {
            *per_user.entry((entity, user)).or_default() += i64::from(weight);
        }
    }
    let mut per_entity: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for ((entity, user), weight) in per_user {
        let factor = factors.get(&user).copied().unwrap_or(full);
        let sums = per_entity.entry(entity).or_default();
        sums.0 += (weight * full / listener_weight).min(full) * factor / full;
        sums.1 += weight * factor / full;
    }
    let mut ranked: Vec<Ranked> = per_entity
        .into_iter()
        .filter(|(_, (listeners, _))| *listeners >= min_listeners)
        .map(|(entity_id, (listeners, weight))| Ranked {
            entity_id,
            listeners,
            weight,
        })
        .collect();
    ranked.sort_by_key(|r| (-r.listeners, -r.weight, r.entity_id));
    ranked
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn community_charts_equal_a_recount_of_the_weights() {
    with_db(true, |pool| async move {
        let users = history(&pool).await;
        // All but the last are established; that one's plays weigh nothing.
        sqlx::query(
            "UPDATE users SET created_at = NOW() - INTERVAL '100 days',
                              last_seen_at = NOW() - make_interval(mins => id::int % 7)
             WHERE id <> $1",
        )
        .bind(users[5])
        .execute(&pool)
        .await
        .unwrap();
        let ruleset = rdb::register_ruleset(&pool, &RankingParams::default()).await.unwrap();
        let days: Vec<(i64, NaiveDate)> = sqlx::query_as(
            "SELECT user_id, day FROM scrobble_classification_days ORDER BY 1, 2",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        for (user, day) in days {
            rdb::weigh_user_day(&pool, &ruleset, user, day, false).await.unwrap();
        }
        // Two accounts dented by the coordination detection on some days.
        sqlx::query(
            "INSERT INTO coordination_days (user_id, day, factor, run_id)
             SELECT $1, d, 300, 1 FROM generate_series(CURRENT_DATE - 20, CURRENT_DATE - 2, INTERVAL '3 days') d
             UNION ALL SELECT $2, CURRENT_DATE - 12, 0, 1",
        )
        .bind(users[1])
        .bind(users[2])
        .execute(&pool)
        .await
        .unwrap();

        // A viewer following them all, a private account and one leaving.
        let viewer: i64 = sqlx::query_scalar(
            "INSERT INTO users (username, email, password_hash) VALUES ('viewer', 'v@test', 'x') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let hidden: Vec<i64> = sqlx::query_scalar(
            "INSERT INTO users (username, email, password_hash, is_private, delete_after)
             VALUES ('hidden', 'h@test', 'x', TRUE, NULL), ('leaving', 'l@test', 'x', FALSE, NOW())
             RETURNING id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO user_follows (follower_id, followee_id) SELECT $1, unnest($2::bigint[])")
            .bind(viewer)
            .bind([users.clone(), hidden].concat())
            .execute(&pool)
            .await
            .unwrap();
        let (following, accounts) = charts_db::community_accounts(&pool, viewer, 2000).await.unwrap();
        assert_eq!(following, 6);
        let mut sorted = accounts.clone();
        sorted.sort();
        assert_eq!(sorted, users);
        assert_eq!(*accounts.last().unwrap(), users[5], "never seen, last");
        let (_, capped) = charts_db::community_accounts(&pool, viewer, 4).await.unwrap();
        assert_eq!(capped, accounts[..4]);

        let today = Utc::now().date_naive();
        let mut compared = 0;
        for (from, to) in [
            (today - TimeDelta::days(6), today),
            (today - TimeDelta::days(29), today),
            (today - TimeDelta::days(40), today - TimeDelta::days(10)),
        ] {
            for kind in [CommunityKind::Artists, CommunityKind::Tracks, CommunityKind::Albums] {
                for min in [FULL, 2 * FULL] {
                    let chart = charts_db::community(&pool, kind, &accounts, from, to, min, 1000)
                        .await
                        .unwrap();
                    let expected = recount_weights(&pool, &accounts, from, to, kind, min).await;
                    assert_eq!(chart, expected, "{kind:?} {from}..{to}, min {min}");
                    compared += chart.len();
                }
            }
        }
        assert!(compared > 100, "{compared}");
        let few = charts_db::community(&pool, CommunityKind::Tracks, &accounts, today - TimeDelta::days(29), today, FULL, 3)
            .await
            .unwrap();
        assert_eq!(few.len(), 3);
    })
    .await;
}
