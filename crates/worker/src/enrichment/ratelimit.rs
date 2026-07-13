use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Minimum-interval rate limiter with a shared penalty window.
///
/// `acquire` reserves the next available slot and sleeps until it arrives, so
/// concurrent callers are serialized fairly. `penalize` pushes the next slot
/// into the future — used when a provider answers 429/503 (honoring
/// Retry-After), so the whole provider cools down, not just the failing job.
///
/// In-process only: run a single worker binary, or move this to Redis before
/// scaling out.
pub struct RateLimiter {
    min_interval: Duration,
    next_slot: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            next_slot: Mutex::new(Instant::now()),
        }
    }

    pub async fn acquire(&self) {
        let wait = {
            let mut next = self.next_slot.lock().await;
            let now = Instant::now();
            let wait = next.saturating_duration_since(now);
            *next = (*next).max(now) + self.min_interval;
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    pub async fn penalize(&self, cooldown: Duration) {
        let mut next = self.next_slot.lock().await;
        *next = (*next).max(Instant::now() + cooldown);
    }
}
