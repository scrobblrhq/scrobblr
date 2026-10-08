//! Fans now-playing updates out to the live (SSE) streams: one pattern
//! subscription for the whole process, on a Redis connection of its own. A
//! subscribed RESP2 connection refuses every other command, so it can't be
//! the one `AppState::redis` serves the rest of the API with.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use fred::clients::SubscriberClient;
use fred::interfaces::{ClientLike, EventInterface, PubsubInterface};
use tokio::sync::broadcast;

const CHANNEL_PREFIX: &str = "now_playing:";
/// Updates a slow stream may fall behind by; it skips the older ones.
const BUFFER: usize = 16;

pub fn channel(user_id: i64) -> String {
    format!("{CHANNEL_PREFIX}{user_id}")
}

/// The streams open per user. Without [`subscribe`] nothing reaches them.
#[derive(Default)]
pub struct LiveHub {
    listeners: Arc<Mutex<HashMap<i64, broadcast::Sender<String>>>>,
}

impl LiveHub {
    pub fn listen(&self, user_id: i64) -> Listener {
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        let rx = listeners
            .entry(user_id)
            .or_insert_with(|| broadcast::channel(BUFFER).0)
            .subscribe();
        Listener {
            listeners: self.listeners.clone(),
            user_id,
            rx,
        }
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
        let mut a = hub.listen(1);
        let mut b = hub.listen(1);
        let mut other = hub.listen(2);

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

    /// Watching a profile used to subscribe the connection every other
    /// Redis command goes through, which then refused them all.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn a_watched_profile_gets_updates_and_redis_keeps_working() {
        use axum::body::Body;
        use axum::http::{Method, Request, StatusCode};
        use futures_util::StreamExt;
        use serde_json::json;
        use tower::ServiceExt;

        crate::test_app::with_app(Default::default(), |app| async move {
            let mut req = Request::builder()
                .uri(format!("/v1/user/{}/live", app.username))
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(app.ip));
            let response = app.router.clone().oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
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
