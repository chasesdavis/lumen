//! MCP session authorization.
//!
//! When a JWT signing key is configured (LUMEN_JWT_SECRET), tool calls are
//! gated behind an explicit "authorize" exchange: the caller presents an API
//! access token, we validate it as an Access-scope JWT against the same
//! KeyMaterial the api-server issues tokens with, and mark this connection
//! authorized. All subsequent tool calls on that connection run open.
//!
//! Without a configured key the server stays in local open mode - matching
//! stdio MCP hosts running against a user-owned index with no auth surface.
//! In open mode an "authorize" call still fails loudly instead of silently
//! reporting success: pretending to check tokens is worse than not
//! checking them.

use lumen_api_server::auth::{self, KeyMaterial, TokenScope};
use std::sync::Arc;

/// Server-wide auth config shared across all connections.
#[derive(Clone)]
pub struct AuthConfig {
    pub keys: Option<Arc<KeyMaterial>>,
}

impl AuthConfig {
    /// Production loader. Any non-empty LUMEN_JWT_SECRET enables gating;
    /// unset/empty leaves the server in local open mode.
    pub fn from_env() -> Self {
        let secret = std::env::var("LUMEN_JWT_SECRET").unwrap_or_default();
        Self {
            keys: if secret.is_empty() {
                None
            } else {
                Some(Arc::new(KeyMaterial::from_secret(secret.as_bytes())))
            },
        }
    }

    pub fn open_mode() -> Self {
        Self { keys: None }
    }

    pub fn gated(keys: KeyMaterial) -> Self {
        Self {
            keys: Some(Arc::new(keys)),
        }
    }

    pub fn is_gated(&self) -> bool {
        self.keys.is_some()
    }

    /// Validate an access token; on success mark the session authorized.
    pub fn authorize(&self, session: &mut Session, token: &str) -> Result<(), String> {
        match &self.keys {
            // Open mode refuses to claim an authorization happened: silent
            // pass would imply tokens were checked when they were not.
            None => {
                Err("auth not configured on this server; set LUMEN_JWT_SECRET to enable".into())
            }
            Some(keys) => {
                auth::verify(token, TokenScope::Access, keys)
                    .map_err(|e| format!("invalid access token: {}", e))?;
                session.authorized = true;
                Ok(())
            }
        }
    }

    /// Whether a tools/call may proceed in this session state.
    pub fn allows_calls(&self, session: &Session) -> bool {
        !self.is_gated() || session.is_authorized()
    }
}

/// Per-connection authorization state.
#[derive(Default)]
pub struct Session {
    authorized: bool,
}

impl Session {
    pub fn is_authorized(&self) -> bool {
        self.authorized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_api_server::auth::Claims;

    fn keys_and_token(user: &str) -> (KeyMaterial, String) {
        let secret = format!("session-test-secret-{user}");
        let keys = KeyMaterial::from_secret(secret.as_bytes());
        let claims = Claims::new_access(&uuid::Uuid::new_v4().to_string(), user);
        let token = auth::issue(&claims, &keys).expect("token issue");
        (keys, token)
    }

    #[test]
    fn open_mode_allows_all_calls_but_refuses_authorize() {
        let cfg = AuthConfig::open_mode();
        let mut s = Session::default();
        assert!(
            cfg.allows_calls(&s),
            "open mode must allow calls without auth"
        );
        assert!(
            cfg.authorize(&mut s, "anything").is_err(),
            "open mode must not pretend a token was verified"
        );
    }

    #[test]
    fn gated_mode_requires_authorize_before_calls() {
        let (keys, token) = keys_and_token("user@example.com");
        let cfg = AuthConfig::gated(keys);
        let mut s = Session::default();
        assert!(
            !cfg.allows_calls(&s),
            "unauthenticated session must be blocked"
        );
        assert!(cfg.authorize(&mut s, &token).is_ok());
        assert!(s.is_authorized());
        assert!(cfg.allows_calls(&s), "authorized sessions stay allowed");
    }

    #[test]
    fn wrong_key_or_bad_token_fails_closed() {
        let (keys_a, _tok) = keys_and_token("user@example.com");
        let (_keys_b, tok_b) = keys_and_token("other@example.com");
        let cfg = AuthConfig::gated(keys_a);
        let mut s = Session::default();
        assert!(
            cfg.authorize(&mut s, &tok_b).is_err(),
            "tokens from another key must fail"
        );
        assert!(!s.is_authorized());
        assert!(cfg.authorize(&mut s, "not-a-jwt").is_err());
        assert!(cfg.authorize(&mut s, "").is_err());
    }

    #[test]
    fn refresh_tokens_are_not_access_tokens() {
        let keys = KeyMaterial::from_secret(b"scope-test-secret");
        let refresh = auth::issue(&Claims::new_refresh("u", "e"), &keys).unwrap();
        let cfg = AuthConfig::gated(KeyMaterial::from_secret(b"scope-test-secret"));
        let mut s = Session::default();
        assert!(
            cfg.authorize(&mut s, &refresh).is_err(),
            "refresh scope tokens must not authorize MCP sessions - only access tokens can"
        );
        assert!(!s.is_authorized());
        let access = auth::issue(&Claims::new_access("u", "e"), &keys).unwrap();
        assert!(cfg.authorize(&mut s, &access).is_ok());
    }
}
