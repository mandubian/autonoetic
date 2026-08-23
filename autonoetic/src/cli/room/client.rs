//! Minimal JSON-RPC client to the running gateway (#392, P3.b).
//!
//! The Session Room is a *channel* — a client of the gateway API, not a direct
//! reader of `gateway.db` (Separation of Powers). This is the same newline-
//! delimited JSON-RPC-over-TCP transport the chat TUI uses.

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcResponse};
use autonoetic_gateway::server::transport::BoxedConnection;
use autonoetic_types::config::GatewayConfig;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::sync::Mutex;

/// The async path stores a type-erased transport connection (#1122): the
/// RoomClient is transport-agnostic on the Tokio side, so a Unix-socket or
/// in-process gateway transport drops in without touching call logic. The
/// sync TUI path keeps a plain blocking `TcpStream` — no nested runtime.
struct AsyncPersistedConn {
    reader: AsyncBufReader<tokio::io::ReadHalf<BoxedConnection>>,
    writer: tokio::io::WriteHalf<BoxedConnection>,
}

struct SyncPersistedConn {
    stream: TcpStream,
}

pub struct RoomClient {
    addr: String,
    token: String,
    /// Async path (`handle_room` drain / follow) — main Tokio runtime.
    conn: Mutex<Option<AsyncPersistedConn>>,
    /// Sync path (Session Room TUI) — plain blocking TCP, no nested runtime.
    sync_conn: StdMutex<Option<SyncPersistedConn>>,
}

impl Clone for RoomClient {
    fn clone(&self) -> Self {
        Self {
            addr: self.addr.clone(),
            token: self.token.clone(),
            conn: Mutex::new(None),
            sync_conn: StdMutex::new(None),
        }
    }
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
            sync_conn: StdMutex::new(None),
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
            sync_conn: StdMutex::new(None),
        }
    }

    /// Blocking RPC for the sync Session Room TUI. Uses a dedicated blocking
    /// TCP connection so the TUI never nests `block_on` inside `#[tokio::main]`.
    pub fn call_sync(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> anyhow::Result<serde_json::Value> {
        for attempt in 0..2 {
            match self.call_sync_once(method, &params, timeout) {
                Ok(value) => return Ok(value),
                Err(e) if attempt == 0 && is_transport_error(&e) => {
                    self.drop_sync_conn();
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("at most two sync call attempts")
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
            match self.call_on_conn_async(method, &params).await {
                Ok(value) => return Ok(value),
                Err(e) if attempt == 0 && is_transport_error(&e) => {
                    self.drop_async_conn().await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("at most two call attempts")
    }

    /// Like [`Self::call`], but fails instead of waiting indefinitely when the
    /// gateway does not respond (avoids freezing the Session Room TUI).
    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> anyhow::Result<serde_json::Value> {
        match tokio::time::timeout(timeout, self.call(method, params)).await {
            Ok(result) => result,
            Err(_) => anyhow::bail!(
                "{method} timed out after {}s (gateway not responding)",
                timeout.as_secs()
            ),
        }
    }

    fn call_sync_once(
        &self,
        method: &str,
        params: &serde_json::Value,
        timeout: Duration,
    ) -> anyhow::Result<serde_json::Value> {
        let mut guard = self
            .sync_conn
            .lock()
            .map_err(|_| anyhow::anyhow!("room sync connection mutex poisoned"))?;
        self.ensure_sync_conn(&mut guard)?;
        let conn = guard.as_mut().expect("sync connection established");

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: format!("room-{}", uuid::Uuid::new_v4()),
            method: method.to_string(),
            params: params.clone(),
            auth_token: Some(self.token.clone()),
        };
        let encoded = serde_json::to_string(&request)?;

        if conn.stream.set_write_timeout(Some(Duration::from_secs(5))).is_err() {
            *guard = None;
            anyhow::bail!("cannot set write timeout for {method}");
        }
        if let Err(e) = conn.stream.write_all(encoded.as_bytes()) {
            *guard = None;
            return Err(e.into());
        }
        if let Err(e) = conn.stream.write_all(b"\n") {
            *guard = None;
            return Err(e.into());
        }
        if let Err(e) = conn.stream.flush() {
            *guard = None;
            return Err(e.into());
        }

        if conn.stream.set_read_timeout(Some(timeout)).is_err() {
            *guard = None;
            anyhow::bail!("cannot set read timeout for {method}");
        }
        let mut reader = BufReader::new(
            conn.stream
                .try_clone()
                .map_err(|e| io::Error::other(e.to_string()))?,
        );
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                *guard = None;
                anyhow::bail!("gateway closed the connection with no response to {method}");
            }
            Ok(_) => {}
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut
                    || e.kind() == io::ErrorKind::WouldBlock =>
            {
                *guard = None;
                anyhow::bail!(
                    "{method} timed out after {}s (gateway not responding)",
                    timeout.as_secs()
                );
            }
            Err(e) => {
                *guard = None;
                return Err(e.into());
            }
        }

        decode_response(method, &line)
    }

    async fn call_on_conn_async(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let mut guard = self.conn.lock().await;
        self.ensure_async_conn(&mut guard).await?;
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

        decode_response(method, &line)
    }

    fn ensure_sync_conn(
        &self,
        guard: &mut Option<SyncPersistedConn>,
    ) -> anyhow::Result<()> {
        if guard.is_some() {
            return Ok(());
        }
        let addr: SocketAddr = self
            .addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid gateway addr {}: {}", self.addr, e))?;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| {
            anyhow::anyhow!("cannot reach gateway at {}: {}", self.addr, e)
        })?;
        let _ = stream.set_nodelay(true);
        *guard = Some(SyncPersistedConn { stream });
        Ok(())
    }

    async fn ensure_async_conn(
        &self,
        guard: &mut Option<AsyncPersistedConn>,
    ) -> anyhow::Result<()> {
        if guard.is_some() {
            return Ok(());
        }
        let stream = tokio::net::TcpStream::connect(&self.addr)
            .await
            .map_err(|e| anyhow::anyhow!("cannot reach gateway at {}: {}", self.addr, e))?;
        let _ = stream.set_nodelay(true);
        let conn: BoxedConnection = Box::new(stream);
        let (read_half, write_half) = tokio::io::split(conn);
        *guard = Some(AsyncPersistedConn {
            reader: AsyncBufReader::new(read_half),
            writer: write_half,
        });
        Ok(())
    }

    fn drop_sync_conn(&self) {
        if let Ok(mut guard) = self.sync_conn.lock() {
            *guard = None;
        }
    }

    async fn drop_async_conn(&self) {
        *self.conn.lock().await = None;
    }
}

fn decode_response(method: &str, line: &str) -> anyhow::Result<serde_json::Value> {
    let response: JsonRpcResponse = serde_json::from_str(line.trim_end())?;
    if let Some(err) = response.error {
        anyhow::bail!("{method} failed: {}", err.message);
    }
    Ok(response.result.unwrap_or(serde_json::Value::Null))
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
                | io::ErrorKind::TimedOut
        );
    }
    let msg = err.to_string();
    msg.contains("cannot reach gateway")
        || msg.contains("gateway closed the connection")
        || msg.contains("timed out")
        || msg.contains("connection")
}
