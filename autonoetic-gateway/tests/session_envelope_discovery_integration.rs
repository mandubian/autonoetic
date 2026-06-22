//! Integration tests for session envelope data layer (#501).

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
use autonoetic_types::capability::Capability;
use autonoetic_types::causal_chain::ExecutionTraceRecord;
use tempfile::tempdir;

#[test]
fn discover_observed_hosts_extracts_urls_from_traces() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = GatewayStore::open(dir.path())?;
    let root = "session-env-trace";

    store.create_execution_trace(&ExecutionTraceRecord {
        trace_id: "trace-web".to_string(),
        event_id: None,
        agent_id: "researcher.default".to_string(),
        session_id: root.to_string(),
        turn_id: None,
        timestamp: "2026-06-14T10:00:00Z".to_string(),
        tool_name: "web_fetch".to_string(),
        command: None,
        exit_code: Some(0),
        stdout: None,
        stderr: None,
        duration_ms: 50,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: Some(r#"{"url":"https://api.open-meteo.com/v1/forecast"}"#.to_string()),
        result: None,
    })?;

    let hosts = store.discover_observed_hosts(root)?;
    assert_eq!(hosts, vec!["api.open-meteo.com".to_string()]);
    Ok(())
}

#[test]
fn session_envelope_migration_and_lifecycle() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = GatewayStore::open(dir.path())?;
    let root = "session-env-lifecycle";

    let capability = Capability::NetworkAccess {
        hosts: vec!["api.open-meteo.com".to_string()],
    };
    let proposal_id = store.insert_envelope_proposal(
        root,
        &capability,
        "discovered",
        Some("2026-06-14T10:00:00Z"),
        Some("plan-weather"),
        "2026-06-14T10:05:00Z",
    )?;

    assert_eq!(store.get_proposed_envelopes(root)?.len(), 1);
    assert!(store.get_active_envelopes(root)?.is_empty());

    assert!(store.lock_envelope(proposal_id, "operator", "2026-06-14T10:10:00Z")?);

    let active = store.get_active_envelopes(root)?;
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].source, "discovered");
    assert_eq!(active[0].plan_id.as_deref(), Some("plan-weather"));
    assert!(matches!(
        &active[0].capability,
        Capability::NetworkAccess { hosts } if hosts == &["api.open-meteo.com"]
    ));

    // Re-open store to verify migration persisted.
    drop(store);
    let store = GatewayStore::open(dir.path())?;
    assert_eq!(store.get_active_envelopes(root)?.len(), 1);
    Ok(())
}

#[test]
fn discover_observed_hosts_includes_approved_network_actions() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = GatewayStore::open(dir.path())?;
    let root = "session-env-approval";

    store.create_approval(&mut ApprovalRequest {
        request_id: "apr-env-1".to_string(),
        agent_id: "researcher.default".to_string(),
        session_id: root.to_string(),
        action: ScheduledAction::SandboxExec {
            command: "curl https://geocoding-api.open-meteo.com/v1/search".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["geocoding-api.open-meteo.com".to_string()]),
            intent: None,
        },
        approval_level: ApprovalLevel::Operator,
        created_at: "2026-06-14T10:00:00Z".to_string(),
        reason: None,
        evidence_ref: None,
        workflow_id: None,
        task_id: None,
        root_session_id: Some(root.to_string()),
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
    })?;
    store.record_decision(
        "apr-env-1",
        "approved",
        "operator",
        "2026-06-14T10:01:00Z",
        None,
    )?;

    let hosts = store.discover_observed_hosts(root)?;
    assert_eq!(hosts, vec!["geocoding-api.open-meteo.com".to_string()]);
    Ok(())
}
