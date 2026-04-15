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
    /// The `patterns` field limits which commands can be run.
    CodeExecution {
        #[serde(default = "default_patterns_all")]
        patterns: Vec<String>,
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
