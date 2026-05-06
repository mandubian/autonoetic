//! Constitution Phase 4.5 pin: sandbox path validation must not invent
//! content-handle/digest heuristics. Invalid paths fail naturally at exec time.

use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde_json::json;
use tempfile::tempdir;

fn manifest(agent_id: &str) -> AgentManifest {
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
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "test agent".to_string(),
        },
        capabilities: vec![
            Capability::CodeExecution {
                patterns: vec!["*".to_string()],
                commands: vec![],
            },
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
        ],
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        response_contract: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
    }
}

fn run_sandbox_exec(command: &str) -> anyhow::Result<serde_json::Value> {
    let temp = tempdir()?;
    let cfg = GatewayConfig {
        agents_dir: temp.path().join("agents"),
        ..GatewayConfig::default()
    };
    std::fs::create_dir_all(&cfg.agents_dir)?;

    let store = Arc::new(GatewayStore::open(temp.path())?);
    let registry = default_registry();
    let manifest = manifest("executor.default");
    let policy = PolicyEngine::new(manifest.clone());

    let out = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        temp.path(),
        Some(temp.path()),
        &json!({ "command": command }).to_string(),
        Some("root-1/session-1"),
        None,
        Some(&cfg),
        Some(store),
        None,
    )?;
    Ok(serde_json::from_str(&out)?)
}

fn assert_natural_exec_failure(body: &serde_json::Value, expected_path_fragment: &str) {
    assert_eq!(body["ok"], false);
    assert_ne!(
        body["exit_code"].as_i64(),
        Some(0),
        "sandbox exec should fail at command runtime"
    );
    let stderr = body["stderr"].as_str().unwrap_or_default();
    assert!(
        stderr.contains(expected_path_fragment),
        "expected stderr to mention path fragment `{expected_path_fragment}`, got: {stderr}"
    );
    assert!(
        !stderr.contains("content handles (cnt_...) are not filesystem paths"),
        "gateway-invented cnt_ heuristic error must not appear: {stderr}"
    );
    assert!(
        !stderr.contains("content digests (sha256:...) are not filesystem paths"),
        "gateway-invented sha256 heuristic error must not appear: {stderr}"
    );
}

#[test]
fn cnt_handle_like_path_fails_naturally_at_exec_time() -> anyhow::Result<()> {
    let body = run_sandbox_exec("cat /tmp/cnt_deadbeef")?;
    assert_natural_exec_failure(&body, "cnt_deadbeef");
    Ok(())
}

#[test]
fn sha256_like_path_fails_naturally_at_exec_time() -> anyhow::Result<()> {
    let body = run_sandbox_exec(
        "cat /tmp/sha256:30db6cfe48acf14817e914345f2a9657b510a8138a1442c3015103beef35279a",
    )?;
    assert_natural_exec_failure(&body, "sha256:");
    Ok(())
}
