mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, RuntimeDeclaration};
use autonoetic_types::capability::Capability;

fn make_manifest(has_network: bool) -> AgentManifest {
    let mut caps = vec![Capability::CodeExecution {
        patterns: vec![
            "python3 ".to_string(),
            "pip ".to_string(),
            "bash -c ".to_string(),
        ],
        commands: vec![],
    }];
    if has_network {
        caps.push(Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        });
    }
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
            id: "test-coder".to_string(),
            name: "Test Coder".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        llm_overrides: None,
        llm_preset: None,
        llm_config: None,
        limits: None,
        capabilities: caps,
        background: None,
        middleware: None,
        execution_mode: ExecutionMode::Reasoning,
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        agentskills_import: None,
        io: None,
        disclosure: None,
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            egress: None,
        }
}

fn exec_sandbox(manifest: &AgentManifest, command: &str) -> serde_json::Value {
    let registry = default_registry();
    let policy = PolicyEngine::new(manifest.clone());
    let tmpdir = tempfile::tempdir().unwrap();
    let agent_dir = tmpdir.path().join("test-agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let args = serde_json::json!({ "command": command });
    let result = registry
        .execute(
            "sandbox_exec",
            manifest,
            &policy,
            &agent_dir,
            None::<&std::path::Path>,
            &serde_json::to_string(&args).unwrap(),
            Some("test-session"),
            None,
            None::<&autonoetic_types::config::GatewayConfig>,
            None,
            None,
        )
        .unwrap();

    serde_json::from_str(&result).unwrap()
}

#[test]
fn test_pip_install_from_non_network_agent_returns_redirect() {
    let manifest = make_manifest(false);
    let parsed = exec_sandbox(&manifest, "pip install requests flask");

    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["dependency_layer_required"], true);
    assert_eq!(parsed["recommended_agent"], "packager.default");
}

#[test]
fn test_npm_install_from_non_network_agent_returns_redirect() {
    let manifest = make_manifest(false);
    let parsed = exec_sandbox(
        &manifest,
        "bash -c \"cd /tmp/project && npm install express\"",
    );

    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["dependency_layer_required"], true);
    assert_eq!(parsed["recommended_agent"], "packager.default");
}

#[test]
fn test_safe_inspection_pip_list_skips_approval() {
    let manifest = make_manifest(false);
    let parsed = exec_sandbox(&manifest, "pip list");

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed.get("approval_required"), None);
}

fn exec_sandbox_with_artifact(
    manifest: &AgentManifest,
    command: &str,
    artifact_id: &str,
) -> serde_json::Value {
    let registry = default_registry();
    let policy = PolicyEngine::new(manifest.clone());
    let tmpdir = tempfile::tempdir().unwrap();
    let agent_dir = tmpdir.path().join("test-agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let args = serde_json::json!({
        "command": command,
        "artifact_id": artifact_id,
    });
    let result = registry
        .execute(
            "sandbox_exec",
            manifest,
            &policy,
            &agent_dir,
            Some(tmpdir.path()),
            &serde_json::to_string(&args).unwrap(),
            Some("test-session"),
            None,
            None::<&autonoetic_types::config::GatewayConfig>,
            None,
            None,
        )
        .unwrap();

    serde_json::from_str(&result).unwrap()
}

#[test]
fn test_pip_install_redirect_fires_with_artifact_id() {
    let manifest = make_manifest(false);
    let parsed = exec_sandbox_with_artifact(
        &manifest,
        "pip install -r requirements.txt && python3 -m pytest test.py",
        "test-artifact-123",
    );

    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["dependency_layer_required"], true);
    assert_eq!(parsed["recommended_agent"], "packager.default");
}

#[test]
fn test_safe_inspection_pip_show_skips_approval() {
    let manifest = make_manifest(false);
    let parsed = exec_sandbox(&manifest, "pip show requests");

    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed.get("approval_required"), None);
}
