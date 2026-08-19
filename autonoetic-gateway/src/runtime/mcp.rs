//! MCP tool dispatcher for agent runtime.
//!
//! Loads registered MCP servers from a registry file, discovers tools, and
//! dispatches `mcp_<server>_<tool>` calls during the agent execution loop.
//!
//! Hot reload (#1121): the runtime records the registry file's mtime at load
//! and re-reads it at each turn boundary via [`McpToolRuntime::reload_if_changed`].
//! Added servers' tools join the advertised surface on the next turn; removed
//! servers' tools fail closed at dispatch (`Unknown MCP tool`). A registry
//! that fails to parse keeps the previously loaded tools (logged once per
//! change, not retried every turn), and individual servers that fail to
//! connect are skipped with a warning rather than disabling all MCP tools.

use crate::llm::ToolDefinition;
use autonoetic_mcp::{McpClient, McpServer, McpTool};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

const MCP_REGISTRY_PATH_ENV: &str = "AUTONOETIC_MCP_REGISTRY_PATH";

pub struct McpToolRuntime {
    clients: HashMap<String, McpClient>,
    tools_by_name: HashMap<String, McpTool>,
    tool_server: HashMap<String, String>,
    servers_by_name: HashMap<String, McpServer>,
    /// Registry file this runtime was built from (None = env var unset).
    registry_path: Option<PathBuf>,
    /// Mtime of the registry file at last successful load attempt; used to
    /// detect changes (and to avoid retrying an unparseable file every turn).
    registry_mtime: Option<SystemTime>,
}

impl McpToolRuntime {
    /// Load MCP runtime from the registry path provided in env.
    ///
    /// If the env variable is absent or the file does not exist, returns an
    /// empty runtime (no MCP tools available).
    pub async fn from_env() -> anyhow::Result<Self> {
        let Ok(path) = std::env::var(MCP_REGISTRY_PATH_ENV) else {
            tracing::debug!("{} is not set; MCP runtime disabled", MCP_REGISTRY_PATH_ENV);
            return Ok(Self::empty());
        };
        Self::from_registry_path(PathBuf::from(path)).await
    }

    pub fn empty() -> Self {
        Self {
            clients: HashMap::new(),
            tools_by_name: HashMap::new(),
            tool_server: HashMap::new(),
            servers_by_name: HashMap::new(),
            registry_path: None,
            registry_mtime: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tools_by_name.is_empty()
    }

    pub fn has_tool(&self, tool_name: &str) -> bool {
        self.tools_by_name.contains_key(tool_name)
    }

    /// True when the tool's server is classified `remote` (SSE or explicit).
    pub fn tool_requires_network_egress_gate(&self, tool_name: &str) -> bool {
        self.tool_server
            .get(tool_name)
            .and_then(|server| self.servers_by_name.get(server))
            .map(|s| s.requires_network_egress_gate())
            .unwrap_or(true)
    }

    /// Host of the tool's server when it is a remote (SSE) endpoint — used to
    /// scope egress declassification grants to the approved host.
    pub fn tool_server_host(&self, tool_name: &str) -> Option<String> {
        let server = self
            .tool_server
            .get(tool_name)
            .and_then(|name| self.servers_by_name.get(name))?;
        match &server.transport {
            autonoetic_mcp::McpTransportConfig::Sse { url } => {
                crate::runtime::tools::extract_host(url).ok()
            }
            _ => None,
        }
    }

    pub fn tool_definitions(&self) -> anyhow::Result<Vec<ToolDefinition>> {
        let mut defs = Vec::with_capacity(self.tools_by_name.len());
        for tool in self.tools_by_name.values() {
            let description = tool
                .description
                .clone()
                .ok_or_else(|| anyhow::anyhow!("MCP tool '{}' missing description", tool.name))?;
            let input_schema = tool
                .input_schema
                .clone()
                .ok_or_else(|| anyhow::anyhow!("MCP tool '{}' missing input_schema", tool.name))?;
            defs.push(ToolDefinition {
                name: tool.name.clone(),
                description,
                input_schema,
            });
        }
        Ok(defs)
    }

    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments_json: &str,
    ) -> anyhow::Result<String> {
        let server_name = self
            .tool_server
            .get(tool_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown MCP tool '{}'", tool_name))?
            .to_string();
        let client = self
            .clients
            .get_mut(&server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server client '{}' not found", server_name))?;

        let arguments: serde_json::Value = serde_json::from_str(arguments_json).map_err(|e| {
            anyhow::anyhow!("Invalid JSON arguments for tool '{}': {}", tool_name, e)
        })?;
        let result = client.call_tool(tool_name, arguments).await?;
        Ok(serde_json::to_string(&result.payload)?)
    }

    /// Re-read the registry file when it changed since the last load (#1121).
    ///
    /// Returns `true` when the tool surface was rebuilt. Semantics:
    /// - unchanged mtime → no-op
    /// - file deleted → swap to empty (all MCP tools fail closed)
    /// - unparseable file → keep current tools, warn, remember mtime so the
    ///   broken file is not re-parsed every turn
    /// - otherwise rebuild tolerantly (per-server failures skipped) and swap
    pub async fn reload_if_changed(&mut self) -> anyhow::Result<bool> {
        let Some(path) = self.registry_path.clone() else {
            return Ok(false);
        };
        let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(mtime) => Some(mtime),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %e,
                    "MCP registry metadata unreadable; skipping reload check"
                );
                return Ok(false);
            }
        };
        if mtime == self.registry_mtime {
            return Ok(false);
        }

        match mtime {
            None => {
                if !self.tools_by_name.is_empty() {
                    tracing::info!(
                        path = %path.display(),
                        "MCP registry file removed; unloading all MCP tools"
                    );
                }
                *self = Self::empty_at(path, None);
                return Ok(true);
            }
            Some(mtime) => {
                // File-level failure (unreadable / invalid JSON) keeps the
                // current tools; per-server failures are tolerated inside the
                // build. The mtime is recorded either way: a broken edit
                // should not be re-parsed on every turn.
                match Self::load_from_path(&path).await {
                    Some(reloaded) => {
                        let added = reloaded
                            .servers_by_name
                            .keys()
                            .filter(|s| !self.servers_by_name.contains_key(*s))
                            .cloned()
                            .collect::<Vec<_>>();
                        let removed = self
                            .servers_by_name
                            .keys()
                            .filter(|s| !reloaded.servers_by_name.contains_key(*s))
                            .cloned()
                            .collect::<Vec<_>>();
                        tracing::info!(
                            path = %path.display(),
                            added = ?added,
                            removed = ?removed,
                            tools = reloaded.tools_by_name.len(),
                            "MCP registry reloaded"
                        );
                        self.clients = reloaded.clients;
                        self.tools_by_name = reloaded.tools_by_name;
                        self.tool_server = reloaded.tool_server;
                        self.servers_by_name = reloaded.servers_by_name;
                    }
                    None => {
                        tracing::warn!(
                            path = %path.display(),
                            "MCP registry reload failed; keeping previously loaded tools"
                        );
                    }
                }
                self.registry_mtime = Some(mtime);
                Ok(true)
            }
        }
    }

    fn empty_at(registry_path: PathBuf, registry_mtime: Option<SystemTime>) -> Self {
        Self {
            registry_path: Some(registry_path),
            registry_mtime,
            ..Self::empty()
        }
    }

    async fn from_registry_path(path: PathBuf) -> anyhow::Result<Self> {
        if !path.exists() {
            tracing::debug!(
                "MCP registry path {} not found; MCP runtime disabled",
                path.display()
            );
            return Ok(Self::empty_at(path, None));
        }
        let registry_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut loaded = Self::load_from_path(&path)
            .await
            .unwrap_or_else(Self::empty);
        loaded.registry_path = Some(path);
        loaded.registry_mtime = registry_mtime;
        Ok(loaded)
    }

    /// Build the client/tool maps from the registry file, tolerantly: a
    /// server that fails to connect or list tools is skipped with a warning
    /// instead of disabling every MCP tool for the run. A duplicate tool
    /// name skips the later server entirely (config error, logged).
    ///
    /// Returns `None` only for file-level failures (unreadable / invalid
    /// JSON), so reload can tell "broken edit, keep current tools" apart
    /// from "valid registry whose servers all failed".
    async fn load_from_path(path: &std::path::Path) -> Option<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "MCP registry unreadable; no MCP tools loaded"
                );
                return None;
            }
        };
        let servers: Vec<McpServer> = match serde_json::from_str(&raw) {
            Ok(servers) => servers,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "MCP registry is not valid JSON; no MCP tools loaded"
                );
                return None;
            }
        };

        let mut clients = HashMap::new();
        let mut tools_by_name = HashMap::new();
        let mut tool_server = HashMap::new();
        let mut servers_by_name = HashMap::new();

        for server in servers {
            let server_name = server.name.clone();
            let mut client = match McpClient::connect(&server).await {
                Ok(client) => client,
                Err(e) => {
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "MCP server failed to connect; its tools are unavailable this run"
                    );
                    continue;
                }
            };
            let tools = match client.list_tools().await {
                Ok(tools) => tools,
                Err(e) => {
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "MCP server failed tools/list; its tools are unavailable this run"
                    );
                    continue;
                }
            };
            let mut duplicate = false;
            for tool in &tools {
                if tools_by_name.contains_key(&tool.name) {
                    tracing::warn!(
                        server = %server_name,
                        tool = %tool.name,
                        "MCP server duplicates an already-registered tool name; \
                         skipping the whole server (fix the registry)"
                    );
                    duplicate = true;
                    break;
                }
            }
            if duplicate {
                continue;
            }
            servers_by_name.insert(server_name.clone(), server.clone());
            for tool in tools {
                tool_server.insert(tool.name.clone(), server_name.clone());
                tools_by_name.insert(tool.name.clone(), tool);
            }
            clients.insert(server_name, client);
        }

        Some(Self {
            clients,
            tools_by_name,
            tool_server,
            servers_by_name,
            registry_path: None,
            registry_mtime: None,
        })
    }
}
