use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::config::GatewayConfig;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SessionEscalateTool));
    registry.register(Box::new(SessionSearchTool));
    registry.register(Box::new(SessionSummarizeTool));
}

pub struct SessionEscalateTool;

impl NativeTool for SessionEscalateTool {
    fn name(&self) -> &'static str {
        "session_escalate"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Request help when stuck. Use this when you've tried reasonable approaches but cannot proceed correctly.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "Clear explanation of why you're stuck"
                    },
                    "context": {
                        "type": "string",
                        "description": "Relevant context: what you tried, what failed, error messages"
                    },
                    "target": {
                        "type": "string",
                        "enum": ["reasoning_llm", "specialist", "human"],
                        "default": "reasoning_llm",
                        "description": "Who to ask for help"
                    },
                    "urgency": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "default": "medium"
                    },
                    "suggested_actions": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Possible next steps you're considering (helps target respond better)"
                    }
                },
                "required": ["reason", "context"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            reason: String,
            context: String,
            #[serde(default = "default_target")]
            target: String,
            #[serde(default = "default_urgency")]
            urgency: String,
            #[serde(default)]
            suggested_actions: Option<Vec<String>>,
        }

        fn default_target() -> String {
            "reasoning_llm".to_string()
        }

        fn default_urgency() -> String {
            "medium".to_string()
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let workflow_id = session_id
            .map(|sid| {
                let root = crate::runtime::content_store::root_session_id(sid);
                let agents_dir = agent_dir.parent().unwrap_or(agent_dir);
                let fallback_config = GatewayConfig {
                    agents_dir: agents_dir.to_path_buf(),
                    ..GatewayConfig::default()
                };
                let gw_config = config.unwrap_or(&fallback_config);
                crate::scheduler::resolve_workflow_id_for_root_session(gw_config, &root)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "unknown".to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());

        let suggested_actions = args.suggested_actions.clone().unwrap_or_default();

        let mut response = match args.target.as_str() {
            "reasoning_llm" => {
                serde_json::json!({
                    "escalation_type": "reasoning_llm",
                    "analysis": format!(
                        "Based on your situation:\n\nProblem: {}\n\nContext: {}\n\nSuggestions:\n1. Review your assumptions - check if you're working with correct data/parameters\n2. Break down the problem into smaller steps\n3. Consider alternative approaches you may have overlooked",
                        args.reason, args.context
                    ),
                    "confidence": "medium",
                    "next_steps": suggested_actions.clone()
                })
            }
            "specialist" => {
                serde_json::json!({
                    "escalation_type": "specialist",
                    "message": "To escalate to a specialist agent, use agent.spawn() with the appropriate specialist (e.g., 'researcher.default', 'architect.default', 'debugger.default')",
                    "suggested_specialists": [
                        "researcher.default - for information gathering and analysis",
                        "architect.default - for structural design and planning",
                        "debugger.default - for troubleshooting and root cause analysis",
                        "evaluator.default - for testing and validation",
                        "auditor.default - for security and compliance review"
                    ],
                    "original_reason": args.reason,
                    "original_context": args.context
                })
            }
            "human" => {
                let Some(store) = gateway_store.as_ref() else {
                    return Err(anyhow::anyhow!(
                        "GatewayStore is required for human escalation approval"
                    ));
                };
                let sid = session_id.ok_or_else(|| {
                    anyhow::anyhow!("session_id is required for human escalation")
                })?;
                let root_session_id =
                    crate::runtime::content_store::root_session_id(sid).to_string();
                let request_id =
                    format!("esc-{}", uuid::Uuid::new_v4().to_string()[..8].to_string());
                let action = autonoetic_types::background::ScheduledAction::SessionEscalate {
                    session_id: sid.to_string(),
                    root_session_id: root_session_id.clone(),
                    requested_by_agent_id: manifest.agent.id.clone(),
                    reason: args.reason.clone(),
                    context: args.context.clone(),
                    urgency: args.urgency.clone(),
                    suggested_actions: suggested_actions.clone(),
                    payload: None,
                };
                let fallback_config = GatewayConfig {
                    agents_dir: agent_dir.parent().unwrap_or(agent_dir).to_path_buf(),
                    ..GatewayConfig::default()
                };
                let gw_config = config.unwrap_or(&fallback_config);
                let wf_id = crate::scheduler::resolve_workflow_id_for_root_session(
                    gw_config,
                    &root_session_id,
                )
                .ok()
                .flatten();
                let task_id = wf_id.as_ref().and_then(|wf| {
                    crate::scheduler::resolve_task_id_for_session(gw_config, None, wf, sid)
                        .ok()
                        .flatten()
                });
                let approval_level =
                    crate::scheduler::approval::resolve_approval_level(gw_config, &action);
                let mut request = autonoetic_types::background::ApprovalRequest {
                    request_id: request_id.clone(),
                    agent_id: manifest.agent.id.clone(),
                    session_id: sid.to_string(),
                    action: action.clone(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                    reason: Some(format!(
                        "Agent '{}' is stuck and needs human guidance. Urgency: {}. Reason: {}",
                        manifest.agent.id, args.urgency, args.reason
                    )),
                    evidence_ref: None,
                    root_session_id: Some(root_session_id),
                    workflow_id: wf_id.clone(),
                    task_id,
                    status: None,
                    decided_at: None,
                    decided_by: None,
                    decision_reason: None,
                    approval_level,
                    similar_to_request_id: None,
                    similarity_score: None,
                    min_dwell_ms: None,
                    confirm_phrase: None,
                };
                store.create_approval(&mut request)?;

                serde_json::json!({
                    "escalation_type": "human",
                    "message": "Escalation logged. A human operator will review your request.",
                    "urgency": args.urgency,
                    "reason": args.reason,
                    "context": args.context,
                    "suggested_actions": suggested_actions.clone(),
                    "escalation_required": true,
                    "request_id": request_id.clone(),
                    "note": "The session is suspended pending operator approval. Do not continue executing tools."
                })
            }
            _ => {
                serde_json::json!({
                    "error": "Unknown escalation target",
                    "valid_targets": ["reasoning_llm", "specialist", "human"]
                })
            }
        };

        let event = autonoetic_types::workflow::WorkflowEventRecord {
            event_id: format!("esc-{}", uuid::Uuid::new_v4()),
            workflow_id: workflow_id.clone(),
            task_id: None,
            event_type: "workflow.escalated".to_string(),
            agent_id: Some(manifest.agent.id.clone()),
            payload: serde_json::json!({
                "target": args.target,
                "urgency": args.urgency,
                "reason": args.reason,
                "context": args.context,
                "suggested_actions": suggested_actions,
            }),
            occurred_at: chrono::Utc::now().to_rfc3339(),
        };
        let _ = crate::scheduler::workflow_store::append_workflow_event(
            config.unwrap_or(&GatewayConfig::default()),
            gateway_store.as_deref(),
            &event,
        );

        response["escalation_id"] = serde_json::json!(event.event_id);
        response["workflow_id"] = serde_json::json!(workflow_id);

        serde_json::to_string(&response).map_err(Into::into)
    }
}

pub struct SessionSearchTool;

impl NativeTool for SessionSearchTool {
    fn name(&self) -> &'static str {
        "session_search"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search across session transcripts using full-text search. Returns matching sessions ranked by relevance.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Full-text search query (FTS5 MATCH syntax)"
                    },
                    "agent_id": {
                        "type": "string",
                        "description": "Filter by agent ID"
                    },
                    "root_session_id": {
                        "type": "string",
                        "description": "Filter by root session (workflow) ID"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["completed", "suspended", "failed"],
                        "description": "Filter by session status"
                    },
                    "since": {
                        "type": "string",
                        "description": "Filter sessions started after this ISO 8601 timestamp"
                    },
                    "limit": {
                        "type": "integer",
                        "default": 20,
                        "minimum": 1,
                        "maximum": 100,
                        "description": "Max results to return"
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            query: Option<String>,
            agent_id: Option<String>,
            root_session_id: Option<String>,
            status: Option<String>,
            since: Option<String>,
            #[serde(default = "default_limit")]
            limit: i64,
        }

        fn default_limit() -> i64 {
            20
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::json!({
                "error": "Gateway store not available"
            })
            .to_string());
        };

        let skill_path = agent_dir.join("SKILL.md");
        let manifest = if skill_path.exists() {
            let content = std::fs::read_to_string(&skill_path)?;
            let (m, _) = crate::runtime::parser::SkillParser::parse(&content)?;
            m
        } else {
            return Ok(serde_json::json!({
                "error": "Agent manifest not found"
            })
            .to_string());
        };

        let caller_id = &manifest.agent.id;

        let (effective_agent_id, effective_root) = enforce_search_acl(
            caller_id,
            args.agent_id.as_deref(),
            args.root_session_id.as_deref(),
            session_id,
            store.as_ref(),
        )?;

        let results = store.search_session_transcripts(
            args.query.as_deref(),
            effective_agent_id.as_deref(),
            effective_root.as_deref(),
            args.status.as_deref(),
            args.since.as_deref(),
            args.limit,
        )?;

        let output: Vec<serde_json::Value> = results
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "session_id": r.session_id,
                    "root_session_id": r.root_session_id,
                    "agent_id": r.agent_id,
                    "status": r.status,
                    "turn_count": r.turn_count,
                    "started_at": r.started_at,
                    "ended_at": r.ended_at,
                    "excerpt": r.excerpt,
                    "transcript_handle": r.transcript_handle,
                })
            })
            .collect();

        Ok(serde_json::json!({
            "results": output,
            "count": output.len(),
        })
        .to_string())
    }
}

pub struct SessionSummarizeTool;

impl NativeTool for SessionSummarizeTool {
    fn name(&self) -> &'static str {
        "session_peek"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Read the raw transcript of a past or current session. Returns turn counts, role breakdown, and a truncated text excerpt. Accepts either a transcript_handle or a session_id.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "transcript_handle": {
                        "type": "string",
                        "description": "Handle of the session transcript or session_id to read"
                    },
                    "max_length": {
                        "type": "integer",
                        "default": 500,
                        "minimum": 50,
                        "maximum": 5000,
                        "description": "Maximum summary length in characters"
                    }
                },
                "required": ["transcript_handle"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            transcript_handle: String,
            #[serde(
                default = "default_max_length",
                deserialize_with = "crate::runtime::tools::deserialize_usize_lenient"
            )]
            max_length: usize,
        }

        fn default_max_length() -> usize {
            500
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(gw_dir) = gateway_dir else {
            return Ok(serde_json::json!({
                "error": "Gateway directory not available"
            })
            .to_string());
        };

        let Some(store) = gateway_store else {
            return Ok(serde_json::json!({
                "error": "Gateway store not available"
            })
            .to_string());
        };

        let skill_path = agent_dir.join("SKILL.md");
        let manifest = if skill_path.exists() {
            let content = std::fs::read_to_string(&skill_path)?;
            let (m, _) = crate::runtime::parser::SkillParser::parse(&content)?;
            m
        } else {
            return Ok(serde_json::json!({
                "error": "Agent manifest not found"
            })
            .to_string());
        };

        let caller_id = &manifest.agent.id;

        let transcript_record = store
            .find_transcript_by_handle(&args.transcript_handle)?
            .or_else(|| {
                store
                    .find_transcript_by_session_id(&args.transcript_handle)
                    .ok()
                    .flatten()
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Transcript not found for handle or session_id: {}",
                    args.transcript_handle
                )
            })?;

        enforce_peek_acl(
            caller_id,
            &transcript_record.agent_id,
            &transcript_record.root_session_id,
            session_id,
        )?;

        let content_store = crate::runtime::content_store::ContentStore::new(gw_dir)?;
        let handle = transcript_record
            .transcript_handle
            .clone()
            .unwrap_or(args.transcript_handle.clone());
        let bytes = content_store.read(&handle)?;
        let messages: Vec<crate::llm::Message> = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("Failed to parse transcript: {}", e))?;

        let excerpt = crate::runtime::lifecycle::extract_searchable_excerpt(&messages);
        let summary = if excerpt.len() > args.max_length {
            format!("{}...", &excerpt[..args.max_length])
        } else {
            excerpt
        };

        let turn_count = messages.len();
        let user_turns = messages
            .iter()
            .filter(|m| matches!(m.role, crate::llm::Role::User))
            .count();
        let assistant_turns = messages
            .iter()
            .filter(|m| matches!(m.role, crate::llm::Role::Assistant))
            .count();
        let tool_turns = messages
            .iter()
            .filter(|m| matches!(m.role, crate::llm::Role::Tool))
            .count();

        Ok(serde_json::json!({
            "summary": summary,
            "turn_count": turn_count,
            "user_turns": user_turns,
            "assistant_turns": assistant_turns,
            "tool_turns": tool_turns,
            "transcript_handle": args.transcript_handle,
        })
        .to_string())
    }
}

fn enforce_search_acl(
    caller_id: &str,
    requested_agent_id: Option<&str>,
    requested_root: Option<&str>,
    current_session_id: Option<&str>,
    _store: &crate::scheduler::gateway_store::GatewayStore,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let caller_root = current_session_id
        .map(|sid| crate::runtime::content_store::root_session_id(sid).to_string());

    if let Some(aid) = requested_agent_id {
        if aid != caller_id {
            let default_root = caller_root.clone();
            return Ok((
                Some(caller_id.to_string()),
                requested_root.map(|s| s.to_string()).or(default_root),
            ));
        }
    }

    let default_root = caller_root.clone();
    let effective_root = requested_root.map(|s| s.to_string()).or(default_root);

    if let Some(ref root) = effective_root {
        if let Some(ref cr) = caller_root {
            if root != cr {
                return Ok((Some(caller_id.to_string()), Some(cr.clone())));
            }
        }
    }

    Ok((requested_agent_id.map(|s| s.to_string()), effective_root))
}

fn enforce_peek_acl(
    caller_id: &str,
    transcript_agent_id: &str,
    transcript_root: &str,
    current_session_id: Option<&str>,
) -> anyhow::Result<()> {
    if transcript_agent_id == caller_id {
        return Ok(());
    }

    if let Some(sid) = current_session_id {
        let caller_root = crate::runtime::content_store::root_session_id(sid);
        if transcript_root == &caller_root[..] {
            return Ok(());
        }
    }

    return Err(autonoetic_types::tool_error::tagged::Tagged::permission(anyhow::anyhow!(
        "Access denied: transcript belongs to agent '{}' (root '{}'), caller '{}' cannot access it",
        transcript_agent_id,
        transcript_root,
        caller_id
    )).into());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store() -> std::sync::Arc<crate::scheduler::gateway_store::GatewayStore> {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().to_path_buf();
        std::mem::forget(temp);
        std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&path).expect("store"),
        )
    }

    #[test]
    fn search_acl_defaults_to_caller_root() {
        let store = make_store();
        let (agent_id, root) = enforce_search_acl(
            "agent-a",
            None,
            None,
            Some("root-123/child-456"),
            store.as_ref(),
        )
        .unwrap();
        assert_eq!(agent_id, None);
        assert_eq!(root.as_deref(), Some("root-123"));
    }

    #[test]
    fn search_acl_rewrites_foreign_agent_to_caller() {
        let store = make_store();
        let (agent_id, _root) = enforce_search_acl(
            "agent-a",
            Some("agent-b"),
            None,
            Some("root-123/child-456"),
            store.as_ref(),
        )
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("agent-a"));
    }

    #[test]
    fn search_acl_rewrites_foreign_root_to_caller_root() {
        let store = make_store();
        let (agent_id, _root) = enforce_search_acl(
            "agent-a",
            None,
            Some("root-other"),
            Some("root-123/child-456"),
            store.as_ref(),
        )
        .unwrap();
        assert_eq!(agent_id.as_deref(), Some("agent-a"));
        assert_eq!(
            _root.as_deref(),
            Some("root-123"),
            "should be caller's root, not caller's agent id"
        );
    }

    #[test]
    fn search_acl_allows_own_root() {
        let store = make_store();
        let (agent_id, root) = enforce_search_acl(
            "agent-a",
            None,
            Some("root-123"),
            Some("root-123/child-456"),
            store.as_ref(),
        )
        .unwrap();
        assert_eq!(agent_id, None);
        assert_eq!(root.as_deref(), Some("root-123"));
    }

    #[test]
    fn peek_acl_allows_own_agent() {
        assert!(
            enforce_peek_acl("agent-a", "agent-a", "root-123", Some("root-123/child"),).is_ok()
        );
    }

    #[test]
    fn peek_acl_allows_child_under_same_root() {
        assert!(
            enforce_peek_acl("agent-a", "agent-b", "root-123", Some("root-123/child"),).is_ok()
        );
    }

    #[test]
    fn peek_acl_denies_different_root() {
        assert!(
            enforce_peek_acl("agent-a", "agent-b", "root-other", Some("root-123/child"),).is_err()
        );
    }

    #[test]
    fn peek_acl_denies_no_session_context() {
        assert!(enforce_peek_acl("agent-a", "agent-b", "root-other", None,).is_err());
    }
}
