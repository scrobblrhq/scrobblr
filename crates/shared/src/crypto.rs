//! Envelope encryption for secrets that must stay *recoverable*. Today that
//! means only third-party OAuth tokens (`connected_accounts.access_token` /
//! `refresh_token`): passwords and API tokens are one-way hashed instead
//! (see [`crate::password`] and `db::queries::auth::hash_api_token`), but
//! the worker has to replay the original Spotify token back to Spotify, so
//! hashing isn't an option there.
//!
//! AES-256-GCM with a fresh random 96-bit nonce per value, keyed by
//! `TOKEN_ENCRYPTION_KEY` (32 random bytes, base64). Stored form is
//! `v1:<base64(nonce ‖ ciphertext ‖ tag)>`; the `v1:` prefix exists so a
//! later algorithm change or key rotation can tell new values from old ones
//! without a migration.
//!
//! Generate a key with `openssl rand -base64 32`. Losing it means every
//! stored connection has to be re-authorized — it is not derivable from
//! anything else.

use std::sync::OnceLock;

use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, KeyInit, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use thiserror::Error;

const ENV_KEY: &str = "TOKEN_ENCRYPTION_KEY";
const PREFIX: &str = "v1:";
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Deliberately carries no detail about the value that failed — these get
/// logged, and the whole point of the module is to keep token material out
/// of logs and backups.
#[derive(Debug, Clone, Error)]
pub enum CryptoError {
    #[error(
        "TOKEN_ENCRYPTION_KEY is not set; connected accounts cannot store OAuth tokens without it"
    )]
    MissingKey,
    #[error(
        "TOKEN_ENCRYPTION_KEY must be 32 random bytes, base64-encoded (openssl rand -base64 32)"
    )]
    InvalidKey,
    #[error("stored ciphertext is not in the expected v1 format")]
    MalformedCiphertext,
    #[error("could not decrypt stored value; was TOKEN_ENCRYPTION_KEY changed?")]
    Decrypt,
    #[error("could not encrypt value")]
    Encrypt,
}

/// Reads and caches the key on first use. Cached as a `Result` so a
/// misconfigured deployment fails the same way on every call instead of
/// re-reading the environment each time.
fn key() -> Result<&'static [u8; KEY_LEN], CryptoError> {
    static KEY: OnceLock<Result<[u8; KEY_LEN], CryptoError>> = OnceLock::new();
    KEY.get_or_init(load_key).as_ref().map_err(Clone::clone)
}

fn load_key() -> Result<[u8; KEY_LEN], CryptoError> {
    let encoded = std::env::var(ENV_KEY).map_err(|_| CryptoError::MissingKey)?;
    let bytes = BASE64
        .decode(encoded.trim())
        .map_err(|_| CryptoError::InvalidKey)?;
    bytes.try_into().map_err(|_| CryptoError::InvalidKey)
}

/// Returns `Ok(())` if a usable key is configured. Call this at startup so
/// a bad `TOKEN_ENCRYPTION_KEY` surfaces immediately rather than on the
/// first OAuth callback.
pub fn check_key() -> Result<(), CryptoError> {
    key().map(|_| ())
}

pub fn encrypt(plaintext: &str) -> Result<String, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(key()?).map_err(|_| CryptoError::InvalidKey)?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

    let mut blob = nonce.to_vec();
    blob.extend_from_slice(
        &cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| CryptoError::Encrypt)?,
    );

    Ok(format!("{PREFIX}{}", BASE64.encode(blob)))
}

pub fn decrypt(stored: &str) -> Result<String, CryptoError> {
    let encoded = stored
        .strip_prefix(PREFIX)
        .ok_or(CryptoError::MalformedCiphertext)?;
    let blob = BASE64
        .decode(encoded)
        .map_err(|_| CryptoError::MalformedCiphertext)?;
    if blob.len() <= NONCE_LEN {
        return Err(CryptoError::MalformedCiphertext);
    }

    let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new_from_slice(key()?).map_err(|_| CryptoError::InvalidKey)?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| CryptoError::Decrypt)?;

    String::from_utf8(plaintext).map_err(|_| CryptoError::Decrypt)
}

/// [`encrypt`] over an optional value — `refresh_token` is nullable.
pub fn encrypt_opt(plaintext: Option<&str>) -> Result<Option<String>, CryptoError> {
    plaintext.map(encrypt).transpose()
}

/// [`decrypt`] over an optional value.
pub fn decrypt_opt(stored: Option<&str>) -> Result<Option<String>, CryptoError> {
    stored.map(decrypt).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All tests share one process-wide `OnceLock`, so the key is set once
    /// here and every test runs against it.
    fn with_key() {
        // SAFETY: tests in a crate share a process; this races only with
        // other tests that read the key, all of which want this same value.
        unsafe { std::env::set_var(ENV_KEY, BASE64.encode([7u8; KEY_LEN])) };
    }

    #[test]
    fn round_trips() {
        with_key();
        let ciphertext = encrypt("BQC4-secret-access-token").unwrap();
        assert!(ciphertext.starts_with(PREFIX));
        assert!(!ciphertext.contains("secret"));
        assert_eq!(decrypt(&ciphertext).unwrap(), "BQC4-secret-access-token");
    }

    /// A fresh nonce per call means identical tokens don't produce
    /// identical rows — otherwise the DB would leak which users share a
    /// token value.
    #[test]
    fn same_plaintext_encrypts_differently() {
        with_key();
        assert_ne!(encrypt("same").unwrap(), encrypt("same").unwrap());
    }

    #[test]
    fn rejects_tampered_and_unprefixed_values() {
        with_key();
        let ciphertext = encrypt("token").unwrap();

        let mut tampered = ciphertext.clone().into_bytes();
        let last = tampered.len() - 1;
        tampered[last] ^= 0b0000_0001;
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(decrypt(&tampered).is_err());

        // A plaintext value that predates encryption must not silently pass
        // through as if it were decrypted.
        assert!(matches!(
            decrypt("plaintext-token"),
            Err(CryptoError::MalformedCiphertext)
        ));
    }
}
