//! Phase 4 (#909) slice 2b: gateway-native network egress (web tools + hooks).
//!
//! Declassification grants are **host-scoped** (`session:<root>:host:<host>`):
//! approving egress to one host widens only that host. Session-wide
//! `session:<root>` grants remain possible via the explicit `EgressDeclassify`
//! action and still satisfy every boundary check.

use std::sync::Arc;

use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::egress_labeler::{
    network_egress_boundary_refusal_json, session_host_network_declass_target,
    session_network_declass_target,
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
            annotation_counter: None,
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
        Some("example.com"),
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
fn web_network_egress_allowed_with_host_scoped_declassification_grant() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-web2/researcher";
    let root = "root-web2";
    let ctx = run_ctx(session_id, EgressLabel::local_only());

    store.insert_egress_declassification_grant(
        root,
        session_id,
        "researcher.default",
        &session_host_network_declass_target(root, "example.com"),
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
        Some("example.com"),
    );
    assert!(
        refusal.is_none(),
        "host-scoped declassification should allow egress to that host"
    );

    // A different host is NOT covered by the example.com grant.
    let other = network_egress_boundary_refusal_json(
        "web",
        "web_fetch",
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "researcher.default",
        None,
        Some("other.com"),
    );
    assert!(
        other.is_some(),
        "host-scoped grant must not widen to other hosts"
    );
    Ok(())
}

#[test]
fn web_network_egress_allowed_with_session_wide_declassification_grant() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session_id = "root-web3/researcher";
    let root = "root-web3";
    let ctx = run_ctx(session_id, EgressLabel::local_only());

    // Explicit session-wide declass (EgressDeclassify path) still satisfies
    // every boundary check, any host.
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
        Some("anything.example.com"),
    );
    assert!(
        refusal.is_none(),
        "session-wide declassification should allow egress to any host"
    );
    Ok(())
}

#[test]
fn web_fetch_approval_under_taint_materializes_host_scoped_declass() -> anyhow::Result<()> {
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

    // Host-scoped grant materialized for the approved host…
    assert!(store.egress_declassification_allows(
        &session_host_network_declass_target(root, "example.com"),
        Sink::Network,
        session_id,
        root,
    )?);
    // …but NOT a silent session-wide widen…
    assert!(
        !store.egress_declassification_allows(
            &session_network_declass_target(root),
            Sink::Network,
            session_id,
            root,
        )?,
        "approving one host must not materialize a session-wide grant"
    );
    // …and NOT a grant for any other host.
    assert!(
        !store.egress_declassification_allows(
            &session_host_network_declass_target(root, "other.com"),
            Sink::Network,
            session_id,
            root,
        )?,
        "approving example.com must not declassify other.com"
    );

    let events = store.search_causal_events(Some(session_id), None, 20)?;
    let declass = events
        .iter()
        .find(|e| e.action == "egress.declassified")
        .expect("WebFetch approval under taint should emit egress.declassified");
    let payload: serde_json::Value =
        serde_json::from_str(declass.payload.as_deref().unwrap_or("{}"))?;
    assert_eq!(
        payload["target"]["value"].as_str(),
        Some(format!("session:{root}:host:example.com").as_str())
    );
    Ok(())
}

#[test]
fn network_approval_under_taint_without_hosts_materializes_nothing() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let config = GatewayConfig::default();
    let session_id = "root-nohost/researcher";
    let root = "root-nohost";
    store.set_session_egress_taint(session_id, &EgressLabel::local_only())?;

    let decision = ApprovalDecision {
        request_id: "apr-no-hosts".to_string(),
        session_id: session_id.to_string(),
        root_session_id: Some(root.to_string()),
        agent_id: "researcher.default".to_string(),
        action: ScheduledAction::WebSearch {
            query: "weather".to_string(),
            provider: None,
            max_results: Some(5),
            timeout_secs: None,
            engine_url: None,
            duckduckgo_engine_url: None,
            google_engine_url: None,
            google_engine_id: None,
            google_api_key_env: None,
            google_engine_id_env: None,
            cache_ttl_secs: None,
            detected_hosts: None,
            payload: None,
        },
        status: ApprovalStatus::Approved,
        decided_by: "operator".to_string(),
        decided_at: chrono::Utc::now().to_rfc3339(),
        reason: Some("allow search".to_string()),
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

    assert!(
        !store.egress_declassification_allows(
            &session_network_declass_target(root),
            Sink::Network,
            session_id,
            root,
        )?,
        "approval without detected_hosts must not widen anything"
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
        Some("hooks.example.com"),
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
        Some("search.example.com"),
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
        Some("api.example.com"),
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
        &session_host_network_declass_target(root, "hooks.example.com"),
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
        Some("hooks.example.com"),
    );
    assert!(refusal.is_none(), "declassified hook delivery should be allowed");
    Ok(())
}
