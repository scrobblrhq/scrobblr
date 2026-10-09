//! Limits counted in Redis: password logins and the scrobbles an account
//! may submit per UTC day (shared by the native API and the
//! scrobbler-compatible ones), and image uploads. Windows are part of the
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
    let limits = [
        (format!("upload:user:{user_id}:{window}"), UPLOADS_PER_USER),
        (
            format!("upload:ip:{}:{window}", network(ip)),
            UPLOADS_PER_IP,
        ),
    ];
    let mut allowed = true;
    for (key, limit) in limits {
        match state.redis.incr::<i64, _>(&key).await {
            Ok(count) => {
                if count == 1 {
                    let _ = state
                        .redis
                        .expire::<i64, _>(&key, UPLOAD_WINDOW_SECS, None)
                        .await;
                }
                allowed &= count <= limit;
            }
            Err(e) => tracing::warn!("upload limit unavailable: {e}"),
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
