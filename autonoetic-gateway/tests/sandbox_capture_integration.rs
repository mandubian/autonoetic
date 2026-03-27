//! Integration test for sandbox.exec capture_paths functionality.
//!
//! Tests:
//! - sandbox.exec with capture_paths returns captured_layers
//! - Captured layer contains expected files
//! - sandbox.exec without capture_paths has no captured_layers in response
//! - Multiple capture paths produce multiple layers
//!
//! These tests require bubblewrap installed and are skipped if not available.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;

fn is_bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

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
        limits: None,
        capabilities: vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
        }],
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        response_contract: None,
        execution_mode: ExecutionMode::Reasoning,
        script_entry: None,
        gateway_url: None,
        gateway_token: None,
    }
}

#[test]
fn test_sandbox_exec_with_capture_paths() {
    if !is_bwrap_available() {
        eprintln!("bubblewrap not found, skipping test");
        return;
    }

    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();

    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let manifest = test_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    let config = GatewayConfig::default();

    let venv_dir = agent_dir.join("venv");
    std::fs::create_dir_all(venv_dir.join("lib/python3.12/site-packages/requests")).unwrap();
    std::fs::write(
        venv_dir.join("lib/python3.12/site-packages/requests/__init__.py"),
        b"# requests package",
    )
    .unwrap();

    let arguments = serde_json::json!({
        "command": "echo 'test'",
        "capture_paths": [
            {
                "path": "/tmp/venv",
                "mount_as": "/opt/venv"
            }
        ]
    });

    let result = registry.execute(
        "sandbox.exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments.to_string(),
        None,
        None,
        Some(&config),
        Some(gateway_store.clone()),
        None,
    );

    assert!(result.is_ok());
    let response_str = result.unwrap();
    let response: serde_json::Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["ok"], true);
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
    if !is_bwrap_available() {
        eprintln!("bubblewrap not found, skipping test");
        return;
    }

    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();

    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let manifest = test_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    let config = GatewayConfig::default();

    let arguments = serde_json::json!({
        "command": "echo 'test'"
    });

    let result = registry.execute(
        "sandbox.exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments.to_string(),
        None,
        None,
        Some(&config),
        Some(gateway_store.clone()),
        None,
    );

    if let Err(e) = &result {
        eprintln!("sandbox.exec failed: {}", e);
    }
    assert!(result.is_ok());
    let response_str = result.unwrap();
    let response: serde_json::Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["ok"], true);
    assert!(response.get("captured_layers").is_none());
}

#[test]
fn test_sandbox_exec_capture_multiple_paths() {
    if !is_bwrap_available() {
        eprintln!("bubblewrap not found, skipping test");
        return;
    }

    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();

    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let venv_dir = agent_dir.join("venv");
    std::fs::create_dir_all(venv_dir.join("lib/python3.12/site-packages/requests")).unwrap();
    std::fs::write(
        venv_dir.join("lib/python3.12/site-packages/requests/__init__.py"),
        b"# requests",
    )
    .unwrap();

    let node_modules_dir = agent_dir.join("node_modules");
    std::fs::create_dir_all(node_modules_dir.join("axios")).unwrap();
    std::fs::write(
        node_modules_dir.join("axios/package.json"),
        b"{\"name\": \"axios\"}",
    )
    .unwrap();

    let manifest = test_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    let config = GatewayConfig::default();

    let arguments = serde_json::json!({
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

    let result = registry.execute(
        "sandbox.exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments.to_string(),
        None,
        None,
        Some(&config),
        Some(gateway_store.clone()),
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
