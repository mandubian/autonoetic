//! Session Room canonical timeline (#363 / P1).
//!
//! The gateway merges every session stream — the live digest (spine), operator
//! activity, gate asks/decisions, plan/workbench changes, foreign-actor events —
//! into one ordered, actor-attributed, channel-neutral timeline. Every channel
//! (TUI, Discord, WhatsApp, IDE) renders the *same* entries; importance and
//! merge live here, presentation lives in the renderer.

use crate::principal::Principal;
use serde::{Deserialize, Serialize};

/// Importance axis the reader filters on and renderers dial. Ordered
/// `Detail < Normal < Attention < Error`; `min_altitude` keeps `>=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Altitude {
    /// Routine mechanics; hidden at the normal floor (incl. Runtime notices).
    Detail,
    /// Default floor — normal progress.
    Normal,
    /// Needs the operator's eye (gates, divergence interventions).
    Attention,
    /// Failure.
    Error,
}

impl Altitude {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Detail => "detail",
            Self::Normal => "normal",
            Self::Attention => "attention",
            Self::Error => "error",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "detail" => Some(Self::Detail),
            "normal" => Some(Self::Normal),
            "attention" => Some(Self::Attention),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// The seat a participant occupies — occupant-agnostic (a human or an AI may
/// hold `Operator`). Distinct from [`crate::principal::PrincipalKind`] (who).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum SessionRole {
    /// The deciding seat (gates, redirects). Symmetric obligations attach here.
    Operator,
    Planner,
    Specialist { kind: String },
    /// Divergence / security monitor — a participant, not system chrome.
    Sentinel,
    Curator,
    Auditor,
    /// Internal tool surface (workbench, plan, sandbox).
    Tool { surface: String },
    /// External tool surface (IDE, editor).
    ExternalSurface { surface: String },
    /// The executor's own voice (lifecycle, mechanical rulings).
    Runtime,
}

impl SessionRole {
    /// Encode for a single text column; parameterized seats keep their kind:
    /// `"specialist:coder"`, `"tool:workbench"`. Round-trips via [`from_storage`].
    pub fn to_storage(&self) -> String {
        match self {
            Self::Operator => "operator".into(),
            Self::Planner => "planner".into(),
            Self::Specialist { kind } => format!("specialist:{kind}"),
            Self::Sentinel => "sentinel".into(),
            Self::Curator => "curator".into(),
            Self::Auditor => "auditor".into(),
            Self::Tool { surface } => format!("tool:{surface}"),
            Self::ExternalSurface { surface } => format!("external_surface:{surface}"),
            Self::Runtime => "runtime".into(),
        }
    }

    /// Inverse of [`to_storage`]. Unknown ⇒ `Specialist{kind: "unknown"}`.
    pub fn from_storage(s: &str) -> Self {
        match s {
            "operator" => Self::Operator,
            "planner" => Self::Planner,
            "sentinel" => Self::Sentinel,
            "curator" => Self::Curator,
            "auditor" => Self::Auditor,
            "runtime" => Self::Runtime,
            s => {
                if let Some(k) = s.strip_prefix("specialist:") {
                    Self::Specialist { kind: k.to_string() }
                } else if let Some(sf) = s.strip_prefix("tool:") {
                    Self::Tool { surface: sf.to_string() }
                } else if let Some(sf) = s.strip_prefix("external_surface:") {
                    Self::ExternalSurface { surface: sf.to_string() }
                } else {
                    Self::Specialist { kind: "unknown".to_string() }
                }
            }
        }
    }
}

/// Cross-references letting a renderer drill from a timeline line into depth.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineRefs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causal_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interaction_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workbench_id: Option<String>,
    /// Constitutional rule/right IDs this event enforces (e.g. `P-7.19`, `Ri-0.9`)
    /// — first-class so channels can attribute a refusal/gate to its clause and
    /// look the clause up, instead of parsing the payload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enforced_rules: Vec<String>,
}

impl TimelineRefs {
    pub fn is_empty(&self) -> bool {
        *self == TimelineRefs::default()
    }
}

/// One thing that happened in the session, from any stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTimelineEntry {
    pub event_id: String,
    pub root_session_id: String,
    pub source_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// WHO acted.
    pub principal: Principal,
    /// WHICH seat it occupied.
    pub role: SessionRole,
    pub event_type: String,
    pub altitude: Altitude,
    pub occurred_at: String,
    /// Raw JSON payload (renderers format it); `None` for marker events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    #[serde(default, skip_serializing_if = "TimelineRefs::is_empty")]
    pub refs: TimelineRefs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTimelineListParams {
    pub root_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_event_id: Option<String>,
    #[serde(default = "default_timeline_limit")]
    pub limit: u32,
    /// Floor on altitude (`>=`). `None` ⇒ `Normal` is applied by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_altitude: Option<String>,
    /// Optional principal-id filter (e.g. show only one actor's events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
}

fn default_timeline_limit() -> u32 {
    100
}

/// Parent planner turn at which a child session was spawned (`agent.spawn`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpawnLineageEntry {
    pub child_session_id: String,
    pub parent_session_id: String,
    pub spawned_at_turn: u64,
    pub target_agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTimelineListResult {
    pub entries: Vec<SessionTimelineEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub has_more: bool,
    /// Spawn lineage for child sessions under this root — lets channels label
    /// parallel sub-agent rows with the parent planner turn (e.g. `3→coder`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawn_lineage: Vec<SessionSpawnLineageEntry>,
}

/// Parameters for `session.list` — discover existing root sessions so the
/// operator can reload or attach to one. Optional `agent_id` filter narrows
/// the list to a single agent's sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListParams {
    /// Optional agent filter (e.g. `planner.default`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Max entries to return. Clamped to 1..=500 by the gateway.
    #[serde(default = "default_list_limit")]
    pub limit: u32,
}

fn default_list_limit() -> u32 {
    50
}

/// One row of `session.list`. The room uses `last_active_at` to display
/// "last activity 12m ago" hints; the SDKs use it for sorting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListEntry {
    pub root_session_id: String,
    pub agent_id: String,
    pub last_active_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListResult {
    pub sessions: Vec<SessionListEntry>,
}
