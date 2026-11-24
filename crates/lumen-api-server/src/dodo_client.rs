//! Dodo Payments REST client.
//!
//! Two calls cover the whole billing surface today:
//!
//! - POST /checkouts - create a hosted checkout session for the base
//!   subscription product, returning {session_id, checkout_url}.
//! - POST /events/ingest - report metered storage usage as usage events
//!   against a Dodo meter; event_id is the idempotency key so retries
//!   never double-bill. This replaces Stripe-style metered line items
//!   with Dodo's first-class usage-based billing.
//!
//! Bearer API-key auth. Base URL is config-driven (test mode points at
//! test.dodopayments.com, live at live.dodopayments.com); tests override
//! it to hit a local mock server.

use serde::{Deserialize, Serialize};

pub const DEFAULT_TEST_API_URL: &str = "https://test.dodopayments.com";
pub const DEFAULT_LIVE_API_URL: &str = "https://live.dodopayments.com";

#[derive(Debug)]
pub enum DodoClientError {
    /// Transport failure - caller should retry with backoff.
    Transport(String),
    /// Non-2xx from Dodo - includes status and body so callers can branch
    /// on the documented error "code" field, not the message.
    Status(u16, String),
    /// Response body was not valid JSON or missed expected fields.
    Decode(String),
}

impl std::fmt::Display for DodoClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DodoClientError::Transport(e) => write!(f, "transport error: {e}"),
            DodoClientError::Status(code, body) => write!(f, "HTTP {code}: {body}"),
            DodoClientError::Decode(e) => write!(f, "decode error: {e}"),
        }
    }
}

/// One entry of the checkout session's product cart.
#[derive(Debug, Clone, Serialize)]
pub struct ProductCartItem {
    /// Dashboard-created product ID (pdt_...), never hardcoded.
    pub product_id: String,
    /// Subscription products use 1; storage is metered via events.
    pub quantity: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateCheckoutRequest {
    pub product_cart: Vec<ProductCartItem>,
    /// Custom key-value pairs propagated to the session and its webhook
    /// events. We embed {"user_id": ...} so webhook fulfillment can link
    /// the Dodo customer to our user without a second lookup round-trip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<std::collections::HashMap<String, String>>,
    /// The url to redirect after payment failure or success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_url: Option<String>,
    /// Shown when customer goes back from checkout (optional per API).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_url: Option<String>,
}

impl CreateCheckoutRequest {
    /// Minimal subscription checkout for the base plan product.
    pub fn base_subscription(product_id: &str, return_url: &str, cancel_url: &str) -> Self {
        Self {
            product_cart: vec![ProductCartItem {
                product_id: product_id.into(),
                quantity: 1,
            }],
            metadata: None,
            return_url: Some(return_url.into()),
            cancel_url: Some(cancel_url.into()),
        }
    }
}

/// POST /checkouts response - only fields we consume.
#[derive(Debug, Deserialize)]
pub struct CheckoutSessionResponse {
    pub session_id: String,
    pub checkout_url: Option<String>,
}

/// One usage event ingested via POST /events/ingest. event_id is the
/// idempotency key - a reused event_id is treated as duplicate and never
/// double-counted, so derive it from the action deterministically
/// (e.g. "{customer_id}_storage_{period}" for monthly storage billing).
#[derive(Debug, Clone, Serialize)]
pub struct UsageEvent {
    pub event_id: String,
    pub customer_id: String,
    /// Must match the meter's event name exactly (case-sensitive).
    pub event_name: String,
    /// ISO 8601; must be within 1h past / 5min future.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Meter aggregation/filter metadata (e.g. stored_bytes). Serializes
    /// under the documented "metadata" wire field.
    #[serde(rename = "metadata", skip_serializing_if = "Option::is_none")]
    pub extra_metadata: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct IngestRequest<'a> {
    events: &'a [UsageEvent],
}

/// Storage meter event name used across Lumen. The matching meter must
/// exist in the Dodo dashboard with this exact event name and aggregate
/// over "stored_bytes" in metadata.
pub const STORAGE_EVENT_NAME: &str = "lumen.storage.stored_bytes";

impl UsageEvent {
    /// Deterministic ID so ingestion retries are idempotent per period.
    /// stored_bytes is attached as meter metadata via the builder.
    pub fn storage_event(customer_id: &str, period_key: &str, stored_bytes: u64) -> Self {
        Self {
            event_id: format!("{customer_id}_storage_{period_key}"),
            customer_id: customer_id.into(),
            event_name: STORAGE_EVENT_NAME.into(),
            timestamp: None,
            extra_metadata: None,
        }
        .with_stored_bytes(stored_bytes)
    }

    pub fn with_stored_bytes(mut self, stored_bytes: u64) -> Self {
        self.extra_metadata = Some(serde_json::json!({ "stored_bytes": stored_bytes }));
        self
    }
}

#[derive(Debug, Clone)]
pub struct DodoClientConfig {
    /// https://test.dodopayments.com or https://live.dodopayments.com.
    /// Tests point this at a local mock server.
    pub api_url: String,
    /// API key from Developer - API Keys. Env-provided, never logged.
    pub api_key: String,
}

impl DodoClientConfig {
    pub fn test_mode(api_key: String) -> Self {
        Self {
            api_url: DEFAULT_TEST_API_URL.to_string(),
            api_key,
        }
    }

    pub fn live_mode(api_key: String) -> Self {
        Self {
            api_url: DEFAULT_LIVE_API_URL.to_string(),
            api_key,
        }
    }
}

pub struct DodoClient {
    cfg: DodoClientConfig,
    http: reqwest::Client,
}

impl DodoClient {
    pub fn new(cfg: DodoClientConfig) -> Result<Self, reqwest::Error> {
        Ok(Self {
            cfg,
            http: reqwest::Client::builder().build()?,
        })
    }

    async fn post_json<Req, Res>(&self, path: &str, body: &Req) -> Result<Res, DodoClientError>
    where
        Req: Serialize,
        Res: for<'de> Deserialize<'de>,
    {
        let res = self
            .http
            .post(format!("{}{}", self.cfg.api_url, path))
            .bearer_auth(&self.cfg.api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| DodoClientError::Transport(e.to_string()))?;
        let status = res.status();
        let body_text = res
            .text()
            .await
            .map_err(|e| DodoClientError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(DodoClientError::Status(status.as_u16(), body_text));
        }
        serde_json::from_str(&body_text).map_err(|e| DodoClientError::Decode(e.to_string()))
    }

    /// Create a hosted checkout session; the caller redirects the user to
    /// checkout_url. checkout_url is single-use and expires in 24h.
    pub async fn create_checkout(
        &self,
        req: &CreateCheckoutRequest,
    ) -> Result<CheckoutSessionResponse, DodoClientError> {
        self.post_json("/checkouts", req).await
    }

    /// Ingest usage events (metered storage billing). event_id acts as the
    /// idempotency key, so retrying the same batch never double-bills.
    pub async fn ingest_events(&self, events: &[UsageEvent]) -> Result<(), DodoClientError> {
        let _: serde_json::Value = self
            .post_json("/events/ingest", &IngestRequest { events })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn first_line(req: &str) -> &str {
        req.split("\r\n").next().unwrap_or("")
    }

    fn body_of(req: &str) -> &str {
        req.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
    }

    fn find_content_length(headers: &str) -> Option<usize> {
        for line in headers.lines() {
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                return v.trim().parse().ok();
            }
        }
        None
    }

    /// Single-blocking-read consumer: reads until headers plus any stated
    /// Content-Length are fully buffered. The 2s read timeout bounds worst
    /// cases so failed expectation paths surface as errors, not hangs.
    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok();
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    let s = String::from_utf8_lossy(&buf).to_string();
                    if let Some(hpos) = s.find("\r\n\r\n") {
                        if let Some(cl) = find_content_length(&s[..hpos]) {
                            if buf.len() >= hpos + 4 + cl {
                                break;
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    fn write_response(stream: &mut std::net::TcpStream, status: u16, body: &str) {
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            422 => "Unprocessable Entity",
            _ => "",
        };
        let resp = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status, reason, body.len(), body
        );
        stream.write_all(resp.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    /// One-shot blocking mock Dodo API on an ephemeral port. Runs entirely
    /// on its own OS thread - no runtime interplay with the tokio test -
    /// because serving a single tiny JSON POST is trivially fast. Wire
    /// expectations run inside `expect`: a passing expectation yields the
    /// canned response (`reply_status`/`reply_body`), while any failed
    /// expectation causes a 400 whose body carries the failure reason;
    /// tests therefore see clean successful paths on pass and an explicit
    /// DodoClientError::Status(400, "...reason...") on failure - which
    /// reads far better than opaque cross-thread panics.
    async fn spawn_mock_dodo(
        expect: impl Fn(&str) -> Result<(), String> + Send + 'static,
        reply_status: u16,
        reply_body: &'static str,
    ) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let req = read_request(&mut stream);
            match expect(&req) {
                Ok(()) => write_response(&mut stream, reply_status, reply_body),
                Err(reason) => {
                    let esc = reason.replace('"', "'");
                    write_response(
                        &mut stream,
                        400,
                        &format!("{{\"code\":\"WIRE_ASSERT_FAILED\",\"message\":\"{esc}\"}}"),
                    );
                }
            };
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn create_checkout_posts_wire_format_and_parses_response() {
        let base = spawn_mock_dodo(|req| {
            if !first_line(req).starts_with("POST /checkouts ") {
                return Err(format!("bad request line: {}", first_line(req)));
            }
            if req.to_ascii_lowercase().contains("authorization: bearer sk_dodo_test_key") {
                // ok
            } else {
                return Err("missing bearer auth".into());
            }
            let v: serde_json::Value = serde_json::from_str(body_of(req)).map_err(|e| e.to_string())?;
            if v["product_cart"][0]["product_id"] != "pdt_lumen_base" { return Err("wrong product id".into()); }
            if v["product_cart"][0]["quantity"] != serde_json::json!(1) { return Err("wrong quantity".into()); }
            if v["return_url"] != "https://lumen.app/billing/return" { return Err("missing return_url".into()); }
            Ok(())
        }, 200, r#"{ "session_id": "sess_abc123", "checkout_url": "https://test.dodopayments.com/c/pay_abc" }"#).await;

        let client = DodoClient::new(DodoClientConfig {
            api_url: base,
            api_key: "sk_dodo_test_key".into(),
        })
        .expect("client build");
        let req = CreateCheckoutRequest::base_subscription(
            "pdt_lumen_base",
            "https://lumen.app/billing/return",
            "https://lumen.app/billing",
        );
        match client.create_checkout(&req).await {
            Ok(res) => {
                assert_eq!(res.session_id, "sess_abc123");
                assert_eq!(
                    res.checkout_url.as_deref(),
                    Some("https://test.dodopayments.com/c/pay_abc")
                );
            }
            Err(DodoClientError::Status(400, why)) => panic!("wire assertion failed: {}", why),
            other => panic!("unexpected result: {:?}", other.is_ok()),
        }
    }

    #[tokio::test]
    async fn ingest_events_sends_idempotent_storage_events() {
        let base = spawn_mock_dodo(
            |req| {
                if !first_line(req).starts_with("POST /events/ingest ") {
                    return Err(format!("bad request line: {}", first_line(req)));
                }
                let v: serde_json::Value =
                    serde_json::from_str(body_of(req)).map_err(|e| e.to_string())?;
                let events = v["events"].as_array().ok_or("missing events array")?;
                if events.len() != 1 {
                    return Err("expected exactly one event".into());
                }
                if events[0]["event_id"] != "cus_42_storage_2026-11" {
                    return Err("event_id must be customer+period deterministic".into());
                }
                if events[0]["event_name"] != STORAGE_EVENT_NAME {
                    return Err("wrong event_name".into());
                }
                if events[0]["metadata"]["stored_bytes"] != serde_json::json!(644_245_094_400_u64) {
                    return Err("stored_bytes missing from metadata".into());
                }
                Ok(())
            },
            200,
            "{}",
        )
        .await;

        let client = DodoClient::new(DodoClientConfig {
            api_url: base,
            api_key: "k".into(),
        })
        .expect("client build");
        let evt = UsageEvent::storage_event("cus_42", "2026-11", 644_245_094_400);
        match client.ingest_events(&[evt]).await {
            Ok(()) => {}
            Err(DodoClientError::Status(400, why)) => panic!("wire assertion failed: {}", why),
            other => panic!("unexpected result: {:?}", other.is_ok()),
        }
    }

    #[tokio::test]
    async fn error_responses_surface_as_status_with_body_for_code_branching() {
        // Dodo documents JSON error envelopes {"code","message"}; callers
        // branch on code, so Status carries the body through verbatim.
        let base = spawn_mock_dodo(
            |_| Ok(()),
            422,
            r#"{ "code": "INVALID_PRODUCT_ID", "message": "product not found" }"#,
        )
        .await;
        let client = DodoClient::new(DodoClientConfig {
            api_url: base,
            api_key: "k".into(),
        })
        .expect("client build");
        let req = CreateCheckoutRequest::base_subscription(
            "pdt_bad",
            "https://r.example",
            "https://c.example",
        );
        match client.create_checkout(&req).await {
            Err(DodoClientError::Status(422, body)) => {
                assert!(
                    body.contains("INVALID_PRODUCT_ID"),
                    "body lost code field: {body}"
                );
            }
            other => panic!("expected Status(422), got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn storage_event_builder_is_deterministic_and_serializes_metadata() {
        let a = UsageEvent::storage_event("cus_x", "2026-11", 1024);
        let b = UsageEvent::storage_event("cus_x", "2026-11", 1024);
        assert_eq!(
            a.event_id, b.event_id,
            "idempotency key must be stable across retries"
        );
        assert_eq!(a.event_name, STORAGE_EVENT_NAME);
        assert_eq!(a.timestamp, None);
        let wire = serde_json::to_value(&a).unwrap();
        assert_eq!(wire["metadata"]["stored_bytes"], serde_json::json!(1024));
        assert_eq!(wire["event_name"], STORAGE_EVENT_NAME);
    }
}
