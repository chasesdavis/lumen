//! lumen-api-server binary: boots the Axum service with config from env.
//!
//! Dev mode defaults work out of the box (in-memory stores, signing secret
//! from LUMEN_JWT_SECRET). Production overrides come from LUMEN_* env vars;
//! see the config block below. **NEVER** ship or commit real secrets: the
//! default must always be overridden in production via LUMEN_JWT_SECRET.

use lumen_api_server::auth::KeyMaterial;
use lumen_api_server::routes::{router, AppState};
use lumen_api_server::usage::UsageLedger;
use lumen_api_server::users::UserStore;
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lumen_api_server=info".into()),
        )
        .init();

    let host = std::env::var("LUMEN_API_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("LUMEN_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    // File-backed persistence when paths are provided, in-memory otherwise.
    let users = match std::env::var("LUMEN_USERS_DB") {
        Ok(path) => {
            if path.is_empty() {
                UserStore::open_memory().expect("user store init")
            } else {
                UserStore::open(&path).expect("user store file open failed")
            }
        }
        Err(_) => UserStore::open_memory().expect("user store init"),
    };
    let usage = match std::env::var("LUMEN_USAGE_DB") {
        Ok(path) => {
            if path.is_empty() {
                UsageLedger::open_memory().expect("usage ledger init")
            } else {
                UsageLedger::open(&path).expect("usage ledger file open failed")
            }
        }
        Err(_) => UsageLedger::open_memory().expect("usage ledger init"),
    };
    let keys = KeyMaterial::from_secret(
        std::env::var("LUMEN_JWT_SECRET")
            .expect("LUMEN_JWT_SECRET must be set (e.g. 32+ random bytes); never use a committed value in production")
            .as_bytes(),
    );

    let state = std::sync::Arc::new(AppState {
        changes: std::sync::Arc::new(lumen_core::changelog::ChangeLog::default()),
        keys,
        users,
        usage,
        dodo_webhook_key: std::env::var("LUMEN_DODO_WEBHOOK_KEY")
            .unwrap_or_else(|_| "whsec_lumen_dev_placeholder".into()),
    });
    let app = router(state);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .expect("LUMEN_API_HOST/PORT must form a valid socket address");
    tracing::info!("lumen-api-server listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|_| panic!("failed to bind {addr}"));
    axum::serve(listener, app).await.expect("api server error");
}
