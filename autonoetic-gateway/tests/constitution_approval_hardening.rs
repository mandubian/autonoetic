//! Constitution R++4: Operator approval hardening.
//!
//! Three sub-features:
//! 1. Dwell time — minimum visible seconds before confirm enables for high-risk approvals
//! 2. Typed confirmation string — required for destructive approval classes
//! 3. Operator-facing structural-similarity dedup (tested via similarity score presence)

mod support;

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
        similar_to_request_id: None,
        similarity_score: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
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
        similar_to_request_id: None,
        similarity_score: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
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
fn r4_risk_classification_revision_promote_is_critical() {
    let action = ScheduledAction::RevisionPromote {
        agent_id: "a".to_string(),
        revision_id: "r".to_string(),
        outgoing_revision_id: "o".to_string(),
        added_capabilities: vec![],
        broadened_capabilities: vec![],
        payload: None,
    };
    assert_eq!(classify_approval_risk(&action), ApprovalRisk::Critical);
}

#[test]
fn r4_risk_classification_credential_prompt_is_critical() {
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
fn r4_risk_classification_sandbox_exec_with_hosts_is_high() {
    let action = ScheduledAction::SandboxExec {
        command: "curl https://x.com".to_string(),
        dependencies: None,
        requires_approval: true,
        evidence_ref: None,
        detected_hosts: Some(vec!["x.com".to_string()]),
    };
    assert_eq!(classify_approval_risk(&action), ApprovalRisk::High);
}

#[test]
fn r4_risk_classification_write_file_is_standard() {
    let action = ScheduledAction::WriteFile {
        path: "/tmp/f".to_string(),
        content: "data".to_string(),
        requires_approval: true,
        evidence_ref: None,
    };
    assert_eq!(classify_approval_risk(&action), ApprovalRisk::Standard);
}

#[test]
fn r4_enrich_sets_dwell_and_phrase_for_critical() {
    let mut req = make_critical_request(&chrono::Utc::now().to_rfc3339());
    assert!(req.min_dwell_ms.is_none());
    assert!(req.confirm_phrase.is_none());
    enrich_request(&mut req);
    assert!(req.min_dwell_ms.unwrap() > 0);
    assert!(req.confirm_phrase.is_some());
    let phrase = req.confirm_phrase.unwrap();
    assert!(phrase.contains("promote"));
    assert!(phrase.contains("my.agent"));
}

#[test]
fn r4_enrich_no_dwell_for_standard() {
    let mut req = make_standard_request();
    enrich_request(&mut req);
    assert!(req.min_dwell_ms.is_none());
    assert!(req.confirm_phrase.is_none());
}

#[test]
fn r4_dwell_time_rejects_too_fast() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let just_now = chrono::Utc::now().to_rfc3339();
    let mut req = make_critical_request(&just_now);
    enrich_request(&mut req);
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
        err.to_string().contains("R++4"),
        "error should reference R++4: {}",
        err
    );
    assert!(
        err.to_string().contains("Dwell"),
        "error should mention Dwell: {}",
        err
    );
}

#[test]
fn r4_confirm_phrase_rejects_wrong_phrase() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let old_time = chrono::Utc::now() - chrono::Duration::seconds(30);
    let mut req = make_critical_request(&old_time.to_rfc3339());
    enrich_request(&mut req);
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
        err.to_string().contains("R++4"),
        "error should reference R++4: {}",
        err
    );
    assert!(
        err.to_string().contains("confirm"),
        "error should mention confirm: {}",
        err
    );
}

#[test]
fn r4_confirm_phrase_rejects_missing_phrase() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let old_time = chrono::Utc::now() - chrono::Duration::seconds(30);
    let mut req = make_critical_request(&old_time.to_rfc3339());
    enrich_request(&mut req);
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
    assert!(err.to_string().contains("R++4"));
}

#[test]
fn r4_approve_succeeds_after_dwell_with_correct_phrase() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let old_time = chrono::Utc::now() - chrono::Duration::seconds(30);
    let mut req = make_critical_request(&old_time.to_rfc3339());
    enrich_request(&mut req);
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
fn r4_standard_approval_no_phrase_needed() {
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
fn r4_hardening_persisted_in_store() {
    let temp = tempdir().unwrap();
    let (_gw_dir, store) = setup_gateway(temp.path());

    let mut req = make_critical_request(&chrono::Utc::now().to_rfc3339());
    enrich_request(&mut req);
    let expected_dwell = req.min_dwell_ms;
    let expected_phrase = req.confirm_phrase.clone();
    store.create_approval(&mut req).unwrap();

    let loaded = store.get_approval("apr-critical-test").unwrap().unwrap();
    assert_eq!(loaded.min_dwell_ms, expected_dwell);
    assert_eq!(loaded.confirm_phrase, expected_phrase);
}
