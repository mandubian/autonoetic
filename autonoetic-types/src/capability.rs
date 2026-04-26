//! Capability enums for agent permission declarations.
//!
//! Capability categories:
//! - **SandboxFunctions**: MCP tool access by prefix (web.*, sandbox.*)
//! - **ReadAccess**: Read content, memory, knowledge (includes search)
//! - **WriteAccess**: Write content, memory, knowledge (includes share)
//! - **CodeExecution**: Execute scripts in sandbox
//! - **NetworkAccess**: Make HTTP requests
//! - **AgentSpawn**: Create child agent sessions
//! - **AgentMessage**: Send messages to other agents
//! - **BackgroundReevaluation**: Periodic wake-ups

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A typed capability that an Agent may request.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    AgentSpawn { max_children: u32 },

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

pub fn compute_capability_delta(previous: &[Capability], current: &[Capability]) -> CapabilityDelta {
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
        Capability::EmergencyStop => "EmergencyStop".to_string(),
        Capability::AgentRevision { .. } => "AgentRevision".to_string(),
        Capability::Evaluation { .. } => "Evaluation".to_string(),
        Capability::ApprovalQueue { .. } => "ApprovalQueue".to_string(),
        Capability::SchedulerSignal { .. } => "SchedulerSignal".to_string(),
        Capability::CredentialAccess { .. } => "CredentialAccess".to_string(),
        Capability::UserProfileAccess { .. } => "UserProfileAccess".to_string(),
        Capability::SchedulerAccess { .. } => "SchedulerAccess".to_string(),
        Capability::SkillInstall { .. } => "SkillInstall".to_string(),
    }
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
        (Capability::SandboxFunctions { allowed: a }, Capability::SandboxFunctions { allowed: b }) => {
            scope_broadening(capability_type, a, b)
        }
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
        (Capability::SchedulerSignal { patterns: a }, Capability::SchedulerSignal { patterns: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (Capability::SchedulerAccess { patterns: a }, Capability::SchedulerAccess { patterns: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (Capability::CredentialAccess { services: a }, Capability::CredentialAccess { services: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (Capability::UserProfileAccess { scopes: a }, Capability::UserProfileAccess { scopes: b }) => {
            scope_broadening(capability_type, a, b)
        }
        (Capability::SkillInstall { allowed_sources: a }, Capability::SkillInstall { allowed_sources: b }) => {
            scope_broadening(capability_type, a, b)
        }
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
            },
            Capability::AgentSpawn {
                max_children: max_b,
            },
        ) => {
            if max_b > max_a {
                Some(CapabilityBroadening {
                    capability_type: capability_type.to_string(),
                    previous_scope: vec![max_a.to_string()],
                    new_scope: vec![max_b.to_string()],
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
        (Capability::SandboxFunctions { allowed: a }, Capability::SandboxFunctions { allowed: b }) => {
            is_scope_narrowed(a, b)
        }
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
}
