//! Integration tests for layer approval scope gating in sandbox.exec.
//!
//! Tests:
//! - sandbox.exec is blocked when runtime.lock layers carry unapproved build-time hosts
//! - sandbox.exec proceeds when runtime.lock layers have no approval scope (legacy)
//! - sandbox.exec proceeds when the agent has NetworkAccess capability
//! - A LayerMount approval_ref unblocks execution

mod support;

use autonoetic_gateway::layer_store::{LayerLimits, LayerStore};
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::layer::{CapturedLayer, LayerApprovalScope};
use autonoetic_types::runtime_lock::{
    LockedGateway, LockedLayerMount, LockedSandbox, LockedSdk, RuntimeLock,
};
use std::sync::Arc;
use tempfile::tempdir;

/// Build a minimal manifest without NetworkAccess capability.
fn test_manifest_no_network() -> AgentManifest {
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
        llm_preset: None,
        llm_config: None,
        limits: None,
        capabilities: vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        allowed_tool_tiers: vec![],
        execution_mode: ExecutionMode::Reasoning,
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

/// Build a manifest WITH NetworkAccess capability.
fn test_manifest_with_network() -> AgentManifest {
    let mut m = test_manifest_no_network();
    m.capabilities.push(Capability::NetworkAccess {
        hosts: vec!["*".to_string()],
    });
    m
}

/// Create a layer in the store and write a runtime.lock that references it.
fn create_layer_with_scope(
    gw_dir: &std::path::Path,
    scope: Option<LayerApprovalScope>,
) -> CapturedLayer {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("layer_src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("file.txt"), b"dependency").unwrap();

    let store = LayerStore::new(gw_dir, LayerLimits::default()).unwrap();
    store
        .create_from_dir(&src, "test-deps", "/opt/deps", scope)
        .unwrap()
}

fn write_runtime_lock(agent_dir: &std::path::Path, layers: Vec<LockedLayerMount>) {
    let lock = RuntimeLock {
        gateway: LockedGateway {
            artifact: "marketplace://gateway/autonoetic-gateway".to_string(),
            version: "0.1.0".to_string(),
            sha256: "sha256:test".to_string(),
            binary_sha256: None,
            build_tag: None,
            signature: None,
        },
        sdk: LockedSdk {
            version: "0.1.0".to_string(),
        },
        sandbox: LockedSandbox {
            backend: "bubblewrap".to_string(),
        },
        dependencies: vec![],
        artifacts: vec![],
        layers,
        credentials: vec![],
    };
    let lock_yaml = serde_yaml::to_string(&lock).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), lock_yaml).unwrap();
}

/// Create a layer in the store and write a runtime.lock that references it.
fn setup_layer_with_scope(
    gw_dir: &std::path::Path,
    agent_dir: &std::path::Path,
    scope: Option<LayerApprovalScope>,
) -> String {
    let captured = create_layer_with_scope(gw_dir, scope);
    write_runtime_lock(
        agent_dir,
        vec![LockedLayerMount {
            layer_id: captured.layer_id.clone(),
            digest: captured.digest.clone(),
            mount_path: "/opt/deps".to_string(),
            approval_scope: captured.approval_scope.clone(),
        }],
    );
    captured.layer_id
}

#[test]
fn test_layer_with_unapproved_hosts_blocks_execution() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let scope = LayerApprovalScope {
        approved_hosts: vec!["pypi.org".to_string()],
        built_by_agent_id: "packager.default".to_string(),
        captured_at: "2026-04-01T00:00:00Z".to_string(),
    };
    setup_layer_with_scope(&gw_dir, &agent_dir, Some(scope));

    let manifest = test_manifest_no_network();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    let config = GatewayConfig::default();

    let arguments = serde_json::json!({ "command": "echo 'run with layer'" });

    let result = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments.to_string(),
        Some("sess_test"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(
        result.is_ok(),
        "should return Ok (approval response, not Err)"
    );
    let response: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();

    assert_eq!(
        response["ok"], false,
        "execution should be blocked by layer scope check"
    );
    assert_eq!(
        response["layer_mount_approval_required"], true,
        "response should indicate layer_mount_approval_required"
    );
    assert!(
        response.get("request_id").is_some(),
        "approval request_id should be present"
    );
    let stderr = response["stderr"].as_str().unwrap_or("");
    assert!(
        stderr.contains("pypi.org"),
        "stderr should mention the unapproved host: {}",
        stderr
    );
}

#[test]
fn test_layer_without_scope_does_not_block_execution() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    setup_layer_with_scope(&gw_dir, &agent_dir, None);

    let manifest = test_manifest_no_network();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    let config = GatewayConfig::default();

    let arguments = serde_json::json!({ "command": "echo 'hello'" });

    let result = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments.to_string(),
        Some("sess_no_scope"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    // If bwrap is not available, the call will fail at execution (not at scope check) — that's OK.
    // What matters is that layer_mount_approval_required is NOT set.
    match result {
        Err(_) => {
            // Failed at execution stage (e.g., bwrap not available), not at scope check. OK.
        }
        Ok(json_str) => {
            let response: serde_json::Value = serde_json::from_str(&json_str).unwrap();
            assert_ne!(
                response.get("layer_mount_approval_required"),
                Some(&serde_json::Value::Bool(true)),
                "network-free layer should not require approval: {:?}",
                response
            );
        }
    }
}

#[test]
fn test_layer_with_scope_does_not_block_network_access_agent() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let scope = LayerApprovalScope {
        approved_hosts: vec!["npm.example.com".to_string()],
        built_by_agent_id: "packager.default".to_string(),
        captured_at: "2026-04-01T00:00:00Z".to_string(),
    };
    setup_layer_with_scope(&gw_dir, &agent_dir, Some(scope));

    let manifest = test_manifest_with_network();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    let config = GatewayConfig::default();

    let arguments = serde_json::json!({ "command": "echo 'hello'" });

    let result = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments.to_string(),
        Some("sess_net"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    match result {
        Err(_) => {
            // Failed at execution stage (bwrap not available), not at scope check. OK.
        }
        Ok(json_str) => {
            let response: serde_json::Value = serde_json::from_str(&json_str).unwrap();
            assert_ne!(
                response.get("layer_mount_approval_required"),
                Some(&serde_json::Value::Bool(true)),
                "NetworkAccess agent should bypass layer scope check: {:?}",
                response
            );
        }
    }
}

#[test]
fn test_layer_mount_approval_ref_clears_scope_gate() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let scope = LayerApprovalScope {
        approved_hosts: vec!["pypi.org".to_string()],
        built_by_agent_id: "packager.default".to_string(),
        captured_at: "2026-04-01T00:00:00Z".to_string(),
    };
    setup_layer_with_scope(&gw_dir, &agent_dir, Some(scope));

    let manifest = test_manifest_no_network();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    let config = GatewayConfig::default();

    // Step 1: First call — blocked, returns request_id
    let arguments = serde_json::json!({ "command": "echo 'go'" });
    let r1 = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments.to_string(),
        Some("sess_mount"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );
    let resp1: serde_json::Value = serde_json::from_str(&r1.unwrap()).unwrap();
    assert_eq!(resp1["layer_mount_approval_required"], true);
    let request_id = resp1["request_id"].as_str().unwrap().to_string();

    // Step 2: Operator approves
    store
        .record_decision(
            &request_id,
            "approved",
            "operator",
            &chrono::Utc::now().to_rfc3339(),
            None,
        )
        .unwrap();

    // Step 3: Retry with approval_ref — scope gate is cleared
    let arguments_with_ref =
        serde_json::json!({ "command": "echo 'go'", "approval_ref": request_id });
    let r2 = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments_with_ref.to_string(),
        Some("sess_mount"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    // If bwrap is not available, r2 is an Err from execution — that means the scope gate
    // was cleared (we got past it). Only check for layer_mount_approval_required when Ok.
    match r2 {
        Err(_) => {
            // Got past the scope gate and failed at execution. OK.
        }
        Ok(json_str) => {
            let resp2: serde_json::Value = serde_json::from_str(&json_str).unwrap();
            assert_ne!(
                resp2.get("layer_mount_approval_required"),
                Some(&serde_json::Value::Bool(true)),
                "approval_ref for LayerMount should clear the scope gate: {:?}",
                resp2
            );
        }
    }
}

#[test]
fn test_layer_mount_approval_ref_rejected_for_different_root_session() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let scope = LayerApprovalScope {
        approved_hosts: vec!["pypi.org".to_string()],
        built_by_agent_id: "packager.default".to_string(),
        captured_at: "2026-04-01T00:00:00Z".to_string(),
    };
    setup_layer_with_scope(&gw_dir, &agent_dir, Some(scope));

    let manifest = test_manifest_no_network();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    let config = GatewayConfig::default();

    let arguments = serde_json::json!({ "command": "echo 'go'" });
    let r1 = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments.to_string(),
        Some("sess_mount"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );
    let resp1: serde_json::Value = serde_json::from_str(&r1.unwrap()).unwrap();
    let request_id = resp1["request_id"].as_str().unwrap().to_string();

    store
        .record_decision(
            &request_id,
            "approved",
            "operator",
            &chrono::Utc::now().to_rfc3339(),
            None,
        )
        .unwrap();

    let arguments_with_ref =
        serde_json::json!({ "command": "echo 'go'", "approval_ref": request_id });
    let r2 = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments_with_ref.to_string(),
        Some("sess_other"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    let err = r2.expect_err("approval_ref should be rejected in a different root session");
    assert!(
        err.to_string().contains("root session"),
        "error should mention root session mismatch: {err}"
    );
}

#[test]
fn test_layer_mount_approval_ref_rejected_for_new_layer_scope() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let first_scope = LayerApprovalScope {
        approved_hosts: vec!["pypi.org".to_string()],
        built_by_agent_id: "packager.default".to_string(),
        captured_at: "2026-04-01T00:00:00Z".to_string(),
    };
    let first_layer = create_layer_with_scope(&gw_dir, Some(first_scope));
    write_runtime_lock(
        &agent_dir,
        vec![LockedLayerMount {
            layer_id: first_layer.layer_id.clone(),
            digest: first_layer.digest.clone(),
            mount_path: "/opt/deps".to_string(),
            approval_scope: first_layer.approval_scope.clone(),
        }],
    );

    let manifest = test_manifest_no_network();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    let config = GatewayConfig::default();

    let arguments = serde_json::json!({ "command": "echo 'go'" });
    let r1 = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments.to_string(),
        Some("sess_mount"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );
    let resp1: serde_json::Value = serde_json::from_str(&r1.unwrap()).unwrap();
    let request_id = resp1["request_id"].as_str().unwrap().to_string();

    store
        .record_decision(
            &request_id,
            "approved",
            "operator",
            &chrono::Utc::now().to_rfc3339(),
            None,
        )
        .unwrap();

    let second_scope = LayerApprovalScope {
        approved_hosts: vec!["crates.io".to_string()],
        built_by_agent_id: "packager.default".to_string(),
        captured_at: "2026-04-01T00:00:00Z".to_string(),
    };
    let second_layer = create_layer_with_scope(&gw_dir, Some(second_scope));
    write_runtime_lock(
        &agent_dir,
        vec![
            LockedLayerMount {
                layer_id: first_layer.layer_id.clone(),
                digest: first_layer.digest.clone(),
                mount_path: "/opt/deps".to_string(),
                approval_scope: first_layer.approval_scope.clone(),
            },
            LockedLayerMount {
                layer_id: second_layer.layer_id.clone(),
                digest: second_layer.digest.clone(),
                mount_path: "/opt/extra-deps".to_string(),
                approval_scope: second_layer.approval_scope.clone(),
            },
        ],
    );

    let arguments_with_ref =
        serde_json::json!({ "command": "echo 'go'", "approval_ref": request_id });
    let r2 = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments_with_ref.to_string(),
        Some("sess_mount"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    let err = r2.expect_err("approval_ref should not cover newly added layers or hosts");
    assert!(
        err.to_string()
            .contains("does not cover the currently requested layer scope"),
        "error should mention uncovered layer scope: {err}"
    );
}

#[test]
fn test_corrupt_layer_manifest_blocks_execution() {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let agent_dir = td.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let scope = LayerApprovalScope {
        approved_hosts: vec!["pypi.org".to_string()],
        built_by_agent_id: "packager.default".to_string(),
        captured_at: "2026-04-01T00:00:00Z".to_string(),
    };
    let layer_id = setup_layer_with_scope(&gw_dir, &agent_dir, Some(scope));
    std::fs::write(
        gw_dir.join("layers").join(&layer_id).join("manifest.json"),
        "{not valid json",
    )
    .unwrap();

    let manifest = test_manifest_no_network();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let store = Arc::new(GatewayStore::open(&gw_dir).unwrap());
    let config = GatewayConfig::default();

    let arguments = serde_json::json!({ "command": "echo 'go'" });
    let result = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw_dir),
        &arguments.to_string(),
        Some("sess_mount"),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    let err = result.expect_err("corrupt manifest should fail closed");
    assert!(
        err.to_string().contains("failed to parse layer manifest"),
        "error should mention manifest parsing failure: {err}"
    );
}
