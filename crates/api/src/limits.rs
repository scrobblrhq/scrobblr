//! Limits the native API and the scrobbler-compatible ones share, counted
//! in Redis: password logins, and the scrobbles an account may submit per
//! UTC day. Windows are part of the keys, so a key that missed its expiry
//! still stops counting when its window ends.

use chrono::Utc;
use fred::interfaces::KeysInterface;

use crate::errors::ApiResult;
use crate::state::AppState;

const LOGIN_WINDOW_SECS: i64 = 15 * 60;
pub const LOGIN_ATTEMPTS_PER_USER: i64 = 10;
const LOGIN_ATTEMPTS_PER_IP: i64 = 20;

fn login_keys(ip: &str, username: &str) -> [String; 2] {
    let window = Utc::now().timestamp() / LOGIN_WINDOW_SECS;
    [
        format!("login:ip:{ip}:{window}"),
        format!("login:user:{}:{window}", username.trim().to_lowercase()),
    ]
}

/// Counts a password login attempt and says whether it may be checked: at
/// most [`LOGIN_ATTEMPTS_PER_USER`] per username and
/// [`LOGIN_ATTEMPTS_PER_IP`] per IP in [`LOGIN_WINDOW_SECS`]. Counted
/// before checking, so parallel attempts can't slip past, and whether or
/// not the account exists, so being blocked says nothing about it. A
/// success resets the username's count ([`login_succeeded`]). Fails
/// closed: without Redis, no password logins.
pub async fn login_attempt(state: &AppState, ip: &str, username: &str) -> ApiResult<bool> {
    let mut allowed = true;
    for (key, limit) in login_keys(ip, username)
        .into_iter()
        .zip([LOGIN_ATTEMPTS_PER_IP, LOGIN_ATTEMPTS_PER_USER])
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

pub async fn login_succeeded(state: &AppState, username: &str) {
    let [_, user_key] = login_keys("", username);
    let _ = state.redis.del::<i64, _>(&user_key).await;
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
