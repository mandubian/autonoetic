use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use tempfile::tempdir;

fn evaluator_manifest() -> AgentManifest {
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
            id: "evaluator.default".to_string(),
            name: "evaluator.default".to_string(),
            description: "Evaluator".to_string(),
        },
        capabilities: vec![],
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
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn setup_store(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let gw = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gw).unwrap();
    let cs = ContentStore::new(&gw).unwrap();
    let handle = cs.write(b"test artifact content".as_slice()).unwrap();
    cs.register_name("test-session", "artifact.tar.zst", &handle)
        .unwrap();
    gw
}

#[test]
fn test_promotion_record_rejects_empty_finding_description() {
    let tmp = tempdir().unwrap();
    let gw = setup_store(&tmp);
    let manifest = evaluator_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let err = registry
        .execute(
            "promotion_record",
            &manifest,
            &policy,
            tmp.path(),
            Some(&gw),
            &serde_json::json!({
                "artifact_id": "art_test123",
                "role": "evaluator",
                "pass": false,
                "findings": [{"severity": "error", "description": ""}]
            })
            .to_string(),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("findings[0]"),
        "Expected findings index, got: {msg}"
    );
    assert!(
        msg.contains("description is empty"),
        "Expected 'description is empty', got: {msg}"
    );
}

#[test]
fn test_promotion_record_accepts_valid_findings() {
    let tmp = tempdir().unwrap();
    let gw = setup_store(&tmp);
    let manifest = evaluator_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let result = registry
        .execute(
            "promotion_record",
            &manifest,
            &policy,
            tmp.path(),
            Some(&gw),
            &serde_json::json!({
                "artifact_id": "art_test123",
                "role": "evaluator",
                "pass": true,
                "findings": [{"severity": "info", "description": "All checks passed"}]
            })
            .to_string(),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

    let output: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert!(output["ok"].as_bool().unwrap());
}
