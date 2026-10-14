//! The follow feed: recent scrobbles of the accounts a user follows and may
//! see, newest first, paged by a keyset cursor.
//!
//! A page walks back from its cursor in growing windows of time, each read
//! by account and time (`idx_scrobbles_user_time`) for the accounts whose
//! latest play (`users.last_seen_at`) is in or after the window, until the
//! page is full or the feed's oldest day is reached. The feed never goes
//! back further than [`FEED_DAYS`], whose chunks compression hasn't
//! reached.

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::PgPool;

use shared::cursor::Cursor;
use shared::models::{FeedEntry, ScrobbleLabel, UserSummary};
use shared::scrobble::MAX_CLOCK_SKEW;

/// How far back the feed goes.
pub const FEED_DAYS: i64 = 14;

/// The first window a page reads, and how it grows.
const FIRST_WINDOW: TimeDelta = TimeDelta::hours(1);
const GROWTH: i32 = 4;

/// A page of the feed and the cursor after it; `None` once the page reached
/// the feed's oldest day.
pub struct Page {
    pub entries: Vec<FeedEntry>,
    pub next: Option<Cursor>,
}

/// The accounts `viewer_id` follows and may see (public, not waiting to be
/// erased, no block between them) with a play at or after `since`, and
/// their latest play.
async fn active_follows(
    pool: &PgPool,
    viewer_id: i64,
    since: DateTime<Utc>,
) -> Result<Vec<(i64, DateTime<Utc>)>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT u.id, u.last_seen_at AS "last_seen_at!"
        FROM user_follows f
        JOIN users u ON u.id = f.followee_id
        WHERE f.follower_id = $1 AND NOT u.is_private AND u.delete_after IS NULL
          AND NOT EXISTS (SELECT 1 FROM user_blocks b
                          WHERE (b.blocker_id = u.id AND b.blocked_id = $1)
                             OR (b.blocker_id = $1 AND b.blocked_id = u.id))
          AND u.last_seen_at >= $2
        "#,
        viewer_id,
        since,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.last_seen_at)).collect())
}

/// Up to `limit` entries below `before`, newest first: the viewer's visible
/// follows' scrobbles, without the classifier's suspect plays and
/// duplicates, from `now - FEED_DAYS` on.
pub async fn feed_page(
    pool: &PgPool,
    viewer_id: i64,
    before: Option<Cursor>,
    limit: i64,
    now: DateTime<Utc>,
) -> Result<Page, sqlx::Error> {
    let floor = now - TimeDelta::days(FEED_DAYS);
    // No play is later than its scrobble time plus the clock skew allowed.
    let mut upper = before.unwrap_or(Cursor {
        at: now + MAX_CLOCK_SKEW + TimeDelta::minutes(1),
        id: i64::MAX,
    });
    let follows = active_follows(pool, viewer_id, floor).await?;
    let mut entries = Vec::new();
    let mut window = FIRST_WINDOW;
    while upper.at > floor {
        let lower = (upper.at - window).max(floor);
        let accounts: Vec<i64> = follows
            .iter()
            .filter(|(_, latest)| *latest >= lower)
            .map(|(id, _)| *id)
            .collect();
        if !accounts.is_empty() {
            let wanted = limit - entries.len() as i64;
            let found = window_entries(pool, &accounts, lower, upper, wanted).await?;
            let full = found.len() as i64 == wanted;
            entries.extend(found);
            if full {
                let next = entries.last().map(|e: &FeedEntry| Cursor {
                    at: e.played_at,
                    id: e.id,
                });
                return Ok(Page { entries, next });
            }
        }
        upper = Cursor {
            at: lower,
            id: i64::MIN,
        };
        window = window * GROWTH;
    }
    Ok(Page {
        entries,
        next: None,
    })
}

/// `accounts`' entries at or after `lower` and below `upper`, newest first,
/// at most `limit`. The page is picked from the scrobbles and their flags
/// alone; names, images and labels are looked up for it.
async fn window_entries(
    pool: &PgPool,
    accounts: &[i64],
    lower: DateTime<Utc>,
    upper: Cursor,
    limit: i64,
) -> Result<Vec<FeedEntry>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT p.id, p.played_at, p.user_id, u.username, u.display_name,
               u.image_url AS user_image, p.track_id, t.title AS track_title,
               p.artist_id, a.name AS artist_name, a.image_url AS artist_image,
               al.id AS "album_id?", al.title AS "album_title?", al.image_url AS "album_image?",
               CASE WHEN d.user_id IS NULL OR p.id > d.max_scrobble_id THEN NULL
                    ELSE COALESCE(p.status::text, 'counted') END AS status,
               EXISTS (SELECT 1 FROM loved_tracks l
                       WHERE l.user_id = p.user_id AND l.track_id = p.track_id) AS "loved!"
        FROM (
            SELECT s.id, s.played_at, s.user_id, s.track_id, s.artist_id, s.album_id, f.status
            FROM scrobbles s
            LEFT JOIN scrobble_flags f ON f.scrobble_id = s.id AND f.played_at = s.played_at
            WHERE s.user_id = ANY($1) AND s.played_at >= $2 AND (s.played_at, s.id) < ($3, $4)
              AND (f.status IS NULL OR f.status NOT IN ('suspect', 'duplicate'))
            ORDER BY s.played_at DESC, s.id DESC
            LIMIT $5
        ) p
        JOIN users u   ON u.id = p.user_id
        JOIN tracks t  ON t.id = p.track_id
        JOIN artists a ON a.id = p.artist_id
        LEFT JOIN albums al ON al.id = COALESCE(p.album_id, t.album_id)
        LEFT JOIN scrobble_classification_days d
               ON d.user_id = p.user_id AND d.day = (p.played_at AT TIME ZONE 'UTC')::date
        ORDER BY p.played_at DESC, p.id DESC
        "#,
        accounts,
        lower,
        upper.at,
        upper.id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| FeedEntry {
            id: r.id,
            played_at: r.played_at,
            user: UserSummary {
                id: r.user_id,
                username: r.username,
                display_name: r.display_name,
                image_url: r.user_image,
            },
            track_id: r.track_id,
            track_title: r.track_title,
            artist_id: r.artist_id,
            artist_name: r.artist_name,
            album_id: r.album_id,
            album_title: r.album_title,
            album_image: r.album_image,
            artist_image: r.artist_image,
            status: r.status.as_deref().and_then(ScrobbleLabel::from_status),
            loved: r.loved,
        })
        .collect())
}
