//! Minimal JSON-RPC client to the running gateway (#392, P3.b).
//!
//! The Session Room is a *channel* — a client of the gateway API, not a direct
//! reader of `gateway.db` (Separation of Powers). This is the same newline-
//! delimited JSON-RPC-over-TCP transport the chat TUI uses.

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcResponse};
use autonoetic_types::config::GatewayConfig;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub struct RoomClient {
    addr: String,
    token: String,
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
        })
    }

    /// One JSON-RPC round-trip. Returns the `result` value, or an error carrying
    /// the gateway's message (connect failure, auth, or method error).
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let mut stream = TcpStream::connect(&self.addr)
            .await
            .map_err(|e| anyhow::anyhow!("cannot reach gateway at {}: {}", self.addr, e))?;

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: format!("room-{}", uuid::Uuid::new_v4()),
            method: method.to_string(),
            params,
            auth_token: Some(self.token.clone()),
        };
        let encoded = serde_json::to_string(&request)?;
        stream.write_all(encoded.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut line = String::new();
        let mut reader = BufReader::new(stream);
        if reader.read_line(&mut line).await? == 0 {
            anyhow::bail!("gateway closed the connection with no response to {method}");
        }
        let response: JsonRpcResponse = serde_json::from_str(line.trim_end())?;
        if let Some(err) = response.error {
            anyhow::bail!("{method} failed: {}", err.message);
        }
        Ok(response.result.unwrap_or_else(|| serde_json::json!({})))
    }
}
