//! Constitution R-2.14 — `user_ask` is blocked while approvals are pending.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
use std::sync::Arc;

fn no_capability_manifest() -> AgentManifest {
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
            id: "test-agent".to_string(),
            name: "test-agent".to_string(),
            description: "test".to_string(),
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
        response_contract: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
    }
}

#[test]
fn r_2_14_user_ask_refused_when_pending_approval_exists() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let root_session_id = "root-r-2-14";
    let mut pending = ApprovalRequest {
        request_id: "apr-r-2-14".to_string(),
        agent_id: "test-agent".to_string(),
        session_id: format!("{root_session_id}/child"),
        action: ScheduledAction::WriteFile {
            path: "/tmp/demo.txt".to_string(),
            content: "data".to_string(),
            requires_approval: true,
            evidence_ref: None,
        },
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
        evidence_ref: None,
        root_session_id: Some(root_session_id.to_string()),
        workflow_id: None,
        task_id: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        similar_to_request_id: None,
        similarity_score: None,
        min_dwell_ms: None,
        confirm_phrase: None,
    };
    store.create_approval(&mut pending)?;

    let manifest = no_capability_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let agent_dir = workspace.path().join("agent");
    std::fs::create_dir_all(&agent_dir)?;

    let raw = registry.execute(
        "user_ask",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        r#"{"question":"Can I continue?"}"#,
        Some(root_session_id),
        Some("turn-r-2-14"),
        Some(&config),
        Some(store),
        None,
    )?;

    let response: serde_json::Value = serde_json::from_str(&raw)?;
    assert_eq!(response["ok"], false);
    assert_eq!(response["error_type"], "conflict");
    let message = response["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("pending approvals"),
        "expected pending-approval block message, got: {message}"
    );

    Ok(())
}
