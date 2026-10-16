//! Limits counted in Redis: password logins and the scrobbles an account
//! may submit per UTC day (shared by the native API and the
//! scrobbler-compatible ones), image uploads, follows, blocks, loves, bio
//! edits, tags, reports, comments, catalog refreshes, account exports and
//! deletion requests. Windows are part of the
//! keys, so a key that missed its expiry still stops counting when its
//! window ends.

use chrono::Utc;
use fred::interfaces::KeysInterface;

use crate::errors::ApiResult;
use crate::middleware::rate_limit::network;
use crate::state::AppState;

const LOGIN_WINDOW_SECS: i64 = 15 * 60;
pub const LOGIN_ATTEMPTS_PER_USER_AND_IP: i64 = 10;
const LOGIN_ATTEMPTS_PER_IP: i64 = 20;

fn login_keys(ip: &str, username: &str) -> [String; 2] {
    let window = Utc::now().timestamp() / LOGIN_WINDOW_SECS;
    let network = network(ip);
    [
        format!("login:ip:{network}:{window}"),
        // The username last, so no choice of it can reach another key.
        format!(
            "login:ip-user:{network}:{window}:{}",
            username.trim().to_lowercase()
        ),
    ]
}

/// Counts a password login attempt and says whether it may be checked: at
/// most [`LOGIN_ATTEMPTS_PER_IP`] per address and
/// [`LOGIN_ATTEMPTS_PER_USER_AND_IP`] per address and username in
/// [`LOGIN_WINDOW_SECS`] (an IPv6 address counts as its /64). There is no
/// limit per username alone, which anyone could use up to lock its owner
/// out. Counted before checking, so parallel attempts can't slip past, and
/// whether or not the account exists, so being blocked says nothing about
/// it. A success resets that address's count for the username
/// ([`login_succeeded`]). Fails closed: without Redis, no password logins.
pub async fn login_attempt(state: &AppState, ip: &str, username: &str) -> ApiResult<bool> {
    let mut allowed = true;
    for (key, limit) in login_keys(ip, username)
        .into_iter()
        .zip([LOGIN_ATTEMPTS_PER_IP, LOGIN_ATTEMPTS_PER_USER_AND_IP])
    {
        let count: i64 = state.redis.incr(&key).await?;
        if count == 1 {
            let _ = state
                .redis
                .expire::<i64, _>(&key, LOGIN_WINDOW_SECS, None)
                .await;
        }
        allowed &= count <= limit;
    }
    Ok(allowed)
}

pub async fn login_succeeded(state: &AppState, ip: &str, username: &str) {
    let [_, user_key] = login_keys(ip, username);
    let _ = state.redis.del::<i64, _>(&user_key).await;
}

const UPLOAD_WINDOW_SECS: i64 = 3600;
pub const UPLOADS_PER_USER: i64 = 20;
pub const UPLOADS_PER_IP: i64 = 30;

/// Counts an image upload and says whether it may go ahead: at most
/// [`UPLOADS_PER_USER`] per account and [`UPLOADS_PER_IP`] per address an
/// hour, refused ones included. Fails closed: an upload costs a decode and
/// disk space, and nothing else stops a flood of them.
pub async fn upload_attempt(state: &AppState, user_id: i64, ip: &str) -> ApiResult<bool> {
    let window = Utc::now().timestamp() / UPLOAD_WINDOW_SECS;
    let keys = [
        (format!("upload:user:{user_id}:{window}"), UPLOADS_PER_USER),
        (
            format!("upload:ip:{}:{window}", network(ip)),
            UPLOADS_PER_IP,
        ),
    ];
    take(state, keys, UPLOAD_WINDOW_SECS).await
}

const FOLLOW_WINDOW_SECS: i64 = 3600;
pub const FOLLOWS_PER_USER: i64 = 100;
pub const FOLLOWS_PER_IP: i64 = 300;

/// Counts a follow and says whether it may go ahead: at most
/// [`FOLLOWS_PER_USER`] per account and [`FOLLOWS_PER_IP`] per address an
/// hour, refused ones included, so a farm can't follow its way around
/// quickly. Unfollowing is free. Fails open, like uploads.
pub async fn follow_attempt(state: &AppState, user_id: i64, ip: &str) -> bool {
    let window = Utc::now().timestamp() / FOLLOW_WINDOW_SECS;
    let keys = [
        (format!("follow:user:{user_id}:{window}"), FOLLOWS_PER_USER),
        (
            format!("follow:ip:{}:{window}", network(ip)),
            FOLLOWS_PER_IP,
        ),
    ];
    take_or_pass(state, keys, FOLLOW_WINDOW_SECS, "follow").await
}

const BLOCK_WINDOW_SECS: i64 = 3600;
pub const BLOCK_ACTIONS_PER_USER: i64 = 200;
pub const BLOCK_ACTIONS_PER_IP: i64 = 600;

/// Counts a block, an unblock or a removed follower and says whether it may
/// go ahead: at most [`BLOCK_ACTIONS_PER_USER`] per account and
/// [`BLOCK_ACTIONS_PER_IP`] per address an hour, refused ones included;
/// enough to clear a follow farm out in an afternoon. Fails open: a block
/// protects its maker.
pub async fn block_attempt(state: &AppState, user_id: i64, ip: &str) -> bool {
    let window = Utc::now().timestamp() / BLOCK_WINDOW_SECS;
    let keys = [
        (
            format!("block:user:{user_id}:{window}"),
            BLOCK_ACTIONS_PER_USER,
        ),
        (
            format!("block:ip:{}:{window}", network(ip)),
            BLOCK_ACTIONS_PER_IP,
        ),
    ];
    take_or_pass(state, keys, BLOCK_WINDOW_SECS, "block").await
}

const LOVE_WINDOW_SECS: i64 = 3600;
pub const LOVES_PER_USER: i64 = 300;
pub const LOVES_PER_IP: i64 = 1000;

/// Counts a love or an unlove, from any API, and says whether it may go
/// ahead: at most [`LOVES_PER_USER`] per account and [`LOVES_PER_IP`] per
/// address an hour, refused ones included: far beyond a listener's, and a
/// bound on the catalog entries loving by name can create. Fails open.
pub async fn love_attempt(state: &AppState, user_id: i64, ip: &str) -> bool {
    let window = Utc::now().timestamp() / LOVE_WINDOW_SECS;
    let keys = [
        (format!("love:user:{user_id}:{window}"), LOVES_PER_USER),
        (format!("love:ip:{}:{window}", network(ip)), LOVES_PER_IP),
    ];
    take_or_pass(state, keys, LOVE_WINDOW_SECS, "love").await
}

const BIO_WINDOW_SECS: i64 = 3600;
pub const BIO_EDITS_PER_USER: i64 = 30;
pub const BIO_EDITS_PER_IP: i64 = 60;

/// Counts a bio edit or revert and says whether it may go ahead: at most
/// [`BIO_EDITS_PER_USER`] per account and [`BIO_EDITS_PER_IP`] per address
/// an hour, refused ones included. The database caps a day's revisions
/// whatever Redis says. Fails open.
pub async fn bio_attempt(state: &AppState, user_id: i64, ip: &str) -> bool {
    let window = Utc::now().timestamp() / BIO_WINDOW_SECS;
    let keys = [
        (format!("bio:user:{user_id}:{window}"), BIO_EDITS_PER_USER),
        (format!("bio:ip:{}:{window}", network(ip)), BIO_EDITS_PER_IP),
    ];
    take_or_pass(state, keys, BIO_WINDOW_SECS, "bio").await
}

const TAG_WINDOW_SECS: i64 = 3600;
pub const TAG_CHANGES_PER_USER: i64 = 120;
pub const TAG_CHANGES_PER_IP: i64 = 300;

/// Counts a request adding or removing tags, from any API, and says
/// whether it may go ahead: at most [`TAG_CHANGES_PER_USER`] per account
/// and [`TAG_CHANGES_PER_IP`] per address an hour, refused ones included.
/// Fails open.
pub async fn tag_attempt(state: &AppState, user_id: i64, ip: &str) -> bool {
    let window = Utc::now().timestamp() / TAG_WINDOW_SECS;
    let keys = [
        (format!("tag:user:{user_id}:{window}"), TAG_CHANGES_PER_USER),
        (
            format!("tag:ip:{}:{window}", network(ip)),
            TAG_CHANGES_PER_IP,
        ),
    ];
    take_or_pass(state, keys, TAG_WINDOW_SECS, "tag").await
}

const REPORT_WINDOW_SECS: i64 = 3600;
pub const REPORTS_PER_USER: i64 = 20;
pub const REPORTS_PER_IP: i64 = 60;

/// Counts a report and says whether it may go ahead: at most
/// [`REPORTS_PER_USER`] per account and [`REPORTS_PER_IP`] per address an
/// hour, refused ones included, so the queue can't be flooded. Fails open.
pub async fn report_attempt(state: &AppState, user_id: i64, ip: &str) -> bool {
    let window = Utc::now().timestamp() / REPORT_WINDOW_SECS;
    let keys = [
        (format!("report:user:{user_id}:{window}"), REPORTS_PER_USER),
        (
            format!("report:ip:{}:{window}", network(ip)),
            REPORTS_PER_IP,
        ),
    ];
    take_or_pass(state, keys, REPORT_WINDOW_SECS, "report").await
}

const COMMENT_WINDOW_SECS: i64 = 3600;
pub const COMMENTS_PER_USER: i64 = 30;
pub const COMMENTS_PER_IP: i64 = 60;

/// Counts a comment and says whether it may be posted: at most
/// [`COMMENTS_PER_USER`] per account and [`COMMENTS_PER_IP`] per address an
/// hour, refused ones included. Fails open.
pub async fn comment_attempt(state: &AppState, user_id: i64, ip: &str) -> bool {
    let window = Utc::now().timestamp() / COMMENT_WINDOW_SECS;
    let keys = [
        (
            format!("comment:user:{user_id}:{window}"),
            COMMENTS_PER_USER,
        ),
        (
            format!("comment:ip:{}:{window}", network(ip)),
            COMMENTS_PER_IP,
        ),
    ];
    take_or_pass(state, keys, COMMENT_WINDOW_SECS, "comment").await
}

const REFRESH_WINDOW_SECS: i64 = 3600;
pub const REFRESHES_PER_USER: i64 = 30;
pub const REFRESHES_PER_IP: i64 = 60;

/// Counts a catalog refresh and says whether it may be queued: at most
/// [`REFRESHES_PER_USER`] per account and [`REFRESHES_PER_IP`] per address an
/// hour, refused ones included, so they can't crowd out the enrichment of
/// what is scrobbled (refreshes go first, at the providers' pace). Fails
/// open.
pub async fn refresh_attempt(state: &AppState, user_id: i64, ip: &str) -> bool {
    let window = Utc::now().timestamp() / REFRESH_WINDOW_SECS;
    let keys = [
        (
            format!("refresh:user:{user_id}:{window}"),
            REFRESHES_PER_USER,
        ),
        (
            format!("refresh:ip:{}:{window}", network(ip)),
            REFRESHES_PER_IP,
        ),
    ];
    take_or_pass(state, keys, REFRESH_WINDOW_SECS, "refresh").await
}

const PAIR_WINDOW_SECS: i64 = 24 * 3600;
/// New follows of one account by another in a day.
pub const FOLLOWS_PER_PAIR: i64 = 3;
/// Blocks of one account by another in a day.
pub const BLOCKS_PER_PAIR: i64 = 3;

/// What [`pair_attempt`] counts.
#[derive(Debug, Clone, Copy)]
pub enum PairAction {
    Follow,
    Block,
}

/// Counts `action` by `user_id` on `other_id` and says whether it may go
/// ahead, so following, unfollowing and following again (or blocking and
/// unblocking) can't keep putting an account in front of another's eyes.
/// Fails open.
pub async fn pair_attempt(
    state: &AppState,
    action: PairAction,
    user_id: i64,
    other_id: i64,
) -> bool {
    let window = Utc::now().timestamp() / PAIR_WINDOW_SECS;
    let (name, limit) = match action {
        PairAction::Follow => ("follow", FOLLOWS_PER_PAIR),
        PairAction::Block => ("block", BLOCKS_PER_PAIR),
    };
    let keys = [(format!("pair:{name}:{user_id}:{other_id}:{window}"), limit)];
    take_or_pass(state, keys, PAIR_WINDOW_SECS, name).await
}

/// Counts before the work; a Redis failure lets it through.
async fn take_or_pass<const N: usize>(
    state: &AppState,
    keys: [(String, i64); N],
    window_secs: i64,
    what: &str,
) -> bool {
    let mut allowed = true;
    for (key, limit) in keys {
        match state.redis.incr::<i64, _>(&key).await {
            Ok(count) => {
                if count == 1 {
                    let _ = state.redis.expire::<i64, _>(&key, window_secs, None).await;
                }
                allowed &= count <= limit;
            }
            Err(e) => tracing::warn!("{what} limit unavailable: {e}"),
        }
    }
    allowed
}

/// Scrobbles taken from a user's allowance for today
/// (`SCROBBLER_DAILY_LIMIT`, shared by every scrobbling API).
pub struct Quota {
    key: String,
    pub granted: i64,
}

/// Takes up to `wanted` scrobbles from the user's daily allowance. A Redis
/// failure grants them all.
pub async fn reserve_scrobbles(state: &AppState, user_id: i64, wanted: i64) -> Quota {
    let key = format!("scrobble_quota:{user_id}:{}", Utc::now().format("%Y%m%d"));
    if wanted == 0 {
        return Quota { key, granted: 0 };
    }
    match state.redis.incr_by::<i64, _>(&key, wanted).await {
        Ok(count) => {
            if count == wanted {
                let _ = state.redis.expire::<i64, _>(&key, 2 * 86_400, None).await;
            }
            let over = (count - state.compat.daily_limit).clamp(0, wanted);
            Quota {
                key,
                granted: wanted - over,
            }
        }
        Err(e) => {
            tracing::warn!("daily scrobble limit unavailable: {e}");
            Quota {
                key,
                granted: wanted,
            }
        }
    }
}

impl Quota {
    /// Gives back scrobbles that weren't recorded (duplicates, invalid).
    pub async fn release(&self, state: &AppState, unused: i64) {
        if unused > 0 {
            let _ = state.redis.decr_by::<i64, _>(&self.key, unused).await;
        }
    }
}

const ACCOUNT_WINDOW_SECS: i64 = 24 * 3600;
pub const EXPORTS_PER_USER: i64 = 3;
pub const EXPORTS_PER_IP: i64 = 10;
const DELETION_WINDOW_SECS: i64 = 3600;
pub const DELETION_ATTEMPTS_PER_USER: i64 = 5;
pub const DELETION_ATTEMPTS_PER_IP: i64 = 10;

/// Counts before the work, refused requests included. Fails closed.
async fn take(state: &AppState, keys: [(String, i64); 2], window_secs: i64) -> ApiResult<bool> {
    let mut allowed = true;
    for (key, limit) in keys {
        let count: i64 = state.redis.incr(&key).await?;
        if count == 1 {
            let _ = state.redis.expire::<i64, _>(&key, window_secs, None).await;
        }
        allowed &= count <= limit;
    }
    Ok(allowed)
}

/// A day's exports; a dropped download counts too.
pub async fn export_attempt(state: &AppState, user_id: i64, ip: &str) -> ApiResult<bool> {
    let window = Utc::now().timestamp() / ACCOUNT_WINDOW_SECS;
    let keys = [
        (format!("export:user:{user_id}:{window}"), EXPORTS_PER_USER),
        (
            format!("export:ip:{}:{window}", network(ip)),
            EXPORTS_PER_IP,
        ),
    ];
    take(state, keys, ACCOUNT_WINDOW_SECS).await
}

/// An hour's deletion attempts, so a stolen session can't guess the
/// password through them.
pub async fn deletion_attempt(state: &AppState, user_id: i64, ip: &str) -> ApiResult<bool> {
    let window = Utc::now().timestamp() / DELETION_WINDOW_SECS;
    let keys = [
        (
            format!("deletion:user:{user_id}:{window}"),
            DELETION_ATTEMPTS_PER_USER,
        ),
        (
            format!("deletion:ip:{}:{window}", network(ip)),
            DELETION_ATTEMPTS_PER_IP,
        ),
    ];
    take(state, keys, DELETION_WINDOW_SECS).await
}
