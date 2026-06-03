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

/// Base importance of a digest event type, before any role refinement.
pub fn base_altitude(event_type: &str) -> Altitude {
    match event_type {
        // Mechanics / poll-ish — hidden at the normal floor.
        "turn.start" | "turn.end" | "llm.round" | "tool.requested" => Altitude::Detail,
        // Failures always surface.
        "llm.request_failed" | "tool.failed" => Altitude::Error,
        // Gates awaiting the operator (conversational asks, RFC §3.5).
        "user.ask.pending" | "approval.pending" | "plan.pending" | "divergence.intervention" => {
            Altitude::Attention
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn failures_stay_error_regardless_of_seat() {
        assert_eq!(
            altitude_for("llm.request_failed", &SessionRole::Planner),
            Altitude::Error
        );
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
