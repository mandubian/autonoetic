//! Phase 4 (#909) sandbox composition: session-taint network gate helpers.

use std::sync::Arc;

use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::egress_labeler::{
    require_boundary_session_taint, resolve_session_egress_taint,
    session_host_network_declass_target, session_network_declass_target,
    session_network_declassified, session_network_declassified_for_hosts,
};
use autonoetic_gateway::runtime::remote_access::{DetectedPatternCategory, 
    classify_network_coverage, DetectedPattern, NetworkCoverage,
};
use autonoetic_gateway::scheduler::approval::{apply_decision, ApproveOptions, DecisionContext};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalStatus, GrantScope, ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::egress::{EgressLabel, Sink};

fn run_ctx(session_id: &str, taint: Option<EgressLabel>) -> NativeToolRunContext {
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
        egress_taint: taint,
        egress_query_sink: None,
    }
}

#[test]
fn resolve_session_egress_taint_prefers_run_context() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    store.set_session_egress_taint("sess-a", &EgressLabel::unrestricted())?;

    let ctx = run_ctx("sess-a", Some(EgressLabel::local_only()));
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
fn require_boundary_session_taint_refuses_without_store() {
    let err = require_boundary_session_taint(None, None, Some("sess-x")).unwrap_err();
    assert!(
        err.to_string().contains("egress_boundary_unknown_taint"),
        "expected fail-closed unknown taint, got: {err}"
    );
}

#[test]
fn require_boundary_session_taint_store_miss_is_unrestricted() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let got = require_boundary_session_taint(None, Some(&store), Some("clean-sess"))?;
    assert_eq!(got, EgressLabel::unrestricted());
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
        category: DetectedPatternCategory::DependencyInstall,
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

#[test]
fn sandbox_exec_approval_under_taint_materializes_host_scoped_declass() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let config = GatewayConfig::default();
    let session_id = "root-net/coder";
    let root = "root-net";
    store.set_session_egress_taint(session_id, &EgressLabel::local_only())?;

    let decision = ApprovalDecision {
        request_id: "apr-sandbox-declass".to_string(),
        session_id: session_id.to_string(),
        root_session_id: Some(root.to_string()),
        agent_id: "coder.default".to_string(),
        action: ScheduledAction::SandboxExec {
            command: "python fetch.py".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["example.com".to_string()]),
            intent: None,
        },
        status: ApprovalStatus::Approved,
        decided_by: "operator".to_string(),
        decided_at: chrono::Utc::now().to_rfc3339(),
        reason: Some("allow network for this task".to_string()),
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
        !session_network_declassified(&store, session_id, root),
        "SandboxExec approve under taint must NOT materialize a session-wide grant"
    );
    assert!(
        session_network_declassified_for_hosts(
            &store,
            session_id,
            root,
            &["example.com".to_string()],
        ),
        "SandboxExec approve under taint must materialize a host-scoped declass grant"
    );
    assert!(
        !session_network_declassified_for_hosts(
            &store,
            session_id,
            root,
            &["example.com".to_string(), "other.com".to_string()],
        ),
        "host grant for example.com must not cover other.com"
    );
    let target = session_host_network_declass_target(root, "example.com");
    assert!(store.egress_declassification_allows(
        &target,
        Sink::Network,
        session_id,
        root,
    )?);
    assert!(
        !store.egress_declassification_allows(
            &session_network_declass_target(root),
            Sink::Network,
            session_id,
            root,
        )?,
        "no silent session-wide widen"
    );

    let events = store.search_causal_events(Some(session_id), None, 50)?;
    assert!(
        events.iter().any(|e| e.action == "egress.declassified"),
        "expected egress.declassified for sandbox widen"
    );
    Ok(())
}

#[test]
fn bare_wildcard_source_pattern_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();
    let err = store
        .insert_egress_declassification_grant(
            "root",
            "root/sess",
            "coder.default",
            &autonoetic_types::egress::EgressDeclassificationTarget::SourcePattern("*".into()),
            Sink::Network,
            &GrantScope::RootSession,
            "operator",
            &chrono::Utc::now().to_rfc3339(),
            None,
            None,
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("bound"),
        "expected bound-pattern refuse, got: {err}"
    );
}
