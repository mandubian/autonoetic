use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use crate::scheduler::gateway_store::admin_proposals::AdminProposal;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::notification::{NotificationRecord, NotificationType};
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AdminProposalCreateTool));
    registry.register(Box::new(AdminProposalListTool));
}

struct AdminProposalCreateTool;

impl NativeTool for AdminProposalCreateTool {
    fn name(&self) -> &'static str {
        "admin_proposal_create"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::ApprovalQueue { patterns }
                if patterns.iter().any(|p| p == "*" || p.starts_with("admin.proposal")))
        })
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Create a feature-evolution proposal for admin review. Surfaces systemic gaps that cannot be fixed by agent tuning alone.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "maxLength": 200, "description": "Short title for the proposal" },
                    "category": { "type": "string", "enum": ["capability", "tool", "protocol", "ux", "agent"], "description": "Category of the gap" },
                    "evidence": { "type": "object", "description": "Cross-session pattern evidence (structured JSON)" },
                    "remediation": { "type": "string", "description": "Suggested fix" },
                    "blast_radius": { "type": "string", "enum": ["low", "medium", "high"], "description": "Estimated impact radius" },
                    "priority": { "type": "string", "enum": ["low", "medium", "high", "critical"], "description": "Priority level (default: medium)" }
                },
                "required": ["title", "category", "evidence", "remediation", "blast_radius"],
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
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            title: String,
            category: String,
            evidence: serde_json::Value,
            remediation: String,
            blast_radius: String,
            #[serde(default = "default_priority")]
            priority: String,
        }
        fn default_priority() -> String {
            "medium".to_string()
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON for '{}': {}", self.name(), e))?;

        let valid_categories = ["capability", "tool", "protocol", "ux", "agent"];
        if !valid_categories.contains(&args.category.as_str()) {
            return Ok(ToolError::validation(
                format!("category must be one of: {}", valid_categories.join(", ")),
                None::<String>,
            )
            .to_error_response());
        }
        let valid_blast = ["low", "medium", "high"];
        if !valid_blast.contains(&args.blast_radius.as_str()) {
            return Ok(ToolError::validation(
                format!("blast_radius must be one of: {}", valid_blast.join(", ")),
                None::<String>,
            )
            .to_error_response());
        }
        let valid_priority = ["low", "medium", "high", "critical"];
        if !valid_priority.contains(&args.priority.as_str()) {
            return Ok(ToolError::validation(
                format!("priority must be one of: {}", valid_priority.join(", ")),
                None::<String>,
            )
            .to_error_response());
        }

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("GatewayStore not available", None::<String>)
                    .to_error_response(),
            );
        };

        let existing = store.find_open_proposals_by_title_category(&args.title, &args.category)?;
        if let Some(dup) = existing.first() {
            let mut merged = dup.evidence_json.clone();
            if let serde_json::Value::Array(ref mut arr) = merged {
                if let serde_json::Value::Object(map) = args.evidence {
                    arr.push(serde_json::Value::Object(map));
                }
            } else {
                merged = json!([dup.evidence_json.clone(), args.evidence]);
            }
            let updated = AdminProposal {
                proposal_id: dup.proposal_id.clone(),
                title: dup.title.clone(),
                category: dup.category.clone(),
                evidence_json: merged,
                remediation: args.remediation,
                blast_radius: dup.blast_radius.clone(),
                priority: args.priority,
                created_by: dup.created_by.clone(),
                created_at: dup.created_at.clone(),
                status: dup.status.clone(),
                triaged_by: dup.triaged_by.clone(),
                triaged_at: dup.triaged_at.clone(),
                decision_reason: dup.decision_reason.clone(),
            };
            store.upsert_admin_proposal(&updated)?;
            return Ok(json!({
                "ok": true,
                "proposal_id": dup.proposal_id,
                "deduped": true,
                "message": "Merged evidence into existing open proposal"
            })
            .to_string());
        }

        let proposal_id = autonoetic_types::id_format::short_random_id_hex("prop-", 12);
        let now = chrono::Utc::now().to_rfc3339();
        let proposal = AdminProposal {
            proposal_id: proposal_id.clone(),
            title: args.title,
            category: args.category,
            evidence_json: args.evidence,
            remediation: args.remediation,
            blast_radius: args.blast_radius,
            priority: args.priority,
            created_by: manifest.agent.id.clone(),
            created_at: now,
            status: "open".to_string(),
            triaged_by: None,
            triaged_at: None,
            decision_reason: None,
        };

        store.insert_admin_proposal(&proposal)?;

        let notification = NotificationRecord::new(
            autonoetic_types::id_format::short_random_id("ntf-"),
            NotificationType::AdminProposal,
            "system".to_string(),
            json!({
                "proposal_id": &proposal_id,
                "title": &proposal.title,
                "category": &proposal.category,
                "priority": &proposal.priority,
            }),
        );
        if let Err(e) = store.create_notification_record(&notification) {
            tracing::warn!("Failed to create admin proposal notification: {}", e);
        }

        Ok(json!({
            "ok": true,
            "proposal_id": proposal_id,
            "deduped": false
        })
        .to_string())
    }
}

struct AdminProposalListTool;

impl NativeTool for AdminProposalListTool {
    fn name(&self) -> &'static str {
        "admin_proposal_list"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::ReadAccess { scopes }
                if scopes.iter().any(|s| s == "*" || s.starts_with("admin_")))
        })
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List admin proposals filtered by status and/or category.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "Filter by status: open, triaged, accepted, rejected, implemented" },
                    "category": { "type": "string", "description": "Filter by category: capability, tool, protocol, ux, agent" },
                    "limit": { "type": "integer", "description": "Max results (default 50, max 200)", "default": 50 }
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            status: Option<String>,
            category: Option<String>,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        fn default_limit() -> usize {
            50
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("GatewayStore not available", None::<String>)
                    .to_error_response(),
            );
        };

        let limit = args.limit.min(200);
        let proposals =
            store.list_admin_proposals(args.status.as_deref(), args.category.as_deref(), limit)?;

        let items: Vec<serde_json::Value> = proposals
            .iter()
            .map(|p| {
                json!({
                    "proposal_id": p.proposal_id,
                    "title": p.title,
                    "category": p.category,
                    "blast_radius": p.blast_radius,
                    "priority": p.priority,
                    "created_by": p.created_by,
                    "created_at": p.created_at,
                    "status": p.status,
                })
            })
            .collect();

        Ok(json!({
            "ok": true,
            "proposals": items,
            "count": items.len()
        })
        .to_string())
    }
}
