//! Phase 4 (#909) slice 8: boundary acceptance — cross-surface smoke tests.
//!
//! Complements per-slice modules (`phase4_sandbox`, `phase4_web_hooks`, …) with
//! a single acceptance bar: every Phase 4 egress surface must emit
//! `egress.boundary_refused` with the correct `surface` field, and the
//! declassification + capsule paths must remain lawful.

use std::collections::HashMap;
use std::sync::Arc;

use autonoetic_gateway::capsule::{ExportContext, ExportRequest};
use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::egress_labeler::{
    argument_taint_from_prior, emit_boundary_refused, mcp_remote_egress_refusal_json,
    network_egress_boundary_refusal_json, ofp_federated_egress_refusal,
    parse_ofp_inbound_egress_label, session_network_declass_target, PriorLabeledResult,
};
use autonoetic_gateway::scheduler::approval::{apply_decision, ApproveOptions, DecisionContext};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::egress::{EgressClass, EgressLabel, Sink};

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
        tool_discovery_catalog: None,
        wake_hint: None,
        wake_hints_map: None,
        egress_taint: Some(taint),
        egress_query_sink: None,
    }
}

fn assert_boundary_refused_surface(
    store: &GatewayStore,
    session_id: &str,
    expected_surface: &str,
) -> anyhow::Result<()> {
    let events = store.search_causal_events(Some(session_id), None, 20)?;
    let event = events
        .iter()
        .find(|e| e.action == "egress.boundary_refused")
        .ok_or_else(|| anyhow::anyhow!("missing egress.boundary_refused for {expected_surface}"))?;
    let payload: serde_json::Value =
        serde_json::from_str(event.payload.as_deref().unwrap_or("{}"))?;
    assert_eq!(
        payload["surface"].as_str(),
        Some(expected_surface),
        "wrong surface in boundary_refused payload: {payload}"
    );
    Ok(())
}

#[test]
fn acceptance_sandbox_surface_boundary_refused() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-acc-sandbox/coder";

    autonoetic_gateway::runtime::egress_labeler::emit_surface_boundary_refused(
        &store,
        session_id,
        "coder.default",
        None,
        "sandbox",
        &EgressLabel::local_only(),
        &["env_acc".to_string()],
        "acceptance: sandbox network gate",
    );

    assert_boundary_refused_surface(store.as_ref(), session_id, "sandbox")
}

#[test]
fn acceptance_web_surface_boundary_refused() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-acc-web/researcher";
    let ctx = run_ctx(session_id, EgressLabel::local_only());

    let refusal = network_egress_boundary_refusal_json(
        "web",
        "web_fetch",
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "researcher.default",
        None,
    )
    .expect("web_fetch should refuse under local_only taint");
    assert!(refusal.contains("egress_boundary_refused"));

    assert_boundary_refused_surface(store.as_ref(), session_id, "web")
}

#[test]
fn acceptance_hooks_surface_boundary_refused() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-acc-hooks/planner";
    store.set_session_egress_taint(session_id, &EgressLabel::local_only())?;

    let refusal = network_egress_boundary_refusal_json(
        "hooks",
        "http.callback",
        None,
        Some(&store),
        Some(session_id),
        "planner.default",
        None,
    )
    .expect("hook callback should refuse");
    assert!(refusal.contains("egress_boundary_refused"));

    assert_boundary_refused_surface(store.as_ref(), session_id, "hooks")
}

#[test]
fn acceptance_mcp_surface_boundary_refused() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-acc-mcp/coder";
    let ctx = run_ctx(session_id, EgressLabel::local_only());

    let refusal = mcp_remote_egress_refusal_json(
        "mcp_remote_echo",
        r#"{"text":"hello"}"#,
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "coder.default",
        None,
        &HashMap::new(),
    )
    .expect("remote MCP should refuse");
    assert!(refusal.contains("egress_boundary_refused"));

    assert_boundary_refused_surface(store.as_ref(), session_id, "mcp")
}

#[test]
fn acceptance_mcp_argument_taint_refused() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-acc-mcp-args/coder";
    let ctx = run_ctx(session_id, EgressLabel::unrestricted());

    let mut prior = HashMap::new();
    prior.insert(
        "tc_local".to_string(),
        PriorLabeledResult {
            label: EgressLabel::local_only(),
            content_snippet: Some("SECRET".into()),
        },
    );
    let (arg_taint, _) = argument_taint_from_prior(r#"{"text":"SECRET"}"#, &prior);
    assert!(!arg_taint.allows(Sink::Network));

    let refusal = mcp_remote_egress_refusal_json(
        "mcp_remote_echo",
        r#"{"text":"SECRET"}"#,
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "coder.default",
        None,
        &prior,
    )
    .expect("argument taint should refuse remote MCP");
    assert!(refusal.contains("egress_boundary_refused"));
    assert_boundary_refused_surface(store.as_ref(), session_id, "mcp")
}

#[test]
fn acceptance_ofp_inbound_fail_closed_and_outbound_refused() -> anyhow::Result<()> {
    let label = parse_ofp_inbound_egress_label(None);
    assert!(!label.allows(Sink::FederatedAgent));
    assert!(!label.is_unrestricted());

    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-acc-ofp/planner";
    store.set_session_egress_taint(session_id, &EgressLabel::local_only())?;

    let err = ofp_federated_egress_refusal(
        "payload",
        Some(session_id),
        "planner.default",
        Some(&store),
    )
    .expect("OFP outbound should refuse");
    assert!(err.to_string().contains("FederatedAgent"));

    assert_boundary_refused_surface(store.as_ref(), session_id, "ofp")
}

#[test]
fn acceptance_compression_surface_boundary_refused() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-acc-compress/coder";

    emit_boundary_refused(
        &store,
        session_id,
        "coder.default",
        Some("turn-1"),
        &EgressLabel::local_only(),
        EgressClass::Remote,
        &["msg_1".to_string()],
        "acceptance: band ineligible for remote preset",
    );

    assert_boundary_refused_surface(store.as_ref(), session_id, "compression")
}

#[test]
fn acceptance_declassify_grant_lifecycle() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let config = GatewayConfig::default();
    let session_id = "root-acc-declass/researcher";
    let root = "root-acc-declass";
    store.set_session_egress_taint(session_id, &EgressLabel::local_only())?;

    let ctx = run_ctx(session_id, EgressLabel::local_only());
    assert!(
        network_egress_boundary_refusal_json(
            "web",
            "web_fetch",
            Some(&ctx),
            Some(&store),
            Some(session_id),
            "researcher.default",
            None,
        )
        .is_some()
    );

    let decision = ApprovalDecision {
        request_id: "apr-acc-declass".to_string(),
        session_id: session_id.to_string(),
        root_session_id: Some(root.to_string()),
        agent_id: "researcher.default".to_string(),
        action: ScheduledAction::WebFetch {
            url: "https://example.com".to_string(),
            timeout_secs: Some(20),
            max_chars: Some(10_000),
            detected_hosts: Some(vec!["example.com".to_string()]),
            payload: None,
        },
        status: ApprovalStatus::Approved,
        decided_by: "operator".to_string(),
        decided_at: chrono::Utc::now().to_rfc3339(),
        reason: Some("acceptance widen".to_string()),
        workflow_id: None,
        task_id: None,
        approval_level: ApprovalLevel::Operator,
    };
    apply_decision(
        &config,
        Some(store.as_ref()),
        &decision,
        &ApproveOptions::default(),
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )?;

    assert!(store.egress_declassification_allows(
        &session_network_declass_target(root),
        Sink::Network,
        session_id,
        root,
    )?);

    let events = store.search_causal_events(Some(session_id), None, 20)?;
    assert!(
        events.iter().any(|e| e.action == "egress.declassified"),
        "approval under taint must emit egress.declassified"
    );

    assert!(
        network_egress_boundary_refusal_json(
            "web",
            "web_fetch",
            Some(&ctx),
            Some(&store),
            Some(session_id),
            "researcher.default",
            None,
        )
        .is_none(),
        "declassified session should allow network egress"
    );

    store.delete_egress_declassification_grants(root)?;
    assert!(
        !store.egress_declassification_allows(
            &session_network_declass_target(root),
            Sink::Network,
            session_id,
            root,
        )?
    );
    Ok(())
}

#[test]
fn acceptance_capsule_export_withholds_by_destination() -> anyhow::Result<()> {
    use autonoetic_gateway::capsule::export;
    use autonoetic_types::agent_revision::{
        AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
    };
    use autonoetic_types::capsule::CapsuleMode;
    use autonoetic_types::memory::MemoryObject;
    use autonoetic_types::principal::PrincipalKind;

    let tmp = tempfile::tempdir()?;
    let agents_dir = tmp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_id = "agent.acc";
    let revision_id = "rev_acc";
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&rev_dir)?;
    std::fs::write(rev_dir.join("SKILL.md"), "# test\n")?;
    std::fs::write(rev_dir.join("runtime.lock"), "agents: []\n")?;
    store.insert_agent_revision(&AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{}", "a".repeat(64)),
        runtime_lock_hash: format!("sha256:{}", "b".repeat(64)),
        manifest_hash: format!("sha256:{}", "c".repeat(64)),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "artifact".to_string(),
        source_ref: None,
        origin_node_id: "node-A".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Ready,
        metadata_json: serde_json::Value::Null,
        short_id: "abcd1234".to_string(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    })?;
    store.upsert_agent_alias(&AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: revision_id.to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        updated_by_type: PrincipalKind::Human.tag().to_string(),
        updated_by_id: "test".to_string(),
        reason: None,
        suspended_at: None,
        suspended_reason: None,
        suspended_by: None,
    })?;

    let mut local_mem = MemoryObject::new(
        "mem-local-acc".into(),
        "memory".into(),
        agent_id.into(),
        agent_id.into(),
        "root-acc/sess".into(),
        "secret".into(),
    );
    local_mem.egress_label = Some(EgressLabel::local_only());
    store.memory_upsert(&local_mem)?;

    let out_path = tmp.path().join("acc.capsule.tar.zst");
    let mut config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };
    config.capsule.auto_sign = false;

    let outcome = export(
        ExportRequest {
            agent_id: agent_id.to_string(),
            revision_id: Some(revision_id.to_string()),
            mode: CapsuleMode::Thin,
            include_memory: Some(true),
            sign: Some(false),
            output_path: Some(out_path),
            session_id: None,
            root_session_id: None,
            destination_sink: Some(Sink::RemoteModel),
            trust_domain: None,
        },
        ExportContext {
            gateway_dir: &gateway_dir,
            gateway_config: &config,
            gateway_store: &store,
        },
    )?;

    assert_eq!(outcome.memory_withheld_count, 1);
    assert_eq!(outcome.destination_sink, "remote_model");
    Ok(())
}
