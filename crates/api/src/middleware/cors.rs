//! Which web pages may call the API from a browser. The web app calls the
//! native API from its server, and the browser extension holds host
//! permissions, so by default no page may; the scrobbler protocol routes
//! answer any page, as Last.fm's and ListenBrainz's own APIs do.

use std::time::Duration;

use axum::http::{HeaderValue, Method};
use tower_http::cors::{AllowHeaders, AllowOrigin, Any, CorsLayer};

/// How long a browser may reuse a preflight's answer.
const MAX_AGE: Duration = Duration::from_secs(3600);

/// The origins allowed to call the native API (`CORS_ALLOWED_ORIGINS`).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum CorsOrigins {
    #[default]
    None,
    /// `*`.
    Any,
    /// Comma-separated origins, e.g. `https://scrobblr.app`.
    List(Vec<HeaderValue>),
}

impl CorsOrigins {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::parse(std::env::var("CORS_ALLOWED_ORIGINS").ok().as_deref())
    }

    pub fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        let entries: Vec<&str> = value
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .collect();
        match entries[..] {
            [] => Ok(Self::None),
            ["*"] => Ok(Self::Any),
            _ => entries
                .into_iter()
                .map(origin)
                .collect::<anyhow::Result<_>>()
                .map(Self::List),
        }
    }

    /// The native API's layer; `None` sends no CORS headers at all.
    pub fn layer(&self) -> Option<CorsLayer> {
        let allow_origin = match self {
            Self::None => return None,
            Self::Any => AllowOrigin::from(Any),
            Self::List(origins) => AllowOrigin::list(origins.iter().cloned()),
        };
        Some(
            CorsLayer::new()
                .allow_origin(allow_origin)
                .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
                // `*` would not cover Authorization.
                .allow_headers(AllowHeaders::mirror_request())
                .max_age(MAX_AGE),
        )
    }
}

/// The scrobbler protocol routes': any page, no cookies (clients send their
/// credentials themselves).
pub fn protocol_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers(AllowHeaders::mirror_request())
        .max_age(MAX_AGE)
}

/// `value` as browsers send it in `Origin`: scheme, host and a port other
/// than the scheme's default, nothing else.
fn origin(value: &str) -> anyhow::Result<HeaderValue> {
    let invalid = || {
        anyhow::anyhow!(
            "CORS_ALLOWED_ORIGINS: `{value}` is not an origin such as https://scrobblr.app"
        )
    };
    let url = reqwest::Url::parse(value).map_err(|_| invalid())?;
    let bare = matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    let host = url.host_str().filter(|_| bare).ok_or_else(invalid)?;
    let origin = match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    };
    HeaderValue::try_from(origin).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(origins: &[&str]) -> CorsOrigins {
        CorsOrigins::List(
            origins
                .iter()
                .map(|o| HeaderValue::from_str(o).unwrap())
                .collect(),
        )
    }

    #[test]
    fn origins_are_read_as_browsers_send_them() {
        assert_eq!(CorsOrigins::parse(None).unwrap(), CorsOrigins::None);
        assert_eq!(CorsOrigins::parse(Some(" , ")).unwrap(), CorsOrigins::None);
        assert_eq!(CorsOrigins::parse(Some(" * ")).unwrap(), CorsOrigins::Any);
        assert_eq!(
            CorsOrigins::parse(Some(
                "https://Scrobblr.app/, http://localhost:5173, https://web.test:443, chrome-extension://abcdef"
            ))
            .unwrap(),
            list(&[
                "https://scrobblr.app",
                "http://localhost:5173",
                "https://web.test",
                "chrome-extension://abcdef",
            ])
        );
        for invalid in [
            "scrobblr.app",
            "https://scrobblr.app/settings",
            "https://scrobblr.app?x=1",
            "https://user@scrobblr.app",
            "*, https://scrobblr.app",
            "mailto:me@scrobblr.app",
        ] {
            assert!(CorsOrigins::parse(Some(invalid)).is_err(), "{invalid}");
        }
    }

    #[test]
    fn closed_means_no_layer() {
        assert!(CorsOrigins::None.layer().is_none());
        assert!(CorsOrigins::Any.layer().is_some());
        assert!(list(&["https://scrobblr.app"]).layer().is_some());
    }
}
