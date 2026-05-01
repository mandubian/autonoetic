//! Constitution R++1 — Attestation freshness: block reflects current turn
//! state.
//!
//! Tests that:
//!   - turn counter is monotonic across successive attestations
//!   - budget meters reflect consumption after simulated rounds
//!   - capability changes appear in the attestation immediately
//!   - pending approval IDs are current (not stale from a previous turn)
//!   - attested_at timestamps advance across turns
//!   - spawn depth tracks session path depth

mod support;

use autonoetic_gateway::runtime::crypto::GatewayIdentityKey;
use autonoetic_gateway::runtime::state_attestation::{
    compose_and_sign, verify, AttestationInputs, BudgetMeter,
};
use autonoetic_gateway::runtime::session_budget::SessionBudgetRegistry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::SessionBudgetConfig;
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
            id: "freshness-agent".to_string(),
            name: "freshness-agent".to_string(),
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
        response_contract: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
    }
}

#[test]
fn turn_counter_monotonic_across_attestations() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![]);

    let mut prev_turn = 0u64;
    for turn in 1..=5 {
        let att = compose_and_sign(
            AttestationInputs {
                agent_id: &manifest.agent.id,
                session_id: Some("root"),
                root_session_id: Some("root"),
                turn_counter: turn,
                manifest: &manifest,
                gateway_node_id: "node",
                pending_approval_ids: vec![],
                budget_meters: vec![],
            },
            &key,
        )
        .expect("compose");
        let payload = verify(&key.public_key_bytes(), &att).expect("verify");
        assert!(
            payload.turn_counter > prev_turn,
            "turn_counter must be monotonic: {} <= {}",
            payload.turn_counter,
            prev_turn
        );
        prev_turn = payload.turn_counter;
    }
}

#[test]
fn budget_meters_reflect_consumption() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![]);

    let budget_config = SessionBudgetConfig {
        max_llm_rounds: Some(10),
        max_llm_tokens: Some(1000),
        ..Default::default()
    };
    let registry = SessionBudgetRegistry::new(budget_config);
    let scope = "root";

    let att_before = compose_and_sign(
        AttestationInputs {
            agent_id: &manifest.agent.id,
            session_id: Some(scope),
            root_session_id: Some(scope),
            turn_counter: 0,
            manifest: &manifest,
            gateway_node_id: "node",
            pending_approval_ids: vec![],
            budget_meters: vec![BudgetMeter {
                name: "llm_rounds".to_string(),
                used: 0.0,
                limit: Some(10.0),
            }],
        },
        &key,
    )
    .expect("compose before");
    let payload_before = verify(&key.public_key_bytes(), &att_before).expect("verify before");
    let rounds_before = &payload_before
        .budget
        .iter()
        .find(|m| m.name == "llm_rounds")
        .expect("llm_rounds meter");
    assert_eq!(rounds_before.used, 0.0);
    assert_eq!(rounds_before.remaining(), Some(10.0));

    for _ in 0..3 {
        registry.check_pre_llm(scope).unwrap();
        registry
            .record_llm_completion(scope, 50, 40, None)
            .unwrap();
    }
    let (rounds, tokens, _cost) = registry.snapshot_counters(scope).expect("snapshot");

    let att_after = compose_and_sign(
        AttestationInputs {
            agent_id: &manifest.agent.id,
            session_id: Some(scope),
            root_session_id: Some(scope),
            turn_counter: 3,
            manifest: &manifest,
            gateway_node_id: "node",
            pending_approval_ids: vec![],
            budget_meters: vec![
                BudgetMeter {
                    name: "llm_rounds".to_string(),
                    used: rounds as f64,
                    limit: Some(10.0),
                },
                BudgetMeter {
                    name: "llm_tokens".to_string(),
                    used: tokens as f64,
                    limit: Some(1000.0),
                },
            ],
        },
        &key,
    )
    .expect("compose after");
    let payload_after = verify(&key.public_key_bytes(), &att_after).expect("verify after");
    let rounds_after = &payload_after
        .budget
        .iter()
        .find(|m| m.name == "llm_rounds")
        .expect("llm_rounds meter");
    let tokens_after = &payload_after
        .budget
        .iter()
        .find(|m| m.name == "llm_tokens")
        .expect("llm_tokens meter");
    assert_eq!(rounds_after.used, 3.0);
    assert_eq!(rounds_after.remaining(), Some(7.0));
    assert_eq!(tokens_after.used, 270.0);
    assert_eq!(tokens_after.remaining(), Some(730.0));
}

#[test]
fn capability_changes_appear_immediately() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");

    let manifest_r = manifest_with_caps(vec![Capability::ReadAccess {
        scopes: vec!["*".to_string()],
    }]);
    let att_r = compose_and_sign(
        AttestationInputs {
            agent_id: &manifest_r.agent.id,
            session_id: Some("root"),
            root_session_id: Some("root"),
            turn_counter: 0,
            manifest: &manifest_r,
            gateway_node_id: "node",
            pending_approval_ids: vec![],
            budget_meters: vec![],
        },
        &key,
    )
    .expect("compose read-only");
    let payload_r = verify(&key.public_key_bytes(), &att_r).expect("verify");
    assert_eq!(payload_r.active_capabilities, vec!["ReadAccess"]);

    let manifest_rw = manifest_with_caps(vec![
        Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        },
        Capability::WriteAccess {
            scopes: vec!["fs/tmp".to_string()],
        },
    ]);
    let att_rw = compose_and_sign(
        AttestationInputs {
            agent_id: &manifest_rw.agent.id,
            session_id: Some("root"),
            root_session_id: Some("root"),
            turn_counter: 1,
            manifest: &manifest_rw,
            gateway_node_id: "node",
            pending_approval_ids: vec![],
            budget_meters: vec![],
        },
        &key,
    )
    .expect("compose read-write");
    let payload_rw = verify(&key.public_key_bytes(), &att_rw).expect("verify");
    assert!(payload_rw.active_capabilities.contains(&"ReadAccess".to_string()));
    assert!(payload_rw.active_capabilities.contains(&"WriteAccess".to_string()));
}

#[test]
fn pending_approval_ids_are_current() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![]);

    let att_turn1 = compose_and_sign(
        AttestationInputs {
            agent_id: &manifest.agent.id,
            session_id: Some("root"),
            root_session_id: Some("root"),
            turn_counter: 1,
            manifest: &manifest,
            gateway_node_id: "node",
            pending_approval_ids: vec!["apr-aaa".to_string()],
            budget_meters: vec![],
        },
        &key,
    )
    .expect("compose turn 1");
    let p1 = verify(&key.public_key_bytes(), &att_turn1).expect("verify");
    assert_eq!(p1.pending_approval_ids, vec!["apr-aaa"]);
    assert_eq!(p1.pending_approval_count, 1);

    let att_turn2 = compose_and_sign(
        AttestationInputs {
            agent_id: &manifest.agent.id,
            session_id: Some("root"),
            root_session_id: Some("root"),
            turn_counter: 2,
            manifest: &manifest,
            gateway_node_id: "node",
            pending_approval_ids: vec!["apr-aaa".to_string(), "apr-bbb".to_string()],
            budget_meters: vec![],
        },
        &key,
    )
    .expect("compose turn 2");
    let p2 = verify(&key.public_key_bytes(), &att_turn2).expect("verify");
    assert_eq!(p2.pending_approval_ids.len(), 2);
    assert_eq!(p2.pending_approval_count, 2);

    let att_turn3 = compose_and_sign(
        AttestationInputs {
            agent_id: &manifest.agent.id,
            session_id: Some("root"),
            root_session_id: Some("root"),
            turn_counter: 3,
            manifest: &manifest,
            gateway_node_id: "node",
            pending_approval_ids: vec![],
            budget_meters: vec![],
        },
        &key,
    )
    .expect("compose turn 3");
    let p3 = verify(&key.public_key_bytes(), &att_turn3).expect("verify");
    assert!(p3.pending_approval_ids.is_empty());
    assert_eq!(p3.pending_approval_count, 0);
}

#[test]
fn attested_at_advances_across_turns() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![]);

    let att1 = compose_and_sign(
        AttestationInputs {
            agent_id: &manifest.agent.id,
            session_id: Some("root"),
            root_session_id: Some("root"),
            turn_counter: 1,
            manifest: &manifest,
            gateway_node_id: "node",
            pending_approval_ids: vec![],
            budget_meters: vec![],
        },
        &key,
    )
    .expect("compose turn 1");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let att2 = compose_and_sign(
        AttestationInputs {
            agent_id: &manifest.agent.id,
            session_id: Some("root"),
            root_session_id: Some("root"),
            turn_counter: 2,
            manifest: &manifest,
            gateway_node_id: "node",
            pending_approval_ids: vec![],
            budget_meters: vec![],
        },
        &key,
    )
    .expect("compose turn 2");

    assert_ne!(
        att1.payload.attested_at, att2.payload.attested_at,
        "attested_at must differ across turns"
    );
}

#[test]
fn spawn_depth_tracks_session_path() {
    let dir = tempdir().expect("tempdir");
    let key = GatewayIdentityKey::load_or_generate(dir.path()).expect("key");
    let manifest = manifest_with_caps(vec![]);

    let cases = vec![
        ("root", 0u32),
        ("root/child", 1),
        ("root/child/grandchild", 2),
        ("root/a/b/c", 3),
    ];

    for (session_id, expected_depth) in cases {
        let att = compose_and_sign(
            AttestationInputs {
                agent_id: &manifest.agent.id,
                session_id: Some(session_id),
                root_session_id: Some("root"),
                turn_counter: 0,
                manifest: &manifest,
                gateway_node_id: "node",
                pending_approval_ids: vec![],
                budget_meters: vec![],
            },
            &key,
        )
        .expect("compose");
        let payload = verify(&key.public_key_bytes(), &att).expect("verify");
        assert_eq!(
            payload.spawn_depth, expected_depth,
            "session_id={}",
            session_id
        );
    }
}
