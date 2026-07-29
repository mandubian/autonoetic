//! Phase 4 (#909) slice 2b: gateway-native network egress (web tools + hooks).

use std::sync::Arc;

use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::egress_labeler::{
    network_egress_boundary_refusal_json, session_network_declass_target,
};
use autonoetic_gateway::scheduler::approval::{apply_decision, ApproveOptions, DecisionContext};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalStatus, GrantScope, ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::egress::{EgressLabel, Sink};

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
        agent_id: "researcher.default".into(),
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

#[test]
fn web_network_egress_refused_under_local_only_taint() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-web/researcher";
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
    .expect("expected refusal without declassification grant");
    let payload: serde_json::Value = serde_json::from_str(&refusal)?;
    assert_eq!(payload["error_type"], "egress_boundary_refused");
    assert_eq!(payload["surface"], "web");
    assert_eq!(payload["tool"], "web_fetch");

    let events = store.search_causal_events(Some(session_id), None, 10)?;
    assert!(
        events.iter().any(|e| e.action == "egress.boundary_refused"),
        "expected egress.boundary_refused"
    );
    Ok(())
}

#[test]
fn web_network_egress_allowed_with_declassification_grant() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-web2/researcher";
    let root = "root-web2";
    let ctx = run_ctx(session_id, EgressLabel::local_only());

    store.insert_egress_declassification_grant(
        root,
        session_id,
        "researcher.default",
        &session_network_declass_target(root),
        Sink::Network,
        &GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;

    let refusal = network_egress_boundary_refusal_json(
        "web",
        "web_fetch",
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "researcher.default",
        None,
    );
    assert!(refusal.is_none(), "declassified session should allow network egress");
    Ok(())
}

#[test]
fn web_fetch_approval_under_taint_materializes_network_declass() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let config = GatewayConfig::default();
    let session_id = "root-fetch/researcher";
    let root = "root-fetch";
    store.set_session_egress_taint(session_id, &EgressLabel::local_only())?;

    let decision = ApprovalDecision {
        request_id: "apr-web-fetch".to_string(),
        session_id: session_id.to_string(),
        root_session_id: Some(root.to_string()),
        agent_id: "researcher.default".to_string(),
        action: ScheduledAction::WebFetch {
            url: "https://example.com/doc".to_string(),
            timeout_secs: Some(20),
            max_chars: Some(10_000),
            detected_hosts: Some(vec!["example.com".to_string()]),
            payload: None,
        },
        status: ApprovalStatus::Approved,
        decided_by: "operator".to_string(),
        decided_at: chrono::Utc::now().to_rfc3339(),
        reason: Some("allow fetch".to_string()),
        workflow_id: None,
        task_id: None,
        approval_level: ApprovalLevel::Operator,
    };

    apply_decision(
        &config,
        Some(&store),
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
        "WebFetch approval under taint should emit egress.declassified"
    );
    Ok(())
}

#[test]
fn hooks_network_egress_refused_under_local_only_taint() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-hook/sess";
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
    .expect("expected hook refusal without declassification grant");
    let payload: serde_json::Value = serde_json::from_str(&refusal)?;
    assert_eq!(payload["error_type"], "egress_boundary_refused");
    assert_eq!(payload["surface"], "hooks");
    Ok(())
}

#[test]
fn web_search_network_egress_refused_under_local_only_taint() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-search/researcher";
    let ctx = run_ctx(session_id, EgressLabel::local_only());

    let refusal = network_egress_boundary_refusal_json(
        "web",
        "web_search",
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "researcher.default",
        None,
    )
    .expect("expected web_search refusal without declassification grant");
    let payload: serde_json::Value = serde_json::from_str(&refusal)?;
    assert_eq!(payload["surface"], "web");
    assert_eq!(payload["tool"], "web_search");
    Ok(())
}

#[test]
fn web_call_network_egress_refused_under_local_only_taint() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-call/researcher";
    let ctx = run_ctx(session_id, EgressLabel::local_only());

    let refusal = network_egress_boundary_refusal_json(
        "web",
        "web_call",
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "researcher.default",
        None,
    )
    .expect("expected web_call refusal without declassification grant");
    let payload: serde_json::Value = serde_json::from_str(&refusal)?;
    assert_eq!(payload["tool"], "web_call");
    Ok(())
}

#[test]
fn hooks_network_egress_allowed_with_declassification_grant() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-hook2/sess";
    let root = "root-hook2";
    store.set_session_egress_taint(session_id, &EgressLabel::local_only())?;
    store.insert_egress_declassification_grant(
        root,
        session_id,
        "planner.default",
        &session_network_declass_target(root),
        Sink::Network,
        &GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;

    let refusal = network_egress_boundary_refusal_json(
        "hooks",
        "http.callback",
        None,
        Some(&store),
        Some(session_id),
        "planner.default",
        None,
    );
    assert!(refusal.is_none(), "declassified hook delivery should be allowed");
    Ok(())
}