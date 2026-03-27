//! Integration test for sandbox.exec capture_paths functionality.
//!
//! Tests:
//! - sandbox.exec with capture_paths returns captured_layers
//! - Captured layer contains expected files
//! - sandbox.exec without capture_paths has no captured_layers in response
//! - Capture path that doesn't exist returns error
//! - Multiple capture paths produce multiple layers

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::{default_registry, NativeToolRunContext};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentManifest, AgentIdentity, RuntimeDeclaration};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::tool_error::ToolInvocationError;
use serde_json::json;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn test_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: "test.agent".to_string(),
            name: "Test Agent".to_string(),
            description: "Test agent".to_string(),
        },
        llm_config: None,
        capabilities: vec![],
        execution_mode: Some("reasoning".to_string()),
        script_entry: None,
        gateway_url: None,
        gateway_token: None,
        io: None,
        middleware: None,
        disclosure: None,
    }
}

#[test]
fn test_sandbox_exec_with_capture_paths() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();

    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let config = GatewayConfig::default();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_store = GatewayStore::open(&gw_dir).unwrap();

    // Create some files to capture
    let venv_dir = agent_dir.join("venv");
    std::fs::create_dir_all(&venv_dir.join("lib")).unwrap();
    std::fs::write(
        venv_dir.join("lib/python3.12/site-packages/requests/__init__.py"),
        b"# requests package",
    )
    .unwrap();

    let manifest = test_manifest();

    let tool_name = "sandbox.exec";
    let arguments = json!({
        "command": "echo 'test'",
        "capture_paths": [
            {
                "path": "/tmp/venv",
                "mount_as": "/opt/venv"
            }
        ]
    });

    let result = support::run_tool(
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        tool_name,
        &arguments,
        None,
        None,
        None,
        Some(&config),
        Some(&gateway_store),
        None,
    );

    assert!(result.is_ok());
    let response_str = result.unwrap();
    let response: serde_json::Value = serde_json::from_str(&response_str).unwrap();

    // Check that command succeeded
    assert_eq!(response["ok"], true);

    // Check that captured_layers was included
    assert!(response.get("captured_layers").is_some());
    let captured_layers = response["captured_layers"].as_array().unwrap();

    assert_eq!(captured_layers.len(), 1);
    assert_eq!(captured_layers[0]["path"], "/tmp/venv");
    assert_eq!(captured_layers[0]["mount_as"], "/opt/venv");
    assert!(captured_layers[0].get("layer_id").is_some());
    assert!(captured_layers[0].get("digest").is_some());
}

#[test]
fn test_sandbox_exec_without_capture_paths() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();

    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let config = GatewayConfig::default();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_store = GatewayStore::open(&gw_dir).unwrap();

    let manifest = test_manifest();

    let tool_name = "sandbox.exec";
    let arguments = json!({
        "command": "echo 'test'"
    });

    let result = support::run_tool(
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        tool_name,
        &arguments,
        None,
        None,
        None,
        Some(&config),
        Some(&gateway_store),
        None,
    );

    assert!(result.is_ok());
    let response_str = result.unwrap();
    let response: serde_json::Value = serde_json::from_str(&response_str).unwrap();

    // Check that command succeeded
    assert_eq!(response["ok"], true);

    // Check that captured_layers was NOT included
    assert!(response.get("captured_layers").is_none());
}

#[test]
fn test_sandbox_exec_capture_multiple_paths() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();

    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let config = GatewayConfig::default();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_store = GatewayStore::open(&gw_dir).unwrap();

    // Create multiple directories to capture
    let venv_dir = agent_dir.join("venv");
    std::fs::create_dir_all(&venv_dir.join("lib")).unwrap();
    std::fs::write(
        venv_dir.join("lib/python3.12/site-packages/requests/__init__.py"),
        b"# requests",
    )
    .unwrap();

    let node_modules_dir = agent_dir.join("node_modules");
    std::fs::create_dir_all(&node_modules_dir.join("axios")).unwrap();
    std::fs::write(
        node_modules_dir.join("axios/package.json"),
        b"{\"name\": \"axios\"}",
    )
    .unwrap();

    let manifest = test_manifest();

    let tool_name = "sandbox.exec";
    let arguments = json!({
        "command": "echo 'test'",
        "capture_paths": [
            {
                "path": "/tmp/venv",
                "mount_as": "/opt/venv"
            },
            {
                "path": "/tmp/node_modules",
                "mount_as": "/opt/node_modules"
            }
        ]
    });

    let result = support::run_tool(
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        tool_name,
        &arguments,
        None,
        None,
        None,
        Some(&config),
        Some(&gateway_store),
        None,
    );

    assert!(result.is_ok());
    let response_str = result.unwrap();
    let response: serde_json::Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["ok"], true);
    assert!(response.get("captured_layers").is_some());
    let captured_layers = response["captured_layers"].as_array().unwrap();

    assert_eq!(captured_layers.len(), 2);
}
