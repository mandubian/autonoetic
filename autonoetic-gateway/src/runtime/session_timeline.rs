//! Session Room timeline classification (#363 P1): map a digest event's type +
//! seat to an [`Altitude`], and derive the [`SessionRole`] seat from an agent id.
//!
//! Importance is gateway-owned: `altitude = max(base(event_type),
//! role_floor(role))`. Roles may only *raise* the floor, never suppress, so a
//! critical seat (Sentinel) always surfaces. `role_floor` defaults live here and
//! are config-tunable (don't-pin-tunables); base mapping is deterministic.

use autonoetic_types::principal::Principal;
use autonoetic_types::session_timeline::{Altitude, SessionRole, TimelineRefs};

use crate::scheduler::gateway_store::LiveDigestEventRecord;

/// Build a canonical-timeline (`live_digest_events`) record with attribution.
/// Centralizes the event_id / node_id / kind-storage boilerplate so every
/// producer (digest tracer, gates, divergence) emits a consistent shape.
/// `altitude = None` derives it from `(event_type, role)`.
#[allow(clippy::too_many_arguments)]
pub fn build_timeline_event(
    root_session_id: String,
    session_id: String,
    turn_id: Option<String>,
    principal: &Principal,
    role: &SessionRole,
    event_type: &str,
    altitude: Option<Altitude>,
    payload: Option<serde_json::Value>,
    refs: TimelineRefs,
) -> LiveDigestEventRecord {
    let altitude = altitude.unwrap_or_else(|| altitude_for(event_type, role));
    // NOTE: an explicit `altitude` arg is used AS-IS and can undercut the
    // seat floor (`role_floor`). This is intentional for a few plumbing
    // emitters (e.g. `workflow.signal` pins to Detail regardless of seat),
    // but most callers should pass `None` and let `altitude_for` apply the
    // floor. For a raise-only override, use the session_tracer's
    // `append_live_digest_event_at(.., Some(alt))`, which takes `max`.
    LiveDigestEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        root_session_id,
        source_session_id: session_id,
        turn_id,
        source_agent_id: Some(principal.id.clone()),
        source_node_id: std::env::var("AUTONOETIC_NODE_ID").unwrap_or_else(|_| "gateway".to_string()),
        event_type: event_type.to_string(),
        payload: payload.and_then(|v| serde_json::to_string(&v).ok()),
        created_at: chrono::Utc::now().to_rfc3339(),
        principal_kind: Some(principal.kind_to_storage()),
        principal_id: Some(principal.id.clone()),
        role: Some(role.to_storage()),
        altitude: Some(altitude.as_str().to_string()),
        refs_json: if refs.is_empty() {
            None
        } else {
            serde_json::to_string(&refs).ok()
        },
    }
}

/// Optional structured fields for an `approval.pending` timeline payload, derived
/// from the persisted `ScheduledAction`.
pub fn approval_timeline_extra_from_action(
    action: &autonoetic_types::background::ScheduledAction,
) -> Option<serde_json::Value> {
    use autonoetic_types::background::ScheduledAction;
    match action {
        ScheduledAction::WikiProposal {
            page_id,
            title,
            content_sha256,
            tags,
            ..
        } => Some(serde_json::json!({
            "page_id": page_id,
            "title": title,
            "content_sha256": content_sha256,
            "tags": tags,
        })),
        ScheduledAction::SandboxExec {
            command,
            detected_hosts,
            ..
        } => Some(serde_json::json!({
            "command": command,
            "host_patterns": detected_hosts,
        })),
        ScheduledAction::SessionEscalate {
            session_id,
            root_session_id,
            requested_by_agent_id,
            reason,
            context,
            urgency,
            suggested_actions,
            ..
        } => Some(serde_json::json!({
            "reason": reason,
            "urgency": urgency,
            "session_id": session_id,
            "root_session_id": root_session_id,
            "requested_by_agent_id": requested_by_agent_id,
            "context": context,
            "suggested_actions": suggested_actions,
        })),
        ScheduledAction::RevisionPromote {
            agent_id,
            revision_id,
            added_capabilities,
            broadened_capabilities,
            ..
        } => Some(serde_json::json!({
            "agent_id": agent_id,
            "revision_id": revision_id,
            "added_capabilities": added_capabilities,
            "broadened_capabilities": broadened_capabilities,
        })),
        ScheduledAction::ProfileShare {
            user_id,
            scope,
            ..
        } => Some(serde_json::json!({
            "user_id": user_id,
            "scope": scope,
        })),
        ScheduledAction::SessionContinue {
            max_turns,
            turn_counter,
            ..
        } => Some(serde_json::json!({
            "max_turns": max_turns,
            "turn_counter": turn_counter,
        })),
        _ => None,
    }
}

/// Emit `approval.pending` on the canonical timeline after persisting an approval row.
///
/// The Room TUI resolves gates from timeline events, not from the `approvals`
/// table alone (#363). `GatewayStore::create_approval` invokes this automatically.
pub fn emit_approval_pending_timeline_event(
    store: &crate::scheduler::gateway_store::GatewayStore,
    approval: &autonoetic_types::background::ApprovalRequest,
    turn_id: Option<&str>,
) {
    let action_label = approval.action.kind();
    let extra_payload = approval_timeline_extra_from_action(&approval.action);
    let root = approval
        .root_session_id
        .clone()
        .unwrap_or_else(|| approval.session_id.clone());
    let role = derive_role(&approval.agent_id);
    let principal = autonoetic_types::principal::Principal::agent(approval.agent_id.clone());
    let refs = TimelineRefs {
        approval_request_id: Some(approval.request_id.clone()),
        ..Default::default()
    };
    let mut payload = serde_json::json!({
        "request_id": approval.request_id,
        "approval_level": approval.approval_level.to_config(),
        "action": action_label,
    });
    if let Some(extra) = extra_payload {
        if let (Some(obj), Some(extra_obj)) = (payload.as_object_mut(), extra.as_object()) {
            for (k, v) in extra_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    if let Some(reason) = approval.reason.as_ref().filter(|r| !r.is_empty()) {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(
                "summary".into(),
                serde_json::Value::String(crate::log_redaction::redact_text_for_logs(reason)),
            );
        }
    }
    if let Some(phrase) = approval.confirm_phrase.as_ref().filter(|p| !p.is_empty()) {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("confirm_phrase".into(), serde_json::Value::String(phrase.clone()));
        }
    }
    if let Some(root) = approval.root_session_id.as_deref() {
        if let Some(targets) = approval.action.detected_hosts() {
            if let Some(hint) =
                crate::runtime::session_envelope::envelope_expansion_hint(store, root, &targets)
            {
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("envelope_expansion_hint".into(), hint);
                }
            }
        }
    }
    let event = build_timeline_event(
        root,
        approval.session_id.clone(),
        turn_id.map(str::to_string),
        &principal,
        &role,
        "approval.pending",
        None,
        Some(payload),
        refs,
    );
    if let Err(e) = store.create_live_digest_event(&event) {
        tracing::debug!(
            target: "session_timeline",
            error = %e,
            request_id = %approval.request_id,
            "approval.pending timeline emit failed"
        );
    }
}

/// Emit `user.ask.pending` on the canonical timeline after persisting an interaction.
///
/// `GatewayStore::create_user_interaction` invokes this automatically so every
/// clarification path is visible in the Room TUI (#363).
pub fn emit_user_ask_pending_timeline_event(
    store: &crate::scheduler::gateway_store::GatewayStore,
    interaction: &autonoetic_types::background::UserInteraction,
) {
    let root = if interaction.root_session_id.is_empty() {
        crate::runtime::content_store::root_session_id(&interaction.session_id).to_string()
    } else {
        interaction.root_session_id.clone()
    };
    let role = derive_role(&interaction.agent_id);
    let principal = autonoetic_types::principal::Principal::agent(interaction.agent_id.clone());
    let refs = TimelineRefs {
        interaction_id: Some(interaction.interaction_id.clone()),
        ..Default::default()
    };
    let options_for_event: Vec<serde_json::Value> = interaction
        .options
        .iter()
        .map(|o| {
            serde_json::json!({
                "id": o.id,
                "label": o.label,
            })
        })
        .collect();
    let mut payload = serde_json::json!({
        "interaction_id": interaction.interaction_id,
        "question": crate::log_redaction::redact_text_for_logs(&interaction.question),
        "kind": interaction.kind.as_str(),
        "options_count": interaction.options.len(),
        "options": options_for_event,
        "allow_freeform": interaction.allow_freeform,
    });
    if let Some(ctx) = interaction.context.as_deref().filter(|c| !c.trim().is_empty()) {
        payload["context"] = serde_json::json!(crate::log_redaction::redact_text_for_logs(ctx));
    }
    let event = build_timeline_event(
        root,
        interaction.session_id.clone(),
        Some(interaction.turn_id.clone()).filter(|t| !t.is_empty() && t != "unknown"),
        &principal,
        &role,
        "user.ask.pending",
        None,
        Some(payload),
        refs,
    );
    if let Err(e) = store.create_live_digest_event(&event) {
        tracing::debug!(
            target: "session_timeline",
            error = %e,
            interaction_id = %interaction.interaction_id,
            "user.ask.pending timeline emit failed"
        );
    }
}

/// Build the `operator.message` timeline event for an operator-originated chat
/// message into a session (#405) — so channels show both sides of the
/// conversation, not just agent replies. Attribution: a human chat (no
/// `source_agent_id`) is the **Operator seat / Human** principal; an
/// agent-originated ingest is attributed to that agent's seat. Altitude is
/// derived via `altitude_for` (`None` below): `operator.message` has a `Normal`
/// base (first-class conversation, visible at the default floor) but the seat's
/// `role_floor` can only *raise* it — e.g. a Sentinel-seat message surfaces at
/// `Attention`, honoring the module's raise-only invariant. The caller redacts
/// the message text before passing it in.
/// Whether an `event.ingest` chat payload is an automated workflow/gateway
/// signal rather than a human or agent-authored chat line.
pub fn is_signal_delivered_chat(metadata: Option<&serde_json::Value>) -> bool {
    metadata
        .and_then(|m| m.get("signal_delivered"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Map an `event.ingest` chat line onto the canonical timeline.
///
/// Human/room/chat lines become `operator.message` (Normal). Automated workflow
/// signals (`child_state_notification`, `workflow_join_satisfied`, …) become
/// gateway-owned `workflow.*` rows at Detail so they don't drown out real
/// operator conversation in the room.
pub fn ingest_chat_timeline_event(
    session_id: &str,
    source_agent_id: Option<&str>,
    message: &str,
    metadata: Option<&serde_json::Value>,
) -> Option<LiveDigestEventRecord> {
    if is_signal_delivered_chat(metadata) {
        return workflow_signal_timeline_event(session_id, message);
    }
    Some(operator_message_event(
        session_id,
        source_agent_id,
        message,
    ))
}

fn workflow_signal_timeline_event(
    session_id: &str,
    message: &str,
) -> Option<LiveDigestEventRecord> {
    let parsed = serde_json::from_str::<serde_json::Value>(message).ok()?;
    let signal_type = parsed.get("type").and_then(|v| v.as_str())?;
    let (principal, role) = actor_from_kind_id("system", "gateway");
    let root = crate::runtime::content_store::root_session_id(session_id).to_string();

    let (event_type, payload) = match signal_type {
        "child_state_notification" => {
            let notification = parsed.get("notification")?;
            let child_status = notification
                .get("child_status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let summary = notification
                .get("summary")
                .and_then(|v| v.as_str())
                .map(crate::log_redaction::redact_text_for_logs)
                .unwrap_or_default();
            (
                "workflow.child_state",
                serde_json::json!({
                    "signal_type": signal_type,
                    "message": parsed.get("message").and_then(|v| v.as_str()),
                    "child_status": child_status,
                    "child_session_id": notification.get("child_session_id"),
                    "task_id": notification.get("task_id"),
                    "workflow_id": notification.get("workflow_id"),
                    "failure_class": notification.get("failure_class"),
                    "summary": summary,
                }),
            )
        }
        "workflow_join_satisfied" => (
            "workflow.join_satisfied",
            serde_json::json!({
                "signal_type": signal_type,
                "message": parsed.get("message").and_then(|v| v.as_str()),
                "workflow_id": parsed.get("workflow_id"),
                "join_task_ids": parsed.get("join_task_ids"),
            }),
        ),
        other => (
            "workflow.signal",
            serde_json::json!({
                "signal_type": other,
                "message": parsed.get("message").and_then(|v| v.as_str()),
            }),
        ),
    };

    Some(build_timeline_event(
        root,
        session_id.to_string(),
        None,
        &principal,
        &role,
        event_type,
        Some(Altitude::Detail),
        Some(payload),
        TimelineRefs::default(),
    ))
}

pub fn operator_message_event(
    session_id: &str,
    source_agent_id: Option<&str>,
    redacted_message: &str,
) -> LiveDigestEventRecord {
    let (principal, role) = match source_agent_id {
        None | Some("") => (Principal::human("operator"), SessionRole::Operator),
        Some(agent_id) => (Principal::agent(agent_id), derive_role(agent_id)),
    };
    let root = crate::runtime::content_store::root_session_id(session_id);
    build_timeline_event(
        root.to_string(),
        session_id.to_string(),
        None,
        &principal,
        &role,
        "operator.message",
        None, // derive: max(base=Normal, role_floor(role))
        Some(serde_json::json!({ "message": redacted_message })),
        TimelineRefs::default(),
    )
}

/// Base importance of a digest event type, before any role refinement.
/// Base importance for a timeline event type. The effective altitude written
/// to a row is normally `max(base_altitude(et), role_floor(role))` (see
/// [`altitude_for`]).
///
/// # Explicit altitude — two contracts
///
/// Emitters can pass an explicit altitude, but the contract depends on the
/// helper used — they are NOT the same:
///
/// - **`build_timeline_event(.., Some(alt))`** uses the explicit value AS-IS
///   (it REPLACES `altitude_for`, and can undercut `role_floor`). This is
///   relied on by a few plumbing emitters that pin to `Detail` regardless of
///   seat (e.g. `workflow.signal`). Callers that should not undercut the
///   floor must pass `None`.
/// - **`session_tracer::append_live_digest_event_at(.., Some(alt))`** is
///   **raise-only**: it takes `max(altitude_for(et, role), alt)`, so a caller
///   can never undercut the floor. Used by `tool.completed` to bump failures
///   (`ok:false`) from `Detail` up to `Attention`.
///
/// Most emitters pass `None` and let `base_altitude` + the seat floor decide.
///
/// # Altitude policy
///
/// The four altitudes (`Detail < Normal < Attention < Error`) partition events
/// by what the operator needs to see:
///
/// - **Error** — failures and integrity breaches. Always visible; the operator
///   must know these happened.
/// - **Attention** — operator **gates**: the full lifecycle of a decision that
///   suspends for, or records, operator input. Both the *request* (`*.pending`,
///   `*.proposed`) and the *decision* (`*.approved`, `*.rejected`, `*.promoted`)
///   share this altitude so each gate reads as a paired ask↔resolution.
///   Abandonments (`*.cancelled`, `*.withdrawn`) are NOT decisions → Normal.
/// - **Normal** — visible progress: agent/operator narrative, session
///   boundaries, workbench creation (a milestone), audits, retries, and gate
///   abandonments. The default floor.
/// - **Detail** — hidable plumbing: turns, LLM rounds, reasoning, tool
///   requests *and* successful tool completions, workflow bookkeeping,
///   scheduled jobs, workbench reconcile/discard.
///
/// # Per-event rationale
///
/// | Event type | Altitude | Why |
/// |---|---|---|
/// | `turn.start` / `turn.end` | Detail | turn plumbing |
/// | `llm.round` | Detail | per-round token accounting |
/// | `agent.reasoning` | Detail | verbose "why"; surfaced on dial-down |
/// | `tool.requested` | Detail | the request is plumbing; the agent's message carries intent |
/// | `tool.completed` | Detail | success completion pairs with the request; **failures** (`ok:false`) are bumped to Attention at the emit site so they aren't hidden |
/// | `workbench.created` | Normal | milestone — content becomes reviewable (NOT plumbing) |
/// | `workbench.reconciled` / `workbench.discarded` | Detail | edit/discard mechanics around the real work |
/// | `workflow.child_state` / `workflow.join_satisfied` / `workflow.signal` | Detail | workflow bookkeeping |
/// | `scheduled_job.*` | Detail | cron plumbing |
/// | `agent.message` | Normal | primary agent narrative — the thing to read |
/// | `operator.message` | Normal | operator input — primary |
/// | `session.start` / `session.end` | Normal | session boundaries a new agent joins/leaves |
/// | `digest_annotate` | Normal | auditor/evaluator annotation — output worth seeing |
/// | `llm.retry` | Normal | a retry is notable (transient trouble) but not a failure |
/// | `plan.pending` / `plan.approved` | Attention | plan gate lifecycle (request + decision) |
/// | `approval.pending` / `approval.approved` / `approval.rejected` | Attention | approval gate lifecycle |
/// | `approval.cancelled` | Normal | abandonment, not a decision |
/// | `escalation.pending` | Attention | escalation gate request |
/// | `user.ask.pending` | Attention | conversational clarification gate |
/// | `wiki.proposed` / `wiki.promoted` / `wiki.rejected` | Attention | wiki-contribution gate lifecycle |
/// | `wiki.withdrawn` | Normal | abandonment, not a decision |
/// | `divergence.intervention` | Attention | sentinel intervention (emit site raises to Error when critical) |
/// | `runtime.lock_drift` | Attention | integrity event (emit site stores Error when rejected) |
/// | `security.escape_threshold` | Attention | escape-probability threshold reached |
/// | `llm.request_failed` / `llm.empty_response` | Error | LLM failures |
/// | `guard.tripped` | Error | LoopGuard circuit breaker |
/// | `session.emergency_stop` | Error | emergency stop fired |
/// | `security.sandbox_escape` | Error | sandbox breach |
/// | `tool.failed` | Error | reserved — no emitter today (failures use `tool.completed` with `ok:false`, bumped to Attention at the emit site). Kept so a future dedicated failure event lands at Error. |
///
/// This match is **exhaustive over every event type the gateway emits**; the
/// `_ => Normal` arm is a safe fallback for forward-compat (a new event type
/// defaults to visible progress until consciously classified here).
pub fn base_altitude(event_type: &str) -> Altitude {
    match event_type {
        // ─── Error: failures and integrity breaches. ───
        "llm.request_failed" | "llm.empty_response" | "guard.tripped"
        | "session.emergency_stop" | "security.sandbox_escape"
        | "tool.failed" => Altitude::Error,

        // ─── Attention: operator gates — requests AND decisions. ───
        // The full lifecycle of a gate shares one altitude so each ask reads
        // paired with its resolution. Abandonments (cancelled/withdrawn) are
        // NOT decisions → Normal below.
        "plan.pending" | "plan.approved"
        | "envelope.proposed" | "envelope.locked"
        | "approval.pending" | "approval.approved" | "approval.rejected"
        | "escalation.pending"
        | "user.ask.pending"
        | "wiki.proposed" | "wiki.promoted" | "wiki.rejected"
        | "divergence.intervention"
        | "runtime.lock_drift"
        | "security.escape_threshold" => Altitude::Attention,

        // ─── Normal: visible progress (the default floor). ───
        // Agent/operator narrative, session boundaries, the workbench-CREATED
        // milestone, audits, retries, and gate abandonments.
        "agent.message" | "operator.message"
        | "session.start" | "session.end"
        | "workbench.created"
        | "digest_annotate"
        | "llm.retry"
        | "approval.cancelled" | "wiki.withdrawn" => Altitude::Normal,

        // ─── Detail: hidable plumbing. ───
        // Turns, LLM rounds, reasoning, tool requests AND successful tool
        // completions (failures are bumped to Attention at the emit site),
        // workflow bookkeeping, scheduled jobs, workbench reconcile/discard.
        "turn.start" | "turn.end" | "llm.round" | "agent.reasoning"
        | "tool.requested" | "tool.completed"
        | "workbench.reconciled" | "workbench.discarded"
        | "workflow.child_state" | "workflow.join_satisfied" | "workflow.signal"
        | "scheduled_job.triggered" | "scheduled_job.completed" | "scheduled_job.failed" => {
            Altitude::Detail
        }

        // Unknown event type: Normal is the safe default (visible). New event
        // types surface until consciously classified above.
        _ => Altitude::Normal,
    }
}

/// Minimum altitude a seat guarantees for its events. Only raises (`max`),
/// never lowers. Configurable via `session_room.role_floors` in the gateway
/// config (see `role_floor_with_config`); unconfigured roles keep their
/// hardcoded defaults.
pub fn role_floor(role: &SessionRole) -> Altitude {
    role_floor_with_config(role, None)
}

pub fn role_floor_with_config(role: &SessionRole, config_floors: Option<&std::collections::HashMap<String, String>>) -> Altitude {
    if let Some(floors) = config_floors {
        let key = match role {
            SessionRole::Operator => "operator",
            SessionRole::Planner => "planner",
            SessionRole::Specialist { .. } => "specialist",
            SessionRole::Sentinel => "sentinel",
            SessionRole::Curator => "curator",
            SessionRole::Auditor => "auditor",
            SessionRole::Tool { .. } => "tool",
            SessionRole::ExternalSurface { .. } => "external_surface",
            SessionRole::Runtime => "runtime",
        };
        if let Some(alt_str) = floors.get(key) {
            if let Some(alt) = Altitude::parse_str(alt_str) {
                return alt;
            }
        }
    }
    match role {
        SessionRole::Sentinel => Altitude::Attention,
        SessionRole::Runtime => Altitude::Detail,
        _ => Altitude::Detail,
    }
}

/// `max(base, role_floor)` — the effective altitude written to the row.
pub fn altitude_for(event_type: &str, role: &SessionRole) -> Altitude {
    base_altitude(event_type).max(role_floor(role))
}

pub fn altitude_for_with_config(event_type: &str, role: &SessionRole, config_floors: Option<&std::collections::HashMap<String, String>>) -> Altitude {
    base_altitude(event_type).max(role_floor_with_config(role, config_floors))
}

/// Derive the session seat from an agent id (e.g. `planner.default`,
/// `sentinel.divergence`, `coder.default`). Heuristic until seats are explicit.
pub fn derive_role(agent_id: &str) -> SessionRole {
    let head = agent_id.split('.').next().unwrap_or(agent_id);
    match head {
        "planner" => SessionRole::Planner,
        "sentinel" => SessionRole::Sentinel,
        "curator" => SessionRole::Curator,
        "auditor" => SessionRole::Auditor,
        "runtime" | "gateway" => SessionRole::Runtime,
        other => SessionRole::Specialist { kind: other.to_string() },
    }
}

/// Map an explicit `(kind, id)` pair to a `(principal, seat)` — for producers
/// that already know who acted (emergency stop, escalations) rather than parsing
/// a `decided_by` string. `user`/`operator`/`human` ⇒ Operator seat (the
/// emergency-stop API/CLI uses `"user"`); `agent` ⇒ that agent's seat; anything
/// else (`system`, `security_policy`, …) ⇒ Runtime.
pub fn actor_from_kind_id(kind: &str, id: &str) -> (autonoetic_types::principal::Principal, SessionRole) {
    use autonoetic_types::principal::{Principal, PrincipalKind};
    match kind {
        "user" | "operator" | "human" => (Principal::human(id), SessionRole::Operator),
        "agent" | "autonoetic_agent" => (Principal::agent(id), derive_role(id)),
        _ => (
            Principal { kind: PrincipalKind::Script, id: id.to_string() },
            SessionRole::Runtime,
        ),
    }
}

/// Attribute a gate decision to a `(principal, seat)` from its recorded
/// `decided_by` string. Operator (human) ⇒ Operator seat; an agent decider
/// (`auditor.default` or `agent:auditor.default`) ⇒ that agent's seat (the
/// `agent:` prefix is stripped first); a mechanical/unknown decider
/// (`gateway`, `emergency_stop:<id>`, …) ⇒ the hidable Runtime seat.
pub fn decider_seat(decided_by: &str) -> (autonoetic_types::principal::Principal, SessionRole) {
    use autonoetic_types::principal::{Principal, PrincipalKind};
    match autonoetic_types::principal::decider_principal_kind(decided_by) {
        Some(PrincipalKind::Human) => (Principal::human(decided_by), SessionRole::Operator),
        Some(PrincipalKind::AutonoeticAgent) => {
            let id = decided_by.strip_prefix("agent:").unwrap_or(decided_by);
            (Principal::agent(id), derive_role(id))
        }
        _ => (
            Principal { kind: PrincipalKind::Script, id: decided_by.to_string() },
            SessionRole::Runtime,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_chat_signal_emits_workflow_child_state_at_detail() {
        let msg = serde_json::json!({
            "type": "child_state_notification",
            "message": "Workflow child failed",
            "notification": {
                "child_session_id": "sched-child-1",
                "child_status": "failed",
                "summary": "Script execution failed",
                "task_id": "task-abc",
                "workflow_id": "sched-sj-1"
            }
        });
        let meta = serde_json::json!({ "signal_delivered": true });
        let event = ingest_chat_timeline_event(
            "session-1",
            None,
            &msg.to_string(),
            Some(&meta),
        )
        .expect("signal should emit timeline row");
        assert_eq!(event.event_type, "workflow.child_state");
        assert_eq!(event.altitude.as_deref(), Some("detail"));
        assert_eq!(event.role, Some(SessionRole::Runtime.to_storage()));
        let payload: serde_json::Value =
            serde_json::from_str(event.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["child_status"], "failed");
        assert_eq!(payload["summary"], "Script execution failed");
    }

    #[test]
    fn ingest_chat_human_stays_operator_message() {
        let event = ingest_chat_timeline_event(
            "session-1",
            None,
            "please fix the cron job",
            Some(&serde_json::json!({ "source": "session_room" })),
        )
        .expect("human chat should emit");
        assert_eq!(event.event_type, "operator.message");
        assert_eq!(event.altitude.as_deref(), Some("normal"));
    }

    #[test]
    fn operator_message_event_attributes_human_and_agent() {
        // A human chat (no source_agent_id) ⇒ Operator seat / Human principal,
        // Normal altitude (Operator's role_floor is Detail ⇒ max stays Normal).
        let human = operator_message_event("session-1", None, "hello there");
        assert_eq!(human.event_type, "operator.message");
        assert_eq!(human.altitude.as_deref(), Some("normal"));
        assert_eq!(human.role, Some(SessionRole::Operator.to_storage()));
        assert_eq!(
            human.principal_kind,
            Some(Principal::human("operator").kind_to_storage())
        );
        assert_eq!(human.principal_id.as_deref(), Some("operator"));
        let payload: serde_json::Value =
            serde_json::from_str(human.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["message"], "hello there");

        // An agent-originated ingest is attributed to that agent, not the operator.
        let agent = operator_message_event("session-1", Some("planner.default"), "ping");
        assert_eq!(agent.principal_id.as_deref(), Some("planner.default"));
        assert_ne!(
            agent.principal_kind,
            Some(Principal::human("operator").kind_to_storage())
        );

        // role_floor must still raise altitude: a Sentinel-seat message surfaces
        // at Attention, not Normal (the raise-only invariant — not hard-coded).
        let sentinel = operator_message_event("session-1", Some("sentinel.divergence"), "halt");
        assert_eq!(sentinel.altitude.as_deref(), Some("attention"));
    }

    #[test]
    fn sentinel_floor_raises_mild_events() {
        // turn.start is Detail, but a Sentinel seat raises it to Attention.
        assert_eq!(
            altitude_for("turn.start", &SessionRole::Sentinel),
            Altitude::Attention
        );
        // A planner's turn.start stays Detail.
        assert_eq!(
            altitude_for("turn.start", &SessionRole::Planner),
            Altitude::Detail
        );
    }

    #[test]
    fn create_approval_emits_approval_pending_timeline_event() {
        use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};

        let temp = tempfile::tempdir().unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(temp.path()).unwrap();
        let mut approval = ApprovalRequest {
            request_id: "apr-test01".to_string(),
            agent_id: "unit_test_runner.default".to_string(),
            session_id: "root/unit_test_runner-abc".to_string(),
            root_session_id: Some("root".to_string()),
            workflow_id: None,
            task_id: None,
            action: ScheduledAction::SandboxExec {
                command: "python3 test.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            status: None,
            decided_at: None,
            decided_by: None,
            reason: Some("run tests".to_string()),
            evidence_ref: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store.create_approval(&mut approval).unwrap();
        let page = store
            .list_session_timeline("root", None, 10, None, None)
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].event_type, "approval.pending");
        assert_eq!(
            page.entries[0].refs.approval_request_id.as_deref(),
            Some("apr-test01")
        );
    }

    #[test]
    fn create_user_interaction_emits_user_ask_pending_timeline_event() {
        use autonoetic_types::background::{
            UserInteraction, UserInteractionKind, UserInteractionStatus,
        };

        let temp = tempfile::tempdir().unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(temp.path()).unwrap();
        let interaction = UserInteraction {
            interaction_id: "ui-test01".to_string(),
            session_id: "root/planner-abc".to_string(),
            root_session_id: "root".to_string(),
            workflow_id: None,
            task_id: None,
            agent_id: "planner.default".to_string(),
            turn_id: "turn-000002".to_string(),
            kind: UserInteractionKind::Clarification,
            question: "Which API should I use?".to_string(),
            context: Some("Signals:\n- loop_pressure (critical): 10 cycles without progress".to_string()),
            options: vec![],
            allow_freeform: true,
            status: UserInteractionStatus::Pending,
            answer_option_id: None,
            answer_text: None,
            answered_by: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            answered_at: None,
            expires_at: None,
            checkpoint_turn_id: Some("turn-000002".to_string()),
        };
        store.create_user_interaction(&interaction).unwrap();
        let page = store
            .list_session_timeline("root", None, 10, None, None)
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].event_type, "user.ask.pending");
        assert_eq!(
            page.entries[0].refs.interaction_id.as_deref(),
            Some("ui-test01")
        );
        let payload: serde_json::Value =
            serde_json::from_str(page.entries[0].payload.as_deref().unwrap_or("{}")).unwrap();
        assert!(payload.get("context").is_some());
        assert!(payload["context"]
            .as_str()
            .unwrap_or("")
            .contains("loop_pressure"));
    }

    #[test]
    fn build_timeline_event_populates_attribution_and_derives_altitude() {
        let principal = autonoetic_types::principal::Principal::agent("planner.default");
        let role = derive_role("planner.default");
        let refs = autonoetic_types::session_timeline::TimelineRefs {
            approval_request_id: Some("apr-abc".to_string()),
            ..Default::default()
        };
        let ev = build_timeline_event(
            "root-1".to_string(),
            "root-1".to_string(),
            None,
            &principal,
            &role,
            "approval.pending",
            None, // derive
            Some(serde_json::json!({ "request_id": "apr-abc" })),
            refs,
        );
        assert_eq!(ev.event_type, "approval.pending");
        // approval.pending is a gate ⇒ Attention.
        assert_eq!(ev.altitude.as_deref(), Some("attention"));
        assert_eq!(ev.role.as_deref(), Some("planner"));
        assert_eq!(ev.principal_kind.as_deref(), Some("autonoetic_agent"));
        assert_eq!(ev.principal_id.as_deref(), Some("planner.default"));
        assert!(ev.refs_json.as_deref().unwrap().contains("apr-abc"));
    }

    #[test]
    fn agent_narrative_altitudes() {
        // The agent speaking is first-class (Normal, default floor); its reasoning
        // is hidable (Detail) until the operator dials down. (#367 P4)
        assert_eq!(base_altitude("agent.message"), Altitude::Normal);
        assert_eq!(base_altitude("agent.reasoning"), Altitude::Detail);
        // A Sentinel still raises both to its floor.
        assert_eq!(
            altitude_for("agent.reasoning", &SessionRole::Sentinel),
            Altitude::Attention
        );
    }

    #[test]
    fn altitude_policy_gate_lifecycle_shares_attention() {
        // A gate's request AND decision share Attention so each ask reads
        // paired with its resolution. Abandonments (cancelled/withdrawn) are
        // NOT decisions → Normal.
        for approved in ["plan.approved", "approval.approved", "approval.rejected"] {
            assert_eq!(base_altitude(approved), Altitude::Attention, "{approved}");
        }
        for pending in [
            "plan.pending", "approval.pending", "escalation.pending",
            "user.ask.pending", "wiki.proposed", "wiki.promoted", "wiki.rejected",
        ] {
            assert_eq!(base_altitude(pending), Altitude::Attention, "{pending}");
        }
        // Abandonments — visible, but not decision checkpoints.
        assert_eq!(base_altitude("approval.cancelled"), Altitude::Normal);
        assert_eq!(base_altitude("wiki.withdrawn"), Altitude::Normal);
    }

    #[test]
    fn altitude_policy_tool_completion_is_detail_success() {
        // tool.requested and tool.completed are a matched pair at Detail
        // (success = plumbing). Failures are bumped to Attention at the emit
        // site (see session_tracer), not here.
        assert_eq!(base_altitude("tool.requested"), Altitude::Detail);
        assert_eq!(base_altitude("tool.completed"), Altitude::Detail);
    }

    #[test]
    fn altitude_policy_workbench_created_is_normal_milestone() {
        // Creation is a milestone (content becomes reviewable); reconcile/
        // discard remain plumbing.
        assert_eq!(base_altitude("workbench.created"), Altitude::Normal);
        assert_eq!(base_altitude("workbench.reconciled"), Altitude::Detail);
        assert_eq!(base_altitude("workbench.discarded"), Altitude::Detail);
    }

    #[test]
    fn altitude_policy_explicit_normal_progress() {
        for et in [
            "agent.message", "operator.message", "session.start", "session.end",
            "digest_annotate", "llm.retry",
        ] {
            assert_eq!(base_altitude(et), Altitude::Normal, "{et}");
        }
    }

    #[test]
    fn guard_tripped_is_error() {
        // A loop-guard trip terminates the session — it must surface at the
        // Error floor so the room shows *why*, not slip by as routine.
        assert_eq!(base_altitude("guard.tripped"), Altitude::Error);
    }

    #[test]
    fn emergency_stop_is_error_and_attributed() {
        use autonoetic_types::principal::PrincipalKind;
        // Always Error — the circuit breaker must surface at the top floor.
        assert_eq!(base_altitude("session.emergency_stop"), Altitude::Error);

        // Attribution by explicit kind. The emergency-stop API/CLI uses "user";
        // it (and "operator"/"human") must read as a human in the Operator seat.
        for human_kind in ["user", "operator", "human"] {
            let (op, seat) = actor_from_kind_id(human_kind, "operator");
            assert!(
                matches!(op.kind, PrincipalKind::Human) && matches!(seat, SessionRole::Operator),
                "kind {human_kind:?} should map to Human/Operator"
            );
        }
        let (agent, seat) = actor_from_kind_id("agent", "auditor.default");
        assert!(matches!(agent.kind, PrincipalKind::AutonoeticAgent));
        assert!(matches!(seat, SessionRole::Auditor));
        let (sys, seat) = actor_from_kind_id("security_policy", "gateway");
        assert!(matches!(sys.kind, PrincipalKind::Script) && matches!(seat, SessionRole::Runtime));
    }

    #[test]
    fn escape_threshold_is_attention() {
        assert_eq!(
            base_altitude("security.escape_threshold"),
            Altitude::Attention
        );
        assert_eq!(
            altitude_for("security.escape_threshold", &SessionRole::Runtime),
            Altitude::Attention
        );
    }

    #[test]
    fn failures_stay_error_regardless_of_seat() {
        assert_eq!(
            altitude_for("llm.request_failed", &SessionRole::Planner),
            Altitude::Error
        );
    }

    #[test]
    fn decider_seat_attribution() {
        use autonoetic_types::principal::PrincipalKind;

        let (p, r) = decider_seat("operator");
        assert_eq!(p.kind, PrincipalKind::Human);
        assert_eq!(r, SessionRole::Operator);

        // `agent:` prefix is stripped before deriving the seat (regression: #375 review).
        let (p, r) = decider_seat("agent:auditor.default");
        assert_eq!(p.kind, PrincipalKind::AutonoeticAgent);
        assert_eq!(p.id, "auditor.default");
        assert_eq!(r, SessionRole::Auditor);

        let (_p, r) = decider_seat("coder.default");
        assert_eq!(r, SessionRole::Specialist { kind: "coder".to_string() });

        // Mechanical resolutions ⇒ hidable Runtime seat, never Specialist{emergency_stop:..}.
        let (_p, r) = decider_seat("emergency_stop:estop-1a2b3c4d");
        assert_eq!(r, SessionRole::Runtime);
        let (_p, r) = decider_seat("gateway");
        assert_eq!(r, SessionRole::Runtime);
    }

    #[test]
    fn role_derivation() {
        assert_eq!(derive_role("planner.default"), SessionRole::Planner);
        assert_eq!(derive_role("sentinel.divergence"), SessionRole::Sentinel);
        assert_eq!(
            derive_role("coder.default"),
            SessionRole::Specialist { kind: "coder".to_string() }
        );
    }

    #[test]
    fn role_floor_config_override() {
        let mut floors = std::collections::HashMap::new();
        floors.insert("planner".to_string(), "attention".to_string());
        floors.insert("runtime".to_string(), "normal".to_string());

        assert_eq!(
            role_floor_with_config(&SessionRole::Planner, Some(&floors)),
            Altitude::Attention
        );
        assert_eq!(
            role_floor_with_config(&SessionRole::Runtime, Some(&floors)),
            Altitude::Normal
        );
        assert_eq!(
            role_floor_with_config(&SessionRole::Sentinel, Some(&floors)),
            Altitude::Attention,
            "sentinel uses hardcoded default when not in config map"
        );
    }

    #[test]
    fn role_floor_config_ignores_invalid_altitude() {
        let mut floors = std::collections::HashMap::new();
        floors.insert("sentinel".to_string(), "invalid_value".to_string());
        assert_eq!(
            role_floor_with_config(&SessionRole::Sentinel, Some(&floors)),
            Altitude::Attention,
            "invalid altitude string falls back to hardcoded default"
        );
    }

    #[test]
    fn role_floor_no_config_uses_defaults() {
        assert_eq!(
            role_floor_with_config(&SessionRole::Sentinel, None),
            Altitude::Attention
        );
        assert_eq!(
            role_floor_with_config(&SessionRole::Runtime, None),
            Altitude::Detail
        );
        assert_eq!(
            role_floor_with_config(&SessionRole::Planner, None),
            Altitude::Detail
        );
    }
}
