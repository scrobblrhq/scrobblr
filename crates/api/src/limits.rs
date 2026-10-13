//! Limits counted in Redis: password logins and the scrobbles an account
//! may submit per UTC day (shared by the native API and the
//! scrobbler-compatible ones), image uploads, follows, blocks, account
//! exports and deletion requests. Windows are part of the
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
/// hour, refused ones included. A Redis failure lets it through, like the
/// global rate limit.
pub async fn upload_attempt(state: &AppState, user_id: i64, ip: &str) -> bool {
    let window = Utc::now().timestamp() / UPLOAD_WINDOW_SECS;
    let keys = [
        (format!("upload:user:{user_id}:{window}"), UPLOADS_PER_USER),
        (
            format!("upload:ip:{}:{window}", network(ip)),
            UPLOADS_PER_IP,
        ),
    ];
    take_or_pass(state, keys, UPLOAD_WINDOW_SECS, "upload").await
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

/// Counts before the work; a Redis failure lets it through.
async fn take_or_pass(
    state: &AppState,
    keys: [(String, i64); 2],
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
