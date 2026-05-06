//! `agent.install` has been removed; these tests ensure calls fail fast.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;

fn evolution_manifest() -> AgentManifest {
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
            id: "specialized_builder.default".to_string(),
            name: "specialized_builder.default".to_string(),
            description: "Builder".to_string(),
        },
        capabilities: vec![Capability::AgentSpawn {
            max_children: 10,
            max_spawn_depth: 0,
        }],
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
    }
}

#[tokio::test]
async fn test_agent_install_is_unavailable() {
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).expect("builder dir should create");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    let store = ContentStore::new(&gateway_dir).unwrap();
    let content_handle = store.write(b"print(1)").unwrap();
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
    let session_id = "test-session";
    store
        .register_name(session_id, "main.py", &content_handle)
        .unwrap();
    let bundle = artifact_store
        .build(&["main.py".to_string()], None, None, session_id)
        .unwrap();

    let registry = default_registry();
    let install_args = serde_json::json!({
        "agent_id": "legacy.agent",
        "name": "Legacy",
        "instructions": "# Legacy",
        "capabilities": [],
        "artifact_id": bundle.artifact_id,
    });

    let err = registry
        .execute(
            "agent.install",
            &evolution_manifest(),
            &PolicyEngine::new(evolution_manifest()),
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&install_args).unwrap(),
            Some("session-unavailable"),
            None,
            Some(&config),
            None,
            None,
        )
        .expect_err("agent.install must not be available");

    let msg = err.to_string();
    assert!(
        msg.contains("Unknown native tool"),
        "expected unavailable-tool error, got: {}",
        msg
    );
}
