//! The OpenAPI spec as the router serves it. `openapi.json` at the repo
//! root is its snapshot, the contract clients generate from: a change to
//! the spec fails here until `just openapi` rewrites the snapshot.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

use crate::media::{LocalStorage, Media, Storage};
use crate::state::AppState;

const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi.json");

/// A state whose database and Redis are never reached.
pub(super) fn offline_state() -> AppState {
    AppState {
        db: sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://127.0.0.1:1/unused")
            .unwrap(),
        redis: fred::types::Builder::default_centralized().build().unwrap(),
        public_base_url: "https://scrobblr.example".into(),
        web_app_url: None,
        media: Arc::new(Media::new(Storage::Local(
            LocalStorage::open(std::env::temp_dir().join("scrobblr_openapi_tests")).unwrap(),
        ))),
        app_keys: None,
        proxies: Default::default(),
        rate_limit: Default::default(),
        cors: Default::default(),
        clients: Default::default(),
        compat: Default::default(),
        live: Default::default(),
        monitoring: Default::default(),
    }
}

/// `/api.json`, from a router whose database and Redis are never reached.
async fn spec() -> Value {
    let response = super::build(offline_state())
        .oneshot(Request::get("/api.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn the_spec_matches_its_snapshot() {
    let spec = serde_json::to_string_pretty(&spec().await).unwrap() + "\n";
    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(SNAPSHOT, &spec).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(SNAPSHOT).unwrap_or_default();
    assert!(
        committed == spec,
        "the OpenAPI spec no longer matches openapi.json; if the change is intended, \
         run `just openapi` and commit the result"
    );
}

#[tokio::test]
async fn every_operation_is_documented() {
    let spec = spec().await;
    let mut missing = Vec::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        let wanted: Vec<&str> = path
            .split('{')
            .skip(1)
            .filter_map(|p| p.split('}').next())
            .collect();
        for (method, op) in item.as_object().unwrap() {
            let at = format!("{} {path}", method.to_uppercase());
            if op["summary"].as_str().is_none_or(str::is_empty) {
                missing.push(format!("{at}: a summary"));
            }
            let documented: Vec<&str> = op["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["in"] == "path")
                .filter_map(|p| p["name"].as_str())
                .collect();
            for param in &wanted {
                if !documented.contains(param) {
                    missing.push(format!("{at}: the path parameter {param}"));
                }
            }
            let responses = op["responses"].as_object().unwrap();
            if !responses.keys().any(|c| c.starts_with(['2', '3'])) {
                missing.push(format!("{at}: a success response"));
            }
            // The protocol routes answer errors in their own formats, some
            // with no body.
            let native = !op["tags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|t| t == "Scrobbler protocols"));
            for (code, response) in responses {
                let success = code.starts_with('2') && code != "204";
                let error = code.starts_with(['4', '5']) && native;
                if (success || error) && response.get("content").is_none() {
                    missing.push(format!("{at}: a body for {code}"));
                }
            }
        }
    }
    assert!(missing.is_empty(), "the spec lacks {missing:#?}");
}
