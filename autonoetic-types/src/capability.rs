//! Capability enums for agent permission declarations.
//!
//! Capability categories:
//! - **SandboxFunctions**: MCP tool access by prefix (web_, sandbox_)
//! - **ReadAccess**: Read content, memory, knowledge (includes search)
//! - **WriteAccess**: Write content, memory, knowledge (includes share)
//! - **CodeExecution**: Execute command strings with `sandbox_exec`
//! - **ArtifactExecution**: Execute immutable artifact entrypoints
//! - **NetworkAccess**: Make HTTP requests
//! - **AgentSpawn**: Create child agent sessions
//! - **AgentMessage**: Send messages to other agents
//! - **BackgroundReevaluation**: Periodic wake-ups

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A typed capability that an Agent may request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Capability {
    /// MCP tool access by prefix.
    /// Controls which external tools (from MCP servers) can be invoked.
    /// Example: ["web.", "sandbox.exec"] allows web tools and sandbox.exec.
    SandboxFunctions { allowed: Vec<String> },

    /// Read access to all storage: content, memory, knowledge.
    /// Includes search operations.
    /// The `scopes` field restricts which paths/areas can be read.
    ReadAccess { scopes: Vec<String> },

    /// Write access to all storage: content, memory, knowledge.
    /// Includes sharing with other agents.
    /// The `scopes` field restricts which paths/areas can be written.
    WriteAccess { scopes: Vec<String> },

    /// HTTP/network access - escapes the sandbox boundary.
    /// Use ["*"] for all hosts, or specific domains.
    NetworkAccess { hosts: Vec<String> },

    /// Create child agent sessions.
    /// The `max_children` field limits concurrent children.
    /// The `max_spawn_depth` field limits how deep the spawn chain may go
    /// (0 = use system default). Depth is measured by counting `/` in session_id.
    AgentSpawn {
        max_children: u32,
        #[serde(default)]
        max_spawn_depth: u32,
    },

    /// Send messages to other agents.
    AgentMessage {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
    },

    /// Periodic wake-ups for background processing.
    BackgroundReevaluation {
        min_interval_secs: u64,
        allow_reasoning: bool,
    },

    /// Execute scripts/code in the sandbox.
    /// The `patterns` field limits which commands can be run (prefix matching).
    /// The `commands` field allows specific shell commands (word-boundary matching).
    CodeExecution {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
        #[serde(default)]
        commands: Vec<String>,
    },

    /// Execute immutable artifact entrypoints with `artifact_exec`.
    ///
    /// This is intentionally separate from [`Capability::CodeExecution`]:
    /// artifact execution is bound to a content-addressed artifact and does not
    /// authorize arbitrary command strings.
    ArtifactExecution,

    /// Request a gateway-level emergency stop for a root session (dedicated responders only).
    EmergencyStop,

    /// Access to agent revision operations (create, promote, rollback).
    AgentRevision {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
    },

    /// Access to evaluation operations (suite publish, run, report).
    Evaluation {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
    },

    /// Access to approval queue operations.
    ApprovalQueue {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
    },

    /// Access to scheduler signal operations.
    SchedulerSignal {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
    },

    /// Access to credential operations (check, request, setup).
    /// The `services` field restricts which services can be accessed.
    /// Use ["*"] for all services, or specific service names.
    CredentialAccess {
        #[serde(default = "default_services_all")]
        services: Vec<String>,
    },

    /// Access to user profile operations (read, update, share, revoke).
    /// The `scopes` field controls which operations are allowed.
    /// Use ["read"] for read-only, ["read", "write"] for full access.
    UserProfileAccess { scopes: Vec<String> },

    /// Access to scheduler/cron operations (create, list, pause, resume, cancel jobs).
    /// The `patterns` field restricts which operations are allowed.
    /// Use ["*"] for all operations, or specific patterns like "scheduler.cron.create".
    SchedulerAccess {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
    },

    /// Install a remote SKILL.md as a new local agent via `skill.install`.
    /// The `allowed_sources` field restricts which URL hosts are permitted.
    /// Use ["*"] to allow any source, or specific hosts like ["agentskills.io"].
    SkillInstall {
        #[serde(default = "default_sources_all")]
        allowed_sources: Vec<String>,
    },

    /// Submit constitutional amendment proposals via `constitution_propose_amendment`.
    /// Enforcement of Ri-0.8 (right to propose amendments) — a high-risk capability
    /// that must be explicitly granted; not a default. The `patterns` field selects
    /// which proposal kinds the agent may submit (e.g. `add_rule`, `modify_rule`,
    /// `remove_rule`, `add_right`, `modify_right`, `remove_right`, or `*` for all).
    ConstitutionalProposal {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
    },

    /// Read reasoning traces from other agents' sessions via `observability_read_reasoning`.
    /// Enforcement of Ri-0.13(c) — reasoning disclosure is capability-gated.
    /// The `targets` field scopes which agent IDs can be audited (prefix match, `*` for all).
    /// Every disclosure writes a `reasoning.disclosed` causal event to the reviewed agent's
    /// session, listing who read what and when.
    ReasoningAudit {
        #[serde(default = "default_patterns_all")]
        targets: Vec<String>,
    },

    /// Explicit opt-in to allow running with `max_session_price_usd` while
    /// model price metadata is unavailable.
    #[serde(rename = "budget.no_price_available.allow")]
    BudgetNoPriceAvailableAllow,

    /// Create GitHub issues via `github.issue.create`.
    /// The `patterns` field restricts which repos can be targeted.
    /// Use ["*"] for any repo, or specific patterns like "owner/repo".
    GithubIssueCreate {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
    },

    /// Submit adversarial attack-pattern proposals via `attack_pattern_propose`.
    /// Granted exclusively to the system-tier red-team agent. Intentionally
    /// separate from `Evaluation` — the red-team agent must NOT also author eval
    /// suites targeting itself (ownership invariant from #32 applies at tier boundary).
    SecurityRedTeam,

    /// Export or import Cognitive Capsules via `capsule.export` / `capsule.import`.
    /// Capsules carry agent revisions, runtime closures, optional memory
    /// snapshots, and (in Replay mode) session checkpoints across machines
    /// or gateways — a high-impact data-movement boundary that must be
    /// explicitly granted. Not a default. See
    /// `docs/cognitive-capsule.md`.
    CapsuleExport,

    /// Propose new wiki pages (docs) to be curated into the platform wiki.
    /// Writing durable documentation is a trust boundary — requires judgment.
    /// Only agents with this capability can call wiki.propose.
    WikiContribute,

    /// Access to PlanFrame operations (propose, amend, list, get).
    /// Controls whether an agent can create and modify collaborative plans.
    /// The `patterns` field restricts which operations are allowed.
    /// Use ["*"] for all *participation* operations, or specific patterns like
    /// "planframe.propose".
    ///
    /// `planframe.approve` is an **authority**, not participation: it is NEVER
    /// granted by `["*"]` or a prefix and must be listed EXACTLY
    /// (`"planframe.approve"`). This keeps a proposing agent (e.g. the planner,
    /// which holds `["*"]`) from approving its own plan — approval is a held
    /// right exercised by a distinct authority (the operator, or an agent
    /// explicitly granted it).
    PlanFrameAccess {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
    },

    /// Pre-authorizes promotion of an agent whose declared capabilities fall
    /// within this set. Checked at promotion time against locked session envelopes.
    PromoteWith {
        #[serde(default)]
        agent_id: String,
        capabilities: Vec<Capability>,
    },

    /// Resolve approval/escalation gates created by other agents.
    /// The `kinds` field declares which gate kinds the agent may resolve
    /// (`approval`, `escalation`, or both). An agent without `GateDecider`
    /// cannot call `approve_request` or `reject_request`. Decider agents are
    /// subject to the same dwell time, confirmation phrase, and hardening
    /// rules as human operators (P-2.24).
    GateDecider {
        #[serde(default = "default_patterns_all")]
        kinds: Vec<String>,
    },
}

/// Operations that confer **authority** rather than mere participation.
/// Authority rights must be granted EXACTLY — they are never satisfied by a
/// `*` wildcard or a prefix pattern. This is the separation-of-powers boundary
/// for pattern-scoped capabilities: a broad participation grant (e.g.
/// `PlanFrameAccess: ["*"]`) must NOT let a proposing agent exercise an
/// authority operation (e.g. `planframe.approve`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityOp {
    /// Approve a PlanFrame. The authority operation that motivated the
    /// e316cd53 separation-of-powers fix.
    PlanFrameApprove,
}

impl AuthorityOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthorityOp::PlanFrameApprove => "planframe.approve",
        }
    }

    /// True if `operation` is an authority-class operation.
    pub fn is_authority_operation(operation: &str) -> bool {
        operation == Self::PlanFrameApprove.as_str()
    }

    /// Whether a set of patterns grants `operation`.
    ///
    /// Empty/whitespace patterns — and degenerate prefixes like `"."` that
    /// trim to empty — grant nothing. Authority operations require an exact
    /// grant; participation operations may be granted by exact match, `*`, or
    /// a non-empty prefix.
    pub fn patterns_allow(patterns: &[String], operation: &str) -> bool {
        let authority = Self::is_authority_operation(operation);
        patterns.iter().any(|raw| {
            let p = raw.trim();
            if p.is_empty() {
                return false;
            }
            if p == operation {
                return true;
            }
            if authority {
                return false;
            }
            if p == "*" {
                return true;
            }
            let prefix = p.trim_end_matches('.');
            !prefix.is_empty() && operation.starts_with(prefix)
        })
    }
}

fn default_patterns_all() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_services_all() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_sources_all() -> Vec<String> {
    vec!["*".to_string()]
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityDelta {
    pub added: Vec<String>,
    pub broadened: Vec<CapabilityBroadening>,
    pub narrowed: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityBroadening {
    pub capability_type: String,
    pub previous_scope: Vec<String>,
    pub new_scope: Vec<String>,
}

impl CapabilityDelta {
    pub fn has_broadening(&self) -> bool {
        !self.added.is_empty() || !self.broadened.is_empty()
    }
}

pub fn compute_capability_delta(
    previous: &[Capability],
    current: &[Capability],
) -> CapabilityDelta {
    let mut added = Vec::new();
    let mut broadened = Vec::new();
    let mut narrowed = Vec::new();
    let mut removed = Vec::new();

    let previous_map = capability_map(previous);
    let current_map = capability_map(current);

    for (capability_type, current_cap) in &current_map {
        match previous_map.get(capability_type) {
            None => added.push(capability_type.clone()),
            Some(previous_cap) => {
                if let Some(b) = capability_broadening(capability_type, previous_cap, current_cap) {
                    broadened.push(b);
                } else if capability_narrowed(previous_cap, current_cap) {
                    narrowed.push(capability_type.clone());
                }
            }
        }
    }

    for capability_type in previous_map.keys() {
        if !current_map.contains_key(capability_type) {
            removed.push(capability_type.clone());
        }
    }

    CapabilityDelta {
        added,
        broadened,
        narrowed,
        removed,
    }
}

fn capability_map(caps: &[Capability]) -> BTreeMap<String, Capability> {
    let mut out = BTreeMap::new();
    for cap in caps {
        out.insert(capability_type_name(cap), cap.clone());
    }
    out
}

/// Canonical capability-kind names — every value here matches what
/// [`capability_type_name`] would produce for that `Capability`
/// variant. Exported so config-validation code (e.g.,
/// `ImproveConfig::high_blast_radius_capability_kinds`) can check
/// operator-supplied capability names against the known set rather
/// than silently accepting typos. Keep this list in sync with
/// [`capability_type_name`] — both are exhaustive over the
/// `Capability` enum.
pub fn all_capability_kind_names() -> &'static [&'static str] {
    &[
        "SandboxFunctions",
        "ReadAccess",
        "WriteAccess",
        "NetworkAccess",
        "AgentSpawn",
        "AgentMessage",
        "BackgroundReevaluation",
        "CodeExecution",
        "ArtifactExecution",
        "EmergencyStop",
        "AgentRevision",
        "Evaluation",
        "ApprovalQueue",
        "SchedulerSignal",
        "CredentialAccess",
        "UserProfileAccess",
        "SchedulerAccess",
        "SkillInstall",
        "ConstitutionalProposal",
        "ReasoningAudit",
        "GithubIssueCreate",
        // BudgetNoPriceAvailableAllow uses its serialized rename:
        "budget.no_price_available.allow",
        "SecurityRedTeam",
        "CapsuleExport",
        "PlanFrameAccess",
        "WikiContribute",
        "PromoteWith",
        "GateDecider",
    ]
}

fn capability_type_name(cap: &Capability) -> String {
    match cap {
        Capability::SandboxFunctions { .. } => "SandboxFunctions".to_string(),
        Capability::ReadAccess { .. } => "ReadAccess".to_string(),
        Capability::WriteAccess { .. } => "WriteAccess".to_string(),
        Capability::NetworkAccess { .. } => "NetworkAccess".to_string(),
        Capability::AgentSpawn { .. } => "AgentSpawn".to_string(),
        Capability::AgentMessage { .. } => "AgentMessage".to_string(),
        Capability::BackgroundReevaluation { .. } => "BackgroundReevaluation".to_string(),
        Capability::CodeExecution { .. } => "CodeExecution".to_string(),
        Capability::ArtifactExecution => "ArtifactExecution".to_string(),
        Capability::EmergencyStop => "EmergencyStop".to_string(),
        Capability::AgentRevision { .. } => "AgentRevision".to_string(),
        Capability::Evaluation { .. } => "Evaluation".to_string(),
        Capability::ApprovalQueue { .. } => "ApprovalQueue".to_string(),
        Capability::SchedulerSignal { .. } => "SchedulerSignal".to_string(),
        Capability::CredentialAccess { .. } => "CredentialAccess".to_string(),
        Capability::UserProfileAccess { .. } => "UserProfileAccess".to_string(),
        Capability::SchedulerAccess { .. } => "SchedulerAccess".to_string(),
        Capability::SkillInstall { .. } => "SkillInstall".to_string(),
        Capability::ConstitutionalProposal { .. } => "ConstitutionalProposal".to_string(),
        Capability::ReasoningAudit { .. } => "ReasoningAudit".to_string(),
        Capability::GithubIssueCreate { .. } => "GithubIssueCreate".to_string(),
        Capability::BudgetNoPriceAvailableAllow => "budget.no_price_available.allow".to_string(),
        Capability::SecurityRedTeam => "SecurityRedTeam".to_string(),
        Capability::CapsuleExport => "CapsuleExport".to_string(),
        Capability::PlanFrameAccess { .. } => "PlanFrameAccess".to_string(),
        Capability::WikiContribute => "WikiContribute".to_string(),
        Capability::PromoteWith { .. } => "PromoteWith".to_string(),
        Capability::GateDecider { .. } => "GateDecider".to_string(),
    }
}

/// True when every capability in `artifact_caps` is equal to or narrower than
/// some capability in `declared`.
pub fn capability_set_covers(declared: &[Capability], artifact_caps: &[Capability]) -> bool {
    let declared_map = capability_map(declared);
    artifact_caps.iter().all(|ac| {
        let name = capability_type_name(ac);
        match declared_map.get(&name) {
            None => false,
            Some(dc) => capability_broadening(&name, dc, ac).is_none(),
        }
    })
}

fn capability_broadening(
    capability_type: &str,
    previous: &Capability,
    current: &Capability,
) -> Option<CapabilityBroadening> {
    match (previous, current) {
        (Capability::NetworkAccess { hosts: a }, Capability::NetworkAccess { hosts: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (Capability::ReadAccess { scopes: a }, Capability::ReadAccess { scopes: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (Capability::WriteAccess { scopes: a }, Capability::WriteAccess { scopes: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (
            Capability::SandboxFunctions { allowed: a },
            Capability::SandboxFunctions { allowed: b },
        ) => scope_broadening(capability_type, a, b),
        (Capability::AgentMessage { patterns: a }, Capability::AgentMessage { patterns: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (Capability::AgentRevision { patterns: a }, Capability::AgentRevision { patterns: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (Capability::Evaluation { patterns: a }, Capability::Evaluation { patterns: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (Capability::ApprovalQueue { patterns: a }, Capability::ApprovalQueue { patterns: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (
            Capability::SchedulerSignal { patterns: a },
            Capability::SchedulerSignal { patterns: b },
        ) => scope_broadening(capability_type, a, b),
        (
            Capability::SchedulerAccess { patterns: a },
            Capability::SchedulerAccess { patterns: b },
        ) => scope_broadening(capability_type, a, b),
        (
            Capability::CredentialAccess { services: a },
            Capability::CredentialAccess { services: b },
        ) => scope_broadening(capability_type, a, b),
        (
            Capability::UserProfileAccess { scopes: a },
            Capability::UserProfileAccess { scopes: b },
        ) => scope_broadening(capability_type, a, b),
        (
            Capability::SkillInstall { allowed_sources: a },
            Capability::SkillInstall { allowed_sources: b },
        ) => scope_broadening(capability_type, a, b),
        (
            Capability::ConstitutionalProposal { patterns: a },
            Capability::ConstitutionalProposal { patterns: b },
        ) => scope_broadening(capability_type, a, b),
        (Capability::ReasoningAudit { targets: a }, Capability::ReasoningAudit { targets: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (
            Capability::GithubIssueCreate { patterns: a },
            Capability::GithubIssueCreate { patterns: b },
        ) => scope_broadening(capability_type, a, b),
        (
            Capability::PlanFrameAccess { patterns: a },
            Capability::PlanFrameAccess { patterns: b },
        ) => scope_broadening(capability_type, a, b),
        (
            Capability::GateDecider { kinds: a },
            Capability::GateDecider { kinds: b },
        ) => scope_broadening(capability_type, a, b),
        (
            Capability::CodeExecution {
                patterns: ap,
                commands: ac,
            },
            Capability::CodeExecution {
                patterns: bp,
                commands: bc,
            },
        ) => {
            let mut from = ap.clone();
            from.extend(ac.clone());
            let mut to = bp.clone();
            to.extend(bc.clone());
            scope_broadening(capability_type, &from, &to)
        }
        (
            Capability::AgentSpawn {
                max_children: max_a,
                max_spawn_depth: depth_a,
            },
            Capability::AgentSpawn {
                max_children: max_b,
                max_spawn_depth: depth_b,
            },
        ) => {
            let children_broadened = max_b > max_a;
            let depth_broadened = if *depth_a == 0 && *depth_b > 0 {
                false
            } else if *depth_a == 0 && *depth_b == 0 {
                false
            } else {
                *depth_b > *depth_a
            };
            if children_broadened || depth_broadened {
                let mut prev = vec![format!("max_children={}", max_a)];
                let mut next = vec![format!("max_children={}", max_b)];
                if depth_broadened {
                    prev.push(format!("max_spawn_depth={}", depth_a));
                    next.push(format!("max_spawn_depth={}", depth_b));
                }
                Some(CapabilityBroadening {
                    capability_type: capability_type.to_string(),
                    previous_scope: prev,
                    new_scope: next,
                })
            } else {
                None
            }
        }
        (
            Capability::BackgroundReevaluation {
                min_interval_secs: a_interval,
                allow_reasoning: a_reasoning,
            },
            Capability::BackgroundReevaluation {
                min_interval_secs: b_interval,
                allow_reasoning: b_reasoning,
            },
        ) => {
            if b_interval < a_interval || (*b_reasoning && !a_reasoning) {
                Some(CapabilityBroadening {
                    capability_type: capability_type.to_string(),
                    previous_scope: vec![format!(
                        "min_interval_secs={},allow_reasoning={}",
                        a_interval, a_reasoning
                    )],
                    new_scope: vec![format!(
                        "min_interval_secs={},allow_reasoning={}",
                        b_interval, b_reasoning
                    )],
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

fn scope_broadening(
    capability_type: &str,
    previous: &[String],
    current: &[String],
) -> Option<CapabilityBroadening> {
    if is_scope_broadened(previous, current) {
        Some(CapabilityBroadening {
            capability_type: capability_type.to_string(),
            previous_scope: previous.to_vec(),
            new_scope: current.to_vec(),
        })
    } else {
        None
    }
}

fn is_scope_broadened(previous: &[String], current: &[String]) -> bool {
    let prev_has_all = previous.iter().any(|x| x == "*");
    let curr_has_all = current.iter().any(|x| x == "*");
    if prev_has_all {
        return false;
    }
    if curr_has_all && !prev_has_all {
        return true;
    }
    let prev: BTreeSet<&str> = previous.iter().map(String::as_str).collect();
    let curr: BTreeSet<&str> = current.iter().map(String::as_str).collect();
    curr.iter().any(|scope| !prev.contains(scope))
}

fn capability_narrowed(previous: &Capability, current: &Capability) -> bool {
    match (previous, current) {
        (Capability::NetworkAccess { hosts: a }, Capability::NetworkAccess { hosts: b }) => {
            is_scope_narrowed(a, b)
        }
        (Capability::ReadAccess { scopes: a }, Capability::ReadAccess { scopes: b }) => {
            is_scope_narrowed(a, b)
        }
        (Capability::WriteAccess { scopes: a }, Capability::WriteAccess { scopes: b }) => {
            is_scope_narrowed(a, b)
        }
        (
            Capability::SandboxFunctions { allowed: a },
            Capability::SandboxFunctions { allowed: b },
        ) => is_scope_narrowed(a, b),
        _ => false,
    }
}

fn is_scope_narrowed(previous: &[String], current: &[String]) -> bool {
    let prev_has_all = previous.iter().any(|x| x == "*");
    let curr_has_all = current.iter().any(|x| x == "*");
    if curr_has_all {
        return false;
    }
    if prev_has_all && !curr_has_all {
        return true;
    }
    let prev: BTreeSet<&str> = previous.iter().map(String::as_str).collect();
    let curr: BTreeSet<&str> = current.iter().map(String::as_str).collect();
    curr.len() < prev.len() && curr.is_subset(&prev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_detects_added_capability_type() {
        let prev = vec![Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }];
        let curr = vec![
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
            Capability::SchedulerAccess {
                patterns: vec!["*".to_string()],
            },
        ];
        let d = compute_capability_delta(&prev, &curr);
        assert_eq!(d.added, vec!["SchedulerAccess".to_string()]);
        assert!(d.has_broadening());
    }

    #[test]
    fn delta_detects_scope_broadening() {
        let prev = vec![Capability::NetworkAccess {
            hosts: vec!["pypi.org".to_string()],
        }];
        let curr = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }];
        let d = compute_capability_delta(&prev, &curr);
        assert_eq!(d.broadened.len(), 1);
        assert_eq!(d.broadened[0].capability_type, "NetworkAccess");
        assert!(d.has_broadening());
    }

    #[test]
    fn delta_treats_narrowing_as_non_broadening() {
        let prev = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }];
        let curr = vec![Capability::NetworkAccess {
            hosts: vec!["api.example.com".to_string()],
        }];
        let d = compute_capability_delta(&prev, &curr);
        assert!(d.broadened.is_empty());
        assert!(!d.has_broadening());
        assert_eq!(d.narrowed, vec!["NetworkAccess".to_string()]);
    }

    #[test]
    fn delta_detects_broadening_when_scope_replaced() {
        let prev = vec![Capability::NetworkAccess {
            hosts: vec!["a.example.com".to_string(), "b.example.com".to_string()],
        }];
        let curr = vec![Capability::NetworkAccess {
            hosts: vec!["a.example.com".to_string(), "c.example.com".to_string()],
        }];
        let d = compute_capability_delta(&prev, &curr);
        assert_eq!(d.broadened.len(), 1);
        assert!(d.has_broadening());
    }

    #[test]
    fn delta_does_not_broaden_when_previous_has_wildcard() {
        let prev = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }];
        let curr = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string(), "api.example.com".to_string()],
        }];
        let d = compute_capability_delta(&prev, &curr);
        assert!(d.broadened.is_empty());
        assert!(!d.has_broadening());
    }

    #[test]
    fn delta_does_not_narrow_when_current_has_wildcard() {
        let prev = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string(), "api.example.com".to_string()],
        }];
        let curr = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }];
        let d = compute_capability_delta(&prev, &curr);
        assert!(d.narrowed.is_empty());
    }

    #[test]
    fn delta_spawn_depth_0_to_3_is_not_broadening() {
        let prev = vec![Capability::AgentSpawn {
            max_children: 5,
            max_spawn_depth: 0,
        }];
        let curr = vec![Capability::AgentSpawn {
            max_children: 5,
            max_spawn_depth: 3,
        }];
        let d = compute_capability_delta(&prev, &curr);
        assert!(
            d.broadened.is_empty(),
            "0→3 should not be broadening (0 = system default)"
        );
        assert!(!d.has_broadening());
    }

    #[test]
    fn delta_spawn_depth_3_to_5_is_broadening() {
        let prev = vec![Capability::AgentSpawn {
            max_children: 5,
            max_spawn_depth: 3,
        }];
        let curr = vec![Capability::AgentSpawn {
            max_children: 5,
            max_spawn_depth: 5,
        }];
        let d = compute_capability_delta(&prev, &curr);
        assert_eq!(d.broadened.len(), 1);
        assert!(d.has_broadening());
    }

    #[test]
    fn all_capability_kind_names_matches_capability_type_name() {
        // Pin: every value `capability_type_name` produces for a real
        // `Capability` variant must appear in `all_capability_kind_names()`.
        // Adding a new variant without updating the list (used by
        // ImproveConfig validation) would silently let typos through;
        // this test fails loudly instead.
        use std::collections::HashSet;
        let known: HashSet<&str> = all_capability_kind_names().iter().copied().collect();
        let samples = vec![
            Capability::SandboxFunctions { allowed: vec![] },
            Capability::ReadAccess { scopes: vec![] },
            Capability::WriteAccess { scopes: vec![] },
            Capability::NetworkAccess { hosts: vec![] },
            Capability::AgentSpawn {
                max_children: 1,
                max_spawn_depth: 0,
            },
            Capability::AgentMessage { patterns: vec![] },
            Capability::BackgroundReevaluation {
                min_interval_secs: 1,
                allow_reasoning: false,
            },
            Capability::CodeExecution {
                patterns: vec![],
                commands: vec![],
            },
            Capability::ArtifactExecution,
            Capability::EmergencyStop,
            Capability::AgentRevision { patterns: vec![] },
            Capability::Evaluation { patterns: vec![] },
            Capability::ApprovalQueue { patterns: vec![] },
            Capability::SchedulerSignal { patterns: vec![] },
            Capability::CredentialAccess { services: vec![] },
            Capability::UserProfileAccess { scopes: vec![] },
            Capability::SchedulerAccess { patterns: vec![] },
            Capability::SkillInstall {
                allowed_sources: vec![],
            },
            Capability::ConstitutionalProposal { patterns: vec![] },
            Capability::ReasoningAudit { targets: vec![] },
            Capability::GithubIssueCreate { patterns: vec![] },
            Capability::BudgetNoPriceAvailableAllow,
            Capability::SecurityRedTeam,
            Capability::CapsuleExport,
            Capability::PlanFrameAccess { patterns: vec![] },
            Capability::WikiContribute,
            Capability::PromoteWith {
                agent_id: "agent.test".into(),
                capabilities: vec![Capability::ReadAccess {
                    scopes: vec!["self.*".into()],
                }],
            },
            Capability::GateDecider { kinds: vec![] },
        ];
        for cap in &samples {
            let name = capability_type_name(cap);
            assert!(
                known.contains(name.as_str()),
                "capability_type_name() returned '{}' but it's not in all_capability_kind_names() — \
                 add it to keep ImproveConfig high-blast validation honest",
                name
            );
        }
    }

    #[test]
    fn capability_set_covers_equal_and_narrower() {
        let declared = vec![
            Capability::NetworkAccess {
                hosts: vec!["api.example.com".into(), "cdn.example.com".into()],
            },
            Capability::ReadAccess {
                scopes: vec!["self.*".into()],
            },
        ];
        let artifact = vec![
            Capability::NetworkAccess {
                hosts: vec!["api.example.com".into()],
            },
            Capability::ReadAccess {
                scopes: vec!["self.*".into()],
            },
        ];
        assert!(capability_set_covers(&declared, &artifact));
        assert!(!capability_set_covers(&artifact, &declared));
    }

    #[test]
    fn authority_op_wildcard_does_not_grant_approval() {
        let p = vec!["*".to_string()];
        assert!(AuthorityOp::patterns_allow(&p, "planframe.propose"));
        assert!(!AuthorityOp::patterns_allow(&p, "planframe.approve"));
    }

    #[test]
    fn authority_op_prefix_does_not_grant_approval() {
        let p = vec!["planframe.".to_string()];
        assert!(AuthorityOp::patterns_allow(&p, "planframe.propose"));
        assert!(!AuthorityOp::patterns_allow(&p, "planframe.approve"));
    }

    #[test]
    fn authority_op_exact_grant_confers_approval() {
        let p = vec!["planframe.approve".to_string()];
        assert!(AuthorityOp::patterns_allow(&p, "planframe.approve"));
    }

    #[test]
    fn authority_op_empty_patterns_grant_nothing() {
        for bad in [&[][..], &[""][..], &["   "][..], &["."][..]] {
            let p: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert!(!AuthorityOp::patterns_allow(&p, "planframe.propose"));
            assert!(!AuthorityOp::patterns_allow(&p, "planframe.approve"));
        }
    }

    #[test]
    fn authority_op_recognizes_planframe_approve() {
        assert!(AuthorityOp::is_authority_operation("planframe.approve"));
        assert!(!AuthorityOp::is_authority_operation("planframe.propose"));
        assert!(!AuthorityOp::is_authority_operation("planframe.amend"));
    }
}
