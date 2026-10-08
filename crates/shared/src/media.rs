//! Uploaded images: the keys they are stored under and the URLs clients get.
//!
//! The database stores an upload as its key (`avatars/3f/3f…e1.jpg`), never
//! as a URL, so serving the files from another host or a bucket is a
//! configuration change. Image columns also hold absolute URLs, from
//! enrichment providers or set by users; those are served as they are.
//! Models serialize a key as `{UPLOAD_PUBLIC_URL}/{key}`.

use std::borrow::Cow;
use std::sync::OnceLock;

use serde::Serializer;
use thiserror::Error;
use uuid::Uuid;

/// Where the API serves the upload directory when nothing else is set.
const DEFAULT_PUBLIC_URL: &str = "http://localhost:8080/uploads";
const MAX_KEY_LEN: usize = 128;
const EXTENSIONS: &[&str] = &["jpg"];

static PUBLIC_URL: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadKind {
    Avatar,
    ArtistImage,
    AlbumImage,
}

impl UploadKind {
    const ALL: [Self; 3] = [Self::Avatar, Self::ArtistImage, Self::AlbumImage];

    fn dir(self) -> &'static str {
        match self {
            Self::Avatar => "avatars",
            Self::ArtistImage => "artists",
            Self::AlbumImage => "albums",
        }
    }
}

/// An upload's path relative to the storage root. Keys are random and never
/// reused, so a stored file never changes. [`UploadKey::parse`] accepts
/// nothing that could leave the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadKey(String);

impl UploadKey {
    /// `{kind}/{2 hex}/{32 hex}.jpg`: the shard directory keeps any one
    /// directory small.
    pub fn generate(kind: UploadKind) -> Self {
        let id = Uuid::new_v4().simple().to_string();
        Self(format!("{}/{}/{id}.jpg", kind.dir(), &id[..2]))
    }

    pub fn parse(value: &str) -> Option<Self> {
        let extension = value.rsplit_once('.')?.1;
        let valid = value.len() <= MAX_KEY_LEN
            && EXTENSIONS.contains(&extension)
            && value.split('/').all(|segment| {
                !segment.is_empty()
                    && !segment.starts_with('.')
                    && segment.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'
                    })
            });
        valid.then(|| Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `None` for the `{uuid}.jpg` keys of uploads made before kinds existed.
    pub fn kind(&self) -> Option<UploadKind> {
        let mut segments = self.0.split('/');
        let dir = segments.next()?;
        if segments.count() != 2 {
            return None;
        }
        UploadKind::ALL.into_iter().find(|kind| kind.dir() == dir)
    }
}

impl std::fmt::Display for UploadKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Error)]
#[error("{var} must be an http(s) URL without a query or fragment, got {value:?}")]
pub struct InvalidPublicUrl {
    var: &'static str,
    value: String,
}

/// The base URL uploads are served under: `upload_public_url`
/// (`UPLOAD_PUBLIC_URL`), else the API's own `/uploads` under
/// `public_base_url` (`PUBLIC_BASE_URL`). Blank values count as unset.
pub fn public_url_from(
    upload_public_url: Option<&str>,
    public_base_url: Option<&str>,
) -> Result<String, InvalidPublicUrl> {
    fn set(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|v| !v.is_empty())
    }
    let (var, url) = match (set(upload_public_url), set(public_base_url)) {
        (Some(url), _) => ("UPLOAD_PUBLIC_URL", url.trim_end_matches('/').to_owned()),
        (None, Some(base)) => (
            "PUBLIC_BASE_URL",
            format!("{}/uploads", base.trim_end_matches('/')),
        ),
        (None, None) => return Ok(DEFAULT_PUBLIC_URL.to_owned()),
    };
    match reqwest::Url::parse(&url) {
        Ok(parsed)
            if matches!(parsed.scheme(), "http" | "https")
                && parsed.has_host()
                && parsed.query().is_none()
                && parsed.fragment().is_none() =>
        {
            Ok(url)
        }
        _ => Err(InvalidPublicUrl { var, value: url }),
    }
}

/// [`public_url_from`] the environment.
pub fn public_url_from_env() -> Result<String, InvalidPublicUrl> {
    public_url_from(
        std::env::var("UPLOAD_PUBLIC_URL").ok().as_deref(),
        std::env::var("PUBLIC_BASE_URL").ok().as_deref(),
    )
}

/// Sets the base URL keys are served under, once per process: later calls
/// are ignored.
pub fn set_public_url(url: String) {
    let _ = PUBLIC_URL.set(url);
}

pub fn public_base() -> &'static str {
    PUBLIC_URL.get().map_or(DEFAULT_PUBLIC_URL, String::as_str)
}

/// The URL a stored image value is served at: keys get the public base,
/// anything else (an absolute URL) is returned unchanged.
pub fn public_url(stored: &str) -> Cow<'_, str> {
    resolve(public_base(), stored)
}

fn resolve<'a>(base: &str, stored: &'a str) -> Cow<'a, str> {
    match UploadKey::parse(stored) {
        Some(_) => Cow::Owned(format!("{base}/{stored}")),
        None => Cow::Borrowed(stored),
    }
}

/// `serialize_with` for an image column, so no response carries a bare key.
pub fn serialize_url<S: Serializer>(value: &str, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&public_url(value))
}

/// [`serialize_url`] for a nullable column.
pub fn serialize_opt_url<S: Serializer>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => serializer.serialize_some(&*public_url(value)),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_parse_back_with_their_kind() {
        for kind in UploadKind::ALL {
            let key = UploadKey::generate(kind);
            assert_eq!(UploadKey::parse(key.as_str()), Some(key.clone()));
            assert_eq!(key.kind(), Some(kind));
            let parts: Vec<_> = key.as_str().split('/').collect();
            assert_eq!(parts.len(), 3);
            assert_eq!(parts[2].len(), 36);
            assert!(parts[2].starts_with(parts[1]));
        }
        assert_ne!(
            UploadKey::generate(UploadKind::Avatar),
            UploadKey::generate(UploadKind::Avatar)
        );
    }

    #[test]
    fn keys_from_before_kinds_parse_without_one() {
        let key = UploadKey::parse("0b4e7a0e-5c2f-4a8d-9f3e-2d1c0b9a8f7e.jpg").unwrap();
        assert_eq!(key.kind(), None);
        assert_eq!(UploadKey::parse("other/ab/abcd.jpg").unwrap().kind(), None);
    }

    #[test]
    fn parse_refuses_anything_that_could_leave_the_root() {
        for value in [
            "",
            ".jpg",
            "/avatars/ab/abcd.jpg",
            "../abcd.jpg",
            "avatars/../../etc/passwd.jpg",
            "avatars/..",
            "avatars/.tmp/abcd.jpg",
            "avatars//abcd.jpg",
            "avatars/ab/abcd.jpg/",
            "avatars\\ab\\abcd.jpg",
            "avatars/ab/ABCD.jpg",
            "avatars/ab/abcd.jpg\0",
            "avatars/ab/abcd.png",
            "avatars/ab/abcd",
            "https://cdn.example.com/avatars/ab/abcd.jpg",
            "c:/abcd.jpg",
            "avatars/ab/ab cd.jpg",
        ] {
            assert_eq!(UploadKey::parse(value), None, "{value:?}");
        }
        assert_eq!(UploadKey::parse(&format!("{}.jpg", "a".repeat(200))), None);
    }

    #[test]
    fn keys_resolve_against_the_base_and_urls_stay_as_they_are() {
        let base = "https://cdn.example.com";
        assert_eq!(
            resolve(base, "avatars/ab/abcd.jpg"),
            "https://cdn.example.com/avatars/ab/abcd.jpg"
        );
        for absolute in [
            "https://e-cdns-images.dzcdn.net/images/artist/x/500x500.jpg",
            "http://coverartarchive.org/release/x/front-500",
            "",
        ] {
            assert!(matches!(resolve(base, absolute), Cow::Borrowed(v) if v == absolute));
        }
    }

    #[test]
    fn the_public_url_defaults_to_the_apis_own_uploads_route() {
        assert_eq!(public_url_from(None, None).unwrap(), DEFAULT_PUBLIC_URL);
        assert_eq!(
            public_url_from(Some("  "), Some("")).unwrap(),
            DEFAULT_PUBLIC_URL
        );
        assert_eq!(
            public_url_from(None, Some("https://api.example.com/")).unwrap(),
            "https://api.example.com/uploads"
        );
        assert_eq!(
            public_url_from(
                Some("https://cdn.example.com/"),
                Some("https://api.example.com")
            )
            .unwrap(),
            "https://cdn.example.com"
        );
        assert_eq!(
            public_url_from(Some("https://example.com/media"), None).unwrap(),
            "https://example.com/media"
        );
        for invalid in [
            "cdn.example.com",
            "ftp://cdn.example.com",
            "https://cdn.example.com/?v=1",
            "https://cdn.example.com/#x",
            "https://",
        ] {
            assert!(public_url_from(Some(invalid), None).is_err(), "{invalid}");
        }
        assert!(
            public_url_from(None, Some("localhost:8080"))
                .unwrap_err()
                .to_string()
                .starts_with("PUBLIC_BASE_URL")
        );
    }

    #[test]
    fn serializing_resolves_keys() {
        #[derive(serde::Serialize)]
        struct Row {
            #[serde(serialize_with = "serialize_opt_url")]
            image: Option<String>,
            #[serde(serialize_with = "serialize_url")]
            url: String,
        }
        let row = Row {
            image: Some("avatars/ab/abcd.jpg".into()),
            url: "https://example.com/a.jpg".into(),
        };
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            serde_json::json!({
                "image": format!("{}/avatars/ab/abcd.jpg", public_base()),
                "url": "https://example.com/a.jpg",
            })
        );
        let empty = Row {
            image: None,
            url: String::new(),
        };
        assert_eq!(
            serde_json::to_value(&empty).unwrap()["image"],
            serde_json::Value::Null
        );
    }
}
