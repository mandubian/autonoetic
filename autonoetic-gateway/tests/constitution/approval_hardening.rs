//! Constitution P-2.24: Operator approval hardening.
//!
//! Three sub-features:
//! 1. Dwell time — minimum visible seconds before confirm enables for high-risk approvals
//! 2. Typed confirmation string — required for destructive approval classes
//! 3. Operator-facing structural-similarity dedup (tested via similarity score presence)


use autonoetic_gateway::scheduler::approval_hardening::{
    classify_approval_risk, enrich_request, ApprovalRisk,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::{approve_request_with_options, ApproveOptions};
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use tempfile::tempdir;

fn make_critical_request(created_at: &str) -> ApprovalRequest {
    ApprovalRequest {
        request_id: "apr-critical-test".to_string(),
        agent_id: "test.agent".to_string(),
        session_id: "sess/test".to_string(),
        action: ScheduledAction::RevisionPromote {
            agent_id: "my.agent".to_string(),
            revision_id: "rev_sha256:abcdef1234567890".to_string(),
            outgoing_revision_id: "rev_sha256:old".to_string(),
            added_capabilities: vec!["NetworkAccess".to_string()],
            broadened_capabilities: vec![],
            payload: None,
            federation_context: None,
        },
        created_at: created_at.to_string(),
        reason: None,
        evidence_ref: None,
        root_session_id: Some("root/test".to_string()),
        workflow_id: None,
        task_id: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    }
}

fn make_standard_request() -> ApprovalRequest {
    ApprovalRequest {
        request_id: "apr-standard-test".to_string(),
        agent_id: "test.agent".to_string(),
        session_id: "sess/test".to_string(),
        action: ScheduledAction::WriteFile {
            path: "/tmp/test".to_string(),
            content: "hello".to_string(),
            requires_approval: true,
            evidence_ref: None,
        },
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
        evidence_ref: None,
        root_session_id: Some("root/test".to_string()),
        workflow_id: None,
        task_id: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    }
}

fn setup_gateway(base: &std::path::Path) -> (std::path::PathBuf, GatewayStore) {
    let gw_dir = base.join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let store = GatewayStore::open(&gw_dir).unwrap();
    (gw_dir, store)
}

fn config() -> autonoetic_types::config::GatewayConfig {
    autonoetic_types::config::GatewayConfig::default()
}

#[test]
fn p_2_24_risk_classification_revision_promote_is_critical() {
    let action = ScheduledAction::RevisionPromote {
        agent_id: "a".to_string(),
        revision_id: "r".to_string(),
        outgoing_revision_id: "o".to_string(),
        added_capabilities: vec![],
        broadened_capabilities: vec![],
        payload: None,
        federation_context: None,
    };
    assert_eq!(classify_approval_risk(&action), ApprovalRisk::Critical);
}

#[test]
fn p_2_24_risk_classification_credential_prompt_is_critical() {
    let action = ScheduledAction::CredentialPrompt {
        service: "aws".to_string(),
        credential_id: "cred_1".to_string(),
        message: "enter key".to_string(),
        secret_fields: vec![],
        payload: None,
    };
    assert_eq!(classify_approval_risk(&action), ApprovalRisk::Critical);
}

#[test]
fn p_2_24_risk_classification_sandbox_exec_with_hosts_is_high() {
    let action = ScheduledAction::SandboxExec {
        command: "curl https://x.com".to_string(),
        dependencies: None,
        requires_approval: true,
        evidence_ref: None,
        detected_hosts: Some(vec!["x.com".to_string()]),
        detected_mounts: None,
        intent: None,
    };
    assert_eq!(classify_approval_risk(&action), ApprovalRisk::High);
}

#[test]
fn p_2_24_risk_classification_write_file_is_standard() {
    let action = ScheduledAction::WriteFile {
        path: "/tmp/f".to_string(),
        content: "data".to_string(),
        requires_approval: true,
        evidence_ref: None,
    };
    assert_eq!(classify_approval_risk(&action), ApprovalRisk::Standard);
}

#[test]
fn p_2_24_enrich_sets_dwell_and_phrase_for_critical() {
    let mut req = make_critical_request(&chrono::Utc::now().to_rfc3339());
    assert!(req.min_dwell_ms.is_none());
    assert!(req.confirm_phrase.is_none());
    enrich_request(&mut req, None);
    assert!(req.min_dwell_ms.unwrap() > 0);
    assert!(req.confirm_phrase.is_some());
    let phrase = req.confirm_phrase.unwrap();
    assert!(phrase.contains("promote"));
    assert!(phrase.contains("my.agent"));
}

#[test]
fn p_2_24_enrich_no_dwell_for_standard() {
    let mut req = make_standard_request();
    enrich_request(&mut req, None);
    assert!(req.min_dwell_ms.is_none());
    assert!(req.confirm_phrase.is_none());
}

#[test]
fn p_2_24_dwell_time_rejects_too_fast() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let just_now = chrono::Utc::now().to_rfc3339();
    let mut req = make_critical_request(&just_now);
    enrich_request(&mut req, None);
    let min_dwell = req.min_dwell_ms.unwrap();
    assert!(min_dwell > 0);
    store.create_approval(&mut req).unwrap();

    let cfg = config();
    let result = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-critical-test",
        "cli",
        None,
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            confirm_phrase: req.confirm_phrase.clone(),
            ..Default::default()
        },
    );

    let err = result.expect_err("should reject — dwell time not met");
    assert!(
        err.to_string().contains("P-2.24"),
        "error should reference P-2.24: {}",
        err
    );
    assert!(
        err.to_string().contains("Dwell"),
        "error should mention Dwell: {}",
        err
    );
}

#[test]
fn p_2_24_confirm_phrase_rejects_wrong_phrase() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let old_time = chrono::Utc::now() - chrono::Duration::seconds(30);
    let mut req = make_critical_request(&old_time.to_rfc3339());
    enrich_request(&mut req, None);
    store.create_approval(&mut req).unwrap();

    let cfg = config();
    let result = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-critical-test",
        "cli",
        None,
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            confirm_phrase: Some("wrong phrase".to_string()),
            ..Default::default()
        },
    );

    let err = result.expect_err("should reject — wrong confirm phrase");
    assert!(
        err.to_string().contains("P-2.24"),
        "error should reference P-2.24: {}",
        err
    );
    assert!(
        err.to_string().contains("confirm"),
        "error should mention confirm: {}",
        err
    );
}

#[test]
fn p_2_24_confirm_phrase_rejects_missing_phrase() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let old_time = chrono::Utc::now() - chrono::Duration::seconds(30);
    let mut req = make_critical_request(&old_time.to_rfc3339());
    enrich_request(&mut req, None);
    store.create_approval(&mut req).unwrap();

    let cfg = config();
    let result = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-critical-test",
        "cli",
        None,
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions::default(),
    );

    let err = result.expect_err("should reject — missing confirm phrase");
    assert!(err.to_string().contains("P-2.24"));
}

#[test]
fn p_2_24_approve_succeeds_after_dwell_with_correct_phrase() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let old_time = chrono::Utc::now() - chrono::Duration::seconds(30);
    let mut req = make_critical_request(&old_time.to_rfc3339());
    enrich_request(&mut req, None);
    let phrase = req.confirm_phrase.clone();
    store.create_approval(&mut req).unwrap();

    let cfg = config();
    let result = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-critical-test",
        "cli",
        None,
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            confirm_phrase: phrase,
            acknowledged_capabilities: vec!["NetworkAccess".to_string()],
            ..Default::default()
        },
    );

    let decision = result.expect("should succeed — dwell met + correct phrase");
    assert_eq!(decision.status, ApprovalStatus::Approved);
}

#[test]
fn p_2_24_standard_approval_no_phrase_needed() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let mut req = make_standard_request();
    store.create_approval(&mut req).unwrap();

    let cfg = config();
    let result = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-standard-test",
        "cli",
        None,
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions::default(),
    );

    let decision = result.expect("standard approval should not need phrase");
    assert_eq!(decision.status, ApprovalStatus::Approved);
}

#[test]
fn p_2_24_hardening_persisted_in_store() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let mut req = make_critical_request(&chrono::Utc::now().to_rfc3339());
    enrich_request(&mut req, None);
    let expected_dwell = req.min_dwell_ms;
    let expected_phrase = req.confirm_phrase.clone();
    store.create_approval(&mut req).unwrap();

    let loaded = store.get_approval("apr-critical-test").unwrap().unwrap();
    assert_eq!(loaded.min_dwell_ms, expected_dwell);
    assert_eq!(loaded.confirm_phrase, expected_phrase);
}

/// #722 Stage 2 regression: constructing the execution service must wire the
/// runtime config into the store (`GatewayStore::set_config`), so a standalone
/// approval receives a TTL (`expires_at`). Guards against the store's config
/// staying `None` — which silently made standalone approval expiry inert.
#[tokio::test]
async fn standalone_approval_gets_ttl_once_service_wires_store_config() {
    let dir = tempdir().unwrap();
    let gw_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    let store = std::sync::Arc::new(GatewayStore::open(&gw_dir).unwrap());

    let mut cfg = autonoetic_types::config::GatewayConfig::default();
    cfg.standalone_approval_timeout_secs = 3600;

    // Constructing the service is what wires the config into the store.
    let _svc = autonoetic_gateway::execution::GatewayExecutionService::new(cfg, Some(store.clone()));

    let mut req = ApprovalRequest {
        request_id: "apr-ttl-1".to_string(),
        agent_id: "researcher.default".to_string(),
        session_id: "root-x".to_string(),
        action: ScheduledAction::WebFetch {
            url: "https://example.org/x".to_string(),
            timeout_secs: None,
            max_chars: None,
            detected_hosts: Some(vec!["example.org".to_string()]),
            payload: None,
        },
        approval_level: ApprovalLevel::Operator,
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
        evidence_ref: None,
        workflow_id: None, // standalone
        task_id: None,     // standalone
        root_session_id: Some("root-x".to_string()),
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
    store.create_approval(&mut req).unwrap();

    let stored = store
        .get_approval("apr-ttl-1")
        .unwrap()
        .expect("approval exists");
    assert!(
        stored.expires_at.is_some(),
        "standalone approval must receive a TTL once the service wires store config"
    );
}

/// Minimal pending approval carrying `action`, for the #1213 at-rest tests.
fn pending_request(request_id: &str, action: ScheduledAction) -> ApprovalRequest {
    ApprovalRequest {
        request_id: request_id.to_string(),
        agent_id: "test.agent".to_string(),
        session_id: "sess/test".to_string(),
        action,
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
        evidence_ref: None,
        root_session_id: Some("root".to_string()),
        workflow_id: None,
        task_id: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    }
}

// ── #1213: secrets at rest in approvals.action_payload ─────────────────────

/// A rejected gate's turn is dead — the checkpoint is reaped and the command
/// will never run — so the stored payload has nothing left to be raw for.
/// Approved and stale gates are excluded: both remain resolvable, and scrubbing
/// them would leave a command with `***REDACTED***` where a token belongs.
#[test]
fn rejecting_a_gate_scrubs_the_credential_from_its_stored_action() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;

    let mut request = pending_request(
        "apr-scrub",
        ScheduledAction::SandboxExec {
            command: "curl -H 'Authorization: Bearer eyJhbGc.supersecret' https://x".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["x".to_string()]),
            detected_mounts: None,
            intent: None,
        },
    );
    store.create_approval(&mut request)?;

    // Raw on disk while the gate is live: this is the scheduler's execution
    // input, not merely a record.
    let live = store.get_approval("apr-scrub")?.expect("exists");
    assert!(
        matches!(&live.action, ScheduledAction::SandboxExec { command, .. }
            if command.contains("eyJhbGc.supersecret")),
        "a live gate must keep the executable command intact"
    );

    assert!(store.scrub_dead_approval_payload("apr-scrub")?);

    let dead = store.get_approval("apr-scrub")?.expect("exists");
    let ScheduledAction::SandboxExec { command, detected_hosts, .. } = &dead.action else {
        panic!("expected SandboxExec");
    };
    assert!(
        !command.contains("eyJhbGc.supersecret"),
        "credential survived at rest: {command}"
    );
    assert!(
        command.contains("curl") && command.contains("https://x"),
        "the shape a reviewer reads must survive: {command}"
    );
    assert_eq!(
        detected_hosts.as_deref(),
        Some(&["x".to_string()][..]),
        "structural fields are untouched"
    );
    Ok(())
}

/// Retention prunes decided approvals, and deliberately never prunes pending
/// ones — an unanswered gate is outstanding work, not stale data.
#[test]
fn retention_prunes_decided_approvals_but_never_pending_ones() -> anyhow::Result<()> {
    use autonoetic_types::config::RetentionConfig;

    let temp = tempfile::tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;

    let action = || ScheduledAction::SandboxExec {
        command: "echo hi".to_string(),
        dependencies: None,
        requires_approval: true,
        evidence_ref: None,
        detected_hosts: None,
        detected_mounts: None,
        intent: None,
    };

    let mut old_decided = pending_request("apr-old", action());
    store.create_approval(&mut old_decided)?;
    let long_ago = (chrono::Utc::now() - chrono::Duration::days(400)).to_rfc3339();
    store.record_decision("apr-old", "approved", "operator", &long_ago, Some("ok"))?;

    let mut still_pending = pending_request("apr-pending", action());
    store.create_approval(&mut still_pending)?;

    store.apply_retention_policy(&RetentionConfig {
        approvals_days: 90,
        ..Default::default()
    })?;

    assert!(
        store.get_approval("apr-old")?.is_none(),
        "a decision from 400 days ago should be pruned at a 90-day policy"
    );
    assert!(
        store.get_approval("apr-pending")?.is_some(),
        "a pending gate must never be reaped by retention — it is work the \
         operator still owes a decision on, at any age"
    );
    Ok(())
}
