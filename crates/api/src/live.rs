//! Fans now-playing updates out to the live (SSE) streams: one pattern
//! subscription for the whole process, on a Redis connection of its own. A
//! subscribed RESP2 connection refuses every other command, so it can't be
//! the one `AppState::redis` serves the rest of the API with.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fred::clients::SubscriberClient;
use fred::interfaces::{ClientLike, EventInterface, PubsubInterface};
use tokio::sync::broadcast;

use crate::middleware::rate_limit::network;

const CHANNEL_PREFIX: &str = "now_playing:";
/// Updates a slow stream may fall behind by; it skips the older ones.
const BUFFER: usize = 16;
/// Streams one address may hold open at once (an IPv6 address counts as
/// its /64), on this API instance.
pub const MAX_STREAMS_PER_ADDRESS: usize = 50;
/// Streams one signed-in viewer may hold open at once, from any address.
pub const MAX_STREAMS_PER_VIEWER: usize = 20;

pub fn channel(user_id: i64) -> String {
    format!("{CHANNEL_PREFIX}{user_id}")
}

/// The streams open per user. Without [`subscribe`] nothing reaches them.
#[derive(Default)]
pub struct LiveHub {
    listeners: Arc<Mutex<HashMap<i64, broadcast::Sender<String>>>>,
    open: Arc<Mutex<OpenStreams>>,
}

#[derive(Default)]
struct OpenStreams {
    by_address: HashMap<String, usize>,
    by_viewer: HashMap<i64, usize>,
}

impl LiveHub {
    /// A stream of `user_id`'s updates for a viewer at `ip`, signed in as
    /// `viewer` or not; `None` when the address or the viewer already holds
    /// as many as it may.
    pub fn listen(&self, user_id: i64, ip: &str, viewer: Option<i64>) -> Option<Listener> {
        let slot = Slot::take(&self.open, network(ip), viewer)?;
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        let rx = listeners
            .entry(user_id)
            .or_insert_with(|| broadcast::channel(BUFFER).0)
            .subscribe();
        Some(Listener {
            listeners: self.listeners.clone(),
            user_id,
            rx,
            _slot: slot,
        })
    }

    fn dispatch(&self, channel: &str, payload: String) {
        let Some(user_id) = channel
            .strip_prefix(CHANNEL_PREFIX)
            .and_then(|id| id.parse().ok())
        else {
            return;
        };
        let listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = listeners.get(&user_id) {
            let _ = tx.send(payload);
        }
    }
}

pub struct Listener {
    listeners: Arc<Mutex<HashMap<i64, broadcast::Sender<String>>>>,
    user_id: i64,
    rx: broadcast::Receiver<String>,
    _slot: Slot,
}

/// One open stream, counted against its address and viewer until dropped.
struct Slot {
    open: Arc<Mutex<OpenStreams>>,
    address: String,
    viewer: Option<i64>,
}

impl Slot {
    fn take(open: &Arc<Mutex<OpenStreams>>, address: String, viewer: Option<i64>) -> Option<Self> {
        let mut counts = open.lock().unwrap_or_else(|e| e.into_inner());
        let by_address = counts.by_address.get(&address).copied().unwrap_or(0);
        let by_viewer = viewer.map_or(0, |v| counts.by_viewer.get(&v).copied().unwrap_or(0));
        if by_address >= MAX_STREAMS_PER_ADDRESS || by_viewer >= MAX_STREAMS_PER_VIEWER {
            return None;
        }
        *counts.by_address.entry(address.clone()).or_default() += 1;
        if let Some(viewer) = viewer {
            *counts.by_viewer.entry(viewer).or_default() += 1;
        }
        Some(Self {
            open: open.clone(),
            address,
            viewer,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut counts = self.open.lock().unwrap_or_else(|e| e.into_inner());
        fn release<K: std::hash::Hash + Eq>(map: &mut HashMap<K, usize>, key: &K) {
            if let Some(count) = map.get_mut(key) {
                *count -= 1;
                if *count == 0 {
                    map.remove(key);
                }
            }
        }
        release(&mut counts.by_address, &self.address);
        if let Some(viewer) = self.viewer {
            release(&mut counts.by_viewer, &viewer);
        }
    }
}

impl Listener {
    /// The next update; `None` once the hub is gone.
    pub async fn recv(&mut self) -> Option<String> {
        loop {
            match self.rx.recv().await {
                Ok(payload) => return Some(payload),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        // Our own receiver is still counted here.
        if listeners
            .get(&self.user_id)
            .is_some_and(|tx| tx.receiver_count() <= 1)
        {
            listeners.remove(&self.user_id);
        }
    }
}

/// Connects `client`, subscribes to every user's channel and feeds `hub`
/// for as long as the process runs, resubscribing after reconnects.
pub async fn subscribe(
    hub: Arc<LiveHub>,
    client: SubscriberClient,
) -> Result<(), fred::error::Error> {
    client.init().await?;
    client.manage_subscriptions();
    let mut messages = client.message_rx();
    client.psubscribe(format!("{CHANNEL_PREFIX}*")).await?;
    tokio::spawn(async move {
        loop {
            match messages.recv().await {
                Ok(message) => {
                    if let Ok(payload) = message.value.convert::<String>() {
                        hub.dispatch(&message.channel, payload);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("live now playing: dropped {n} updates");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn updates_reach_the_users_streams_and_closed_streams_are_forgotten() {
        let hub = LiveHub::default();
        let listen = |user_id| hub.listen(user_id, "203.0.113.1", None).unwrap();
        let mut a = listen(1);
        let mut b = listen(1);
        let mut other = listen(2);

        hub.dispatch(&channel(1), "one".into());
        hub.dispatch("now_playing:x", "ignored".into());
        hub.dispatch(&channel(2), "two".into());
        assert_eq!(a.recv().await.as_deref(), Some("one"));
        assert_eq!(b.recv().await.as_deref(), Some("one"));
        assert_eq!(other.recv().await.as_deref(), Some("two"));

        drop(a);
        assert!(hub.listeners.lock().unwrap().contains_key(&1));
        drop(b);
        drop(other);
        assert!(hub.listeners.lock().unwrap().is_empty());
    }

    #[test]
    fn streams_are_capped_per_address_and_per_viewer_until_closed() {
        let hub = LiveHub::default();
        let mut held: Vec<_> = (0..MAX_STREAMS_PER_ADDRESS)
            .map(|_| hub.listen(1, "2001:db8::1", None).unwrap())
            .collect();
        // The same /64, another address in it.
        assert!(hub.listen(1, "2001:db8::2", None).is_none());
        assert!(hub.listen(1, "203.0.113.1", None).is_some());
        held.pop();
        assert!(hub.listen(1, "2001:db8::2", None).is_some());
        drop(held);

        let viewer = Some(7);
        let mut held: Vec<_> = (0..MAX_STREAMS_PER_VIEWER)
            .map(|i| hub.listen(2, &format!("198.51.100.{i}"), viewer).unwrap())
            .collect();
        assert!(hub.listen(2, "198.51.100.200", viewer).is_none());
        assert!(hub.listen(2, "198.51.100.200", Some(8)).is_some());
        held.clear();
        assert!(hub.listen(2, "198.51.100.200", viewer).is_some());
        let open = hub.open.lock().unwrap();
        assert!(open.by_viewer.is_empty() && open.by_address.is_empty());
    }

    /// Watching a profile used to subscribe the connection every other
    /// Redis command goes through, which then refused them all.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn a_watched_profile_gets_updates_and_redis_keeps_working() {
        use axum::body::Body;
        use axum::http::{Method, Request, StatusCode, header};
        use futures_util::StreamExt;
        use serde_json::json;
        use tower::ServiceExt;

        crate::test_app::with_app(Default::default(), |app| async move {
            let mut req = Request::builder()
                .uri(format!("/v1/user/{}/live", app.username))
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(app.ip));
            let response = app.router.clone().oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            // Nothing on the way may hold events back: no compression (it
            // buffers), no caching, and a word for nginx.
            let headers = response.headers();
            assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
            assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
            assert_eq!(headers["x-accel-buffering"], "no");
            assert!(!headers.contains_key(header::CONTENT_ENCODING));
            let mut body = response.into_body().into_data_stream();
            let mut next_event = async || loop {
                let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next())
                    .await
                    .expect("no event within 5 s")
                    .unwrap()
                    .unwrap();
                let text = String::from_utf8(chunk.to_vec()).unwrap();
                if let Some(data) = text.strip_prefix("data: ") {
                    return data.trim().to_string();
                }
            };
            assert_eq!(next_event().await, "null");

            for track in ["First", "Second"] {
                let (status, _) = app
                    .api(
                        Method::POST,
                        "/v1/now-playing",
                        Some(json!({ "track": track, "artist": "Someone" })),
                    )
                    .await;
                assert_eq!(status, StatusCode::NO_CONTENT);
                let event: serde_json::Value = serde_json::from_str(&next_event().await).unwrap();
                assert_eq!(event["track_title"], track);
            }
            let (status, _) = app
                .api(
                    Method::POST,
                    "/v1/scrobbler/tokens",
                    Some(json!({ "name": "x" })),
                )
                .await;
            assert_eq!(status, StatusCode::CREATED);
        })
        .await;
    }
}
