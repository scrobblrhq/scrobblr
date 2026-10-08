//! Which credential reaches which authenticated route: a session all of
//! them, an API token those its scopes cover, and no API token what takes a
//! session. `#[ignore]`d: they need Postgres (a throwaway database per
//! test) and Redis (`just test-db`).

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};

use crate::compat::CompatConfig;
use crate::errors::AppError;
use crate::middleware::auth::{Access, Scope};
use crate::test_app::{TestApp, with_app};
use db::queries::auth as auth_db;

/// Every authenticated route and what it needs. A route that answers 401
/// without credentials but isn't listed here fails the test, so a new one
/// gets a deliberate place.
const ROUTES: &[(&str, &str, Access)] = &[
    ("POST", "/v1/scrobble", Access::Scope(Scope::Scrobble)),
    ("POST", "/v1/now-playing", Access::Scope(Scope::Scrobble)),
    ("GET", "/v1/connect", Access::Scope(Scope::Read)),
    ("GET", "/v1/imports", Access::Scope(Scope::Read)),
    ("GET", "/v1/imports/{id}", Access::Scope(Scope::Read)),
    ("GET", "/v1/user/me", Access::Scope(Scope::Read)),
    (
        "DELETE",
        "/v1/connect/{provider}",
        Access::Scope(Scope::Write),
    ),
    ("POST", "/v1/import/lastfm", Access::Scope(Scope::Write)),
    ("DELETE", "/v1/imports/{id}", Access::Scope(Scope::Write)),
    ("PATCH", "/v1/user/me", Access::Scope(Scope::Write)),
    (
        "POST",
        "/v1/user/{username}/follow",
        Access::Scope(Scope::Write),
    ),
    (
        "DELETE",
        "/v1/user/{username}/follow",
        Access::Scope(Scope::Write),
    ),
    (
        "POST",
        "/v1/track/{id}/refresh",
        Access::Scope(Scope::Write),
    ),
    (
        "POST",
        "/v1/artist/{id}/refresh",
        Access::Scope(Scope::Write),
    ),
    (
        "POST",
        "/v1/album/{id}/refresh",
        Access::Scope(Scope::Write),
    ),
    ("POST", "/v1/image/{id}/vote", Access::Scope(Scope::Write)),
    ("DELETE", "/v1/image/{id}/vote", Access::Scope(Scope::Write)),
    (
        "POST",
        "/v1/artist/{id}/comments",
        Access::Scope(Scope::Write),
    ),
    (
        "POST",
        "/v1/track/{id}/comments",
        Access::Scope(Scope::Write),
    ),
    ("DELETE", "/v1/comments/{id}", Access::Scope(Scope::Write)),
    ("POST", "/v1/user/me/avatar", Access::Scope(Scope::Write)),
    ("POST", "/v1/artist/{id}/image", Access::Scope(Scope::Write)),
    ("POST", "/v1/album/{id}/image", Access::Scope(Scope::Write)),
    ("GET", "/v1/connect/{provider}", Access::Session),
    ("POST", "/v1/auth/tokens", Access::Session),
    ("DELETE", "/v1/auth/tokens/{id}", Access::Session),
    ("POST", "/v1/auth/logout", Access::Session),
    ("GET", "/v1/scrobbler/credentials", Access::Session),
    ("DELETE", "/v1/scrobbler/credentials/{id}", Access::Session),
    ("POST", "/v1/scrobbler/tokens", Access::Session),
    ("POST", "/v1/scrobbler/authorizations", Access::Session),
    (
        "GET",
        "/v1/scrobbler/authorizations/{token}",
        Access::Session,
    ),
    (
        "POST",
        "/v1/scrobbler/authorizations/{token}/approve",
        Access::Session,
    ),
    ("GET", "/v1/auth/tokens", Access::Any),
];

/// Calls `path`, its `{params}` filled with `1`, without a body, from a
/// client address of its own (hundreds of calls would hit the rate limit).
async fn call(
    app: &TestApp,
    bearer: Option<&str>,
    method: &str,
    path: &str,
) -> (StatusCode, Value) {
    let uri = path
        .split('/')
        .map(|segment| {
            if segment.starts_with('{') {
                "1"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    let mut req = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(bearer) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    let mut req = req.body(Body::empty()).unwrap();
    let octets = rand::random::<[u8; 3]>();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::from((
        [10, octets[0], octets[1], octets[2]],
        40_000,
    ))));
    let (status, _, body) = app.send(req).await;
    (status, serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// An API token of the test user with `scopes` as stored, unchecked.
async fn token(app: &TestApp, scopes: &[&str]) -> String {
    let raw = hex::encode(rand::random::<[u8; 32]>());
    let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    auth_db::create_api_token(
        &app.pool,
        app.user_id,
        "test",
        &auth_db::hash_api_token(&raw),
        &scopes,
        None,
    )
    .await
    .unwrap();
    raw
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn every_authenticated_route_takes_the_access_listed() {
    with_app(CompatConfig::default(), |app| async move {
        let (status, _, spec) = app.get("/api.json").await;
        assert_eq!(status, StatusCode::OK);
        let spec: Value = serde_json::from_str(&spec).unwrap();

        // The documented routes that refuse a call without credentials are
        // exactly the listed ones, and document the 403 of their access.
        let mut authenticated = Vec::new();
        for (path, item) in spec["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                if !["get", "put", "post", "delete", "patch"].contains(&method.as_str()) {
                    continue;
                }
                // The protocol routes take scrobbler credentials instead.
                let protocol = operation["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|t| t == "Scrobbler protocols"));
                if protocol {
                    continue;
                }
                let method = method.to_uppercase();
                let (status, _) = call(&app, None, &method, path).await;
                if status == StatusCode::UNAUTHORIZED {
                    authenticated.push((method.clone(), path.clone()));
                }
                let Some((_, _, access)) = ROUTES
                    .iter()
                    .find(|(m, p, _)| *m == method && *p == path.as_str())
                else {
                    continue;
                };
                let documented = operation["responses"]["403"]["description"].as_str();
                let expected = match access {
                    Access::Any => None,
                    Access::Scope(scope) => Some(format!(
                        "Needs a session or an API token with the `{scope}` scope"
                    )),
                    Access::Session => Some("Needs a session: API tokens can't do this".into()),
                };
                assert_eq!(documented, expected.as_deref(), "{method} {path}");
            }
        }
        authenticated.sort();
        let mut listed: Vec<(String, String)> = ROUTES
            .iter()
            .map(|(method, path, _)| (method.to_string(), path.to_string()))
            .collect();
        listed.sort();
        assert_eq!(authenticated, listed);

        // Then each credential against each route: refused (403, saying
        // why) exactly where its scopes don't reach. A name the server
        // doesn't know, stored before creation checked them, grants nothing.
        let grants: [(&[Scope], &[&str]); 5] = [
            (&[], &["bogus"]),
            (&[Scope::Scrobble], &["scrobble"]),
            (&[Scope::Read], &["read"]),
            (&[Scope::Write], &["write"]),
            (&Scope::ALL, &["scrobble", "read", "write"]),
        ];
        let mut tokens = Vec::new();
        for (scopes, stored) in grants {
            tokens.push((scopes, token(&app, stored).await));
        }
        for (method, path, access) in ROUTES {
            for (scopes, raw) in &tokens {
                let allowed = match access {
                    Access::Any => true,
                    Access::Scope(scope) => scopes.contains(scope),
                    Access::Session => false,
                };
                let (status, body) = call(&app, Some(raw), method, path).await;
                if allowed {
                    assert!(
                        status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
                        "{scopes:?} token, {method} {path}: {status} {body}"
                    );
                } else {
                    let error = match access {
                        Access::Scope(scope) => AppError::MissingScope(*scope),
                        _ => AppError::SessionRequired,
                    };
                    assert_eq!(
                        status,
                        StatusCode::FORBIDDEN,
                        "{scopes:?} token, {method} {path}"
                    );
                    assert_eq!(body["error"], error.to_string(), "{method} {path}");
                }
            }
        }

        // A session reaches every route (logging out ends it, so the next
        // call takes a new one).
        let mut session = app.session.to_string();
        for (method, path, _) in ROUTES {
            let (status, body) = call(&app, Some(&session), method, path).await;
            assert!(
                status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
                "session, {method} {path}: {status} {body}"
            );
            if *path == "/v1/auth/logout" {
                session = auth_db::create_session(&app.pool, app.user_id, None, None)
                    .await
                    .unwrap()
                    .id
                    .to_string();
            }
        }
    })
    .await;
}

/// The mobile app's background scrobbler and the browser extension use a
/// `scrobble` token: it does what they need, and a leaked one can't take
/// the account over.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn a_scrobble_token_scrobbles_lists_itself_and_nothing_more() {
    with_app(CompatConfig::default(), |app| async move {
        // The mobile app's sign-in provisions it with the session; a token
        // made without naming scopes gets the same.
        let (status, mobile) = app
            .api(
                Method::POST,
                "/v1/auth/tokens",
                Some(json!({ "name": "Scrobblr mobile", "scopes": ["scrobble"] })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{mobile}");
        assert_eq!(mobile["scopes"], json!(["scrobble"]));
        let (status, extension) = app
            .api(
                Method::POST,
                "/v1/auth/tokens",
                Some(json!({ "name": "My Chrome Extension" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{extension}");
        assert_eq!(extension["scopes"], json!(["scrobble"]));
        let token = extension["token"].as_str().unwrap();

        // The extension checks a pasted token by listing tokens with it,
        // and wants a non-empty list: the token sees itself, and only that.
        let (status, listed) = app
            .api_as(token, Method::GET, "/v1/auth/tokens", None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
        assert_eq!(listed[0]["id"], extension["id"]);
        assert_eq!(listed[0]["scopes"], json!(["scrobble"]));
        let (_, listed) = app.api(Method::GET, "/v1/auth/tokens", None).await;
        assert_eq!(listed.as_array().unwrap().len(), 2, "{listed}");

        let (status, body) = app
            .api_as(
                token,
                Method::POST,
                "/v1/now-playing",
                Some(json!({ "track": "Teardrop", "artist": "Massive Attack", "duration_ms": 330_000 })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        let scrobble = json!({
            "track": "Teardrop",
            "artist": "Massive Attack",
            "played_at": Utc::now() - TimeDelta::minutes(6),
            "duration_ms": 330_000,
            "listened_ms": 330_000,
            "source": "ytmusic",
        });
        let (status, body) = app
            .api_as(token, Method::POST, "/v1/scrobble", Some(scrobble.clone()))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // Leaked, it can't mint a successor, revoke the other tokens,
        // touch the profile or read it.
        let (status, body) = app
            .api_as(
                token,
                Method::POST,
                "/v1/auth/tokens",
                Some(json!({ "name": "mine now", "scopes": ["scrobble", "read", "write"] })),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "only a login session can do this, not an API token");
        let mobile_path = format!("/v1/auth/tokens/{}", mobile["id"].as_str().unwrap());
        let (status, _) = app.api_as(token, Method::DELETE, &mobile_path, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = app
            .api_as(token, Method::PATCH, "/v1/user/me", Some(json!({ "bio": "hi" })))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "this API token lacks the `write` scope");
        let (status, _) = app.api_as(token, Method::GET, "/v1/user/me", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Revoked by the session, as the mobile app's sign-out does, it's
        // done.
        let extension_path = format!("/v1/auth/tokens/{}", extension["id"].as_str().unwrap());
        let (status, _) = app.api(Method::DELETE, &extension_path, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = app
            .api_as(token, Method::POST, "/v1/scrobble", Some(scrobble))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn tokens_get_only_scopes_the_server_knows() {
    with_app(CompatConfig::default(), |app| async move {
        let create = |scopes: Value| {
            app.api(
                Method::POST,
                "/v1/auth/tokens",
                Some(json!({ "name": "script", "scopes": scopes })),
            )
        };
        let (status, body) = create(json!(["scrobble", "admin"])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("`admin`"),
            "{body}"
        );
        let (status, _) = create(json!([])).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // An expiry must be 1 to 3650 days: zero or less is expired already,
        // and a huge count overflowed the date, which panicked the handler
        // and dropped the connection unanswered.
        let expiring = |days: i64| {
            app.api(
                Method::POST,
                "/v1/auth/tokens",
                Some(json!({ "name": "script", "expires_days": days })),
            )
        };
        for days in [0, -1, 3651, 100_000_000_000] {
            let (status, body) = expiring(days).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{days}: {body}");
            assert!(
                body["error"].as_str().unwrap().contains("expires_days"),
                "{body}"
            );
        }
        let (_, listed) = app.api(Method::GET, "/v1/auth/tokens", None).await;
        assert_eq!(listed, json!([]));
        let (status, created) = expiring(3650).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let expires_at: DateTime<Utc> = created["expires_at"].as_str().unwrap().parse().unwrap();
        assert!((expires_at - Utc::now() - TimeDelta::days(3650)).abs() < TimeDelta::minutes(1));

        let (status, created) = create(json!(["write", "read", "write"])).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["scopes"], json!(["read", "write"]));
        let token = created["token"].as_str().unwrap();

        // It reads and writes, and scrobbles nothing: no scope implies another.
        let (status, _) = app.api_as(token, Method::GET, "/v1/user/me", None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, profile) = app
            .api_as(
                token,
                Method::PATCH,
                "/v1/user/me",
                Some(json!({ "bio": "from a script" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(profile["bio"], "from a script");
        let (status, body) = app
            .api_as(
                token,
                Method::POST,
                "/v1/now-playing",
                Some(json!({ "track": "Teardrop", "artist": "Massive Attack" })),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "this API token lacks the `scrobble` scope");
    })
    .await;
}
