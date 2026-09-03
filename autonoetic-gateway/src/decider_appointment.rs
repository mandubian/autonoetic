//! Appointing a decider for a run (#1195, umbrella #1191).
//!
//! The `GateDecider` capability says an agent *may* hold the seat. This module
//! is the operator act that *seats* one, and it is the single chokepoint where
//! that act is validated — so the invariants hold no matter which activation
//! surface the operator used (CLI, plan-card field, session-launch flag). The
//! surfaces are ergonomics over one record; they are not alternative code
//! paths.
//!
//! The gateway is a Lawful Executor here as elsewhere: every refusal below is
//! a mechanical check with a named reason, not reserved judgment.

use anyhow::Result;

use autonoetic_types::background::ApprovalRisk;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::decider_appointment::{
    AppointmentError, DeciderAppointment, DeciderGateRouting, APPOINTABLE_GATE_KINDS,
};

use crate::scheduler::gateway_store::GatewayStore;

/// What an operator supplies to seat a decider. Deliberately not the stored
/// record: `appointment_id`, `appointed_at` and `gates_decided` are the
/// gateway's to assign, not the caller's to claim.
#[derive(Debug, Clone)]
pub struct AppointmentRequest {
    pub decider_agent: String,
    pub kinds: Vec<String>,
    pub scope_root_session: String,
    pub risk_ceiling: ApprovalRisk,
    pub advice_only: bool,
    pub expires_at: Option<String>,
    pub max_gates: Option<u32>,
    pub appointed_by: String,
}

/// Seat a decider for a run.
///
/// Refuses, in order: an empty scope, no kinds, an unknown kind, a `Critical`
/// ceiling, a binding (non-advisory) appointment, a malformed expiry, and an
/// appointee that does not already hold `GateDecider` covering every named
/// kind. The capability check is last because it is the one that needs to
/// touch the agent repository.
pub fn appoint(
    config: &GatewayConfig,
    store: &GatewayStore,
    req: AppointmentRequest,
) -> Result<DeciderAppointment> {
    if req.scope_root_session.trim().is_empty() {
        return Err(AppointmentError::NoScope.into());
    }
    if req.kinds.is_empty() {
        return Err(AppointmentError::NoKinds.into());
    }
    for kind in &req.kinds {
        if !APPOINTABLE_GATE_KINDS.contains(&kind.as_str()) {
            return Err(AppointmentError::UnknownKind { kind: kind.clone() }.into());
        }
    }

    // Promotion and secret delivery are not delegable by a single operator
    // gesture, so `Critical` is refused at appointment time rather than left
    // sitting above a configurable ceiling where a later edit could raise it.
    if req.risk_ceiling == ApprovalRisk::Critical {
        return Err(AppointmentError::CriticalNotAppointable.into());
    }

    // Phase 1 is advisory-only (§4.4): a judgment layer earns binding
    // authority from measured agreement, not from assertion at creation time.
    if !req.advice_only {
        return Err(AppointmentError::BindingNotYetAvailable.into());
    }

    if let Some(exp) = &req.expires_at {
        if chrono::DateTime::parse_from_rfc3339(exp).is_err() {
            return Err(AppointmentError::BadExpiry { value: exp.clone() }.into());
        }
    }

    // An appointment never widens capabilities: the appointee must already
    // hold `GateDecider` covering every kind named. Checked against the
    // promoted revision, so an agent that exists only as an ungated bundle
    // copy cannot be seated.
    let gateway_dir = crate::execution::gateway_root_dir(config);
    let repo = crate::agent::repository::AgentRepository::from_config(config);
    let loaded = repo
        .get_sync_from_store(&req.decider_agent, &gateway_dir, Some(store))
        .map_err(|e| {
            anyhow::anyhow!(
                "Cannot appoint '{}': it does not resolve to an installed agent ({}); \
                 an appointment names an existing capability holder, it does not create one",
                req.decider_agent,
                e
            )
        })?;
    let held = held_gate_kinds(&loaded.manifest.capabilities);
    if held.is_empty() {
        return Err(AppointmentError::NotADecider {
            agent_id: req.decider_agent.clone(),
        }
        .into());
    }
    for kind in &req.kinds {
        if !held.iter().any(|h| h == "*" || h == kind) {
            return Err(AppointmentError::KindNotHeld {
                agent_id: req.decider_agent.clone(),
                kind: kind.clone(),
            }
            .into());
        }
    }

    // Resolve the preset now, and refuse a routing one. `is_routing_preset`
    // means the model is chosen per call, so the seat would be served by
    // different models on different gates — see `RoutingPresetNotSeatable`.
    let profile = crate::runtime::inference_profile::resolve_inference_profile(
        &req.decider_agent,
        &loaded.manifest,
        config,
        None,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "Cannot appoint '{}': its inference preset does not resolve ({}). An appointment \
             records the model it seated, and an agent whose model cannot be resolved could not \
             decide anything anyway — better to fail here than at 3am. Define the preset named by \
             the agent's `llm_preset` in `llm_presets` (see docs/reference/config.md)",
            req.decider_agent,
            e
        )
    })?;
    if profile.is_routing_preset {
        return Err(AppointmentError::RoutingPresetNotSeatable {
            agent_id: req.decider_agent.clone(),
            preset: profile.preset_name.clone().unwrap_or_else(|| "?".to_string()),
        }
        .into());
    }

    let now = chrono::Utc::now().to_rfc3339();
    // Pin the closure, not just the name. The revision carries the
    // instructions, the capabilities and the model; calibration evidence
    // (#1198) is a property of what actually produced the verdicts, so the
    // record says which revision was seated. A later promotion changes the
    // agent without silently changing what this appointment seated.
    // Not best-effort: an appointment that cannot name its revision cannot
    // support the agreement rate it exists to justify. `get_sync_from_store`
    // above already resolved through the promoted alias, so a miss here is an
    // inconsistency, not a normal case — surface it rather than writing a
    // record that quietly lacks the pin.
    let decider_revision = store
        .get_agent_alias(&req.decider_agent)
        .map_err(|e| {
            anyhow::anyhow!(
                "Cannot appoint '{}': failed to read its promoted revision ({}); \
                 an appointment must record the revision it seated",
                req.decider_agent,
                e
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot appoint '{}': it has no promoted revision to seat; \
                 an appointment must record the revision it seated",
                req.decider_agent
            )
        })?
        .revision_id;

    let appointment_id = format!("apt_{}", uuid::Uuid::new_v4());
    // The peer-root session: a **top-level** id, so it shares no root with the
    // run and is therefore outside it for P-10.7, budget, emergency stop,
    // session grants and content-visibility push — none of which needs a
    // hand-written exclusion (#1196). Owned by the decider agent so P-10.7's
    // ownership check authenticates, with the appointing operator as the
    // recorded principal so the chain reads as delegation, not spawn.
    let decider_session = format!("decider-{}", uuid::Uuid::new_v4());

    let appointment = DeciderAppointment {
        appointment_id,
        decider_agent: req.decider_agent,
        decider_revision,
        decider_provider: Some(profile.llm_config.provider.clone()),
        decider_model: Some(profile.llm_config.model.clone()),
        kinds: req.kinds,
        scope_root_session: req.scope_root_session,
        decider_session: Some(decider_session.clone()),
        risk_ceiling: req.risk_ceiling,
        advice_only: req.advice_only,
        expires_at: req.expires_at,
        max_gates: req.max_gates,
        gates_decided: 0,
        appointed_by: req.appointed_by,
        appointed_at: now,
        revoked_at: None,
        revoked_by: None,
        revoked_reason: None,
    };

    // Record the peer-root session before the appointment, so an appointment
    // that exists always has a session P-10.7 can authenticate against.
    store.upsert_session_transcript(&autonoetic_types::causal_chain::SessionTranscriptRecord {
        transcript_id: format!("tr-{}", decider_session),
        session_id: decider_session.clone(),
        root_session_id: decider_session.clone(),
        agent_id: appointment.decider_agent.clone(),
        revision_id: Some(appointment.decider_revision.clone()),
        user_id: Some(appointment.appointed_by.clone()),
        started_at: appointment.appointed_at.clone(),
        ended_at: None,
        // A vocabulary the rest of the pipeline already handles. The seat
        // exists but has never run, and `suspended` is the one existing status
        // that says that honestly — `active` would inflate in-progress and
        // liveness counts with a session that has no turns. When #1197 wakes
        // it, `upsert_session_transcript`'s reopen path (excluded `active` over
        // a suspended row) is exactly the transition needed. A new status value
        // would be unhandled by that mapping and by the
        // `status IN ('active','suspended')` queries.
        status: "suspended".to_string(),
        turn_count: 0,
        transcript_handle: None,
        excerpt: None,
        origin_node_id: None,
    })?;
    store.insert_decider_appointment(&appointment)?;
    emit_appointment_event(store, &appointment, "decider.appointed", "appointed", None);
    Ok(appointment)
}

/// Revoke an appointment. Returns false when it does not exist or was already
/// revoked — a second revoke never rewrites the first one's attribution.
///
/// Revocation takes effect on the next gate; verdicts already attributed stay
/// attributed, because a ruling that was lawful when made does not become
/// unlawful when the seat is vacated.
pub fn revoke(
    store: &GatewayStore,
    appointment_id: &str,
    revoked_by: &str,
    reason: Option<&str>,
) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let revoked = store.revoke_decider_appointment(appointment_id, revoked_by, &now, reason)?;
    if revoked {
        if let Some(a) = store.get_decider_appointment(appointment_id)? {
            emit_appointment_event(store, &a, "decider.revoked", "revoked", reason);
        }
    }
    Ok(revoked)
}

/// Vacate every seat over a run, because the run is over (#1199).
///
/// A peer-root decider session outlives the run it serves — that is the point
/// of peer-root, and it is also why appointments need an explicit end. Without
/// this the system accumulates live seats pointing at finished runs, each one
/// still satisfying the provenance check that lets an agent decide.
///
/// Called from the two places a run actually ends: a non-suspended root
/// `close_session`, and emergency stop. **Not** on suspension — a suspended
/// run resumes and must keep its seat, the same reasoning that keeps session
/// grants and the egress policy alive across a yield.
///
/// Returns the appointments vacated. Idempotent: an already-revoked seat is
/// skipped rather than re-attributed.
pub fn revoke_appointments_for_scope(
    store: &GatewayStore,
    scope_root_session: &str,
    revoked_by: &str,
    reason: &str,
) -> Result<Vec<String>> {
    let mut vacated = Vec::new();
    for appointment in store.list_decider_appointments_for_scope(scope_root_session, true)? {
        match revoke(store, &appointment.appointment_id, revoked_by, Some(reason)) {
            Ok(true) => vacated.push(appointment.appointment_id),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                target: "decider_appointment",
                appointment_id = %appointment.appointment_id,
                scope = %scope_root_session,
                error = %e,
                "Failed to vacate a seat as its run ended; it will outlive the run"
            ),
        }
    }
    Ok(vacated)
}

/// The appointment covering a gate, if any: right scope, right kind, at or
/// below the ceiling, and not expired by revocation, clock or gate count.
///
/// Returns the most recent match. Overlapping appointments for one scope are
/// permitted — re-appointing without revoking first is an ordinary operator
/// act — and the newest wins.
pub fn active_appointment_for_gate(
    store: &GatewayStore,
    scope_root_session: &str,
    kind: &str,
    risk: ApprovalRisk,
) -> Result<Option<DeciderAppointment>> {
    let now = chrono::Utc::now().to_rfc3339();
    let candidates = store.list_decider_appointments_for_scope(scope_root_session, true)?;
    Ok(candidates
        .into_iter()
        .find(|a| a.covers(kind, risk) && !a.is_expired(&now)))
}

/// Any active, unexpired appointment naming this agent for this scope —
/// regardless of kind or ceiling. This is the *provenance* question ("is this
/// agent seated here at all?"), distinct from the *coverage* question ("does
/// its seat reach this gate?") answered by [`active_appointment_for_gate`].
pub fn agent_is_appointed_for_scope(
    store: &GatewayStore,
    agent_id: &str,
    scope_root_session: &str,
) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    Ok(store
        .list_decider_appointments_for_scope(scope_root_session, true)?
        .iter()
        .any(|a| a.decider_agent == agent_id && !a.is_expired(&now)))
}

/// The disclosure class an agent gets when viewing a gate in `gate_session_id`.
///
/// `Decider` when the agent holds an active appointment over that run,
/// `Agent` otherwise. Resolved per *gate*, not per caller: the same agent
/// listing gates from two runs gets two different classes, because the read
/// right belongs to the seat and not to the identity. That is also what makes
/// revocation real — nothing was ever held by the agent to take back.
///
/// Errors resolve to `Agent`: a store failure must widen nobody's view.
pub fn viewer_class_for_gate(
    store: &GatewayStore,
    agent_id: &str,
    gate_session_id: &str,
) -> autonoetic_types::disclosure::ViewerClass {
    use autonoetic_types::disclosure::ViewerClass;
    let root = crate::runtime::content_store::root_session_id(gate_session_id);
    match agent_is_appointed_for_scope(store, agent_id, root) {
        Ok(true) => ViewerClass::Decider,
        Ok(false) => ViewerClass::Agent,
        Err(e) => {
            tracing::warn!(
                target: "decider_appointment",
                agent_id = %agent_id,
                gate_session_id = %gate_session_id,
                error = %e,
                "Failed to resolve appointment for disclosure; falling back to agent class"
            );
            ViewerClass::Agent
        }
    }
}

/// Route a freshly opened gate to the seat appointed over its run, if any
/// (#1197).
///
/// Called from `create_approval_row`, the one place every gate is persisted, so
/// a new gate path cannot forget to consider the seat.
///
/// **The fail direction is the design.** This decides only *that* a gate goes
/// to a decider; nothing here resolves a gate. A bug that fails to route leaves
/// it parked for the operator — the status quo. That asymmetry is the argument
/// for routing at gate creation rather than sweeping the pending queue: a
/// sweeper's bug decides something it should not have, and this one's bug does
/// nothing.
///
/// Phase 1 compounds it: `appoint` refuses a non-advisory appointment, so every
/// routing recorded here is `advice_only` and the gate parks regardless of what
/// the seat later says.
pub fn route_gate_to_decider(
    store: &GatewayStore,
    gate_id: &str,
    gate_session_id: &str,
    action: &autonoetic_types::background::ScheduledAction,
) -> Result<Option<DeciderGateRouting>> {
    let kind = if matches!(
        action,
        autonoetic_types::background::ScheduledAction::SessionEscalate { .. }
    ) {
        autonoetic_types::decider_appointment::GATE_KIND_ESCALATION
    } else {
        autonoetic_types::decider_appointment::GATE_KIND_APPROVAL
    };
    let risk = crate::scheduler::approval_hardening::classify_approval_risk(action);
    let root = crate::runtime::content_store::root_session_id(gate_session_id);

    let Some(appointment) = active_appointment_for_gate(store, root, kind, risk)? else {
        return Ok(None);
    };

    let routing = DeciderGateRouting {
        routing_id: format!("rtg_{}", uuid::Uuid::new_v4()),
        gate_id: gate_id.to_string(),
        appointment_id: appointment.appointment_id.clone(),
        decider_agent: appointment.decider_agent.clone(),
        decider_session: appointment.decider_session.clone(),
        gate_kind: kind.to_string(),
        gate_risk: risk.as_str().to_string(),
        advice_only: appointment.advice_only,
        routed_at: chrono::Utc::now().to_rfc3339(),
        verdict: None,
        verdict_reason: None,
        verdict_at: None,
    };

    if !store.insert_decider_gate_routing(&routing)? {
        // Already routed to this seat — a retried gate creation, not a second
        // referral.
        return Ok(None);
    }

    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: routing.decider_agent.clone(),
        session_id: gate_session_id.to_string(),
        turn_id: None,
        event_seq: chrono::Utc::now().timestamp_millis().max(0) as u64,
        timestamp: routing.routed_at.clone(),
        category: "governance.decider".to_string(),
        action: "decider.gate_routed".to_string(),
        status: "routed".to_string(),
        enforced_rules: vec!["P-2.20".to_string()],
        target: Some(gate_id.to_string()),
        payload: Some(
            serde_json::json!({
                "routing_id": routing.routing_id,
                "gate_id": gate_id,
                "appointment_id": routing.appointment_id,
                "decider_agent": routing.decider_agent,
                "decider_session": routing.decider_session,
                "gate_kind": routing.gate_kind,
                "gate_risk": routing.gate_risk,
                "advice_only": routing.advice_only,
            })
            .to_string(),
        ),
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    };
    if let Err(e) = store.create_causal_event(&event) {
        tracing::warn!(
            target: "decider_appointment",
            gate_id = %gate_id,
            error = %e,
            "Failed to record gate routing on the causal chain"
        );
    }
    Ok(Some(routing))
}

fn held_gate_kinds(capabilities: &[Capability]) -> Vec<String> {
    capabilities
        .iter()
        .filter_map(|c| match c {
            Capability::GateDecider { kinds } => Some(kinds.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn emit_appointment_event(
    store: &GatewayStore,
    a: &DeciderAppointment,
    action: &str,
    status: &str,
    reason: Option<&str>,
) {
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        // The appointment is an act *by the operator upon* the appointee, so
        // the event is attributed to the appointee's identity and carries the
        // operator in the payload — the same shape as other power acts.
        agent_id: a.decider_agent.clone(),
        session_id: a.scope_root_session.clone(),
        turn_id: None,
        event_seq: chrono::Utc::now().timestamp_millis().max(0) as u64,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "governance.decider".to_string(),
        action: action.to_string(),
        status: status.to_string(),
        enforced_rules: vec!["P-2.20".to_string()],
        target: Some(a.appointment_id.clone()),
        payload: Some(
            serde_json::json!({
                "appointment_id": a.appointment_id,
                "decider_agent": a.decider_agent,
                "decider_revision": a.decider_revision,
                "decider_provider": a.decider_provider,
                "decider_model": a.decider_model,
                "decider_session": a.decider_session,
                "kinds": a.kinds,
                "scope_root_session": a.scope_root_session,
                "risk_ceiling": a.risk_ceiling.as_str(),
                "advice_only": a.advice_only,
                "expires_at": a.expires_at,
                "max_gates": a.max_gates,
                "standing": a.is_standing(),
                "appointed_by": a.appointed_by,
                "revoked_by": a.revoked_by,
            })
            .to_string(),
        ),
        payload_ref: None,
        evidence_ref: None,
        reason: reason.map(str::to_string),
    };
    if let Err(e) = store.create_causal_event(&event) {
        tracing::warn!(
            target: "decider_appointment",
            appointment_id = %a.appointment_id,
            error = %e,
            "Failed to record decider appointment event on the causal chain"
        );
    }
}
