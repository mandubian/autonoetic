use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ApprovalStatusTool));
    registry.register(Box::new(ApprovalWithdrawTool));
}

#[derive(Debug, Deserialize)]
struct ApprovalStatusArgs {
    #[serde(default)]
    request_id: Option<String>,
}

pub struct ApprovalStatusTool;

impl NativeTool for ApprovalStatusTool {
    fn name(&self) -> &'static str {
        "approval.status"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Check the status of approval requests for the current session. Returns pending, approved, and rejected approvals. Use this to discover if an approval is still pending before deciding to withdraw and re-submit.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "request_id": {
                        "type": "string",
                        "description": "Check a specific approval request by ID. If omitted, returns all pending approvals for the current session."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        _arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: ApprovalStatusArgs = serde_json::from_str(_arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "Gateway store not available"
            }).to_string());
        };

        if let Some(rid) = &args.request_id {
            let req = store.get_approval(rid)?;
            return match req {
                Some(r) => Ok(serde_json::json!({
                    "ok": true,
                    "approval": approval_summary(&r),
                }).to_string()),
                None => Ok(serde_json::json!({
                    "ok": false,
                    "error": format!("Approval '{}' not found", rid),
                }).to_string()),
            };
        }

        let sid = session_id.unwrap_or("");
        let root_sid = crate::runtime::content_store::root_session_id(sid);

        let pending = store.get_pending_approvals_for_root(root_sid)?;
        let mine: Vec<_> = pending
            .iter()
            .filter(|r| r.agent_id == manifest.agent.id)
            .map(|r| approval_summary(r))
            .collect();

        let decision_info = match store.get_approved_approvals_for_session(sid) {
            Ok(decided) => decided
                .iter()
                .filter(|r| r.agent_id == manifest.agent.id && r.decided_at.is_some())
                .take(5)
                .map(|r| approval_summary(r))
                .collect(),
            Err(_) => Vec::new(),
        };

        Ok(serde_json::json!({
            "ok": true,
            "pending": mine,
            "recent_decisions": decision_info,
        }).to_string())
    }
}

#[derive(Debug, Deserialize)]
struct ApprovalWithdrawArgs {
    request_id: String,
    reason: Option<String>,
}

pub struct ApprovalWithdrawTool;

impl NativeTool for ApprovalWithdrawTool {
    fn name(&self) -> &'static str {
        "approval.withdraw"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::CodeExecution { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Withdraw a pending approval request that the calling agent created. Use this when new information from the user or environment makes the original request stale or incorrect, then re-submit with updated parameters.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "request_id": {
                        "type": "string",
                        "description": "The approval request ID to withdraw (e.g., 'apr-2f85bc63')"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why the approval is being withdrawn (e.g., 'User provided updated domain list')"
                    }
                },
                "required": ["request_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        _arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: ApprovalWithdrawArgs = serde_json::from_str(_arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "Gateway store not available"
            }).to_string());
        };

        let request = store.get_approval(&args.request_id)?;
        match request {
            None => Ok(serde_json::json!({
                "ok": false,
                "error": format!("Approval '{}' not found", args.request_id),
            }).to_string()),
            Some(r) => {
                if r.agent_id != manifest.agent.id {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error": format!(
                            "Approval '{}' belongs to agent '{}', not '{}'. Can only withdraw your own approvals.",
                            args.request_id, r.agent_id, manifest.agent.id
                        ),
                    }).to_string());
                }
                let status_str = r
                    .status
                    .as_ref()
                    .map(|s| match s {
                        autonoetic_types::background::ApprovalStatus::Approved => "approved",
                        autonoetic_types::background::ApprovalStatus::Rejected => "rejected",
                        autonoetic_types::background::ApprovalStatus::Cancelled => "cancelled",
                    })
                    .unwrap_or("pending");
                if status_str != "pending" {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error": format!("Approval '{}' is already '{}' — only pending approvals can be withdrawn", args.request_id, status_str),
                    }).to_string());
                }

                let reason = args.reason.as_deref().unwrap_or("Withdrawn by agent");
                store.cancel_approval(
                    &args.request_id,
                    &format!("agent:{}", manifest.agent.id),
                    &chrono::Utc::now().to_rfc3339(),
                )?;

                tracing::info!(
                    target: "approval.withdraw",
                    request_id = %args.request_id,
                    agent_id = %manifest.agent.id,
                    reason = %reason,
                    "Agent withdrew approval"
                );

                Ok(serde_json::json!({
                    "ok": true,
                    "request_id": args.request_id,
                    "message": format!("Approval {} withdrawn. You can now re-submit with updated parameters.", args.request_id),
                }).to_string())
            }
        }
    }
}

fn approval_summary(r: &autonoetic_types::background::ApprovalRequest) -> serde_json::Value {
    let action_summary = match &r.action {
        autonoetic_types::background::ScheduledAction::SandboxExec {
            command,
            detected_hosts,
            ..
        } => serde_json::json!({
            "kind": "sandbox_exec",
            "command": command,
            "hosts": detected_hosts,
        }),
        autonoetic_types::background::ScheduledAction::AgentInstall {
            agent_id,
            summary,
            ..
        } => serde_json::json!({
            "kind": "agent_install",
            "agent_id": agent_id,
            "summary": summary,
        }),
        autonoetic_types::background::ScheduledAction::CredentialPrompt {
            service,
            credential_id,
            ..
        } => serde_json::json!({
            "kind": "credential_prompt",
            "service": service,
            "credential_id": credential_id,
        }),
        other => serde_json::json!({
            "kind": "other",
            "detail": format!("{:?}", other),
        }),
    };
    serde_json::json!({
        "request_id": r.request_id,
        "agent_id": r.agent_id,
        "session_id": r.session_id,
        "status": r.status.as_ref().map(|s| format!("{:?}", s).to_lowercase()).unwrap_or_else(|| "pending".to_string()),
        "action": action_summary,
        "reason": r.reason,
        "created_at": r.created_at,
        "decided_at": r.decided_at,
        "decided_by": r.decided_by,
        "decision_reason": r.decision_reason,
    })
}
