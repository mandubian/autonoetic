use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::background::GrantTarget;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum HookEvent {
    #[serde(rename = "session.closed")]
    SessionClosed,
    #[serde(rename = "session.suspended")]
    SessionSuspended,
    #[serde(rename = "approval.resolved")]
    ApprovalResolved,
    #[serde(rename = "approval.requested")]
    ApprovalRequested,
    #[serde(rename = "workflow.join.satisfied")]
    WorkflowJoinSatisfied,
    #[serde(rename = "artifact.created")]
    ArtifactCreated,
    #[serde(rename = "agent.promoted")]
    AgentPromoted,
    #[serde(rename = "emergency_stop")]
    EmergencyStop,
    /// Fired after a row is inserted into `causal_events` when the event matches
    /// gateway policy-hook filters (observer-only).
    #[serde(rename = "policy.decision")]
    PolicyDecision,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionClosed => "session.closed",
            Self::SessionSuspended => "session.suspended",
            Self::ApprovalResolved => "approval.resolved",
            Self::ApprovalRequested => "approval.requested",
            Self::WorkflowJoinSatisfied => "workflow.join.satisfied",
            Self::ArtifactCreated => "artifact.created",
            Self::AgentPromoted => "agent.promoted",
            Self::EmergencyStop => "emergency_stop",
            Self::PolicyDecision => "policy.decision",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HookAction {
    PublishReport,
    DeliverSignal,
    #[serde(rename = "agent.spawn")]
    AgentSpawn,
    #[serde(rename = "http.callback")]
    HttpCallback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    #[serde(rename = "on")]
    pub event: HookEvent,
    pub action: HookAction,
    #[serde(default)]
    pub r#async: bool,
    #[serde(default)]
    pub params: HashMap<String, serde_json::Value>,
    /// Allowlist for `http.callback` destinations. Entries may match the full
    /// URL (`UrlPrefix`) or the parsed authority (`ExactHost`, `HostSuffix`,
    /// `HostAndPort`). Empty means `http.callback` is disabled for this hook.
    #[serde(default)]
    pub callback_allowlist: Vec<GrantTarget>,
    /// Allowlist of agent IDs that may be spawned by an `agent.spawn` hook.
    /// When non-empty the gateway enforces that `params.agent_id` is in this
    /// list before dispatching. An empty list means any agent is allowed.
    #[serde(default)]
    pub allowed_agents: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContext {
    pub event: HookEvent,
    pub root_session_id: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub gateway_dir: Option<String>,
    pub fields: HashMap<String, String>,
}

impl HookContext {
    pub fn for_session_closed(
        root_session_id: &str,
        session_id: &str,
        agent_id: &str,
        close_reason: &str,
        turn_count: u64,
        gateway_dir: Option<&std::path::Path>,
    ) -> Self {
        let mut fields = HashMap::new();
        fields.insert("close_reason".to_string(), close_reason.to_string());
        fields.insert("turn_count".to_string(), turn_count.to_string());
        Self {
            event: HookEvent::SessionClosed,
            root_session_id: root_session_id.to_string(),
            session_id: Some(session_id.to_string()),
            agent_id: Some(agent_id.to_string()),
            gateway_dir: gateway_dir.map(|p| p.to_string_lossy().to_string()),
            fields,
        }
    }

    pub fn for_approval_resolved(
        root_session_id: &str,
        session_id: &str,
        agent_id: &str,
        request_id: &str,
        decision: &str,
    ) -> Self {
        let mut fields = HashMap::new();
        fields.insert("request_id".to_string(), request_id.to_string());
        fields.insert("decision".to_string(), decision.to_string());
        Self {
            event: HookEvent::ApprovalResolved,
            root_session_id: root_session_id.to_string(),
            session_id: Some(session_id.to_string()),
            agent_id: Some(agent_id.to_string()),
            gateway_dir: None,
            fields,
        }
    }

    pub fn for_workflow_join_satisfied(
        root_session_id: &str,
        workflow_id: &str,
        join_task_ids: &[String],
    ) -> Self {
        let mut fields = HashMap::new();
        fields.insert("workflow_id".to_string(), workflow_id.to_string());
        fields.insert("task_ids".to_string(), join_task_ids.join(","));
        Self {
            event: HookEvent::WorkflowJoinSatisfied,
            root_session_id: root_session_id.to_string(),
            session_id: None,
            agent_id: None,
            gateway_dir: None,
            fields,
        }
    }

    /// Context for [`HookEvent::PolicyDecision`] after a causal event row is persisted.
    ///
    /// `fields` keys are stable for `message_template` substitution (same names are also on
    /// `root_session_id` / `session_id` / `agent_id` for structured consumers):
    /// `root_session_id`, `session_id`, `agent_id`, `event_id`, `rule_ids`, `primary_rule_id`,
    /// `decision`, `status`, `category`, `action`, `target`, `reason`, `turn_id`, `source`
    /// (`causal_events`).
    pub fn for_policy_decision(
        root_session_id: &str,
        event: &crate::causal_chain::CausalEventRecord,
    ) -> Self {
        use crate::causal_chain::RULE_ID_EVENT_ATTRIBUTION;

        let rule_ids = event.enforced_rules.join(",");
        let primary_rule_id = event
            .enforced_rules
            .iter()
            .find(|r| r.as_str() != RULE_ID_EVENT_ATTRIBUTION)
            .cloned()
            .or_else(|| event.enforced_rules.first().cloned())
            .unwrap_or_default();

        let status_u = event.status.to_ascii_uppercase();
        let decision = match status_u.as_str() {
            "DENIED" | "ERROR" => "denied",
            "SUCCESS" => "allowed",
            _ => "unknown",
        };

        let reason = event
            .reason
            .as_deref()
            .map(|s| {
                const MAX: usize = 2048;
                if s.chars().count() <= MAX {
                    s.to_string()
                } else {
                    s.chars().take(MAX).collect::<String>()
                }
            })
            .unwrap_or_default();

        let mut fields = HashMap::new();
        fields.insert("root_session_id".to_string(), root_session_id.to_string());
        fields.insert("session_id".to_string(), event.session_id.clone());
        fields.insert("agent_id".to_string(), event.agent_id.clone());
        fields.insert("event_id".to_string(), event.event_id.clone());
        fields.insert("rule_ids".to_string(), rule_ids);
        fields.insert("primary_rule_id".to_string(), primary_rule_id);
        fields.insert("decision".to_string(), decision.to_string());
        fields.insert("status".to_string(), event.status.clone());
        fields.insert("category".to_string(), event.category.clone());
        fields.insert("action".to_string(), event.action.clone());
        fields.insert(
            "target".to_string(),
            event.target.clone().unwrap_or_default(),
        );
        fields.insert("reason".to_string(), reason);
        fields.insert(
            "turn_id".to_string(),
            event.turn_id.clone().unwrap_or_default(),
        );
        fields.insert("source".to_string(), "causal_events".to_string());

        Self {
            event: HookEvent::PolicyDecision,
            root_session_id: root_session_id.to_string(),
            session_id: Some(event.session_id.clone()),
            agent_id: Some(event.agent_id.clone()),
            gateway_dir: None,
            fields,
        }
    }
}
