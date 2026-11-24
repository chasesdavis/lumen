//! Dodo Payments integration boundary.
//!
//! Two concerns live here and nothing else:
//! 1. Webhook verification per the Standard Webhooks spec Dodo follows:
//!    three headers (webhook-id, webhook-timestamp, webhook-signature),
//!    HMAC-SHA256 over "{id}.{timestamp}.{raw_body}", Base64-encoded,
//!    constant-time compared, timestamp inside a replay window.
//! 2. Event envelope parsing for the events the billing flow acts on
//!    (payment.succeeded / payment.failed, subscription lifecycle).

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

/// Replay tolerance. Standard Webhooks suggests 5 minutes; we follow that.
pub const WEBHOOK_TOLERANCE_SECS: u64 = 5 * 60;

#[derive(Debug)]
pub enum WebhookError {
    /// Any required header missing or undecodable - reject with 400.
    MalformedHeaders(String),
    /// Timestamp outside tolerance - possible replay attempt.
    StaleTimestamp,
    /// HMAC mismatch or bad signature encoding - reject with 401.
    SignatureMismatch,
}

impl std::fmt::Display for WebhookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebhookError::MalformedHeaders(why) => write!(f, "malformed webhook headers: {why}"),
            WebhookError::StaleTimestamp => write!(f, "webhook timestamp outside tolerance"),
            WebhookError::SignatureMismatch => write!(f, "webhook signature verification failed"),
        }
    }
}

/// The three Standard Webhooks headers extracted from a request.
#[derive(Debug, Clone)]
pub struct WebhookHeaders {
    pub id: String,
    pub timestamp: u64,
    pub signature_b64: String,
}

impl WebhookHeaders {
    /// Extract from an HTTP header map. Header names are lowercase in
    /// practice (axum normalizes), so lowercase lookups suffice.
    pub fn from_headers(headers: &axum::http::HeaderMap) -> Result<Self, WebhookError> {
        let get = |name: &str| -> Option<String> {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        let id = get("webhook-id")
            .ok_or_else(|| WebhookError::MalformedHeaders("missing webhook-id".into()))?;
        let ts_raw = get("webhook-timestamp")
            .ok_or_else(|| WebhookError::MalformedHeaders("missing webhook-timestamp".into()))?;
        let timestamp = ts_raw
            .parse()
            .map_err(|_| WebhookError::MalformedHeaders("bad webhook-timestamp".into()))?;
        let signature_b64 = get("webhook-signature")
            .ok_or_else(|| WebhookError::MalformedHeaders("missing webhook-signature".into()))?;
        Ok(Self {
            id,
            timestamp,
            signature_b64,
        })
    }
}

/// Verify one webhook per Standard Webhooks. Signed content is
/// "{id}.{timestamp}.{raw_body}" HMAC-SHA256'd with the endpoint signing
/// key, Base64-encoded, prefixed "v1," per the spec's default scheme.
pub fn verify_webhook(
    signing_key: &str,
    headers: &WebhookHeaders,
    raw_body: &[u8],
    now_unix: u64,
) -> Result<(), WebhookError> {
    if now_unix.abs_diff(headers.timestamp) > WEBHOOK_TOLERANCE_SECS {
        return Err(WebhookError::StaleTimestamp);
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_key.as_bytes())
        .map_err(|e| WebhookError::MalformedHeaders(e.to_string()))?;
    mac.update(headers.id.as_bytes());
    mac.update(b".");
    mac.update(headers.timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(raw_body);

    // Standard Webhooks allows multiple space-separated signatures; any
    // candidate matching is valid (supports key rotation). Candidates use
    // the "v1," prefix (scheme version tag + comma), not "=".
    for candidate in headers.signature_b64.split(' ') {
        let Some(raw) = candidate.strip_prefix("v1,") else {
            continue;
        };
        match B64.decode(raw.trim()) {
            Ok(bytes) => {
                if mac.clone().verify_slice(&bytes).is_ok() {
                    return Ok(());
                }
                continue;
            }
            Err(_) => return Err(WebhookError::SignatureMismatch),
        }
    }
    Err(WebhookError::SignatureMismatch)
}

/// Minimal event envelope we act on. Dodo payloads carry "type" at the
/// top level (e.g. "payment.succeeded") with resource data in "data".
/// Unknown events parse fine and dispatch idempotently - returning 200
/// stops webhook retries from hammering the endpoint.
#[derive(Debug, Deserialize)]
pub struct DodoEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// Events the billing flow acts on today.
pub const EVENT_PAYMENT_SUCCEEDED: &str = "payment.succeeded";
pub const EVENT_PAYMENT_FAILED: &str = "payment.failed";
pub const EVENT_SUBSCRIPTION_ACTIVE: &str = "subscription.active";
pub const EVENT_SUBSCRIPTION_RENEWED: &str = "subscription.renewed";

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(key: &str, id: &str, ts: u64, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
        mac.update(id.as_bytes());
        mac.update(b".");
        mac.update(ts.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        format!("v1,{}", B64.encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn webhook_verification_accepts_valid_and_rejects_bad() {
        let key = "whsec_test_endpoint_key";
        let body = br#"{"type":"payment.succeeded","data":{"payment_id":"pay_1"}}"#;
        let id = "msg_2uJcZ0V1b9";
        let ts = 1700000000_u64;
        let good_sig = sign(key, id, ts, body);

        // Correct signature, in-tolerance timestamp -> Ok
        assert!(verify_webhook(
            key,
            &WebhookHeaders {
                id: id.into(),
                timestamp: ts,
                signature_b64: good_sig.clone()
            },
            body,
            ts + 60,
        )
        .is_ok());

        // Wrong signing key -> SignatureMismatch (fail closed)
        assert!(matches!(
            verify_webhook(
                "wrong-key",
                &WebhookHeaders {
                    id: id.into(),
                    timestamp: ts,
                    signature_b64: good_sig
                },
                body,
                ts + 60
            ),
            Err(WebhookError::SignatureMismatch)
        ));

        // Replay outside tolerance rejected even with a valid signature.
        let sig2 = sign(key, id, ts, body);
        assert!(matches!(
            verify_webhook(
                key,
                &WebhookHeaders {
                    id: id.into(),
                    timestamp: ts,
                    signature_b64: sig2
                },
                body,
                ts + WEBHOOK_TOLERANCE_SECS + 1,
            ),
            Err(WebhookError::StaleTimestamp)
        ));
    }

    #[test]
    fn multiple_signature_candidates_support_key_rotation() {
        let key_old = "whsec_old_key";
        let key_new = "whsec_new_key";
        let body = br#"{"type":"payment.succeeded"}"#;
        let headers = WebhookHeaders {
            id: "msg_rotate".into(),
            timestamp: 1700000000,
            // Old key candidate first, new key second: either validates.
            signature_b64: format!(
                "{} {}",
                sign(key_old, "msg_rotate", 1700000000, body),
                sign(key_new, "msg_rotate", 1700000000, body)
            ),
        };
        assert!(verify_webhook(key_new, &headers, body, 1700000060).is_ok());
        assert!(verify_webhook(key_old, &headers, body, 1700000060).is_ok());
    }

    #[test]
    fn tampered_body_and_garbage_signatures_fail() {
        let key = "whsec_test_endpoint_key";
        let id = "msg_tamper";
        let ts = 1700000000_u64;
        let body = br#"{"type":"payment.succeeded"}"#;

        // Valid signature over different content -> mismatch.
        let sig = sign(key, id, ts, br#"{"type":"payment.failed"}"#);
        assert!(matches!(
            verify_webhook(
                key,
                &WebhookHeaders {
                    id: id.into(),
                    timestamp: ts,
                    signature_b64: sig
                },
                body,
                ts
            ),
            Err(WebhookError::SignatureMismatch)
        ));

        // Non-Base64 garbage -> malformed signature surfaces as error.
        let bad_b64 = WebhookHeaders {
            id: id.into(),
            timestamp: ts,
            signature_b64: "v1,!!!notbase64!!!".into(),
        };
        assert!(verify_webhook(key, &bad_b64, body, ts).is_err());
    }

    #[test]
    fn event_envelope_parses_known_and_unknown_types() {
        let known: DodoEvent =
            serde_json::from_str(r#"{"type":"payment.succeeded","data":{"payment_id":"pay_9"}}"#)
                .unwrap();
        assert_eq!(known.event_type, EVENT_PAYMENT_SUCCEEDED);
        assert!(known.data.is_some());

        // Unknown events parse and are ignorable at dispatch.
        let unknown: DodoEvent =
            serde_json::from_str(r#"{"type":"dispute.opened","data":{}}"#).unwrap();
        assert_eq!(unknown.event_type, "dispute.opened");
    }
}
