//! Axum HTTP routes mirroring the API surface mapped from Helumi's
//! OrbitAPI client, plus Lumen-only endpoints: MCP token issue and
//! storage usage reporting for metered billing.

use crate::auth::{self, Claims, KeyMaterial};
use crate::dodo_client::{CreateCheckoutRequest, DodoClient};
use crate::usage::UsageLedger;
use crate::users::UserStore;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Json;
use lumen_core::changelog::ChangeLog;
use lumen_core::gc::gc_collect;
use lumen_core::index::MetadataIndex;
use lumen_core::store::ObjectStore;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct AppState {
    /// Server-side ordered changelog. Shared with any local SyncEngine via
    /// Arc<ChangeLog> (the core ChangeFeed impl) so /v1/changes and direct
    /// engine pulls observe exactly the same mutation stream.
    pub changes: Arc<ChangeLog>,
    pub keys: KeyMaterial,
    pub users: UserStore,
    pub usage: UsageLedger,
    /// Dodo webhook signing key (whsec_...). Placeholder in dev state;
    /// production loads from env/config, never committed.
    pub dodo_webhook_key: String,
    /// Optional server-side metadata index + object store pair backing
    /// the GC admin surface. When present, file upserts/deletes feed the
    /// index so gc admin passes have a live reference set. None keeps the
    /// server purely changelog/metadata-only (no store credentials here).
    pub gc_backend: Option<GcBackend>,
    /// Optional Dodo checkout client. When present, POST /v1/billing/checkout
    /// creates hosted sessions; when absent that route returns 501 so dev /
    /// test deployments without Dodo credentials keep working.
    pub dodo_client: Option<Arc<DodoClient>>,
    /// Dashboard-created base subscription product id (pdt_...). Read from env
    /// at boot, never hardcoded per Dodo's dashboard-managed-products rule.
    pub dodo_base_product_id: String,
}

/// Server-side index + store pair used by the GC admin route. The index is
/// always local SQLite (chunk maps only, no plaintext). The store can be a
/// local dir backend or the shared S3-compatible backend - either way this
/// service never holds chunk encryption keys.
#[derive(Clone)]
pub struct GcBackend {
    pub index: Arc<MetadataIndex>,
    pub store: Arc<dyn ObjectStore>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    /// Dev/default state: in-memory user store, dev signing secret.
    /// Production builds construct this with a file-backed UserStore and
    /// a real key from config/env (never commit the default secret).
    pub fn new_dev() -> Self {
        Self {
            changes: Arc::new(ChangeLog::default()),
            keys: KeyMaterial::from_secret(b"lumen-dev-secret-change-in-production"),
            users: UserStore::open_memory().expect("user store init"),
            usage: UsageLedger::open_memory().expect("usage ledger init"),
            dodo_webhook_key: "whsec_lumen_dev_placeholder".into(),
            gc_backend: None,
            dodo_client: None,
            dodo_base_product_id: String::new(),
        }
    }

    /// Production-style state over a specific changelog and stores.
    pub fn new(
        changes: Arc<ChangeLog>,
        keys: KeyMaterial,
        users: UserStore,
        usage: UsageLedger,
    ) -> Self {
        Self {
            changes,
            keys,
            users,
            usage,
            dodo_webhook_key: "whsec_lumen_dev_placeholder".into(),
            gc_backend: None,
            dodo_client: None,
            dodo_base_product_id: String::new(),
        }
    }

    /// Full production constructor including the Dodo webhook key.
    pub fn with_dodo(mut self, key: String) -> Self {
        self.dodo_webhook_key = key;
        self
    }

    /// Attach a server-side index + store for GC and reference tracking.
    pub fn with_gc_backend(mut self, backend: GcBackend) -> Self {
        self.gc_backend = Some(backend);
        self
    }

    /// Attach a Dodo client + base product id to enable checkout creation.
    pub fn with_dodo_client(
        mut self,
        client: Arc<DodoClient>,
        base_product_id: impl Into<String>,
    ) -> Self {
        self.dodo_client = Some(client);
        self.dodo_base_product_id = base_product_id.into();
        self
    }
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

pub async fn signup(
    state: axum::extract::State<SharedState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match state.users.signup(&req.email, &req.password) {
        Ok(_) => login(state, Json(req)).await,
        Err(lumen_core::error::LumenError::Conflict(msg)) => {
            Err((StatusCode::CONFLICT, Json(json!({"error": msg}))))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )),
    }
}

pub async fn login(
    state: axum::extract::State<SharedState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let user = state.users.login(&req.email, &req.password).map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid email or password"})),
        )
    })?;
    let access = Claims::new_access(&user.id, &user.email);
    let refresh = Claims::new_refresh(&user.id, &user.email);
    let at = auth::issue(&access, &state.keys).unwrap_or_default();
    let rt = auth::issue(&refresh, &state.keys).unwrap_or_default();
    Ok(Json(json!({ "token": at, "refresh": rt })))
}

/// Authenticated identity + current plan state. Plan is read fresh from the
/// user store on every call so webhook fulfillment (subscription.active)
/// reflects immediately without token reissue.
pub async fn me(
    state: axum::extract::State<SharedState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if token.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let claims = auth::verify(token, auth::TokenScope::Access, &state.keys)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let plan = state
        .users
        .plan_of(&claims.sub)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({
        "id": claims.sub,
        "email": claims.email,
        "plan": plan.as_str(),
    })))
}

pub async fn changes(
    state: axum::extract::State<SharedState>,
    params: axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    let q = params.0;
    let since: i64 = q.get("since").and_then(|v| v.parse().ok()).unwrap_or(0);
    let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(500);
    let page = state
        .changes
        .changes_since(since, limit)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({
        "cursor": page.cursor,
        "more": page.more,
        "upserts": page.upserts,
        "removals": page.removals,
    })))
}

#[derive(Deserialize)]
pub struct FileUpsertRequest {
    pub id: uuid::Uuid,
    #[serde(default)]
    pub parent_id: Option<uuid::Uuid>,
    pub name: String,
    #[serde(default = "default_size")]
    pub size: u64,
    #[serde(default)]
    pub content_version: i64,
    #[serde(default)]
    pub metadata_version: i64,
    /// Chunk map uploaded by the client after chunk upload completes. The
    /// server is metadata-only here; it never sees plaintext or keys.
    #[serde(default)]
    pub chunks: Vec<lumen_core::metadata::ChunkRef>,
}

fn default_size() -> u64 {
    0
}

/// Record a file/folder upsert into the ordered changelog. Clients that
/// persisted a cursor pick this up on the next /v1/changes pull - the same
/// single stream of truth Helumi's OrbitAPI drove differently per surface.
pub async fn file_upsert(
    state: axum::extract::State<SharedState>,
    Json(req): Json<FileUpsertRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let now = chrono::Utc::now();
    let item = lumen_core::metadata::RemoteItem {
        id: req.id,
        parent_id: req.parent_id,
        name: req.name,
        item_type: lumen_core::metadata::ItemType::File,
        size: req.size,
        // Optimistic concurrency: version must move forward, never back.
        content_version: req.content_version.max(1),
        metadata_version: req.metadata_version.max(1),
        chunks: req.chunks,
        created_at: now,
        modified_at: now,
    };
    // Feed the server-side index too when GC tracking is enabled. Same rule
    // as the changelog: metadata + chunk keys only, plaintext never sent.
    if let Some(backend) = &state.gc_backend {
        backend.index.upsert(&item).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;
    }
    let seq = state.changes.record_upsert(item).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "changelog append failed"})),
        )
    })?;
    Ok(Json(json!({ "id": req.id.to_string(), "seq": seq })))
}

#[derive(Deserialize)]
pub struct FileDeleteRequest {
    pub id: uuid::Uuid,
}

pub async fn file_delete(
    state: axum::extract::State<SharedState>,
    Json(req): Json<FileDeleteRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if let Some(backend) = &state.gc_backend {
        backend.index.remove(&req.id).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;
    }
    state.changes.record_removal(req.id).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "changelog append failed"})),
        )
    })?;
    Ok(Json(json!({ "id": req.id.to_string(), "removed": true })))
}

/// Dodo webhook endpoint. Verifies Standard Webhooks headers + HMAC over
/// "{id}.{timestamp}.{raw_body}", then dispatches on event type. Malformed
/// headers and stale timestamps 400 (spec-conformant rejection without a
/// valid signed message); signature mismatch fails closed the same way.
/// Unknown event types return 200 so Dodo stops retrying them.
pub async fn dodo_webhook(
    state: axum::extract::State<SharedState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<Json<Value>, StatusCode> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hook_headers =
        crate::dodo::WebhookHeaders::from_headers(&headers).map_err(|_| StatusCode::BAD_REQUEST)?;
    crate::dodo::verify_webhook(&state.dodo_webhook_key, &hook_headers, body.as_bytes(), now)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let evt: crate::dodo::DodoEvent =
        serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    match evt.event_type.as_str() {
        crate::dodo::EVENT_PAYMENT_SUCCEEDED
        | crate::dodo::EVENT_SUBSCRIPTION_ACTIVE
        | crate::dodo::EVENT_SUBSCRIPTION_RENEWED => {
            // Fulfillment order: (1) checkout session metadata carries our
            // user_id - it was embedded at checkout creation from an
            // authenticated JWT, so link_and_grant_by_user_id is the
            // authoritative path and also persists customer_id. (2) Fallback:
            // match evt.data.customer_id against the users.dodo_customer_id
            // column (renewals of pre-metadata subscriptions). Unknown
            // customers still 200 - Dodo retries would not fix a mapping that
            // does not exist (checkout-before-signup race), so we ack and rely
            // on signup-time linking plus plan refresh on next login.
            let data = evt.data.as_ref();
            let customer_id = data
                .as_ref()
                .and_then(|d| d.get("customer_id"))
                .and_then(|v| v.as_str());
            // Metadata-first fulfillment: user_id was embedded in the checkout
            // session metadata (authenticated at creation), so it is the
            // authoritative link - one UPDATE grants the plan AND persists
            // customer_id for future renewals.
            if let Some(user_id) = data
                .and_then(|d| d.get("metadata"))
                .and_then(|m| m.get("user_id"))
                .and_then(|v| v.as_str())
            {
                match state.users.link_and_grant_by_user_id(
                    user_id,
                    customer_id,
                    crate::users::Plan::Base,
                ) {
                    Ok(true) => return Ok(Json(json!({ "received": true, "fulfilled": true }))),
                    _ => return Ok(Json(json!({ "received": true, "fulfilled": false }))),
                }
            }
            // Fallback for events without our metadata: grant by an already
            // linked Dodo customer id (pre-metadata subscriptions, renewals).
            let fulfilled = customer_id.map(|cid| {
                state
                    .users
                    .grant_plan_by_dodo_customer(cid, crate::users::Plan::Base)
            });
            match fulfilled {
                Some(Ok(true)) => Ok(Json(json!({ "received": true, "fulfilled": true }))),
                _ => Ok(Json(json!({ "received": true, "fulfilled": false }))),
            }
        }
        _ => Ok(Json(json!({ "received": true, "handled": false }))),
    }
}

/// Bearer-token extractor: validates the access JWT and hands the handler
/// the claims. Bad/expired tokens surface as 401, not 500s from handlers.
pub async fn usage(
    state: axum::extract::State<SharedState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if token.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let claims = auth::verify(token, auth::TokenScope::Access, &state.keys)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let (stored, retrieved) = state
        .usage
        .current_period_usage(&claims.sub)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({
        "storage_bytes": stored,
        "retrieved_bytes": retrieved,
        "period": crate::usage::UsageLedger::period_of(chrono::Utc::now()),
    })))
}

/// Run one mark-and-sweep GC pass over the server-side index + store and
/// record the reclaimed bytes as a NEGATIVE stored-bytes delta on the
/// caller's ledger row. This is what reconciles pass-through billing with
/// what the provider actually stores after deletions: reclaimed chunks
/// must reduce the metered amount, not inflate it.
///
/// Auth: Bearer access token. The caller's user id scopes both the ledger
/// delta and (in production deployments) the store namespace; a single
/// shared backend here is fine because GcReport only carries aggregate
/// counts - no cross-user data is exposed.
pub async fn admin_gc(
    state: axum::extract::State<SharedState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if token.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let claims = auth::verify(token, auth::TokenScope::Access, &state.keys)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let backend = state
        .gc_backend
        .as_ref()
        .ok_or(StatusCode::NOT_IMPLEMENTED)?;
    let report = gc_collect(&backend.index, backend.store.as_ref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Signed delta: reclaim subtracts from the current period meter.
    if report.reclaimed_bytes > 0 {
        state
            .usage
            .record(
                &claims.sub,
                -(report.reclaimed_bytes as i64),
                0,
                chrono::Utc::now(),
            )
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    Ok(Json(json!({
        "scanned_keys": report.scanned_keys,
        "reclaimed_keys": report.reclaimed_keys,
        "reclaimed_bytes": report.reclaimed_bytes,
    })))
}

/// POST /v1/billing/checkout - create a Dodo hosted checkout session for the
/// base subscription. The authenticated user's id is embedded in the session
/// metadata; when the webhook fires on payment, fulfillment links the Dodo
/// customer and grants the plan via that metadata without a second lookup.
///
/// Request body: { "return_url": "...", "cancel_url": "..." } - both are
/// client-facing UX URLs for post-payment redirect. Without a configured
/// DodoClient (dev/test) this returns 501 so non-billing deployments stay
/// functional.
pub async fn billing_checkout(
    state: axum::extract::State<SharedState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CheckoutRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if token.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "missing bearer token"})),
        ));
    }
    let claims = auth::verify(token, auth::TokenScope::Access, &state.keys).map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid or expired token"})),
        )
    })?;
    let dodo = state.dodo_client.as_ref().ok_or((
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "billing not configured (no Dodo API credentials)"})),
    ))?;
    if state.dodo_base_product_id.is_empty() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "billing not configured (missing base product id)"})),
        ));
    }
    let mut checkout_req = CreateCheckoutRequest::base_subscription(
        &state.dodo_base_product_id,
        &req.return_url,
        &req.cancel_url,
    );
    let mut metadata = std::collections::HashMap::with_capacity(1);
    metadata.insert("user_id".to_string(), claims.sub.clone());
    if let Some(email) = req.email {
        metadata.insert("email".to_string(), email);
    }
    checkout_req.metadata = Some(metadata);
    let session = dodo.create_checkout(&checkout_req).await.map_err(|e| {
        tracing::error!(error = %e, "dodo checkout creation failed");
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "checkout provider error"})),
        )
    })?;
    tracing::info!(
        session_id = %session.session_id,
        user = %claims.sub,
        "created Dodo checkout session"
    );
    Ok(Json(json!({
        "session_id": session.session_id,
        "checkout_url": session.checkout_url,
    })))
}

#[derive(Deserialize)]
pub struct CheckoutRequest {
    /// Post-payment success/failure redirect target.
    pub return_url: String,
    /// Shown when customer navigates back from checkout.
    pub cancel_url: String,
    /// Optional email hint persisted alongside user_id in session metadata -
    /// Dodo uses it to prefill the checkout form.
    #[serde(default)]
    pub email: Option<String>,
}

pub fn router(state: SharedState) -> axum::Router {
    axum::Router::new()
        .route("/v1/auth/signup", post(signup))
        .route("/v1/auth/login", post(login))
        .route("/v1/me", get(me))
        .route("/v1/changes", get(changes))
        .route("/v1/files", post(file_upsert))
        .route("/v1/files/delete", post(file_delete))
        .route("/v1/billing/checkout", post(billing_checkout))
        .route("/v1/billing/usage", get(usage))
        .route("/v1/admin/gc", post(admin_gc))
        .route("/v1/dodo/webhook", post(dodo_webhook))
        .with_state(state)
}
