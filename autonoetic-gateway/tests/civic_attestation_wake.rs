//! #772 A.2 — civic line in the P-6.23 turn attestation: an agent that filed
//! a constitutional proposal or an anomaly flag sees it in its own signed
//! state block every turn ("voice with amnesia is no voice").
//!
//! #771 D.2 — the same line also surfaces open amendment invitations
//! addressed to the agent, carried as one-line summaries (rule + denial
//! count) because the agent did not file them: the friction evidence IS the
//! message.
//!
//! Exercises the store + `compose_and_sign` path directly (the cheapest
//! reliable seam — mirrors `context.rs::build_state_attestation_tail`'s own
//! store queries) rather than standing up a full executor.

use autonoetic_gateway::runtime::crypto::GatewayIdentityKey;
use autonoetic_gateway::runtime::state_attestation::{
    compose_and_sign, render_tail, AttestationInputs, BudgetMeter, InvitationSummary,
};
use autonoetic_gateway::scheduler::gateway_store::amendment_invitations::AmendmentInvitation;
use autonoetic_gateway::scheduler::gateway_store::anomaly_flags::AnomalyFlag;
use autonoetic_gateway::scheduler::gateway_store::constitutional_proposals::ConstitutionalProposal;
use autonoetic_gateway::scheduler::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use tempfile::tempdir;

fn manifest_for(agent_id: &str) -> AgentManifest {
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
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![],
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
        excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

#[test]
fn attestation_tail_surfaces_own_pending_proposal_and_flag() {
    let store_dir = tempdir().expect("store dir");
    let store = GatewayStore::open(store_dir.path()).expect("open store");

    let agent_id = "citizen.agent";

    store
        .insert_constitutional_proposal(&ConstitutionalProposal {
            proposal_id: "prop-wake-1".to_string(),
            proposer_agent_id: agent_id.to_string(),
            proposer_session_id: Some("root".to_string()),
            kind: "add_right".to_string(),
            target_id: None,
            proposed_text: Some("agents may do X".to_string()),
            justification: "closes a gap".to_string(),
            evidence_json: serde_json::json!({}),
            status: "pending".to_string(),
            operator_decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            published_in_release: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            sla_breached_at: None,
        })
        .expect("insert proposal");

    store
        .insert_anomaly_flag(&AnomalyFlag {
            flag_id: "flag-wake-1".to_string(),
            reporter_agent_id: agent_id.to_string(),
            reporter_session_id: Some("root".to_string()),
            subject_ref: "sess-target".to_string(),
            observation: "tool call bypassed policy check".to_string(),
            evidence_json: serde_json::json!([]),
            severity: "high".to_string(),
            status: "pending".to_string(),
            decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            sla_breached_at: None,
        })
        .expect("insert flag");

    // Same queries as `context.rs::build_state_attestation_tail`: only
    // non-terminal items ride along in the signed block, status-filtered
    // in SQL so terminal decisions can't displace them from the window.
    let pending_proposal_ids: Vec<String> = store
        .list_pending_constitutional_proposals(Some(agent_id), 64)
        .expect("list proposals")
        .into_iter()
        .map(|p| p.proposal_id)
        .collect();
    let pending_flag_ids: Vec<String> = store
        .list_pending_anomaly_flags(Some(agent_id), 64)
        .expect("list flags")
        .into_iter()
        .map(|f| f.flag_id)
        .collect();

    assert_eq!(pending_proposal_ids, vec!["prop-wake-1".to_string()]);
    assert_eq!(pending_flag_ids, vec!["flag-wake-1".to_string()]);

    let key_dir = tempdir().expect("key dir");
    let key = GatewayIdentityKey::load_or_generate(key_dir.path()).expect("key");
    let manifest = manifest_for(agent_id);

    let att = compose_and_sign(
        AttestationInputs {
            agent_id,
            session_id: Some("root"),
            root_session_id: Some("root"),
            turn_counter: 1,
            manifest: &manifest,
            gateway_node_id: "node-a",
            pending_approval_ids: vec![],
            pending_user_interaction_ids: vec![],
            pending_escalation_ids: vec![],
            pending_proposal_ids,
            pending_flag_ids,
            pending_invitations: vec![],
            budget_meters: vec![BudgetMeter {
                name: "llm_rounds".to_string(),
                used: 1.0,
                limit: Some(20.0),
            }],
            burn_rate: None,
            constitution_version: "2026.07.02",
            constitution_digest: "deadbeef",
        },
        &key,
    )
    .expect("compose");

    assert_eq!(att.payload.pending_proposal_ids, vec!["prop-wake-1"]);
    assert_eq!(att.payload.pending_proposal_count, 1);
    assert_eq!(att.payload.pending_flag_ids, vec!["flag-wake-1"]);
    assert_eq!(att.payload.pending_flag_count, 1);

    let tail = render_tail(&att).expect("render");
    assert!(
        tail.contains("prop-wake-1"),
        "rendered attestation tail must surface the agent's own pending proposal id: {tail}"
    );
    assert!(
        tail.contains("flag-wake-1"),
        "rendered attestation tail must surface the agent's own pending flag id: {tail}"
    );
}

/// #771 D.2 — an open amendment invitation rides the same signed civic
/// line as the agent's own filings, but as a one-line summary (rule +
/// denial count): the agent did not file the invitation, so the friction
/// evidence must travel with it. Answered/expired invitations drop off the
/// line (SQL-level status filter, same displacement contract as flags).
#[test]
fn attestation_tail_surfaces_open_amendment_invitations() {
    let store_dir = tempdir().expect("store dir");
    let store = GatewayStore::open(store_dir.path()).expect("open store");

    let agent_id = "citizen.agent";

    store
        .insert_amendment_invitation(&AmendmentInvitation {
            invitation_id: "ainv-wake-1".to_string(),
            agent_id: agent_id.to_string(),
            rule_id: "P-1.5".to_string(),
            denial_count: 4,
            threshold: 3,
            window_secs: 604800,
            status: "open".to_string(),
            answered_proposal_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            resolved_at: None,
        })
        .expect("insert invitation");

    // An answered invitation for the same agent must NOT surface.
    store
        .insert_amendment_invitation(&AmendmentInvitation {
            invitation_id: "ainv-wake-2".to_string(),
            agent_id: agent_id.to_string(),
            rule_id: "P-7.5".to_string(),
            denial_count: 3,
            threshold: 3,
            window_secs: 604800,
            status: "answered".to_string(),
            answered_proposal_id: Some("cprop-x".to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            resolved_at: Some(chrono::Utc::now().to_rfc3339()),
        })
        .expect("insert answered invitation");

    // An open invitation for a DIFFERENT agent must NOT surface.
    store
        .insert_amendment_invitation(&AmendmentInvitation {
            invitation_id: "ainv-wake-3".to_string(),
            agent_id: "other.agent".to_string(),
            rule_id: "P-1.9".to_string(),
            denial_count: 7,
            threshold: 3,
            window_secs: 604800,
            status: "open".to_string(),
            answered_proposal_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            resolved_at: None,
        })
        .expect("insert other-agent invitation");

    // Same query as `context.rs::build_state_attestation_tail`.
    let pending_invitations: Vec<InvitationSummary> = store
        .list_amendment_invitations(Some("open"), Some(agent_id), 64)
        .expect("list invitations")
        .into_iter()
        .map(|inv| InvitationSummary {
            invitation_id: inv.invitation_id,
            rule_id: inv.rule_id,
            denial_count: inv.denial_count,
        })
        .collect();

    assert_eq!(pending_invitations.len(), 1);
    assert_eq!(pending_invitations[0].invitation_id, "ainv-wake-1");
    assert_eq!(pending_invitations[0].rule_id, "P-1.5");
    assert_eq!(pending_invitations[0].denial_count, 4);

    let key_dir = tempdir().expect("key dir");
    let key = GatewayIdentityKey::load_or_generate(key_dir.path()).expect("key");
    let manifest = manifest_for(agent_id);

    let att = compose_and_sign(
        AttestationInputs {
            agent_id,
            session_id: Some("root"),
            root_session_id: Some("root"),
            turn_counter: 1,
            manifest: &manifest,
            gateway_node_id: "node-a",
            pending_approval_ids: vec![],
            pending_user_interaction_ids: vec![],
            pending_escalation_ids: vec![],
            pending_proposal_ids: vec![],
            pending_flag_ids: vec![],
            pending_invitations,
            budget_meters: vec![],
            burn_rate: None,
            constitution_version: "2026.07.02",
            constitution_digest: "deadbeef",
        },
        &key,
    )
    .expect("compose");

    assert_eq!(att.payload.pending_invitation_count, 1);
    assert_eq!(att.payload.pending_invitations[0].rule_id, "P-1.5");

    let tail = render_tail(&att).expect("render");
    assert!(
        tail.contains("ainv-wake-1") && tail.contains("P-1.5"),
        "rendered attestation tail must surface the open invitation id and its rule: {tail}"
    );
    assert!(
        !tail.contains("ainv-wake-2") && !tail.contains("ainv-wake-3"),
        "answered and other-agent invitations must not surface: {tail}"
    );
}
