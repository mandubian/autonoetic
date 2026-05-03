//! User profile tools — user.profile.read, user.profile.update, user.profile.share, user.profile.revoke.
//!
//! Agents use these tools to read and manage user profiles for cross-session personalization.
//! Access is controlled by the `UserProfileAccess` capability and user-agent bindings.

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::{AgentManifest, BindingScope, UserProfileRecord};
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(UserProfileReadTool));
    registry.register(Box::new(UserProfileUpdateTool));
    registry.register(Box::new(UserProfileShareTool));
    registry.register(Box::new(UserProfileRevokeTool));
}

// ---------------------------------------------------------------------------
// user.profile.read
// ---------------------------------------------------------------------------

struct UserProfileReadTool;

impl NativeTool for UserProfileReadTool {
    fn name(&self) -> &'static str {
        "user_profile_read"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "user_profile_read".to_string(),
            description: "Read a user's profile. Defaults to the bound user for the current agent. Respects binding scope: 'full' returns all profile data, 'restricted' returns only preferences and constraints, 'task_only' returns nothing.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "string",
                        "description": "User ID to read. Defaults to the agent's bound user if omitted."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::UserProfileAccess { .. }))
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
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            user_id: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)?;

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("Gateway store not available", None::<String>)
                    .to_error_response(),
            );
        };

        let agent_id = &manifest.agent.id;
        let user_id = args.user_id.unwrap_or_else(|| {
            run_context
                .and_then(|ctx| ctx.user_id.clone())
                .unwrap_or_default()
        });

        if user_id.is_empty() {
            return Ok(ToolError::validation(
                "No user_id provided and no bound user found for this agent",
                Some("Provide a user_id or ensure the agent has a bound user.".to_string()),
            )
            .to_error_response());
        }

        // Check binding
        let binding = store.get_user_binding(&user_id, agent_id)?;
        let Some(binding) = binding else {
            return Ok(ToolError::not_found(
                format!(
                    "binding between user '{}' and agent '{}'",
                    user_id, agent_id
                ),
                Some("Use user.profile.share to request access.".to_string()),
            )
            .to_error_response());
        };

        if binding.scope == BindingScope::TaskOnly {
            return Ok(json!({
                "ok": true,
                "user_id": user_id,
                "scope": "task_only",
                "profile": null,
                "message": "Binding exists with task_only scope — no profile data is accessible"
            })
            .to_string());
        }

        let profile = store.get_user_profile(&user_id)?;
        let Some(profile) = profile else {
            return Ok(json!({
                "ok": true,
                "user_id": user_id,
                "scope": binding.scope.to_string(),
                "profile": null,
                "display_name": null,
                "message": "No profile has been created for this user yet"
            })
            .to_string());
        };

        let profile_data = match &profile.profile_json {
            Some(json_str) => {
                let parsed: serde_json::Value =
                    serde_json::from_str(json_str).unwrap_or(json!(null));
                if binding.scope == BindingScope::Restricted {
                    // Extract only preferences and constraints
                    filter_restricted_profile(&parsed)
                } else {
                    parsed
                }
            }
            None => serde_json::Value::Null,
        };

        Ok(json!({
            "ok": true,
            "user_id": user_id,
            "scope": binding.scope.to_string(),
            "display_name": profile.display_name,
            "profile": profile_data,
            "version": profile.profile_version
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// user.profile.update
// ---------------------------------------------------------------------------

struct UserProfileUpdateTool;

impl NativeTool for UserProfileUpdateTool {
    fn name(&self) -> &'static str {
        "user_profile_update"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "user_profile_update".to_string(),
            description: "Update a user's profile data. Requires UserProfileAccess with 'write' scope. Creates the profile if it doesn't exist. Merges the provided fields into the existing profile.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "string",
                        "description": "User ID to update. Defaults to the agent's bound user if omitted."
                    },
                    "display_name": {
                        "type": "string",
                        "description": "Update the user's display name"
                    },
                    "profile_data": {
                        "type": "object",
                        "description": "JSON object to merge into the existing profile. Keys replace existing values; new keys are added."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::UserProfileAccess { scopes } if scopes.iter().any(|s| s == "write" || s == "*"))
        })
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
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            user_id: Option<String>,
            display_name: Option<String>,
            profile_data: Option<serde_json::Value>,
        }
        let args: Args = serde_json::from_str(arguments_json)?;

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("Gateway store not available", None::<String>)
                    .to_error_response(),
            );
        };

        let agent_id = &manifest.agent.id;
        let user_id = args.user_id.unwrap_or_else(|| {
            run_context
                .and_then(|ctx| ctx.user_id.clone())
                .unwrap_or_default()
        });

        if user_id.is_empty() {
            return Ok(ToolError::validation(
                "No user_id provided and no bound user found",
                Some("Provide a user_id or ensure the agent has a bound user.".to_string()),
            )
            .to_error_response());
        }

        // Verify binding exists (agents can only update bound users)
        let binding = store.get_user_binding(&user_id, agent_id)?;
        if binding.is_none() {
            return Ok(ToolError::not_found(
                format!(
                    "binding between user '{}' and agent '{}'",
                    user_id, agent_id
                ),
                Some("Use user.profile.share to request access.".to_string()),
            )
            .to_error_response());
        }

        let now = chrono::Utc::now().to_rfc3339();

        // Load existing profile or create new one
        let existing = store.get_user_profile(&user_id)?;
        let (mut current_json, mut version) = match &existing {
            Some(p) => {
                let parsed: serde_json::Value = p
                    .profile_json
                    .as_ref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(json!({}));
                (parsed, p.profile_version)
            }
            None => (json!({}), 0),
        };

        // Merge provided data
        if let Some(data) = args.profile_data {
            if let Some(obj) = data.as_object() {
                if let Some(current_obj) = current_json.as_object_mut() {
                    for (k, v) in obj {
                        current_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        version += 1;
        let profile = UserProfileRecord {
            user_id: user_id.clone(),
            display_name: args
                .display_name
                .or_else(|| existing.as_ref().and_then(|p| p.display_name.clone())),
            trust_domain: existing
                .as_ref()
                .map(|p| p.trust_domain.clone())
                .unwrap_or_else(|| "local".to_string()),
            origin_node_id: existing.as_ref().and_then(|p| p.origin_node_id.clone()),
            profile_json: Some(serde_json::to_string(&current_json)?),
            profile_version: version,
            created_at: existing
                .map(|p| p.created_at)
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        };

        store.upsert_user_profile(&profile)?;

        Ok(json!({
            "ok": true,
            "user_id": user_id,
            "version": version
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// user.profile.share
// ---------------------------------------------------------------------------

struct UserProfileShareTool;

impl NativeTool for UserProfileShareTool {
    fn name(&self) -> &'static str {
        "user_profile_share"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "user_profile_share".to_string(),
            description: "Request access to a user's profile. Creates an approval request for the user to approve. Once approved, a binding is created between the agent and the user.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "string",
                        "description": "User ID to request access to"
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["full", "restricted", "task_only"],
                        "default": "restricted",
                        "description": "Requested access scope"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why the agent needs access to this user's profile"
                    }
                },
                "required": ["user_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::UserProfileAccess { .. }))
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            user_id: String,
            #[serde(default = "default_restricted")]
            scope: String,
            reason: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)?;

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("Gateway store not available", None::<String>)
                    .to_error_response(),
            );
        };

        let agent_id = &manifest.agent.id;

        // Check if binding already exists
        let existing = store.get_user_binding(&args.user_id, agent_id)?;
        if existing.is_some() {
            return Ok(json!({
                "ok": true,
                "already_bound": true,
                "scope": existing.unwrap().scope.to_string(),
                "message": "Binding already exists"
            })
            .to_string());
        }

        // Create approval request
        let request_id = format!("profile_share_{}_{}", args.user_id, uuid::Uuid::new_v4());
        let mut approval = ApprovalRequest {
            request_id: request_id.clone(),
            agent_id: agent_id.clone(),
            session_id: session_id.unwrap_or("").to_string(),
            action: ScheduledAction::ProfileShare {
                user_id: args.user_id.clone(),
                agent_id: agent_id.clone(),
                scope: args.scope.clone(),
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            reason: args.reason.clone().or_else(|| {
                Some(format!(
                    "Agent '{}' requests access to user '{}' profile with scope '{}'",
                    agent_id, args.user_id, args.scope
                ))
            }),
            evidence_ref: None,
            root_session_id: None,
            workflow_id: None,
            task_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
        };

        store.create_approval(&mut approval)?;

        Ok(json!({
            "ok": true,
            "approval_required": true,
            "approval_request_id": request_id,
            "user_id": args.user_id,
            "scope": args.scope,
            "message": "Profile share request created. Awaiting user approval."
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// user.profile.revoke
// ---------------------------------------------------------------------------

struct UserProfileRevokeTool;

impl NativeTool for UserProfileRevokeTool {
    fn name(&self) -> &'static str {
        "user_profile_revoke"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "user_profile_revoke".to_string(),
            description: "Revoke an agent's access to a user's profile. Requires UserProfileAccess with 'write' scope.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "user_id": {
                        "type": "string",
                        "description": "User ID whose binding to revoke"
                    },
                    "agent_id": {
                        "type": "string",
                        "description": "Agent ID to revoke. Defaults to the calling agent."
                    }
                },
                "required": ["user_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::UserProfileAccess { scopes } if scopes.iter().any(|s| s == "write" || s == "*"))
        })
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
            user_id: String,
            agent_id: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)?;

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("Gateway store not available", None::<String>)
                    .to_error_response(),
            );
        };

        let target_agent_id = args.agent_id.unwrap_or_else(|| manifest.agent.id.clone());

        let deleted = store.delete_user_binding(&args.user_id, &target_agent_id)?;
        if deleted {
            Ok(json!({
                "ok": true,
                "user_id": args.user_id,
                "agent_id": target_agent_id,
                "message": "Profile binding revoked"
            })
            .to_string())
        } else {
            Ok(ToolError::not_found(
                format!(
                    "binding between user '{}' and agent '{}'",
                    args.user_id, target_agent_id
                ),
                None::<String>,
            )
            .to_error_response())
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_restricted() -> String {
    "restricted".to_string()
}

/// Filter a full profile JSON to only include preferences and constraints.
fn filter_restricted_profile(profile: &serde_json::Value) -> serde_json::Value {
    let mut restricted = serde_json::Map::new();
    if let Some(obj) = profile.as_object() {
        for key in &["preferences", "constraints"] {
            if let Some(val) = obj.get(*key) {
                restricted.insert((*key).to_string(), val.clone());
            }
        }
    }
    serde_json::Value::Object(restricted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_restricted_profile() {
        let full = json!({
            "preferences": {"theme": "dark"},
            "constraints": {"max_budget": 100},
            "personal_notes": "secret stuff",
            "history": []
        });
        let restricted = filter_restricted_profile(&full);
        assert!(restricted.get("preferences").is_some());
        assert!(restricted.get("constraints").is_some());
        assert!(restricted.get("personal_notes").is_none());
        assert!(restricted.get("history").is_none());
    }

    #[test]
    fn test_filter_restricted_profile_empty() {
        let full = json!({"other": "data"});
        let restricted = filter_restricted_profile(&full);
        assert!(restricted.as_object().unwrap().is_empty());
    }
}
