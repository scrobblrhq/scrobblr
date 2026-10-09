mod common;

use chrono::{TimeDelta, Utc};
use common::with_db;
use db::queries::{enrichment as edb, scrobbles as scrobbles_db, tracks as tracks_db};
use shared::track_lengths::{Outlier, outlier};
use sqlx::PgPool;
use uuid::Uuid;

/// A track with the three lengths, in seconds.
async fn track(pool: &PgPool, title: &str, mb: i32, catalog: i32, deezer: i32) -> (i64, Uuid) {
    let artist = tracks_db::find_or_create_artist(pool, "Band")
        .await
        .unwrap();
    let track = tracks_db::find_or_create_track(pool, artist.id, None, title, Some(catalog * 1000))
        .await
        .unwrap();
    let mbid = Uuid::new_v4();
    sqlx::query(
        "UPDATE tracks SET mbid = $2, mbid_hint = $2, mb_duration_ms = $3, deezer_duration_ms = $4
         WHERE id = $1",
    )
    .bind(track.id)
    .bind(mbid)
    .bind(mb * 1000)
    .bind(deezer * 1000)
    .execute(pool)
    .await
    .unwrap();
    (track.id, mbid)
}

#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn a_musicbrainz_match_two_sources_contradict_is_taken_back() {
    with_db(true, |pool| async move {
        let (live_take, mbid) = track(&pool, "Runaway Baby", 510, 147, 148).await;
        let (wrong_catalog, _) = track(&pool, "Bohemian Rhapsody", 356, 142, 355).await;
        track(&pool, "Fine", 200, 201, 199).await;

        let user: i64 = sqlx::query_scalar(
            "INSERT INTO users (username, email, password_hash) VALUES ('u', 'u@test', 'x') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let played_at = Utc::now() - TimeDelta::days(40);
        let artist_id: i64 = sqlx::query_scalar("SELECT artist_id FROM tracks WHERE id = $1")
            .bind(live_take)
            .fetch_one(&pool)
            .await
            .unwrap();
        scrobbles_db::insert_scrobble(
            &pool,
            &scrobbles_db::InsertScrobble {
                user_id: user,
                track_id: live_take,
                artist_id,
                album_id: None,
                played_at,
                source: "test".into(),
                duration_ms: None,
                listened_ms: None,
                client_id: None,
            },
        )
        .await
        .unwrap();
        scrobbles_db::refresh_scrobble_aggregates(&pool, played_at, played_at)
            .await
            .unwrap();

        let disputed = edb::disputed_lengths(&pool, 10).await.unwrap();
        let verdicts: Vec<(i64, Option<Outlier>)> = disputed
            .iter()
            .map(|t| {
                (
                    t.track_id,
                    outlier(t.musicbrainz_ms.into(), t.catalog_ms.into(), t.deezer_ms.into()),
                )
            })
            .collect();
        assert_eq!(
            verdicts,
            [
                (live_take, Some(Outlier::MusicBrainz)),
                (wrong_catalog, Some(Outlier::Catalog)),
            ]
        );

        // A length changed since the review leaves the track alone.
        assert_eq!(
            edb::unlink_musicbrainz(&pool, live_take, 999_000)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            edb::unlink_musicbrainz(&pool, live_take, 510_000)
                .await
                .unwrap(),
            Some(1)
        );

        let after: (Option<Uuid>, Option<Uuid>, Option<i32>, Option<i32>) = sqlx::query_as(
            "SELECT mbid, mbid_hint, mb_duration_ms, duration_ms FROM tracks WHERE id = $1",
        )
        .bind(live_take)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(after, (None, None, None, Some(147_000)));
        let ctx = edb::get_track_ctx(&pool, live_take).await.unwrap().unwrap();
        assert_eq!(ctx.rejected_mbids, [mbid]);
        let job: String = sqlx::query_scalar(
            "SELECT status FROM enrichment_jobs WHERE entity_type = 'track' AND entity_id = $1",
        )
        .bind(live_take)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(job, "pending");
        let queued: i64 =
            sqlx::query_scalar("SELECT count(*) FROM classification_queue WHERE user_id = $1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(queued, 1);
        assert_eq!(
            edb::disputed_lengths(&pool, 10).await.unwrap().len(),
            1,
            "only the catalog outlier is left"
        );
    })
    .await;
}
