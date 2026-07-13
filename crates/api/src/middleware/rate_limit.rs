use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use fred::interfaces::KeysInterface;
use std::net::SocketAddr;

use crate::{errors::AppError, state::AppState};

/// Sliding-window rate limiter: [`MAX_REQUESTS`] per [`WINDOW_SECS`] per IP.
const MAX_REQUESTS: i64 = 60;
const WINDOW_SECS: i64 = 60;

pub async fn rate_limit(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let ip = extract_ip(&req);
    let key = format!("rl:{ip}");

    let count: i64 = state.redis.incr(&key).await.unwrap_or(1);

    if count == 1 {
        let _ = state.redis.expire::<i64, _>(&key, WINDOW_SECS, None).await;
    }

    if count > MAX_REQUESTS {
        return Err(AppError::RateLimited);
    }

    Ok(next.run(req).await)
}

/// Extracts the client IP from `X-Forwarded-For` (first hop) when behind a
/// reverse proxy, falling back to the raw connection address otherwise.
fn extract_ip(req: &Request) -> String {
    if let Some(forwarded) = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
    {
        return forwarded.trim().to_string();
    }

    req.extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
