//! Operational routes, outside the OpenAPI spec: `/health` (the API's
//! readiness), `/health/worker` (whether the worker keeps up, without
//! details) and `/metrics` (Prometheus text, only with `METRICS_TOKEN`).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use fred::interfaces::ClientLike;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::state::AppState;
use db::queries::monitoring as mdb;
use shared::monitoring::{self, LoopState};

const TIMEOUT: Duration = Duration::from_secs(2);

/// Settings and the API's own counters.
pub struct Monitoring {
    /// `WORKER_STALL_FACTOR`.
    pub stall_factor: f64,
    /// `METRICS_TOKEN`; `/metrics` answers 404 without one.
    pub metrics_token: Option<String>,
    responses: [AtomicU64; 5],
    rate_limited: AtomicU64,
}

impl Default for Monitoring {
    fn default() -> Self {
        Self::new(monitoring::DEFAULT_STALL_FACTOR, None)
    }
}

impl Monitoring {
    pub const MIN_TOKEN_LEN: usize = 16;

    pub fn new(stall_factor: f64, metrics_token: Option<String>) -> Self {
        Self {
            stall_factor,
            metrics_token,
            responses: Default::default(),
            rate_limited: AtomicU64::new(0),
        }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let metrics_token = std::env::var("METRICS_TOKEN")
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        if metrics_token
            .as_ref()
            .is_some_and(|t| t.len() < Self::MIN_TOKEN_LEN)
        {
            anyhow::bail!(
                "METRICS_TOKEN must be at least {} characters",
                Self::MIN_TOKEN_LEN
            );
        }
        Ok(Self::new(
            monitoring::stall_factor_from_env().map_err(anyhow::Error::msg)?,
            metrics_token,
        ))
    }

    fn count(&self, status: StatusCode) {
        if let Some(class) = (status.as_u16() / 100).checked_sub(1)
            && let Some(counter) = self.responses.get(usize::from(class))
        {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            self.rate_limited.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Counts every response by status class, for `/metrics`.
pub async fn count_responses(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let response = next.run(req).await;
    state.monitoring.count(response.status());
    response
}

/// Readiness for uptime checks and the container's healthcheck: 200 `ok`
/// when the database and Redis answer within 2 s, otherwise 503 naming
/// which doesn't (the error itself goes to the log only).
pub async fn health(State(state): State<AppState>) -> Response {
    let (database, redis) = dependencies(&state).await;
    let down: Vec<&str> = [(database, "database"), (redis, "redis")]
        .into_iter()
        .filter_map(|(up, name)| (!up).then_some(name))
        .collect();
    if down.is_empty() {
        "ok".into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("unavailable: {}", down.join(", ")),
        )
            .into_response()
    }
}

/// 200 `ok` while every worker loop keeps up, 503 otherwise; which one
/// doesn't is for `worker status` or `/metrics` to say.
pub async fn health_worker(State(state): State<AppState>) -> Response {
    match tokio::time::timeout(TIMEOUT, mdb::heartbeats(&state.db)).await {
        Ok(Ok(beats)) if monitoring::healthy(&beats, Utc::now(), state.monitoring.stall_factor) => {
            "ok".into_response()
        }
        Ok(Ok(_)) => (StatusCode::SERVICE_UNAVAILABLE, "unhealthy").into_response(),
        Ok(Err(e)) => {
            tracing::warn!("health: reading worker heartbeats: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response()
        }
        Err(_) => {
            tracing::warn!("health: worker heartbeats didn't come within {TIMEOUT:?}");
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response()
        }
    }
}

async fn dependencies(state: &AppState) -> (bool, bool) {
    let database = async {
        match tokio::time::timeout(TIMEOUT, sqlx::query("SELECT 1").execute(&state.db)).await {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                tracing::warn!("health: database: {e}");
                false
            }
            Err(_) => {
                tracing::warn!("health: database didn't answer within {TIMEOUT:?}");
                false
            }
        }
    };
    let redis = async {
        match tokio::time::timeout(TIMEOUT, state.redis.ping::<()>(None)).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!("health: redis: {e}");
                false
            }
            Err(_) => {
                tracing::warn!("health: redis didn't answer within {TIMEOUT:?}");
                false
            }
        }
    };
    tokio::join!(database, redis)
}

/// Prometheus text format: dependencies, worker loops and queues (read
/// from the database at each scrape) and this process's response counts.
pub async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(token) = &state.monitoring.metrics_token else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let matches = Sha256::digest(given.trim().as_bytes()).ct_eq(&Sha256::digest(token.as_bytes()));
    if !bool::from(matches) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response();
    }

    let mut out = String::new();
    let (database, redis) = dependencies(&state).await;
    metric(
        &mut out,
        "scrobblr_up",
        "gauge",
        "Whether a dependency answers (1) or not (0).",
    );
    let _ = writeln!(
        out,
        "scrobblr_up{{dependency=\"database\"}} {}",
        u8::from(database)
    );
    let _ = writeln!(
        out,
        "scrobblr_up{{dependency=\"redis\"}} {}",
        u8::from(redis)
    );

    let m = &state.monitoring;
    metric(
        &mut out,
        "scrobblr_http_responses_total",
        "counter",
        "Responses this API process sent, by status class.",
    );
    for (i, counter) in m.responses.iter().enumerate() {
        let _ = writeln!(
            out,
            "scrobblr_http_responses_total{{class=\"{}xx\"}} {}",
            i + 1,
            counter.load(Ordering::Relaxed)
        );
    }
    metric(
        &mut out,
        "scrobblr_http_rate_limited_total",
        "counter",
        "Responses this API process sent with 429 Too Many Requests.",
    );
    let _ = writeln!(
        out,
        "scrobblr_http_rate_limited_total {}",
        m.rate_limited.load(Ordering::Relaxed)
    );

    if database {
        let read =
            async { tokio::try_join!(mdb::heartbeats(&state.db), mdb::queue_depths(&state.db)) };
        match tokio::time::timeout(TIMEOUT, read).await {
            Ok(Ok((beats, queues))) => worker_metrics(&mut out, &beats, &queues, m.stall_factor),
            Ok(Err(e)) => tracing::warn!("metrics: reading the worker's state: {e}"),
            Err(_) => tracing::warn!("metrics: the worker's state didn't come within {TIMEOUT:?}"),
        }
    }

    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}

fn worker_metrics(
    out: &mut String,
    beats: &[shared::monitoring::Heartbeat],
    queues: &[mdb::QueueDepth],
    factor: f64,
) {
    let now = Utc::now();
    metric(
        out,
        "scrobblr_worker_healthy",
        "gauge",
        "1 while every worker loop keeps up (as /health/worker).",
    );
    let _ = writeln!(
        out,
        "scrobblr_worker_healthy {}",
        u8::from(monitoring::healthy(beats, now, factor))
    );
    type Value = fn(&shared::monitoring::Heartbeat, chrono::DateTime<Utc>, f64) -> f64;
    let per_loop: [(&str, &str, Value); 4] = [
        (
            "scrobblr_worker_loop_last_run_age_seconds",
            "Seconds since the loop last finished a run.",
            |b, now, _| b.run_age_secs(now),
        ),
        (
            "scrobblr_worker_loop_last_success_age_seconds",
            "Seconds since the loop last ran without an error.",
            |b, now, _| b.ok_age_secs(now),
        ),
        (
            "scrobblr_worker_loop_deadline_seconds",
            "Age past which the loop counts as stalled or failing.",
            |b, _, factor| f64::from(b.interval_secs) * factor,
        ),
        (
            "scrobblr_worker_loop_healthy",
            "1 while the loop keeps up, 0 when stalled or failing.",
            |b, now, factor| f64::from(u8::from(b.state(now, factor) == LoopState::Ok)),
        ),
    ];
    for (name, help, value) in per_loop {
        metric(out, name, "gauge", help);
        for b in beats {
            let _ = writeln!(
                out,
                "{name}{{loop=\"{}\"}} {}",
                label(&b.loop_name),
                value(b, now, factor)
            );
        }
    }
    metric(
        out,
        "scrobblr_queue_due",
        "gauge",
        "Items due now in a worker queue.",
    );
    for q in queues {
        let _ = writeln!(
            out,
            "scrobblr_queue_due{{queue=\"{}\"}} {}",
            label(&q.queue),
            q.due
        );
    }
    metric(
        out,
        "scrobblr_queue_oldest_due_age_seconds",
        "gauge",
        "How long the oldest due item has waited (0 when none).",
    );
    for q in queues {
        let _ = writeln!(
            out,
            "scrobblr_queue_oldest_due_age_seconds{{queue=\"{}\"}} {}",
            label(&q.queue),
            q.oldest_due_secs.unwrap_or(0.0)
        );
    }
}

fn metric(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
