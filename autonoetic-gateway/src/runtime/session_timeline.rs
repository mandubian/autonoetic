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
pub fn base_altitude(event_type: &str) -> Altitude {
    match event_type {
        // Mechanics / poll-ish / infra — hidden at the normal floor. Workbench
        // lifecycle is plumbing around the real work (edits/artifacts), so it
        // stays Detail and surfaces only when the operator dials down.
        // Extended-thinking "why" is verbose; hidable by default, surfaced on dial-down.
        "turn.start" | "turn.end" | "llm.round" | "tool.requested" | "agent.reasoning"
        | "workbench.created" | "workbench.reconciled" | "workbench.discarded" => Altitude::Detail,
        // Failures and the emergency-stop circuit breaker always surface.
        "llm.request_failed" | "tool.failed" | "session.emergency_stop" => Altitude::Error,
        // Gates awaiting the operator (conversational asks, RFC §3.5) and
        // integrity events. `runtime.lock_drift` stores an explicit altitude
        // (Error when rejected, Attention when overridden); this is just the
        // safe floor for any NULL-altitude fallback.
        "user.ask.pending" | "approval.pending" | "plan.pending" | "divergence.intervention"
        | "runtime.lock_drift" | "escalation.pending" => Altitude::Attention,
        // Everything else is normal progress.
        _ => Altitude::Normal,
    }
}

/// Minimum altitude a seat guarantees for its events. Only raises (`max`),
/// never lowers. Tunable later via `session_room.role_floors` config.
pub fn role_floor(role: &SessionRole) -> Altitude {
    match role {
        // A divergence/security intervention must never sit below the floor.
        SessionRole::Sentinel => Altitude::Attention,
        // The executor's mechanical voice is hidable by default.
        SessionRole::Runtime => Altitude::Detail,
        _ => Altitude::Detail,
    }
}

/// `max(base, role_floor)` — the effective altitude written to the row.
pub fn altitude_for(event_type: &str, role: &SessionRole) -> Altitude {
    base_altitude(event_type).max(role_floor(role))
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
}
