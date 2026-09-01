//! #1197: a gate opening in an appointed scope is routed to the seat, and
//! every other case parks for the operator.
//!
//! The matrix below is the point of the feature, not a formality. Routing
//! decides *that* a gate reaches a decider; it never resolves one. So the
//! failure this suite guards is not "a gate was decided wrongly" — it is "a
//! gate was routed that should not have been", and its mirror, "a gate parked
//! that should have been seen". The first is a safety failure, the second is
//! only a missed convenience, which is why every ambiguous case here is
//! asserted to park.

use autonoetic_gateway::decider_appointment::{appoint, revoke, route_gate_to_decider};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{ApprovalRisk, ScheduledAction};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;

use crate::gate_decider::{seed_decider_revision, write_agent_dir};

struct Fx {
    _temp: tempfile::TempDir,
    cfg: GatewayConfig,
    store: GatewayStore,
}

fn fx() -> anyhow::Result<Fx> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    write_agent_dir(
        &agents_dir,
        "nightwatch.default",
        &[Capability::GateDecider {
            kinds: vec!["approval".to_string()],
        }],
    );
    let mut llm_presets = std::collections::HashMap::new();
    llm_presets.insert(
        "decider".to_string(),
        autonoetic_types::config::LlmPreset {
            provider: Some("anthropic".to_string()),
            model: Some("claude-opus-4-20250514".to_string()),
            temperature: Some(0.0),
            fallback_provider: None,
            fallback_model: None,
            chat_only: None,
            context_window_tokens: None,
            max_tokens: None,
            base_url: None,
            api_key_env: None,
            thinking: None,
            tier: None,
            cost: None,
            latency: None,
            routing: None,
            egress_class: None,
            request_timeout_secs: None,
            ttfb_timeout_secs: None,
        },
    );
    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        llm_presets,
        ..Default::default()
    };
    let store = GatewayStore::open(&gateway_dir)?;
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "nightwatch.default")?;
    Ok(Fx {
        _temp: temp,
        cfg,
        store,
    })
}

fn seat(f: &Fx, scope: &str, ceiling: ApprovalRisk) -> anyhow::Result<String> {
    let a = appoint(
        &f.cfg,
        &f.store,
        autonoetic_gateway::decider_appointment::AppointmentRequest {
            decider_agent: "nightwatch.default".to_string(),
            kinds: vec!["approval".to_string()],
            scope_root_session: scope.to_string(),
            risk_ceiling: ceiling,
            advice_only: true,
            expires_at: None,
            max_gates: None,
            appointed_by: "operator".to_string(),
        },
    )?;
    Ok(a.appointment_id)
}

/// `sandbox_exec` with detected hosts is High — the Night Shift shape.
fn high_risk_exec() -> ScheduledAction {
    ScheduledAction::SandboxExec {
        command: "curl https://stooq.com/q/l/?s=aapl".to_string(),
        dependencies: None,
        requires_approval: true,
        evidence_ref: None,
        detected_hosts: Some(vec!["stooq.com".to_string()]),
        intent: None,
    }
}

/// No detected hosts — Standard.
fn standard_exec() -> ScheduledAction {
    ScheduledAction::SandboxExec {
        command: "python3 -m pytest -q".to_string(),
        dependencies: None,
        requires_approval: true,
        evidence_ref: None,
        detected_hosts: None,
        intent: None,
    }
}

// ── Routed ─────────────────────────────────────────────────────────────────

#[test]
fn a_gate_in_an_appointed_scope_at_or_below_the_ceiling_is_routed() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1", ApprovalRisk::High)?;

    let routed = route_gate_to_decider(&f.store, "apr-1", "root-1/coder", &high_risk_exec())?
        .expect("a covered gate is routed");
    assert_eq!(routed.appointment_id, apt);
    assert_eq!(routed.gate_kind, "approval");
    assert_eq!(routed.gate_risk, "high");
    assert!(
        routed.advice_only,
        "phase 1 seats are advisory, so the gate parks whatever the seat says"
    );
    assert!(
        routed.is_awaiting_verdict(),
        "routing records that a gate reached the seat, not what the seat said"
    );

    // On the chain, so a 3am referral is visible in the morning.
    let events = f
        .store
        .search_causal_events(None, Some("nightwatch.default"), 20)?;
    assert!(
        events.iter().any(|e| e.action == "decider.gate_routed"),
        "routing a gate is a governance act and belongs on the record"
    );
    Ok(())
}

#[test]
fn routing_is_idempotent_for_a_retried_gate_creation() -> anyhow::Result<()> {
    let f = fx()?;
    seat(&f, "root-1", ApprovalRisk::High)?;
    assert!(route_gate_to_decider(&f.store, "apr-1", "root-1/coder", &high_risk_exec())?.is_some());
    assert!(
        route_gate_to_decider(&f.store, "apr-1", "root-1/coder", &high_risk_exec())?.is_none(),
        "a retried gate creation is not a second referral"
    );
    assert_eq!(f.store.list_decider_gate_routings("apr-1")?.len(), 1);
    Ok(())
}

// ── The park matrix ────────────────────────────────────────────────────────

#[test]
fn a_gate_parks_when_nothing_is_seated() -> anyhow::Result<()> {
    let f = fx()?;
    assert!(
        route_gate_to_decider(&f.store, "apr-1", "root-1/coder", &high_risk_exec())?.is_none(),
        "no appointment means no routing — the status quo, not a failure"
    );
    Ok(())
}

#[test]
fn a_gate_above_the_ceiling_parks() -> anyhow::Result<()> {
    let f = fx()?;
    seat(&f, "root-1", ApprovalRisk::Standard)?;
    // The consequence stated in the CLI docs: a Standard-ceiling night watch
    // decides neither Night Shift gate, because both are High.
    assert!(
        route_gate_to_decider(&f.store, "apr-1", "root-1/coder", &high_risk_exec())?.is_none(),
        "a gate above the ceiling parks for the operator"
    );
    // ... and the same seat does cover a Standard gate, so the ceiling is
    // bounding the risk rather than failing open or closed wholesale.
    assert!(
        route_gate_to_decider(&f.store, "apr-2", "root-1/coder", &standard_exec())?.is_some()
    );
    Ok(())
}

#[test]
fn a_gate_in_another_run_parks() -> anyhow::Result<()> {
    let f = fx()?;
    seat(&f, "root-1", ApprovalRisk::High)?;
    assert!(
        route_gate_to_decider(&f.store, "apr-1", "root-2/coder", &high_risk_exec())?.is_none(),
        "an appointment is run-scoped, never a standing grant"
    );
    Ok(())
}

#[test]
fn a_gate_of_an_unappointed_kind_parks() -> anyhow::Result<()> {
    let f = fx()?;
    seat(&f, "root-1", ApprovalRisk::High)?;
    let escalation = ScheduledAction::SessionEscalate {
        session_id: "root-1/coder".to_string(),
        root_session_id: "root-1".to_string(),
        requested_by_agent_id: "coder.default".to_string(),
        reason: "stuck".to_string(),
        context: String::new(),
        urgency: "normal".to_string(),
        suggested_actions: Vec::new(),
        payload: None,
        kind: Default::default(),
    };
    assert!(
        route_gate_to_decider(&f.store, "apr-1", "root-1/coder", &escalation)?.is_none(),
        "seated for approvals only — an escalation is a different grant"
    );
    Ok(())
}

#[test]
fn a_gate_parks_once_the_seat_is_revoked() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1", ApprovalRisk::High)?;
    assert!(route_gate_to_decider(&f.store, "apr-1", "root-1/coder", &high_risk_exec())?.is_some());

    revoke(&f.store, &apt, "operator", Some("shift over"))?;

    assert!(
        route_gate_to_decider(&f.store, "apr-2", "root-1/coder", &high_risk_exec())?.is_none(),
        "revocation takes effect on the next gate"
    );
    Ok(())
}

#[test]
fn a_gate_parks_once_the_appointment_has_expired() -> anyhow::Result<()> {
    let f = fx()?;
    let a = appoint(
        &f.cfg,
        &f.store,
        autonoetic_gateway::decider_appointment::AppointmentRequest {
            decider_agent: "nightwatch.default".to_string(),
            kinds: vec!["approval".to_string()],
            scope_root_session: "root-1".to_string(),
            risk_ceiling: ApprovalRisk::High,
            advice_only: true,
            expires_at: Some("2000-01-01T00:00:00Z".to_string()),
            max_gates: None,
            appointed_by: "operator".to_string(),
        },
    )?;
    assert!(a.is_expired(&chrono::Utc::now().to_rfc3339()));
    assert!(
        route_gate_to_decider(&f.store, "apr-1", "root-1/coder", &high_risk_exec())?.is_none(),
        "an expired seat routes nothing"
    );
    Ok(())
}

// ── The property that matters most ─────────────────────────────────────────

#[test]
fn routing_never_resolves_a_gate() -> anyhow::Result<()> {
    // The whole safety argument for event-driven routing: a bug here fails to
    // route, which parks. There must be no path from routing to a decision.
    let f = fx()?;
    seat(&f, "root-1", ApprovalRisk::High)?;

    let mut request = autonoetic_types::background::ApprovalRequest {
        request_id: "apr-1".to_string(),
        agent_id: "coder.default".to_string(),
        session_id: "root-1/coder".to_string(),
        action: high_risk_exec(),
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
        evidence_ref: None,
        root_session_id: Some("root-1".to_string()),
        workflow_id: None,
        task_id: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: autonoetic_types::background::ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    };
    f.store.create_approval(&mut request)?;
    route_gate_to_decider(&f.store, "apr-1", "root-1/coder", &high_risk_exec())?
        .expect("routed");

    let after = f.store.get_approval("apr-1")?.expect("gate still exists");
    assert!(
        after.decided_at.is_none() && after.decided_by.is_none(),
        "routing must leave the gate pending for the operator: {:?}",
        after.status
    );
    Ok(())
}


// ── Through the real gate path, not the routing function ───────────────────

/// Drives `GateService::check` — the path a real gate takes — rather than
/// calling `route_gate_to_decider` directly.
///
/// Every other test here would still pass if routing were never wired into
/// gate creation. That is the failure mode this series keeps hitting: a
/// function that works and a surface that never calls it. This asserts the
/// wiring.
#[test]
fn a_gate_created_through_the_gate_service_is_routed() -> anyhow::Result<()> {
    use autonoetic_gateway::runtime::human_gate::{
        DecisionContext, GateKind, GateRequest, GateService, MatchStrategy,
    };
    use std::sync::Arc;

    let f = fx()?;
    seat(&f, "root-1", ApprovalRisk::High)?;

    let store = Arc::new(GatewayStore::open(
        &f.cfg.runtime_dir,
    )?);
    let svc = GateService::new(store.clone());
    let manifest = crate::gate_decider::agent_manifest("coder.default", vec![]);

    let result = svc.check(GateRequest {
        kind: GateKind::Approval {
            action: high_risk_exec(),
            targets: vec!["stooq.com".to_string()],
            match_strategy: MatchStrategy::HostLevel,
        },
        manifest: &manifest,
        session_id: Some("root-1/coder"),
        run_context: None,
        config: None,
        context: DecisionContext::tier1("fetch prices", "test gate"),
        summary: "fetch prices".into(),
        approval_ref: None,
        request_id: None,
        pre_validated: false,
        cache_backfill: None,
        turn_id: None,
    })?;

    let gate_id = match &result {
        autonoetic_gateway::runtime::human_gate::GateResult::Suspended { gate_id, .. }
        | autonoetic_gateway::runtime::human_gate::GateResult::AlreadyPending { gate_id, .. } => {
            gate_id.clone()
        }
        other => panic!("expected a gate to open, got {other:?}"),
    };

    let routings = store.list_decider_gate_routings(&gate_id)?;
    assert_eq!(
        routings.len(),
        1,
        "a gate opened through the real path must reach the seated decider"
    );
    assert!(routings[0].is_awaiting_verdict());
    Ok(())
}
