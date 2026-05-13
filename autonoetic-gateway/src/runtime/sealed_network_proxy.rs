//! HTTP proxy that intercepts outbound requests from sealed/recording
//! sandbox sessions and serves fixtures or `unfixtured_target` errors
//! (RFC scope 5.2b).
//!
//! The proxy binds to `127.0.0.1` on a random port. The caller (when 5.2c
//! ships) sets `HTTP_PROXY=http://127.0.0.1:<port>` in the sandbox's exec
//! environment so the artifact's HTTP client routes through it. The proxy
//! consults the per-session `FixtureLoader` and:
//!
//! - On fixture hit: returns the canned response.
//! - On fixture miss (Sealed): returns a 502 with the structured
//!   `unfixtured_target` envelope body.
//! - On fixture miss (Recording): forwards the request live, redacts
//!   credentials, writes the response as a new fixture, and serves the
//!   live response back to the sandbox.
//! - On CONNECT (HTTPS tunnelling): rejects with 502 + diagnostic. HTTPS
//!   termination is a future scope — see follow-up RFC §7 open question 4.
//!
//! Refs: docs/design/recording-mode-design.md §2.2.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Router,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use autonoetic_types::agent::SandboxNetworkPolicy;

use crate::sandbox::BwrapIsolationOverrides;

use crate::runtime::sealed_network::{
    decide_egress, parse_host_port, redact_fixture, unfixtured_envelope_body,
    write_recording_fixture, EgressDecision, FixtureLoader, FixtureRecord, RecordedRequest,
    RecordedResponse,
};

/// Handle to a running sealed-network proxy. Drop the handle (or call
/// `shutdown`) to stop the server.
pub struct SealedProxyHandle {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for SealedProxyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedProxyHandle")
            .field("addr", &self.addr)
            .finish()
    }
}

impl SealedProxyHandle {
    /// The address the proxy is bound to. Use this to set `HTTP_PROXY` in
    /// the sandbox's exec environment.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Convenience: the proxy URL as agents/HTTP clients expect to see it.
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stop the proxy. Idempotent — calling twice is a no-op.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for SealedProxyHandle {
    fn drop(&mut self) {
        // Best-effort: signal shutdown; we cannot await in Drop so the
        // axum server gracefully exits on its own when the channel
        // closes.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

#[derive(Clone)]
struct ProxyState {
    policy: SandboxNetworkPolicy,
    loader: Arc<FixtureLoader>,
    recording_dir: Option<PathBuf>,
    recording_session_id: Option<String>,
}

/// Start the proxy. Returns a handle once the listener is bound.
///
/// Sessions in `Normal` mode should not call this — it's only meaningful
/// for `Sealed` and `Recording`. For `Recording`, `recording_dir` is the
/// staging directory where captured fixtures are written.
pub async fn start_sealed_proxy(
    policy: SandboxNetworkPolicy,
    loader: Arc<FixtureLoader>,
    recording_dir: Option<PathBuf>,
    recording_session_id: Option<String>,
) -> anyhow::Result<SealedProxyHandle> {
    anyhow::ensure!(
        !matches!(policy, SandboxNetworkPolicy::Normal),
        "sealed proxy is only meaningful for Sealed or Recording policies; got Normal"
    );

    let state = ProxyState {
        policy,
        loader,
        recording_dir,
        recording_session_id,
    };

    let app = Router::new()
        .fallback(handle_request)
        .with_state(state);

    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let join = tokio::spawn(async move {
        let server = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            });
        if let Err(e) = server.await {
            tracing::warn!(
                target: "sealed_network_proxy",
                error = %e,
                "sealed proxy server exited with error"
            );
        }
    });

    tracing::info!(
        target: "sealed_network_proxy",
        addr = %addr,
        policy = ?policy,
        "sealed proxy listening"
    );

    Ok(SealedProxyHandle {
        addr,
        shutdown_tx: Some(shutdown_tx),
        join: Some(join),
    })
}

async fn handle_request(State(state): State<ProxyState>, req: Request) -> Response {
    // Reject CONNECT (HTTPS tunnelling) explicitly. Without HTTPS-MITM
    // (which needs a generated CA cert distributed into the sandbox)
    // the proxy cannot see plaintext, so it cannot consult fixtures.
    if req.method() == Method::CONNECT {
        return https_not_supported_response();
    }

    // Determine the target. Two shapes:
    //
    // 1. HTTP-proxy form: `GET http://api.example.com/v1/x HTTP/1.1`
    //    — request URI carries scheme + authority + path.
    // 2. Direct form: `GET /v1/x HTTP/1.1\nHost: api.example.com`
    //    — request URI is just the path; the Host header carries the target.
    //
    // urllib/requests/httpx use the proxy form when HTTP_PROXY is set.
    let uri = req.uri().clone();
    let (host, port, path) = match extract_target(&uri, req.headers()) {
        Ok(parts) => parts,
        Err(msg) => return bad_request(&msg),
    };

    let method = req.method().to_string();

    match decide_egress(
        state.policy,
        Some(state.loader.as_ref()),
        &host,
        port,
        &method,
        &path,
    ) {
        Ok(EgressDecision::Allow) => {
            // For sealed/recording mode this should be unreachable — only
            // Normal returns Allow. If we got here, the policy must be
            // Normal, but the proxy wouldn't have been started. Treat as
            // a configuration error.
            unreachable_proxy_state()
        }
        Ok(EgressDecision::Fixture(response)) => {
            fixture_to_response(response)
        }
        Ok(EgressDecision::Unfixtured { expected_path }) => {
            if matches!(state.policy, SandboxNetworkPolicy::Recording) {
                // Recording mode: forward live, capture, redact, write fixture.
                match forward_and_capture(
                    req,
                    &host,
                    port,
                    &method,
                    &path,
                    &state,
                )
                .await
                {
                    Ok(response) => response,
                    Err(e) => {
                        tracing::error!(
                            target: "sealed_network_proxy",
                            host = %host,
                            port = ?port,
                            method = %method,
                            path = %path,
                            error = %e,
                            "recording proxy failed to forward request"
                        );
                        let body = serde_json::json!({
                            "ok": false,
                            "error_type": "fatal",
                            "message": format!("recording proxy forward error: {}", e),
                        })
                        .to_string();
                        (StatusCode::BAD_GATEWAY, [("content-type", "application/json")], body)
                            .into_response()
                    }
                }
            } else {
                // Sealed mode: return unfixtured error.
                let body =
                    unfixtured_envelope_body(&host, port, &method, &path, &expected_path);
                tracing::warn!(
                    target: "sealed_network_proxy",
                    host = %host,
                    port = ?port,
                    method = %method,
                    path = %path,
                    fixture = %expected_path,
                    "sealed-mode miss: returning unfixtured_target envelope"
                );
                (StatusCode::BAD_GATEWAY, [("content-type", "application/json")], body)
                    .into_response()
            }
        }
        Err(e) => {
            let body = serde_json::json!({
                "ok": false,
                "error_type": "fatal",
                "message": format!("sealed proxy internal error: {}", e),
            })
            .to_string();
            (StatusCode::INTERNAL_SERVER_ERROR, [("content-type", "application/json")], body)
                .into_response()
        }
    }
}

/// Forward a request to the live target, capture the response, redact it,
/// write a fixture to the recording staging directory, and return the
/// live response to the sandbox.
async fn forward_and_capture(
    req: Request,
    host: &str,
    port: Option<u16>,
    method: &str,
    path: &str,
    state: &ProxyState,
) -> anyhow::Result<Response> {
    // Build the target URL.
    let target_url = match port {
        Some(p) => format!("http://{}:{}{}", host, p, path),
        None => format!("http://{}{}", host, path),
    };

    // Extract headers before consuming the body.
    let req_headers = build_request_header_map(req.headers());

    // Read the request body.
    let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await?;
    let body_str = if body_bytes.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&body_bytes).to_string())
    };

    // Forward the request using reqwest.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut req_builder = client.request(
        reqwest::Method::from_bytes(method.as_bytes())?,
        &target_url,
    );

    // Copy request headers (but not hop-by-hop or proxy headers).
    for (name, value) in &req_headers {
        req_builder = req_builder.header(name.as_str(), value.as_str());
    }

    if let Some(ref body) = body_str {
        req_builder = req_builder.body(body.clone());
    }

    let resp = req_builder.send().await?;
    let resp_status = resp.status().as_u16();

    // Read the response headers and body.
    let mut resp_headers = std::collections::BTreeMap::new();
    for (name, value) in resp.headers() {
        if let Ok(v) = value.to_str() {
            resp_headers.insert(name.to_string(), v.to_string());
        }
    }
    let resp_body = resp.text().await.unwrap_or_default();

    // Build the fixture record.
    let mut record = FixtureRecord {
        request: RecordedRequest {
            method: method.to_uppercase(),
            url: target_url.clone(),
            headers: req_headers,
            body: body_str,
        },
        response: RecordedResponse {
            status: resp_status,
            headers: resp_headers,
            body: resp_body.clone(),
        },
        recorded_at: chrono::Utc::now().to_rfc3339(),
        redacted: Vec::new(),
    };

    // Redact credentials.
    let redacted_fields = redact_fixture(&mut record);

    // Write the fixture to the staging directory.
    if let Some(ref staging_dir) = state.recording_dir {
        if let Err(e) = write_recording_fixture(
            staging_dir,
            host,
            port,
            method,
            path,
            &record,
        ) {
            tracing::warn!(
                target: "sealed_network_proxy",
                host = %host,
                path = %path,
                error = %e,
                "failed to write recording fixture"
            );
        }
    }

    if !redacted_fields.is_empty() {
        tracing::info!(
            target: "sealed_network_proxy",
            host = %host,
            path = %path,
            redacted = ?redacted_fields,
            "recorded and redacted fixture"
        );
    } else {
        tracing::info!(
            target: "sealed_network_proxy",
            host = %host,
            path = %path,
            "recorded fixture (no redactions needed)"
        );
    }

    // Build and return the live response to the sandbox.
    let mut response = Response::new(axum::body::Body::from(resp_body));
    *response.status_mut() = StatusCode::from_u16(resp_status).unwrap_or(StatusCode::OK);
    for (name, value) in &record.response.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value),
        ) {
            response.headers_mut().insert(n, v);
        }
    }
    Ok(response)
}

/// Build a BTreeMap of request headers from the original request (excluding
/// hop-by-hop headers that should not be forwarded).
fn build_request_header_map(headers: &HeaderMap) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for (name, value) in headers {
        let n = name.as_str().to_lowercase();
        // Skip hop-by-hop and proxy headers.
        match n.as_str() {
            "host" | "connection" | "proxy-connection" | "keep-alive"
            | "transfer-encoding" | "te" | "upgrade" | "proxy-authorization" => continue,
            _ => {}
        }
        if let Ok(v) = value.to_str() {
            map.insert(n, v.to_string());
        }
    }
    map
}

/// Extract `(host, port, path)` from either the request URI (proxy form)
/// or the URI path + `Host:` header (direct form).
fn extract_target(
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(String, Option<u16>, String), String> {
    if let Some(authority) = uri.authority() {
        // Proxy form. Authority carries host[:port]; path comes from the URI.
        let host = authority.host().to_string();
        let port = authority.port_u16();
        let path = uri
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());
        return Ok((host, port, path));
    }
    // Direct form. Read Host header.
    let host_header = headers
        .get("host")
        .ok_or_else(|| "request has no authority and no Host header".to_string())?
        .to_str()
        .map_err(|_| "Host header is not valid UTF-8".to_string())?;
    let (host, port) = parse_host_port(host_header);
    let path = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    Ok((host, port, path))
}

fn fixture_to_response(
    fixture: crate::runtime::sealed_network::FixtureResponse,
) -> Response {
    let status = StatusCode::from_u16(fixture.status).unwrap_or(StatusCode::OK);
    let mut headers = HeaderMap::new();
    for (name, value) in fixture.headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value),
        ) {
            headers.insert(n, v);
        }
    }
    if !headers.contains_key("content-type") {
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/octet-stream"),
        );
    }
    (status, headers, fixture.body).into_response()
}

fn bad_request(msg: &str) -> Response {
    let body = serde_json::json!({
        "ok": false,
        "error_type": "validation",
        "message": msg,
    })
    .to_string();
    (StatusCode::BAD_REQUEST, [("content-type", "application/json")], body).into_response()
}

fn https_not_supported_response() -> Response {
    let body = serde_json::json!({
        "ok": false,
        "error_type": "unfixtured_target",
        "message": "Sealed-network sandbox: HTTPS (CONNECT) is not supported by the proxy yet. \
                    Use HTTP for fixture-driven evaluation, or seed a CA-cert pipeline for HTTPS \
                    (future RFC work).",
    })
    .to_string();
    (StatusCode::BAD_GATEWAY, [("content-type", "application/json")], body).into_response()
}

fn unreachable_proxy_state() -> Response {
    let body = serde_json::json!({
        "ok": false,
        "error_type": "fatal",
        "message": "sealed proxy reached an Allow decision — this indicates the proxy was \
                    started for a Normal policy and should not have been routed to. Report this \
                    as a gateway bug.",
    })
    .to_string();
    (StatusCode::INTERNAL_SERVER_ERROR, [("content-type", "application/json")], body)
        .into_response()
}

// ---------------------------------------------------------------------------
// Sandbox-exec setup helper (RFC scope 5.2c — advisory layer)
// ---------------------------------------------------------------------------
//
// Called by native tools that build a bubblewrap exec (`artifact_exec`,
// `sandbox_exec`) before they spawn the sandbox. When the agent's manifest
// declares `Sealed` or `Recording`:
//
// 1. Starts the proxy with a FixtureLoader rooted at `artifact_root`.
// 2. Injects `HTTP_PROXY`, `HTTPS_PROXY`, lowercase variants, and empty
//    `NO_PROXY` into the sandbox's environment so HTTP-clients route
//    every request through the proxy.
// 3. Forces `overrides.share_net = true` so the sandboxed process can
//    reach the proxy on host loopback. (The enforcing seal — kernel
//    netns + nftables transparent redirect — is a future scope; the
//    advisory layer here only catches HTTP_PROXY-aware clients.)
//
// Returns the proxy handle. The caller MUST drop it (or call
// `shutdown_sealed_proxy`) after the exec returns to free the listener.

/// Async core. Use from async contexts directly.
pub async fn setup_sealed_proxy_for_exec_async(
    policy: SandboxNetworkPolicy,
    artifact_root: PathBuf,
    extra_env: &mut Vec<(String, String)>,
    overrides: &mut BwrapIsolationOverrides,
    gateway_dir: Option<&Path>,
    session_id: Option<&str>,
) -> anyhow::Result<Option<SealedProxyHandle>> {
    if matches!(policy, SandboxNetworkPolicy::Normal) {
        return Ok(None);
    }

    // Derive recording staging directory from gateway_dir + session_id when Recording.
    let (recording_dir, recording_session_id): (Option<PathBuf>, Option<String>) =
        if matches!(policy, SandboxNetworkPolicy::Recording) {
            if let (Some(gw), Some(sid)) = (gateway_dir, session_id) {
                let dir = gw.join("recordings").join(sid).join("fixtures");
                std::fs::create_dir_all(&dir)?;
                (Some(dir), Some(sid.to_string()))
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

    let loader = Arc::new(FixtureLoader::new(artifact_root));
    let handle = start_sealed_proxy(policy, loader, recording_dir, recording_session_id).await?;
    let proxy_url = handle.proxy_url();

    // Inject standard env vars (most HTTP clients respect at least one
    // of these). The lowercase form is honoured by some libraries; the
    // uppercase form by others. NO_PROXY is set empty so libs that
    // default to bypassing localhost route everything through us.
    extra_env.push(("HTTP_PROXY".to_string(), proxy_url.clone()));
    extra_env.push(("HTTPS_PROXY".to_string(), proxy_url.clone()));
    extra_env.push(("http_proxy".to_string(), proxy_url.clone()));
    extra_env.push(("https_proxy".to_string(), proxy_url.clone()));
    extra_env.push(("NO_PROXY".to_string(), String::new()));
    extra_env.push(("no_proxy".to_string(), String::new()));

    // The proxy lives on host loopback. The sandbox needs network
    // sharing to reach it. (When 5.2c-enforcing lands, this is replaced
    // by a private network namespace with a forwarded loopback so the
    // proxy is the *only* reachable target.)
    overrides.share_net = true;

    tracing::info!(
        target: "sealed_network_proxy",
        policy = ?policy,
        proxy_url = %proxy_url,
        artifact_root = ?handle.addr(),
        "advisory sealed-proxy setup complete; HTTP_PROXY injected into sandbox env"
    );

    Ok(Some(handle))
}

/// Sync wrapper for `NativeTool::execute` callers. Bridges to the
/// existing tokio runtime via `block_on_http`'s pattern.
pub fn setup_sealed_proxy_for_exec(
    policy: SandboxNetworkPolicy,
    artifact_root: PathBuf,
    extra_env: &mut Vec<(String, String)>,
    overrides: &mut BwrapIsolationOverrides,
    gateway_dir: Option<&Path>,
    session_id: Option<&str>,
) -> anyhow::Result<Option<SealedProxyHandle>> {
    if matches!(policy, SandboxNetworkPolicy::Normal) {
        return Ok(None);
    }
    if overrides.force_network_off {
        tracing::warn!(
            target: "sealed_network_proxy",
            ?policy,
            "force_network_off is set — sealed proxy would be unreachable. Skipping proxy setup."
        );
        return Ok(None);
    }
    // We need a mutable-reference-safe block_on. Inline the pattern from
    // `block_on_http` since we capture &mut.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| {
            handle.block_on(setup_sealed_proxy_for_exec_async(
                policy,
                artifact_root,
                extra_env,
                overrides,
                gateway_dir,
                session_id,
            ))
        })
    } else {
        tokio::runtime::Runtime::new()?.block_on(setup_sealed_proxy_for_exec_async(
            policy,
            artifact_root,
            extra_env,
            overrides,
            gateway_dir,
            session_id,
        ))
    }
}

/// Tear down a proxy returned by `setup_sealed_proxy_for_exec`. Idempotent —
/// passing `None` is a no-op.
pub fn shutdown_sealed_proxy(handle: Option<SealedProxyHandle>) {
    let Some(h) = handle else { return };
    if let Ok(rt_handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| {
            rt_handle.block_on(async move { h.shutdown().await })
        });
    } else if let Ok(rt) = tokio::runtime::Runtime::new() {
        rt.block_on(async move { h.shutdown().await });
    } else {
        // Last resort: Drop's best-effort abort fires.
        drop(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::capability::Capability;
    use tempfile::tempdir;

    #[tokio::test(flavor = "multi_thread")]
    async fn setup_returns_none_for_normal_policy() {
        let dir = tempdir().unwrap();
        let mut env = Vec::new();
        let mut overrides = BwrapIsolationOverrides::from_capabilities(&[]);
        let handle = setup_sealed_proxy_for_exec_async(
            SandboxNetworkPolicy::Normal,
            dir.path().to_path_buf(),
            &mut env,
            &mut overrides,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(handle.is_none());
        assert!(env.is_empty(), "normal policy must not touch env");
        assert!(!overrides.share_net, "normal policy must not force share_net");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn setup_injects_env_and_forces_share_net_for_sealed() {
        let dir = tempdir().unwrap();
        let mut env: Vec<(String, String)> = vec![("EXISTING".into(), "ok".into())];
        let mut overrides =
            BwrapIsolationOverrides::from_capabilities(&[Capability::ReadAccess {
                scopes: vec!["self.*".into()],
            }]);
        let handle = setup_sealed_proxy_for_exec_async(
            SandboxNetworkPolicy::Sealed,
            dir.path().to_path_buf(),
            &mut env,
            &mut overrides,
            None,
            None,
        )
        .await
        .unwrap()
        .expect("sealed must return a handle");

        // Existing env preserved.
        assert!(env.iter().any(|(k, v)| k == "EXISTING" && v == "ok"));

        // Proxy env vars injected for both case conventions.
        for key in &["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            let v = env
                .iter()
                .find_map(|(k, v)| (k == key).then_some(v.as_str()))
                .unwrap_or_else(|| panic!("expected {key} in env: {env:?}"));
            assert!(v.starts_with("http://127.0.0.1:"), "{key} should be host-loopback URL: {v}");
        }

        // NO_PROXY explicitly empty so libs that auto-bypass localhost
        // still route through the proxy.
        for key in &["NO_PROXY", "no_proxy"] {
            let v = env
                .iter()
                .find_map(|(k, v)| (k == key).then_some(v.as_str()));
            assert_eq!(v, Some(""), "{key} should be set empty");
        }

        // share_net forced so the sandbox can reach host loopback.
        assert!(overrides.share_net, "sealed mode must force share_net=true");

        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn setup_returns_handle_for_recording_too() {
        // Recording mode shares the proxy mechanism with Sealed — the
        // 5.3 differentiator (live capture on miss) is downstream of
        // the proxy decision, not of the setup helper.
        let dir = tempdir().unwrap();
        let mut env = Vec::new();
        let mut overrides = BwrapIsolationOverrides::from_capabilities(&[]);
        let handle = setup_sealed_proxy_for_exec_async(
            SandboxNetworkPolicy::Recording,
            dir.path().to_path_buf(),
            &mut env,
            &mut overrides,
            None,
            None,
        )
        .await
        .unwrap()
        .expect("recording must return a handle");
        assert!(env.iter().any(|(k, _)| k == "HTTP_PROXY"));
        assert!(overrides.share_net);
        handle.shutdown().await;
    }
}
