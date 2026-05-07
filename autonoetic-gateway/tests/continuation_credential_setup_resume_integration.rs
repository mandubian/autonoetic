use autonoetic_gateway::runtime::continuation::{execute_approved_action, PendingApprovalToolCall};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;

fn test_manifest(agent_id: &str) -> AgentManifest {
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
            description: "test".to_string(),
        },
        capabilities: vec![Capability::CredentialAccess {
            services: vec!["*".to_string()],
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

#[test]
fn approved_credential_setup_remote_gate_replays_credential_setup_tool() {
    let temp = tempdir().expect("tempdir should create");
    let agent_dir = temp.path().join("registration.default");
    std::fs::create_dir_all(&agent_dir).expect("agent dir should create");
    std::fs::write(
        agent_dir.join("SKILL.md"),
        r#"---
metadata:
  autonoetic:
    remote_access:
      approval_mode: "required"
      targets:
        - kind: "any"
---
"#,
    )
    .expect("skill should write");

    let manifest = test_manifest("registration.default");
    let config = GatewayConfig {
        agents_dir: temp.path().join("agents"),
        ..GatewayConfig::default()
    };

    let decision = ApprovalDecision {
        request_id: "apr-test-setup".to_string(),
        agent_id: "registration.default".to_string(),
        session_id: "demo-session-1/registration.default-abc123".to_string(),
        action: ScheduledAction::CredentialRequest {
            credential_id: "".to_string(),
            url: "http://localhost:9876/skill.md".to_string(),
            method: Some("GET".to_string()),
            headers: None,
            body: None,
            inject_secret_as: None,
            payload: Some(serde_json::json!({
                "source_tool": "credential_setup",
                "setup_phase": "skill_url"
            })),
        },
        status: ApprovalStatus::Approved,
        decided_at: chrono::Utc::now().to_rfc3339(),
        decided_by: "tester".to_string(),
        reason: None,
        root_session_id: Some("demo-session-1".to_string()),
        workflow_id: Some("wf-test".to_string()),
        task_id: Some("task-test".to_string()),
        approval_level: ApprovalLevel::Operator,
    };

    let pending = PendingApprovalToolCall {
        call_id: "call-1".to_string(),
        tool_name: "credential_setup".to_string(),
        arguments: serde_json::json!({
            "intent": "resume credential setup",
            "skill_url": "http://localhost:9876/skill.md"
        })
        .to_string(),
        approval_response: "{}".to_string(),
    };

    let result = execute_approved_action(
        &decision,
        &manifest,
        &agent_dir,
        None,
        Some("demo-session-1/registration.default-abc123"),
        &config,
        None,
        Some(&pending),
    )
    .expect("execute_approved_action should run credential_setup path");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("result should be valid json");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "resource");
    assert!(
        parsed["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Gateway store not available"),
        "unexpected payload: {}",
        result
    );
}
