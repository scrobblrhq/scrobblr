mod common;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use common::with_db;
use db::queries::classification::{self as cdb, Ruleset as ClassifierRuleset};
use db::queries::rankings::{self as rdb, Kind, Period, Ranked, Ruleset};
use db::queries::scrobble_clients::{self, ClientIdentity};
use db::queries::tracks as tracks_db;
use shared::classification::BudgetParams;
use shared::ranking::{FULL, RankingParams, Reason};
use sqlx::PgPool;

fn days_ago(n: i64) -> NaiveDate {
    (Utc::now() - TimeDelta::days(n)).date_naive()
}

fn at(day: NaiveDate, secs: i64) -> DateTime<Utc> {
    day.and_hms_opt(0, 0, 0).unwrap().and_utc() + TimeDelta::seconds(secs)
}

/// A user whose account is `age_days` old.
async fn user(pool: &PgPool, name: &str, age_days: i64) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash, created_at)
         VALUES ($1, $1 || '@test', 'x', NOW() - make_interval(days => $2)) RETURNING id",
    )
    .bind(name)
    .bind(age_days as i32)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Returns `(track_id, artist_id)`; the track is `length_ms` long by
/// MusicBrainz and the catalog alike.
async fn track(pool: &PgPool, artist: &str, title: &str, length_ms: i32) -> (i64, i64) {
    let artist = tracks_db::find_or_create_artist(pool, artist)
        .await
        .unwrap();
    let track = tracks_db::find_or_create_track(pool, artist.id, None, title, Some(length_ms))
        .await
        .unwrap();
    sqlx::query("UPDATE tracks SET mb_duration_ms = $2 WHERE id = $1")
        .bind(track.id)
        .bind(length_ms)
        .execute(pool)
        .await
        .unwrap();
    (track.id, artist.id)
}

async fn client(pool: &PgPool, protocol: &'static str, name: &str, verified: bool) -> i32 {
    scrobble_clients::resolve_client(pool, &ClientIdentity::new(protocol, name, verified))
        .await
        .unwrap()
}

struct Scrobble {
    user_id: i64,
    track: (i64, i64),
    played_at: DateTime<Utc>,
    length_ms: i32,
    listened_ms: Option<i32>,
    client_id: Option<i32>,
    import_id: Option<i64>,
}

async fn scrobble(pool: &PgPool, s: Scrobble) {
    sqlx::query(
        "INSERT INTO scrobbles (user_id, track_id, artist_id, played_at, source, duration_ms,
                                listened_ms, client_id, import_id)
         VALUES ($1, $2, $3, $4, 'test', $5, $6, $7, $8)",
    )
    .bind(s.user_id)
    .bind(s.track.0)
    .bind(s.track.1)
    .bind(s.played_at)
    .bind(s.length_ms)
    .bind(s.listened_ms)
    .bind(s.client_id)
    .bind(s.import_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn rulesets(pool: &PgPool) -> (ClassifierRuleset, Ruleset) {
    (
        cdb::register_ruleset(pool, BudgetParams::default())
            .await
            .unwrap(),
        rdb::register_ruleset(pool, &RankingParams::default())
            .await
            .unwrap(),
    )
}

/// Classifies and weighs every (user, day) with scrobbles, as the worker
/// would.
async fn classify_and_weigh(pool: &PgPool, classifier: &ClassifierRuleset, weights: &Ruleset) {
    let days: Vec<(i64, NaiveDate)> = sqlx::query_as(
        "SELECT DISTINCT user_id, (played_at AT TIME ZONE 'UTC')::date FROM scrobbles ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    for (user_id, day) in &days {
        cdb::classify_user_day(pool, classifier, *user_id, *day, false)
            .await
            .unwrap();
    }
    for queued in rdb::claim_due(pool, 10_000).await.unwrap() {
        rdb::weigh_user_day(pool, weights, queued.user_id, queued.day, false)
            .await
            .unwrap();
    }
}

fn find<'a>(ranked: &'a [Ranked], name: &str) -> &'a Ranked {
    ranked
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("{name} not ranked"))
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn classification_queues_the_day_and_weights_follow_the_labels() {
    with_db(true, |pool| async move {
        let (classifier, weights) = rulesets(&pool).await;
        let fan = user(&pool, "fan", 60).await;
        let extension = client(&pool, "scrobblr", "ytmusic", false).await;
        let day = days_ago(3);
        let song = track(&pool, "Band", "Song", 180_000).await;
        // Back to back, each reported again 5 s later: 30 plays, 30 echoes.
        for n in 0..30 {
            for echo in [0, 5] {
                scrobble(
                    &pool,
                    Scrobble {
                        user_id: fan,
                        track: song,
                        played_at: at(day, 3600 + n * 180 + echo),
                        length_ms: 180_000,
                        listened_ms: Some(90_500),
                        client_id: Some(extension),
                        import_id: None,
                    },
                )
                .await;
            }
        }

        cdb::classify_user_day(&pool, &classifier, fan, day, false)
            .await
            .unwrap();
        let queued = rdb::claim_due(&pool, 10).await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!((queued[0].user_id, queued[0].day), (fan, day));

        let outcome = rdb::weigh_user_day(&pool, &weights, fan, day, false)
            .await
            .unwrap();
        assert_eq!(outcome.plays, 60);
        assert_eq!(outcome.reasons.get(Reason::Duplicate), 30);
        // A loop counts 4 plays a day; the echoes use none of them.
        assert_eq!(outcome.weight, 4 * FULL);
        assert_eq!(outcome.reasons.get(Reason::TrackCap), 26);

        let stored: Vec<(i64, i32, i32)> =
            sqlx::query_as("SELECT track_id, plays, weight FROM ranking_daily WHERE user_id = $1")
                .bind(fan)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(stored, [(song.0, 60, 4000)]);

        // Weighing again changes nothing.
        let again = rdb::weigh_user_day(&pool, &weights, fan, day, false)
            .await
            .unwrap();
        assert_eq!(again, outcome);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn stale_and_orphaned_days_are_queued_again() {
    with_db(true, |pool| async move {
        let (classifier, weights) = rulesets(&pool).await;
        let fan = user(&pool, "fan", 60).await;
        let song = track(&pool, "Band", "Song", 200_000).await;
        let (d1, d2) = (days_ago(5), days_ago(4));
        for day in [d1, d2] {
            scrobble(
                &pool,
                Scrobble {
                    user_id: fan,
                    track: song,
                    played_at: at(day, 600),
                    length_ms: 200_000,
                    listened_ms: None,
                    client_id: None,
                    import_id: None,
                },
            )
            .await;
        }
        classify_and_weigh(&pool, &classifier, &weights).await;
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(
            rdb::enqueue_stale(&mut conn, weights.id, None, None, None)
                .await
                .unwrap(),
            0
        );

        // Reclassified: the weights are older than the labels.
        cdb::classify_user_day(&pool, &classifier, fan, d1, false)
            .await
            .unwrap();
        sqlx::query("DELETE FROM ranking_queue")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            rdb::enqueue_stale(&mut conn, weights.id, None, None, None)
                .await
                .unwrap(),
            1
        );
        for q in rdb::claim_due(&pool, 10).await.unwrap() {
            rdb::weigh_user_day(&pool, &weights, q.user_id, q.day, false)
                .await
                .unwrap();
        }

        // A classification gone takes its weights with it.
        sqlx::query("DELETE FROM scrobble_classification_days WHERE day = $1")
            .bind(d2)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            rdb::enqueue_stale(&mut conn, weights.id, None, None, None)
                .await
                .unwrap(),
            1
        );
        rdb::weigh_user_day(&pool, &weights, fan, d2, false)
            .await
            .unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM ranking_daily WHERE day = $1")
            .bind(d2)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);

        // New params make every day stale.
        let other = rdb::register_ruleset(
            &pool,
            &RankingParams {
                no_listened: 300,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rdb::enqueue_stale(&mut conn, other.id, None, None, None)
                .await
                .unwrap(),
            1
        );
    })
    .await;
}

/// The four ways of botting from scripts/synthetic/botting.sql, small:
/// each tops a raw ranking and none moves the filtered ones.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn the_filtered_rankings_resist_four_kinds_of_botting() {
    with_db(true, |pool| async move {
        let (classifier, weights) = rulesets(&pool).await;
        let day = days_ago(3);
        let extension = client(&pool, "scrobblr", "ytmusic", false).await;
        let unknown = client(&pool, "scrobblr", "botclient", false).await;

        // 40 listeners, each playing 3 of 5 artists for an evening.
        let mut popular = Vec::new();
        for a in 0..5 {
            let mut tracks = Vec::new();
            for t in 0..4 {
                tracks.push(
                    track(&pool, &format!("Popular {a}"), &format!("Hit {t}"), 200_000).await,
                );
            }
            popular.push(tracks);
        }
        for n in 0..40 {
            let fan = user(&pool, &format!("fan{n}"), 100).await;
            for k in 0..12 {
                let artist = (n + k / 4) % 5;
                scrobble(
                    &pool,
                    Scrobble {
                        user_id: fan,
                        track: popular[artist as usize][(k % 4) as usize],
                        played_at: at(day, 18 * 3600 + k * 205),
                        length_ms: 200_000,
                        listened_ms: Some(100_500),
                        client_id: Some(extension),
                        import_id: None,
                    },
                )
                .await;
            }
        }

        // Farm: 60 accounts registered yesterday, claiming the extension.
        let farm_song = track(&pool, "Farm Target", "Farmed", 200_000).await;
        for n in 0..60 {
            let account = user(&pool, &format!("farm{n}"), 1).await;
            for k in 0..10 {
                scrobble(
                    &pool,
                    Scrobble {
                        user_id: account,
                        track: farm_song,
                        played_at: at(day, 10 * 3600 + k * 205),
                        length_ms: 200_000,
                        listened_ms: Some(100_500),
                        client_id: Some(extension),
                        import_id: None,
                    },
                )
                .await;
            }
        }

        // Looper: one 3-minute track, 160 times overnight.
        let looper = user(&pool, "looper", 100).await;
        let loop_song = track(&pool, "Loop Target", "Rain", 180_000).await;
        for k in 0..160 {
            scrobble(
                &pool,
                Scrobble {
                    user_id: looper,
                    track: loop_song,
                    played_at: at(day, k * 180),
                    length_ms: 180_000,
                    listened_ms: Some(180_000),
                    client_id: Some(extension),
                    import_id: None,
                },
            )
            .await;
        }

        // Import dump: 2,000 plays, one every 12 s.
        let dumper = user(&pool, "dumper", 100).await;
        let import_id: i64 = sqlx::query_scalar(
            "INSERT INTO scrobble_imports (user_id, provider, external_user, verified, status)
             VALUES ($1, 'lastfm', 'dump', TRUE, 'done') RETURNING id",
        )
        .bind(dumper)
        .fetch_one(&pool)
        .await
        .unwrap();
        let mut dump_songs = Vec::new();
        for t in 0..8 {
            dump_songs.push(track(&pool, "Dump Target", &format!("Dumped {t}"), 200_000).await);
        }
        for k in 0..2000 {
            scrobble(
                &pool,
                Scrobble {
                    user_id: dumper,
                    track: dump_songs[k as usize % 8],
                    played_at: at(day, k * 12),
                    length_ms: 200_000,
                    listened_ms: None,
                    client_id: None,
                    import_id: Some(import_id),
                },
            )
            .await;
        }

        // Five established bots declaring 30 s listens every 35 s, which
        // the classifier counts.
        let mut bot_songs = Vec::new();
        for t in 0..8 {
            bot_songs.push(track(&pool, "Bot Target", &format!("Botted {t}"), 200_000).await);
        }
        for n in 0..5 {
            let bot = user(&pool, &format!("bot{n}"), 100).await;
            for k in 0..300 {
                scrobble(
                    &pool,
                    Scrobble {
                        user_id: bot,
                        track: bot_songs[k as usize % 8],
                        played_at: at(day, 8 * 3600 + k * 35),
                        length_ms: 200_000,
                        listened_ms: Some(30_000),
                        client_id: Some(unknown),
                        import_id: None,
                    },
                )
                .await;
            }
        }

        classify_and_weigh(&pool, &classifier, &weights).await;
        let ranked = rdb::rankings(&pool, Kind::Artist, day, day, 250, 10)
            .await
            .unwrap();

        // Raw: the farm has the most listeners; the four have the most plays.
        assert_eq!(find(&ranked, "Farm Target").raw_rank, 1);
        let mut by_plays: Vec<&Ranked> = ranked.iter().filter(|r| r.raw_play_rank <= 4).collect();
        by_plays.sort_by_key(|r| r.raw_play_rank);
        let names: Vec<&str> = by_plays.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["Dump Target", "Bot Target", "Farm Target", "Loop Target"]
        );

        // Filtered: the five real artists lead both orders.
        for order in [|r: &Ranked| r.rank, |r: &Ranked| r.weight_rank] {
            let mut top: Vec<&str> = ranked
                .iter()
                .filter(|r| order(r) <= 5)
                .map(|r| r.name.as_str())
                .collect();
            top.sort();
            assert_eq!(
                top,
                [
                    "Popular 0",
                    "Popular 1",
                    "Popular 2",
                    "Popular 3",
                    "Popular 4"
                ]
            );
        }
        for name in ["Farm Target", "Dump Target", "Bot Target"] {
            let r = find(&ranked, name);
            assert_eq!((r.listeners, r.weight), (0, 0), "{name}");
        }
        // The looper is one listener with a fan's day of plays.
        let looped = find(&ranked, "Loop Target");
        assert_eq!((looped.listeners, looped.weight), (FULL, 4 * FULL));

        let totals = rdb::totals(&pool, day, day).await.unwrap();
        assert_eq!(totals.reasons.get(Reason::NewAccount), 600);
        assert_eq!(totals.reasons.get(Reason::Imported), 2000);
        assert_eq!(totals.reasons.get(Reason::ShortListen), 1500);
        assert_eq!(totals.reasons.get(Reason::TrackCap), 156);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn weighing_compressed_history_leaves_chunks_compressed() {
    with_db(true, |pool| async move {
        let (classifier, weights) = rulesets(&pool).await;
        let fan = user(&pool, "fan", 400).await;
        let day = days_ago(90);
        for n in 0..50 {
            let song = track(&pool, "Old Band", &format!("Old {n}"), 180_000).await;
            scrobble(
                &pool,
                Scrobble {
                    user_id: fan,
                    track: song,
                    played_at: at(day, n * 185),
                    length_ms: 180_000,
                    listened_ms: Some(180_000),
                    client_id: None,
                    import_id: None,
                },
            )
            .await;
        }
        let compressed: Vec<String> = sqlx::query_scalar(
            "SELECT compress_chunk(c)::text FROM show_chunks('scrobbles', older_than => INTERVAL '60 days') c",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(compressed.len(), 1);

        classify_and_weigh(&pool, &classifier, &weights).await;
        let totals = rdb::totals(&pool, day, day).await.unwrap();
        assert_eq!(totals.plays, 50);
        // An unknown client weighs half; the artist cap keeps 30 plays.
        assert_eq!(totals.weight, 30 * FULL / 2);

        let still: bool = sqlx::query_scalar(
            "SELECT bool_and(is_compressed) FROM timescaledb_information.chunks WHERE hypertable_name = 'scrobbles'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(still);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn unclassified_scrobbles_weigh_nothing_until_classified() {
    with_db(true, |pool| async move {
        let (classifier, weights) = rulesets(&pool).await;
        let fan = user(&pool, "fan", 60).await;
        let day = days_ago(2);
        let song = track(&pool, "Band", "Song", 200_000).await;
        let play = |secs| Scrobble {
            user_id: fan,
            track: song,
            played_at: at(day, secs),
            length_ms: 200_000,
            listened_ms: Some(200_000),
            client_id: None,
            import_id: None,
        };
        scrobble(&pool, play(600)).await;
        classify_and_weigh(&pool, &classifier, &weights).await;
        scrobble(&pool, play(6000)).await;
        let outcome = rdb::weigh_user_day(&pool, &weights, fan, day, true)
            .await
            .unwrap();
        assert_eq!(outcome.plays, 2);
        assert_eq!(outcome.reasons.get(Reason::Unclassified), 1);
        assert_eq!(outcome.weight, FULL / 2);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn snapshots_keep_the_filtered_ranking_and_replace_it() {
    with_db(true, |pool| async move {
        let (classifier, weights) = rulesets(&pool).await;
        let day = days_ago(1);
        let extension = client(&pool, "scrobblr", "ytmusic", false).await;
        let play = |user_id, track, n: i64| Scrobble {
            user_id,
            track,
            played_at: at(day, 18 * 3600 + n * 205),
            length_ms: 200_000,
            listened_ms: Some(100_500),
            client_id: Some(extension),
            import_id: None,
        };
        // Band 0 has three fans, Band 1 two, Band 2 one; the farm's five
        // accounts are a day old and weigh nothing.
        let mut n = 0;
        for (band, fans) in [(0, 3), (1, 2), (2, 1)] {
            let song = track(&pool, &format!("Band {band}"), "Song", 200_000).await;
            for _ in 0..fans {
                let fan = user(&pool, &format!("fan{n}"), 100).await;
                scrobble(&pool, play(fan, song, n)).await;
                n += 1;
            }
        }
        let farmed = track(&pool, "Farm Target", "Farmed", 200_000).await;
        for f in 0..5 {
            let account = user(&pool, &format!("farm{f}"), 1).await;
            scrobble(&pool, play(account, farmed, f)).await;
        }
        classify_and_weigh(&pool, &classifier, &weights).await;

        let today = Utc::now().date_naive();
        let snapshots = rdb::refresh_snapshots(&pool, &weights, Period::Week, today, 2)
            .await
            .unwrap();
        assert_eq!(snapshots.len(), 2);
        for s in &snapshots {
            assert_eq!((s.from_day, s.to_day), (today - TimeDelta::days(6), today));
            assert_eq!((s.ranked, s.pending_days), (3, 0), "{:?}", s.kind);
        }

        let names = |entries: &[rdb::Entry]| -> Vec<String> {
            entries.iter().map(|e| e.name.clone().unwrap()).collect()
        };
        let artists = rdb::snapshot_entries(&pool, Period::Week, Kind::Artist, 0, 10)
            .await
            .unwrap();
        assert_eq!(names(&artists), ["Band 0", "Band 1"]);
        assert_eq!(
            artists.iter().map(|e| e.position).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(
            (artists[0].listeners, artists[0].raw_listeners),
            (3 * FULL, 3)
        );
        // The same order as the ranking computed on the spot.
        let ranked = rdb::rankings(
            &pool,
            Kind::Artist,
            today - TimeDelta::days(6),
            today,
            weights.params.listener_weight(),
            10,
        )
        .await
        .unwrap();
        for e in &artists {
            let r = ranked.iter().find(|r| r.entity_id == e.entity_id).unwrap();
            assert_eq!(r.rank, e.position as i64);
            assert_eq!((r.listeners, r.weight), (e.listeners, e.weight));
        }
        let tracks = rdb::snapshot_entries(&pool, Period::Week, Kind::Track, 1, 10)
            .await
            .unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].position, 2);
        assert_eq!(tracks[0].artist_name.as_deref(), Some("Band 1"));

        // A refresh replaces the period's rows and leaves the others alone.
        rdb::refresh_snapshots(&pool, &weights, Period::Month, today, 10)
            .await
            .unwrap();
        rdb::refresh_snapshots(&pool, &weights, Period::Week, today, 10)
            .await
            .unwrap();
        let artists = rdb::snapshot_entries(&pool, Period::Week, Kind::Artist, 0, 10)
            .await
            .unwrap();
        assert_eq!(names(&artists), ["Band 0", "Band 1", "Band 2"]);
        let stored = rdb::snapshots(&pool).await.unwrap();
        assert_eq!(stored.len(), 4);
        let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM ranking_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(entries, 12);
    })
    .await;
}
