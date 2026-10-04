use std::collections::HashMap;

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use fred::interfaces::KeysInterface;
use fred::types::{Expiration, SetOptions};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{errors::AppError, state::AppState};

const APP_ID_HEADER: &str = "x-app-id";
const TIMESTAMP_HEADER: &str = "x-app-timestamp";
const NONCE_HEADER: &str = "x-app-nonce";
const SIGNATURE_HEADER: &str = "x-app-signature";

const MAX_SKEW_SECS: i64 = 300;
const MAX_BODY_BYTES: usize = 64 * 1024;
const NONCE_MIN_LEN: usize = 16;
const NONCE_MAX_LEN: usize = 128;

type HmacSha256 = Hmac<Sha256>;

/// Registered first-party application secrets, parsed from `AUTH_APP_KEYS`
/// as `app_id:base64secret` pairs separated by commas.
#[derive(Debug, Clone, Default)]
pub struct AppKeys(HashMap<String, Vec<u8>>);

impl AppKeys {
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Ok(raw) = std::env::var("AUTH_APP_KEYS") else {
            return Ok(None);
        };
        if raw.trim().is_empty() {
            return Ok(None);
        }

        let mut keys = HashMap::new();
        for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (app_id, secret) = entry.split_once(':').ok_or_else(|| {
                anyhow::anyhow!("AUTH_APP_KEYS entries must be formatted as app_id:base64secret")
            })?;
            let secret = BASE64
                .decode(secret.trim())
                .map_err(|_| anyhow::anyhow!("AUTH_APP_KEYS secret for {app_id} is not base64"))?;
            if secret.len() < 32 {
                anyhow::bail!("AUTH_APP_KEYS secret for {app_id} must be at least 32 bytes");
            }
            keys.insert(app_id.trim().to_string(), secret);
        }

        if keys.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self(keys)))
    }

    fn secret(&self, app_id: &str) -> Option<&[u8]> {
        self.0.get(app_id).map(Vec::as_slice)
    }

    pub fn app_ids(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}

/// Rejects requests that don't carry a valid first-party app signature.
///
/// Only attached when `AUTH_APP_KEYS` is configured. A signature proves the
/// caller holds a secret shipped in an official client — it raises the cost
/// of scripted abuse but is not a trust boundary, since any secret embedded
/// in a distributed binary is ultimately extractable. Rate limiting stays
/// the real defence.
pub async fn require_app_signature(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let Some(keys) = state.app_keys.as_ref() else {
        return Ok(next.run(req).await);
    };

    let app_id = header(&req, APP_ID_HEADER)?;
    let timestamp: i64 = header(&req, TIMESTAMP_HEADER)?
        .parse()
        .map_err(|_| reject("invalid app timestamp"))?;
    let nonce = header(&req, NONCE_HEADER)?;
    let signature = header(&req, SIGNATURE_HEADER)?;

    if !(NONCE_MIN_LEN..=NONCE_MAX_LEN).contains(&nonce.len())
        || !nonce.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Err(reject("invalid app nonce"));
    }

    let secret = keys.secret(&app_id).ok_or_else(|| reject("unknown app"))?;

    let skew = (chrono::Utc::now().timestamp() - timestamp).abs();
    if skew > MAX_SKEW_SECS {
        return Err(reject("app signature timestamp outside accepted window"));
    }

    let target = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let method = req.method().as_str().to_string();

    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| reject("request body too large to verify"))?;

    let expected = sign(secret, &method, &target, timestamp, &nonce, &bytes);
    let provided = hex_decode(&signature).ok_or_else(|| reject("malformed app signature"))?;
    if provided.ct_eq(&expected).unwrap_u8() != 1 {
        return Err(reject("invalid app signature"));
    }

    consume_nonce(&state, &app_id, &nonce).await?;

    Ok(next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await)
}

fn sign(
    secret: &[u8],
    method: &str,
    target: &str,
    timestamp: i64,
    nonce: &str,
    body: &Bytes,
) -> Vec<u8> {
    let body_digest = hex::encode(Sha256::digest(body));
    let canonical = format!("{method}\n{target}\n{timestamp}\n{nonce}\n{body_digest}");

    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts keys of any length");
    mac.update(canonical.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Burns the nonce so a captured request can't be replayed inside the
/// timestamp window. Redis being unavailable must not open the door, so a
/// failure here rejects rather than passes.
async fn consume_nonce(state: &AppState, app_id: &str, nonce: &str) -> Result<(), AppError> {
    let key = format!("appsig:{app_id}:{nonce}");
    let stored: Option<String> = state
        .redis
        .set(
            &key,
            1,
            Some(Expiration::EX(MAX_SKEW_SECS * 2)),
            Some(SetOptions::NX),
            true,
        )
        .await
        .map_err(AppError::Redis)?;

    if stored.is_some() {
        return Err(reject("app signature nonce already used"));
    }
    Ok(())
}

fn header(req: &Request, name: &str) -> Result<String, AppError> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| reject(&format!("missing {name} header")))
}

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    hex::decode(value.trim()).ok()
}

fn reject(reason: &str) -> AppError {
    AppError::UntrustedClient(reason.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_covers_every_canonical_field() {
        let secret = [9u8; 32];
        let body = Bytes::from_static(b"{\"username\":\"a\"}");
        let base = sign(&secret, "POST", "/v1/auth/login", 1700, "abc", &body);

        assert_ne!(
            base,
            sign(&secret, "GET", "/v1/auth/login", 1700, "abc", &body)
        );
        assert_ne!(
            base,
            sign(&secret, "POST", "/v1/auth/register", 1700, "abc", &body)
        );
        assert_ne!(
            base,
            sign(&secret, "POST", "/v1/auth/login", 1701, "abc", &body)
        );
        assert_ne!(
            base,
            sign(&secret, "POST", "/v1/auth/login", 1700, "abd", &body)
        );
        assert_ne!(
            base,
            sign(
                &secret,
                "POST",
                "/v1/auth/login",
                1700,
                "abc",
                &Bytes::from_static(b"{}")
            )
        );
        assert_ne!(
            base,
            sign(&[8u8; 32], "POST", "/v1/auth/login", 1700, "abc", &body)
        );
        assert_eq!(
            base,
            sign(&secret, "POST", "/v1/auth/login", 1700, "abc", &body)
        );
    }

    #[test]
    fn parses_configured_keys() {
        let secret = BASE64.encode([1u8; 32]);
        unsafe {
            std::env::set_var(
                "AUTH_APP_KEYS",
                format!("extension:{secret}, mobile:{secret}"),
            )
        };
        let keys = AppKeys::from_env().unwrap().unwrap();
        assert!(keys.secret("extension").is_some());
        assert!(keys.secret("mobile").is_some());
        assert!(keys.secret("cli").is_none());

        unsafe { std::env::set_var("AUTH_APP_KEYS", "  ") };
        assert!(AppKeys::from_env().unwrap().is_none());

        unsafe { std::env::set_var("AUTH_APP_KEYS", "extension:not-base64!") };
        assert!(AppKeys::from_env().is_err());

        unsafe {
            std::env::set_var(
                "AUTH_APP_KEYS",
                format!("extension:{}", BASE64.encode([1u8; 8])),
            )
        };
        assert!(AppKeys::from_env().is_err());

        unsafe { std::env::remove_var("AUTH_APP_KEYS") };
        assert!(AppKeys::from_env().unwrap().is_none());
    }
}
