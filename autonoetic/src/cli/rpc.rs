//! Minimal sync JSON-RPC client for CLI operator commands (#1119).
//!
//! The CLI is a client of the gateway API, not a direct reader of
//! `gateway.db` (Separation of Powers) — same newline-delimited JSON-RPC
//! over TCP transport the Session Room and chat TUI use. One-shot
//! connection per call: CLI commands make a handful of calls at most, and a
//! fresh connection avoids lifetime coupling with long-running commands.
//!
//! Commands using this client require a running gateway
//! (`autonoetic gateway start`); they no longer work offline against a
//! stopped gateway's SQLite file.

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcResponse};
use autonoetic_types::config::GatewayConfig;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::time::Duration;

pub struct GatewayRpc {
    addr: SocketAddr,
    token: Option<String>,
}

impl GatewayRpc {
    pub fn from_config(config: &GatewayConfig) -> anyhow::Result<Self> {
        let token = std::env::var("AUTONOETIC_SHARED_SECRET").ok();
        Ok(Self {
            addr: SocketAddr::from(([127, 0, 0, 1], config.port)),
            token,
        })
    }

    /// One-shot RPC: connect, send, read a single response line.
    pub fn call(&self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let stream = std::net::TcpStream::connect_timeout(&self.addr, Duration::from_secs(5))
            .map_err(|e| {
            anyhow::anyhow!(
                "cannot reach gateway at {} — is it running? Operator commands speak \
                 JSON-RPC to the gateway, they no longer read gateway.db directly. \
                 Start it with `autonoetic gateway start` ({e})",
                self.addr
            )
        })?;
        let _ = stream.set_nodelay(true);
        let mut stream = stream;
        let id = format!("cli-{}", method);
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            method: method.to_string(),
            params,
            auth_token: self.token.clone(),
        };
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        reader.read_line(&mut response_line)?;
        let response: JsonRpcResponse = serde_json::from_str(response_line.trim())?;
        if let Some(err) = response.error {
            anyhow::bail!("{method} failed: {}", err.message);
        }
        Ok(response.result.unwrap_or(serde_json::Value::Null))
    }
}
