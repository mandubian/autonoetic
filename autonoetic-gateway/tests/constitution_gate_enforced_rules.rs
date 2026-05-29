//! R+++3 compliance for GateService: every gate decision carries enforced rule IDs.
//!
//! These tests verify that `GateResult` variants produced by `GateService::check()`
//! include the correct constitutional rule references, enabling real-time compliance
//! reporting and dead-rule detection.

use std::sync::Arc;

use anyhow::Result;

use autonoetic_gateway::runtime::human_gate::{
    ClearanceSource, GateKind, GateRequest, GateResult, GateService, MatchStrategy,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, GrantScope, GrantTarget, ScheduledAction,
};

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
            id: "test-agent".to_string(),
            name: "test-agent".to_string(),
            description: "test agent".to_string(),
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

fn make_action(url: &str) -> ScheduledAction {
    ScheduledAction::CredentialRequest {
        credential_id: "cred-test".to_string(),
        url: url.to_string(),
        method: Some("GET".to_string()),
        headers: None,
        body: None,
        inject_secret_as: None,
        payload: None,
    }
}

/// R+++3: pre-validated bypass records P-2.6.
#[test]
fn r3_pre_validated_bypass_enforces_r_2_6() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let svc = GateService::new(Arc::new(GatewayStore::open(tmp.path())?));
    let manifest = test_manifest();

    let result = svc.check(GateRequest {
        kind: GateKind::Approval {
            action: make_action("http://localhost:8080/api"),
            targets: vec!["localhost".to_string()],
            match_strategy: MatchStrategy::HostLevel,
        },
        manifest: &manifest,
        session_id: Some("ses-1"),
        run_context: None,
        config: None,
        reason: "test".into(),
        summary: "test".into(),
        approval_ref: None,
        pre_validated: true,
        turn_id: None,
    })?;

    assert!(result.is_cleared());
    let rules = result.enforced_rules();
    assert!(
        rules.contains(&"P-2.6"),
        "pre-validated bypass must record P-2.6, got {:?}",
        rules
    );
    Ok(())
}

/// R+++3: session grant clearance records P-2.4.
#[test]
fn r3_session_grant_clearance_enforces_r_2_4() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let svc = GateService::new(store.clone());
    let manifest = test_manifest();

    store.insert_session_grant(
        "ses-grant",
        "ses-grant",
        "test-agent",
        &GrantScope::RootSession,
        &[GrantTarget::ExactHost("localhost".to_string())],
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;

    let result = svc.check(GateRequest {
        kind: GateKind::Approval {
            action: make_action("http://localhost:8080/api"),
            targets: vec!["localhost".to_string()],
            match_strategy: MatchStrategy::HostLevel,
        },
        manifest: &manifest,
        session_id: Some("ses-grant"),
        run_context: None,
        config: None,
        reason: "test".into(),
        summary: "test".into(),
        approval_ref: None,
        pre_validated: false,
        turn_id: None,
    })?;

    assert!(result.is_cleared());
    let rules = result.enforced_rules();
    assert!(
        rules.contains(&"P-2.4"),
        "session grant clearance must record P-2.4, got {:?}",
        rules
    );
    Ok(())
}

/// R+++3: dedup returns AlreadyPending with P-2.3.
#[test]
fn r3_dedup_enforces_r_2_3() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let svc = GateService::new(Arc::new(GatewayStore::open(tmp.path())?));
    let manifest = test_manifest();
    let sid = "ses-dedup";
    let action = make_action("http://localhost:8080/api");

    let _ = svc.check(GateRequest {
        kind: GateKind::Approval {
            action: action.clone(),
            targets: vec!["localhost".to_string()],
            match_strategy: MatchStrategy::HostLevel,
        },
        manifest: &manifest,
        session_id: Some(sid),
        run_context: None,
        config: None,
        reason: "first".into(),
        summary: "first".into(),
        approval_ref: None,
        pre_validated: false,
        turn_id: None,
    })?;

    let result2 = svc.check(GateRequest {
        kind: GateKind::Approval {
            action,
            targets: vec!["localhost".to_string()],
            match_strategy: MatchStrategy::HostLevel,
        },
        manifest: &manifest,
        session_id: Some(sid),
        run_context: None,
        config: None,
        reason: "second".into(),
        summary: "second".into(),
        approval_ref: None,
        pre_validated: false,
        turn_id: None,
    })?;

    assert!(matches!(result2, GateResult::AlreadyPending { .. }));
    let rules = result2.enforced_rules();
    assert!(
        rules.contains(&"P-2.3"),
        "dedup must record P-2.3, got {:?}",
        rules
    );
    Ok(())
}

/// R+++3: new approval suspension records P-2.1, P-2.2, P-2.18.
#[test]
fn r3_new_approval_enforces_r_2_1_r_2_2_r_2_18() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let svc = GateService::new(Arc::new(GatewayStore::open(tmp.path())?));
    let manifest = test_manifest();

    let result = svc.check(GateRequest {
        kind: GateKind::Approval {
            action: make_action("http://localhost:8080/api"),
            targets: vec!["localhost".to_string()],
            match_strategy: MatchStrategy::HostLevel,
        },
        manifest: &manifest,
        session_id: Some("ses-new"),
        run_context: None,
        config: None,
        reason: "network access".into(),
        summary: "fetch API".into(),
        approval_ref: None,
        pre_validated: false,
        turn_id: None,
    })?;

    assert!(matches!(result, GateResult::Suspended { .. }));
    let rules = result.enforced_rules();
    assert!(rules.contains(&"P-2.1"), "must record P-2.1, got {:?}", rules);
    assert!(rules.contains(&"P-2.2"), "must record P-2.2, got {:?}", rules);
    assert!(rules.contains(&"P-2.18"), "must record P-2.18, got {:?}", rules);
    Ok(())
}

/// R+++3: approval_ref clearance records P-2.6.
#[test]
fn r3_approval_ref_clearance_enforces_r_2_6() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let svc = GateService::new(store.clone());
    let manifest = test_manifest();
    let sid = "ses-ref";
    let action = make_action("http://localhost:8080/api");

    let ref_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let mut approval = ApprovalRequest {
        request_id: ref_id.clone(),
        agent_id: manifest.agent.id.clone(),
        session_id: sid.to_string(),
        root_session_id: Some(sid.to_string()),
        workflow_id: None,
        task_id: None,
        action: action.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        status: None,
        decided_at: None,
        decided_by: None,
        reason: Some("test".to_string()),
        evidence_ref: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        similar_to_request_id: None,
        similarity_score: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
    };
    store.create_approval(&mut approval)?;
    store.record_decision(
        &ref_id,
        "approved",
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
    )?;

    let result = svc.check(GateRequest {
        kind: GateKind::Approval {
            action,
            targets: vec!["localhost".to_string()],
            match_strategy: MatchStrategy::HostLevel,
        },
        manifest: &manifest,
        session_id: Some(sid),
        run_context: None,
        config: None,
        reason: "test".into(),
        summary: "test".into(),
        approval_ref: Some(&ref_id),
        pre_validated: false,
        turn_id: None,
    })?;

    assert!(result.is_cleared());
    let rules = result.enforced_rules();
    assert!(
        rules.contains(&"P-2.6"),
        "approval_ref clearance must record P-2.6, got {:?}",
        rules
    );
    match result {
        GateResult::Cleared {
            source: ClearanceSource::ApprovalRef(_),
            ..
        } => {}
        other => panic!("expected Cleared(ApprovalRef), got {:?}", other),
    }
    Ok(())
}

/// R+++3: user_input gate records P-2.13 and P-2.18.
#[test]
fn r3_user_input_gate_enforces_r_2_13_r_2_18() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let svc = GateService::new(Arc::new(GatewayStore::open(tmp.path())?));
    let manifest = test_manifest();

    let result = svc.check(GateRequest {
        kind: GateKind::UserInput {
            question: "Which environment?".into(),
            kind: "clarification".into(),
            options: None,
            allow_freeform: true,
            context: None,
        },
        manifest: &manifest,
        session_id: Some("ses-ui"),
        run_context: None,
        config: None,
        reason: String::new(),
        summary: String::new(),
        approval_ref: None,
        pre_validated: false,
        turn_id: None,
    })?;

    assert!(matches!(result, GateResult::Suspended { .. }));
    let rules = result.enforced_rules();
    assert!(rules.contains(&"P-2.13"), "must record P-2.13, got {:?}", rules);
    assert!(rules.contains(&"P-2.18"), "must record P-2.18, got {:?}", rules);
    Ok(())
}

/// R+++3: escalation gate records P-2.18.
#[test]
fn r3_escalation_gate_enforces_r_2_18() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let svc = GateService::new(Arc::new(GatewayStore::open(tmp.path())?));
    let manifest = test_manifest();

    let result = svc.check(GateRequest {
        kind: GateKind::Escalation {
            reason: "Policy ambiguity".into(),
        },
        manifest: &manifest,
        session_id: Some("ses-esc"),
        run_context: None,
        config: None,
        reason: String::new(),
        summary: String::new(),
        approval_ref: None,
        pre_validated: false,
        turn_id: None,
    })?;

    assert!(matches!(result, GateResult::Suspended { .. }));
    let rules = result.enforced_rules();
    assert!(rules.contains(&"P-2.18"), "must record P-2.18, got {:?}", rules);
    Ok(())
}

/// P-2.19: gate enrichment messages are recorded with sender and content.
#[test]
fn r_2_19_gate_enrichment_recorded_with_sender() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let svc = GateService::new(store.clone());
    let manifest = test_manifest();

    let result = svc.check(GateRequest {
        kind: GateKind::Approval {
            action: make_action("http://api.example.com/data"),
            targets: vec!["api.example.com".to_string()],
            match_strategy: MatchStrategy::HostLevel,
        },
        manifest: &manifest,
        session_id: Some("ses-enrich"),
        run_context: None,
        config: None,
        reason: "API access".into(),
        summary: "fetch data".into(),
        approval_ref: None,
        pre_validated: false,
        turn_id: None,
    })?;

    let gate_id = match result {
        GateResult::Suspended { gate_id, .. } => gate_id,
        other => panic!("expected Suspended, got {:?}", other),
    };

    svc.add_gate_message(&gate_id, "operator", "What is the API used for?")?;
    svc.add_gate_message(&gate_id, "system", "Agent context: data retrieval for analysis")?;

    let msgs = svc.get_gate_messages(&gate_id)?;
    assert!(msgs.len() >= 3, "should have seed + 2 added messages");

    assert_eq!(msgs[0].sender, "system");
    assert_eq!(msgs[1].sender, "operator");
    assert!(msgs[1].content.contains("What is the API used for?"));
    assert_eq!(msgs[2].sender, "system");
    assert!(msgs[2].content.contains("data retrieval"));

    for msg in &msgs {
        assert!(!msg.sender.is_empty(), "sender must be non-empty");
        assert!(!msg.created_at.is_empty(), "created_at must be recorded");
    }

    Ok(())
}
