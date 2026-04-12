use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionClosed,
    SessionSuspended,
    ApprovalResolved,
    ApprovalRequested,
    WorkflowJoinSatisfied,
    ArtifactCreated,
    AgentPromoted,
    EmergencyStop,
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
}
