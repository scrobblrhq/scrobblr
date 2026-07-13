use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PasswordError {
    #[error("failed to hash password: {0}")]
    HashError(String),
    #[error("invalid password")]
    InvalidPassword,
    #[error("invalid hash format: {0}")]
    InvalidHash(String),
}

/// Hash a plaintext password using Argon2id with a random salt.
pub fn hash_password(password: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();

    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| PasswordError::HashError(e.to_string()))
}

/// Verify a plaintext password against a stored Argon2 hash.
pub fn verify_password(password: &str, hash: &str) -> Result<(), PasswordError> {
    let parsed_hash =
        PasswordHash::new(hash).map_err(|e| PasswordError::InvalidHash(e.to_string()))?;

    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .map_err(|_| PasswordError::InvalidPassword)
}
