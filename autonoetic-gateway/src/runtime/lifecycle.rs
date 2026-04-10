//! Agent Execution Lifecycle.
//!
//! Manages Wake -> Context Assembly -> Reasoning -> Act -> Hibernate.

use crate::llm::{CompletionRequest, LlmDriver, Message, StopReason, ToolDefinition};
use crate::policy::PolicyEngine;
use crate::runtime::artifact::extract_artifacts_from_text;
use crate::runtime::checkpoint::{
    prune_checkpoints, save_checkpoint, LlmConfigSnapshot, PendingToolCall, PendingToolState,
    SessionCheckpoint, YieldReason,
};
use crate::runtime::disclosure::DisclosureState;
use crate::runtime::guard::LoopGuard;
use crate::runtime::mcp::McpToolRuntime;
use crate::runtime::openrouter_catalog::OpenRouterCatalog;
use crate::runtime::reevaluation_state::persist_reevaluation_state;
use crate::runtime::session_budget::SessionBudgetRegistry;
use crate::runtime::session_tracer::{EvidenceMode, SessionTracer};
use crate::runtime::store::SecretStoreRuntime;
use crate::runtime::tool_call_processor::ToolCallProcessor;
use autonoetic_types::agent::{AgentManifest, LlmExchangeUsage, Middleware};
use autonoetic_types::background::{ApprovalRequest, ScheduledAction};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::disclosure::DisclosurePolicy;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Foundation Instructions
// ---------------------------------------------------------------------------

const FOUNDATION_CORE: &str = include_str!("foundation_core.md");
const FOUNDATION_WORKFLOW: &str = include_str!("foundation_workflow.md");
const FOUNDATION_ARTIFACT: &str = include_str!("foundation_artifact.md");
const FOUNDATION_SCRIPT: &str = include_str!("foundation_script.md");
const FOUNDATION_DIGEST: &str = include_str!("foundation_digest.md");
const LLM_OTHER_EMPTY_RETRY_ENV: &str = "AUTONOETIC_LLM_OTHER_EMPTY_RETRIES";
const LLM_OTHER_EMPTY_RETRY_DEFAULT: usize = 1;

/// Compose foundation instructions based on agent capabilities and execution mode.
///
/// Always includes core instructions. Adds workflow, artifact, script, and digest
/// layers based on what the agent can actually do.
fn compose_foundation(manifest: &AgentManifest) -> String {
    let mut parts = Vec::new();
    parts.push(FOUNDATION_CORE.trim());

    let has_workflow_caps = manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            autonoetic_types::capability::Capability::AgentSpawn { .. }
        )
    });
    let has_artifact_caps = manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            autonoetic_types::capability::Capability::WriteAccess { .. }
        )
    });
    let is_script_mode = manifest.execution_mode == autonoetic_types::agent::ExecutionMode::Script;
    let has_digest_cap = manifest.capabilities.iter().any(|c| {
        if let autonoetic_types::capability::Capability::WriteAccess { scopes } = c {
            scopes.iter().any(|s| s.starts_with("digest") || s == "*")
        } else {
            false
        }
    });

    if has_workflow_caps || !is_script_mode {
        parts.push(FOUNDATION_WORKFLOW.trim());
    }

    if has_artifact_caps {
        parts.push(FOUNDATION_ARTIFACT.trim());
    }

    if is_script_mode {
        parts.push(FOUNDATION_SCRIPT.trim());
    }

    if has_digest_cap {
        parts.push(FOUNDATION_DIGEST.trim());
    }

    parts.join("\n\n---\n\n")
}

const TOOL_BRIDGING_APPENDIX: &str = r#"---

Tool Compatibility Notes (auto-generated from AgentSkills import)

This skill was imported from the Agent Skills (agentskills.io) format.
The following tool mappings apply:

| Skill references | Autonoetic equivalent |
|---|---|
| `Bash(command)` | `sandbox.exec({"command": "command"})` |
| `Read(path)` | `content.read(name_or_handle)` — files must be loaded via content store |
| `Write(path, content)` | `content.write(name, content)` |
| `WebSearch(query)` | `web.search({"query": "query"})` |
| `WebFetch(url)` | `web.fetch({"url": "url"})` |

File paths referenced by the skill are available in the agent directory.
Use content.read/content.write or sandbox paths relative to the agent working directory."#;

fn tool_bridging_appendix() -> String {
    TOOL_BRIDGING_APPENDIX.to_string()
}

#[derive(Debug, Clone, Default)]
struct SchemaValidation {
    valid: bool,
    messages: Vec<String>,
}

// ---------------------------------------------------------------------------
// TurnOutcome
// ---------------------------------------------------------------------------

/// Result of a single `execute_with_history` call.
#[derive(Debug)]
pub enum TurnOutcome {
    /// The turn completed normally.  Contains the final assistant reply text
    /// (filtered by disclosure policy), or `None` when the turn ended without
    /// producing any text.
    Completed(Option<String>),

    /// The turn was suspended at an approval boundary.  The `TurnContinuation`
    /// has already been saved to disk by `execute_with_history`; the caller
    /// (typically `spawn_task_execution`) should set the task to
    /// `AwaitingApproval` and release the tokio task / claim — no resources
    /// need to be held while waiting for the operator.
    Suspended {
        approval_request_id: String,
        /// The full continuation, when suspension happened mid-tool batch.
        /// `None` means a non-tool approval boundary (e.g. max-turn continuation gate).
        continuation: Option<Box<crate::runtime::continuation::TurnContinuation>>,
    },

    /// The turn was suspended because a user interaction is pending.
    /// The checkpoint has already been saved by `execute_with_history`;
    /// the caller should record this outcome so the session is visible
    /// as blocked on user input (not "completed empty").
    SuspendedUserInput { interaction_id: String },

    /// The turn was suspended because the agent escalated to a human operator.
    /// The checkpoint has already been saved; the session resumes when the
    /// operator approves the escalation and provides guidance.
    Escalated { escalation_request_id: String },
}

/// Build the system prompt given agent instructions and (optionally) raw agent
/// metadata (the full `metadata.autonoetic` object from the SKILL.md frontmatter).
///
/// When `metadata` is provided and contains a `response_contract`, a
/// "Your Response Contract" section is appended so the agent knows upfront what
/// constraints the gateway will validate before returning its output to the caller.
pub(crate) fn compose_system_instructions_with_metadata(
    agent_instructions: &str,
    manifest: &AgentManifest,
    metadata: Option<&serde_json::Value>,
) -> String {
    compose_system_instructions_with_user_context(agent_instructions, manifest, metadata, None)
}

/// Full system prompt composition with optional user context injection.
pub(crate) fn compose_system_instructions_with_user_context(
    agent_instructions: &str,
    manifest: &AgentManifest,
    metadata: Option<&serde_json::Value>,
    user_context_snippet: Option<&str>,
) -> String {
    let foundation = compose_foundation(manifest);

    let tool_bridging = manifest
        .agentskills_import
        .as_ref()
        .filter(|m| m.needs_tool_bridging)
        .map(|_| tool_bridging_appendix());

    let base = {
        let trimmed = agent_instructions.trim();
        let mut parts = vec![foundation.as_str()];
        if let Some(ref bridging) = tool_bridging {
            parts.push(bridging);
        }
        if let Some(snippet) = user_context_snippet {
            parts.push(snippet);
        }
        if !trimmed.is_empty() {
            parts.push("---\n\nAgent-Specific Instructions\n\n");
            parts.push(trimmed);
        }
        parts.join("\n\n")
    };

    let contract_section = metadata
        .and_then(|m| m.get("response_contract"))
        .and_then(|v| {
            let mut lines: Vec<String> = Vec::new();

            if let Some(arr) = v.get("required_artifacts").and_then(|a| a.as_array()) {
                if !arr.is_empty() {
                    let names: Vec<String> = arr
                        .iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect();
                    lines.push(format!("- **required_artifacts**: {}", names.join(", ")));
                }
            }
            if let Some(n) = v.get("max_artifacts").and_then(|x| x.as_u64()) {
                lines.push(format!("- **max_artifacts**: {n}"));
            }
            if let Some(n) = v.get("max_total_size_mb").and_then(|x| x.as_u64()) {
                lines.push(format!("- **max_total_size_mb**: {n}"));
            }
            if let Some(n) = v.get("max_reply_length_chars").and_then(|x| x.as_u64()) {
                lines.push(format!("- **max_reply_length_chars**: {n}"));
            }
            if let Some(n) = v.get("min_artifact_builds").and_then(|x| x.as_u64()) {
                lines.push(format!(
                    "- **min_artifact_builds**: {n} (durable `artifact.build` trace required)"
                ));
            }
            if let Some(schema) = v.get("output_schema") {
                // Compact JSON so the token cost is low, but the agent can read it
                if let Ok(compact) = serde_json::to_string(schema) {
                    lines.push(format!("- **output_schema** (your reply must conform): `{compact}`"));
                }
            }
            if let Some(arr) = v.get("prohibited_text_patterns").and_then(|a| a.as_array()) {
                if !arr.is_empty() {
                    let pats: Vec<String> = arr
                        .iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect();
                    lines.push(format!("- **prohibited_text_patterns**: {}", pats.join(", ")));
                }
            }
            if let Some(n) = v.get("validation_max_loops").and_then(|x| x.as_u64()) {
                lines.push(format!("- **validation_max_loops**: {n}"));
            }

            if lines.is_empty() {
                None
            } else {
                Some(format!(
                    "---\n\nYour Response Contract\n\nThe gateway will validate your final output against these constraints before returning it to the caller. Violating constraints triggers a repair prompt; you have at most `validation_max_loops` attempts to fix the actual outputs.\n\n{}",
                    lines.join("\n")
                ))
            }
        });

    match contract_section {
        Some(section) => format!("{base}\n\n{section}"),
        None => base,
    }
}

/// Build a bounded user context snippet for system prompt injection.
/// Returns None if the scope is task_only or profile has no data.
pub(crate) fn render_user_context_snippet(
    profile: &autonoetic_types::agent::UserProfileRecord,
    scope: &autonoetic_types::agent::BindingScope,
) -> Option<String> {
    use autonoetic_types::agent::BindingScope;

    match scope {
        BindingScope::TaskOnly => None,
        BindingScope::Full | BindingScope::Restricted => {
            let json_str = profile.profile_json.as_ref()?;
            let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;

            let filtered = if *scope == BindingScope::Restricted {
                let mut restricted = serde_json::Map::new();
                if let Some(obj) = parsed.as_object() {
                    for key in &["preferences", "constraints"] {
                        if let Some(val) = obj.get(*key) {
                            restricted.insert((*key).to_string(), val.clone());
                        }
                    }
                }
                serde_json::Value::Object(restricted)
            } else {
                parsed
            };

            if filtered.is_null() || (filtered.is_object() && filtered.as_object().unwrap().is_empty()) {
                return None;
            }

            let compact = serde_json::to_string(&filtered).ok()?;
            // Bound to ~2000 chars (~500 tokens)
            let bounded = if compact.len() > 2000 {
                format!("{}...", &compact[..2000])
            } else {
                compact
            };

            Some(format!(
                "---\n\nUser Profile Context\n\nYou have access to this user's profile data (scope: {}). Use it to personalize your behavior.\n\n```json\n{}\n```",
                scope, bounded
            ))
        }
    }
}

#[cfg(test)]
mod agentskills_bridging_tests {
    use super::*;
    use autonoetic_types::agent::AgentSkillsImportMetadata;

    #[test]
    fn tool_bridging_injected_for_agentskills_import() {
        let mut manifest = default_test_manifest();
        manifest.agentskills_import = Some(AgentSkillsImportMetadata {
            license: Some("MIT".to_string()),
            compatibility: Some("claude-code".to_string()),
            allowed_tools: vec!["Bash(*)".to_string(), "Read".to_string()],
            needs_tool_bridging: true,
        });

        let output = compose_system_instructions_with_metadata(
            "Do git things with Bash(git log).",
            &manifest,
            None,
        );

        assert!(
            output.contains("Tool Compatibility Notes"),
            "should include tool bridging appendix"
        );
        assert!(
            output.contains("Bash(command)"),
            "should contain Bash mapping"
        );
        assert!(
            output.contains("content.read"),
            "should contain content.read mapping"
        );
        assert!(
            output.contains("Do git things with Bash(git log)."),
            "should still contain agent instructions"
        );
    }

    #[test]
    fn no_tool_bridging_without_agentskills_import() {
        let manifest = default_test_manifest();
        let output = compose_system_instructions_with_metadata("Do things.", &manifest, None);
        assert!(
            !output.contains("Tool Compatibility Notes"),
            "should not include tool bridging for native agents"
        );
    }

    fn default_test_manifest() -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: autonoetic_types::agent::RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: autonoetic_types::agent::AgentIdentity {
                id: "test".to_string(),
                name: "Test".to_string(),
                description: "Test".to_string(),
            },
            capabilities: vec![],
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            gateway_url: None,
            gateway_token: None,
            response_contract: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
        compression: None,
        }
    }
}

fn max_other_empty_retries() -> usize {
    std::env::var(LLM_OTHER_EMPTY_RETRY_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(LLM_OTHER_EMPTY_RETRY_DEFAULT)
}

fn is_retryable_empty_other_response(response: &crate::llm::CompletionResponse) -> bool {
    matches!(&response.stop_reason, StopReason::Other(s) if s.trim().is_empty())
        && response.tool_calls.is_empty()
        && response.text.trim().is_empty()
}

/// Apply prompt budget enforcement based on the configured strategy.
///
/// Returns potentially modified tools and history after enforcement actions.
fn apply_prompt_budget(
    tools: Vec<ToolDefinition>,
    history: Vec<Message>,
    breakdown: &crate::runtime::prompt_budget::PromptBudgetBreakdown,
    config: Option<&GatewayConfig>,
    _session_id: &str,
    _turn_id: &str,
    tracer: &mut SessionTracer,
) -> anyhow::Result<(Vec<ToolDefinition>, Vec<Message>)> {
    let Some(config) = config else {
        return Ok((tools, history));
    };
    let budget_config = &config.prompt_budget;
    let action = &budget_config.on_exceeded;

    let effective_limit = breakdown
        .context_window
        .map(|cw| cw.saturating_sub(budget_config.margin_tokens))
        .unwrap_or(usize::MAX);

    let current_total = breakdown.total_tokens;
    let within_total_budget = current_total <= effective_limit;

    // Check per-section caps. These apply regardless of whether the total
    // budget is satisfied — a section cap is a hard constraint independent
    // of the overall context window.
    let section_cap_violation = {
        let sys_exceeded = budget_config.system_prompt_max_tokens > 0
            && breakdown.system_prompt_tokens > budget_config.system_prompt_max_tokens;
        let tool_exceeded = budget_config.tool_definitions_max_tokens > 0
            && breakdown.tool_definition_tokens > budget_config.tool_definitions_max_tokens;
        sys_exceeded || tool_exceeded
    };

    if !section_cap_violation && within_total_budget {
        if let Some(pct) = breakdown.utilization_pct {
            if pct >= budget_config.warn_at_pct {
                tracing::warn!(
                    target: "autonoetic::prompt_budget",
                    utilization_pct = pct,
                    warn_threshold = budget_config.warn_at_pct,
                    total_tokens = current_total,
                    "Prompt budget approaching limit"
                );
            }
        }
        return Ok((tools, history));
    }

    let _ = tracer.log_event(
        "agent.process",
        "prompt_budget_enforcement",
        autonoetic_types::causal_chain::EntryStatus::Success,
        Some(serde_json::json!({
            "action": format!("{action:?}"),
            "current_total": current_total,
            "effective_limit": effective_limit,
            "over_by": current_total.saturating_sub(effective_limit),
            "section_cap_violation": section_cap_violation,
        })),
    );

    let strategy = crate::runtime::prompt_budget::enforcement_strategy(*action);
    let result = strategy.enforce(tools, history, breakdown, effective_limit, budget_config)?;
    Ok((result.tools, result.history))
}

pub struct AgentExecutor {
    pub manifest: AgentManifest,
    pub instructions: String,
    pub llm: std::sync::Arc<dyn LlmDriver>,
    pub agent_dir: PathBuf,
    pub gateway_dir: Option<PathBuf>,
    pub registry: crate::runtime::tools::NativeToolRegistry,
    pub initial_user_message: String,
    pub guard: LoopGuard,
    pub session_id: Option<String>,
    pub session_started: bool,
    pub turn_counter: u64,
    /// When set, passed to tool execution for config-dependent behavior.
    pub config: Option<Arc<GatewayConfig>>,
    /// Optional per-session LLM/tool/token/wall-clock budgets (shared `Arc` across spawns).
    pub session_budget: Option<Arc<SessionBudgetRegistry>>,
    /// Middleware hooks declared in the agent manifest.
    pub middleware: Middleware,
    /// Token usage per real LLM completion in the last `execute_with_history` run.
    pub llm_usage_last_run: Vec<LlmExchangeUsage>,
    /// Optional OpenRouter models catalog (context + pricing) for UX and session price budgets.
    pub openrouter_catalog: Option<Arc<OpenRouterCatalog>>,
    pub gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    /// Workflow / task context used to populate `TurnContinuation` on suspension.
    pub workflow_id: Option<String>,
    pub task_id: Option<String>,
    /// SHA-256 of runtime.lock content, captured at session start for reproducibility.
    pub runtime_lock_hash: Option<String>,
    /// Emergency-stop hooks (sandbox PIDs, etc.); same registry as [`crate::execution::GatewayExecutionService`].
    pub active_executions:
        Option<Arc<crate::runtime::active_execution_registry::ActiveExecutionRegistry>>,
    /// Shared live digest (`digest.md`) when `gateway_dir` is set.
    pub live_digest: Option<Arc<std::sync::Mutex<crate::runtime::live_digest::LiveDigestWriter>>>,
    /// Last conversation history from `execute_with_history`, retained for `close_session` transcript persistence.
    pub last_history: Vec<Message>,
    /// Session start timestamp (ISO 8601), captured when session_id is first assigned.
    pub session_started_at: Option<String>,
    /// Compression state carried across turns within a session.
    pub compression_metadata: crate::runtime::compression::CompressionMetadata,
    /// Shared HTTP client for compression and other gateway-side operations.
    pub http_client: reqwest::Client,
    /// User ID for profile binding resolution (if authenticated).
    pub user_id: Option<String>,
}

fn tool_result_counts_as_progress(result: &str) -> bool {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result) {
        if let Some(ok) = parsed.get("ok").and_then(|v| v.as_bool()) {
            return ok;
        }
        if let Some(approval_required) = parsed.get("approval_required").and_then(|v| v.as_bool())
        {
            return !approval_required;
        }
        if let Some(exit_code) = parsed.get("exit_code").and_then(|v| v.as_i64()) {
            return exit_code == 0;
        }
        if parsed.get("error").is_some() || parsed.get("error_type").is_some() {
            return false;
        }
        return true;
    }
    false
}

impl AgentExecutor {
    pub fn new(
        manifest: AgentManifest,
        instructions: String,
        llm: std::sync::Arc<dyn LlmDriver>,
        agent_dir: PathBuf,
        registry: crate::runtime::tools::NativeToolRegistry,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    ) -> Self {
        Self {
            manifest: manifest.clone(),
            instructions,
            llm,
            agent_dir,
            registry,
            gateway_dir: None,
            initial_user_message: String::new(),
            guard: LoopGuard::new(5),
            session_id: None,
            session_started: false,
            turn_counter: 0,
            config: None,
            session_budget: None,
            middleware: manifest.middleware.clone().unwrap_or_default(),
            llm_usage_last_run: Vec::new(),
            openrouter_catalog: None,
            gateway_store,
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            active_executions: None,
            live_digest: None,
            last_history: Vec::new(),
            session_started_at: None,
            compression_metadata: Default::default(),
            http_client: reqwest::Client::new(),
            user_id: None,
        }
    }

    /// Take accumulated LLM usage from the last `execute_with_history` (consumes the buffer).
    pub fn take_llm_usage_last_run(&mut self) -> Vec<LlmExchangeUsage> {
        std::mem::take(&mut self.llm_usage_last_run)
    }

    pub fn with_gateway_dir(mut self, gateway_dir: PathBuf) -> Self {
        self.gateway_dir = Some(gateway_dir);
        self
    }

    pub fn with_config(mut self, config: Arc<GatewayConfig>) -> Self {
        self.guard = LoopGuard::with_config(&config.loop_guard);
        self.config = Some(config);
        self
    }

    pub fn with_session_budget(mut self, registry: Option<Arc<SessionBudgetRegistry>>) -> Self {
        self.session_budget = registry;
        self
    }

    pub fn with_openrouter_catalog(mut self, catalog: Option<Arc<OpenRouterCatalog>>) -> Self {
        self.openrouter_catalog = catalog;
        self
    }

    pub fn with_initial_user_message(mut self, message: impl Into<String>) -> Self {
        self.initial_user_message = message.into();
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        if self.session_started_at.is_none() {
            self.session_started_at = Some(chrono::Utc::now().to_rfc3339());
        }
        self
    }

    pub fn with_middleware(mut self, middleware: Middleware) -> Self {
        self.middleware = middleware;
        self
    }

    pub fn with_workflow_context(
        mut self,
        workflow_id: Option<String>,
        task_id: Option<String>,
    ) -> Self {
        self.workflow_id = workflow_id;
        self.task_id = task_id;
        self
    }

    pub fn with_active_executions(
        mut self,
        registry: Option<Arc<crate::runtime::active_execution_registry::ActiveExecutionRegistry>>,
    ) -> Self {
        self.active_executions = registry;
        self
    }

    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = client;
        self
    }

    pub fn with_user_id(mut self, user_id: Option<String>) -> Self {
        self.user_id = user_id;
        self
    }

    fn ensure_session_id(&mut self) -> String {
        if let Some(id) = &self.session_id {
            return id.clone();
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.session_id = Some(id.clone());
        self.session_started_at = Some(chrono::Utc::now().to_rfc3339());
        id
    }

    fn next_turn_id(&mut self) -> String {
        self.turn_counter += 1;
        format!("turn-{:06}", self.turn_counter)
    }

    fn approved_session_continue_count(&self, session_id: &str) -> anyhow::Result<u64> {
        let Some(store) = self.gateway_store.as_ref() else {
            return Ok(0);
        };
        let approved = store.get_approved_approvals_for_session(session_id)?;
        Ok(approved
            .iter()
            .filter(|r| matches!(r.action, ScheduledAction::SessionContinue { .. }))
            .count() as u64)
    }

    fn pending_session_continue_request_id(
        &self,
        cfg: &GatewayConfig,
        session_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let pending = crate::scheduler::approval::pending_approval_requests_for_session(
            cfg,
            self.gateway_store.as_deref(),
            session_id,
        )?;
        Ok(pending.into_iter().find_map(|r| {
            if matches!(r.action, ScheduledAction::SessionContinue { .. }) {
                Some(r.request_id)
            } else {
                None
            }
        }))
    }

    fn create_session_continue_approval(
        &self,
        cfg: &GatewayConfig,
        session_id: &str,
        max_turns: u32,
        blocked_turn: u64,
    ) -> anyhow::Result<String> {
        let Some(store) = self.gateway_store.as_ref() else {
            anyhow::bail!("GatewayStore is required for max-session-turn approval gating");
        };
        let root_session_id =
            crate::runtime::content_store::root_session_id(session_id).to_string();
        let request_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let action = ScheduledAction::SessionContinue {
            session_id: session_id.to_string(),
            root_session_id: root_session_id.clone(),
            requested_by_agent_id: self.manifest.agent.id.clone(),
            turn_counter: blocked_turn,
            max_turns,
            payload: Some(serde_json::json!({
                "reason": "max_session_turns_reached",
            })),
        };
        let workflow_id = self.workflow_id.clone().or_else(|| {
            crate::scheduler::resolve_workflow_id_for_root_session(cfg, &root_session_id)
                .ok()
                .flatten()
        });
        let task_id = self.task_id.clone().or_else(|| {
            workflow_id.as_ref().and_then(|wf_id| {
                crate::scheduler::resolve_task_id_for_session(cfg, None, wf_id, session_id)
                    .ok()
                    .flatten()
            })
        });
        let request = ApprovalRequest {
            request_id: request_id.clone(),
            agent_id: self.manifest.agent.id.clone(),
            session_id: session_id.to_string(),
            action: action.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            reason: Some(format!(
                "Session '{}' reached max_session_turns={} at turn {}. Approve to continue for another window of {} turns.",
                session_id, max_turns, blocked_turn, max_turns
            )),
            evidence_ref: None,
            root_session_id: Some(root_session_id),
            workflow_id,
            task_id,
            status: None,
            decided_at: None,
            decided_by: None,
            approval_level: crate::scheduler::approval::resolve_approval_level(cfg, &action),
        };
        store.create_approval(&request)?;
        Ok(request_id)
    }

    pub fn close_session(&mut self, reason: &str) -> anyhow::Result<()> {
        if !self.session_started {
            return Ok(());
        }
        let session_id = self.ensure_session_id();
        persist_reevaluation_state(&self.agent_dir, |state| {
            state.last_outcome = Some(reason.to_string());
        })?;

        if let Some(gateway_dir) = self.gateway_dir.as_ref() {
            if !self.last_history.is_empty() {
                let mut tracer =
                    SessionTracer::new(&self.agent_dir, &self.manifest.agent.id, &session_id)?;
                let disclosure_state = DisclosureState::new(
                    self.manifest
                        .disclosure
                        .clone()
                        .unwrap_or_else(DisclosurePolicy::default),
                );
                if let Err(e) = persist_history_to_content_store(
                    &self.agent_dir,
                    &session_id,
                    &self.last_history,
                    gateway_dir,
                    &mut tracer,
                    &disclosure_state,
                    self.gateway_store.as_deref(),
                    Some(&self.manifest.agent.id),
                    self.session_started_at.as_deref(),
                ) {
                    tracing::warn!("Failed to persist history on close: {}", e);
                }

                if let Some(gs) = self.gateway_store.as_ref() {
                    let ended_at = chrono::Utc::now().to_rfc3339();
                    let status = if reason.contains("suspended") {
                        "suspended"
                    } else if reason.contains("error") {
                        "failed"
                    } else {
                        "completed"
                    };
                    if let Err(e) = gs.finalize_session_transcript(&session_id, &ended_at, status) {
                        tracing::warn!("Failed to finalize transcript: {}", e);
                    }
                }
            }
        }

        if !reason.contains("suspended") {
            if let Some(gs) = self.gateway_store.as_ref() {
                let root_sid = crate::runtime::content_store::root_session_id(&session_id);
                if let Err(e) = gs.delete_session_grants(&root_sid) {
                    tracing::warn!(
                        root_session_id = %root_sid,
                        error = %e,
                        "Failed to delete session grants on session close"
                    );
                }
            }
        }

        if let Some(d) = self.live_digest.take() {
            if let Ok(mut g) = d.lock() {
                let _ = g.write_session_summary(reason);
            }
        }
        let mut tracer = SessionTracer::new(&self.agent_dir, &self.manifest.agent.id, &session_id)?;
        tracer.log_session_end(reason);
        self.session_started = false;
        self.session_id = None;
        self.turn_counter = 0;
        Ok(())
    }

    /// Build a `SessionCheckpoint` from the current executor state.
    fn build_checkpoint(
        &self,
        history: &[Message],
        turn_id: &str,
        yield_reason: YieldReason,
        pending_tool_state: Option<PendingToolState>,
    ) -> SessionCheckpoint {
        let llm_config_snapshot = self
            .manifest
            .llm_config
            .as_ref()
            .map(LlmConfigSnapshot::from_config);

        // Gather budget counters from the session budget registry
        let (llm_rounds, tokens, cost) = self
            .session_budget
            .as_ref()
            .and_then(|b| b.snapshot_counters(&self.session_id.clone().unwrap_or_default()))
            .unwrap_or((0, 0, 0.0));

        SessionCheckpoint {
            history: history.to_vec(),
            turn_counter: self.turn_counter,
            loop_guard_state: self.guard.snapshot(),
            agent_id: self.manifest.agent.id.clone(),
            session_id: self.session_id.clone().unwrap_or_default(),
            turn_id: turn_id.to_string(),
            workflow_id: self.workflow_id.clone(),
            task_id: self.task_id.clone(),
            runtime_lock_hash: self.runtime_lock_hash.clone(),
            llm_config_snapshot,
            tool_registry_version: None,
            yield_reason,
            content_store_refs: vec![],
            created_at: chrono::Utc::now().to_rfc3339(),
            pending_tool_state,
            llm_rounds_consumed: llm_rounds,
            tool_invocations_consumed: 0, // tracked separately if needed
            tokens_consumed: tokens,
            estimated_cost_usd: cost,
            compression_metadata: if self.compression_metadata.compression_count > 0 {
                Some(self.compression_metadata.clone())
            } else {
                None
            },
        }
    }

    /// Save a checkpoint if config is available. Logs errors as warnings.
    fn save_checkpoint_if_possible(&self, checkpoint: &SessionCheckpoint) {
        if let Some(config) = self.config.as_ref() {
            if let Err(e) = save_checkpoint(config, checkpoint) {
                tracing::warn!(
                    target: "checkpoint",
                    session_id = %checkpoint.session_id,
                    turn_id = %checkpoint.turn_id,
                    error = %e,
                    "Failed to save session checkpoint"
                );
            }
        }
    }

    /// Build user context snippet for system prompt injection.
    fn build_user_context_snippet(&self) -> Option<String> {
        let user_id = self.user_id.as_ref()?;
        let store = self.gateway_store.as_ref()?;
        let agent_id = &self.manifest.agent.id;

        let binding = store.get_user_binding(user_id, agent_id).ok()??;
        let profile = store.get_user_profile(user_id).ok()??;

        render_user_context_snippet(&profile, &binding.scope)
    }

    /// Run the agent loop until completion or guard trip.
    pub async fn execute_loop(&mut self) -> anyhow::Result<()> {
        let user_context = self.build_user_context_snippet();
        let system_instructions = compose_system_instructions_with_user_context(
            &self.instructions,
            &self.manifest,
            self.manifest.response_contract.as_ref(),
            user_context.as_deref(),
        );
        let mut history: Vec<Message> = vec![
            Message::system(system_instructions),
            Message::user(self.initial_user_message.clone()),
        ];
        match self.execute_with_history(&mut history).await {
            Ok(TurnOutcome::Completed(_)) => {
                let _ = self.close_session("execute_loop_complete");
                Ok(())
            }
            Ok(TurnOutcome::Suspended { .. }) => {
                // Suspension is expected in scheduler context; continuation already saved.
                // In standalone execute_loop context, just end the session.
                let _ = self.close_session("execute_loop_suspended");
                Ok(())
            }
            Ok(TurnOutcome::SuspendedUserInput { .. }) => {
                // User interaction checkpoint already saved. End the session;
                // resume happens via the interaction answer path.
                let _ = self.close_session("execute_loop_suspended_user_input");
                Ok(())
            }
            Ok(TurnOutcome::Escalated { .. }) => {
                // Escalation checkpoint already saved. End the session;
                // resume happens via approval resolution.
                let _ = self.close_session("execute_loop_escalated");
                Ok(())
            }
            Err(e) => {
                let _ = self.close_session("execute_loop_error");
                Err(e)
            }
        }
    }

    /// Continue execution from an existing conversation history.
    pub async fn execute_with_history(
        &mut self,
        history: &mut Vec<Message>,
    ) -> anyhow::Result<TurnOutcome> {
        tracing::info!("Agent {} waking up...", self.manifest.agent.id);
        self.guard = LoopGuard::new(5);
        self.llm_usage_last_run.clear();
        let session_id = self.ensure_session_id();
        let turn_id = self.next_turn_id();

        // Hard session-level turn limit with explicit approval gate.
        // Each approval grants one additional window of `max_session_turns`.
        if let Some(cfg) = &self.config {
            if cfg.max_session_turns > 0 {
                let approved_windows = self.approved_session_continue_count(&session_id)?;
                let allowed_turns =
                    (cfg.max_session_turns as u64).saturating_mul(1 + approved_windows);
                // turn_counter already includes the in-flight turn (next_turn_id incremented above),
                // so we trip only when attempting turn N+1 for an allowance of N.
                if self.turn_counter > allowed_turns {
                    let blocked_turn = self.turn_counter;
                    // Do not consume a turn when execution is blocked at the approval gate.
                    self.turn_counter = self.turn_counter.saturating_sub(1);
                    let request_id = if let Some(existing) =
                        self.pending_session_continue_request_id(cfg, &session_id)?
                    {
                        existing
                    } else {
                        self.create_session_continue_approval(
                            cfg,
                            &session_id,
                            cfg.max_session_turns,
                            blocked_turn,
                        )?
                    };
                    tracing::warn!(
                        agent_id = %self.manifest.agent.id,
                        session_id = %session_id,
                        turn_counter = blocked_turn,
                        max_turns = cfg.max_session_turns,
                        approved_windows = approved_windows,
                        approval_request_id = %request_id,
                        "Session reached max turns limit; approval required to continue"
                    );
                    let cp = self.build_checkpoint(
                        history,
                        &turn_id,
                        YieldReason::ApprovalRequired {
                            approval_request_id: request_id.clone(),
                        },
                        None,
                    );
                    self.save_checkpoint_if_possible(&cp);
                    return Ok(TurnOutcome::Suspended {
                        approval_request_id: request_id,
                        continuation: None,
                    });
                }
            }
        }

        if let Some(gw) = self.gateway_dir.as_ref() {
            if self.live_digest.is_none() {
                let agent_id = &self.manifest.agent.id;
                match crate::runtime::live_digest::LiveDigestWriter::open(gw, &session_id, agent_id)
                {
                    Ok(w) => {
                        self.live_digest = Some(Arc::new(std::sync::Mutex::new(w)));
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "live_digest",
                            session_id = %session_id,
                            error = %e,
                            "Failed to open live digest"
                        );
                    }
                }
            }
        }

        let evidence_mode_raw = self
            .config
            .as_ref()
            .map(|cfg| cfg.evidence_mode.clone())
            .unwrap_or_else(|| {
                std::env::var("AUTONOETIC_EVIDENCE_MODE").unwrap_or_else(|_| "full".to_string())
            });
        let evidence_mode = EvidenceMode::parse(&evidence_mode_raw)?;

        let mut tracer = {
            let mut t = if self.config.is_some() {
                SessionTracer::new_with_evidence_mode(
                    &self.agent_dir,
                    &self.manifest.agent.id,
                    &session_id,
                    &evidence_mode_raw,
                )?
            } else {
                SessionTracer::new(&self.agent_dir, &self.manifest.agent.id, &session_id)?
            }
            .with_turn_id(&turn_id);
            if let Some(ld) = self.live_digest.clone() {
                t = t.with_live_digest(ld);
            }
            t
        };

        let active_agent_dir = self.agent_dir.clone();

        if !self.session_started {
            let trigger = history
                .iter()
                .rev()
                .find(|m| matches!(m.role, crate::llm::Role::User))
                .map(|m| m.content.clone())
                .unwrap_or_default();
            tracer.log_session_start("user_input", &trigger, evidence_mode)?;
            self.session_started = true;

            // Capture runtime_lock_hash at session start for reproducibility
            if self.runtime_lock_hash.is_none() {
                self.runtime_lock_hash =
                    crate::runtime::checkpoint::compute_runtime_lock_hash(&self.agent_dir);
            }
        }

        tracer.log_wake(history.len(), evidence_mode);

        let mut mcp_runtime = McpToolRuntime::from_env().await?;
        let mut secret_store: Option<SecretStoreRuntime> =
            SecretStoreRuntime::from_instructions(&self.instructions)?;
        let mut disclosure_state = DisclosureState::new(
            self.manifest
                .disclosure
                .clone()
                .unwrap_or_else(DisclosurePolicy::default),
        );

        let model = self
            .manifest
            .llm_config
            .as_ref()
            .map(|c| c.model.clone())
            .unwrap_or_else(|| "gpt-4o".to_string());
        let temperature = self
            .manifest
            .llm_config
            .as_ref()
            .map(|c| c.temperature as f32);
        let context_window_resolved = resolve_context_window_for_run(
            &self.manifest,
            &model,
            self.openrouter_catalog.as_ref(),
        )
        .await;
        let mut latest_assistant_text: Option<String> = None;
        let policy = PolicyEngine::new(self.manifest.clone());
        let max_empty_other_retries = max_other_empty_retries();
        let mut empty_other_retries_used = 0usize;
        let mut digest_turn_active = false;

        loop {
            // Loop guard check — save checkpoint before propagating max-turns error
            if let Err(e) = self.guard.check_loop() {
                let cp =
                    self.build_checkpoint(history, &turn_id, YieldReason::MaxTurnsReached, None);
                self.save_checkpoint_if_possible(&cp);
                return Err(e);
            }

            // Budget check — save checkpoint before propagating budget-exhausted error
            if let Some(budget) = self.session_budget.as_ref() {
                if let Err(e) = budget.check_pre_llm(&session_id) {
                    let cp = self.build_checkpoint(
                        history,
                        &turn_id,
                        YieldReason::BudgetExhausted,
                        None,
                    );
                    self.save_checkpoint_if_possible(&cp);
                    return Err(e);
                }
            }

            if !digest_turn_active {
                tracer.start_digest_turn()?;
                digest_turn_active = true;
            }

            // Update system message — ensure exactly one system message at position 0
            let user_context = self.build_user_context_snippet();
            let system_instructions = compose_system_instructions_with_user_context(
                &self.instructions,
                &self.manifest,
                self.manifest.response_contract.as_ref(),
                user_context.as_deref(),
            );

            // Remove any existing system messages (could be stale from previous turns)
            history.retain(|m| !matches!(m.role, crate::llm::Role::System));

            // Insert fresh system message at the front
            history.insert(0, Message::system(&system_instructions));

            let tools: Vec<ToolDefinition> = {
                let tier_filter = determine_tool_tier_filter(
                    &self.manifest,
                    self.workflow_id.as_deref(),
                    self.task_id.as_deref(),
                );
                let mut t: Vec<ToolDefinition> = mcp_runtime
                    .tool_definitions()?
                    .into_iter()
                    .filter(|def| policy.can_invoke_tool(&def.name))
                    .filter(|def| {
                        tier_filter
                            .as_ref()
                            .map(|f| f.allows(&def.name))
                            .unwrap_or(true)
                    })
                    .collect();
                t.extend(
                    self.registry
                        .available_definitions_filtered(&self.manifest, tier_filter.as_ref()),
                );
                let turn_index = self.turn_counter.saturating_sub(1);
                let should_compress = self
                    .config
                    .as_ref()
                    .map(|c| c.prompt_budget.compress_tool_schemas_after_turn_0)
                    .unwrap_or(false);
                if should_compress {
                    crate::runtime::prompt_budget::compress_tool_definitions(t, turn_index as usize)
                } else {
                    t
                }
            };

            // --- Prompt Budget Transparency + Enforcement ---
            let budget_breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown::compute(
                &system_instructions,
                &history,
                &tools,
                context_window_resolved.map(|w| w as usize),
            );
            tracing::info!(
                target: "autonoetic::prompt_budget",
                system_tokens = budget_breakdown.system_prompt_tokens,
                conversation_tokens = budget_breakdown.conversation_tokens,
                tool_count = budget_breakdown.tool_count,
                tool_tokens = budget_breakdown.tool_definition_tokens,
                total_tokens = budget_breakdown.total_tokens,
                utilization_pct = ?budget_breakdown.utilization_pct,
                model = %model,
                "Prompt budget breakdown"
            );
            let _ = tracer.log_event(
                "agent.process",
                "prompt_budget",
                autonoetic_types::causal_chain::EntryStatus::Success,
                Some(serde_json::json!({
                    "system_prompt_tokens": budget_breakdown.system_prompt_tokens,
                    "conversation_tokens": budget_breakdown.conversation_tokens,
                    "tool_count": budget_breakdown.tool_count,
                    "tool_definition_tokens": budget_breakdown.tool_definition_tokens,
                    "total_tokens": budget_breakdown.total_tokens,
                    "context_window": budget_breakdown.context_window,
                    "utilization_pct": budget_breakdown.utilization_pct,
                    "model": model,
                })),
            );

            // --- Budget Enforcement ---
            let (tools, trimmed_history) = apply_prompt_budget(
                tools,
                history.clone(),
                &budget_breakdown,
                self.config.as_ref().map(|c| &**c),
                &session_id,
                &turn_id,
                &mut tracer,
            )?;
            *history = trimmed_history;

            // --- Context Compression ---
            // Note: Budget enforcement (above) trims old turns when the prompt budget
            // is exceeded. Context compression (below) summarizes old turns instead of
            // discarding them. Both operate on the same token threshold independently.
            // If `prompt_budget.warn_at_pct` and `context_compression.threshold_pct`
            // are set to similar values, they may compete for the same boundary.
            // Recommended: set compression threshold slightly below the budget trim
            // threshold so compression fires first, preserving information.
            let compression_cfg = self.config.as_ref().map(|c| &c.context_compression);
            let agent_compression = self.manifest.compression.as_ref();
            let should_compress = compression_cfg.map(|c| c.enabled).unwrap_or(false);
            if should_compress {
                let empty_presets = std::collections::HashMap::new();
                let presets = match self.config.as_ref() {
                    Some(c) => &c.llm_presets,
                    None => &empty_presets,
                };
                match crate::runtime::compression::compress_context(
                    history.clone(),
                    context_window_resolved.map(|w| w as usize),
                    compression_cfg.unwrap(),
                    agent_compression,
                    presets,
                    &self.http_client,
                    &session_id,
                    self.turn_counter,
                    Some(&self.compression_metadata),
                )
                .await
                {
                    Ok(result) => {
                        if result.compressed {
                            let mut metadata = result.metadata;
                            if let Some(gateway_dir) = self.gateway_dir.as_ref() {
                                match crate::runtime::compression::persist_compressed_context(
                                    gateway_dir,
                                    &session_id,
                                    &result.original_history,
                                    &metadata,
                                ) {
                                    Ok(handle) => {
                                        metadata.compressed_context_handle = handle;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            target: "autonoetic::compression",
                                            error = %e,
                                            "Failed to persist compressed context"
                                        );
                                    }
                                }
                            }
                            let _ = tracer.log_event(
                                "agent.process",
                                "context_compression",
                                autonoetic_types::causal_chain::EntryStatus::Success,
                                Some(serde_json::json!({
                                    "messages_summarized": metadata.messages_summarized,
                                    "compression_count": metadata.compression_count,
                                    "compressed_context_handle": metadata.compressed_context_handle,
                                })),
                            );
                            *history = result.history;
                            self.compression_metadata = metadata;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "autonoetic::compression",
                            error = %e,
                            "Context compression failed, proceeding without compression"
                        );
                    }
                }
            }

            // --- Model Routing: select model based on budget/complexity signals ---
            use crate::runtime::llm_preset_resolver::{
                is_routing_preset, resolve_classifier_config, resolve_model_list,
            };
            let default_cfg = autonoetic_types::agent::LlmConfig {
                provider: "openai".to_string(),
                model: "gpt-4o".to_string(),
                temperature: 0.2,
                fallback_provider: None,
                fallback_model: None,
                chat_only: false,
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                routing_preset: None,
                thinking: None,
            };
            let mut routed_llm_cfg = self.manifest.llm_config.clone().unwrap_or(default_cfg);

            let presets = &self
                .config
                .as_ref()
                .map(|c| &c.llm_presets)
                .cloned()
                .unwrap_or_default();
            let routing_cfg = self.config.as_ref().and_then(|c| c.llm_routing.as_ref());
            let preset_name = self
                .manifest
                .llm_config
                .as_ref()
                .and_then(|c| c.routing_preset.clone());

            let (routed_model, routing_decision_json, matched_entry) = if let (
                Some(routing_cfg),
                Some(llm_cfg),
                Some(ref name),
            ) =
                (routing_cfg, self.manifest.llm_config.as_ref(), preset_name)
            {
                if let Some(preset) = presets.get(name) {
                    if is_routing_preset(preset) {
                        let routing = preset.routing.as_ref().unwrap();
                        let resolved_models = resolve_model_list(routing, presets);
                        if resolved_models.is_empty() {
                            (model.clone(), None, None)
                        } else {
                            let budget_state = self
                                .session_budget
                                .as_ref()
                                .and_then(|sb| {
                                    sb.snapshot_counters(&session_id).and_then(
                                        |(rounds, _tokens, cost)| {
                                            let config = self.config.as_ref()?;
                                            let max_rounds =
                                                config.session_budget.max_llm_rounds? as f32;
                                            Some(autonoetic_types::config::BudgetState {
                                                session_budget_used_pct: Some(
                                                    rounds as f32 / max_rounds,
                                                ),
                                                prompt_budget_used_pct: budget_breakdown
                                                    .utilization_pct
                                                    .map(|v| v as f32),
                                                session_cost_usd: Some(cost),
                                            })
                                        },
                                    )
                                })
                                .unwrap_or_default();

                            let complexity = autonoetic_types::config::ComplexitySignals {
                                tool_count: Some(tools.len() as u32),
                                recent_tool_use_count: None,
                                has_workflow_caps: self.manifest.capabilities.iter().any(|c| {
                                    matches!(
                                        c,
                                        autonoetic_types::capability::Capability::AgentSpawn { .. }
                                    )
                                }),
                                has_artifact_caps: self.manifest.capabilities.iter().any(|c| {
                                    matches!(
                                        c,
                                        autonoetic_types::capability::Capability::WriteAccess { .. }
                                    )
                                }),
                                is_script_mode: self.manifest.execution_mode
                                    == autonoetic_types::agent::ExecutionMode::Script,
                            };

                            let ctx = autonoetic_types::config::RoutingContext {
                                agent_id: self.manifest.agent.id.clone(),
                                session_id: session_id.clone(),
                                budget: budget_state,
                                complexity,
                                time: autonoetic_types::config::TimeSignals {
                                    turn_number: Some(self.turn_counter as u32),
                                    session_turn_count: Some(self.turn_counter as u32),
                                    elapsed_secs: None,
                                },
                            };

                            let classifier_config = routing
                                .classifier_preset
                                .as_ref()
                                .and_then(|cp| resolve_classifier_config(cp, presets));

                            let (router, _) =
                                crate::runtime::model_router::create_router_from_preset(
                                    routing,
                                    resolved_models.clone(),
                                    classifier_config,
                                );
                            let decision = router
                                .route(&ctx, llm_cfg, &resolved_models, routing_cfg)
                                .await;
                            let matched_entry = resolved_models
                                .iter()
                                .find(|m| {
                                    m.config.provider == decision.provider
                                        && m.config.model == decision.model
                                })
                                .cloned();

                            if decision.provider != llm_cfg.provider {
                                tracing::warn!(
                                    target: "autonoetic::model_routing",
                                    original_provider = %llm_cfg.provider,
                                    routed_provider = %decision.provider,
                                    routed_model = %decision.model,
                                    "Cross-provider routing requested but not supported — staying with original provider"
                                );
                                (llm_cfg.model.clone(), Some(decision), matched_entry)
                            } else {
                                routed_llm_cfg =
                                    crate::runtime::model_router::decision_to_llm_config(
                                        &decision,
                                        llm_cfg,
                                        matched_entry.as_ref(),
                                    );
                                if decision.model != llm_cfg.model {
                                    tracing::info!(
                                        target: "autonoetic::model_routing",
                                        original_model = %llm_cfg.model,
                                        routed_model = %decision.model,
                                        strategy = %decision.strategy_name,
                                        rationale = %decision.rationale,
                                        was_downgraded = decision.was_downgraded,
                                        context_window = ?routed_llm_cfg.context_window_tokens,
                                        base_url = ?routed_llm_cfg.base_url,
                                        "Model routing decision"
                                    );
                                }
                                if routed_llm_cfg.base_url != llm_cfg.base_url {
                                    tracing::warn!(
                                        target: "autonoetic::model_routing",
                                        original_base_url = ?llm_cfg.base_url,
                                        routed_base_url = ?routed_llm_cfg.base_url,
                                        "Model-specific base_url override cannot be applied — driver already built"
                                    );
                                }
                                (decision.model.clone(), Some(decision), matched_entry)
                            }
                        }
                    } else {
                        // Fixed preset — no routing needed
                        (llm_cfg.model.clone(), None, None)
                    }
                } else {
                    tracing::warn!(
                        target: "autonoetic::model_routing",
                        preset_name = %name,
                        "Routing preset not found, using primary model"
                    );
                    (model.clone(), None, None)
                }
            } else {
                (model.clone(), None, None)
            };

            // Log routing decision to causal chain
            if let Some(ref decision) = routing_decision_json {
                let _ = tracer.log_event(
                    "agent.process",
                    "model_routing",
                    autonoetic_types::causal_chain::EntryStatus::Success,
                    Some(serde_json::to_value(decision).unwrap_or_default()),
                );
            }

            // From this point forward, use routed_model for all tracing and cost estimation
            let model = routed_model.clone();

            // Update context window if routing selected a model with different context
            let context_window_resolved = matched_entry
                .as_ref()
                .and_then(|e| e.config.context_window_tokens)
                .or(context_window_resolved);

            let req = CompletionRequest {
                model: routed_model.clone(),
                messages: history.clone(),
                tools,
                max_tokens: None,
                temperature,
                metadata: None,
                thinking: routed_llm_cfg.thinking.clone(),
            };

            // --- Pre-process hook: transform input before LLM call ---
            let pre_hook = self.middleware.pre_process.as_ref();
            let req = if let Some(pre_hook) = pre_hook {
                self.apply_middleware_pre(
                    req,
                    pre_hook,
                    &active_agent_dir,
                    &session_id,
                    &turn_id,
                    &mut tracer,
                )?
            } else {
                req
            };

            // --- Skip LLM if signaled by pre-process hook ---
            // The hook can return a response in metadata.assistant_reply and set metadata.skip_llm: true
            let skip_llm = req
                .metadata
                .as_ref()
                .and_then(|m| m.get("skip_llm"))
                .and_then(|v| v.as_bool())
                == Some(true);

            let mut actual_model = routed_model.clone();
            let response = if skip_llm {
                let assistant_reply = req
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("assistant_reply"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();

                let _ = tracer.log_event(
                    "agent.process",
                    "pre_hook_skip_llm",
                    autonoetic_types::causal_chain::EntryStatus::Success,
                    None,
                );

                crate::llm::CompletionResponse {
                    text: assistant_reply,
                    tool_calls: vec![],
                    usage: crate::llm::TokenUsage::default(),
                    stop_reason: crate::llm::StopReason::EndTurn,
                }
            } else {
                tracing::debug!("Calling LLM");
                let fallback_chain: Vec<(String, String, String)> = routing_decision_json
                    .as_ref()
                    .map(|d| d.fallback_chain.clone())
                    .unwrap_or_default();

                let mut last_err = None;
                let response = self.llm.complete(&req).await;
                match response {
                    Ok(resp) => resp,
                    Err(e) => {
                        if fallback_chain.is_empty() {
                            return Err(e);
                        }
                        tracing::warn!(
                            target: "autonoetic::model_routing",
                            original_model = %routed_model,
                            error = %e,
                            "Primary model failed, trying fallback chain"
                        );
                        last_err = Some(e);
                        let mut final_response = None;
                        for (_fb_preset, fb_provider, fb_model) in &fallback_chain {
                            if *fb_provider != routed_llm_cfg.provider {
                                continue;
                            }
                            let mut fallback_req = req.clone();
                            fallback_req.model = fb_model.clone();
                            tracing::info!(
                                target: "autonoetic::model_routing",
                                fallback_model = %fb_model,
                                "Trying fallback model"
                            );
                            match self.llm.complete(&fallback_req).await {
                                Ok(resp) => {
                                    tracing::info!(
                                        target: "autonoetic::model_routing",
                                        fallback_model = %fb_model,
                                        "Fallback model succeeded"
                                    );
                                    actual_model = fb_model.clone();
                                    final_response = Some(resp);
                                    break;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        target: "autonoetic::model_routing",
                                        fallback_model = %fb_model,
                                        error = %e,
                                        "Fallback model failed"
                                    );
                                    last_err = Some(e);
                                }
                            }
                        }
                        match final_response {
                            Some(resp) => resp,
                            None => {
                                return Err(last_err.unwrap());
                            }
                        }
                    }
                }
            };

            // --- Post-process hook: transform output after LLM call ---
            let post_hook = self.middleware.post_process.as_ref();
            let response = if let Some(post_hook) = post_hook {
                self.apply_middleware_post(
                    response,
                    post_hook,
                    &active_agent_dir,
                    &session_id,
                    &turn_id,
                    &mut tracer,
                )?
            } else {
                response
            };

            let estimated_cost_usd = if skip_llm {
                None
            } else {
                match self.openrouter_catalog.as_ref() {
                    Some(cat) => {
                        cat.estimate_cost_usd(
                            &actual_model,
                            response.usage.input_tokens,
                            response.usage.output_tokens,
                        )
                        .await
                    }
                    None => None,
                }
            };

            if let Some(budget) = self.session_budget.as_ref() {
                if !skip_llm {
                    if let Err(e) = budget.record_llm_completion(
                        &session_id,
                        response.usage.input_tokens,
                        response.usage.output_tokens,
                        estimated_cost_usd,
                    ) {
                        let cp = self.build_checkpoint(
                            history,
                            &turn_id,
                            YieldReason::BudgetExhausted,
                            None,
                        );
                        self.save_checkpoint_if_possible(&cp);
                        return Err(e);
                    }
                }
            }

            self.log_output_schema_validation(&response, &mut tracer);

            // Extract new artifacts from response for logging
            let new_artifacts = extract_artifacts_from_text(&response.text);
            for artifact in &new_artifacts {
                tracer.log_artifact_detected(artifact)?;
            }

            let tool_call_details: Vec<serde_json::Value> = response
                .tool_calls
                .iter()
                .map(|tc| {
                    serde_json::json!({
                        "id": tc.id,
                        "name": tc.name,
                        "arguments": crate::log_redaction::redact_text_for_logs(&tc.arguments)
                    })
                })
                .collect();

            let context_window_tokens = if skip_llm {
                None
            } else {
                context_window_resolved
            };
            let input_context_pct = if skip_llm {
                None
            } else {
                input_tokens_as_context_pct(response.usage.input_tokens, context_window_tokens)
            };

            tracer.log_llm_completion(
                &actual_model,
                &format!("{:?}", response.stop_reason),
                &response.text,
                response.tool_calls.len(),
                response.usage.input_tokens,
                response.usage.output_tokens,
                &tool_call_details,
                context_window_tokens,
                input_context_pct,
            )?;

            let _ = tracer.record_digest_llm_round(
                &actual_model,
                &format!("{:?}", response.stop_reason),
                response.tool_calls.len(),
                response.usage.input_tokens,
                response.usage.output_tokens,
            );

            if !skip_llm {
                self.llm_usage_last_run.push(LlmExchangeUsage {
                    model: model.clone(),
                    input_tokens: response.usage.input_tokens,
                    output_tokens: response.usage.output_tokens,
                    context_window_tokens,
                    input_context_pct,
                    estimated_cost_usd,
                });
                tracing::info!(
                    target: "autonoetic.llm",
                    agent_id = %self.manifest.agent.id,
                    session_id = %session_id,
                    model = %actual_model,
                    input_tokens = response.usage.input_tokens,
                    output_tokens = response.usage.output_tokens,
                    input_context_pct = ?input_context_pct,
                    context_window_tokens = ?context_window_tokens,
                    "llm exchange"
                );
            }

            // Some providers occasionally return an empty completion with
            // stop_reason Other(""). Retry a small bounded number of times at
            // gateway level before surfacing an error to planner.
            if is_retryable_empty_other_response(&response)
                && empty_other_retries_used < max_empty_other_retries
            {
                empty_other_retries_used += 1;
                let _ = tracer.log_event(
                    "llm",
                    "completion_retry",
                    autonoetic_types::causal_chain::EntryStatus::Success,
                    Some(serde_json::json!({
                        "reason": "empty_other_stop_reason",
                        "attempt": empty_other_retries_used,
                        "max_retries": max_empty_other_retries,
                    })),
                );
                let _ = tracer.record_digest_llm_retry_note(
                    empty_other_retries_used,
                    max_empty_other_retries,
                );
                continue;
            }

            // Only count consecutive anomalies.
            if !is_retryable_empty_other_response(&response) {
                empty_other_retries_used = 0;
            }

            if !response.text.trim().is_empty() {
                latest_assistant_text = Some(response.text.clone());
            }

            match response.stop_reason {
                StopReason::ToolUse => {
                    // Keep the assistant message aside — we only push it to history
                    // if no suspension occurs (continuation reconstruction re-injects it).
                    let mut assistant_msg = Message::assistant(response.text.clone());
                    assistant_msg.tool_calls = response.tool_calls.clone();

                    if let Some(budget) = self.session_budget.as_ref() {
                        if let Err(e) = budget
                            .reserve_tool_invocations(&session_id, response.tool_calls.len() as u64)
                        {
                            let cp = self.build_checkpoint(
                                history,
                                &turn_id,
                                YieldReason::BudgetExhausted,
                                None,
                            );
                            self.save_checkpoint_if_possible(&cp);
                            return Err(e);
                        }
                    }

                    let tool_run_ctx = self.session_id.as_ref().map(|sid| {
                        crate::runtime::active_execution_registry::NativeToolRunContext {
                            registry: self
                                .active_executions
                                .clone()
                                .unwrap_or_else(
                                    crate::runtime::active_execution_registry::ActiveExecutionRegistry::new,
                                ),
                            root_session_id: crate::runtime::live_digest::base_session_id(sid)
                                .to_string(),
                            workflow_id: self.workflow_id.clone(),
                            task_id: self.task_id.clone(),
                            session_id: sid.clone(),
                            agent_id: self.manifest.agent.id.clone(),
                            live_digest: self.live_digest.clone(),
                            user_id: self.user_id.clone(),
                        }
                    });
                    let mut processor = ToolCallProcessor::new(
                        &mut mcp_runtime,
                        &self.registry,
                        &self.manifest,
                        &mut disclosure_state,
                        secret_store.as_mut(),
                        self.config.as_deref(),
                        self.gateway_store.clone(),
                        tool_run_ctx,
                    )
                    .with_session_context(self.session_id.clone(), Some(turn_id.clone()));

                    let (_had_any_success, results) = processor
                        .process_tool_calls(
                            &response.tool_calls,
                            &active_agent_dir,
                            self.gateway_dir.as_deref(),
                            &mut tracer,
                        )
                        .await?;

                    // Check whether the last executed tool call requires approval.
                    // `process_tool_calls` already stops after the first approval-required result,
                    // so if any approval is pending it is always the last entry in `results`.
                    let approval_info = results.last().and_then(|(id, _name, result_json)| {
                        let parsed = serde_json::from_str::<serde_json::Value>(result_json).ok()?;
                        if parsed
                            .get("approval_required")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            let request_id = parsed
                                .get("request_id")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .unwrap_or_default();
                            Some((id.clone(), request_id, result_json.clone()))
                        } else {
                            None
                        }
                    });

                    if let Some((pending_call_id, request_id, approval_response)) = approval_info {
                        // Build a TurnContinuation and save it, then suspend.
                        let completed_results = results[..results.len() - 1].to_vec();
                        // Tool calls that did NOT run because they came after the approval gate.
                        let remaining_calls = response.tool_calls[results.len()..].to_vec();

                        let pending_tc = response
                            .tool_calls
                            .iter()
                            .find(|tc| tc.id == pending_call_id)
                            .expect("pending call id must match a tool call in the response");

                        let continuation = crate::runtime::continuation::TurnContinuation {
                            history: history.clone(), // snapshot BEFORE assistant_msg
                            assistant_message: assistant_msg,
                            completed_tool_results: completed_results,
                            pending_tool_call:
                                crate::runtime::continuation::PendingApprovalToolCall {
                                    call_id: pending_call_id,
                                    tool_name: pending_tc.name.clone(),
                                    arguments: pending_tc.arguments.clone(),
                                    approval_response,
                                },
                            remaining_tool_calls: remaining_calls,
                            approval_request_id: request_id.clone(),
                            workflow_id: self.workflow_id.clone(),
                            task_id: self.task_id.clone(),
                            session_id: session_id.clone(),
                            turn_id: turn_id.clone(),
                            suspended_at: chrono::Utc::now().to_rfc3339(),
                            loop_guard_state: self.guard.snapshot(),
                        };

                        // Persist continuation to disk when we have a task_id and config.
                        if let (Some(task_id), Some(config)) =
                            (self.task_id.as_deref(), self.config.as_deref())
                        {
                            crate::runtime::continuation::save_continuation(
                                config,
                                task_id,
                                &continuation,
                            )?;
                        }

                        tracing::info!(
                            target: "continuation",
                            agent_id = %self.manifest.agent.id,
                            session_id = %session_id,
                            approval_request_id = %request_id,
                            "Turn suspended at approval boundary; continuation saved"
                        );

                        // Also save a checkpoint for general respawn capability
                        let cp = self.build_checkpoint(
                            history,
                            &turn_id,
                            YieldReason::ApprovalRequired {
                                approval_request_id: request_id.clone(),
                            },
                            None,
                        );
                        self.save_checkpoint_if_possible(&cp);

                        let _ = tracer.end_digest_turn();
                        return Ok(TurnOutcome::Suspended {
                            approval_request_id: request_id,
                            continuation: Some(Box::new(continuation)),
                        });
                    }

                    // Check whether the last executed tool call requires user interaction.
                    let interaction_info = results.last().and_then(|(id, _name, result_json)| {
                        let parsed = serde_json::from_str::<serde_json::Value>(result_json).ok()?;
                        if parsed
                            .get("interaction_required")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            let interaction_id = parsed
                                .get("interaction_id")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .unwrap_or_default();
                            Some((id.clone(), interaction_id))
                        } else {
                            None
                        }
                    });

                    if let Some((pending_call_id, interaction_id)) = interaction_info {
                        // User interaction required — persist assistant prefix + completed tool
                        // results, then checkpoint (pending `user.ask` has no result until resume).
                        let completed_results = results[..results.len() - 1].to_vec();
                        let remaining_calls = response.tool_calls[results.len()..].to_vec();

                        let pending_tc = response
                            .tool_calls
                            .iter()
                            .find(|tc| tc.id == pending_call_id)
                            .expect("pending user interaction call id must match a tool call");

                        history.push(assistant_msg);
                        for (id, name, result) in &completed_results {
                            history.push(Message::tool_result(
                                id.clone(),
                                name.clone(),
                                result.clone(),
                            ));
                        }

                        let pending_tool_state = Some(PendingToolState {
                            completed_tool_results: completed_results.clone(),
                            pending_tool_call: PendingToolCall {
                                call_id: pending_call_id.clone(),
                                tool_name: pending_tc.name.clone(),
                                arguments: pending_tc.arguments.clone(),
                                approval_response: None,
                            },
                            remaining_tool_calls: remaining_calls.clone(),
                        });

                        let cp = self.build_checkpoint(
                            history,
                            &turn_id,
                            YieldReason::UserInputRequired {
                                interaction_id: interaction_id.clone(),
                            },
                            pending_tool_state,
                        );
                        self.save_checkpoint_if_possible(&cp);

                        tracing::info!(
                            target: "user_interaction",
                            agent_id = %self.manifest.agent.id,
                            session_id = %session_id,
                            interaction_id = %interaction_id,
                            pending_call_id = %pending_call_id,
                            "Turn suspended at user interaction boundary"
                        );

                        // Return SuspendedUserInput — the checkpoint has been saved
                        // with YieldReason::UserInputRequired. The resume happens via
                        // checkpoint loading + answer injection. Unlike Completed(None),
                        // this outcome signals to the caller that the session is blocked
                        // on user input (not "done").
                        let _ = tracer.end_digest_turn();
                        return Ok(TurnOutcome::SuspendedUserInput {
                            interaction_id: interaction_id.clone(),
                        });
                    }

                    // Check whether the last executed tool call requires human escalation.
                    let escalation_info = results.last().and_then(|(_id, _name, result_json)| {
                        let parsed = serde_json::from_str::<serde_json::Value>(result_json).ok()?;
                        if parsed
                            .get("escalation_required")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            let request_id = parsed
                                .get("request_id")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .unwrap_or_default();
                            Some(request_id)
                        } else {
                            None
                        }
                    });

                    if let Some(request_id) = escalation_info {
                        let cp = self.build_checkpoint(
                            history,
                            &turn_id,
                            YieldReason::HumanEscalation {
                                escalation_request_id: request_id.clone(),
                            },
                            None,
                        );
                        self.save_checkpoint_if_possible(&cp);

                        tracing::info!(
                            target: "escalation",
                            agent_id = %self.manifest.agent.id,
                            session_id = %session_id,
                            escalation_request_id = %request_id,
                            "Turn suspended for human escalation"
                        );

                        let _ = tracer.end_digest_turn();
                        return Ok(TurnOutcome::Escalated {
                            escalation_request_id: request_id,
                        });
                    }

                    // No approval or interaction required — commit assistant message + tool results to history.
                    history.push(assistant_msg);
                    for (id, _name, result) in &results {
                        history.push(Message::tool_result(
                            id.clone(),
                            _name.clone(),
                            result.clone(),
                        ));
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result) {
                            if parsed.get("ok") == Some(&serde_json::Value::Bool(false)) {
                                if let Some(tc) = response.tool_calls.iter().find(|tc| tc.id == *id) {
                                    self.guard.register_failure(&tc.name, &tc.arguments);
                                }
                            } else if tool_result_counts_as_progress(result) {
                                if let Some(tc) = response.tool_calls.iter().find(|tc| tc.id == *id) {
                                    self.guard.register_progress(&tc.name, &tc.arguments);
                                }
                            }
                        }
                    }

                    let _ = tracer.end_digest_turn();
                    digest_turn_active = false;
                }
                StopReason::EndTurn | StopReason::StopSequence => {
                    if !response.text.trim().is_empty() {
                        history.push(Message::assistant(response.text.clone()));
                    }
                    tracer.log_hibernate(&format!("{:?}", response.stop_reason));

                    // Inject compact workflow summary if any tasks are tracked
                    if let Some(cfg) = self.config.as_ref() {
                        if let Ok(Some(summary)) =
                            crate::scheduler::compact_workflow_summary(cfg, None, &session_id)
                        {
                            // Append to the first system message rather than creating a second one
                            // (some Jinja templates like Qwen reject multiple system messages)
                            if let Some(first) = history.get_mut(0) {
                                if matches!(first.role, crate::llm::Role::System) {
                                    first.content.push_str("\n\n[workflow status] ");
                                    first.content.push_str(&summary);
                                } else {
                                    history.insert(
                                        0,
                                        Message::system(format!("[workflow status] {}", summary)),
                                    );
                                }
                            }
                            tracing::info!(
                                target: "workflow",
                                session_id = %session_id,
                                summary = %summary,
                                "Injected workflow summary at turn end"
                            );
                        }

                        // Durable planner checkpoint at turn end
                        let root = crate::runtime::content_store::root_session_id(&session_id);
                        if let Ok(Some(wf_id)) =
                            crate::scheduler::resolve_workflow_id_for_root_session(cfg, &root)
                        {
                            let planner_intent = response.text.trim();
                            let context = serde_json::json!({
                                "turn_id": turn_id,
                                "session_id": session_id,
                                "assistant_message_len": planner_intent.len(),
                            });
                            if let Err(e) = crate::scheduler::checkpoint_planner(
                                cfg,
                                None,
                                &wf_id,
                                if planner_intent.is_empty() {
                                    format!("Turn {} ended", &turn_id[..turn_id.len().min(8)])
                                } else {
                                    let truncated = if planner_intent.len() > 200 {
                                        format!("{}…", &planner_intent[..200])
                                    } else {
                                        planner_intent.to_string()
                                    };
                                    truncated
                                },
                                context,
                            ) {
                                tracing::debug!(
                                    target: "workflow",
                                    error = %e,
                                    "Planner checkpoint skipped (no workflow or save failed)"
                                );
                            }
                        }
                    }

                    // Persist history to content store at hibernate points
                    if let Some(gateway_dir) = self.gateway_dir.as_ref() {
                        if let Err(e) = persist_history_to_content_store(
                            &self.agent_dir,
                            &session_id,
                            history,
                            gateway_dir,
                            &mut tracer,
                            &disclosure_state,
                            self.gateway_store.as_deref(),
                            Some(&self.manifest.agent.id),
                            self.session_started_at.as_deref(),
                        ) {
                            tracing::warn!("Failed to persist history: {}", e);
                        }
                    }

                    // Save checkpoint at hibernation yield point
                    let cp =
                        self.build_checkpoint(history, &turn_id, YieldReason::Hibernation, None);
                    self.save_checkpoint_if_possible(&cp);
                    if let Some(config) = self.config.as_ref() {
                        // Prune old checkpoints, keep last 3
                        let _ = prune_checkpoints(config, &session_id, 3);
                    }

                    let _ = tracer.end_digest_turn();
                    break;
                }
                StopReason::MaxTokens | StopReason::Other(_) => {
                    if !response.text.trim().is_empty() {
                        history.push(Message::assistant(response.text.clone()));
                    }
                    tracer.log_stopped(&format!("{:?}", response.stop_reason));
                    let _ = tracer.end_digest_turn();
                    break;
                }
            }
        }

        let outcome = Ok(TurnOutcome::Completed(
            latest_assistant_text.map(|t| disclosure_state.filter_reply(&t)),
        ));
        self.last_history = history.clone();
        outcome
    }

    fn log_output_schema_validation(
        &self,
        response: &crate::llm::CompletionResponse,
        tracer: &mut SessionTracer,
    ) {
        // Only validate final output when agent claims completion (EndTurn).
        // Skip validation for tool use responses - agents may emit free text
        // alongside tool calls, which is expected reasoning/narration.
        if !matches!(
            response.stop_reason,
            crate::llm::StopReason::EndTurn | crate::llm::StopReason::StopSequence
        ) {
            return;
        }

        let Some(returns_schema) = self.manifest.io.as_ref().and_then(|io| io.returns.as_ref())
        else {
            return;
        };

        let validation = validate_against_schema(&response.text, returns_schema);
        let _ = tracer.log_event(
            "agent.process",
            "output_schema_validation",
            autonoetic_types::causal_chain::EntryStatus::Success,
            Some(serde_json::json!({
                "valid": validation.valid,
                "messages": validation.messages,
            })),
        );
    }

    /// Executes middleware pre-process script in a sandbox.
    fn apply_middleware_pre(
        &self,
        mut req: crate::llm::CompletionRequest,
        hook_script: &str,
        active_agent_dir: &Path,
        session_id: &str,
        turn_id: &str,
        tracer: &mut SessionTracer,
    ) -> anyhow::Result<crate::llm::CompletionRequest> {
        let _ = tracer.log_event(
            "agent.process",
            "pre_hook_requested",
            autonoetic_types::causal_chain::EntryStatus::Success,
            Some(serde_json::json!({ "turn_id": turn_id })),
        );

        let input_json = serde_json::to_string(&req)?;
        let output =
            self.run_middleware_script(hook_script, input_json, active_agent_dir, session_id)?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(transformed) = serde_json::from_str::<serde_json::Value>(&stdout) {
                if let Ok(new_req) =
                    serde_json::from_value::<crate::llm::CompletionRequest>(transformed.clone())
                {
                    req = new_req;
                } else if let Some(skip) = transformed.get("skip_llm").and_then(|v| v.as_bool()) {
                    let mut meta = req.metadata.unwrap_or_default();
                    meta.insert("skip_llm".to_string(), serde_json::Value::Bool(skip));
                    if let Some(reply) = transformed.get("assistant_reply").and_then(|v| v.as_str())
                    {
                        meta.insert(
                            "assistant_reply".to_string(),
                            serde_json::Value::String(reply.to_string()),
                        );
                    }
                    req.metadata = Some(meta);
                }
                let _ = tracer.log_event(
                    "agent.process",
                    "pre_hook_completed",
                    autonoetic_types::causal_chain::EntryStatus::Success,
                    None,
                );
                Ok(req)
            } else {
                let _ = tracer.log_event(
                    "agent.process",
                    "pre_hook_failed",
                    autonoetic_types::causal_chain::EntryStatus::Error,
                    Some(serde_json::json!({ "error": "Invalid JSON from hook" })),
                );
                anyhow::bail!("Pre-process hook returned invalid JSON");
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = tracer.log_event(
                "agent.process",
                "pre_hook_failed",
                autonoetic_types::causal_chain::EntryStatus::Error,
                Some(serde_json::json!({ "error": stderr })),
            );
            anyhow::bail!("Pre-process hook failed: {}", stderr);
        }
    }

    /// Executes middleware post-process script in a sandbox.
    fn apply_middleware_post(
        &self,
        mut response: crate::llm::CompletionResponse,
        hook_script: &str,
        active_agent_dir: &Path,
        session_id: &str,
        turn_id: &str,
        tracer: &mut SessionTracer,
    ) -> anyhow::Result<crate::llm::CompletionResponse> {
        let _ = tracer.log_event(
            "agent.process",
            "post_hook_requested",
            autonoetic_types::causal_chain::EntryStatus::Success,
            Some(serde_json::json!({ "turn_id": turn_id })),
        );

        let input_json = serde_json::to_string(&response)?;
        let output =
            self.run_middleware_script(hook_script, input_json, active_agent_dir, session_id)?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(transformed) = serde_json::from_str::<crate::llm::CompletionResponse>(&stdout)
            {
                response = transformed;
                let _ = tracer.log_event(
                    "agent.process",
                    "post_hook_completed",
                    autonoetic_types::causal_chain::EntryStatus::Success,
                    None,
                );
                Ok(response)
            } else {
                let _ = tracer.log_event(
                    "agent.process",
                    "post_hook_failed",
                    autonoetic_types::causal_chain::EntryStatus::Error,
                    Some(serde_json::json!({ "error": "Invalid JSON from hook" })),
                );
                anyhow::bail!("Post-process hook returned invalid JSON");
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = tracer.log_event(
                "agent.process",
                "post_hook_failed",
                autonoetic_types::causal_chain::EntryStatus::Error,
                Some(serde_json::json!({ "error": stderr })),
            );
            anyhow::bail!("Post-process hook failed: {}", stderr);
        }
    }

    fn run_middleware_script(
        &self,
        command: &str,
        stdin_json: String,
        active_agent_dir: &Path,
        _session_id: &str,
    ) -> anyhow::Result<std::process::Output> {
        use crate::sandbox::{SandboxDriverKind, SandboxRunner};
        use std::io::Write;

        let driver = SandboxDriverKind::parse(&self.manifest.runtime.sandbox)?;
        let agent_dir_str = active_agent_dir
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid active_agent_dir"))?;

        let mut runner = SandboxRunner::spawn_with_driver_and_dependencies(
            driver,
            agent_dir_str,
            command,
            None,
            None,
        )?;

        if let Some(mut stdin) = runner.process.stdin.take() {
            stdin.write_all(stdin_json.as_bytes())?;
        }

        runner.process.wait_with_output().map_err(Into::into)
    }
}

/// Extracts JSON from markdown-wrapped content.
/// Handles common LLM output formats:
/// - ```json ... ``` (code block with json language hint)
/// - ``` ... ``` (plain code block)
/// - Plain JSON without markdown wrapping
fn extract_json_from_markdown(input: &str) -> String {
    let trimmed = input.trim();

    // Try to find ```json ... ``` or ``` ... ``` blocks
    if let Some(start) = trimmed.find("```") {
        let after_first_block = &trimmed[start + 3..];

        // Skip language hint (e.g., "json\n" -> "\n")
        let content_start = after_first_block.find('\n').map(|i| i + 1).unwrap_or(0);
        let content = &after_first_block[content_start..];

        // Find closing ```
        if let Some(end) = content.find("```") {
            return content[..end].trim().to_string();
        }
    }

    // No markdown wrapping found, return original
    input.to_string()
}

/// Lightweight schema validation: checks required fields and basic type hints.
/// Extracts JSON from markdown-wrapped content before validation.
fn validate_against_schema(input: &str, schema: &serde_json::Value) -> SchemaValidation {
    let mut validation = SchemaValidation {
        valid: true,
        messages: Vec::new(),
    };

    // Extract JSON from markdown if present
    let json_input = extract_json_from_markdown(input);

    let parsed_input: serde_json::Value = match serde_json::from_str(&json_input) {
        Ok(v) => v,
        Err(_) => {
            validation.valid = false;
            validation
                .messages
                .push("Output is not valid JSON".to_string());
            return validation;
        }
    };

    if let Some(expected_type) = schema.get("type").and_then(|t| t.as_str()) {
        let type_matches = match expected_type {
            "object" => parsed_input.is_object(),
            "array" => parsed_input.is_array(),
            "string" => parsed_input.is_string(),
            "number" => parsed_input.is_number(),
            "boolean" => parsed_input.is_boolean(),
            _ => true,
        };
        if !type_matches {
            validation.valid = false;
            validation.messages.push(format!(
                "Type mismatch: expected {}, got {}",
                expected_type, parsed_input
            ));
        }
    }

    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        if let Some(obj) = parsed_input.as_object() {
            for field in required {
                if let Some(field_name) = field.as_str() {
                    if !obj.contains_key(field_name) {
                        validation.valid = false;
                        validation
                            .messages
                            .push(format!("Missing required field: {}", field_name));
                    }
                }
            }
        }
    }

    validation
}

fn resolve_context_window_tokens(manifest: &AgentManifest) -> Option<u32> {
    if let Some(cfg) = &manifest.llm_config {
        if let Some(w) = cfg.context_window_tokens {
            return Some(w);
        }
    }
    std::env::var("AUTONOETIC_LLM_CONTEXT_WINDOW")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Determine the tool tier filter based on agent manifest configuration.
///
/// Agents can declare `allowed_tool_tiers` in their manifest to restrict
/// which tool tiers are exposed to them. When not declared (empty list),
/// all tiers are available and the DemoteTools enforcement strategy handles
/// runtime tier reduction when the prompt budget is exceeded.
fn determine_tool_tier_filter(
    manifest: &AgentManifest,
    _workflow_id: Option<&str>,
    _task_id: Option<&str>,
) -> Option<crate::runtime::tools::ToolTierFilter> {
    if manifest.allowed_tool_tiers.is_empty() {
        return None;
    }
    Some(crate::runtime::tools::ToolTierFilter {
        allowed_tiers: manifest.allowed_tool_tiers.clone(),
        always_include_approval_tools: false,
    })
}

/// Manifest/env first; if still unknown and provider is OpenRouter, use the public models API cache.
async fn resolve_context_window_for_run(
    manifest: &AgentManifest,
    model: &str,
    catalog: Option<&Arc<OpenRouterCatalog>>,
) -> Option<u32> {
    if let Some(w) = resolve_context_window_tokens(manifest) {
        return Some(w);
    }
    let use_openrouter = manifest
        .llm_config
        .as_ref()
        .map(|c| c.provider.eq_ignore_ascii_case("openrouter"))
        .unwrap_or(false);
    if !use_openrouter {
        return None;
    }
    match catalog {
        Some(cat) => cat.context_length_for_model(model).await,
        None => None,
    }
}

/// Maps provider prompt (`input`) token count to % of a declared context window.
fn input_tokens_as_context_pct(input_tokens: u64, context_window: Option<u32>) -> Option<f32> {
    let w = f64::from(context_window?);
    if w <= 0.0 {
        return None;
    }
    let pct = (input_tokens as f64 / w) * 100.0;
    Some(pct.min(9999.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{
        CompletionRequest, CompletionResponse, LlmDriver, StopReason, TokenUsage, ToolCall,
        ToolDefinition,
    };
    use crate::policy::PolicyEngine;
    use crate::runtime::reevaluation_state::execute_scheduled_action;
    use crate::runtime::tools::{NativeTool, NativeToolRegistry};
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};
    use autonoetic_types::background::ScheduledAction;
    use autonoetic_types::capability::Capability;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;
    use tempfile::tempdir;

    fn manifest_with_capabilities(capabilities: Vec<Capability>) -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "test-agent".to_string(),
                name: "test-agent".to_string(),
                description: "test".to_string(),
            },
            capabilities,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            gateway_url: None,
            gateway_token: None,

            response_contract: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
        compression: None,
        }
    }

    #[test]
    fn test_apply_prompt_budget_warn_passes_through() {
        let tools = vec![ToolDefinition {
            name: "content.write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({}),
        }];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];
        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 100,
            conversation_tokens: 50,
            tool_count: 1,
            tool_definition_tokens: 30,
            total_tokens: 180,
            context_window: Some(128_000),
            utilization_pct: Some(0.14),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded = autonoetic_types::config::PromptBudgetAction::Warn;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let (result_tools, result_history) = apply_prompt_budget(
            tools.clone(),
            history.clone(),
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        )
        .expect("warn should not fail");

        assert_eq!(result_tools.len(), tools.len());
        assert_eq!(result_history.len(), history.len());
    }

    #[test]
    fn test_apply_prompt_budget_demote_tools_removes_specialized() {
        let tools = vec![
            ToolDefinition {
                name: "content.write".to_string(),
                description: "Write content".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web.search".to_string(),
                description: "Search web".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "agent.spawn".to_string(),
                description: "Spawn agent".to_string(),
                input_schema: serde_json::json!({}),
            },
        ];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];

        // Use breakdown values consistent with the actual tool definitions.
        // estimate_tool_definition for each tool ≈ 37-39 tokens, so 3 tools ≈ 117.
        // total = 100 (system) + 12 (conv) + 117 (tools) = 229
        // After demotion (remove web.search): 2 tools ≈ 78, total ≈ 190
        // Set context_window = 200 so effective_limit = 200, total 229 > 200 triggers demotion,
        // and filtered total ~190 < 200 succeeds.
        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 100,
            conversation_tokens: 12,
            tool_count: 3,
            tool_definition_tokens: 117,
            total_tokens: 229,
            context_window: Some(200),
            utilization_pct: Some(114.5),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded =
            autonoetic_types::config::PromptBudgetAction::DemoteTools;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let (result_tools, result_history) = apply_prompt_budget(
            tools,
            history.clone(),
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        )
        .expect("demote tools should not fail");

        assert_eq!(result_tools.len(), 2);
        assert!(result_tools.iter().any(|t| t.name == "content.write"));
        assert!(result_tools.iter().any(|t| t.name == "agent.spawn"));
        assert!(!result_tools.iter().any(|t| t.name == "web.search"));
        assert_eq!(result_history.len(), history.len());
    }

    #[test]
    fn test_apply_prompt_budget_fail_returns_error() {
        let tools = vec![ToolDefinition {
            name: "content.write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({}),
        }];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];
        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 100,
            conversation_tokens: 50,
            tool_count: 1,
            tool_definition_tokens: 30,
            total_tokens: 180,
            context_window: Some(100),
            utilization_pct: Some(180.0),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded = autonoetic_types::config::PromptBudgetAction::Fail;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let result = apply_prompt_budget(
            tools,
            history,
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        );

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Prompt budget exceeded"));
    }

    #[test]
    fn test_apply_prompt_budget_trim_history_removes_oldest_messages() {
        let tools = vec![ToolDefinition {
            name: "content.write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({}),
        }];
        let long_content = "x".repeat(200);
        let history = vec![
            Message::system("System"),
            Message::user(long_content.clone()),
            Message::assistant(long_content.clone()),
            Message::user(long_content.clone()),
            Message::assistant(long_content.clone()),
            Message::user("Last turn".to_string()),
            Message::assistant("Last reply".to_string()),
        ];
        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 50,
            conversation_tokens: 300,
            tool_count: 1,
            tool_definition_tokens: 30,
            total_tokens: 380,
            context_window: Some(200),
            utilization_pct: Some(190.0),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded =
            autonoetic_types::config::PromptBudgetAction::TrimHistory;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let (result_tools, result_history) = apply_prompt_budget(
            tools.clone(),
            history,
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        )
        .expect("trim history should not fail");

        assert_eq!(result_tools.len(), tools.len());
        assert!(result_history.len() < 7);
        assert!(result_history
            .iter()
            .any(|m| m.role == crate::llm::Role::System));
    }

    #[test]
    fn test_apply_prompt_budget_trim_history_preserves_tool_call_groups() {
        let tools = vec![ToolDefinition {
            name: "content.write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let long_content = "x".repeat(200);

        // Build history with tool-call exchanges that must stay together:
        // [user, assistant+tool_calls(id="tc1"), tool_result(tc1), user, assistant+tool_calls(id="tc2"), tool_result(tc2), user_final]
        let mut assistant_with_tc1 = Message::assistant(long_content.clone());
        assistant_with_tc1.tool_calls = vec![ToolCall {
            id: "tc1".to_string(),
            name: "content.write".to_string(),
            arguments: "{}".to_string(),
        }];

        let mut assistant_with_tc2 = Message::assistant(long_content.clone());
        assistant_with_tc2.tool_calls = vec![ToolCall {
            id: "tc2".to_string(),
            name: "content.write".to_string(),
            arguments: "{}".to_string(),
        }];

        let history = vec![
            Message::system("System prompt".to_string()),
            Message::user(long_content.clone()),
            assistant_with_tc1,
            Message::tool_result("tc1", "content.write", "ok".to_string()),
            Message::user(long_content.clone()),
            assistant_with_tc2,
            Message::tool_result("tc2", "content.write", "ok".to_string()),
            Message::user("Final question".to_string()),
            Message::assistant("Final reply".to_string()),
        ];

        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 50,
            conversation_tokens: 1200,
            tool_count: 1,
            tool_definition_tokens: 30,
            total_tokens: 1280,
            context_window: Some(300),
            utilization_pct: Some(426.0),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded =
            autonoetic_types::config::PromptBudgetAction::TrimHistory;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let (_result_tools, result_history) = apply_prompt_budget(
            tools.clone(),
            history,
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        )
        .expect("trim history should not fail");

        // Verify system message is preserved
        assert!(result_history
            .iter()
            .any(|m| m.role == crate::llm::Role::System));

        // Verify no orphaned tool results: every tool result must have a preceding
        // assistant message with a matching tool call ID
        for msg in &result_history {
            if msg.role == crate::llm::Role::Tool {
                let tc_id = msg
                    .tool_call_id
                    .as_ref()
                    .expect("tool result must have call id");
                let has_matching_assistant = result_history.iter().any(|m| {
                    m.role == crate::llm::Role::Assistant
                        && m.tool_calls.iter().any(|tc| &tc.id == tc_id)
                });
                assert!(
                    has_matching_assistant,
                    "Tool result for '{}' has no matching assistant tool call — group was split",
                    tc_id
                );
            }
        }
    }

    #[test]
    fn test_apply_prompt_budget_section_cap_tool_definitions_triggers_demote_tools() {
        let tools = vec![
            ToolDefinition {
                name: "content.write".to_string(),
                description: "Write content".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web.search".to_string(),
                description: "Search web".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "agent.spawn".to_string(),
                description: "Spawn agent".to_string(),
                input_schema: serde_json::json!({}),
            },
        ];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];

        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 100,
            conversation_tokens: 12,
            tool_count: 3,
            tool_definition_tokens: 117,
            total_tokens: 229,
            context_window: Some(10000),
            utilization_pct: Some(2.3),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded =
            autonoetic_types::config::PromptBudgetAction::DemoteTools;
        config.prompt_budget.tool_definitions_max_tokens = 100;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let (result_tools, _result_history) = apply_prompt_budget(
            tools,
            history.clone(),
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        )
        .expect("demote tools should succeed for section-cap violation");

        assert_eq!(result_tools.len(), 2);
        assert!(!result_tools.iter().any(|t| t.name == "web.search"));
    }

    #[test]
    fn test_apply_prompt_budget_section_cap_system_prompt_fails_for_trim_history() {
        let tools = vec![ToolDefinition {
            name: "content.write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({}),
        }];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];

        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 500,
            conversation_tokens: 12,
            tool_count: 1,
            tool_definition_tokens: 40,
            total_tokens: 562,
            context_window: Some(10000),
            utilization_pct: Some(5.6),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded =
            autonoetic_types::config::PromptBudgetAction::TrimHistory;
        config.prompt_budget.system_prompt_max_tokens = 200;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let result = apply_prompt_budget(
            tools,
            history,
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        );

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("System prompt exceeds configured limit"));
    }

    #[test]
    fn test_enforcement_strategy_factory() {
        use crate::runtime::prompt_budget::enforcement_strategy;
        use autonoetic_types::config::PromptBudgetAction;

        assert_eq!(
            enforcement_strategy(PromptBudgetAction::Warn).name(),
            "warn"
        );
        assert_eq!(
            enforcement_strategy(PromptBudgetAction::TrimHistory).name(),
            "trim_history"
        );
        assert_eq!(
            enforcement_strategy(PromptBudgetAction::DemoteTools).name(),
            "demote_tools"
        );
        assert_eq!(
            enforcement_strategy(PromptBudgetAction::Fail).name(),
            "fail"
        );
    }

    #[test]
    fn test_apply_prompt_budget_fail_on_section_cap_system_prompt() {
        // When on_exceeded = Fail and only system_prompt_max_tokens is violated
        // (total is under effective_limit), the error should mention the system
        // prompt cap specifically, not the generic "prompt budget exceeded".
        let tools = vec![ToolDefinition {
            name: "content.write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({}),
        }];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];

        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 500,
            conversation_tokens: 12,
            tool_count: 1,
            tool_definition_tokens: 40,
            total_tokens: 562,
            context_window: Some(10000),
            utilization_pct: Some(5.6),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded = autonoetic_types::config::PromptBudgetAction::Fail;
        config.prompt_budget.system_prompt_max_tokens = 200;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let result = apply_prompt_budget(
            tools,
            history,
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        );

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("System prompt exceeds configured limit"),
            "Expected section-cap error, got: {}",
            err
        );
    }

    #[test]
    fn test_compose_foundation_core_always_present() {
        let manifest = manifest_with_capabilities(vec![]);
        let foundation = compose_foundation(&manifest);
        assert!(foundation.contains("# Foundation Core"));
    }

    #[test]
    fn test_compose_foundation_includes_workflow_for_reasoning_agents() {
        let manifest = manifest_with_capabilities(vec![]);
        let mut manifest = manifest;
        manifest.execution_mode = autonoetic_types::agent::ExecutionMode::Reasoning;
        let foundation = compose_foundation(&manifest);
        assert!(foundation.contains("# Foundation Core"));
        assert!(foundation.contains("# Foundation Workflow"));
    }

    #[test]
    fn test_compose_foundation_includes_script_for_script_mode() {
        let manifest = manifest_with_capabilities(vec![]);
        let mut manifest = manifest;
        manifest.execution_mode = autonoetic_types::agent::ExecutionMode::Script;
        let foundation = compose_foundation(&manifest);
        assert!(foundation.contains("# Foundation Script"));
    }

    #[test]
    fn test_compose_foundation_includes_artifact_for_write_access() {
        let manifest = manifest_with_capabilities(vec![Capability::WriteAccess {
            scopes: vec!["skills/*".to_string()],
        }]);
        let foundation = compose_foundation(&manifest);
        assert!(foundation.contains("# Foundation Artifact"));
    }

    #[test]
    fn test_compose_foundation_includes_digest_for_digest_scope() {
        let manifest = manifest_with_capabilities(vec![Capability::WriteAccess {
            scopes: vec!["digest/*".to_string()],
        }]);
        let foundation = compose_foundation(&manifest);
        assert!(foundation.contains("# Foundation Digest"));
    }

    #[test]
    fn test_compose_foundation_includes_workflow_for_agent_spawn() {
        let manifest = manifest_with_capabilities(vec![Capability::AgentSpawn { max_children: 5 }]);
        let foundation = compose_foundation(&manifest);
        assert!(foundation.contains("# Foundation Workflow"));
    }

    #[test]
    fn test_compose_foundation_script_mode_excludes_workflow() {
        let manifest = manifest_with_capabilities(vec![]);
        let mut manifest = manifest;
        manifest.execution_mode = autonoetic_types::agent::ExecutionMode::Script;
        let foundation = compose_foundation(&manifest);
        assert!(!foundation.contains("# Foundation Workflow"));
    }

    #[test]
    fn test_compose_foundation_no_caps_no_artifact() {
        let manifest = manifest_with_capabilities(vec![]);
        let foundation = compose_foundation(&manifest);
        assert!(!foundation.contains("# Foundation Artifact"));
    }

    #[test]
    fn test_execute_scheduled_write_file_action() {
        let manifest = manifest_with_capabilities(vec![Capability::WriteAccess {
            scopes: vec!["skills/*".to_string()],
        }]);
        let temp = tempdir().expect("tempdir should create");
        let result = execute_scheduled_action(
            &manifest,
            temp.path(),
            &ScheduledAction::WriteFile {
                path: "skills/generated.md".to_string(),
                content: "generated".to_string(),
                requires_approval: false,
                evidence_ref: None,
            },
            &crate::runtime::tools::default_registry(),
            None,
            None,
        )
        .expect("scheduled write should succeed");
        assert!(result.contains("\"ok\":true"));
    }

    struct FixedTextDriver;
    #[async_trait::async_trait]
    impl LlmDriver for FixedTextDriver {
        async fn complete(
            &self,
            _request: &CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            Ok(CompletionResponse {
                text: "assistant reply".to_string(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }
    }

    struct RetryableOtherThenEndTurnDriver {
        calls: Arc<Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl LlmDriver for RetryableOtherThenEndTurnDriver {
        async fn complete(
            &self,
            _request: &CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            let mut guard = self.calls.lock().expect("mutex should lock");
            *guard += 1;
            if *guard == 1 {
                Ok(CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![],
                    stop_reason: StopReason::Other(String::new()),
                    usage: TokenUsage::default(),
                })
            } else {
                Ok(CompletionResponse {
                    text: "recovered reply".to_string(),
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })
            }
        }
    }

    #[tokio::test]
    async fn test_execute_with_history_appends_assistant_text() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );
        let mut history = vec![Message::system("System prompt"), Message::user("Hello")];
        let outcome = runtime
            .execute_with_history(&mut history)
            .await
            .expect("execution should succeed");
        let reply = match outcome {
            TurnOutcome::Completed(r) => r,
            other => panic!("expected Completed, got {:?}", other),
        };
        assert_eq!(reply.as_deref(), Some("assistant reply"));
    }

    #[tokio::test]
    async fn test_execute_with_history_retries_empty_other_once() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let calls = Arc::new(Mutex::new(0u32));
        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(RetryableOtherThenEndTurnDriver {
                calls: Arc::clone(&calls),
            }),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );
        let mut history = vec![Message::system("System prompt"), Message::user("Hello")];
        let outcome = runtime
            .execute_with_history(&mut history)
            .await
            .expect("execution should succeed after retry");
        let reply = match outcome {
            TurnOutcome::Completed(r) => r,
            other => panic!("expected Completed, got {:?}", other),
        };
        assert_eq!(reply.as_deref(), Some("recovered reply"));
        assert_eq!(*calls.lock().expect("mutex should lock"), 2);
    }

    #[test]
    fn test_is_retryable_empty_other_response() {
        let retryable = CompletionResponse {
            text: String::new(),
            tool_calls: vec![],
            stop_reason: StopReason::Other(String::new()),
            usage: TokenUsage::default(),
        };
        assert!(is_retryable_empty_other_response(&retryable));

        let not_retryable = CompletionResponse {
            text: "has text".to_string(),
            tool_calls: vec![],
            stop_reason: StopReason::Other(String::new()),
            usage: TokenUsage::default(),
        };
        assert!(!is_retryable_empty_other_response(&not_retryable));
    }

    struct CaptureSystemDriver {
        system_message: Arc<Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl LlmDriver for CaptureSystemDriver {
        async fn complete(
            &self,
            request: &CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            let system = request
                .messages
                .iter()
                .find(|m| m.role == crate::llm::Role::System)
                .map(|m| m.content.clone());
            *self.system_message.lock().expect("mutex should lock") = system;
            Ok(CompletionResponse {
                text: "ok".to_string(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn test_execute_loop_includes_foundation_in_system_prompt() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let captured = Arc::new(Mutex::new(None));
        let driver = CaptureSystemDriver {
            system_message: Arc::clone(&captured),
        };
        let mut runtime = AgentExecutor::new(
            manifest,
            "Agent local rules".to_string(),
            Arc::new(driver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );

        runtime
            .execute_loop()
            .await
            .expect("execution should succeed");

        let system = captured
            .lock()
            .expect("mutex should lock")
            .clone()
            .expect("system message should be captured");
        assert!(system.contains("Foundation Core"));
        assert!(system.contains("content.write(name, content)"));
        assert!(system.contains("Agent local rules"));
    }

    struct ApprovalRequiredLifecycleTool;

    impl NativeTool for ApprovalRequiredLifecycleTool {
        fn name(&self) -> &'static str {
            "test.approval"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_string(),
                description: "Lifecycle approval test tool".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            }
        }

        fn is_available(&self, _manifest: &AgentManifest) -> bool {
            true
        }

        fn execute(
            &self,
            _manifest: &AgentManifest,
            _policy: &PolicyEngine,
            _agent_dir: &Path,
            _gateway_dir: Option<&Path>,
            _arguments_json: &str,
            _session_id: Option<&str>,
            _turn_id: Option<&str>,
            _config: Option<&autonoetic_types::config::GatewayConfig>,
            _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
            _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
        ) -> anyhow::Result<String> {
            Ok(serde_json::json!({
                "ok": false,
                "approval_required": true,
                "request_id": "apr-lifecycle1234"
            })
            .to_string())
        }
    }

    struct ApprovalToolUseDriver {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl LlmDriver for ApprovalToolUseDriver {
        async fn complete(
            &self,
            _request: &CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionResponse {
                text: "trying tool".to_string(),
                tool_calls: vec![ToolCall {
                    id: "tc1".to_string(),
                    name: "test.approval".to_string(),
                    arguments: "{}".to_string(),
                }],
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn test_approval_required_suspends_turn_immediately() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let calls = Arc::new(AtomicUsize::new(0));
        let driver = ApprovalToolUseDriver {
            calls: Arc::clone(&calls),
        };
        let mut registry = NativeToolRegistry::new();
        registry.register(Box::new(ApprovalRequiredLifecycleTool));

        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(driver),
            temp.path().to_path_buf(),
            registry,
            None,
        );
        let mut history = vec![Message::system("System prompt"), Message::user("Hello")];

        let outcome = runtime
            .execute_with_history(&mut history)
            .await
            .expect("execution should succeed");

        // With the continuation model, the turn suspends immediately at the approval gate.
        // No second LLM call is made.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only one LLM call should occur"
        );
        match outcome {
            TurnOutcome::Suspended {
                approval_request_id,
                ..
            } => {
                assert_eq!(approval_request_id, "apr-lifecycle1234");
            }
            other => panic!("expected Suspended, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_max_session_turns_creates_approval_and_suspends() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
        let store = Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
                .expect("gateway store should open"),
        );

        let mut cfg = GatewayConfig::default();
        cfg.agents_dir = temp.path().to_path_buf();
        cfg.max_session_turns = 1;

        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            Some(store.clone()),
        )
        .with_config(Arc::new(cfg))
        .with_session_id("root-loop/coder.default-abcd1234");

        let mut history = vec![Message::system("System prompt"), Message::user("Hello")];
        let first = runtime
            .execute_with_history(&mut history)
            .await
            .expect("first turn should execute");
        assert!(matches!(first, TurnOutcome::Completed(_)));

        history.push(Message::user("Continue"));
        let second = runtime
            .execute_with_history(&mut history)
            .await
            .expect("second turn should suspend on max-turn gate");
        let request_id = match second {
            TurnOutcome::Suspended {
                approval_request_id,
                continuation,
            } => {
                assert!(
                    continuation.is_none(),
                    "max-turn suspension should not require tool continuation"
                );
                approval_request_id
            }
            other => panic!("expected Suspended, got {:?}", other),
        };
        assert!(request_id.starts_with("apr-"));

        let approval = store
            .get_approval(&request_id)
            .expect("approval lookup should succeed")
            .expect("approval should exist");
        assert!(matches!(
            approval.action,
            ScheduledAction::SessionContinue { max_turns: 1, .. }
        ));
        assert!(
            approval
                .reason
                .unwrap_or_default()
                .contains("max_session_turns=1"),
            "reason should mention configured max_session_turns"
        );
    }

    #[tokio::test]
    async fn test_max_session_turns_approved_window_allows_one_more_turn() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
        let store = Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
                .expect("gateway store should open"),
        );

        let mut cfg = GatewayConfig::default();
        cfg.agents_dir = temp.path().to_path_buf();
        cfg.max_session_turns = 1;

        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            Some(store.clone()),
        )
        .with_config(Arc::new(cfg))
        .with_session_id("root-loop/evaluator.default-efgh5678");

        let mut history = vec![Message::system("System prompt"), Message::user("Turn 1")];
        let first = runtime
            .execute_with_history(&mut history)
            .await
            .expect("first turn should execute");
        assert!(matches!(first, TurnOutcome::Completed(_)));

        history.push(Message::user("Turn 2"));
        let second = runtime
            .execute_with_history(&mut history)
            .await
            .expect("second call should suspend");
        let request_id = match second {
            TurnOutcome::Suspended {
                approval_request_id,
                ..
            } => approval_request_id,
            other => panic!("expected Suspended, got {:?}", other),
        };

        store
            .record_decision(
                &request_id,
                "approved",
                "operator",
                &chrono::Utc::now().to_rfc3339(),
            )
            .expect("decision should record");

        // After approval, one additional window (1 turn here) should be granted.
        history.push(Message::user("Turn 2 retry after approval"));
        let third = runtime
            .execute_with_history(&mut history)
            .await
            .expect("third call should execute after approval grant");
        assert!(matches!(third, TurnOutcome::Completed(_)));
    }

    #[test]
    fn test_native_disclosure_path_extraction() {
        let registry = crate::runtime::tools::default_registry();
        // content.read uses name_or_handle, not path
        let meta =
            registry.extract_metadata("content.read", "{\"name_or_handle\": \"secrets.txt\"}");
        assert_eq!(meta.path.as_deref(), Some("secrets.txt"));
    }

    #[test]
    fn test_extract_json_from_markdown_plain_json() {
        let input = r#"{"findings":["fact1"],"summary":"ok"}"#;
        let extracted = extract_json_from_markdown(input);
        assert_eq!(extracted, input);
    }

    #[test]
    fn test_extract_json_from_markdown_json_code_block() {
        let input = r#"Here is the result:
```json
{"findings":["fact1"],"summary":"ok"}
```
Hope this helps!"#;
        let extracted = extract_json_from_markdown(input);
        let expected = r#"{"findings":["fact1"],"summary":"ok"}"#;
        assert_eq!(extracted, expected);
    }

    #[test]
    fn test_extract_json_from_markdown_plain_code_block() {
        let input = r#"Result:
```
{"findings":["fact1"],"summary":"ok"}
```"#;
        let extracted = extract_json_from_markdown(input);
        let expected = r#"{"findings":["fact1"],"summary":"ok"}"#;
        assert_eq!(extracted, expected);
    }

    #[test]
    fn test_extract_json_from_markdown_multiline_json() {
        let input = r#"```json
{
  "findings": ["fact1", "fact2"],
  "summary": "ok"
}
```"#;
        let extracted = extract_json_from_markdown(input);
        let expected = r#"{
  "findings": ["fact1", "fact2"],
  "summary": "ok"
}"#;
        assert_eq!(extracted, expected);
    }

    #[test]
    fn test_validate_output_schema_valid_json_input() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["findings", "summary"]
        });
        let output = r#"{"findings":["fact1"],"summary":"ok"}"#;
        let result = validate_against_schema(output, &schema);
        assert!(result.valid);
        assert!(result.messages.is_empty());
    }

    #[test]
    fn test_validate_output_schema_non_json_input() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["findings"]
        });
        let output = "plain text response";
        let result = validate_against_schema(output, &schema);
        assert!(!result.valid);
        assert!(result.messages.iter().any(|m| m.contains("not valid JSON")));
    }

    #[test]
    fn test_validate_output_schema_accepts_markdown_wrapped_json() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["findings", "summary"]
        });
        let output = r#"Here is the result:
```json
{"findings":["fact1"],"summary":"ok"}
```
Hope this helps!"#;
        let result = validate_against_schema(output, &schema);
        assert!(
            result.valid,
            "Should accept markdown-wrapped JSON: {:?}",
            result.messages
        );
    }

    #[tokio::test]
    async fn test_unknown_tool_fails_cleanly() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        struct ToolDriver;
        #[async_trait::async_trait]
        impl LlmDriver for ToolDriver {
            async fn complete(
                &self,
                _req: &CompletionRequest,
            ) -> anyhow::Result<CompletionResponse> {
                Ok(CompletionResponse {
                    text: "".to_string(),
                    tool_calls: vec![ToolCall {
                        id: "c1".to_string(),
                        name: "unknown.tool".to_string(),
                        arguments: "{}".to_string(),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                })
            }
        }
        let mut runtime = AgentExecutor::new(
            manifest,
            "p".to_string(),
            Arc::new(ToolDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );
        let mut history = vec![Message::user("go")];
        let res = runtime.execute_with_history(&mut history).await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("LoopGuard tripped"),
            "expected loop-guard failure for repeated unknown tool calls, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_disclosure_enforcement_in_executor_loop() {
        // Test that the disclosure filter mechanism works
        // The actual filtering is tested in unit tests, here we just verify
        // that the executor loop applies the filter
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");

        struct DisclosureDriver;
        #[async_trait::async_trait]
        impl LlmDriver for DisclosureDriver {
            async fn complete(
                &self,
                _req: &CompletionRequest,
            ) -> anyhow::Result<CompletionResponse> {
                // Direct response without tool use
                Ok(CompletionResponse {
                    text: "The answer is 42".to_string(),
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })
            }
        }

        let mut runtime = AgentExecutor::new(
            manifest,
            "p".to_string(),
            Arc::new(DisclosureDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );
        let mut history = vec![Message::user("what is the answer?")];
        let outcome = runtime
            .execute_with_history(&mut history)
            .await
            .expect("exec success");
        let r = match outcome {
            TurnOutcome::Completed(Some(r)) => r,
            other => panic!("expected Completed reply, got {:?}", other),
        };
        assert!(r.contains("42"), "Expected answer in reply");
    }

    #[test]
    fn test_log_output_schema_validation_skips_tool_use() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let executor = AgentExecutor::new(
            manifest,
            "p".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );

        let mut tracer = crate::runtime::session_tracer::SessionTracer::test_tracer();

        // ToolUse with any text should be skipped - no validation
        let response = CompletionResponse {
            text: "Let me check the database first...".to_string(),
            tool_calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "any".to_string(),
                arguments: "{}".to_string(),
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        };

        executor.log_output_schema_validation(&response, &mut tracer);
    }

    #[test]
    fn test_log_output_schema_validation_validates_end_turn() {
        let mut manifest = manifest_with_capabilities(vec![]);
        manifest.io = Some(autonoetic_types::agent::AgentIO {
            accepts: None,
            returns: Some(serde_json::json!({
                "type": "object",
                "required": ["result"]
            })),
        });

        let temp = tempdir().expect("tempdir should create");
        let executor = AgentExecutor::new(
            manifest,
            "p".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );

        let mut tracer = crate::runtime::session_tracer::SessionTracer::test_tracer();

        // EndTurn with invalid JSON should produce validation error
        let response = CompletionResponse {
            text: "plain text response".to_string(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };

        executor.log_output_schema_validation(&response, &mut tracer);

        // EndTurn with valid JSON matching schema should pass
        let mut tracer2 = crate::runtime::session_tracer::SessionTracer::test_tracer();
        let response2 = CompletionResponse {
            text: r#"{"result": "success"}"#.to_string(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };

        executor.log_output_schema_validation(&response2, &mut tracer2);
    }

    #[test]
    fn with_session_id_initializes_session_started_at() {
        use std::sync::Arc;
        let manifest = manifest_with_capabilities(vec![]);
        let llm: Arc<dyn LlmDriver> = Arc::new(FixedTextDriver);
        let registry = NativeToolRegistry::new();
        let executor = AgentExecutor::new(
            manifest,
            String::new(),
            llm,
            PathBuf::from("/tmp"),
            registry,
            None,
        )
        .with_session_id("preassigned-session-123");

        assert_eq!(
            executor.session_id.as_deref(),
            Some("preassigned-session-123")
        );
        assert!(
            executor.session_started_at.is_some(),
            "with_session_id must initialize session_started_at"
        );
    }
}

/// Persists conversation history to content store at hibernate points.
fn persist_history_to_content_store(
    _agent_dir: &Path,
    session_id: &str,
    history: &[Message],
    gateway_dir: &Path,
    tracer: &mut SessionTracer,
    disclosure_state: &DisclosureState,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    agent_id: Option<&str>,
    session_started_at: Option<&str>,
) -> anyhow::Result<()> {
    use crate::runtime::content_store::ContentStore;
    const MAX_PERSISTED_MESSAGES: usize = 400;

    let store = ContentStore::new(gateway_dir)?;

    // Merge with previously persisted history so reconnecting to the same
    // session can restore prior turns instead of only the latest run window.
    let mut merged_history: Vec<Message> = match store.read_by_name(session_id, "session_history") {
        Ok(existing) => serde_json::from_slice(&existing).unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    if merged_history.is_empty() {
        merged_history.extend_from_slice(history);
    } else {
        for msg in history {
            if matches!(msg.role, crate::llm::Role::System) {
                // Keep only the first system instruction block in persisted history
                // to avoid duplicate foundation prompts on every turn.
                continue;
            }
            merged_history.push(msg.clone());
        }
    }

    // Bound persisted history size while preserving the first system message if present.
    if merged_history.len() > MAX_PERSISTED_MESSAGES {
        let keep = MAX_PERSISTED_MESSAGES;
        let mut trimmed = Vec::with_capacity(keep);
        if let Some(first) = merged_history.first().cloned() {
            if matches!(first.role, crate::llm::Role::System) {
                trimmed.push(first);
                let tail_keep = keep.saturating_sub(1);
                let tail_start = merged_history.len().saturating_sub(tail_keep);
                trimmed.extend(merged_history[tail_start..].iter().cloned());
                merged_history = trimmed;
            } else {
                let tail_start = merged_history.len().saturating_sub(keep);
                merged_history = merged_history[tail_start..].to_vec();
            }
        }
    }

    // Serialize history
    for msg in &mut merged_history {
        // Persist a redacted view of message content.
        msg.content = crate::log_redaction::redact_text_for_logs(
            &disclosure_state.filter_reply(&msg.content),
        );
        for tc in &mut msg.tool_calls {
            tc.arguments = crate::log_redaction::redact_text_for_logs(
                &disclosure_state.filter_reply(&tc.arguments),
            );
        }
    }

    let history_json = serde_json::to_string(&merged_history)?;
    let history_handle = store.write(history_json.as_bytes())?;

    // Register in session
    store.register_name(session_id, "session_history", &history_handle)?;

    // Extract searchable excerpt for FTS
    let excerpt = extract_searchable_excerpt(&merged_history);

    // Upsert session transcript to database for FTS
    if let Some(gs) = gateway_store {
        let root_session_id = crate::runtime::content_store::root_session_id(session_id);
        let transcript_id = format!("stx-{}", session_id);
        let record = autonoetic_types::causal_chain::SessionTranscriptRecord {
            transcript_id,
            session_id: session_id.to_string(),
            root_session_id: root_session_id.to_string(),
            agent_id: agent_id.unwrap_or("unknown").to_string(),
            revision_id: None,
            user_id: None,
            started_at: session_started_at
                .map(|s| s.to_string())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            ended_at: None,
            status: "active".to_string(),
            turn_count: merged_history.len() as i64,
            transcript_handle: Some(history_handle.to_string()),
            excerpt: Some(excerpt),
            origin_node_id: None,
        };
        if let Err(e) = gs.upsert_session_transcript(&record) {
            tracing::warn!(
                target: "lifecycle",
                session_id = %session_id,
                error = %e,
                "Failed to upsert session transcript"
            );
        }
    }

    // Log causal chain entry
    tracer.log_history_persisted(history.len(), &history_handle);

    tracing::debug!(
        target: "lifecycle",
        session_id = %session_id,
        handle = %history_handle,
        message_count = merged_history.len(),
        "Persisted session history to content store"
    );

    Ok(())
}

pub fn extract_searchable_excerpt(messages: &[Message]) -> String {
    const MAX_CHARS: usize = 8000;
    let mut parts = Vec::new();
    let mut total = 0;
    for msg in messages {
        if !msg.content.is_empty() {
            let role_label = match msg.role {
                crate::llm::Role::System => "[system]",
                crate::llm::Role::User => "[user]",
                crate::llm::Role::Assistant => "[assistant]",
                crate::llm::Role::Tool => "[tool]",
            };
            let line = format!("{}: {}", role_label, msg.content);
            let line_len = line.len();
            if total + line_len > MAX_CHARS {
                let remaining = MAX_CHARS.saturating_sub(total);
                if remaining > 0 {
                    parts.push(line[..remaining].to_string());
                }
                break;
            }
            parts.push(line);
            total += line_len;
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod history_persistence_tests {
    use super::*;
    use crate::llm::ToolCall;
    use crate::runtime::content_store::ContentStore;
    use crate::runtime::disclosure::DisclosureState;
    use tempfile::tempdir;

    #[test]
    fn persisted_history_redacts_secret_like_text_and_tool_args() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir)?;

        let mut assistant = Message::assistant("Will use Authorization: Bearer very-secret-value");
        assistant.tool_calls = vec![ToolCall {
            id: "tc-1".to_string(),
            name: "web.fetch".to_string(),
            arguments: r#"{"headers":{"authorization":"Bearer very-secret-value"}}"#.to_string(),
        }];

        let history = vec![Message::system("sys"), assistant];
        let mut tracer = SessionTracer::test_tracer();
        let disclosure = DisclosureState::default();

        persist_history_to_content_store(
            temp.path(),
            "sess-redact",
            &history,
            &gateway_dir,
            &mut tracer,
            &disclosure,
            None,
            None,
            None,
        )?;

        let store = ContentStore::new(&gateway_dir)?;
        let bytes = store.read_by_name("sess-redact", "session_history")?;
        let persisted: Vec<Message> = serde_json::from_slice(&bytes)?;

        let raw = serde_json::to_string(&persisted)?;
        assert!(raw.contains("***REDACTED***"));
        assert!(!raw.contains("very-secret-value"));
        Ok(())
    }
}

#[cfg(test)]
mod loop_guard_tests {
    use super::tool_result_counts_as_progress;

    #[test]
    fn test_tool_result_counts_as_progress_ok_true() {
        assert!(tool_result_counts_as_progress(r#"{"ok": true}"#));
    }

    #[test]
    fn test_tool_result_counts_as_progress_ok_false() {
        assert!(!tool_result_counts_as_progress(r#"{"ok": false}"#));
    }

    #[test]
    fn test_tool_result_counts_as_progress_approval_required_true() {
        assert!(!tool_result_counts_as_progress(
            r#"{"approval_required": true}"#
        ));
    }

    #[test]
    fn test_tool_result_counts_as_progress_approval_required_false() {
        assert!(tool_result_counts_as_progress(
            r#"{"approval_required": false}"#
        ));
    }

    #[test]
    fn test_tool_result_counts_as_progress_exit_code_zero() {
        assert!(tool_result_counts_as_progress(r#"{"exit_code": 0}"#));
    }

    #[test]
    fn test_tool_result_counts_as_progress_exit_code_nonzero() {
        assert!(!tool_result_counts_as_progress(r#"{"exit_code": 1}"#));
    }

    #[test]
    fn test_tool_result_counts_as_progress_error_field() {
        assert!(!tool_result_counts_as_progress(
            r#"{"error": "something went wrong"}"#
        ));
    }

    #[test]
    fn test_tool_result_counts_as_progress_error_type_field() {
        assert!(!tool_result_counts_as_progress(
            r#"{"error_type": "validation", "message": "bad input"}"#
        ));
    }

    #[test]
    fn test_tool_result_counts_as_progress_plain_data() {
        assert!(tool_result_counts_as_progress(
            r#"{"results": [], "count": 0}"#
        ));
    }

    #[test]
    fn test_tool_result_counts_as_progress_invalid_json() {
        assert!(!tool_result_counts_as_progress("not json"));
    }
}
