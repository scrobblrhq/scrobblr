//! Which album a track belongs to: the worker loop deciding queued tracks
//! from their accounts' votes (`shared::albums`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{PgConnection, PgPool};

use crate::heartbeat::Beat;
use db::queries::albums::{self as albums_db, CandidateRow, Eligibility, Move};
use shared::albums::{Candidate, choose};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const BATCH: i64 = 200;

/// Who counts: accounts as old as rankings want them, flagged ones at
/// their factors of the coordination window.
#[derive(Debug, Clone, Copy)]
pub struct Rules {
    pub min_account_days: i64,
    pub coordination_days: i64,
}

impl Rules {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            min_account_days: crate::rankings::params_from_env()?.min_account_days,
            coordination_days: crate::rankings::coordination::params_from_env()?.window_days,
        })
    }

    fn at(&self, now: DateTime<Utc>) -> Eligibility {
        Eligibility {
            created_before: now - TimeDelta::days(self.min_account_days),
            factor_since: (now - TimeDelta::days(self.coordination_days)).date_naive(),
        }
    }
}

/// A track's decision: where it moves, if anywhere, and when it must be
/// decided again (an account behind one of its albums comes of age).
#[derive(Debug, Clone)]
struct Decision {
    track_id: i64,
    from: Option<i64>,
    to: Option<i64>,
    recheck_at: Option<DateTime<Utc>>,
}

fn decide(current: &[(i64, Option<i64>)], rows: Vec<CandidateRow>, rules: &Rules) -> Vec<Decision> {
    let mut by_track: HashMap<i64, Vec<CandidateRow>> = HashMap::new();
    for row in rows {
        by_track.entry(row.track_id).or_default().push(row);
    }
    current
        .iter()
        .map(|&(track_id, from)| {
            let candidates = by_track.remove(&track_id).unwrap_or_default();
            let scored: Vec<Candidate> = candidates
                .iter()
                .map(|c| Candidate {
                    album_id: c.album_id,
                    weight: c.weight,
                    confirmed: c.confirmed,
                })
                .collect();
            let recheck_at = candidates
                .iter()
                .filter_map(|c| c.youngest_since)
                .min()
                .map(|created| created + TimeDelta::days(rules.min_account_days));
            Decision {
                track_id,
                from,
                to: choose(from, &scored),
                recheck_at,
            }
        })
        .collect()
}

pub struct Attributor {
    db: PgPool,
    rules: Rules,
}

impl Attributor {
    pub fn from_env(db: PgPool) -> anyhow::Result<Self> {
        Ok(Self {
            db,
            rules: Rules::from_env()?,
        })
    }

    pub async fn run(self: Arc<Self>, beat: Beat) {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match self.drain().await {
                Ok(moved) => {
                    if moved > 0 {
                        tracing::info!("albums: {moved} tracks moved to another album");
                    }
                    beat.ok().await;
                }
                Err(e) => {
                    tracing::error!("albums: deciding failed, retrying later: {e}");
                    beat.failed(e).await;
                }
            }
        }
    }

    async fn drain(&self) -> Result<u64, sqlx::Error> {
        let mut moved = 0;
        loop {
            let (decided, n) = decide_due(&self.db, &self.rules, BATCH).await?;
            moved += n;
            if decided < BATCH as usize {
                return Ok(moved);
            }
        }
    }
}

/// Decides up to `limit` due tracks in one transaction: how many were
/// decided, and how many moved.
pub async fn decide_due(
    pool: &PgPool,
    rules: &Rules,
    limit: i64,
) -> Result<(usize, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let tracks = albums_db::claim_due(&mut tx, limit).await?;
    if tracks.is_empty() {
        return Ok((0, 0));
    }
    let decisions = decide_tracks(&mut tx, "track_album_votes", &tracks, rules).await?;
    albums_db::dequeue(&mut tx, &tracks).await?;
    let moved = settle(&mut tx, &decisions).await?;
    tx.commit().await?;
    Ok((tracks.len(), moved))
}

async fn decide_tracks(
    conn: &mut PgConnection,
    votes: &str,
    tracks: &[i64],
    rules: &Rules,
) -> Result<Vec<Decision>, sqlx::Error> {
    let current = albums_db::current(conn, tracks).await?;
    let rows = albums_db::candidates(conn, votes, tracks, rules.at(Utc::now())).await?;
    Ok(decide(&current, rows, rules))
}

/// Moves the tracks decided to move and queues those to decide again.
async fn settle(conn: &mut PgConnection, decisions: &[Decision]) -> Result<u64, sqlx::Error> {
    let moves: Vec<Move> = decisions
        .iter()
        .filter_map(|d| {
            d.to.map(|to| Move {
                track_id: d.track_id,
                from: d.from,
                to,
            })
        })
        .collect();
    let moved = albums_db::apply(conn, &moves).await?;
    let mut rechecks: Vec<(DateTime<Utc>, i64)> = decisions
        .iter()
        .filter_map(|d| d.recheck_at.map(|at| (at, d.track_id)))
        .collect();
    rechecks.sort();
    for group in rechecks.chunk_by(|a, b| a.0 == b.0) {
        let ids: Vec<i64> = group.iter().map(|(_, id)| *id).collect();
        albums_db::enqueue(&mut *conn, &ids, group[0].0).await?;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};
    use sqlx::PgPool;

    use super::*;
    use crate::test_support::with_db;
    use db::queries::scrobbles::ingest_scrobble;
    use shared::scrobble::ScrobbleInput;

    const RULES: Rules = Rules {
        min_account_days: 7,
        coordination_days: 28,
    };

    async fn accounts(pool: &PgPool, prefix: &str, n: i32, age_days: i32) -> Vec<i64> {
        sqlx::query_scalar(
            "INSERT INTO users (username, email, password_hash, created_at)
             SELECT $1 || i, $1 || i || '@test', 'x', NOW() - make_interval(days => $3)
             FROM generate_series(1, $2) i
             RETURNING id",
        )
        .bind(prefix)
        .bind(n)
        .bind(age_days)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// A play through ingest, as every live source sends one.
    async fn play(pool: &PgPool, user_id: i64, track: &str, album: &str, minutes_ago: i64) {
        let input = ScrobbleInput {
            track_title: track.into(),
            artist_name: "Massive Attack".into(),
            featured_artists: vec![],
            album_title: Some(album.into()),
            played_at: Utc::now() - TimeDelta::minutes(minutes_ago),
            duration_ms: Some(330_000),
            listened_ms: None,
            source: "test".into(),
            client_id: None,
            recording_mbid: None,
        };
        ingest_scrobble(pool, user_id, &input).await.unwrap();
    }

    async fn plays(pool: &PgPool, users: &[i64], track: &str, album: &str) {
        for &user_id in users {
            play(pool, user_id, track, album, 10).await;
        }
    }

    /// What the worker decides once every queued track is due.
    async fn decide_now(pool: &PgPool) {
        sqlx::query("UPDATE album_attribution_queue SET due_at = NOW()")
            .execute(pool)
            .await
            .unwrap();
        while decide_due(pool, &RULES, 50).await.unwrap().0 > 0 {}
    }

    async fn album_of(pool: &PgPool, track: &str) -> String {
        sqlx::query_scalar(
            "SELECT al.title FROM tracks t JOIN albums al ON al.id = t.album_id WHERE t.title = $1",
        )
        .bind(track)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn flag(pool: &PgPool, users: &[i64], factor: i32) {
        sqlx::query(
            "INSERT INTO coordination_days (user_id, day, factor, run_id)
             SELECT id, CURRENT_DATE - 1, $2, 1 FROM unnest($1::bigint[]) id",
        )
        .bind(users)
        .bind(factor)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A popular track's album, attacked: by many new accounts, one account
    /// playing it all day, a farm the coordination detection flagged, and a
    /// farm of established accounts it didn't, which moves it once it has
    /// half again the listeners' number and one more.
    #[tokio::test]
    #[ignore = "needs Postgres: just test-db"]
    async fn an_album_takes_half_again_its_accounts_to_move() {
        with_db(|pool| async move {
            let fans = accounts(&pool, "fan", 20, 400).await;
            plays(&pool, &fans, "Teardrop", "Mezzanine").await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Teardrop").await, "Mezzanine");

            // 200 accounts made this week: they count from next week, and
            // the track is decided again then.
            let young = accounts(&pool, "new", 200, 2).await;
            plays(&pool, &young, "Teardrop", "Fake Album").await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Teardrop").await, "Mezzanine");
            let recheck: chrono::DateTime<Utc> = sqlx::query_scalar(
                "SELECT due_at FROM album_attribution_queue q JOIN tracks t ON t.id = q.track_id
                 WHERE t.title = 'Teardrop'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!((recheck - (Utc::now() + TimeDelta::days(5))).abs() < TimeDelta::minutes(1));

            // One established account playing it 300 times: one say.
            let [obsessed] = accounts(&pool, "loop", 1, 400).await[..] else {
                unreachable!()
            };
            for n in 0..300 {
                play(&pool, obsessed, "Teardrop", "Fake Album", 5 * n + 1).await;
            }
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Teardrop").await, "Mezzanine");
            let votes: i64 =
                sqlx::query_scalar("SELECT count(*) FROM track_album_votes WHERE user_id = $1")
                    .bind(obsessed)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(votes, 1);

            // 40 established accounts the coordination detection flagged.
            let flagged = accounts(&pool, "flagged", 40, 60).await;
            flag(&pool, &flagged, 0).await;
            plays(&pool, &flagged, "Teardrop", "Fake Album").await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Teardrop").await, "Mezzanine");

            // Established accounts it didn't flag: with the one above, 29
            // against 20 isn't enough; 30 is, 15 more than half again.
            let farm = accounts(&pool, "farm", 28, 60).await;
            plays(&pool, &farm, "Teardrop", "Fake Album").await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Teardrop").await, "Mezzanine");
            let more = accounts(&pool, "farm_more", 1, 60).await;
            plays(&pool, &more, "Teardrop", "Fake Album").await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Teardrop").await, "Fake Album");

            // Flagged later, the farm weighs nothing and the album goes back.
            let mut farm = farm;
            farm.extend(more);
            farm.push(obsessed);
            flag(&pool, &farm, 0).await;
            albums_db::enqueue(&pool, &[track_id(&pool, "Teardrop").await], Utc::now())
                .await
                .unwrap();
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Teardrop").await, "Mezzanine");
        })
        .await;
    }

    async fn track_id(pool: &PgPool, title: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM tracks WHERE title = $1")
            .bind(title)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// A release MusicBrainz lists the recording on counts double, so a farm
    /// needs three times the listeners; the first account to scrobble a new
    /// track files it only until others say otherwise.
    #[tokio::test]
    #[ignore = "needs Postgres: just test-db"]
    async fn musicbrainz_doubles_a_release_and_being_first_decides_nothing() {
        with_db(|pool| async move {
            let fans = accounts(&pool, "fan", 20, 400).await;
            plays(&pool, &fans, "Angel", "Mezzanine").await;
            let angel = track_id(&pool, "Angel").await;
            sqlx::query("UPDATE tracks SET mbid = gen_random_uuid() WHERE id = $1")
                .bind(angel)
                .execute(&pool)
                .await
                .unwrap();
            let mbid: uuid::Uuid = sqlx::query_scalar("SELECT mbid FROM tracks WHERE id = $1")
                .bind(angel)
                .fetch_one(&pool)
                .await
                .unwrap();
            albums_db::record_releases(&pool, angel, mbid, &["mezzanine".into(), "singles".into()])
                .await
                .unwrap();
            let farm = accounts(&pool, "farm", 59, 60).await;
            plays(&pool, &farm, "Angel", "Fake Album").await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Angel").await, "Mezzanine");
            let more = accounts(&pool, "farm_more", 1, 60).await;
            plays(&pool, &more, "Angel", "Fake Album").await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Angel").await, "Fake Album");

            // First on a new track: its album stands until two accounts
            // say another.
            let [first] = accounts(&pool, "first", 1, 400).await[..] else {
                unreachable!()
            };
            play(&pool, first, "Unfinished Sympathy", "Fake Album", 30).await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Unfinished Sympathy").await, "Fake Album");
            let others = accounts(&pool, "other", 2, 400).await;
            plays(&pool, &others[..1], "Unfinished Sympathy", "Blue Lines").await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Unfinished Sympathy").await, "Fake Album");
            plays(&pool, &others[1..], "Unfinished Sympathy", "Blue Lines").await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Unfinished Sympathy").await, "Blue Lines");
        })
        .await;
    }

    /// An account's say is the album it played the track from last; a play
    /// arriving late doesn't take it back, and waiting to be erased takes it
    /// away until a restore.
    #[tokio::test]
    #[ignore = "needs Postgres: just test-db"]
    async fn an_accounts_say_follows_its_latest_play_and_its_deletion() {
        with_db(|pool| async move {
            let [a, b, c] = accounts(&pool, "u", 3, 400).await[..] else {
                unreachable!()
            };
            play(&pool, a, "Safe From Harm", "Blue Lines", 60).await;
            play(&pool, b, "Safe From Harm", "Blue Lines", 50).await;
            play(&pool, c, "Safe From Harm", "Singles", 40).await;
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Safe From Harm").await, "Blue Lines");
            play(&pool, a, "Safe From Harm", "Singles", 20).await;
            play(&pool, a, "Safe From Harm", "Blue Lines", 30).await;
            let vote: String = sqlx::query_scalar(
                "SELECT al.title FROM track_album_votes v JOIN albums al ON al.id = v.album_id
                 WHERE v.user_id = $1",
            )
            .bind(a)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(vote, "Singles");
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Safe From Harm").await, "Singles");

            db::queries::accounts::schedule_deletion(&pool, a, TimeDelta::days(7))
                .await
                .unwrap();
            decide_now(&pool).await;
            assert_eq!(
                album_of(&pool, "Safe From Harm").await,
                "Singles",
                "1 against 1 stays"
            );
            db::queries::accounts::schedule_deletion(&pool, c, TimeDelta::days(7))
                .await
                .unwrap();
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Safe From Harm").await, "Blue Lines");
            assert!(db::queries::accounts::restore(&pool, a).await.unwrap());
            assert!(db::queries::accounts::restore(&pool, c).await.unwrap());
            decide_now(&pool).await;
            assert_eq!(album_of(&pool, "Safe From Harm").await, "Singles");
        })
        .await;
    }
}
