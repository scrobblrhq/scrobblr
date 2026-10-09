mod common;

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use common::with_db;
use db::queries::classification as cdb;
use db::queries::{scrobbles as scrobbles_db, tracks as tracks_db, users as users_db};
use shared::classification::BudgetParams;
use shared::models::{ScrobbleBreakdown, ScrobbleLabel};
use sqlx::PgPool;

fn days_ago(n: i64) -> NaiveDate {
    (Utc::now() - TimeDelta::days(n)).date_naive()
}

fn at(day: NaiveDate, secs: i64) -> DateTime<Utc> {
    day.and_hms_opt(0, 0, 0).unwrap().and_utc() + TimeDelta::seconds(secs)
}

async fn scrobble(
    pool: &PgPool,
    user_id: i64,
    title: &str,
    length: Option<i32>,
    played_at: DateTime<Utc>,
) {
    let artist = tracks_db::find_or_create_artist(pool, "Band")
        .await
        .unwrap();
    let track = tracks_db::find_or_create_track(pool, artist.id, None, title, length)
        .await
        .unwrap();
    scrobbles_db::insert_scrobble(
        pool,
        &scrobbles_db::InsertScrobble {
            user_id,
            track_id: track.id,
            artist_id: artist.id,
            album_id: None,
            played_at,
            source: "test".into(),
            duration_ms: length,
            listened_ms: None,
            client_id: None,
        },
    )
    .await
    .unwrap();
}

async fn scrobble_count(pool: &PgPool, user_id: i64) -> i64 {
    users_db::find_by_id(pool, user_id)
        .await
        .unwrap()
        .unwrap()
        .scrobble_count
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn profiles_leave_duplicates_out_and_tell_verified_from_unverified() {
    with_db(true, |pool| async move {
        let user_id: i64 = sqlx::query_scalar(
            "INSERT INTO users (username, email, password_hash) VALUES ('ivy', 'ivy@test', 'x') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let day = days_ago(3);
        // An evening of 20 songs, each reported again 6 s later...
        for n in 0..20 {
            for echo in [0, 6] {
                scrobble(&pool, user_id, &format!("Song {n}"), Some(240_000), at(day, 3600 + n * 240 + echo)).await;
            }
        }
        // ...three plays of a track with no known length...
        for n in 0..3 {
            scrobble(&pool, user_id, "Untimed", None, at(day, 40_000 + n * 300)).await;
        }
        // ...and 300 distinct tracks in ten minutes.
        for n in 0..300 {
            scrobble(&pool, user_id, &format!("Bot {n}"), Some(180_000), at(day, 50_000 + n * 2)).await;
        }
        assert_eq!(scrobble_count(&pool, user_id).await, 343);

        let rules = cdb::register_ruleset(&pool, BudgetParams::default())
            .await
            .unwrap();
        let outcome = cdb::classify_user_day(&pool, &rules, user_id, day, false)
            .await
            .unwrap();
        let c = outcome.counts;
        assert_eq!((c.duplicate, c.no_data), (20, 3));
        assert!(c.suspect > 200);

        assert_eq!(scrobble_count(&pool, user_id).await, 323);
        let breakdown = users_db::scrobble_breakdown(&pool, user_id).await.unwrap();
        assert_eq!(
            breakdown,
            ScrobbleBreakdown {
                verified: c.counted,
                unverified: c.suspect + 3,
                suspect: c.suspect,
                no_data: 3,
                pending: 0,
                duplicates: 20,
            }
        );

        // Reclassifying changes nothing; a new scrobble is pending.
        cdb::classify_user_day(&pool, &rules, user_id, day, false)
            .await
            .unwrap();
        scrobble(&pool, user_id, "Late", Some(200_000), at(day, 80_000)).await;
        let breakdown = users_db::scrobble_breakdown(&pool, user_id).await.unwrap();
        assert_eq!((breakdown.pending, breakdown.duplicates), (1, 20));
        assert_eq!(scrobble_count(&pool, user_id).await, 324);
        assert_eq!(breakdown.verified + breakdown.unverified, 324);

        scrobbles_db::refresh_scrobble_aggregates(&pool, at(day, 0), at(day, 0))
            .await
            .unwrap();
        let since = at(day, 0);
        let artists = scrobbles_db::get_top_artists(&pool, user_id, since, 10)
            .await
            .unwrap();
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].play_count, 324);
        assert_eq!(artists[0].unverified_count, c.suspect + 3);
        let tracks = scrobbles_db::get_top_tracks(&pool, user_id, since, 50)
            .await
            .unwrap();
        let song = tracks.iter().find(|t| t.track_title == "Song 0").unwrap();
        assert_eq!((song.play_count, song.unverified_count), (1, Some(0)));
        let untimed = tracks.iter().find(|t| t.track_title == "Untimed").unwrap();
        assert_eq!((untimed.play_count, untimed.unverified_count), (3, Some(3)));

        let heatmap = scrobbles_db::get_activity_heatmap(&pool, user_id, since)
            .await
            .unwrap();
        assert_eq!(heatmap.len(), 1);
        assert_eq!(heatmap[0].scrobble_count, 324);

        let recent = scrobbles_db::get_recent_scrobbles(&pool, user_id, 500, None)
            .await
            .unwrap();
        assert_eq!(recent[0].track_title, "Late");
        assert_eq!(recent[0].status, None);
        let evening: Vec<_> = recent
            .iter()
            .filter(|s| s.track_title.starts_with("Song "))
            .collect();
        assert_eq!(evening.len(), 20);
        assert!(evening.iter().all(|s| s.status == Some(ScrobbleLabel::Counted)));
        assert!(
            recent
                .iter()
                .filter(|s| s.track_title == "Untimed")
                .all(|s| s.status == Some(ScrobbleLabel::NoData))
        );
        assert!(recent.iter().any(|s| s.status == Some(ScrobbleLabel::Suspect)));

        assert_eq!(
            scrobbles_db::count_scrobbles(&pool, user_id, Some(at(day, 0)), None)
                .await
                .unwrap(),
            324
        );
        let page = scrobbles_db::recent_scrobbles_page(&pool, user_id, None, None, 500, 0)
            .await
            .unwrap();
        assert_eq!(page.len(), 324);
    })
    .await;
}
