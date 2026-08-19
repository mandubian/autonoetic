//! Integration tests for MCP runtime wiring in gateway.
//!
//! Run with:
//!   cargo test -p autonoetic-gateway --test mcp_integration -- --nocapture

use autonoetic_gateway::runtime::mcp::McpToolRuntime;
use autonoetic_mcp::protocol::JsonRpcRequest;
use autonoetic_mcp::{AgentExecutor, AgentMcpServer, ExposedAgent, McpServer, McpTransportConfig};
use serial_test::serial;
use tempfile::tempdir;

const MCP_REGISTRY_ENV: &str = "AUTONOETIC_MCP_REGISTRY_PATH";

struct EnvGuard(Option<String>);

impl EnvGuard {
    fn set_registry(path: &std::path::Path) -> Self {
        let old = std::env::var(MCP_REGISTRY_ENV).ok();
        std::env::set_var(MCP_REGISTRY_ENV, path.display().to_string());
        Self(old)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => std::env::set_var(MCP_REGISTRY_ENV, v),
            None => std::env::remove_var(MCP_REGISTRY_ENV),
        }
    }
}

/// Rewrite the registry file and wait until its mtime differs from `before`,
/// so `reload_if_changed` cannot miss the change on coarse-mtime filesystems.
/// Fails loudly when the mtime cannot be bumped within the deadline — silently
/// returning would make the caller's reload assertion flaky instead of broken.
fn rewrite_registry_bumping_mtime(
    path: &std::path::Path,
    servers: &serde_json::Value,
    before: Option<std::time::SystemTime>,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        std::fs::write(path, serde_json::to_vec(servers)?)?;
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime != before {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() <= deadline,
            "could not bump mtime of {} within 3s (filesystem mtime granularity?)",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn mock_server(name: &str, script_path: &std::path::Path) -> McpServer {
    McpServer {
        name: name.to_string(),
        command: "bash".to_string(),
        args: vec![script_path.display().to_string()],
        transport: McpTransportConfig::Stdio,
        egress_class: None,
    }
}

fn write_mock_stdio_mcp_server_script(script_path: &std::path::Path) -> anyhow::Result<()> {
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
while IFS= read -r line; do
  id="$(printf '%s' "$line" | sed -n 's/.*"id":[[:space:]]*\([0-9][0-9]*\).*/\1/p')"
  if [[ -z "${id}" ]]; then
    id=1
  fi

  if [[ "$line" == *"\"tools/list\""* ]]; then
    echo "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"Echo input\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"]}}]}}"
  elif [[ "$line" == *"\"tools/call\""* ]]; then
    echo "{\"jsonrpc\":\"2.0\",\"id\":${id},\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"mock-echo-ok\"}]}}"
  else
    echo "{\"jsonrpc\":\"2.0\",\"id\":${id},\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}"
  fi
done
"#;
    std::fs::write(script_path, script)?;
    Ok(())
}

struct MockAgentExec;

#[async_trait::async_trait]
impl AgentExecutor for MockAgentExec {
    async fn call_agent(&self, agent_id: &str, message: &str) -> anyhow::Result<String> {
        Ok(format!("agent={} message={}", agent_id, message))
    }
}

#[tokio::test]
#[serial]
async fn test_mcp_integration_loads_existing_server_and_exposes_agent_tool() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let script_path = tmp.path().join("mock-mcp.sh");
    let registry_path = tmp.path().join("mcp_servers.json");
    write_mock_stdio_mcp_server_script(&script_path)?;

    let servers = vec![McpServer {
        name: "mock".to_string(),
        command: "bash".to_string(),
        args: vec![script_path.display().to_string()],
        transport: McpTransportConfig::Stdio,
        egress_class: None,
    }];
    std::fs::write(&registry_path, serde_json::to_vec(&servers)?)?;

    // 1) Gateway runtime loads existing MCP server and dispatches tool call.
    let old_registry = std::env::var("AUTONOETIC_MCP_REGISTRY_PATH").ok();
    std::env::set_var(
        "AUTONOETIC_MCP_REGISTRY_PATH",
        registry_path.display().to_string(),
    );

    let mut runtime = McpToolRuntime::from_env().await?;
    assert!(!runtime.is_empty(), "Expected MCP runtime to load tools");

    let defs = runtime.tool_definitions()?;
    assert!(
        defs.iter().any(|d| d.name == "mcp_mock_echo"),
        "Expected namespaced MCP tool mcp_mock_echo"
    );

    let call_result = runtime
        .call_tool("mcp_mock_echo", r#"{"text":"hello"}"#)
        .await?;
    let call_json: serde_json::Value = serde_json::from_str(&call_result)?;
    assert_eq!(call_json["content"][0]["text"], "mock-echo-ok");

    match old_registry {
        Some(v) => std::env::set_var("AUTONOETIC_MCP_REGISTRY_PATH", v),
        None => std::env::remove_var("AUTONOETIC_MCP_REGISTRY_PATH"),
    }

    // 2) MCP server side exposes agent as callable MCP tool.
    let mut agent_server = AgentMcpServer::new(MockAgentExec);
    agent_server.register_agent(ExposedAgent {
        id: "agent-42".to_string(),
        name: "researcher".to_string(),
        description: "Research specialist".to_string(),
    });

    let list_req = JsonRpcRequest::new(1, "tools/list", serde_json::json!({}));
    let list_resp = agent_server.handle(list_req).await;
    let tools = list_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("Expected tools/list result"))?["tools"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Expected tools array"))?
        .clone();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "autonoetic_agent_researcher");

    let call_req = JsonRpcRequest::new(
        2,
        "tools/call",
        serde_json::json!({
            "name": "autonoetic_agent_researcher",
            "arguments": { "message": "ping" }
        }),
    );
    let call_resp = agent_server.handle(call_req).await;
    let text = call_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("Expected tools/call result"))?["content"][0]["text"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Expected text content"))?
        .to_string();
    assert_eq!(text, "agent=agent-42 message=ping");

    Ok(())
}

/// #1121: a server added to the registry file while the runtime is live must
/// become callable after `reload_if_changed` — no gateway restart.
#[tokio::test]
#[serial]
async fn mcp_hot_reload_picks_up_added_server() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let script_path = tmp.path().join("mock-mcp.sh");
    let registry_path = tmp.path().join("mcp_servers.json");
    write_mock_stdio_mcp_server_script(&script_path)?;
    std::fs::write(&registry_path, b"[]")?;

    let _guard = EnvGuard::set_registry(&registry_path);
    let mut runtime = McpToolRuntime::from_env().await?;
    assert!(runtime.is_empty());
    assert!(
        !runtime.reload_if_changed().await?,
        "unchanged registry must not reload"
    );

    let mtime_before = std::fs::metadata(&registry_path).and_then(|m| m.modified()).ok();
    rewrite_registry_bumping_mtime(
        &registry_path,
        &serde_json::json!([mock_server("mock", &script_path)]),
        mtime_before,
    )?;

    assert!(
        runtime.reload_if_changed().await?,
        "changed registry must reload"
    );
    assert!(runtime.has_tool("mcp_mock_echo"));
    let call_result = runtime
        .call_tool("mcp_mock_echo", r#"{"text":"hi"}"#)
        .await?;
    assert!(call_result.contains("mock-echo-ok"));
    Ok(())
}

/// #1121: a server removed from the registry must fail closed — its tools
/// disappear from the surface and dispatch errors instead of reaching a
/// stale client.
#[tokio::test]
#[serial]
async fn mcp_hot_reload_removed_server_fails_closed() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let script_path = tmp.path().join("mock-mcp.sh");
    let registry_path = tmp.path().join("mcp_servers.json");
    write_mock_stdio_mcp_server_script(&script_path)?;
    std::fs::write(
        &registry_path,
        serde_json::to_vec(&vec![mock_server("mock", &script_path)])?,
    )?;

    let _guard = EnvGuard::set_registry(&registry_path);
    let mut runtime = McpToolRuntime::from_env().await?;
    assert!(runtime.has_tool("mcp_mock_echo"));

    let mtime_before = std::fs::metadata(&registry_path).and_then(|m| m.modified()).ok();
    rewrite_registry_bumping_mtime(&registry_path, &serde_json::json!([]), mtime_before)?;

    assert!(runtime.reload_if_changed().await?);
    assert!(!runtime.has_tool("mcp_mock_echo"));
    let err = runtime
        .call_tool("mcp_mock_echo", r#"{"text":"hi"}"#)
        .await
        .expect_err("removed server's tool must fail closed");
    assert!(err.to_string().contains("Unknown MCP tool"));

    // Deleting the registry file entirely also unloads everything.
    std::fs::remove_file(&registry_path)?;
    assert!(runtime.reload_if_changed().await?);
    assert!(runtime.is_empty());
    Ok(())
}

/// #1121: a broken registry edit must not wipe the working tool surface —
/// the previous tools stay live, and a later valid edit is picked up.
#[tokio::test]
#[serial]
async fn mcp_hot_reload_broken_registry_keeps_previous_tools() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let script_path = tmp.path().join("mock-mcp.sh");
    let registry_path = tmp.path().join("mcp_servers.json");
    write_mock_stdio_mcp_server_script(&script_path)?;
    std::fs::write(
        &registry_path,
        serde_json::to_vec(&vec![mock_server("mock", &script_path)])?,
    )?;

    let _guard = EnvGuard::set_registry(&registry_path);
    let mut runtime = McpToolRuntime::from_env().await?;
    assert!(runtime.has_tool("mcp_mock_echo"));

    let mtime_before = std::fs::metadata(&registry_path).and_then(|m| m.modified()).ok();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        std::fs::write(&registry_path, b"{not json")?;
        let mtime = std::fs::metadata(&registry_path).and_then(|m| m.modified()).ok();
        if mtime != mtime_before {
            break;
        }
        assert!(
            std::time::Instant::now() <= deadline,
            "could not bump registry mtime within 3s (filesystem mtime granularity?)"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    assert!(runtime.reload_if_changed().await?);
    assert!(
        runtime.has_tool("mcp_mock_echo"),
        "broken registry must keep previously loaded tools"
    );

    // The broken mtime is remembered: a second identical check is a no-op...
    assert!(!runtime.reload_if_changed().await?);

    // ...and repairing the file is picked up on the next change.
    let mtime_before = std::fs::metadata(&registry_path).and_then(|m| m.modified()).ok();
    rewrite_registry_bumping_mtime(
        &registry_path,
        &serde_json::json!([mock_server("mock", &script_path)]),
        mtime_before,
    )?;
    assert!(runtime.reload_if_changed().await?);
    assert!(runtime.has_tool("mcp_mock_echo"));
    Ok(())
}

/// #1121: one dead server must not disable the healthy ones (per-server
/// tolerance at load and reload).
#[tokio::test]
#[serial]
async fn mcp_load_tolerates_dead_server() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let script_path = tmp.path().join("mock-mcp.sh");
    let registry_path = tmp.path().join("mcp_servers.json");
    write_mock_stdio_mcp_server_script(&script_path)?;

    let dead = McpServer {
        name: "dead".to_string(),
        command: "definitely-not-a-real-binary-autonoetic".to_string(),
        args: vec![],
        transport: McpTransportConfig::Stdio,
        egress_class: None,
    };
    std::fs::write(
        &registry_path,
        serde_json::to_vec(&vec![dead, mock_server("mock", &script_path)])?,
    )?;

    let _guard = EnvGuard::set_registry(&registry_path);
    let runtime = McpToolRuntime::from_env().await?;
    assert!(
        runtime.has_tool("mcp_mock_echo"),
        "healthy server must load despite the dead one"
    );
    Ok(())
}
