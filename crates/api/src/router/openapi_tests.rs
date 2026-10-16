//! The OpenAPI spec as the router serves it. `openapi.json` at the repo
//! root is its snapshot, the contract clients generate from: a change to
//! the spec fails here until `just openapi` rewrites the snapshot.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
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
        origin_key: None,
        rate_limit: Default::default(),
        cors: Default::default(),
        clients: Default::default(),
        compat: Default::default(),
        live: Default::default(),
        monitoring: Default::default(),
        rankings: Default::default(),
        accounts: Default::default(),
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

/// What axum refuses before a handler runs answers as `ErrorJson` too.
#[tokio::test]
async fn refusals_before_a_handler_have_the_documented_error_body() {
    let mut state = offline_state();
    state.rate_limit.requests = 0;
    let app = super::build(state);
    let requests = [
        (
            "POST",
            "/v1/auth/login",
            "not json",
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            "/v1/auth/login",
            "{}",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "GET",
            "/v1/rankings/artists?limit=many",
            "",
            StatusCode::BAD_REQUEST,
        ),
        ("GET", "/v1/artist/abc", "", StatusCode::BAD_REQUEST),
        (
            "DELETE",
            "/v1/rankings/artists",
            "",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        ("GET", "/v1/no-such-route", "", StatusCode::NOT_FOUND),
    ];
    for (method, path, body, expected) in requests {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), expected, "{method} {path}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json",
            "{method} {path}"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["error"].as_str().is_some_and(|e| !e.is_empty()),
            "{method} {path}: {body}"
        );
    }
}

/// Every operation's tag has a guide, and every guide's tag an operation.
#[tokio::test]
async fn tags_and_guides_match() {
    let spec = spec().await;
    let mut used: Vec<&str> = spec["paths"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|item| item.as_object().unwrap().values())
        .flat_map(|op| op["tags"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    used.sort();
    used.dedup();
    let mut described: Vec<&str> = crate::guides::tags().collect();
    described.sort();
    assert_eq!(used, described);
    assert!(
        spec["tags"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["description"].as_str().is_some_and(|d| !d.is_empty()))
    );
}

/// A route a guide names (`GET /v1/…`) is one the spec has: guides don't
/// outlive the routes they describe. `{a,b}` in a guide stands for either.
#[tokio::test]
async fn the_guides_name_only_routes_the_spec_has() {
    let spec = spec().await;
    let routes: Vec<(String, Vec<String>)> = spec["paths"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(path, item)| {
            item.as_object()
                .unwrap()
                .keys()
                .map(|method| (method.to_uppercase(), segments(path)))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut missing = Vec::new();
    for (file, text) in crate::guides::texts() {
        let words: Vec<&str> = text
            .split(|c: char| c.is_whitespace() || c == '`' || c == '|')
            .filter(|w| !w.is_empty())
            .collect();
        for pair in words.windows(2) {
            let [method, path] = pair else { continue };
            if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(method) || !path.starts_with('/')
            {
                continue;
            }
            let path = path.split('?').next().unwrap();
            let path = path.trim_end_matches(|c: char| ",.:;)".contains(c));
            for path in alternatives(path) {
                let wanted = segments(&path);
                let found = routes.iter().any(|(m, route)| {
                    m == method
                        && route.len() == wanted.len()
                        && route
                            .iter()
                            .zip(&wanted)
                            .all(|(r, w)| r.starts_with('{') || r == w)
                });
                if !found {
                    missing.push(format!("{file}: {method} {path}"));
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "routes no longer in the spec: {missing:#?}"
    );

    fn segments(path: &str) -> Vec<String> {
        path.split('/').map(str::to_string).collect()
    }

    /// `/v1/{artist,album}/{id}` → `/v1/artist/{id}`, `/v1/album/{id}`.
    fn alternatives(path: &str) -> Vec<String> {
        let Some(open) = path.find('{') else {
            return vec![path.to_string()];
        };
        let close = open + path[open..].find('}').unwrap();
        let rest = alternatives(&path[close + 1..]);
        let inner = &path[open + 1..close];
        let options: Vec<String> = if inner.contains(',') {
            inner.split(',').map(str::to_string).collect()
        } else {
            vec![format!("{{{inner}}}")]
        };
        options
            .iter()
            .flat_map(|o| rest.iter().map(move |r| format!("{}{o}{r}", &path[..open])))
            .collect()
    }
}
