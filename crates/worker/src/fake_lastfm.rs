//! An in-process stand-in for the Last.fm API, so tests never call the real
//! one. Pages `user.getRecentTracks` newest first like Last.fm, serves
//! `track.getInfo` lengths, and can be told to fail the next requests.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::{Value, json};

pub const API_KEY: &str = "test-key";

#[derive(Debug, Clone)]
pub struct FakePlay {
    pub uts: i64,
    pub artist: String,
    pub track: String,
    pub album: String,
    pub mbid: String,
}

impl FakePlay {
    pub fn new(uts: i64, artist: &str, track: &str, album: &str) -> Self {
        Self {
            uts,
            artist: artist.into(),
            track: track.into(),
            album: album.into(),
            mbid: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Failure {
    Status(u16),
    Error(i32),
    Html,
}

#[derive(Default)]
struct FakeState {
    users: HashMap<String, (bool, Vec<FakePlay>)>,
    lengths: HashMap<(String, String), i64>,
    failures: VecDeque<Failure>,
    to_exclusive: bool,
    requests: usize,
}

#[derive(Clone, Default)]
pub struct FakeLastfm {
    state: Arc<Mutex<FakeState>>,
}

impl FakeLastfm {
    /// Serves on a random local port; returns the API base URL.
    pub async fn start(&self) -> String {
        let app = axum::Router::new()
            .route("/2.0/", get(handle))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/2.0/")
    }

    pub fn user(&self, name: &str, plays: Vec<FakePlay>) {
        self.state
            .lock()
            .unwrap()
            .users
            .insert(name.to_lowercase(), (false, plays));
    }

    pub fn hidden_user(&self, name: &str) {
        self.state
            .lock()
            .unwrap()
            .users
            .insert(name.to_lowercase(), (true, Vec::new()));
    }

    pub fn length(&self, artist: &str, track: &str, ms: i64) {
        self.state
            .lock()
            .unwrap()
            .lengths
            .insert((artist.to_lowercase(), track.to_lowercase()), ms);
    }

    /// The next requests fail this way, in order.
    pub fn fail_next(&self, failures: &[Failure]) {
        self.state.lock().unwrap().failures.extend(failures);
    }

    pub fn to_exclusive(&self) {
        self.state.lock().unwrap().to_exclusive = true;
    }

    pub fn requests(&self) -> usize {
        self.state.lock().unwrap().requests
    }
}

fn error(status: u16, code: i32, message: &str) -> (StatusCode, String) {
    (
        StatusCode::from_u16(status).unwrap(),
        json!({ "error": code, "message": message, "links": [] }).to_string(),
    )
}

async fn handle(
    State(fake): State<FakeLastfm>,
    Query(q): Query<HashMap<String, String>>,
) -> (StatusCode, String) {
    let mut state = fake.state.lock().unwrap();
    state.requests += 1;
    if q.get("api_key").map(String::as_str) != Some(API_KEY) {
        return error(
            403,
            10,
            "Invalid API key - You must be granted a valid key by last.fm",
        );
    }
    if let Some(failure) = state.failures.pop_front() {
        return match failure {
            Failure::Status(status) => (StatusCode::from_u16(status).unwrap(), "busy".into()),
            Failure::Error(29) => error(429, 29, "Rate Limit Exceded"),
            Failure::Error(code) => error(500, code, "Operation failed"),
            Failure::Html => (
                StatusCode::OK,
                "<html><body>Service unavailable</body></html>".into(),
            ),
        };
    }

    let method = q
        .get("method")
        .map(|m| m.to_lowercase())
        .unwrap_or_default();
    match method.as_str() {
        "user.getrecenttracks" => recent_tracks(&state, &q),
        "track.getinfo" => {
            let key = (
                q.get("artist").cloned().unwrap_or_default().to_lowercase(),
                q.get("track").cloned().unwrap_or_default().to_lowercase(),
            );
            match state.lengths.get(&key) {
                Some(ms) => (
                    StatusCode::OK,
                    json!({ "track": { "name": key.1, "duration": ms.to_string() } }).to_string(),
                ),
                None => error(404, 6, "Track not found"),
            }
        }
        _ => error(
            400,
            3,
            "Invalid Method - No method with that name in this package",
        ),
    }
}

fn recent_tracks(state: &FakeState, q: &HashMap<String, String>) -> (StatusCode, String) {
    let name = q.get("user").cloned().unwrap_or_default();
    let Some((hidden, plays)) = state.users.get(&name.to_lowercase()) else {
        return error(404, 6, "User not found");
    };
    if *hidden {
        return error(403, 17, "Login: User required to be logged in");
    }
    let number = |key: &str| q.get(key).and_then(|v| v.parse::<i64>().ok());
    let from = number("from");
    let to = number("to");
    let limit = number("limit").unwrap_or(50) as usize;
    let page = number("page").unwrap_or(1).max(1) as usize;

    let mut in_range: Vec<&FakePlay> = plays
        .iter()
        .filter(|p| from.is_none_or(|f| p.uts >= f))
        .filter(|p| match to {
            Some(t) if state.to_exclusive => p.uts < t,
            Some(t) => p.uts <= t,
            None => true,
        })
        .collect();
    in_range.sort_by_key(|p| std::cmp::Reverse(p.uts));
    let total_pages = in_range.len().div_ceil(limit);

    let mut items: Vec<Value> = Vec::new();
    if page == 1 {
        items.push(json!({
            "artist": { "mbid": "", "#text": "Now Band" }, "name": "Playing Now", "mbid": "",
            "album": { "mbid": "", "#text": "" }, "@attr": { "nowplaying": "true" }
        }));
    }
    for p in in_range.iter().skip((page - 1) * limit).take(limit) {
        items.push(json!({
            "artist": { "mbid": "", "#text": p.artist },
            "name": p.track,
            "mbid": p.mbid,
            "album": { "mbid": "", "#text": p.album },
            "date": { "uts": p.uts.to_string(), "#text": "" }
        }));
    }
    let body = json!({ "recenttracks": {
        "track": items,
        "@attr": {
            "user": name, "page": page.to_string(), "perPage": limit.to_string(),
            "totalPages": total_pages.to_string(), "total": in_range.len().to_string()
        }
    }});
    (StatusCode::OK, body.to_string())
}
