//! Minimal JSON-RPC client to the running gateway (#392, P3.b).
//!
//! The Session Room is a *channel* — a client of the gateway API, not a direct
//! reader of `gateway.db` (Separation of Powers). This is the same newline-
//! delimited JSON-RPC-over-TCP transport the chat TUI uses.

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcResponse};
use autonoetic_types::config::GatewayConfig;
use std::io;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

struct PersistedConn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

pub struct RoomClient {
    addr: String,
    token: String,
    conn: Mutex<Option<PersistedConn>>,
}

impl RoomClient {
    pub fn from_config(config: &GatewayConfig) -> anyhow::Result<Self> {
        let token = std::env::var("AUTONOETIC_SHARED_SECRET").map_err(|_| {
            anyhow::anyhow!(
                "Missing AUTONOETIC_SHARED_SECRET — the Session Room reaches the gateway over \
                 JSON-RPC and needs the ingress auth token (start the gateway first)."
            )
        })?;
        Ok(Self {
            addr: format!("127.0.0.1:{}", config.port),
            token,
            conn: Mutex::new(None),
        })
    }

    /// A non-connecting client for unit tests of pure call sites (paths that
    /// return before any RPC). Calling `.call()` on it will fail to connect.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            addr: "127.0.0.1:0".to_string(),
            token: "test".to_string(),
            conn: Mutex::new(None),
        }
    }

    /// One JSON-RPC round-trip. Returns the `result` value, or an error carrying
    /// the gateway's message (connect failure, auth, or method error).
    ///
    /// Reuses a single TCP connection across calls; reconnects once on transport
    /// failure (the gateway keeps connections open for multiple requests).
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        for attempt in 0..2 {
            match self.call_on_conn(method, &params).await {
                Ok(value) => return Ok(value),
                Err(e) if attempt == 0 && is_transport_error(&e) => {
                    self.drop_conn().await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("at most two call attempts")
    }

    async fn call_on_conn(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let mut guard = self.conn.lock().await;
        self.ensure_conn(&mut guard).await?;
        let conn = guard.as_mut().expect("connection established");

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: format!("room-{}", uuid::Uuid::new_v4()),
            method: method.to_string(),
            params: params.clone(),
            auth_token: Some(self.token.clone()),
        };
        let encoded = serde_json::to_string(&request)?;

        if let Err(e) = conn.writer.write_all(encoded.as_bytes()).await {
            *guard = None;
            return Err(e.into());
        }
        if let Err(e) = conn.writer.write_all(b"\n").await {
            *guard = None;
            return Err(e.into());
        }
        if let Err(e) = conn.writer.flush().await {
            *guard = None;
            return Err(e.into());
        }

        let mut line = String::new();
        match conn.reader.read_line(&mut line).await {
            Ok(0) => {
                *guard = None;
                anyhow::bail!("gateway closed the connection with no response to {method}");
            }
            Ok(_) => {}
            Err(e) => {
                *guard = None;
                return Err(e.into());
            }
        }

        let response: JsonRpcResponse = serde_json::from_str(line.trim_end())?;
        if let Some(err) = response.error {
            anyhow::bail!("{method} failed: {}", err.message);
        }
        // A JSON `null` result deserializes to `None`; preserve null-vs-empty-object
        // semantics by returning `Value::Null` rather than substituting `{}`.
        Ok(response.result.unwrap_or(serde_json::Value::Null))
    }

    async fn ensure_conn(
        &self,
        guard: &mut Option<PersistedConn>,
    ) -> anyhow::Result<()> {
        if guard.is_some() {
            return Ok(());
        }
        let stream = TcpStream::connect(&self.addr)
            .await
            .map_err(|e| anyhow::anyhow!("cannot reach gateway at {}: {}", self.addr, e))?;
        let _ = stream.set_nodelay(true);
        let (read_half, write_half) = stream.into_split();
        *guard = Some(PersistedConn {
            reader: BufReader::new(read_half),
            writer: write_half,
        });
        Ok(())
    }

    async fn drop_conn(&self) {
        *self.conn.lock().await = None;
    }
}

fn is_transport_error(err: &anyhow::Error) -> bool {
    if let Some(io_err) = err.downcast_ref::<io::Error>() {
        return matches!(
            io_err.kind(),
            io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::NotConnected
        );
    }
    let msg = err.to_string();
    msg.contains("cannot reach gateway")
        || msg.contains("gateway closed the connection")
        || msg.contains("connection")
}
