//! Data-owner compartment acceptance (RFC §5.5 / #909).
//!
//! The compartment pattern, end to end at the label plane: a resident owner
//! session pinned local by session policy (source rules + `provider_constraint`)
//! owns a sensitive source; a sibling queries it via `agent_message` and the
//! reply's label crosses into the sibling's taint; network boundaries stay
//! closed on both sides until a host-scoped declassification opens exactly one
//! host for exactly one session; the causal chain explains every step.
//!
//! Drives the label plane against a real [`GatewayStore`] (tempfile-isolated) —
//! the same integration boundary as the other egress suites. Live-session
//! mechanics (residency park/resume, notification pump) are covered by
//! `tests/session/residency.rs`; this test proves the *egress semantics* of
//! the pattern.

use std::sync::Arc;

use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::egress_labeler::{
    network_egress_boundary_refusal_json, plan_taint_following_route,
    session_host_network_declass_target, EgressLabeler, PresetCandidate,
};
use autonoetic_gateway::scheduler::approval::{apply_decision, ApproveOptions, DecisionContext};
use autonoetic_gateway::scheduler::gateway_store::{AgentMessageRecord, GatewayStore};
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalStatus, GrantScope, ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::egress::{
    EgressClass, EgressConfig, EgressLabel, EgressRule, EgressSessionPolicy, ProviderConstraint,
    Sink,
};

fn run_ctx(session_id: &str, taint: EgressLabel) -> NativeToolRunContext {
    NativeToolRunContext {
        registry: ActiveExecutionRegistry::new(),
        root_session_id: session_id
            .split('/')
            .next()
            .unwrap_or(session_id)
            .to_string(),
        workflow_id: None,
        task_id: None,
        session_id: session_id.into(),
        agent_id: "acceptance.agent".into(),
        live_digest: None,
        live_report: None,
        user_id: None,
        artifact_id: None,
        sentinel_suppress_target: None,
        discovered_tools: None,
            annotation_counter: None,
        tool_discovery_catalog: None,
        wake_hint: None,
        wake_hints_map: None,
        egress_taint: Some(taint),
        egress_query_sink: None,
    }
}

#[test]
fn session_policy_without_provider_constraint_deserializes_unconstrained() -> anyhow::Result<()> {
    // Backward compat: policies persisted before the field existed load as
    // unconstrained (serde default), and `is_empty` ignores the absent field.
    let legacy = r#"{"rules": [], "default_label": "local_only"}"#;
    let policy: EgressSessionPolicy = serde_json::from_str(legacy)?;
    assert_eq!(policy.provider_constraint, None);
    assert!(!policy.is_empty());
    Ok(())
}

#[test]
fn data_owner_compartment_end_to_end() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let root = "root-mail";
    let owner = "root-mail/mail";
    let sibling = "root-mail/planner";

    // 1. Operator declares the compartment (RFC §5.4): mail reads are
    //    `local_only`, and the whole session tree is pinned to local presets.
    let policy = EgressSessionPolicy {
        rules: vec![EgressRule {
            source: "fs.read".into(),
            path: Some("~/mail/**".into()),
            label: EgressLabel::local_only(),
        }],
        default_label: None,
        provider_constraint: Some(ProviderConstraint::LocalOnly),
    };
    store.set_egress_session_policy(root, &policy, "operator:cli")?;
    let stored = store
        .get_egress_session_policy(root)?
        .expect("policy stored");
    assert_eq!(
        stored.policy, policy,
        "policy roundtrips through the store incl. provider_constraint"
    );

    // 2. The owner reads its source: the session rule labels the result
    //    `local_only`; the owner's session accumulates the taint.
    let labeler =
        EgressLabeler::from_config(&EgressConfig::default()).with_session_policy(&stored.policy);
    let resolution = labeler.resolve_label("fs.read", Some("~/mail/inbox.eml"));
    assert_eq!(resolution.label, EgressLabel::local_only());
    store.set_session_egress_taint(owner, &resolution.label)?;

    // 3. Provider constraint (rung 1): even a *clean* batch on a remote
    //    primary reroutes to a local preset — the owner stays both functional
    //    and local, instead of routing remote into an all-indications context.
    let presets = vec![
        PresetCandidate {
            name: "sonnet".into(),
            egress_class: Some(EgressClass::Remote),
        },
        PresetCandidate {
            name: "ollama".into(),
            egress_class: Some(EgressClass::Local),
        },
    ];
    let plan = plan_taint_following_route(
        &EgressLabel::unrestricted(),
        Some(EgressClass::Remote),
        &presets,
        stored.policy.provider_constraint,
    );
    assert_eq!(
        plan.reroute_to.as_ref().map(|c| c.name.as_str()),
        Some("ollama"),
        "constrained session reroutes even a clean batch to local"
    );

    // 4. The owner answers a sibling query: the reply crosses back as ONE
    //    labeled envelope — the sibling never holds the raw mail, and the
    //    label travels with the answer across the session boundary.
    let reply = AgentMessageRecord {
        message_id: "m-mail-answer".into(),
        sender_session_id: owner.into(),
        sender_agent_id: "mail.default".into(),
        target_pattern: format!("session:{sibling}"),
        message: "2 unread from the bank; 1 newsletter".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        egress_label: Some(EgressLabel::local_only()),
    };
    store.save_agent_message(&reply)?;
    store.insert_message_delivery("m-mail-answer", sibling)?;
    let fetched = store.fetch_undelivered_messages(sibling)?;
    let delivered = fetched
        .iter()
        .find(|m| m.message_id == "m-mail-answer")
        .expect("reply delivered to sibling");
    assert_eq!(delivered.egress_label, Some(EgressLabel::local_only()));
    // Ingest: the label intersects into the sibling's accumulated taint.
    store.set_session_egress_taint(sibling, &EgressLabel::local_only())?;

    // 5. Boundary gates hold on BOTH sides of the compartment: neither the
    //    owner nor the newly-tainted sibling can reach the network.
    for (sid, agent) in [(owner, "mail.default"), (sibling, "planner.default")] {
        let ctx = run_ctx(sid, EgressLabel::local_only());
        let refusal = network_egress_boundary_refusal_json(
            "web",
            "web_fetch",
            Some(&ctx),
            Some(&store),
            Some(sid),
            agent,
            None,
            Some("example.com"),
        );
        assert!(refusal.is_some(), "{sid} must be refused network egress");
    }

    // 6. Operator declassifies ONE host for the sibling's fetch, scoped to the
    //    sibling session only (RFC §8): example.com opens for the sibling;
    //    other hosts stay closed; the owner stays fully closed.
    let decision = ApprovalDecision {
        request_id: "apr-compartment".into(),
        session_id: sibling.into(),
        root_session_id: Some(root.into()),
        agent_id: "planner.default".into(),
        action: ScheduledAction::EgressDeclassify {
            target: session_host_network_declass_target(root, "example.com"),
            allowed_sink: Sink::Network,
            reason: "allow the bank export fetch".into(),
            payload: None,
        },
        status: ApprovalStatus::Approved,
        decided_by: "operator".into(),
        decided_at: chrono::Utc::now().to_rfc3339(),
        reason: Some("operator widen".into()),
        workflow_id: None,
        task_id: None,
        approval_level: ApprovalLevel::Operator,
    };
    apply_decision(
        &GatewayConfig::default(),
        Some(store.as_ref()),
        &decision,
        &ApproveOptions {
            grant_scope: Some(GrantScope::Session),
            ..Default::default()
        },
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;

    let ctx = run_ctx(sibling, EgressLabel::local_only());
    assert!(
        network_egress_boundary_refusal_json(
            "web",
            "web_fetch",
            Some(&ctx),
            Some(&store),
            Some(sibling),
            "planner.default",
            None,
            Some("example.com"),
        )
        .is_none(),
        "the declassified host opens for the sibling"
    );
    assert!(
        network_egress_boundary_refusal_json(
            "web",
            "web_fetch",
            Some(&ctx),
            Some(&store),
            Some(sibling),
            "planner.default",
            None,
            Some("other.com"),
        )
        .is_some(),
        "other hosts stay closed for the sibling"
    );
    let owner_ctx = run_ctx(owner, EgressLabel::local_only());
    assert!(
        network_egress_boundary_refusal_json(
            "web",
            "web_fetch",
            Some(&owner_ctx),
            Some(&store),
            Some(owner),
            "mail.default",
            None,
            Some("example.com"),
        )
        .is_some(),
        "session-scoped declassification does not leak to the owner"
    );

    // 7. Audit (RFC §9): the causal chain explains every step — refusals on
    //    both sessions and the sibling's declassification.
    let owner_events = store.search_causal_events(Some(owner), None, 50)?;
    assert!(
        owner_events
            .iter()
            .any(|e| e.action == "egress.boundary_refused"),
        "owner refusals are on the chain"
    );
    let sibling_events = store.search_causal_events(Some(sibling), None, 50)?;
    assert!(
        sibling_events
            .iter()
            .any(|e| e.action == "egress.boundary_refused"),
        "sibling refusals are on the chain"
    );
    assert!(
        sibling_events
            .iter()
            .any(|e| e.action == "egress.declassified"),
        "the host-scoped declassification is on the chain"
    );
    Ok(())
}
