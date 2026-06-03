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
}

impl PrincipalKind {
    /// Stable discriminant tag (provider-independent).
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::AutonoeticAgent => "autonoetic_agent",
            Self::Script => "script",
            Self::ForeignAgent { .. } => "foreign_agent",
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
