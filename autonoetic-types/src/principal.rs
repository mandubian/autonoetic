//! Principal model (#359 / #360) — a citizen's identity and kind.
//!
//! A `Principal` is *who* an actor is, independent of *which seat* (role) it
//! occupies in a session (see [`crate::session_timeline::SessionRole`]). Human,
//! autonoetic agent, script, and foreign AI agents are all first-class citizens
//! under the shared constitution; obligations attach to the seat's decisions,
//! not the principal's kind.
//!
//! This module is **additive** — `id` is the same value already bound into the
//! causal-chain entry hash as `actor_id`; we surface it as a typed principal,
//! we do not introduce a new identity system.

use serde::{Deserialize, Serialize};

/// The nature of a citizen. Kind discrimination for display, attribution, and
/// trust posture — not authority (authority is a role/seat concern, #359 Part D).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A human operator.
    Human,
    /// An agent running under this gateway (planner, specialists, sentinel…).
    AutonoeticAgent,
    /// A non-reasoning automation (cron, CI, hook).
    Script,
    /// An external AI agent federating in (claude-code, codex, opencode…),
    /// low-privilege under Separation of Powers.
    ForeignAgent { provider: String },
    /// The end-user a session ultimately serves, when distinct from the
    /// operator running the gateway (e.g. a hosted or multi-tenant
    /// deployment). Distinct from `Human` so the two can never be conflated
    /// once they diverge — see `docs/philosophy.md` §3.3 and §5.1. Carries no
    /// extra authority by default; it is an attribution kind, not a seat.
    ServedUser,
}

impl PrincipalKind {
    /// Stable discriminant tag (provider-independent).
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::AutonoeticAgent => "autonoetic_agent",
            Self::Script => "script",
            Self::ForeignAgent { .. } => "foreign_agent",
            Self::ServedUser => "served_user",
        }
    }

    /// Whether this principal is external to the gateway (carries no privilege
    /// by default; introduced effects gate at elevated authority — RFC §5).
    pub fn is_foreign(&self) -> bool {
        matches!(self, Self::ForeignAgent { .. })
    }
}

/// A citizen: identity + kind. `id` equals the causal-chain `actor_id`.
///
/// `kind` is flattened so the wire shape is flat — `{"kind":"human","id":...}`,
/// or `{"kind":"foreign_agent","provider":"…","id":…}` — rather than the nested
/// `{"kind":{"kind":"human"},…}` the internally-tagged enum would otherwise give.
/// Best-effort principal kind of a gate *decider*, derived from the recorded
/// `decided_by` string (#359 P1.b / #361). Deterministic and mechanically
/// checkable, and **fail-safe**: an unrecognized token is never claimed as an
/// accountable agent.
///
/// - `"operator"` ⇒ Human.
/// - `"user:<id>"` ⇒ ServedUser — the end-user a session serves, distinct
///   from the operator once the two diverge (hosted/multi-tenant
///   deployments). Checked before the agent-shape heuristic below so a
///   dotted user id (e.g. `"user:alice.smith"`) is never misclassified as an
///   agent.
/// - Mechanical / executor resolutions ⇒ `None` (no §O obligation attaches):
///   empty, `"gateway"`, `"system"`, and the `"emergency_stop:<id>"` cascade.
/// - An agent decider ⇒ AutonoeticAgent, recognized positively by an agent-id
///   shape (contains `.`, e.g. `auditor.default`) or the `"agent:<id>"` form.
/// - Anything else ⇒ `None` (fail-safe, not AutonoeticAgent).
///
/// Foreign agents never decide gates (they hold no authority), so they are not
/// produced here.
pub fn decider_principal_kind(decided_by: &str) -> Option<PrincipalKind> {
    let s = decided_by.trim();
    if s == "operator" {
        return Some(PrincipalKind::Human);
    }
    if s.starts_with("user:") {
        return Some(PrincipalKind::ServedUser);
    }
    if s.is_empty() || s == "gateway" || s == "system" || s.starts_with("emergency_stop:") {
        return None;
    }
    if s.starts_with("agent:") || s.contains('.') {
        return Some(PrincipalKind::AutonoeticAgent);
    }
    None
}

/// True when `id` is a reserved, non-agent principal id — the operator, the
/// gateway/system itself, an emergency-stop actor, or empty. These are never
/// seeded agents, so callers must not resolve them through the agent repository
/// (e.g. an operator- or gateway-initiated spawn has no "source agent" whose
/// capabilities gate the spawn; trying to load one bails with "No alias found").
pub fn is_reserved_non_agent_id(id: &str) -> bool {
    let s = id.trim();
    s.is_empty()
        || s == "operator"
        || s == "gateway"
        || s == "system"
        || s.starts_with("emergency_stop:")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    #[serde(flatten)]
    pub kind: PrincipalKind,
    pub id: String,
}

impl Principal {
    pub fn human(id: impl Into<String>) -> Self {
        Self { kind: PrincipalKind::Human, id: id.into() }
    }

    pub fn agent(id: impl Into<String>) -> Self {
        Self { kind: PrincipalKind::AutonoeticAgent, id: id.into() }
    }

    pub fn foreign(provider: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: PrincipalKind::ForeignAgent { provider: provider.into() },
            id: id.into(),
        }
    }

    pub fn served_user(id: impl Into<String>) -> Self {
        Self { kind: PrincipalKind::ServedUser, id: id.into() }
    }

    /// Encode the kind for a single text column: `"foreign_agent:<provider>"`
    /// for foreign principals, the bare tag otherwise. Round-trips via
    /// [`Principal::kind_from_storage`].
    pub fn kind_to_storage(&self) -> String {
        match &self.kind {
            PrincipalKind::ForeignAgent { provider } => format!("foreign_agent:{provider}"),
            other => other.tag().to_string(),
        }
    }

    /// Inverse of [`Principal::kind_to_storage`]. Unknown tags fall back to
    /// `AutonoeticAgent` (the common case) rather than erroring.
    pub fn kind_from_storage(s: &str) -> PrincipalKind {
        match s {
            "human" => PrincipalKind::Human,
            "script" => PrincipalKind::Script,
            "served_user" => PrincipalKind::ServedUser,
            s if s.starts_with("foreign_agent") => {
                let provider = s.strip_prefix("foreign_agent:").unwrap_or("").to_string();
                PrincipalKind::ForeignAgent { provider }
            }
            _ => PrincipalKind::AutonoeticAgent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_non_agent_ids_are_recognized() {
        for id in ["operator", "gateway", "system", "", "  ", "emergency_stop:root-x"] {
            assert!(is_reserved_non_agent_id(id), "{id:?} should be reserved");
        }
        // Real agent ids (dotted / agent: form) are NOT reserved — the source
        // capability check must still run for genuine agent-initiated spawns.
        for id in ["memory-curator.default", "planner.default", "agent:coder"] {
            assert!(!is_reserved_non_agent_id(id), "{id:?} must not be reserved");
        }
    }

    #[test]
    fn foreign_kind_round_trips_through_storage() {
        let p = Principal::foreign("claude-code", "fa-123");
        assert_eq!(p.kind_to_storage(), "foreign_agent:claude-code");
        assert_eq!(
            Principal::kind_from_storage(&p.kind_to_storage()),
            PrincipalKind::ForeignAgent { provider: "claude-code".to_string() }
        );
    }

    #[test]
    fn principal_serializes_to_flat_shape() {
        let human = serde_json::to_value(Principal::human("op-1")).unwrap();
        assert_eq!(human, serde_json::json!({ "kind": "human", "id": "op-1" }));

        let foreign = serde_json::to_value(Principal::foreign("claude-code", "fa-1")).unwrap();
        assert_eq!(
            foreign,
            serde_json::json!({ "kind": "foreign_agent", "provider": "claude-code", "id": "fa-1" })
        );

        // Round-trips back.
        let back: Principal = serde_json::from_value(foreign).unwrap();
        assert_eq!(back, Principal::foreign("claude-code", "fa-1"));
    }

    #[test]
    fn decider_kind_derivation() {
        assert_eq!(decider_principal_kind("operator"), Some(PrincipalKind::Human));
        // Agent deciders, both shapes.
        assert_eq!(
            decider_principal_kind("auditor.default"),
            Some(PrincipalKind::AutonoeticAgent)
        );
        assert_eq!(
            decider_principal_kind("agent:auditor.default"),
            Some(PrincipalKind::AutonoeticAgent)
        );
        // Executor mechanics are not principal decisions.
        assert_eq!(decider_principal_kind("gateway"), None);
        assert_eq!(decider_principal_kind("system"), None);
        assert_eq!(decider_principal_kind("  "), None);
        // Emergency-stop cascade is mechanical, not an agent (regression: #374 review).
        assert_eq!(decider_principal_kind("emergency_stop:estop-1a2b3c4d"), None);
        // Fail-safe: an unrecognized bare token is not claimed as an agent.
        assert_eq!(decider_principal_kind("mystery"), None);
        // Served-user attribution is distinct from the operator (Human), and
        // is recognized even when the id itself contains a dot.
        assert_eq!(decider_principal_kind("user:alice"), Some(PrincipalKind::ServedUser));
        assert_eq!(
            decider_principal_kind("user:alice.smith"),
            Some(PrincipalKind::ServedUser)
        );
        assert_ne!(decider_principal_kind("user:alice"), decider_principal_kind("operator"));
    }

    #[test]
    fn served_user_round_trips_through_storage() {
        let p = Principal::served_user("user-42");
        assert_eq!(p.kind_to_storage(), "served_user");
        assert_eq!(Principal::kind_from_storage(&p.kind_to_storage()), PrincipalKind::ServedUser);
    }

    #[test]
    fn bare_tags_round_trip_and_unknown_defaults_to_agent() {
        for p in [Principal::human("op"), Principal::agent("planner.default")] {
            assert_eq!(Principal::kind_from_storage(&p.kind_to_storage()), p.kind);
        }
        assert_eq!(
            Principal::kind_from_storage("nonsense"),
            PrincipalKind::AutonoeticAgent
        );
    }
}
