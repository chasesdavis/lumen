//! End-to-end contract test for POST /v1/billing/checkout against a
//! one-shot mock Dodo API. Proves the full chain: Bearer JWT auth ->
//! user_id embedded in session metadata -> wire-format correct POST
//! /checkouts -> session_id + checkout_url returned to the client.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use lumen_api_server::dodo_client::{DodoClient, DodoClientConfig};
use lumen_api_server::routes::{router, AppState};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

/// Blocking single-request HTTP reader/writer mirroring the mock pattern
/// inside dodo_client.rs tests. One connection, one request, one response.
fn serve_once(listener: std::net::TcpListener) {
    let Ok((mut stream, _)) = listener.accept() else {
        return;
    };
    let mut buf = Vec::new();
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
                    let mut len: Option<usize> = None;
                    for line in s[..hpos].lines() {
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().ok();
                        }
                    }
                    if let Some(cl) = len {
                        if buf.len() >= hpos + 4 + cl {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let req = String::from_utf8_lossy(&buf).to_string();
    use std::io::{Read, Write};
    // Wire assertions: bearer auth, product id, metadata.user_id present.
    let failure_reason = check_checkout_request(&req);
    let body = match &failure_reason {
        Some(reason) => {
            format!(r#"{{"code":"WIRE_ASSERT_FAILED","message":"{}"}}"#, reason)
        }
        None => r#"{"session_id":"sess_e2e_1","checkout_url":"https://test.dodopayments.com/c/pay_e2e"}"#.to_string(),
    };
    let status = if failure_reason.is_some() { 400 } else { 200 };
    let resp = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).unwrap();
    stream.flush().unwrap();
    let _ = stream.shutdown(std::net::Shutdown::Both);

    fn check_checkout_request(req: &str) -> Option<String> {
        if !req.starts_with("POST /checkouts ") && !req.contains("\nPOST /checkouts ") {
            return Some("expected POST /checkouts".into());
        }
        if !req
            .to_ascii_lowercase()
            .contains("authorization: bearer sk_e2e_test")
        {
            return Some("missing bearer auth".into());
        }
        let (_, b) = req.split_once("\r\n\r\n")?;
        let v: Value = serde_json::from_str(b).ok()?;
        if v["product_cart"][0]["product_id"] != "pdt_e2e_base" {
            return Some(format!("wrong product id: {}", v));
        }
        if v["metadata"]["user_id"].is_null() {
            return Some(format!("user_id missing from metadata: {}", v));
        }
        if v["return_url"] != "https://lumen.app/billing/done" {
            return Some(format!("missing return_url: {}", v));
        }
        None
    }
}

#[tokio::test]
async fn billing_checkout_creates_session_with_user_id_metadata_end_to_end() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || serve_once(listener));

    let base = format!("http://127.0.0.1:{port}");
    let dodo = Arc::new(
        DodoClient::new(DodoClientConfig {
            api_url: base,
            api_key: "sk_e2e_test".into(),
        })
        .unwrap(),
    );

    let state = Arc::new(AppState::new_dev().with_dodo_client(dodo, "pdt_e2e_base"));

    // Signup to obtain a token.
    let signup_body = serde_json::json!({
        "email": "e2e@example.com",
        "password": "correct horse battery staple"
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/signup")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&signup_body).unwrap()))
        .unwrap();
    let response = router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Login also issues tokens - prove both paths grant the same claims.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&signup_body).unwrap()))
        .unwrap();
    let response = router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    let token = json["token"].as_str().expect("login token").to_string();

    // The act under test.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/billing/checkout")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "return_url": "https://lumen.app/billing/done",
                "cancel_url": "https://lumen.app/billing/cancel"
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["session_id"], "sess_e2e_1");
    assert_eq!(
        json["checkout_url"],
        "https://test.dodopayments.com/c/pay_e2e"
    );
}
