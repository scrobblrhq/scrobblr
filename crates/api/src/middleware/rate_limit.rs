use axum::{
    extract::{ConnectInfo, FromRequestParts, Request, State},
    http::{Extensions, HeaderMap, request::Parts},
    middleware::Next,
    response::Response,
};
use fred::interfaces::KeysInterface;
use ipnet::IpNet;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use crate::{errors::AppError, middleware::auth::AuthUser, state::AppState};

/// Loopback and private networks: a proxy on the same host or on a
/// container network connects from one of them.
const PRIVATE_NETWORKS: &str =
    "127.0.0.0/8, ::1/128, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, fc00::/7";

/// Which peers are reverse proxies that may name the client in
/// `X-Forwarded-For`, and how many of them a request passes through.
#[derive(Clone, Debug)]
pub struct TrustedProxies {
    /// `TRUSTED_PROXY_HOPS`; 0 ignores `X-Forwarded-For` altogether.
    pub hops: usize,
    /// `TRUSTED_PROXIES`: the peers believed, by default [`PRIVATE_NETWORKS`].
    pub networks: Vec<IpNet>,
}

impl Default for TrustedProxies {
    fn default() -> Self {
        Self::parse(None, None).expect("the defaults parse")
    }
}

impl TrustedProxies {
    pub fn from_env() -> anyhow::Result<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self::parse(
            var("TRUSTED_PROXY_HOPS").as_deref(),
            var("TRUSTED_PROXIES").as_deref(),
        )
    }

    /// Comma-separated addresses and networks, e.g. `10.0.0.0/8, 192.0.2.7`.
    pub fn parse(hops: Option<&str>, networks: Option<&str>) -> anyhow::Result<Self> {
        let hops = match hops {
            Some(v) => v
                .trim()
                .parse()
                .map_err(|e| anyhow::anyhow!("TRUSTED_PROXY_HOPS={v}: {e}"))?,
            None => 0,
        };
        let networks = networks
            .unwrap_or(PRIVATE_NETWORKS)
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                entry
                    .parse::<IpNet>()
                    .or_else(|_| entry.parse::<IpAddr>().map(IpNet::from))
                    .map(|net| net.trunc())
                    .map_err(|_| {
                        anyhow::anyhow!("TRUSTED_PROXIES: `{entry}` is not an address or a network")
                    })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { hops, networks })
    }

    fn trusts(&self, peer: IpAddr) -> bool {
        self.hops > 0 && self.networks.iter().any(|net| net.contains(&peer))
    }
}

/// Fixed-window rate limiter: [`MAX_REQUESTS`] per [`WINDOW_SECS`] per
/// [`network`].
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
    let network = network(&request_ip(&req, &state.proxies));
    let window = chrono::Utc::now().timestamp() / WINDOW_SECS;
    let key = format!("rl:{network}:{window}");

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

/// [`crate::limits::upload_attempt`] for the upload routes, behind
/// `require_auth`.
pub async fn upload_limit(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let user_id = req
        .extensions()
        .get::<AuthUser>()
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("upload limit before auth")))?
        .id;
    let ip = request_ip(&req, &state.proxies);
    if !crate::limits::upload_attempt(&state, user_id, &ip).await {
        return Err(AppError::RateLimited);
    }
    Ok(next.run(req).await)
}

pub fn request_ip(req: &Request, proxies: &TrustedProxies) -> String {
    client_ip_of(req.extensions(), req.headers(), proxies)
}

pub fn client_ip_of(
    extensions: &Extensions,
    headers: &HeaderMap,
    proxies: &TrustedProxies,
) -> String {
    let peer = extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    client_ip(headers, peer, proxies)
}

/// What per-address limits count `ip` as: itself, or for IPv6 its /64,
/// which a single client usually controls whole.
pub fn network(ip: &str) -> String {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let [a, b, c, d, ..] = v6.segments();
                format!("{a:x}:{b:x}:{c:x}:{d:x}::/64")
            }
        },
        _ => ip.to_string(),
    }
}

/// The client's address, as [`client_ip`] finds it, for handlers.
pub struct ClientIp(pub String);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Infallible> {
        Ok(Self(client_ip_of(
            &parts.extensions,
            &parts.headers,
            &state.proxies,
        )))
    }
}

impl aide::OperationInput for ClientIp {}

/// The client's address. A peer `proxies` trusts is a proxy, and the client
/// is the address the outermost of `proxies.hops` proxies appended to
/// `X-Forwarded-For`; entries to the left of it come from the client, which
/// can write anything there. Any other peer is the client, whatever it
/// claims.
pub fn client_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    proxies: &TrustedProxies,
) -> String {
    let Some(peer) = peer.map(|addr| addr.ip().to_canonical()) else {
        return "unknown".to_string();
    };
    if !proxies.trusts(peer) {
        return peer.to_string();
    }
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|hop| !hop.is_empty())
        .collect::<Vec<_>>()
        .iter()
        .rev()
        .nth(proxies.hops - 1)
        .and_then(|hop| {
            hop.parse::<IpAddr>()
                .or_else(|_| hop.parse::<SocketAddr>().map(|addr| addr.ip()))
                .ok()
        })
        // Fewer entries than hops (the request went around a proxy), or
        // no address in the entry.
        .map_or(peer, |ip| ip.to_canonical())
        .to_string()
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

    fn hops(n: usize) -> TrustedProxies {
        TrustedProxies {
            hops: n,
            ..Default::default()
        }
    }

    #[test]
    fn forwarded_for_is_ignored_without_trusted_proxies() {
        let peer = Some("10.0.0.2:5000".parse().unwrap());
        assert_eq!(
            client_ip(&headers(&["198.51.100.1"]), peer, &hops(0)),
            "10.0.0.2"
        );
        assert_eq!(client_ip(&HeaderMap::new(), None, &hops(1)), "unknown");
    }

    #[test]
    fn a_peer_outside_the_trusted_networks_cant_name_another_client() {
        // A client reaching the API directly, around the proxies.
        for peer in ["203.0.113.7:5000", "[2001:db8::7]:5000"] {
            let peer: SocketAddr = peer.parse().unwrap();
            for forged in [&["198.51.100.1"][..], &["1.2.3.4", "198.51.100.1"]] {
                let ip = client_ip(&headers(forged), Some(peer), &hops(1));
                assert_eq!(ip, peer.ip().to_string());
                assert_eq!(client_ip(&headers(forged), Some(peer), &hops(2)), ip);
            }
        }
        // Only the proxies listed are believed.
        let proxies = TrustedProxies::parse(Some("1"), Some("192.0.2.10")).unwrap();
        let xff = headers(&["198.51.100.1"]);
        let from = |peer: &str| client_ip(&xff, Some(peer.parse().unwrap()), &proxies);
        assert_eq!(from("192.0.2.10:443"), "198.51.100.1");
        assert_eq!(from("10.0.0.2:443"), "10.0.0.2");
    }

    #[test]
    fn proxies_are_trusted_on_private_networks_by_default() {
        let xff = headers(&["198.51.100.1"]);
        for peer in [
            "127.0.0.1:1",
            "[::1]:1",
            "10.1.2.3:1",
            "172.18.0.3:1",
            "192.168.1.1:1",
            "[fd00::3]:1",
            // How a dual-stack listener sees an IPv4 peer.
            "[::ffff:172.18.0.3]:1",
        ] {
            let ip = client_ip(&xff, Some(peer.parse().unwrap()), &hops(1));
            assert_eq!(ip, "198.51.100.1", "{peer}");
        }
        for peer in ["172.32.0.1:1", "100.64.0.1:1", "[fe80::1]:1", "8.8.8.8:1"] {
            let ip = client_ip(&xff, Some(peer.parse().unwrap()), &hops(1));
            assert_ne!(ip, "198.51.100.1", "{peer}");
        }
    }

    #[test]
    fn a_forwarded_entry_must_be_an_address() {
        let peer = Some("10.0.0.2:443".parse().unwrap());
        let ip = |xff: &str| client_ip(&headers(&[xff]), peer, &hops(1));
        assert_eq!(ip("198.51.100.9:4321"), "198.51.100.9");
        assert_eq!(ip("[2001:db8::9]:443"), "2001:db8::9");
        assert_eq!(ip("::ffff:198.51.100.9"), "198.51.100.9");
        assert_eq!(ip("unknown"), "10.0.0.2");
        assert_eq!(ip("rl:1:2"), "10.0.0.2");
    }

    #[test]
    fn trusted_proxies_are_parsed_from_settings() {
        let defaults = TrustedProxies::parse(None, None).unwrap();
        assert_eq!(defaults.hops, 0);
        assert_eq!(defaults.networks.len(), 6);
        let set = TrustedProxies::parse(Some(" 2 "), Some("10.1.2.3/8, 2001:db8::1,,")).unwrap();
        assert_eq!(set.hops, 2);
        assert_eq!(
            set.networks,
            [
                "10.0.0.0/8".parse().unwrap(),
                "2001:db8::1/128".parse().unwrap()
            ]
        );
        assert!(TrustedProxies::parse(Some("-1"), None).is_err());
        assert!(TrustedProxies::parse(None, Some("10.0.0.0/8, private")).is_err());
        assert!(TrustedProxies::parse(None, Some("10.0.0.0/33")).is_err());
    }

    #[test]
    fn ipv6_clients_are_counted_by_their_64() {
        assert_eq!(network("203.0.113.7"), "203.0.113.7");
        assert_eq!(network("::ffff:203.0.113.7"), "203.0.113.7");
        assert_eq!(
            network("2001:db8:1:2:aaaa::1"),
            network("2001:db8:1:2:bbbb:cccc:dddd:eeee")
        );
        assert_eq!(network("2001:db8:1:2::1"), "2001:db8:1:2::/64");
        assert_ne!(network("2001:db8:1:2::1"), network("2001:db8:1:3::1"));
        assert_eq!(network("unknown"), "unknown");
    }

    #[test]
    fn the_hop_the_outermost_trusted_proxy_appended_wins() {
        let peer = Some("10.0.0.2:443".parse().unwrap());
        // The client sent a forged first entry; nginx appended the real one.
        let h = headers(&["1.2.3.4, 198.51.100.9"]);
        assert_eq!(client_ip(&h, peer, &hops(1)), "198.51.100.9");
        let h = headers(&["1.2.3.4", "198.51.100.9, 10.0.0.1"]);
        assert_eq!(client_ip(&h, peer, &hops(2)), "198.51.100.9");
        // Fewer hops than configured: the request bypassed a proxy.
        assert_eq!(client_ip(&headers(&[]), peer, &hops(1)), "10.0.0.2");
    }

    /// The whole router, as a client reaching the API around its proxy: a
    /// different forged address on every request doesn't escape the limit.
    #[tokio::test]
    #[ignore = "needs Postgres and Redis: just test-db"]
    async fn forged_forwarded_for_from_an_untrusted_peer_is_ignored() {
        use axum::body::Body;
        use axum::http::StatusCode;

        crate::test_app::with_custom_app(
            |state| state.proxies = std::sync::Arc::new(hops(1)),
            |app| async move {
                // Each its own /64, the network limits count.
                let client = |prefix: u16, i: u16| format!("2001:db8:{prefix:x}:{i:x}::1");
                let (a, b) = rand::random::<(u16, u16)>();
                let attacker: SocketAddr = format!("[{}]:5000", client(a, 0xffff)).parse().unwrap();
                let proxy = SocketAddr::from(([10, (b >> 8) as u8, b as u8, 1], 5000));
                let get = |peer: SocketAddr, forwarded: String| {
                    let mut req = Request::get("/health")
                        .header("x-forwarded-for", forwarded)
                        .body(Body::empty())
                        .unwrap();
                    req.extensions_mut().insert(ConnectInfo(peer));
                    app.send(req)
                };
                // All of it in one window.
                let second = chrono::Utc::now().timestamp() % WINDOW_SECS;
                if second > WINDOW_SECS - 10 {
                    let wait = (WINDOW_SECS - second) as u64;
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                }

                for i in 0..MAX_REQUESTS as u16 {
                    assert_eq!(get(attacker, client(a, i)).await.0, StatusCode::OK);
                }
                let (status, _, _) = get(attacker, client(a, 1000)).await;
                assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

                // From the proxy, each address it names is a client.
                for i in 0..=MAX_REQUESTS as u16 {
                    assert_eq!(get(proxy, client(b, i)).await.0, StatusCode::OK);
                }
            },
        )
        .await;
    }
}
