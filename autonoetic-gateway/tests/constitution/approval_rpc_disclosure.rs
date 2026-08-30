//! #1233: no operator-facing approval surface may emit a credential.
//!
//! Every redaction test before this one asserted on `redact_for_viewer` — the
//! *function*. None asserted on what an RPC actually returns, and the function
//! turned out to have exactly one caller (the agent-facing tool), so the
//! operator surfaces serialized the stored record verbatim while a PR titled
//! "operators read the command, not the credential inside it" claimed
//! otherwise.
//!
//! These tests are deliberately written at the **exit**: build a record
//! carrying known secrets in every field that can hold one, run it through the
//! redaction the surface applies, and assert the secrets are absent from the
//! serialized bytes. A test on the redactor cannot catch an unwired surface.

use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, CodeExcerpt, RiskSummary, ScheduledAction,
};
use autonoetic_types::disclosure::ViewerClass;

const TOKEN: &str = "eyJhbGc.supersecrettoken";
const PASSWORD: &str = "hunter2wearingahat";
const URL_TOKEN: &str = "abc123urlsecret";

/// An approval carrying a credential in *every* field that can hold one.
fn secret_bearing_request() -> ApprovalRequest {
    ApprovalRequest {
        request_id: "apr-disclosure".to_string(),
        agent_id: "coder.default".to_string(),
        session_id: "root/coder".to_string(),
        action: ScheduledAction::SandboxExec {
            command: format!("curl -H 'Authorization: Bearer {TOKEN}' https://x?token={URL_TOKEN}"),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["x".to_string()]),
            intent: None,
        },
        created_at: chrono::Utc::now().to_rfc3339(),
        // Agent-written free text.
        reason: Some(format!("need to call the API with Bearer {TOKEN}")),
        evidence_ref: None,
        root_session_id: Some("root".to_string()),
        workflow_id: None,
        task_id: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: Some(format!("operator note: PASSWORD={PASSWORD}")),
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        // Artifact source travels with the gate — a script with an inlined key.
        code_excerpts: Some(vec![CodeExcerpt {
            file_name: "deploy.sh".to_string(),
            content: format!("#!/bin/sh\nexport API_KEY={PASSWORD}\ncurl -H 'Authorization: Bearer {TOKEN}' https://x"),
            language: "shell".to_string(),
            size_bytes: 64,
            truncated: false,
            truncated_from_bytes: None,
        }]),
        risk_summary: Some(RiskSummary {
            host_count: 1,
            protocol_mix: vec!["https".to_string()],
            dangerous_patterns: vec![format!("inline credential: Bearer {TOKEN}")],
            auditor_verdict: Some(format!("saw PASSWORD={PASSWORD} in the script")),
            auditor_findings_link: None,
        }),
        expires_at: None,
    }
}

fn assert_no_secrets(label: &str, blob: &str) {
    for (name, secret) in [
        ("bearer token", TOKEN),
        ("password", PASSWORD),
        ("url token", URL_TOKEN),
    ] {
        assert!(
            !blob.contains(secret),
            "{label}: {name} survived into an operator-facing payload\n{blob}"
        );
    }
}

#[test]
fn the_operator_view_of_an_approval_carries_no_credential_in_any_field() {
    let redacted = secret_bearing_request().redact_for_viewer(ViewerClass::Operator);
    let blob = serde_json::to_string(&redacted).expect("serializes");
    assert_no_secrets("operator view", &blob);
}

#[test]
fn the_agent_and_decider_views_carry_no_credential_either() {
    for class in [ViewerClass::Agent, ViewerClass::Decider] {
        let blob = serde_json::to_string(&secret_bearing_request().redact_for_viewer(class))
            .expect("serializes");
        assert_no_secrets(&format!("{class:?} view"), &blob);
    }
}

#[test]
fn redaction_preserves_what_the_operator_triages_on() {
    // Over-masking is the other failure. The operator must still be able to
    // tell what the gate is about.
    let redacted = secret_bearing_request().redact_for_viewer(ViewerClass::Operator);
    let ScheduledAction::SandboxExec { command, detected_hosts, .. } = &redacted.action else {
        panic!("expected SandboxExec");
    };
    assert!(command.contains("curl"), "command shape lost: {command}");
    assert!(command.contains("https://x"), "destination lost: {command}");
    assert_eq!(detected_hosts.as_deref(), Some(&["x".to_string()][..]));

    let excerpt = &redacted.code_excerpts.as_ref().unwrap()[0];
    assert_eq!(excerpt.file_name, "deploy.sh", "structural fields untouched");
    assert!(excerpt.content.contains("#!/bin/sh"), "script shape lost");

    let risk = redacted.risk_summary.as_ref().unwrap();
    assert_eq!(risk.host_count, 1);
    assert_eq!(risk.protocol_mix, vec!["https".to_string()]);
}

#[test]
fn admin_class_still_round_trips_unchanged() {
    // Admin is the one class defined as identity; the record-level pass must
    // not quietly narrow it.
    let original = secret_bearing_request();
    assert_eq!(original.redact_for_viewer(ViewerClass::Admin), original);
}
