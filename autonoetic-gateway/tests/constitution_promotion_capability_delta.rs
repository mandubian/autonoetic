//! Constitution R++2 — Capability-delta gating at promotion (issue #49).
//!
//! `agent_revision_promote` must not silently flip the alias when the new
//! revision's capability set broadens relative to the outgoing one. Detection
//! ≠ prevention: the gate creates a `RevisionPromote` approval whose payload
//! names every added or broadened capability, and the operator must
//! acknowledge each one explicitly to approve. An approval that does not
//! name the full delta is rejected.
//!
//! Tests (per acceptance #49):
//!   - identical caps → promote proceeds, no approval row created
//!   - adds NetworkAccess → approval created with named delta in payload,
//!     promote returns `pending_approval` with `approval_ref`
//!   - approve without confirming delta → rejected
//!   - approve with the matching acknowledgement → succeeds
//!   - retry promote with `approval_ref` after approval → gate bypassed.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::{approve_request_with_options, ApproveOptions};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;

const AGENT_ID: &str = "test-agent";
const OUTGOING_REVISION: &str = "rev_outgoing";
const INCOMING_REVISION: &str = "rev_incoming";

fn manifest_with_capabilities(caps: Vec<Capability>) -> AgentManifest {
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
            id: AGENT_ID.to_string(),
            name: AGENT_ID.to_string(),
            description: "test".to_string(),
        },
        capabilities: caps,
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

struct PromoteHarness {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    agent_dir: std::path::PathBuf,
    gateway_dir: std::path::PathBuf,
}

/// Build a SKILL.md file containing only the YAML frontmatter we need
/// (capabilities + minimal agent metadata). `parse_frontmatter_capabilities`
/// reads `capabilities` directly off the top-level mapping.
fn skill_md(capabilities_yaml: &str) -> String {
    format!(
        "---\nversion: \"1.0\"\nruntime:\n  engine: autonoetic\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: stateful\n  sandbox: bubblewrap\n  runtime_lock: runtime.lock\nagent:\n  id: {}\n  name: {}\n  description: test\n{}\n---\n# Test\n",
        AGENT_ID, AGENT_ID, capabilities_yaml,
    )
}

fn write_revision_skill(gateway_dir: &std::path::Path, revision_id: &str, capabilities_yaml: &str) {
    let dir = gateway_dir
        .join("revisions/agents")
        .join(AGENT_ID)
        .join(revision_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), skill_md(capabilities_yaml)).unwrap();
}

fn make_revision_record(revision_id: &str) -> AgentRevisionRecord {
    AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: AGENT_ID.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{}", revision_id),
        runtime_lock_hash: "sha256:lock".to_string(),
        manifest_hash: "sha256:manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: "user".to_string(),
        created_by_id: "test".to_string(),
        source_kind: "artifact".to_string(),
        source_ref: None,
        origin_node_id: "local".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::Value::Null,
        short_id: revision_id.chars().take(8).collect(),
        signature: None,
        signer_id: None,
    }
}

fn setup_promote_harness(outgoing_caps_yaml: &str, incoming_caps_yaml: &str) -> PromoteHarness {
    let temp = tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join(AGENT_ID);
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = Arc::new(GatewayStore::open(&gateway_dir).expect("store opens"));

    write_revision_skill(&gateway_dir, OUTGOING_REVISION, outgoing_caps_yaml);
    write_revision_skill(&gateway_dir, INCOMING_REVISION, incoming_caps_yaml);

    store
        .insert_agent_revision(&make_revision_record(OUTGOING_REVISION))
        .unwrap();
    store
        .insert_agent_revision(&make_revision_record(INCOMING_REVISION))
        .unwrap();

    let alias = AgentAliasRecord {
        alias_id: AGENT_ID.to_string(),
        agent_id: AGENT_ID.to_string(),
        revision_id: OUTGOING_REVISION.to_string(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        updated_by_type: "user".to_string(),
        updated_by_id: "test".to_string(),
        reason: None,
    };
    store.upsert_agent_alias(&alias).unwrap();

    PromoteHarness {
        _temp: temp,
        store,
        agent_dir,
        gateway_dir,
    }
}

fn invoke_promote(h: &PromoteHarness, args_json: &str) -> serde_json::Value {
    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let raw = registry
        .execute(
            "agent_revision_promote",
            &manifest,
            &policy,
            &h.agent_dir,
            Some(&h.gateway_dir),
            args_json,
            Some("test-session"),
            Some("turn-000001"),
            None,
            Some(h.store.clone()),
            None,
        )
        .expect("execute should not error for normal cases");
    serde_json::from_str(&raw).expect("response is JSON")
}

// ---------------------------------------------------------------------------
// Direct approval-gate tests (no SKILL.md scaffolding)
// ---------------------------------------------------------------------------

fn store_revision_promote_approval(
    store: &GatewayStore,
    request_id: &str,
    added: Vec<&str>,
    broadened: Vec<&str>,
) -> ApprovalRequest {
    let mut req = ApprovalRequest {
        request_id: request_id.to_string(),
        agent_id: AGENT_ID.to_string(),
        session_id: "sess".to_string(),
        root_session_id: None,
        workflow_id: None,
        task_id: None,
        action: ScheduledAction::RevisionPromote {
            agent_id: AGENT_ID.to_string(),
            revision_id: INCOMING_REVISION.to_string(),
            outgoing_revision_id: OUTGOING_REVISION.to_string(),
            added_capabilities: added.into_iter().map(String::from).collect(),
            broadened_capabilities: broadened.into_iter().map(String::from).collect(),
            payload: None,
        },
        created_at: (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339(),
        status: None,
        decided_at: None,
        decided_by: None,
        reason: None,
        evidence_ref: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        similar_to_request_id: None,
        similarity_score: None,
        min_dwell_ms: None,
        confirm_phrase: None,
    };
    store.create_approval(&mut req).unwrap();
    req
}

#[test]
fn approve_without_acknowledgement_is_rejected() {
    let temp = tempdir().expect("tempdir");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = GatewayStore::open(&gateway_dir).expect("store");
    let stored =
        store_revision_promote_approval(&store, "ar-no-ack", vec!["NetworkAccess"], vec![]);

    let mut cfg = GatewayConfig::default();
    cfg.approval_dwell_multiplier = 0.0;
    let result = approve_request_with_options(
        &cfg,
        Some(&store),
        "ar-no-ack",
        "operator",
        Some("sgtm".to_string()),
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            confirm_phrase: stored.confirm_phrase.clone(),
            ..Default::default()
        },
    );
    let err = result.expect_err("approval without acknowledgement must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("Capability-accretion approval (R++2)"),
        "{}",
        msg
    );
    assert!(msg.contains("NetworkAccess"), "{}", msg);
    assert!(msg.contains("Missing: [NetworkAccess]"), "{}", msg);
}

#[test]
fn approve_with_partial_acknowledgement_is_rejected() {
    let temp = tempdir().expect("tempdir");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = GatewayStore::open(&gateway_dir).expect("store");
    let stored = store_revision_promote_approval(
        &store,
        "ar-partial",
        vec!["NetworkAccess"],
        vec!["SandboxFunctions"],
    );

    let mut cfg = GatewayConfig::default();
    cfg.approval_dwell_multiplier = 0.0;
    let result = approve_request_with_options(
        &cfg,
        Some(&store),
        "ar-partial",
        "operator",
        None,
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            acknowledged_capabilities: vec!["NetworkAccess".to_string()],
            confirm_phrase: stored.confirm_phrase.clone(),
            ..Default::default()
        },
    );
    let msg = result.expect_err("partial ack must fail").to_string();
    assert!(msg.contains("Missing: [SandboxFunctions]"), "{}", msg);
}

#[test]
fn approve_with_extra_acknowledgement_is_rejected() {
    let temp = tempdir().expect("tempdir");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = GatewayStore::open(&gateway_dir).expect("store");
    let stored = store_revision_promote_approval(&store, "ar-extra", vec!["NetworkAccess"], vec![]);

    let mut cfg = GatewayConfig::default();
    cfg.approval_dwell_multiplier = 0.0;
    let result = approve_request_with_options(
        &cfg,
        Some(&store),
        "ar-extra",
        "operator",
        None,
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            acknowledged_capabilities: vec![
                "NetworkAccess".to_string(),
                "SandboxFunctions".to_string(),
            ],
            confirm_phrase: stored.confirm_phrase.clone(),
            ..Default::default()
        },
    );
    let msg = result.expect_err("extra ack must fail").to_string();
    assert!(msg.contains("Unexpected: [SandboxFunctions]"), "{}", msg);
}

#[test]
fn approve_with_exact_acknowledgement_succeeds() {
    let temp = tempdir().expect("tempdir");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = GatewayStore::open(&gateway_dir).expect("store");
    let stored = store_revision_promote_approval(
        &store,
        "ar-exact",
        vec!["NetworkAccess"],
        vec!["SandboxFunctions"],
    );

    let mut cfg = GatewayConfig::default();
    cfg.approval_dwell_multiplier = 0.0;
    let decision = approve_request_with_options(
        &cfg,
        Some(&store),
        "ar-exact",
        "operator",
        Some("acked all".to_string()),
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            acknowledged_capabilities: vec![
                "SandboxFunctions".to_string(),
                "NetworkAccess".to_string(),
            ],
            confirm_phrase: stored.confirm_phrase.clone(),
            ..Default::default()
        },
    )
    .expect("exact acknowledgement must succeed");
    assert_eq!(decision.request_id, "ar-exact");
    let row = store.get_approval("ar-exact").unwrap().unwrap();
    assert_eq!(row.status, Some(ApprovalStatus::Approved));
}

// ---------------------------------------------------------------------------
// End-to-end through the promote tool
// ---------------------------------------------------------------------------

#[test]
fn identical_caps_no_approval_required() {
    let h = setup_promote_harness("capabilities: []", "capabilities: []");
    let resp = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION
        ),
    );
    // promote may still fail downstream (e.g. promotion gate), but it must
    // NOT block on capability delta. Either ok=true or any error reason
    // other than capability_delta_requires_approval.
    if resp["ok"] == false {
        assert_ne!(
            resp["error"].as_str(),
            Some("capability_delta_requires_approval"),
            "identical capability sets must not trigger the R++2 gate: {:?}",
            resp
        );
    }
    // No RevisionPromote approval row should have been created.
    let approvals = h
        .store
        .get_pending_approvals()
        .expect("pending approvals query");
    assert!(
        !approvals
            .iter()
            .any(|a| matches!(a.action, ScheduledAction::RevisionPromote { .. })),
        "no RevisionPromote approval should be created for identical caps"
    );
}

#[test]
fn adds_network_access_creates_named_approval() {
    let outgoing_caps = "capabilities: []";
    let incoming_caps = "capabilities:\n  - type: NetworkAccess\n    hosts: [\"api.github.com\"]";
    let h = setup_promote_harness(outgoing_caps, incoming_caps);

    let resp = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION
        ),
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"], "capability_delta_requires_approval");
    assert_eq!(resp["approval_required"], true);
    let approval_ref = resp["approval_ref"].as_str().expect("approval_ref present");
    // Both `request_id` and `approval_ref` are exposed to keep parity with
    // the rest of the gateway's tool-response shape — `request_id` is what
    // SessionTracer indexes against; `approval_ref` is the retry handle.
    assert_eq!(
        resp["request_id"].as_str(),
        Some(approval_ref),
        "request_id must mirror approval_ref"
    );
    assert!(
        approval_ref.starts_with("apr-"),
        "approval IDs use the `apr-` prefix to match the rest of the gateway: {}",
        approval_ref
    );
    let added: Vec<&str> = resp["added_capabilities"]
        .as_array()
        .expect("added_capabilities array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(added, vec!["NetworkAccess"]);

    // The approval row in the store carries the named delta.
    let row = h
        .store
        .get_approval(approval_ref)
        .expect("query")
        .expect("approval row exists");
    if let ScheduledAction::RevisionPromote {
        agent_id,
        revision_id,
        outgoing_revision_id,
        added_capabilities,
        broadened_capabilities,
        ..
    } = &row.action
    {
        assert_eq!(agent_id, AGENT_ID);
        assert_eq!(revision_id, INCOMING_REVISION);
        assert_eq!(outgoing_revision_id, OUTGOING_REVISION);
        assert_eq!(added_capabilities, &vec!["NetworkAccess".to_string()]);
        assert!(broadened_capabilities.is_empty());
    } else {
        panic!("expected RevisionPromote action, got {:?}", row.action);
    }
}

#[test]
fn approval_ref_bypass_is_invalidated_when_alias_moves() {
    // R++2 hardening: an approval acknowledges a delta against a *specific*
    // outgoing baseline. If the alias has moved between approval-mint and
    // retry, the bypass must NOT engage — the operator never acknowledged
    // the (potentially different) delta against the new baseline.
    let outgoing_caps = "capabilities: []";
    let incoming_caps = "capabilities:\n  - type: NetworkAccess\n    hosts: [\"api.github.com\"]";
    let h = setup_promote_harness(outgoing_caps, incoming_caps);

    let first = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION
        ),
    );
    let approval_ref = first["approval_ref"].as_str().unwrap().to_string();

    let mut cfg = GatewayConfig::default();
    cfg.approval_dwell_multiplier = 0.0;
    let apr_row = h.store.get_approval(&approval_ref).unwrap().unwrap();
    approve_request_with_options(
        &cfg,
        Some(&h.store),
        &approval_ref,
        "operator",
        None,
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            acknowledged_capabilities: vec!["NetworkAccess".to_string()],
            confirm_phrase: apr_row.confirm_phrase.clone(),
            ..Default::default()
        },
    )
    .expect("operator approval should succeed");

    // Operator (or another flow) moves the alias to a *third* revision while
    // the agent's approval is still outstanding. The acknowledgement was
    // against `OUTGOING_REVISION`; the baseline is now different.
    let third_revision = "rev_third";
    write_revision_skill(&h.gateway_dir, third_revision, "capabilities: []");
    h.store
        .insert_agent_revision(&make_revision_record(third_revision))
        .unwrap();
    h.store
        .upsert_agent_alias(&AgentAliasRecord {
            alias_id: AGENT_ID.to_string(),
            agent_id: AGENT_ID.to_string(),
            revision_id: third_revision.to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            updated_by_type: "user".to_string(),
            updated_by_id: "test".to_string(),
            reason: None,
        })
        .unwrap();

    let retry = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}","approval_ref":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION, approval_ref
        ),
    );
    assert_eq!(
        retry["error"].as_str(),
        Some("capability_delta_requires_approval"),
        "stale approval_ref must not bypass the gate after the baseline moved: {:?}",
        retry
    );
    // A *fresh* approval row must be minted against the new outgoing.
    let new_ref = retry["approval_ref"].as_str().expect("new approval_ref");
    assert_ne!(new_ref, approval_ref, "fresh approval, not a reuse");
    let new_row = h.store.get_approval(new_ref).unwrap().unwrap();
    if let ScheduledAction::RevisionPromote {
        outgoing_revision_id,
        ..
    } = &new_row.action
    {
        assert_eq!(outgoing_revision_id, third_revision);
    } else {
        panic!("expected RevisionPromote on retry");
    }
}

#[test]
fn approval_ref_bypasses_gate_after_approval() {
    let outgoing_caps = "capabilities: []";
    let incoming_caps = "capabilities:\n  - type: NetworkAccess\n    hosts: [\"api.github.com\"]";
    let h = setup_promote_harness(outgoing_caps, incoming_caps);

    let first = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION
        ),
    );
    let approval_ref = first["approval_ref"].as_str().unwrap().to_string();

    // Operator approves with the matching acknowledgement.
    let mut cfg = GatewayConfig::default();
    cfg.approval_dwell_multiplier = 0.0;
    let apr_row = h.store.get_approval(&approval_ref).unwrap().unwrap();
    approve_request_with_options(
        &cfg,
        Some(&h.store),
        &approval_ref,
        "operator",
        None,
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions {
            acknowledged_capabilities: vec!["NetworkAccess".to_string()],
            confirm_phrase: apr_row.confirm_phrase.clone(),
            ..Default::default()
        },
    )
    .expect("approval should succeed with exact acknowledgement");

    // Retry with approval_ref. The R++2 gate must be bypassed. The promote
    // may still fail at downstream gates (artifact review, eval run) — what
    // we are asserting is that the response no longer carries the
    // `capability_delta_requires_approval` error.
    let retry = invoke_promote(
        &h,
        &format!(
            r#"{{"agent_id":"{}","revision_id":"{}","approval_ref":"{}"}}"#,
            AGENT_ID, INCOMING_REVISION, approval_ref
        ),
    );
    if retry["ok"] == false {
        assert_ne!(
            retry["error"].as_str(),
            Some("capability_delta_requires_approval"),
            "approved approval_ref must bypass the R++2 gate: {:?}",
            retry
        );
    }
}
