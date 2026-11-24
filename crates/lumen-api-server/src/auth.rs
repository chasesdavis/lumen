//! Password hashing and JWT issuance/verification for the API server.
//!
//! Argon2id is the password KDF (OWASP-baseline params) and tokens are
//! short-lived access JWTs plus rotating refresh JWTs with distinct
//! audiences so an access token can never be replayed as a refresh.

use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const ACCESS_TOKEN_TTL_SECS: i64 = 15 * 60;
pub const REFRESH_TOKEN_TTL_SECS: i64 = 30 * 24 * 60 * 60;
const ISSUER: &str = "lumen";

#[derive(Debug)]
pub enum AuthError {
    Hash(argon2::password_hash::Error),
    Token(jsonwebtoken::errors::Error),
    Verify(argon2::password_hash::Error),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Hash(e) => write!(f, "hash error: {}", e),
            AuthError::Token(e) => write!(f, "token error: {}", e),
            AuthError::Verify(e) => write!(f, "verify error: {}", e),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub email: String,
    pub scope: TokenScope,
    pub iat: i64,
    pub exp: i64,
    pub iss: String,
}

#[derive(Serialize, Deserialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum TokenScope {
    Access,
    Refresh,
}

pub struct KeyMaterial {
    enc: EncodingKey,
    dec: DecodingKey,
}

impl KeyMaterial {
    pub fn from_secret(secret: &[u8]) -> Self {
        Self {
            enc: EncodingKey::from_secret(secret),
            dec: DecodingKey::from_secret(secret),
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

impl Claims {
    pub fn new_access(user_id: &str, email: &str) -> Self {
        Self {
            sub: user_id.to_string(),
            email: email.to_string(),
            scope: TokenScope::Access,
            iat: now(),
            exp: now() + ACCESS_TOKEN_TTL_SECS,
            iss: ISSUER.to_string(),
        }
    }

    pub fn new_refresh(user_id: &str, email: &str) -> Self {
        Self {
            sub: user_id.to_string(),
            email: email.to_string(),
            scope: TokenScope::Refresh,
            iat: now(),
            exp: now() + REFRESH_TOKEN_TTL_SECS,
            iss: ISSUER.to_string(),
        }
    }
}

pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(AuthError::Hash)
}

pub fn verify_password(password: &str, hash: &str) -> Result<(), AuthError> {
    let parsed = PasswordHash::new(hash).map_err(AuthError::Verify)?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(AuthError::Verify)
}

pub fn issue(claims: &Claims, keys: &KeyMaterial) -> Result<String, AuthError> {
    encode(&Header::default(), claims, &keys.enc).map_err(AuthError::Token)
}

pub fn verify(
    token: &str,
    expected_scope: TokenScope,
    keys: &KeyMaterial,
) -> Result<Claims, AuthError> {
    let mut validation = Validation::default();
    validation.set_issuer(&[ISSUER]);
    let data = decode::<Claims>(token, &keys.dec, &validation).map_err(AuthError::Token)?;
    if data.claims.scope != expected_scope {
        return Err(AuthError::Token(
            jsonwebtoken::errors::ErrorKind::InvalidToken.into(),
        ));
    }
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash).is_ok());
        assert!(verify_password("wrong password", &hash).is_err());
    }

    #[test]
    fn token_scope_separation() {
        let keys = KeyMaterial::from_secret(b"unit-test-secret-0123456789abcdef");
        let access = Claims::new_access("user-1", "a@b.c");
        let refresh = Claims::new_refresh("user-1", "a@b.c");
        let at = issue(&access, &keys).unwrap();
        let rt = issue(&refresh, &keys).unwrap();
        let a = verify(&at, TokenScope::Access, &keys).unwrap();
        let r = verify(&rt, TokenScope::Refresh, &keys).unwrap();
        assert_eq!(a.sub, r.sub);
        // Access token must not verify as refresh and vice versa.
        assert!(verify(&at, TokenScope::Refresh, &keys).is_err());
        assert!(verify(&rt, TokenScope::Access, &keys).is_err());
    }
}
