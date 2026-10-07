//! End-to-end tests of the scrobbler-compatible APIs through the router,
//! replaying the requests real clients send. `#[ignore]`d: they need
//! Postgres (a throwaway database per test) and Redis (`just test-db`).

use std::future::Future;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use chrono::Utc;
use fred::interfaces::ClientLike;
use futures_util::FutureExt;
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection, PgPool};
use tower::ServiceExt;
use uuid::Uuid;

use super::CompatConfig;
use crate::state::{AppState, UploadConfig};
use db::queries::auth as auth_db;

const PASSWORD: &str = "correct horse battery staple 42!";
const BASE_URL: &str = "https://scrobblr.test";

struct TestApp {
    router: Router,
    pool: PgPool,
    ip: SocketAddr,
    username: String,
    session: Uuid,
}

/// Runs `test` against the full router on a throwaway, migrated database,
/// dropped afterwards even if the test panics.
async fn with_app<F, Fut>(config: CompatConfig, test: F)
where
    F: FnOnce(TestApp) -> Fut,
    Fut: Future<Output = ()>,
{
    dotenvy::dotenv().ok();
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    // SAFETY: tests in this crate share a process; every one sets this same
    // value before anything reads it.
    unsafe {
        std::env::set_var(
            "TOKEN_ENCRYPTION_KEY",
            "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=",
        )
    };
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required for DB tests");
    let admin: PgConnectOptions = url.parse().expect("invalid DATABASE_URL");
    let name = format!("scrobblr_test_{}", Uuid::new_v4().simple());
    let mut conn = PgConnection::connect_with(&admin).await.unwrap();
    conn.execute(format!("CREATE DATABASE {name}").as_str())
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect_with(admin.clone().database(&name))
        .await
        .unwrap();
    db::migrate::run(&pool).await.unwrap();

    // Redis is shared between tests: keys carry user ids, usernames and
    // IPs, so make those unique.
    let first_id: i64 = rand::random::<u32>() as i64 + 1_000_000;
    pool.execute(format!("ALTER SEQUENCE users_id_seq RESTART WITH {first_id}").as_str())
        .await
        .unwrap();
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let redis = fred::types::Builder::from_config(
        fred::types::config::Config::from_url(&redis_url).unwrap(),
    )
    .build()
    .unwrap();
    redis.init().await.unwrap();

    let username = format!("user{}", &Uuid::new_v4().simple().to_string()[..8]);
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash) VALUES ($1, $1 || '@test', $2) RETURNING id",
    )
    .bind(&username)
    .bind(shared::user::hash_password(PASSWORD).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    let session = auth_db::create_session(&pool, user_id, None, None)
        .await
        .unwrap()
        .id;

    let state = AppState {
        db: pool.clone(),
        redis,
        uploads: Arc::new(UploadConfig {
            dir: std::env::temp_dir(),
            public_base_url: BASE_URL.into(),
        }),
        app_keys: None,
        trusted_proxy_hops: 0,
        clients: Default::default(),
        compat: Arc::new(config),
    };
    let octets = rand::random::<[u8; 3]>();
    let app = TestApp {
        router: crate::router::build(state),
        pool: pool.clone(),
        ip: SocketAddr::from(([10, octets[0], octets[1], octets[2]], 40_000)),
        username,
        session,
    };

    let result = AssertUnwindSafe(test(app)).catch_unwind().await;

    pool.close().await;
    conn.execute(format!("DROP DATABASE {name} WITH (FORCE)").as_str())
        .await
        .unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

impl TestApp {
    async fn send(&self, mut req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        req.extensions_mut().insert(ConnectInfo(self.ip));
        let response = self.router.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn get(&self, path_and_query: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        let req = Request::builder()
            .uri(path_and_query)
            .body(Body::empty())
            .unwrap();
        self.send(req).await
    }

    /// A native API call with the user's session.
    async fn api(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.session))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let (status, _, body) = self.send(req).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    async fn new_token(&self, name: &str) -> String {
        let (status, created) = self
            .api(
                Method::POST,
                "/v1/scrobbler/tokens",
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["credential"]["legacy_auth"], true);
        created["token"].as_str().unwrap().to_string()
    }

    async fn listen_brainz(&self, auth: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/1/submit-listens")
            .header(header::AUTHORIZATION, auth)
            .header(header::CONTENT_TYPE, "application/json; charset=UTF-8")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, _, body) = self.send(req).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    async fn scrobbles(&self) -> Vec<(String, String, Option<String>, Option<bool>)> {
        sqlx::query_as(
            r#"
            SELECT t.title, s.source, c.name, c.verified
            FROM scrobbles s
            JOIN tracks t ON t.id = s.track_id
            LEFT JOIN scrobble_clients c ON c.id = s.client_id
            ORDER BY s.played_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

/// Web Scrobbler's and Pano Scrobbler's ListenBrainz scrobblers, pointed
/// at this server.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn listenbrainz_clients_validate_and_submit() {
    with_app(CompatConfig::default(), |app| async move {
        let token = app.new_token("Web Scrobbler").await;
        for (auth, valid) in [
            (format!("Token {token}"), true),
            (format!("token {token}"), true),
            ("Token nope".to_string(), false),
        ] {
            let req = Request::builder()
                .uri("/1/validate-token")
                .header(header::AUTHORIZATION, auth)
                .body(Body::empty())
                .unwrap();
            let (status, _, body) = app.send(req).await;
            assert_eq!(status, StatusCode::OK);
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["valid"], valid);
        }
        let (_, _, body) = app.get(&format!("/1/validate-token?token={token}")).await;
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["user_name"], app.username.as_str());
        let (status, _, _) = app.get("/1/validate-token").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let web_scrobbler_meta = json!({
            "artist_name": "Cocteau Twins",
            "track_name": "Heaven or Las Vegas",
            "release_name": "Heaven or Las Vegas",
            "additional_info": {
                "submission_client": "Web Scrobbler",
                "submission_client_version": "3.14.0",
                "music_service_name": "YouTube",
                "origin_url": "https://www.youtube.com/watch?v=x",
                "duration": 296,
            },
        });
        let auth = format!("Token {token}");
        let (status, body) = app
            .listen_brainz(&auth, json!({ "listen_type": "playing_now", "payload": [{ "track_metadata": web_scrobbler_meta }] }))
            .await;
        assert_eq!((status, &body["status"]), (StatusCode::OK, &json!("ok")));
        let listened_at = Utc::now().timestamp() - 300;
        let (status, _) = app
            .listen_brainz(&auth, json!({ "listen_type": "single", "payload": [{ "listened_at": listened_at, "track_metadata": web_scrobbler_meta }] }))
            .await;
        assert_eq!(status, StatusCode::OK);

        // Pano's cached scrobbles as an import, one of them too old.
        let pano = |track: &str, at: i64| {
            json!({
                "listened_at": at,
                "track_metadata": {
                    "artist_name": "Cocteau Twins",
                    "release_name": null,
                    "track_name": track,
                    "additional_info": {
                        "duration_ms": 214000,
                        "submission_client": "Pano Scrobbler",
                        "submission_client_version": "3.12",
                    },
                },
            })
        };
        let import = json!({
            "listen_type": "import",
            "payload": [
                pano("Iceblink Luck", listened_at - 900),
                pano("Fifty-Fifty Clown", listened_at - 600),
                pano("Cherry-Coloured Funk", listened_at - 30 * 86_400),
            ],
        });
        for _ in 0..2 {
            let (status, body) = app.listen_brainz(&format!("token {token}"), import.clone()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let stored = app.scrobbles().await;
        assert_eq!(
            stored.iter().map(|s| (s.0.as_str(), s.2.as_deref())).collect::<Vec<_>>(),
            [
                ("Iceblink Luck", Some("Pano Scrobbler 3.12")),
                ("Fifty-Fifty Clown", Some("Pano Scrobbler 3.12")),
                ("Heaven or Las Vegas", Some("Web Scrobbler 3.14.0")),
            ]
        );
        let length: Option<i32> = sqlx::query_scalar(
            "SELECT s.duration_ms FROM scrobbles s JOIN tracks t ON t.id = s.track_id WHERE t.title = 'Heaven or Las Vegas'",
        )
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!(length, Some(296_000));

        let (status, _) = app.listen_brainz("Token nope", import.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let two_singles = json!({ "listen_type": "single", "payload": [pano("A", listened_at), pano("B", listened_at)] });
        let (status, _) = app.listen_brainz(&auth, two_singles).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn the_daily_limit_holds() {
    let config = CompatConfig { daily_limit: 3 };
    with_app(config, |app| async move {
        let token = app.new_token("scripts").await;
        let now = Utc::now().timestamp();
        let listens: Vec<Value> = (0..5)
            .map(|i| {
                json!({
                    "listened_at": now - 3600 + 300 * i,
                    "track_metadata": { "artist_name": "Artist", "track_name": format!("Song {i}") },
                })
            })
            .collect();
        let auth = format!("Token {token}");
        let (status, _) = app
            .listen_brainz(&auth, json!({ "listen_type": "import", "payload": listens }))
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(app.scrobbles().await.len(), 3);
    })
    .await;
}

/// Credential management is refused to anything but a session, and
/// scrobbler credentials never reach the native API.
#[tokio::test]
#[ignore = "needs Postgres and Redis: just test-db"]
async fn who_may_manage_credentials() {
    with_app(CompatConfig::default(), |app| async move {
        let raw = "f".repeat(64);
        sqlx::query("INSERT INTO api_tokens (user_id, name, token_hash, scopes) SELECT id, 'app', $1, '{scrobble}' FROM users")
            .bind(auth_db::hash_api_token(&raw))
            .execute(&app.pool)
            .await
            .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/scrobbler/tokens")
            .header(header::AUTHORIZATION, format!("Bearer {raw}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "name": "x" }).to_string()))
            .unwrap();
        let (status, _, _) = app.send(req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let token = app.new_token("player").await;
        let req = Request::builder()
            .uri("/v1/user/me")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = app.send(req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (_, listed) = app.api(Method::GET, "/v1/scrobbler/credentials", None).await;
        let id = listed[0]["id"].as_str().unwrap().to_string();
        let (status, _) = app
            .api(Method::DELETE, &format!("/v1/scrobbler/credentials/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let req = Request::builder()
            .uri("/1/validate-token")
            .header(header::AUTHORIZATION, format!("Token {token}"))
            .body(Body::empty())
            .unwrap();
        let (_, _, body) = app.send(req).await;
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["valid"], false);
    })
    .await;
}
