mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::{default_registry, NativeTool};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;

fn zero_cap_skill_md(agent_id: &str) -> String {
    format!(
        r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "{agent_id}"
  name: "{agent_id}"
  description: "Test agent for inspect"
execution_mode: script
script_entry: main.py
---
# Test agent
"#
    )
}

fn manifest_with_read_access(agent_id: &str) -> AgentManifest {
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
            description: "Test".to_string(),
        },
        capabilities: vec![
            Capability::ReadAccess {
                scopes: vec!["self.*".to_string(), "skills/*".to_string()],
            },
            Capability::AgentRevision {
                patterns: vec!["*".to_string()],
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
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn build_and_promote_agent(
    base_dir: &Path,
    agent_id: &str,
) -> (String, std::path::PathBuf, Arc<GatewayStore>) {
    let gateway_dir = base_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
    let session_id = "session-builder";

    let runtime_lock = r#"gateway:
  artifact: autonoetic-gateway
  version: "0.1.0"
  sha256: unmanaged
  signature: null
sdk:
  version: "0.1.0"
sandbox:
  backend: bubblewrap
dependencies: []
artifacts: []
layers: []
"#;

    let skill_md = zero_cap_skill_md(agent_id);
    let main_py = "#!/usr/bin/env python3\nimport json\nprint(json.dumps({'status': 'ok'}))\n";

    for (path, content) in [
        ("SKILL.md", skill_md.as_bytes()),
        ("runtime.lock", runtime_lock.as_bytes()),
        ("main.py", main_py.as_bytes()),
    ] {
        let handle = content_store.write(content).unwrap();
        content_store
            .register_name(session_id, path, &handle)
            .unwrap();
    }

    let bundle = artifact_store
        .build_with_kind(
            &[
                "SKILL.md".to_string(),
                "runtime.lock".to_string(),
                "main.py".to_string(),
            ],
            Some(&["main.py".to_string()]),
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();

    let agents_dir = base_dir.join("agents");
    let builder_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&builder_dir).unwrap();

    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let builder = manifest_with_read_access("planner.default");
    let policy = PolicyEngine::new(builder.clone());

    let rev_args = serde_json::json!({
        "agent_id": agent_id,
        "artifact_id": bundle.artifact_id,
    });
    let rev_result = registry
        .execute(
            "agent_revision_create",
            &builder,
            &policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&rev_args).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .expect("revision create should succeed");
    let rev_parsed: serde_json::Value = serde_json::from_str(&rev_result).unwrap();
    let revision_id = rev_parsed
        .get("revision_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    let promote_args = serde_json::json!({
        "agent_id": agent_id,
        "revision_id": revision_id,
        "reason": "integration test",
    });
    let promote_result = registry
        .execute(
            "agent_revision_promote",
            &builder,
            &policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&promote_args).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .expect("promote should succeed");
    let promote_parsed: serde_json::Value = serde_json::from_str(&promote_result).unwrap();
    assert_eq!(
        promote_parsed.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "promote should succeed: {promote_result}"
    );

    (bundle.artifact_id, gateway_dir, store)
}

#[test]
fn metadata_only_without_source() {
    let temp = tempdir().unwrap();
    let agent_id = "test.inspect-meta";
    let (artifact_id, gateway_dir, store) = build_and_promote_agent(temp.path(), agent_id);

    let agents_dir = temp.path().join("agents");
    let caller_dir = agents_dir.join("caller");
    std::fs::create_dir_all(&caller_dir).unwrap();
    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };

    let registry = default_registry();
    let caller = manifest_with_read_access("caller");
    let policy = PolicyEngine::new(caller.clone());

    let args = serde_json::json!({
        "agent_id": agent_id,
    });
    let result = registry
        .execute(
            "agent_inspect",
            &caller,
            &policy,
            &caller_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("session-caller"),
            None,
            Some(&config),
            Some(store),
            None,
        )
        .expect("agent_inspect should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(parsed["agent_id"].as_str(), Some(agent_id));
    assert!(parsed["alias"]["revision_id"].is_string());
    assert!(parsed["alias"]["short_ref"].is_string());
    assert!(parsed["revision"]["status"].is_string());
    assert_eq!(parsed["revision"]["trust_domain"].as_str(), Some("local"));
    assert_eq!(parsed["revision"]["artifact_id"].as_str(), Some(artifact_id.as_str()));
    assert!(parsed["skill"]["agent"]["id"].is_string());
    assert!(parsed["files"].as_array().unwrap().len() >= 3);
    assert!(parsed.get("source").is_none(), "source should not be present when include_source is false");
}

#[test]
fn includes_source_when_requested_for_local_agent() {
    let temp = tempdir().unwrap();
    let agent_id = "test.inspect-source";
    let (_artifact_id, gateway_dir, store) = build_and_promote_agent(temp.path(), agent_id);

    let agents_dir = temp.path().join("agents");
    let caller_dir = agents_dir.join("caller");
    std::fs::create_dir_all(&caller_dir).unwrap();
    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };

    let registry = default_registry();
    let caller = manifest_with_read_access("caller");
    let policy = PolicyEngine::new(caller.clone());

    let args = serde_json::json!({
        "agent_id": agent_id,
        "include_source": true,
    });
    let result = registry
        .execute(
            "agent_inspect",
            &caller,
            &policy,
            &caller_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("session-caller"),
            None,
            Some(&config),
            Some(store),
            None,
        )
        .expect("agent_inspect should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed.get("ok").and_then(|v| v.as_bool()), Some(true));

    let source = parsed.get("source").expect("source should be present");
    let source_obj = source.as_object().expect("source should be an object");
    assert!(source_obj.contains_key("SKILL.md"), "source should contain SKILL.md");
    assert!(source_obj.contains_key("main.py"), "source should contain main.py");
    assert!(source_obj.contains_key("runtime.lock"), "source should contain runtime.lock");

    let main_py = source_obj["main.py"].as_str().unwrap();
    assert!(main_py.contains("import json"), "main.py should contain source code");
}

#[test]
fn returns_error_for_unknown_agent() {
    let temp = tempdir().unwrap();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let agents_dir = temp.path().join("agents");
    let caller_dir = agents_dir.join("caller");
    std::fs::create_dir_all(&caller_dir).unwrap();
    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };

    let registry = default_registry();
    let caller = manifest_with_read_access("caller");
    let policy = PolicyEngine::new(caller.clone());

    let args = serde_json::json!({
        "agent_id": "nonexistent.agent",
    });
    let result = registry.execute(
        "agent_inspect",
        &caller,
        &policy,
        &caller_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args).unwrap(),
        Some("session-caller"),
        None,
        Some(&config),
        Some(store),
        None,
    );

    assert!(result.is_err(), "should fail for unknown agent");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not installed"), "error should mention not installed: {err}");
}

#[test]
fn tool_requires_read_access_capability() {
    let manifest_no_read = AgentManifest {
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
            id: "no-read-agent".to_string(),
            name: "no-read-agent".to_string(),
            description: "Test".to_string(),
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
    };

    let tool = autonoetic_gateway::runtime::tools::agent_inspect::AgentInspectTool;
    assert!(!tool.is_available(&manifest_no_read), "should not be available without ReadAccess");
}
