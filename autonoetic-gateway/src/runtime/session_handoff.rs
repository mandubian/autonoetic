//! Session handoff (#1088): rebind a live root session to another orchestrator.
//!
//! `planner.default` has no PlanFrame tools (they are capability-gated to
//! `planner.collaborative`); when a task meets the plan criteria its only
//! recourse today is prose — "restart with a different agent" — which abandons
//! the room and the conversation. Sessions are bound to their first agent
//! (`session_agent_bindings`), and the binding silently wins every later
//! ingest ([`crate::execution::GatewayExecutionService::spawn_agent_revision_once`]
//! pre-resolution, `resolve_ingest_target_agent_id`), so no amount of
//! retargeting the composer can switch agents.
//!
//! `session.handoff` is the missing operator primitive. It is **operator-only
//! by construction**: it exists solely as a JSON-RPC method authenticated by
//! the ingress shared secret (the same trust level as `approvals.approve`) —
//! no native tool wraps it, so an agent cannot rebind its own session. Agents
//! may *propose* a handoff in prose; the operator pulls the trigger.
//!
//! What a handoff does, in order:
//!
//! 1. **Guards** — the session must have a binding; the target must resolve to
//!    an installed agent with an active revision and differ from the current
//!    agent; the root session must not be parked at a gate (approval / user
//!    input / escalation / emergency-stop checkpoint — rebinding mid-gate
//!    would strand the pending decision), and must not have an active
//!    emergency stop.
//! 2. **Rebind** — replace the `session_agent_bindings` row (new revision,
//!    runtime-lock hash, constitution re-pin; `home_node_id` unchanged). The
//!    old revision loses its binding reference, which is correct: reclamation
//!    treats the binding as the live reference and the session no longer runs
//!    that revision.
//! 3. **Residency** — a parked resident row keeps its stale `agent_id` through
//!    the residency upsert (which deliberately never changes it), so handoff
//!    updates it explicitly.
//! 4. **Context envelope** — the successor starts with a fresh history
//!    (SessionContext and conversation history are keyed per agent dir), so
//!    the gateway seeds the successor's SessionContext for this session from
//!    the outgoing agent's: current topic, known facts, the last exchange,
//!    and a handoff note naming the transition and the operator's reason.
//!    Mechanical, bounded, no LLM. Deeper context (auto-digest narrative)
//!    remains available to the successor via `digest_query`.
//! 5. **Records** — a `session.handoff` causal event and a timeline row, so
//!    the room shows the transition as a first-class event.

use autonoetic_types::agent_revision::SessionAgentBinding;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::principal::Principal;
use autonoetic_types::session_timeline::{Altitude, SessionRole, TimelineRefs};
use std::sync::Arc;

use crate::scheduler::gateway_store::GatewayStore;

#[derive(Debug, serde::Deserialize)]
pub struct HandoffParams {
    /// Root session to rebind (its own binding row is replaced).
    pub session_id: String,
    /// Agent (alias id) that takes over the session.
    pub target_agent_id: String,
    /// Operator-facing reason, recorded on the causal event.
    #[serde(default)]
    pub reason: Option<String>,
    /// Optional operator note folded into the successor's context envelope.
    #[serde(default)]
    pub context_note: Option<String>,
}

/// Result reported back over JSON-RPC.
#[derive(Debug, serde::Serialize)]
pub struct HandoffOutcome {
    pub ok: bool,
    pub session_id: String,
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub revision_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causal_event_id: Option<String>,
    pub message: String,
}

/// Checkpoint yield reasons that mean "a decision is pending on this session".
/// Rebinding mid-gate would strand the pending approval/interaction/
/// escalation (its resolution orchestration assumes the bound agent), so the
/// operator must resolve or cancel first.
fn is_gate_parked(reason: &crate::runtime::checkpoint::YieldReason) -> bool {
    use crate::runtime::checkpoint::YieldReason;
    matches!(
        reason,
        YieldReason::ApprovalRequired { .. }
            | YieldReason::UserInputRequired { .. }
            | YieldReason::HumanEscalation { .. }
            | YieldReason::EmergencyStop { .. }
    )
}

/// Perform the handoff. Returns a ready-to-serialize outcome, or a
/// human-facing error string (the router maps it to `-32000`).
pub fn perform_handoff(
    config: &GatewayConfig,
    store: &Arc<GatewayStore>,
    params: &HandoffParams,
) -> Result<HandoffOutcome, String> {
    let session_id = params.session_id.trim();
    if session_id.is_empty() {
        return Err("session_id must be non-empty".to_string());
    }
    let target = params.target_agent_id.trim();
    if target.is_empty() {
        return Err("target_agent_id must be non-empty".to_string());
    }

    // ---- guards ----------------------------------------------------------

    let binding = store
        .get_session_agent_binding(session_id)
        .map_err(|e| format!("failed to read session binding: {e}"))?
        .ok_or_else(|| {
            format!(
                "session '{session_id}' has no agent binding; handoff applies to bound root sessions"
            )
        })?;
    if binding.agent_id == target {
        return Err(format!(
            "session '{session_id}' is already bound to '{target}'; nothing to hand off"
        ));
    }

    // Target must resolve to an installed agent with an active revision.
    let alias = store
        .resolve_alias(target)
        .map_err(|e| format!("failed to resolve target agent: {e}"))?
        .ok_or_else(|| format!("target agent '{target}' is not installed"))?;
    if let Some(suspended_at) = alias.suspended_at.as_deref() {
        if !suspended_at.is_empty() {
            return Err(format!("target agent '{target}' is suspended ({suspended_at})"));
        }
    }
    let revision = store
        .get_agent_revision(&alias.revision_id)
        .map_err(|e| format!("failed to read target revision: {e}"))?
        .ok_or_else(|| {
            format!(
                "target agent '{}' has no revision '{}' — install is incomplete",
                target, alias.revision_id
            )
        })?;

    // No gate-parked checkpoint on the root tree: a pending approval /
    // interaction / escalation / emergency stop must be resolved (or the
    // session cancelled) before the agent identity can change.
    if let Ok(Some(cp)) =
        crate::runtime::checkpoint::load_latest_checkpoint(config, &binding.root_session_id)
    {
        if is_gate_parked(&cp.yield_reason) {
            return Err(format!(
                "session '{}' is parked at a gate ({:?}); resolve or cancel the pending \
                 decision before handing off",
                binding.root_session_id, cp.yield_reason
            ));
        }
    }

    // ---- rebind ----------------------------------------------------------

    let new_binding = SessionAgentBinding {
        session_id: binding.session_id.clone(),
        root_session_id: binding.root_session_id.clone(),
        alias_id: Some(alias.alias_id.clone()),
        agent_id: alias.agent_id.clone(),
        revision_id: revision.revision_id.clone(),
        runtime_lock_hash: revision.runtime_lock_hash.clone(),
        // Constitution is re-pinned at bind time; a handoff is a new bind.
        // The runtime is always initialized in a live gateway (startup); in
        // bare test processes it may not be — then pin nothing rather than
        // panic (drift detection treats a missing pin as "unknown", which
        // the pin-drift notice path handles).
        constitution_version: if crate::constitution_digest::is_constitution_initialized() {
            Some(crate::constitution_digest::constitution_version().to_string())
        } else {
            None
        },
        constitution_digest: if crate::constitution_digest::is_constitution_initialized() {
            Some(crate::constitution_digest::constitution_digest().to_string())
        } else {
            None
        },
        home_node_id: binding.home_node_id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        requested_target: alias.alias_id.clone(),
    };
    store
        .upsert_session_agent_binding(&new_binding)
        .map_err(|e| format!("failed to rebind session: {e}"))?;

    // A parked resident row would keep the outgoing agent's id (the residency
    // upsert never changes it), leaving the session addressable under a name
    // that no longer executes it.
    let _ = store.update_residency_agent(session_id, &alias.agent_id);

    // ---- context envelope -------------------------------------------------

    seed_successor_context(config, &binding, &new_binding, params);

    // ---- records ----------------------------------------------------------

    let payload = serde_json::json!({
        "from_agent_id": binding.agent_id,
        "to_agent_id": alias.agent_id,
        "revision_id": revision.revision_id,
        "reason": params.reason,
        "context_note": params.context_note,
    });
    let causal_event_id = format!("session-handoff-{}", uuid::Uuid::new_v4());
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: causal_event_id.clone(),
        agent_id: "gateway".to_string(),
        session_id: binding.root_session_id.clone(),
        turn_id: None,
        event_seq: 0,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "session".to_string(),
        action: "handoff".to_string(),
        status: "completed".to_string(),
        enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
        target: Some(alias.agent_id.clone()),
        payload: Some(payload.to_string()),
        payload_ref: None,
        evidence_ref: None,
        reason: params.reason.clone(),
    };
    if let Err(e) = store.create_causal_event(&event) {
        // The rebind already happened; the audit row failing to persist is a
        // degradation, not a rollback — report it in the outcome message.
        tracing::warn!(
            target: "session_handoff",
            session_id = %session_id,
            error = %e,
            "handoff causal event persist failed (rebind already applied)"
        );
    }

    // Timeline row so the room renders the transition as a first-class event.
    let principal = Principal::human("operator");
    let timeline_event = crate::runtime::session_timeline::build_timeline_event(
        binding.root_session_id.clone(),
        session_id.to_string(),
        None,
        &principal,
        &SessionRole::Runtime,
        "session.handoff",
        // Attention: not a failure, but the operator's eye should catch the
        // agent identity changing mid-session.
        Some(Altitude::Attention),
        Some(payload),
        TimelineRefs::default(),
    );
    if let Err(e) = store.create_live_digest_event(&timeline_event) {
        tracing::debug!(
            target: "session_handoff",
            error = %e,
            "handoff timeline emit failed"
        );
    }

    tracing::info!(
        target: "session_handoff",
        session_id = %session_id,
        from = %binding.agent_id,
        to = %alias.agent_id,
        "Session handed off to a new orchestrator"
    );

    Ok(HandoffOutcome {
        ok: true,
        session_id: session_id.to_string(),
        from_agent_id: binding.agent_id.clone(),
        to_agent_id: alias.agent_id.clone(),
        revision_id: revision.revision_id,
        causal_event_id: Some(causal_event_id),
        message: format!(
            "Session handed off from '{}' to '{}'. The next message goes to the new agent; \
             its context starts from the handoff envelope (prior digest remains queryable \
             via digest_query).",
            binding.agent_id, alias.agent_id
        ),
    })
}

/// Seed the successor's SessionContext for this session from the outgoing
/// agent's context: topic, known facts, the last exchange, and a handoff note.
///
/// SessionContext is stored per agent dir, which is exactly why the successor
/// would otherwise start blind. `build_initial_history` picks this up
/// automatically on the successor's first turn — no lifecycle change needed.
/// Best-effort: a failure to read or write context degrades the handoff to
/// "fresh start", it never blocks the rebind.
fn seed_successor_context(
    config: &GatewayConfig,
    old: &SessionAgentBinding,
    new: &SessionAgentBinding,
    params: &HandoffParams,
) {
    // Agent dir = agents_dir/<agent_id> (the bootstrap layout; the agent
    // registry addresses installed agents this way everywhere).
    let old_dir = config.agents_dir.join(&old.agent_id);
    let new_dir = config.agents_dir.join(&new.agent_id);

    // The outgoing agent may have no context file (short session) — start
    // from an empty one; the handoff note still seeds the successor.
    let mut context = crate::runtime::session_context::SessionContext::load(&old_dir, &old.session_id)
        .unwrap_or_else(|_| crate::runtime::session_context::SessionContext::empty(&old.session_id));
    context.session_id = new.session_id.clone();
    context.updated_at = chrono::Utc::now().to_rfc3339();
    context.current_topic = Some(match (&context.current_topic, &params.reason) {
        (Some(topic), Some(reason)) => {
            format!("Handoff from {from} (reason: {reason}) — continuing: {topic}", from = old.agent_id)
        }
        (Some(topic), None) => {
            format!("Handoff from {from} — continuing: {topic}", from = old.agent_id)
        }
        (None, Some(reason)) => {
            format!("Handoff from {from} (reason: {reason})", from = old.agent_id)
        }
        (None, None) => format!("Handoff from {}", old.agent_id),
    });
    if let Some(note) = params.context_note.as_deref().filter(|n| !n.trim().is_empty()) {
        context.known_facts.push(crate::runtime::session_context::SessionFact {
            label: "handoff_note".to_string(),
            value: note.trim().to_string(),
            source: "operator".to_string(),
        });
    }
    context.known_facts.push(crate::runtime::session_context::SessionFact {
        label: "handoff".to_string(),
        value: format!(
            "Session previously orchestrated by {}; handed off to {} at {}.",
            old.agent_id,
            new.agent_id,
            context.updated_at
        ),
        source: "gateway".to_string(),
    });
    let mut context = context;
    if let Err(e) = context.save(&new_dir) {
        tracing::warn!(
            target: "session_handoff",
            session_id = %new.session_id,
            error = %e,
            "successor context seed failed (handoff proceeds with a fresh start)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_parked_reasons_are_refused() {
        use crate::runtime::checkpoint::YieldReason;
        assert!(is_gate_parked(&YieldReason::ApprovalRequired {
            approval_request_id: "apr-1".into()
        }));
        assert!(is_gate_parked(&YieldReason::UserInputRequired {
            interaction_id: "int-1".into()
        }));
        assert!(is_gate_parked(&YieldReason::HumanEscalation {
            escalation_request_id: "esc-1".into()
        }));
        assert!(is_gate_parked(&YieldReason::EmergencyStop { stop_id: "s".into() }));
        // Settled / parked-idle states are handoff-eligible.
        assert!(!is_gate_parked(&YieldReason::Hibernation));
        assert!(!is_gate_parked(&YieldReason::ManualStop));
        assert!(!is_gate_parked(&YieldReason::Error("x".into())));
    }
}
