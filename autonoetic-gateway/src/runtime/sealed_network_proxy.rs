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
//! - On CONNECT (HTTPS tunnelling): rejects with 502 + diagnostic. HTTPS
//!   termination is a future scope — see follow-up RFC §7 open question 4.
//!
//! Refs: docs/design/sealed-network-evaluation-plan.md §3.2.

use std::net::SocketAddr;
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

use crate::runtime::sealed_network::{
    decide_egress, parse_host_port, unfixtured_envelope_body, EgressDecision, FixtureLoader,
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
}

/// Start the proxy. Returns a handle once the listener is bound.
///
/// Sessions in `Normal` mode should not call this — it's only meaningful
/// for `Sealed` and `Recording`. Recording-on-miss live capture is not
/// implemented here; this proxy treats miss in either mode the same
/// (return `unfixtured_target`) and the 5.3 scope will extend the miss
/// path for `Recording`.
pub async fn start_sealed_proxy(
    policy: SandboxNetworkPolicy,
    loader: Arc<FixtureLoader>,
) -> anyhow::Result<SealedProxyHandle> {
    anyhow::ensure!(
        !matches!(policy, SandboxNetworkPolicy::Normal),
        "sealed proxy is only meaningful for Sealed or Recording policies; got Normal"
    );

    let state = ProxyState { policy, loader };

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
