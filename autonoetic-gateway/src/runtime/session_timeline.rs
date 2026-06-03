//! Session Room timeline classification (#363 P1): map a digest event's type +
//! seat to an [`Altitude`], and derive the [`SessionRole`] seat from an agent id.
//!
//! Importance is gateway-owned: `altitude = max(base(event_type),
//! role_floor(role))`. Roles may only *raise* the floor, never suppress, so a
//! critical seat (Sentinel) always surfaces. `role_floor` defaults live here and
//! are config-tunable (don't-pin-tunables); base mapping is deterministic.

use autonoetic_types::session_timeline::{Altitude, SessionRole};

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
