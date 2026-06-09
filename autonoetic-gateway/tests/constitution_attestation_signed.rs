//! Constitution R++1 — Signed turn-boundary state attestation: signature
//! verification and tamper detection.
//!
//! Tests that:
//!   - a freshly composed attestation verifies against the gateway's public
//!     key persisted on disk
//!   - tampering any single payload field breaks verification
//!   - tampering the signature breaks verification
//!   - the public-key sidecar matches the in-memory key
//!   - a different gateway key rejects the attestation (cross-gateway
//!     isolation)

mod support;

use autonoetic_gateway::runtime::crypto::GatewayIdentityKey;
use autonoetic_gateway::runtime::state_attestation::{
    compose_and_sign, render_tail, verify, AttestationInputs, BudgetMeter, StateAttestation,
};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use tempfile::tempdir;

fn manifest_with_caps(caps: Vec<Capability>) -> AgentManifest {
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
            id: "signed-test-agent".to_string(),
            name: "signed-test-agent".to_string(),
            description: "test".to_string(),
        },
        capabilities: caps,
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
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn default_inputs<'a>(manifest: &'a AgentManifest) -> AttestationInputs<'a> {
    AttestationInputs {
        agent_id: &manifest.agent.id,
        session_id: Some("root/child-a"),
        root_session_id: Some("root"),
        turn_counter: 5,
        manifest,
        gateway_node_id: "node-xyz",
        pending_approval_ids: vec!["apr-001".to_string(), "apr-002".to_string()],
        pending_user_interaction_ids: vec![],
        pending_escalation_ids: vec![],
        budget_meters: vec![BudgetMeter {
            name: "llm_rounds".to_string(),
            used: 5.0,
            limit: Some(20.0),
        }],
    }
}

#[test]
fn attestation_verifies_against_persisted_public_key() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![Capability::NetworkAccess {
        hosts: vec!["api.example.com".to_string()],
    }]);
    let att = compose_and_sign(default_inputs(&manifest), &key).expect("compose");

    let pub_bytes = key.public_key_bytes();
    let payload = verify(&pub_bytes, &att).expect("verify must succeed");
    assert_eq!(payload.agent_id, "signed-test-agent");
    assert_eq!(payload.turn_counter, 5);
    assert_eq!(payload.spawn_depth, 1);
    assert_eq!(payload.active_capabilities, vec!["NetworkAccess"]);
    assert_eq!(payload.pending_approval_count, 2);
    assert_eq!(payload.budget.len(), 1);
    assert_eq!(payload.budget[0].remaining(), Some(15.0));

    let pub_path = dir.path().join(GatewayIdentityKey::PUBLIC_FILENAME);
    let pub_file_bytes = std::fs::read(&pub_path).expect("read pub");
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&pub_file_bytes);
    let payload2 = verify(&arr, &att).expect("verify from file");
    assert_eq!(payload2.agent_id, payload.agent_id);
}

#[test]
fn tampered_turn_counter_breaks_verification() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![]);
    let mut att = compose_and_sign(default_inputs(&manifest), &key).expect("compose");
    att.payload.turn_counter = 9999;
    let err = verify(&key.public_key_bytes(), &att).expect_err("tampered turn_counter");
    assert!(err.to_string().contains("did not verify"), "{}", err);
}

#[test]
fn tampered_agent_id_breaks_verification() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![]);
    let mut att = compose_and_sign(default_inputs(&manifest), &key).expect("compose");
    att.payload.agent_id = "impostor-agent".to_string();
    let err = verify(&key.public_key_bytes(), &att).expect_err("tampered agent_id");
    assert!(err.to_string().contains("did not verify"), "{}", err);
}

#[test]
fn tampered_budget_breaks_verification() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![]);
    let mut att = compose_and_sign(default_inputs(&manifest), &key).expect("compose");
    att.payload.budget[0].used = 0.0;
    let err = verify(&key.public_key_bytes(), &att).expect_err("tampered budget");
    assert!(err.to_string().contains("did not verify"), "{}", err);
}

#[test]
fn tampered_pending_count_breaks_verification() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![]);
    let mut att = compose_and_sign(default_inputs(&manifest), &key).expect("compose");
    att.payload.pending_approval_count = 0;
    let err = verify(&key.public_key_bytes(), &att).expect_err("tampered count");
    assert!(err.to_string().contains("did not verify"), "{}", err);
}

#[test]
fn tampered_signature_bytes_break_verification() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![]);
    let mut att = compose_and_sign(default_inputs(&manifest), &key).expect("compose");
    let mut chars: Vec<char> = att.signature.chars().collect();
    chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
    att.signature = chars.into_iter().collect();
    let err = verify(&key.public_key_bytes(), &att).expect_err("tampered sig");
    assert!(err.to_string().contains("did not verify"), "{}", err);
}

#[test]
fn different_gateway_key_rejects_attestation() {
    let dir_a = tempdir().expect("tempdir a");
    let dir_b = tempdir().expect("tempdir b");
    let key_a = GatewayIdentityKey::load_or_generate(dir_a.path()).expect("key_a");
    let key_b = GatewayIdentityKey::load_or_generate(dir_b.path()).expect("key_b");
    let manifest = manifest_with_caps(vec![]);
    let att = compose_and_sign(default_inputs(&manifest), &key_a).expect("compose");
    let err = verify(&key_b.public_key_bytes(), &att).expect_err("wrong key");
    assert!(
        err.to_string().contains("fingerprint mismatch")
            || err.to_string().contains("did not verify"),
        "{}",
        err
    );
}

#[test]
fn rendered_tail_contains_verifiable_block() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![Capability::ReadAccess {
        scopes: vec!["*".to_string()],
    }]);
    let att = compose_and_sign(default_inputs(&manifest), &key).expect("compose");
    let tail = render_tail(&att).unwrap();

    assert!(tail.contains("<gateway_state_attestation>"));
    assert!(tail.contains("</gateway_state_attestation>"));
    assert!(tail.contains("authoritative"));

    let start = tail.find("<gateway_state_attestation>").expect("open tag")
        + "<gateway_state_attestation>".len();
    let end = tail
        .find("</gateway_state_attestation>")
        .expect("close tag");
    let json_block = &tail[start..end];
    let parsed: StateAttestation = serde_json::from_str(json_block.trim()).expect("parse json");
    let payload = verify(&key.public_key_bytes(), &parsed).expect("verify from rendered");
    assert_eq!(payload.agent_id, "signed-test-agent");
    assert_eq!(payload.turn_counter, 5);
}
