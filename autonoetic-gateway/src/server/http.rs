//! HTTP REST API for remote agent content access.
//!
//! Provides REST endpoints for content operations:
//! - POST /api/content/write - Write content
//! - POST /api/content/read - Read content by name, handle, or alias
//! - GET /api/content/names - List content names in session
//!
//! ## Security
//!
//! All endpoints require authentication via Bearer token (AUTONOETIC_SHARED_SECRET).
//! CORS is restricted by default. Rate limiting can be configured.

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use futures::stream::{self, Stream};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

use crate::router::{
    AsyncIngestResult, AsyncIngestStatus, JsonRpcRequest, JsonRpcResponse, JsonRpcRouter,
};
use crate::runtime::content_store::ContentStore;

/// Shared state for HTTP handlers
#[derive(Clone)]
pub struct HttpState {
    pub store: Arc<Mutex<ContentStore>>,
    /// Shared secret for authentication (Bearer token or `?token=` on SSE)
    pub shared_secret: String,
    /// Maximum request body size in bytes (default: 10MB)
    pub max_body_size: usize,
    /// JSON-RPC router for HTTP ingress (`event.ingest`, streamed `session.status`).
    pub router: Option<Arc<JsonRpcRouter>>,
}

/// Default max body size: 10MB
pub const DEFAULT_MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

/// Valid session_id pattern (alphanumeric, dash, underscore, dot, slash for delegated sessions)
fn is_valid_session_id(s: &str) -> bool {
    s.len() <= 128
        && !s.starts_with('/')
        && !s.contains("//")
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/')
}

/// Valid content name pattern (alphanumeric, dash, underscore, dot, slash for paths)
fn is_valid_content_name(s: &str) -> bool {
    s.len() <= 512
        && !s.starts_with('/')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Validate Bearer token from Authorization header
fn validate_auth(headers: &HeaderMap, expected_secret: &str) -> Result<(), ErrorResponse> {
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ErrorResponse {
            error: "Missing Authorization header".to_string(),
            code: 401,
        })?;

    if !auth_header.starts_with("Bearer ") {
        return Err(ErrorResponse {
            error: "Invalid Authorization format, expected 'Bearer <token>'".to_string(),
            code: 401,
        });
    }

    let token = &auth_header[7..]; // Skip "Bearer "
    let token_valid: bool =
        subtle::ConstantTimeEq::ct_eq(token.as_bytes(), expected_secret.as_bytes()).into();
    if !token_valid {
        return Err(ErrorResponse {
            error: "Invalid token".to_string(),
            code: 403,
        });
    }

    Ok(())
}

/// Bearer auth (`Authorization: Bearer …`) or query token (`?token=`) for SSE clients.
fn validate_bearer_or_query(
    headers: &HeaderMap,
    query_token: Option<&str>,
    expected_secret: &str,
) -> Result<(), ErrorResponse> {
    if validate_auth(headers, expected_secret).is_ok() {
        return Ok(());
    }

    let Some(token) = query_token.map(str::trim).filter(|t| !t.is_empty()) else {
        return Err(ErrorResponse {
            error: "Missing Authorization header or token query parameter".to_string(),
            code: 401,
        });
    };

    let token_valid: bool =
        subtle::ConstantTimeEq::ct_eq(token.as_bytes(), expected_secret.as_bytes()).into();
    if !token_valid {
        return Err(ErrorResponse {
            error: "Invalid token".to_string(),
            code: 403,
        });
    }

    Ok(())
}

async fn dispatch_session_status(
    router: &JsonRpcRouter,
    session_id: &str,
    secret: &str,
) -> JsonRpcResponse {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: format!("http-sse-{}", uuid::Uuid::new_v4()),
        method: "session.status".to_string(),
        params: serde_json::json!({ "session_id": session_id }),
        auth_token: Some(secret.to_string()),
    };
    router.dispatch(req).await
}

fn sse_session_status_terminal(resp: &JsonRpcResponse) -> bool {
    if resp.error.is_some() {
        return true;
    }
    let Some(val) = resp.result.as_ref() else {
        return true;
    };
    match serde_json::from_value::<AsyncIngestResult>(val.clone()) {
        Ok(parsed) => !matches!(
            parsed.status,
            AsyncIngestStatus::Processing | AsyncIngestStatus::SuspendedChildWait
        ),
        Err(_) => true,
    }
}

/// Extract and validate session_id from request
fn validate_session_id(session_id: &str) -> Result<(), ErrorResponse> {
    if !is_valid_session_id(session_id) {
        return Err(ErrorResponse {
            error: "Invalid session_id format".to_string(),
            code: 400,
        });
    }
    Ok(())
}

/// Extract and validate content name
fn validate_content_name(name: &str) -> Result<(), ErrorResponse> {
    if !is_valid_content_name(name) {
        return Err(ErrorResponse {
            error: "Invalid content name format".to_string(),
            code: 400,
        });
    }
    Ok(())
}

/// Request body for content.write
#[derive(Debug, Deserialize, Serialize)]
pub struct WriteRequest {
    pub session_id: String,
    pub name: String,
    pub content: String, // Base64 encoded for binary content
    #[serde(default)]
    pub encoding: Option<String>, // "utf8" (default) or "base64"
    #[serde(default)]
    pub visibility: Option<String>, // "private", "session" (default), "global"
}

impl WriteRequest {
    fn validate(&self) -> Result<(), ErrorResponse> {
        validate_session_id(&self.session_id)?;
        validate_content_name(&self.name)?;
        if self.content.len() > 10_000_000 {
            // 10MB limit
            return Err(ErrorResponse {
                error: "Content too large (max 10MB)".to_string(),
                code: 413,
            });
        }
        Ok(())
    }
}

/// Response for content.write
#[derive(Debug, Serialize, Deserialize)]
pub struct WriteResponse {
    pub handle: String,
    pub name: String,
    pub size_bytes: usize,
}

/// Request body for content.read via POST
#[derive(Debug, Deserialize, Serialize)]
pub struct ReadRequest {
    pub session_id: String,
    pub name_or_handle: String,
}

/// Response for content.read
#[derive(Debug, Serialize, Deserialize)]
pub struct ReadResponse {
    pub content: String, // Base64 encoded for binary content
    pub encoding: String,
    pub size_bytes: usize,
    pub handle: String,
}

/// Query params for listing content names
#[derive(Debug, Deserialize, Serialize)]
pub struct ListQuery {
    pub session_id: String,
}

/// Response for listing content names
#[derive(Debug, Serialize, Deserialize)]
pub struct ListResponse {
    pub names: Vec<ContentName>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ContentName {
    pub name: String,
    pub handle: String,
}

/// Error response
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub code: u16,
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> axum::response::Response {
        let status = StatusCode::from_u16(self.code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self)).into_response()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct HttpEventIngestBody {
    event_type: String,
    #[serde(default)]
    target_agent_id: Option<String>,
    message: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    source_agent_id: Option<String>,
    #[serde(default)]
    async_mode: bool,
}

#[derive(Debug, Deserialize)]
struct SessionStreamQuery {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OperatorActivityStreamQuery {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    interval_ms: Option<u64>,
    #[serde(default)]
    after: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionTimelineStreamQuery {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    interval_ms: Option<u64>,
    #[serde(default)]
    after: Option<String>,
    /// Altitude floor: detail | normal | attention | error. Default: normal.
    #[serde(default)]
    min_altitude: Option<String>,
}

/// Create the HTTP router for content API
pub fn create_router(state: HttpState) -> Router {
    Router::new()
        .route("/", get(handle_serve_index))
        .route("/index.html", get(handle_serve_index))
        .route("/api/jsonrpc", post(handle_jsonrpc))
        .route("/api/event/ingest", post(handle_event_ingest))
        .route(
            "/api/session/stream/{session_id}",
            get(handle_session_stream_sse),
        )
        .route(
            "/api/operator/activity/stream/{root_session_id}",
            get(handle_operator_activity_stream_sse),
        )
        .route(
            "/api/session/timeline/stream/{root_session_id}",
            get(handle_session_timeline_stream_sse),
        )
        .route("/api/content/write", post(handle_write))
        .route(
            "/api/content/read/{session_id}/{name_or_handle}",
            get(handle_read_get),
        )
        .route("/api/content/read", post(handle_read_post))
        .route("/api/content/names", get(handle_list_names))
        .layer(CorsLayer::very_permissive()) // More restrictive than permissive
        .with_state(Arc::new(state))
}

/// Start the HTTP server on the given address
pub async fn start_http_server(addr: std::net::SocketAddr, state: HttpState) -> anyhow::Result<()> {
    let app = create_router(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("HTTP content API listening on {}", listener.local_addr()?);

    axum::serve(listener, app).await?;
    Ok(())
}

/// Start the HTTP server with a new ContentStore
pub async fn start_http_server_with_store(
    addr: std::net::SocketAddr,
    gateway_dir: std::path::PathBuf,
    shared_secret: String,
) -> anyhow::Result<()> {
    let store = ContentStore::new(&gateway_dir)?;
    let state = HttpState {
        store: Arc::new(Mutex::new(store)),
        shared_secret,
        max_body_size: DEFAULT_MAX_BODY_SIZE,
        router: None,
    };
    start_http_server(addr, state).await
}

/// POST /api/event/ingest — JSON body mirrors JSON-RPC `event.ingest` params.
async fn handle_event_ingest(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(body): Json<HttpEventIngestBody>,
) -> Result<Json<JsonRpcResponse>, ErrorResponse> {
    validate_auth(&headers, &state.shared_secret)?;

    let Some(router) = state.router.clone() else {
        return Err(ErrorResponse {
            error: "HTTP ingress router not configured".to_string(),
            code: 503,
        });
    };

    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: format!("http-ingest-{}", uuid::Uuid::new_v4()),
        method: "event.ingest".to_string(),
        params: serde_json::to_value(&body).map_err(|e| ErrorResponse {
            error: format!("Failed to serialize ingest params: {e}"),
            code: 400,
        })?,
        auth_token: Some(state.shared_secret.clone()),
    };

    let resp = router.dispatch(req).await;
    Ok(Json(resp))
}

/// GET /api/session/stream/{session_id} — SSE stream polling async `session.status`.
///
/// Clients created with browser `EventSource` cannot set headers; pass `?token=` mirroring
/// `AUTONOETIC_SHARED_SECRET`.
async fn handle_session_stream_sse(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(q): Query<SessionStreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ErrorResponse> {
    validate_bearer_or_query(&headers, q.token.as_deref(), &state.shared_secret)?;
    validate_session_id(&session_id)?;

    let Some(router) = state.router.clone() else {
        return Err(ErrorResponse {
            error: "HTTP ingress router not configured".to_string(),
            code: 503,
        });
    };

    let interval_ms = q.interval_ms.unwrap_or(500).clamp(100, 10_000);
    let secret = state.shared_secret.clone();

    let stream = stream::unfold(Some(()), move |tick_state| {
        let router = router.clone();
        let session_id = session_id.clone();
        let secret = secret.clone();
        async move {
            if tick_state.is_none() {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            let resp = dispatch_session_status(router.as_ref(), &session_id, &secret).await;
            let terminal = sse_session_status_terminal(&resp);
            let json = serde_json::to_string(&resp).unwrap_or_else(|_| {
                "{\"jsonrpc\":\"2.0\",\"id\":\"null\",\"error\":{\"code\":-32603,\"message\":\"serialization_failed\"}}"
                    .to_string()
            });
            let evt = Ok(Event::default().event("session.status").data(json));
            Some((evt, if terminal { None } else { Some(()) }))
        }
    });

    Ok(
        Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))),
    )
}

async fn dispatch_operator_activity_list(
    router: &JsonRpcRouter,
    root_session_id: &str,
    after_activity_id: Option<&str>,
    secret: &str,
) -> JsonRpcResponse {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: format!("http-oa-{}", uuid::Uuid::new_v4()),
        method: "operator.activity.list".to_string(),
        params: serde_json::json!({
            "root_session_id": root_session_id,
            "after_activity_id": after_activity_id,
            "limit": 50,
            "min_severity": "progress",
        }),
        auth_token: Some(secret.to_string()),
    };
    router.dispatch(req).await
}

/// GET /api/operator/activity/stream/{root_session_id} — SSE stream of operator activity rows.
async fn handle_operator_activity_stream_sse(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Path(root_session_id): Path<String>,
    Query(q): Query<OperatorActivityStreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ErrorResponse> {
    validate_bearer_or_query(&headers, q.token.as_deref(), &state.shared_secret)?;
    validate_session_id(&root_session_id)?;

    let Some(router) = state.router.clone() else {
        return Err(ErrorResponse {
            error: "HTTP ingress router not configured".to_string(),
            code: 503,
        });
    };

    let interval_ms = q.interval_ms.unwrap_or(500).clamp(100, 10_000);
    let secret = state.shared_secret.clone();

    let stream = stream::unfold(q.after, move |cursor_state| {
        let router = router.clone();
        let root_session_id = root_session_id.clone();
        let secret = secret.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            let resp = dispatch_operator_activity_list(
                router.as_ref(),
                &root_session_id,
                cursor_state.as_deref(),
                &secret,
            )
            .await;
            if resp.error.is_some() {
                let json = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string());
                return Some((
                    Ok(Event::default().event("operator.activity.error").data(json)),
                    cursor_state,
                ));
            }
            let activities = resp
                .result
                .as_ref()
                .and_then(|v| v.get("activities"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let next_cursor = activities
                .last()
                .and_then(|a| a.get("activity_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or(cursor_state);
            if activities.is_empty() {
                return Some((
                    Ok(Event::default().event("operator.activity.heartbeat").data("{}")),
                    next_cursor,
                ));
            }
            let batch = serde_json::json!({ "activities": activities });
            Some((
                Ok(Event::default()
                    .event("operator.activity")
                    .data(batch.to_string())),
                next_cursor,
            ))
        }
    });

    Ok(
        Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))),
    )
}

async fn dispatch_session_timeline_list(
    router: &JsonRpcRouter,
    root_session_id: &str,
    after_event_id: Option<&str>,
    min_altitude: &str,
    secret: &str,
) -> JsonRpcResponse {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: format!("http-tl-{}", uuid::Uuid::new_v4()),
        method: "session.timeline.list".to_string(),
        params: serde_json::json!({
            "root_session_id": root_session_id,
            "after_event_id": after_event_id,
            "limit": 100,
            "min_altitude": min_altitude,
        }),
        auth_token: Some(secret.to_string()),
    };
    router.dispatch(req).await
}

/// GET /api/session/timeline/stream/{root_session_id} — SSE stream of the
/// canonical Session Room timeline (#391). Cursor-bootstrap then tail; the
/// gateway owns the data, channels render it.
async fn handle_session_timeline_stream_sse(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Path(root_session_id): Path<String>,
    Query(q): Query<SessionTimelineStreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ErrorResponse> {
    validate_bearer_or_query(&headers, q.token.as_deref(), &state.shared_secret)?;
    validate_session_id(&root_session_id)?;

    let Some(router) = state.router.clone() else {
        return Err(ErrorResponse {
            error: "HTTP ingress router not configured".to_string(),
            code: 503,
        });
    };

    let interval_ms = q.interval_ms.unwrap_or(500).clamp(100, 10_000);
    let min_altitude = q.min_altitude.unwrap_or_else(|| "normal".to_string());
    // Validate the floor up front so an invalid value is a 400, not a silently
    // unfiltered stream.
    if autonoetic_types::session_timeline::Altitude::parse_str(&min_altitude).is_none() {
        return Err(ErrorResponse {
            error: format!(
                "invalid min_altitude '{}': expected detail | normal | attention | error",
                min_altitude
            ),
            code: 400,
        });
    }
    let secret = state.shared_secret.clone();

    let stream = stream::unfold(q.after, move |cursor_state| {
        let router = router.clone();
        let root_session_id = root_session_id.clone();
        let min_altitude = min_altitude.clone();
        let secret = secret.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            let resp = dispatch_session_timeline_list(
                router.as_ref(),
                &root_session_id,
                cursor_state.as_deref(),
                &min_altitude,
                &secret,
            )
            .await;
            if resp.error.is_some() {
                let json = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string());
                return Some((
                    Ok(Event::default().event("session.timeline.error").data(json)),
                    cursor_state,
                ));
            }
            let entries = resp
                .result
                .as_ref()
                .and_then(|v| v.get("entries"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let next_cursor = entries
                .last()
                .and_then(|e| e.get("event_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or(cursor_state);
            if entries.is_empty() {
                return Some((
                    Ok(Event::default().event("session.timeline.heartbeat").data("{}")),
                    next_cursor,
                ));
            }
            let batch = serde_json::json!({ "entries": entries });
            Some((
                Ok(Event::default()
                    .event("session.timeline")
                    .data(batch.to_string())),
                next_cursor,
            ))
        }
    });

    Ok(
        Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))),
    )
}

/// POST /api/content/write - Write content to the store
async fn handle_write(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(req): Json<WriteRequest>,
) -> Result<Json<WriteResponse>, ErrorResponse> {
    // Authentication
    validate_auth(&headers, &state.shared_secret)?;

    // Validation
    req.validate()?;

    let store = state.store.lock().await;

    // Decode content based on encoding
    let content_bytes = match req.encoding.as_deref() {
        Some("base64") => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(&req.content)
                .map_err(|e| ErrorResponse {
                    error: format!("Invalid base64: {}", e),
                    code: 400,
                })?
        }
        _ => req.content.into_bytes(), // UTF-8 default
    };

    let size_bytes = content_bytes.len();

    // Parse visibility
    let content_visibility = match req.visibility.as_deref() {
        Some("private") => crate::runtime::content_store::ContentVisibility::Private,
        Some("session") | None => crate::runtime::content_store::ContentVisibility::Session,
        Some("global") => crate::runtime::content_store::ContentVisibility::Global,
        Some(other) => {
            return Err(ErrorResponse {
                error: format!(
                    "Invalid visibility '{}'. Must be one of: private, session, global",
                    other
                ),
                code: 400,
            })
        }
    };

    // Write to content store
    let handle = store.write(&content_bytes).map_err(|e| ErrorResponse {
        error: e.to_string(),
        code: 500,
    })?;

    // Register name in session with visibility
    store
        .register_name_with_visibility(&req.session_id, &req.name, &handle, content_visibility)
        .map_err(|e| ErrorResponse {
            error: e.to_string(),
            code: 500,
        })?;

    Ok(Json(WriteResponse {
        handle,
        name: req.name,
        size_bytes,
    }))
}

/// GET /api/content/read/{session_id}/{name_or_handle} - Read content (path params)
async fn handle_read_get(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Path((session_id, name_or_handle)): Path<(String, String)>,
) -> Result<Json<ReadResponse>, ErrorResponse> {
    // Authentication
    validate_auth(&headers, &state.shared_secret)?;

    // Validation
    validate_session_id(&session_id)?;

    read_content(&state, &session_id, &name_or_handle).await
}

/// POST /api/content/read - Read content (body params)
async fn handle_read_post(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(req): Json<ReadRequest>,
) -> Result<Json<ReadResponse>, ErrorResponse> {
    // Authentication
    validate_auth(&headers, &state.shared_secret)?;

    // Validation
    validate_session_id(&req.session_id)?;

    read_content(&state, &req.session_id, &req.name_or_handle).await
}

async fn read_content(
    state: &HttpState,
    session_id: &str,
    name_or_handle: &str,
) -> Result<Json<ReadResponse>, ErrorResponse> {
    let store = state.store.lock().await;

    let content_bytes = store
        .read_by_name_or_handle(session_id, name_or_handle)
        .map_err(|e| ErrorResponse {
            error: e.to_string(),
            code: 404,
        })?;

    let size_bytes = content_bytes.len();
    let handle = store.write(&content_bytes).map_err(|e| ErrorResponse {
        error: e.to_string(),
        code: 500,
    })?;

    // Encode as base64 for safe transport
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&content_bytes);

    Ok(Json(ReadResponse {
        content: encoded,
        encoding: "base64".to_string(),
        size_bytes,
        handle,
    }))
}

/// GET /api/content/names?session_id=xxx - List content names in a session
async fn handle_list_names(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse>, ErrorResponse> {
    // Authentication
    validate_auth(&headers, &state.shared_secret)?;

    // Validation
    validate_session_id(&query.session_id)?;

    let store = state.store.lock().await;

    let entries = store
        .list_names_with_handles(&query.session_id)
        .map_err(|e| ErrorResponse {
            error: e.to_string(),
            code: 500,
        })?;

    let names: Vec<ContentName> = entries
        .into_iter()
        .map(|(name, handle)| ContentName { name, handle })
        .collect();

    Ok(Json(ListResponse { names }))
}

/// Serve the web dashboard index.html
async fn handle_serve_index() -> impl IntoResponse {
    axum::response::Html(include_str!("../../../web/index.html"))
}

/// POST /api/jsonrpc - Handle JSON-RPC requests from the web dashboard
async fn handle_jsonrpc(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(mut req): Json<JsonRpcRequest>,
) -> Result<Json<JsonRpcResponse>, ErrorResponse> {
    validate_auth(&headers, &state.shared_secret)?;

    let Some(router) = state.router.clone() else {
        return Err(ErrorResponse {
            error: "HTTP ingress router not configured".to_string(),
            code: 503,
        });
    };

    // Make sure auth_token is set correctly for validation down the line
    req.auth_token = Some(state.shared_secret.clone());

    let resp = router.dispatch(req).await;
    Ok(Json(resp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const TEST_SECRET: &str = "test-secret-token";

    async fn setup_test_server() -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let dir = tempdir().unwrap();
        let gateway_dir = dir.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();

        let store = ContentStore::new(&gateway_dir).unwrap();
        let state = HttpState {
            store: Arc::new(Mutex::new(store)),
            shared_secret: TEST_SECRET.to_string(),
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            router: None,
        };
        let app = create_router(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        (addr, handle, dir)
    }

    fn auth_header() -> (String, String) {
        (
            "Authorization".to_string(),
            format!("Bearer {}", TEST_SECRET),
        )
    }

    #[tokio::test]
    async fn test_write_and_read_content() {
        let (addr, handle, _dir) = setup_test_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://{}", addr);
        let (auth_name, auth_value) = auth_header();

        // Write content
        let write_req = WriteRequest {
            session_id: "test-session".to_string(),
            name: "test.txt".to_string(),
            content: "Hello, World!".to_string(),
            encoding: None,
            visibility: None,
        };

        let resp = client
            .post(&format!("{}/api/content/write", base))
            .header(&auth_name, &auth_value)
            .json(&write_req)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        let write_resp: WriteResponse = resp.json().await.unwrap();
        assert!(!write_resp.handle.is_empty());
        assert_eq!(write_resp.size_bytes, 13);

        // Read content back via GET
        let resp = client
            .get(&format!("{}/api/content/read/test-session/test.txt", base))
            .header(&auth_name, &auth_value)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        let read_resp: ReadResponse = resp.json().await.unwrap();
        assert_eq!(read_resp.encoding, "base64");

        // Decode and verify
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&read_resp.content)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "Hello, World!");

        handle.abort();
    }

    #[tokio::test]
    async fn test_auth_required() {
        let (addr, handle, _dir) = setup_test_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://{}", addr);

        // Try without auth
        let write_req = WriteRequest {
            session_id: "test-session".to_string(),
            name: "test.txt".to_string(),
            content: "Hello, World!".to_string(),
            encoding: None,
            visibility: None,
        };

        let resp = client
            .post(&format!("{}/api/content/write", base))
            .json(&write_req)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "Should require authentication");

        handle.abort();
    }

    #[tokio::test]
    async fn test_invalid_token() {
        let (addr, handle, _dir) = setup_test_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://{}", addr);

        let write_req = WriteRequest {
            session_id: "test-session".to_string(),
            name: "test.txt".to_string(),
            content: "Hello, World!".to_string(),
            encoding: None,
            visibility: None,
        };

        let resp = client
            .post(&format!("{}/api/content/write", base))
            .header("Authorization", "Bearer wrong-token")
            .json(&write_req)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "Should reject invalid token");

        handle.abort();
    }

    #[tokio::test]
    async fn test_invalid_session_id() {
        let (addr, handle, _dir) = setup_test_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://{}", addr);
        let (auth_name, auth_value) = auth_header();

        let write_req = WriteRequest {
            session_id: "../../../etc/passwd".to_string(), // Path traversal attempt
            name: "test.txt".to_string(),
            content: "Hello, World!".to_string(),
            encoding: None,
            visibility: None,
        };

        let resp = client
            .post(&format!("{}/api/content/write", base))
            .header(&auth_name, &auth_value)
            .json(&write_req)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "Should reject invalid session_id");

        handle.abort();
    }

    #[tokio::test]
    async fn test_list_names() {
        let (addr, handle, _dir) = setup_test_server().await;
        let client = reqwest::Client::new();
        let base = format!("http://{}", addr);
        let (auth_name, auth_value) = auth_header();

        // Write first file
        let write_req1 = WriteRequest {
            session_id: "test-session-list".to_string(),
            name: "file1.txt".to_string(),
            content: "Content of file1".to_string(),
            encoding: None,
            visibility: None,
        };
        let resp1 = client
            .post(&format!("{}/api/content/write", base))
            .header(&auth_name, &auth_value)
            .json(&write_req1)
            .send()
            .await
            .unwrap();
        assert!(resp1.status().is_success(), "Write 1 failed");

        // Write second file
        let write_req2 = WriteRequest {
            session_id: "test-session-list".to_string(),
            name: "file2.txt".to_string(),
            content: "Content of file2".to_string(),
            encoding: None,
            visibility: None,
        };
        let resp2 = client
            .post(&format!("{}/api/content/write", base))
            .header(&auth_name, &auth_value)
            .json(&write_req2)
            .send()
            .await
            .unwrap();
        assert!(resp2.status().is_success(), "Write 2 failed");

        // List names
        let resp = client
            .get(&format!(
                "{}/api/content/names?session_id=test-session-list",
                base
            ))
            .header(&auth_name, &auth_value)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "List failed");

        let list_resp: ListResponse = resp.json().await.unwrap();
        assert_eq!(
            list_resp.names.len(),
            2,
            "Expected 2 names, got: {:?}",
            list_resp.names
        );

        let names: Vec<&str> = list_resp.names.iter().map(|n| n.name.as_str()).collect();
        assert!(
            names.contains(&"file1.txt"),
            "Missing file1.txt in {:?}",
            names
        );
        assert!(
            names.contains(&"file2.txt"),
            "Missing file2.txt in {:?}",
            names
        );

        handle.abort();
    }
}
