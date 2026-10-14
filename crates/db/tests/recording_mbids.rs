mod common;

use common::with_db;
use db::queries::tracks as tracks_db;
use sqlx::PgPool;
use uuid::Uuid;

async fn track(
    pool: &PgPool,
    title: &str,
    mbid: Option<Uuid>,
    hint: Option<Uuid>,
    plays: i64,
) -> i64 {
    let artist = tracks_db::find_or_create_artist(pool, "Band")
        .await
        .unwrap();
    let id = tracks_db::find_or_create_track(pool, artist.id, None, title, None)
        .await
        .unwrap()
        .id;
    sqlx::query("UPDATE tracks SET mbid = $2, mbid_hint = $3, scrobble_count = $4 WHERE id = $1")
        .bind(id)
        .bind(mbid)
        .bind(hint)
        .bind(plays)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// An MBID names the track enrichment matched it to first, else the most
/// played track a client hinted it for that enrichment left undecided and
/// didn't reject it for; nothing else, and nothing is created.
#[tokio::test]
#[ignore = "needs Postgres: just test-db"]
async fn recording_mbids_resolve_to_matched_then_hinted_tracks() {
    with_db(true, |pool| async move {
        let [matched, hinted, rejected, decided, unknown] = std::array::from_fn(|_| Uuid::new_v4());
        let a = track(&pool, "Matched", Some(matched), None, 1).await;
        // Hinted for the matched one too: the match wins.
        track(&pool, "Also Hinted", None, Some(matched), 500).await;
        track(&pool, "Hinted Rarely", None, Some(hinted), 3).await;
        let b = track(&pool, "Hinted Often", None, Some(hinted), 90).await;
        let c = track(&pool, "Rejected", None, Some(rejected), 10).await;
        sqlx::query("INSERT INTO track_mbid_rejections (track_id, mbid) VALUES ($1, $2)")
            .bind(c)
            .bind(rejected)
            .execute(&pool)
            .await
            .unwrap();
        // Enrichment matched it to another recording: the hint is moot.
        track(&pool, "Decided", Some(Uuid::new_v4()), Some(decided), 10).await;

        let tracks_before: i64 = sqlx::query_scalar("SELECT count(*) FROM tracks")
            .fetch_one(&pool)
            .await
            .unwrap();
        let mut found = tracks_db::find_by_recording_mbids(
            &pool,
            &[matched, hinted, rejected, decided, unknown],
        )
        .await
        .unwrap();
        found.sort();
        let mut expected = vec![(matched, a), (hinted, b)];
        expected.sort();
        assert_eq!(found, expected);
        let tracks_after: i64 = sqlx::query_scalar("SELECT count(*) FROM tracks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tracks_after, tracks_before);
    })
    .await;
}
