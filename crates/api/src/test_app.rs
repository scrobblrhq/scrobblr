//! The full router on a throwaway, migrated database, for end-to-end
//! tests. Those are `#[ignore]`d: they need Postgres and Redis (`just
//! test-db`).

use std::future::Future;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use fred::interfaces::ClientLike;
use futures_util::FutureExt;
use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, Executor, PgConnection, PgPool};
use tower::ServiceExt;
use uuid::Uuid;

use crate::compat::CompatConfig;
use crate::state::{AppState, UploadConfig};
use db::queries::auth as auth_db;

pub const PASSWORD: &str = "correct horse battery staple 42!";
pub const BASE_URL: &str = "https://scrobblr.test";

pub struct TestApp {
    pub router: Router,
    pub pool: PgPool,
    /// The client address requests come from, unless they name their own.
    pub ip: SocketAddr,
    pub username: String,
    pub session: Uuid,
}

/// Runs `test` against the full router on a throwaway, migrated database,
/// dropped afterwards even if the test panics.
pub async fn with_app<F, Fut>(config: CompatConfig, test: F)
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
    pub async fn send(&self, mut req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        if req.extensions().get::<ConnectInfo<SocketAddr>>().is_none() {
            req.extensions_mut().insert(ConnectInfo(self.ip));
        }
        let response = self.router.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    pub async fn get(&self, path_and_query: &str) -> (StatusCode, HeaderMap, String) {
        let req = Request::builder()
            .uri(path_and_query)
            .body(Body::empty())
            .unwrap();
        self.send(req).await
    }

    /// A native API call with the user's session.
    pub async fn api(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.api_as(&self.session.to_string(), method, path, body)
            .await
    }

    /// A native API call with `bearer`, a session or an API token.
    pub async fn api_as(
        &self,
        bearer: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let (status, _, body) = self.send(req).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }
}
