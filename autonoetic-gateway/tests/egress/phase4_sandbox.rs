//! Phase 4 (#909) sandbox composition: session-taint network gate helpers.

use std::sync::Arc;

use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::egress_labeler::resolve_session_egress_taint;
use autonoetic_gateway::runtime::remote_access::{
    classify_network_coverage, DetectedPattern, NetworkCoverage,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::{EgressLabel, Sink};

#[test]
fn resolve_session_egress_taint_prefers_run_context() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    store.set_session_egress_taint("sess-a", &EgressLabel::unrestricted())?;

    let ctx = NativeToolRunContext {
        registry: ActiveExecutionRegistry::new(),
        root_session_id: "root".into(),
        workflow_id: None,
        task_id: None,
        session_id: "sess-a".into(),
        agent_id: "coder.default".into(),
        live_digest: None,
        live_report: None,
        user_id: None,
        artifact_id: None,
        sentinel_suppress_target: None,
        discovered_tools: None,
        tool_discovery_catalog: None,
        wake_hint: None,
        wake_hints_map: None,
        egress_taint: Some(EgressLabel::local_only()),
        egress_query_sink: None,
    };

    let got = resolve_session_egress_taint(Some(&ctx), Some(&store), Some("sess-a"))?;
    assert_eq!(got, Some(EgressLabel::local_only()));
    Ok(())
}

#[test]
fn resolve_session_egress_taint_falls_back_to_store_row() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    store.set_session_egress_taint("sess-b", &EgressLabel::no_remote_model())?;

    let got = resolve_session_egress_taint(None, Some(&store), Some("sess-b"))?;
    assert_eq!(got, Some(EgressLabel::no_remote_model()));
    Ok(())
}

#[test]
fn network_sink_excluded_when_local_only_taint() {
    let taint = EgressLabel::local_only();
    assert!(!taint.allows(Sink::Network));
}

#[test]
fn unresolved_coverage_with_taint_is_sandbox_hard_refuse_shape() {
    let patterns = vec![DetectedPattern {
        category: "dependency_install".into(),
        pattern: "pip install requests".into(),
        line_number: Some(1),
        reason: "package manager".into(),
    }];
    let coverage = classify_network_coverage(&patterns, vec![]);
    assert!(matches!(coverage, NetworkCoverage::Unresolved));
}

#[test]
fn surface_boundary_refused_emits_causal_event() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    autonoetic_gateway::runtime::egress_labeler::emit_surface_boundary_refused(
        &store,
        "sess-refuse",
        "coder.default",
        Some("turn-1"),
        "sandbox",
        &EgressLabel::local_only(),
        &[],
        "test refuse",
    );
    let events = store.search_causal_events(Some("sess-refuse"), None, 10)?;
    assert!(
        events
            .iter()
            .any(|e| e.action == "egress.boundary_refused"),
        "expected egress.boundary_refused"
    );
    let payload_raw = events
        .iter()
        .find(|e| e.action == "egress.boundary_refused")
        .and_then(|e| e.payload.as_ref())
        .expect("payload");
    let payload: serde_json::Value = serde_json::from_str(payload_raw)?;
    assert_eq!(payload["surface"], "sandbox");
    Ok(())
}
