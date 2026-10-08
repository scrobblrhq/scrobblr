use axum::{
    extract::{ConnectInfo, Request, State},
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use fred::interfaces::KeysInterface;
use std::net::SocketAddr;

use crate::{errors::AppError, state::AppState};

/// Fixed-window rate limiter: [`MAX_REQUESTS`] per [`WINDOW_SECS`] per IP.
/// The window is part of the key, so a key whose expiry was never set
/// (the process died between the two commands) still stops counting when
/// the window ends. A Redis failure lets the request through.
const MAX_REQUESTS: i64 = 60;
const WINDOW_SECS: i64 = 60;

pub async fn rate_limit(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let ip = request_ip(&req, state.trusted_proxy_hops);
    let window = chrono::Utc::now().timestamp() / WINDOW_SECS;
    let key = format!("rl:{ip}:{window}");

    let count: i64 = match state.redis.incr(&key).await {
        Ok(count) => count,
        Err(e) => {
            tracing::warn!("rate limit unavailable: {e}");
            return Ok(next.run(req).await);
        }
    };
    if count == 1 {
        let _ = state
            .redis
            .expire::<i64, _>(&key, 2 * WINDOW_SECS, None)
            .await;
    }

    if count > MAX_REQUESTS {
        return Err(AppError::RateLimited);
    }

    Ok(next.run(req).await)
}

pub fn request_ip(req: &Request, trusted_proxy_hops: usize) -> String {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    client_ip(req.headers(), peer, trusted_proxy_hops)
}

/// The client's address: the peer's, or, behind `trusted_proxy_hops`
/// proxies, the address the outermost of them appended to
/// `X-Forwarded-For`. Entries to the left of it come from the client, which
/// can write anything there.
pub fn client_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted_proxy_hops: usize,
) -> String {
    if trusted_proxy_hops > 0
        && let Some(forwarded) = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|hop| !hop.is_empty())
            .collect::<Vec<_>>()
            .iter()
            .rev()
            .nth(trusted_proxy_hops - 1)
    {
        return forwarded.to_string();
    }
    peer.map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(xff: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in xff {
            h.append("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn forwarded_for_is_ignored_without_trusted_proxies() {
        let peer = Some("203.0.113.7:5000".parse().unwrap());
        assert_eq!(
            client_ip(&headers(&["198.51.100.1"]), peer, 0),
            "203.0.113.7"
        );
        assert_eq!(client_ip(&HeaderMap::new(), None, 0), "unknown");
    }

    #[test]
    fn the_hop_the_outermost_trusted_proxy_appended_wins() {
        let peer = Some("10.0.0.2:443".parse().unwrap());
        // The client sent a forged first entry; nginx appended the real one.
        let h = headers(&["1.2.3.4, 198.51.100.9"]);
        assert_eq!(client_ip(&h, peer, 1), "198.51.100.9");
        let h = headers(&["1.2.3.4", "198.51.100.9, 10.0.0.1"]);
        assert_eq!(client_ip(&h, peer, 2), "198.51.100.9");
        // Fewer hops than configured: the request bypassed a proxy.
        assert_eq!(client_ip(&headers(&[]), peer, 1), "10.0.0.2");
    }
}
