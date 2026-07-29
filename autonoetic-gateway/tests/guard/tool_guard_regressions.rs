use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::background::{UserInteraction, UserInteractionKind, UserInteractionStatus};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde_json::json;
use tempfile::tempdir;

fn manifest(agent_id: &str, capabilities: Vec<Capability>) -> AgentManifest {
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
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities,
        llm_overrides: None,
        llm_preset: None,
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
            excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        egress: None,
        }
}

#[test]
fn test_impl_artifact_and_cnt_handle_guards() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir)?;

    let cfg = GatewayConfig {
        agents_dir,
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(temp.path())?);
    let registry = default_registry();

    let manifest = manifest(
        "executor.default",
        vec![
            Capability::CodeExecution {
                patterns: vec!["*".to_string()],
                commands: vec![],
            },
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
        ],
    );
    let policy = PolicyEngine::new(manifest.clone());

    let exec = registry.execute(
        "artifact_exec",
        &manifest,
        &policy,
        temp.path(),
        Some(temp.path()),
        &json!({
            "artifact_ref": "impl_task-1234",
            "entrypoint": "main.py"
        })
        .to_string(),
        Some("root-1/session-1"),
        None,
        Some(&cfg),
        Some(store.clone()),
        None,
    );
    let exec_err = exec.expect_err("unknown artifact_ref should fail");
    assert!(
        exec_err
            .to_string()
            .contains("artifact_ref 'impl_task-1234' not found"),
        "unexpected artifact_exec error: {exec_err}"
    );

    let inspect = registry.execute(
        "artifact_inspect",
        &manifest,
        &policy,
        temp.path(),
        Some(temp.path()),
        &json!({
            "artifact_ref": "impl_task-1234"
        })
        .to_string(),
        Some("root-1/session-1"),
        None,
        Some(&cfg),
        Some(store.clone()),
        None,
    );
    let inspect_err = inspect.expect_err("unknown artifact_ref should fail inspect");
    assert!(
        inspect_err
            .to_string()
            .contains("artifact_ref 'impl_task-1234' not found"),
        "unexpected artifact_inspect error: {inspect_err}"
    );

    let sandbox_with_impl = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        temp.path(),
        Some(temp.path()),
        &json!({
            "command": "python3 /tmp/main.py",
            "artifact_id": "impl_task-1234"
        })
        .to_string(),
        Some("root-1/session-1"),
        None,
        Some(&cfg),
        Some(store.clone()),
        None,
    )?;
    let sandbox_impl_json: serde_json::Value = serde_json::from_str(&sandbox_with_impl)?;
    assert_eq!(sandbox_impl_json["ok"], false);
    assert_eq!(sandbox_impl_json["error"], "invalid_artifact_id");

    let cnt_misuse = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        temp.path(),
        Some(temp.path()),
        &json!({
            "command": "cat /tmp/cnt_deadbeef"
        })
        .to_string(),
        Some("root-1/session-1"),
        None,
        Some(&cfg),
        Some(store.clone()),
        None,
    )?;
    let cnt_misuse_json: serde_json::Value = serde_json::from_str(&cnt_misuse)?;
    assert_eq!(cnt_misuse_json["ok"], true);
    assert_eq!(cnt_misuse_json["command_succeeded"], false);
    let stderr = cnt_misuse_json["stderr"].as_str().unwrap_or_default();
    assert!(
        stderr.contains("cnt_deadbeef"),
        "expected natural exec-time missing file error mentioning path, got stderr: {stderr}"
    );
    assert!(
        !stderr.contains("content handles (cnt_...) are not filesystem paths"),
        "gateway heuristic message should not appear: {stderr}"
    );

    Ok(())
}

#[test]
fn test_user_interaction_status_scope_enforced() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let store = Arc::new(GatewayStore::open(temp.path())?);
    let registry = default_registry();

    let owner_manifest = manifest("owner.default", vec![]);
    let owner_policy = PolicyEngine::new(owner_manifest.clone());

    let interaction = UserInteraction {
        interaction_id: "ui-scope-1".to_string(),
        session_id: "root-a/owner-1".to_string(),
        root_session_id: "root-a".to_string(),
        workflow_id: None,
        task_id: None,
        agent_id: owner_manifest.agent.id.clone(),
        turn_id: "turn-1".to_string(),
        kind: UserInteractionKind::Clarification,
        question: "Proceed?".to_string(),
        context: None,
        options: vec![],
        allow_freeform: true,
        status: UserInteractionStatus::Pending,
        answer_option_id: None,
        answer_text: None,
        answered_by: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        answered_at: None,
        expires_at: None,
        checkpoint_turn_id: None,
    };
    store.create_user_interaction(&interaction)?;

    let peer_manifest = manifest("peer.default", vec![]);
    let peer_policy = PolicyEngine::new(peer_manifest.clone());
    let same_root = registry.execute(
        "user_interaction_status",
        &peer_manifest,
        &peer_policy,
        temp.path(),
        None,
        &json!({ "interaction_id": "ui-scope-1" }).to_string(),
        Some("root-a/peer-1"),
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let same_root_json: serde_json::Value = serde_json::from_str(&same_root)?;
    assert_eq!(same_root_json["ok"], true);
    assert_eq!(same_root_json["status"], "pending");

    let foreign_manifest = manifest("foreign.default", vec![]);
    let foreign_policy = PolicyEngine::new(foreign_manifest.clone());
    let foreign = registry.execute(
        "user_interaction_status",
        &foreign_manifest,
        &foreign_policy,
        temp.path(),
        None,
        &json!({ "interaction_id": "ui-scope-1" }).to_string(),
        Some("root-b/foreign-1"),
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let foreign_json: serde_json::Value = serde_json::from_str(&foreign)?;
    assert_eq!(foreign_json["ok"], false);
    assert_eq!(foreign_json["error_type"], "permission");

    let owner_access = registry.execute(
        "user_interaction_status",
        &owner_manifest,
        &owner_policy,
        temp.path(),
        None,
        &json!({ "interaction_id": "ui-scope-1" }).to_string(),
        None,
        None,
        None,
        Some(store),
        None,
    )?;
    let owner_json: serde_json::Value = serde_json::from_str(&owner_access)?;
    assert_eq!(owner_json["ok"], true);
    assert_eq!(owner_json["status"], "pending");

    Ok(())
}
