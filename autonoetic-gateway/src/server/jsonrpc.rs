//! JSON-RPC ingress listener.
//!
//! Transport-agnostic since #1122: the accept loop runs on any
//! [`TransportListener`](crate::server::transport::TransportListener) and the
//! connection handler owns a type-erased
//! [`Connection`](crate::server::transport::Connection) — TCP in production,
//! in-memory in tests.

use crate::router::{JsonRpcRequest, JsonRpcResponse, JsonRpcRouter};
use crate::server::transport::{BoxedConnection, TcpListenerAdapter, TransportListener};
use std::net::SocketAddr;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Start a line-delimited JSON-RPC server over TCP.
pub async fn start_jsonrpc_server(
    listen_addr: SocketAddr,
    router: JsonRpcRouter,
    required_auth_token: Option<String>,
) -> anyhow::Result<()> {
    let listener = TcpListenerAdapter::bind(listen_addr).await?;
    serve_jsonrpc_listener(listener, router, required_auth_token).await
}

pub(crate) async fn serve_jsonrpc_listener<L: TransportListener>(
    mut listener: L,
    router: JsonRpcRouter,
    required_auth_token: Option<String>,
) -> anyhow::Result<()> {
    tracing::info!("JSON-RPC server listening on {}", listener.local_addr()?);

    loop {
        let (conn, peer_addr) = listener.accept().await?;
        let router = router.clone();
        let required_auth_token = required_auth_token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(conn, router, required_auth_token).await {
                tracing::warn!(peer = %peer_addr, error = %e, "JSON-RPC client disconnected");
            }
        });
    }
}

fn constant_time_str_eq(a: &str, b: &str) -> bool {
    subtle::ConstantTimeEq::ct_eq(a.as_bytes(), b.as_bytes()).into()
}

fn is_authorized_request(req: &JsonRpcRequest, required_auth_token: Option<&str>) -> bool {
    match required_auth_token {
        Some(expected) => req
            .auth_token
            .as_deref()
            .map_or(false, |provided| constant_time_str_eq(provided, expected)),
        None => true,
    }
}

async fn handle_connection(
    conn: BoxedConnection,
    router: JsonRpcRouter,
    required_auth_token: Option<String>,
) -> anyhow::Result<()> {
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(req) => {
                if !is_authorized_request(&req, required_auth_token.as_deref()) {
                    JsonRpcResponse::error(req.id.clone(), -32001, "Unauthorized JSON-RPC request")
                } else {
                    router.dispatch(req).await
                }
            }
            Err(e) => {
                JsonRpcResponse::error("null".to_string(), -32700, format!("Parse error: {}", e))
            }
        };

        let encoded = serde_json::to_string(&response)?;
        write_half.write_all(encoded.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::config::GatewayConfig;
    use crate::server::transport::{memory_transport, TcpListenerAdapter};
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    fn test_router() -> (TempDir, JsonRpcRouter) {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let router = JsonRpcRouter::new(
            GatewayConfig {
                runtime_dir: temp.path().join("agents").join(".gateway"),
                agents_dir: temp.path().join("agents"),
                ..GatewayConfig::default()
            },
            None,
        );
        (temp, router)
    }

    #[tokio::test]
    async fn test_jsonrpc_tcp_ping_roundtrip() {
        let (_temp, router) = test_router();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener
            .local_addr()
            .expect("listener should expose local addr");
        let server = tokio::spawn(async move {
            serve_jsonrpc_listener(TcpListenerAdapter::new(listener), router, None)
                .await
                .expect("server should run");
        });

        let stream = TcpStream::connect(addr)
            .await
            .expect("client should connect");
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "ping",
            "params": {}
        });

        write_half
            .write_all(format!("{}\n", request).as_bytes())
            .await
            .expect("request should write");

        let line = lines
            .next_line()
            .await
            .expect("response should read")
            .expect("response line should exist");
        let response: JsonRpcResponse =
            serde_json::from_str(&line).expect("response should decode");

        assert_eq!(response.result, Some(serde_json::json!("pong")));
        assert!(response.error.is_none());

        server.abort();
    }

    #[tokio::test]
    async fn test_jsonrpc_tcp_agent_spawn_unknown_agent() {
        let (_temp, router) = test_router();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener
            .local_addr()
            .expect("listener should expose local addr");
        let server = tokio::spawn(async move {
            serve_jsonrpc_listener(TcpListenerAdapter::new(listener), router, None)
                .await
                .expect("server should run");
        });

        let stream = TcpStream::connect(addr)
            .await
            .expect("client should connect");
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "agent_spawn",
            "params": {
                "agent_id": "missing",
                "message": "hello"
            }
        });

        write_half
            .write_all(format!("{}\n", request).as_bytes())
            .await
            .expect("request should write");

        let line = lines
            .next_line()
            .await
            .expect("response should read")
            .expect("response line should exist");
        let response: JsonRpcResponse =
            serde_json::from_str(&line).expect("response should decode");

        assert!(response.result.is_none());
        let msg = &response.error.as_ref().expect("error should exist").message;
        assert!(
            msg.contains("not found") || msg.contains("GatewayStore is required"),
            "unexpected error: {msg}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn test_jsonrpc_tcp_rejects_missing_auth_token_when_required() {
        let (_temp, router) = test_router();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let addr = listener
            .local_addr()
            .expect("listener should expose local addr");
        let server = tokio::spawn(async move {
            serve_jsonrpc_listener(TcpListenerAdapter::new(listener), router, Some("test-secret".to_string()))
                .await
                .expect("server should run");
        });

        let stream = TcpStream::connect(addr)
            .await
            .expect("client should connect");
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "3",
            "method": "ping",
            "params": {}
        });

        write_half
            .write_all(format!("{}\n", request).as_bytes())
            .await
            .expect("request should write");

        let line = lines
            .next_line()
            .await
            .expect("response should read")
            .expect("response line should exist");
        let response: JsonRpcResponse =
            serde_json::from_str(&line).expect("response should decode");

        assert!(response.result.is_none());
        let err = response.error.expect("error should exist");
        assert_eq!(err.code, -32001);
        assert!(err.message.contains("Unauthorized"));

        server.abort();
    }

    /// #1122: the accept loop and handler are transport-agnostic — the same
    /// `serve_jsonrpc_listener` that fronts TCP in production runs here over
    /// an in-memory pair, with no sockets bound. This is the pluggability
    /// proof for the transport seam (and the pattern future Unix-socket or
    /// TLS listeners would use).
    #[tokio::test]
    async fn test_jsonrpc_serves_over_memory_transport() {
        let (_temp, router) = test_router();
        let (listener, connector) = memory_transport();
        let server = tokio::spawn(async move {
            serve_jsonrpc_listener(listener, router, Some("secret".to_string()))
                .await
                .expect("server should run");
        });

        let client = connector.connect().await.expect("client should connect");
        let (read_half, mut write_half) = tokio::io::split(client);
        let mut lines = BufReader::new(read_half).lines();

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "m1",
            "method": "ping",
            "params": {},
            "auth_token": "secret"
        });
        write_half
            .write_all(format!("{}\n", request).as_bytes())
            .await
            .expect("request should write");

        let line = lines
            .next_line()
            .await
            .expect("response should read")
            .expect("response line should exist");
        let response: JsonRpcResponse =
            serde_json::from_str(&line).expect("response should decode");
        assert_eq!(response.result, Some(serde_json::json!("pong")));
        assert!(response.error.is_none());

        server.abort();
    }
}
