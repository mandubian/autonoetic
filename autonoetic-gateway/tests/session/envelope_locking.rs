//! Integration tests for session envelope locking and materialization (#505).

use autonoetic_gateway::runtime::session_envelope::{
    envelope_expansion_hint, lock_session_envelope, propose_discovered_envelope,
    revoke_session_envelope,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::{
    list_session_envelopes, lock_session_envelope_operator, propose_session_envelope,
};
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
use autonoetic_types::causal_chain::ExecutionTraceRecord;
use tempfile::tempdir;

fn curl_trace(session_id: &str, command: &str) -> ExecutionTraceRecord {
    ExecutionTraceRecord {
        trace_id: format!("trace-{}", uuid::Uuid::new_v4()),
        event_id: None,
        agent_id: "researcher.default".to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        timestamp: "2026-06-14T12:00:00Z".to_string(),
        tool_name: "sandbox_exec".to_string(),
        command: Some(command.to_string()),
        exit_code: Some(0),
        stdout: None,
        stderr: None,
        duration_ms: 10,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: Some(format!(r#"{{"command":"{command}"}}"#)),
        result: None,
        egress_label: None,
    }
}

#[test]
fn lock_flow_materializes_grants_for_observed_host() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = GatewayStore::open(dir.path())?;
    let root = "session-505-flow";

    store.create_execution_trace(&curl_trace(
        root,
        "curl -s https://api.open-meteo.com/v1/forecast",
    ))?;

    // propose_discovered_envelope now auto-locks and materializes grants.
    let proposal = propose_discovered_envelope(&store, root, "discovered", None, "operator")?
        .expect("proposal");
    assert!(store.session_grants_cover_targets(root, "researcher.default", &["api.open-meteo.com".to_string()]));
    // The envelope should be locked (active), not pending (proposed).
    assert!(store.get_proposed_envelopes(root)?.is_empty());
    assert_eq!(store.get_active_envelopes(root)?.len(), 1);
    let _ = proposal;
    Ok(())
}

#[test]
fn locked_envelope_covers_host_outside_lock_still_needs_approval() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = GatewayStore::open(dir.path())?;
    let root = "session-505-expansion";

    store.create_execution_trace(&curl_trace(
        root,
        "curl -s https://api.open-meteo.com/v1/forecast",
    ))?;

    // propose_session_envelope auto-locks — no manual lock needed.
    let proposal =
        propose_session_envelope(&store, root, "operator", None, "operator")?.expect("proposal");

    assert!(store.session_grants_cover_targets(root, "researcher.default", &["api.open-meteo.com".to_string()]));
    assert!(
        !store.session_grants_cover_targets(root, "researcher.default", &["geocoding-api.open-meteo.com".to_string()])
    );

    let hint = envelope_expansion_hint(
        &store,
        root,
        &[
            "api.open-meteo.com".to_string(),
            "geocoding-api.open-meteo.com".to_string(),
        ],
    );
    assert!(
        hint.is_none(),
        "locked host should not trigger expansion hint"
    );
    let _ = proposal;
    Ok(())
}

#[test]
fn explicit_propose_lists_observed_hosts() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = GatewayStore::open(dir.path())?;
    let root = "session-505-propose";

    store.create_execution_trace(&curl_trace(
        root,
        "curl -s https://api.open-meteo.com/v1/forecast",
    ))?;

    let list_before = list_session_envelopes(&store, root)?;
    assert_eq!(
        list_before.observed_hosts,
        vec!["api.open-meteo.com".to_string()]
    );
    assert!(list_before.proposed.is_empty());

    // propose_session_envelope auto-locks, so the result is an active envelope.
    let proposal =
        propose_session_envelope(&store, root, "operator", None, "operator")?.expect("proposal");
    assert!(!proposal.skipped);

    let list_after = list_session_envelopes(&store, root)?;
    assert_eq!(list_after.active.len(), 1);
    assert!(list_after.pending_hosts.is_empty());
    Ok(())
}

#[test]
fn approval_timeline_gets_expansion_hint_for_observed_host() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = GatewayStore::open(dir.path())?;
    let root = "session-505-hint-timeline";

    store.create_execution_trace(&curl_trace(
        root,
        "curl -s https://api.open-meteo.com/v1/forecast",
    ))?;

    let mut approval = ApprovalRequest {
        request_id: "apr-expansion".to_string(),
        agent_id: "coder.default".to_string(),
        session_id: root.to_string(),
        action: ScheduledAction::SandboxExec {
            command: "curl https://geocoding-api.open-meteo.com/v1/search".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec![
                "api.open-meteo.com".to_string(),
                "geocoding-api.open-meteo.com".to_string(),
            ]),
            intent: None,
        },
        approval_level: ApprovalLevel::Operator,
        created_at: "2026-06-14T12:00:00Z".to_string(),
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

        expires_at: None,
    };
    store.create_approval(&mut approval)?;

    let timeline = store.list_session_timeline(root, None, 10, None, None)?;
    let pending = timeline
        .entries
        .iter()
        .find(|e| e.event_type == "approval.pending")
        .expect("approval.pending event");
    let payload: serde_json::Value =
        serde_json::from_str(pending.payload.as_deref().unwrap_or("{}"))?;
    assert!(payload.get("envelope_expansion_hint").is_some());
    Ok(())
}

#[test]
fn revoke_locked_envelope_revokes_grants_and_removes_row() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = GatewayStore::open(dir.path())?;
    let root = "session-511-revoke";

    store.create_execution_trace(&curl_trace(
        root,
        "curl -s https://api.open-meteo.com/v1/forecast",
    ))?;

    // propose_discovered_envelope auto-locks — no manual lock needed.
    let proposal = propose_discovered_envelope(&store, root, "discovered", None, "operator")?
        .expect("proposal");
    assert!(store.session_grants_cover_targets(root, "researcher.default", &["api.open-meteo.com".to_string()]));

    let revoked =
        revoke_session_envelope(&store, proposal.envelope_id, "operator")?.expect("revoked record");
    assert_eq!(revoked.root_session_id, root);
    assert!(revoked.locked_at.is_some());
    assert!(store.get_envelope_by_id(proposal.envelope_id)?.is_none());
    assert!(!store.session_grants_cover_targets(root, "researcher.default", &["api.open-meteo.com".to_string()]));
    Ok(())
}

#[test]
fn revoke_missing_envelope_returns_none() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let store = GatewayStore::open(dir.path())?;
    assert!(revoke_session_envelope(&store, 999_999, "operator")?.is_none());
    assert!(!store.revoke_session_envelope_by_id(999_999)?);
    Ok(())
}
