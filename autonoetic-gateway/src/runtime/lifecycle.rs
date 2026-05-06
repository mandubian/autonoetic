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
use autonoetic_types::agent::{AgentManifest, LlmExchangeUsage, LoopGuardDeclaration, Middleware};
use autonoetic_types::background::{ApprovalRequest, ScheduledAction};
use autonoetic_types::config::{GatewayConfig, LoopGuardConfig};
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
const FOUNDATION_SDK: &str = include_str!("foundation_sdk.md");
const LLM_OTHER_EMPTY_RETRY_ENV: &str = "AUTONOETIC_LLM_OTHER_EMPTY_RETRIES";
const LLM_OTHER_EMPTY_RETRY_DEFAULT: usize = 1;

/// Compose foundation instructions based on agent capabilities and execution mode.
///
/// Always includes core instructions. Adds workflow, artifact, script, digest,
/// and SDK layers based on what the agent can actually do.
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
    let has_code_execution = manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            autonoetic_types::capability::Capability::CodeExecution { .. }
        )
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

    if has_code_execution {
        parts.push(FOUNDATION_SDK.trim());
    }

    parts.join("\n\n---\n\n")
}

const TOOL_BRIDGING_APPENDIX: &str = r#"---

Tool Compatibility Notes (auto-generated from AgentSkills import)

This skill was imported from the Agent Skills (agentskills.io) format.
The following tool mappings apply:

| Skill references | Autonoetic equivalent |
|---|---|
| `Bash(command)` | `sandbox_exec({"command": "command"})` |
| `Read(path)` | `content_read(name_or_handle)` — files must be loaded via content store |
| `Write(path, content)` | `content_write(name, content)` |
| `WebSearch(query)` | `web_search({"query": "query"})` |
| `WebFetch(url)` | `web_fetch({"url": "url"})` |

File paths referenced by the skill are available in the agent directory.
Use content_read/content_write or sandbox paths relative to the agent working directory."#;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecuteLoopTermination {
    AgentRequestedExit,
    SuspendedForApproval,
    SuspendedForUserInput,
    SuspendedForHumanEscalation,
    FatalError,
}

impl ExecuteLoopTermination {
    fn close_reason(self) -> &'static str {
        match self {
            Self::AgentRequestedExit => "execute_loop_complete",
            Self::SuspendedForApproval => "execute_loop_suspended",
            Self::SuspendedForUserInput => "execute_loop_suspended_user_input",
            Self::SuspendedForHumanEscalation => "execute_loop_escalated",
            Self::FatalError => "execute_loop_error",
        }
    }

    fn from_turn_outcome(outcome: &TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::Completed(_) => Self::AgentRequestedExit,
            TurnOutcome::Suspended { .. } => Self::SuspendedForApproval,
            TurnOutcome::SuspendedUserInput { .. } => Self::SuspendedForUserInput,
            TurnOutcome::Escalated { .. } => Self::SuspendedForHumanEscalation,
        }
    }
}

/// Build the system prompt given agent instructions and (optionally) raw agent
/// output policy metadata from the SKILL.md frontmatter.
///
/// When output constraints are declared, an
/// "Your Output Contract" section is appended so the agent knows upfront what
/// constraints the gateway will validate before returning its output to the caller.
pub(crate) fn compose_system_instructions_with_metadata(
    agent_instructions: &str,
    manifest: &AgentManifest,
    output_policy: Option<&autonoetic_types::agent::OutputPolicy>,
) -> String {
    compose_system_instructions_with_user_context(agent_instructions, manifest, output_policy, None)
}

/// Full system prompt composition with optional user context injection.
pub(crate) fn compose_system_instructions_with_user_context(
    agent_instructions: &str,
    manifest: &AgentManifest,
    output_policy: Option<&autonoetic_types::agent::OutputPolicy>,
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

    let contract_section = {
        let mut lines: Vec<String> = Vec::new();

        if let Some(schema) = manifest
            .io
            .as_ref()
            .and_then(|io| io.returns.as_ref())
        {
            if let Ok(compact) = serde_json::to_string(schema) {
                lines.push(format!("- **io.returns** (your reply must conform): `{compact}`"));
            }
        }

        if let Some(policy) = output_policy {
            if !policy.required_artifacts.is_empty() {
                lines.push(format!(
                    "- **required_artifacts**: {}",
                    policy.required_artifacts.join(", ")
                ));
            }
            if let Some(n) = policy.max_artifacts {
                lines.push(format!("- **max_artifacts**: {n}"));
            }
            if let Some(n) = policy.max_total_size_mb {
                lines.push(format!("- **max_total_size_mb**: {n}"));
            }
            if let Some(n) = policy.max_reply_length_chars {
                lines.push(format!("- **max_reply_length_chars**: {n}"));
            }
            if let Some(n) = policy.min_artifact_builds {
                lines.push(format!(
                    "- **min_artifact_builds**: {n} (durable `artifact.build` trace required)"
                ));
            }
            if !policy.prohibited_text_patterns.is_empty() {
                lines.push(format!(
                    "- **prohibited_text_patterns**: {}",
                    policy.prohibited_text_patterns.join(", ")
                ));
            }
            lines.push(format!(
                "- **validation_max_loops**: {}",
                policy.validation_max_loops
            ));
        }

        if lines.is_empty() {
            None
        } else {
            Some(format!(
                "---\n\nYour Output Contract\n\nThe gateway will validate your final output against these constraints before returning it to the caller. Violating constraints triggers a repair prompt; repairs are bounded by the declared policy.\n\n{}",
                lines.join("\n")
            ))
        }
    };

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

            if filtered.is_null()
                || (filtered.is_object() && filtered.as_object().unwrap().is_empty())
            {
                return None;
            }

            let compact = serde_json::to_string(&filtered).ok()?;
            // Bound to ~2000 chars (~500 tokens)
            let bounded = if compact.len() > 2000 {
                format!("{}...", safe_prefix_by_bytes(&compact, 2000))
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
            output.contains("content_read"),
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
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct Ri06CapabilitySnapshot {
    allowed_tier_names: Vec<String>,
    session_state: autonoetic_types::agent::SessionState,
}

impl Ri06CapabilitySnapshot {
    fn from_filter(
        filter: &crate::runtime::tools::ToolTierFilter,
        session_state: autonoetic_types::agent::SessionState,
    ) -> Self {
        use autonoetic_types::agent::ToolTier;
        let mut names: Vec<&'static str> = if filter.allowed_tiers.is_empty() {
            vec!["core", "workflow", "specialized"]
        } else {
            filter
                .allowed_tiers
                .iter()
                .map(|tier| match tier {
                    ToolTier::Core => "core",
                    ToolTier::Workflow => "workflow",
                    ToolTier::Specialized => "specialized",
                })
                .collect()
        };
        names.sort_unstable();
        names.dedup();
        Self {
            allowed_tier_names: names.into_iter().map(|s| s.to_string()).collect(),
            session_state,
        }
    }

    fn is_subset_of(&self, other: &Self) -> bool {
        self.allowed_tier_names
            .iter()
            .all(|tier| other.allowed_tier_names.contains(tier))
    }

    fn is_strict_subset_of(&self, other: &Self) -> bool {
        self.is_subset_of(other) && self.allowed_tier_names != other.allowed_tier_names
    }
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
    pub session_state: autonoetic_types::agent::SessionState,
    pub degraded_sessions: Option<Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>>,
    pub session_id: Option<String>,
    pub session_started: bool,
    pub turn_counter: u64,
    /// When set, passed to tool execution for config-dependent behavior.
    pub config: Option<Arc<GatewayConfig>>,
    /// Optional per-session LLM/tool/token/wall-clock budgets (shared `Arc` across spawns).
    pub session_budget: Option<Arc<SessionBudgetRegistry>>,
    pub root_session_budget:
        Option<Arc<crate::runtime::root_session_budget::RootSessionBudgetRegistry>>,
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
    /// Whether runtime-lock drift has already been checked this session.
    pub drift_checked: bool,
    /// Emergency-stop hooks (sandbox PIDs, etc.); same registry as [`crate::execution::GatewayExecutionService`].
    pub active_executions:
        Option<Arc<crate::runtime::active_execution_registry::ActiveExecutionRegistry>>,
    /// Shared live digest (`digest.md`) when `gateway_dir` is set.
    pub live_digest: Option<Arc<std::sync::Mutex<crate::runtime::live_digest::LiveDigestWriter>>>,
    /// Shared structured live/session report written beside `digest.md`.
    pub live_report:
        Option<Arc<std::sync::Mutex<crate::runtime::session_report::SessionReportWriter>>>,
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
    /// Artifact ID whose layers should be auto-mounted into sandbox.exec calls.
    /// Set when a parent agent spawns this agent with an artifact reference
    /// (typically for evaluator sessions that need packager's dependency layers).
    pub artifact_id: Option<String>,
    /// Previous turn's Ri-0.6 capability snapshot for narrowing checks.
    ri_0_6_previous_snapshot: Option<Ri06CapabilitySnapshot>,
}

fn tool_result_counts_as_progress(result: &str) -> bool {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result) {
        if let Some(ok) = parsed.get("ok").and_then(|v| v.as_bool()) {
            return ok;
        }
        if let Some(approval_required) = parsed.get("approval_required").and_then(|v| v.as_bool()) {
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

fn load_manifest_loop_guard_declaration(agent_dir: &Path) -> Option<LoopGuardDeclaration> {
    let skill_path = agent_dir.join("SKILL.md");
    let skill = std::fs::read_to_string(skill_path).ok()?;
    let frontmatter = skill.split("---").nth(1)?;
    let root = serde_yaml::from_str::<serde_yaml::Value>(frontmatter).ok()?;

    let direct = root.get("loop_guard").cloned();
    let nested = root
        .get("metadata")
        .and_then(|m| m.get("autonoetic"))
        .and_then(|a| a.get("loop_guard"))
        .cloned();

    direct
        .or(nested)
        .and_then(|v| serde_yaml::from_value::<LoopGuardDeclaration>(v).ok())
}

fn effective_loop_guard_config(
    system: &LoopGuardConfig,
    declaration: Option<&LoopGuardDeclaration>,
) -> LoopGuardConfig {
    let Some(decl) = declaration else {
        return system.clone();
    };

    let mut effective = system.clone();
    if let Some(v) = decl.max_loops_without_progress {
        effective.max_loops_without_progress = v.min(system.max_loops_without_progress);
    }
    if let Some(v) = decl.max_tool_failures {
        effective.max_tool_failures = v.min(system.max_tool_failures);
    }
    if let Some(v) = decl.max_consecutive_same_progress {
        effective.max_consecutive_same_progress = v.min(system.max_consecutive_same_progress);
    }
    if let Some(v) = decl.max_child_failures {
        effective.max_child_failures = v.min(system.max_child_failures);
    }
    effective
}

fn loop_guard_from_config_and_manifest(config: Option<&GatewayConfig>, agent_dir: &Path) -> LoopGuard {
    match config {
        Some(cfg) => {
            let declaration = load_manifest_loop_guard_declaration(agent_dir);
            let effective = effective_loop_guard_config(&cfg.loop_guard, declaration.as_ref());
            LoopGuard::with_config(&effective)
        }
        None => LoopGuard::new(5),
    }
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
            session_state: autonoetic_types::agent::SessionState::Normal,
            degraded_sessions: None,
            session_id: None,
            session_started: false,
            turn_counter: 0,
            config: None,
            session_budget: None,
            root_session_budget: None,
            middleware: manifest.middleware.clone().unwrap_or_default(),
            llm_usage_last_run: Vec::new(),
            openrouter_catalog: None,
            gateway_store,
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            drift_checked: false,
            active_executions: None,
            live_digest: None,
            live_report: None,
            last_history: Vec::new(),
            session_started_at: None,
            compression_metadata: Default::default(),
            http_client: reqwest::Client::new(),
            user_id: None,
            artifact_id: None,
            ri_0_6_previous_snapshot: None,
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
        self.guard = loop_guard_from_config_and_manifest(Some(config.as_ref()), &self.agent_dir);
        self.config = Some(config);
        self
    }

    pub fn with_session_budget(mut self, registry: Option<Arc<SessionBudgetRegistry>>) -> Self {
        self.session_budget = registry;
        self
    }

    pub fn with_root_session_budget(
        mut self,
        registry: Option<Arc<crate::runtime::root_session_budget::RootSessionBudgetRegistry>>,
    ) -> Self {
        self.root_session_budget = registry;
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

    pub fn with_artifact_id(mut self, artifact_id: Option<String>) -> Self {
        self.artifact_id = artifact_id;
        self
    }

    pub fn with_degraded_sessions(mut self, set: Option<Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>>) -> Self {
        self.degraded_sessions = set;
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

    fn has_pending_approvals(&self) -> bool {
        let (Some(cfg), Some(session_id)) = (self.config.as_ref(), self.session_id.as_ref()) else {
            return false;
        };
        let root = crate::runtime::content_store::root_session_id(session_id);
        crate::scheduler::approval::pending_approval_requests_for_root(
            cfg,
            self.gateway_store.as_deref(),
            root,
        )
        .map(|p| !p.is_empty())
        .unwrap_or(false)
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
        let mut request = ApprovalRequest {
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
            decision_reason: None,
            approval_level: crate::scheduler::approval::resolve_approval_level(cfg, &action),
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
        };
        store.create_approval(&mut request)?;
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
        if let Some(r) = self.live_report.take() {
            if let Ok(mut g) = r.lock() {
                let latest_assistant = self
                    .last_history
                    .iter()
                    .rev()
                    .find(|m| matches!(m.role, crate::llm::Role::Assistant))
                    .map(|m| m.content.as_str());
                let _ = g.finish_session(reason, latest_assistant);
            }
        }
        let mut tracer = SessionTracer::new(&self.agent_dir, &self.manifest.agent.id, &session_id)?;
        tracer.log_session_end(reason);
        self.session_started = false;
        self.session_id = None;
        self.turn_counter = 0;
        self.ri_0_6_previous_snapshot = None;
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
            session_state: self.session_state,
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

    /// Compose, sign, and render the R++1 state-attestation tail for the
    /// current turn. Returns:
    ///   - `Ok(Some(tail))` whenever the gateway has a directory to keep
    ///     the identity key in (the production path);
    ///   - `Ok(None)` when `gateway_dir` is unset (some unit-test paths
    ///     run an executor without persistent state — there is no key to
    ///     sign with and no operational state to attest to);
    ///   - `Err(_)` fail-shut whenever the key file is malformed or the
    ///     filesystem refuses to honour the strict permissions. The
    ///     surrounding turn must abort rather than proceed without a
    ///     trustworthy attestation.
    fn build_state_attestation_tail(&self) -> anyhow::Result<Option<String>> {
        let Some(gateway_dir) = self.gateway_dir.as_ref() else {
            return Ok(None);
        };
        let key = crate::runtime::crypto::GatewayIdentityKey::load_or_generate(gateway_dir)?;

        let pending_approval_ids = self
            .session_id
            .as_ref()
            .and_then(|sid| self.config.as_ref().map(|cfg| (cfg.as_ref(), sid.as_str())))
            .map(|(cfg, sid)| {
                crate::scheduler::approval::pending_approval_requests_for_session(
                    cfg,
                    self.gateway_store.as_deref(),
                    sid,
                )
                .map(|reqs| reqs.into_iter().map(|r| r.request_id).collect::<Vec<_>>())
            })
            .transpose()?
            .unwrap_or_default();

        let budget_meters = self.snapshot_budget_meters();
        let gateway_node_id =
            std::env::var("AUTONOETIC_NODE_ID").unwrap_or_else(|_| "gateway".to_string());

        let attestation = crate::runtime::state_attestation::compose_and_sign(
            crate::runtime::state_attestation::AttestationInputs {
                agent_id: &self.manifest.agent.id,
                session_id: self.session_id.as_deref(),
                root_session_id: self.root_session_id_opt(),
                turn_counter: self.turn_counter,
                manifest: &self.manifest,
                gateway_node_id: &gateway_node_id,
                pending_approval_ids,
                budget_meters,
            },
            &key,
        )?;

        Ok(Some(crate::runtime::state_attestation::render_tail(
            &attestation,
        )?))
    }

    /// Best-effort budget snapshot for the attestation block. Pulls usage
    /// from the per-session registry and pairs it with the configured
    /// limit (when one exists). Returns an empty list when budgets are
    /// disabled or counters have not been observed yet for this session.
    fn snapshot_budget_meters(&self) -> Vec<crate::runtime::state_attestation::BudgetMeter> {
        use crate::runtime::state_attestation::BudgetMeter;
        let mut meters = Vec::new();
        let session_id = match self.session_id.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => return meters,
        };
        let Some(reg) = self.session_budget.as_ref() else {
            return meters;
        };
        let Some(cfg) = self.config.as_ref() else {
            return meters;
        };
        let limits = &cfg.session_budget;
        if let Some((rounds, tokens, cost)) = reg.snapshot_counters(session_id) {
            meters.push(BudgetMeter {
                name: "llm_rounds".to_string(),
                used: rounds as f64,
                limit: limits.max_llm_rounds.map(|x| x as f64),
            });
            meters.push(BudgetMeter {
                name: "llm_tokens".to_string(),
                used: tokens as f64,
                limit: limits.max_llm_tokens.map(|x| x as f64),
            });
            meters.push(BudgetMeter {
                name: "session_price_usd".to_string(),
                used: cost,
                limit: limits.max_session_price_usd,
            });
        }
        meters
    }

    async fn enforce_cost_catalog_preflight(
        &self,
        model_id: &str,
        allow_unpriced_budget: bool,
    ) -> anyhow::Result<()> {
        if allow_unpriced_budget {
            return Ok(());
        }
        let Some(cfg) = self.config.as_ref() else {
            return Ok(());
        };
        let session_price_cap_enabled = cfg
            .session_budget
            .max_session_price_usd
            .is_some_and(|v| v >= 0.0);
        let root_price_cap_enabled = cfg
            .root_session_budget
            .max_session_price_usd
            .is_some_and(|v| v >= 0.0);
        if !session_price_cap_enabled && !root_price_cap_enabled {
            return Ok(());
        }

        let mode = crate::fail_mode::lookup_fail_mode("R-6.5")
            .map(|m| m.to_string())
            .unwrap_or_else(|| "refuse-session-start".to_string());
        let Some(catalog) = self.openrouter_catalog.as_ref() else {
            anyhow::bail!(
                "Session start refused: cost-budget enforcement requires price metadata but \
                 catalog is unavailable (R-6.5, R++10: fail-mode={}). \
                 Add capability 'budget.no_price_available.allow' to override intentionally.",
                mode
            );
        };
        if catalog.estimate_cost_usd(model_id, 1, 1).await.is_none() {
            anyhow::bail!(
                "Session start refused: cost-budget enforcement requires price metadata for model '{}' \
                 but catalog is unavailable (R-6.5, R++10: fail-mode={}). \
                 Add capability 'budget.no_price_available.allow' to override intentionally.",
                model_id,
                mode
            );
        }
        Ok(())
    }

    fn root_session_id_opt(&self) -> Option<&str> {
        self.session_id
            .as_deref()
            .map(crate::runtime::content_store::root_session_id)
    }

    fn build_ri_0_6_capability_snapshot(&self) -> Ri06CapabilitySnapshot {
        let filter = determine_tool_tier_filter(
            &self.manifest,
            self.session_id.as_deref(),
            false,
            self.session_state,
        );
        Ri06CapabilitySnapshot::from_filter(&filter, self.session_state)
    }

    fn resolve_ri_0_6_narrowing_path(&self, session_id: &str) -> anyhow::Result<&'static str> {
        anyhow::ensure!(
            self.session_state == autonoetic_types::agent::SessionState::Degraded,
            "Ri-0.6 violation: capability narrowing requires degraded mode (session='{}')",
            session_id
        );
        let store = self.gateway_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Ri-0.6 violation: capability narrowing requires gateway store evidence (session='{}')",
                session_id
            )
        })?;
        let degraded_events: Vec<_> = store
            .search_causal_events(Some(session_id), None, 128)?
            .into_iter()
            .filter(|e| e.category == "session" && e.action == "session.degraded")
            .collect();
        anyhow::ensure!(
            !degraded_events.is_empty(),
            "Ri-0.6 violation: narrowing detected without session.degraded causal event (session='{}')",
            session_id
        );

        let mut saw_operator_source = false;
        for event in degraded_events {
            anyhow::ensure!(
                !event.enforced_rules.is_empty(),
                "Ri-0.6 violation: session.degraded event '{}' has no enforced rules",
                event.event_id
            );
            if let Some(payload_raw) = event.payload.as_deref() {
                let payload: serde_json::Value = serde_json::from_str(payload_raw).map_err(|e| {
                    anyhow::anyhow!(
                        "Ri-0.6 violation: session.degraded event '{}' has invalid JSON payload: {}",
                        event.event_id,
                        e
                    )
                })?;
                if payload
                    .get("source")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "operator")
                    .unwrap_or(false)
                {
                    saw_operator_source = true;
                }
            }
        }

        Ok(if saw_operator_source {
            "operator_command"
        } else {
            "degraded_mode"
        })
    }

    fn check_ri_0_6_turn_snapshot(&mut self, session_id: &str, turn_id: &str) -> anyhow::Result<()> {
        let current = self.build_ri_0_6_capability_snapshot();
        let Some(previous) = self.ri_0_6_previous_snapshot.clone() else {
            self.ri_0_6_previous_snapshot = Some(current);
            return Ok(());
        };

        let current_subset_of_previous = current.is_subset_of(&previous);
        let previous_subset_of_current = previous.is_subset_of(&current);

        if current.is_strict_subset_of(&previous) {
            let narrowing_path = self.resolve_ri_0_6_narrowing_path(session_id)?;
            let store = self.gateway_store.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Ri-0.6 violation: capability narrowing event could not be recorded (gateway store unavailable)"
                )
            })?;
            store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
                event_id: format!("ri06-{}", uuid::Uuid::new_v4()),
                agent_id: self.manifest.agent.id.clone(),
                session_id: session_id.to_string(),
                turn_id: Some(turn_id.to_string()),
                event_seq: 0,
                timestamp: chrono::Utc::now().to_rfc3339(),
                category: "session".to_string(),
                action: "session.capability_narrowed".to_string(),
                status: "active".to_string(),
                enforced_rules: vec!["Ri-0.6".to_string()],
                target: None,
                payload: Some(
                    serde_json::json!({
                        "narrowing_path": narrowing_path,
                        "previous_allowed_tiers": previous.allowed_tier_names,
                        "current_allowed_tiers": current.allowed_tier_names,
                        "previous_session_state": previous.session_state,
                        "current_session_state": current.session_state,
                    })
                    .to_string(),
                ),
                payload_ref: None,
                evidence_ref: None,
                reason: None,
            })?;
        } else if !current_subset_of_previous && !previous_subset_of_current {
            anyhow::bail!(
                "Ri-0.6 violation: capability tier set changed outside subset/superset relation \
                 (session='{}', previous={:?}, current={:?})",
                session_id,
                previous.allowed_tier_names,
                current.allowed_tier_names
            );
        }

        self.ri_0_6_previous_snapshot = Some(current);
        Ok(())
    }

    /// Build Ri-0.5 degraded-mode notice text injected into the system prompt
    /// before the next turn executes.
    ///
    /// Constitutional requirement:
    /// - agent is told it is degraded,
    /// - rule IDs are explicit,
    /// - trigger evidence is explicit.
    fn build_degradation_notice_tail(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        if self.session_state != autonoetic_types::agent::SessionState::Degraded {
            return Ok(None);
        }

        let store = self.gateway_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Ri-0.5 violation: degraded session '{}' has no gateway store for evidence lookup",
                session_id
            )
        })?;

        let degraded_event = store
            .search_causal_events(Some(session_id), None, 128)?
            .into_iter()
            .find(|event| event.category == "session" && event.action == "session.degraded")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Ri-0.5 violation: degraded session '{}' missing session.degraded causal event",
                    session_id
                )
            })?;

        anyhow::ensure!(
            !degraded_event.enforced_rules.is_empty(),
            "Ri-0.5 violation: session.degraded event '{}' has no enforced rule IDs",
            degraded_event.event_id
        );

        let evidence = degraded_event
            .payload
            .clone()
            .or_else(|| {
                degraded_event
                    .reason
                    .clone()
                    .map(|reason| serde_json::json!({ "reason": reason }).to_string())
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Ri-0.5 violation: session.degraded event '{}' has no evidence payload",
                    degraded_event.event_id
                )
            })?;

        let rules = degraded_event.enforced_rules.join(", ");
        Ok(Some(format!(
            "---\n\nDegradation Notice (Ri-0.5)\n\n\
             This session is in degraded mode before this turn executes.\n\
             Rule IDs: {}\n\
             Evidence Event: {}\n\
             Evidence: {}\n",
            rules, degraded_event.event_id, evidence
        )))
    }

    /// When an Ri-0.9 last-word gateway notice was injected this wake and the
    /// turn completes, persist `session.last_word_response` referencing the notice
    /// message IDs plus a disclosure-filtered excerpt of the assistant reply.
    fn record_ri09_last_word_response_if_applicable(
        &self,
        session_id: &str,
        turn_id: &str,
        notice_message_ids: &[String],
        assistant_reply: Option<&str>,
    ) {
        if notice_message_ids.is_empty() {
            return;
        }
        let Some(store) = self.gateway_store.as_ref() else {
            tracing::debug!(
                target: "ri_0_9",
                session_id = %session_id,
                "Ri-0.9 last-word response not recorded: no gateway store"
            );
            return;
        };
        const MAX_PREVIEW: usize = 4096;
        let trimmed = assistant_reply.map(|s| s.trim()).filter(|s| !s.is_empty());
        let preview = trimmed.map(|t| {
            if t.len() <= MAX_PREVIEW {
                t.to_string()
            } else {
                let mut end = MAX_PREVIEW;
                while end > 0 && !t.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &t[..end])
            }
        });
        let record = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("ri09resp-{}", uuid::Uuid::new_v4()),
            agent_id: self.manifest.agent.id.clone(),
            session_id: session_id.to_string(),
            turn_id: Some(turn_id.to_string()),
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "session".to_string(),
            action: "session.last_word_response".to_string(),
            status: "active".to_string(),
            enforced_rules: vec!["Ri-0.9".to_string()],
            target: None,
            payload: Some(
                serde_json::json!({
                    "notice_message_ids": notice_message_ids,
                    "assistant_reply_present": trimmed.is_some(),
                    "assistant_reply_preview": preview,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        };
        if let Err(e) = store.as_ref().create_causal_event(&record) {
            tracing::warn!(
                target: "ri_0_9",
                error = %e,
                session_id = %session_id,
                "Failed to persist session.last_word_response"
            );
        }
    }

    /// Run the agent loop until completion or guard trip.
    pub async fn execute_loop(&mut self) -> anyhow::Result<()> {
        let user_context = self.build_user_context_snippet();
        let mut system_instructions = compose_system_instructions_with_user_context(
            &self.instructions,
            &self.manifest,
            self.manifest
                .io
                .as_ref()
                .and_then(|io| io.output_policy.as_ref()),
            user_context.as_deref(),
        );
        if let Some(tail) = self.build_state_attestation_tail()? {
            system_instructions.push_str("\n\n");
            system_instructions.push_str(&tail);
        }
        let mut history: Vec<Message> = vec![
            Message::system(system_instructions),
            Message::user(self.initial_user_message.clone()),
        ];
        let outcome = self.execute_with_history(&mut history).await;
        self.finalize_execute_loop_result(outcome)
    }

    fn finalize_execute_loop_result(
        &mut self,
        outcome: anyhow::Result<TurnOutcome>,
    ) -> anyhow::Result<()> {
        match outcome {
            Ok(outcome) => {
                // Suspension outcomes already have checkpoints; this helper is the
                // single exit path for execute_loop-level session termination.
                let termination = ExecuteLoopTermination::from_turn_outcome(&outcome);
                let _ = self.close_session(termination.close_reason());
                Ok(())
            }
            Err(e) => {
                let _ = self.close_session(ExecuteLoopTermination::FatalError.close_reason());
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
        self.guard = loop_guard_from_config_and_manifest(self.config.as_deref(), &self.agent_dir);
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
                match crate::runtime::live_digest::LiveDigestWriter::open(
                    gw,
                    &session_id,
                    agent_id,
                    self.task_id.as_deref(),
                    self.workflow_id.as_deref(),
                )
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
            if self.live_report.is_none() {
                let agent_id = &self.manifest.agent.id;
                match crate::runtime::session_report::SessionReportWriter::open(
                    gw,
                    &session_id,
                    agent_id,
                ) {
                    Ok(w) => {
                        self.live_report = Some(Arc::new(std::sync::Mutex::new(w)));
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "session_report",
                            session_id = %session_id,
                            error = %e,
                            "Failed to open session report writer"
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
            if let Some(lr) = self.live_report.clone() {
                t = t.with_session_report(lr);
            }
            if let Some(gs) = self.gateway_store.clone() {
                t = t.with_gateway_store(Some(gs));
            }
            t
        };

        let active_agent_dir = self.agent_dir.clone();

        if !self.drift_checked {
            match crate::runtime_lock::check_runtime_lock_drift(&self.agent_dir) {
                crate::runtime_lock::DriftCheckResult::Clean => {}
                crate::runtime_lock::DriftCheckResult::Drift(drift) => {
                    let allow = self
                        .config
                        .as_ref()
                        .map_or(false, |c| c.allow_runtime_lock_drift);
                    let status = if allow {
                        autonoetic_types::causal_chain::EntryStatus::Success
                    } else {
                        autonoetic_types::causal_chain::EntryStatus::Error
                    };
                    let drift_field = if drift.locked_binary_sha256.is_some() {
                        "binary_sha256"
                    } else {
                        "build_sha256"
                    };
                    let _ = tracer.log_event(
                        "runtime_lock_drift",
                        if allow { "override" } else { "rejected" },
                        status,
                        Some(serde_json::json!({
                            "drift_field": drift_field,
                            "locked_build_sha256": drift.locked_build_sha256,
                            "current_build_sha256": drift.current_build_sha256,
                            "locked_binary_sha256": drift.locked_binary_sha256,
                            "current_binary_sha256": drift.current_binary_sha256,
                            "override": allow,
                        })),
                    );
                    if !allow {
                        let mut msg = format!("runtime lock drift detected ({drift_field}): ");
                        if drift.locked_binary_sha256.is_some() {
                            msg.push_str(&format!(
                                "binary SHA locked={:?}, current={:?}. ",
                                drift.locked_binary_sha256, drift.current_binary_sha256,
                            ));
                        }
                        msg.push_str(&format!(
                            "build SHA locked={}, current={}. \
                             Set allow_runtime_lock_drift=true in config to override.",
                            drift.locked_build_sha256, drift.current_build_sha256,
                        ));
                        anyhow::bail!("{}", msg);
                    }
                }
                crate::runtime_lock::DriftCheckResult::Skipped(reason) => {
                    let (action, detail): (&str, &str) = match &reason {
                        crate::runtime_lock::DriftSkippedReason::LockAbsent => {
                            ("lock_absent", "runtime.lock not found in agent dir")
                        }
                        crate::runtime_lock::DriftSkippedReason::LockMalformed(e) => {
                            ("lock_malformed", e.as_str())
                        }
                    };
                    let _ = tracer.log_event(
                        "runtime_lock_drift",
                        action,
                        autonoetic_types::causal_chain::EntryStatus::Success,
                        Some(serde_json::json!({
                            "detail": detail,
                        })),
                    );
                }
            }
            self.drift_checked = true;
        }

        let mut ri_0_9_notice_message_ids: Vec<String> = Vec::new();

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
        // --- Auto-inject Agent Messages ---
        if let Some(store) = self.gateway_store.as_ref() {
            if let Ok(msgs) = store.fetch_undelivered_messages(&session_id) {
                for msg in msgs {
                    if msg.sender_agent_id == "gateway" && msg.message.contains("[Gateway Notice Ri-0.9]")
                    {
                        ri_0_9_notice_message_ids.push(msg.message_id.clone());
                    }
                    let text = format!(
                        "[Direct Message from Agent '{}' (Session: {})]\n{}",
                        msg.sender_agent_id, msg.sender_session_id, msg.message
                    );
                    history.push(Message::user(text.clone()));

                    let _ = tracer.log_event(
                        "agent_message",
                        "received",
                        autonoetic_types::causal_chain::EntryStatus::Success,
                        Some(serde_json::json!({
                            "message_id": msg.message_id,
                            "sender_agent_id": msg.sender_agent_id,
                            "sender_session_id": msg.sender_session_id,
                        })),
                    );

                    let _ = store.mark_message_delivered(&msg.message_id, &session_id);
                }
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
        // Set when compact_workflow_summary runs at hibernate so assistant_reply / persisted
        // history show workflow progress (system prompt injection alone is UI-invisible).
        let mut workflow_transcript_supplement: Option<String> = None;
        let policy = PolicyEngine::new(self.manifest.clone());
        let max_empty_other_retries = max_other_empty_retries();
        let mut empty_other_retries_used = 0usize;
        let mut digest_turn_active = false;
        let mut ri_0_6_snapshot_checked = false;
        let root_session_id = crate::runtime::content_store::root_session_id(&session_id);
        let allow_unpriced_budget = self.manifest.capabilities.iter().any(|c| {
            matches!(
                c,
                autonoetic_types::capability::Capability::BudgetNoPriceAvailableAllow
            )
        });

        loop {
            // Loop guard check — save checkpoint before propagating max-turns error
            if let Err(e) = self.guard.check_loop() {
                let cp =
                    self.build_checkpoint(history, &turn_id, YieldReason::MaxTurnsReached, None);
                self.save_checkpoint_if_possible(&cp);
                return Err(e);
            }

            if self.session_state == autonoetic_types::agent::SessionState::Normal
                && self.guard.is_sub_trip_warning()
            {
                self.session_state = autonoetic_types::agent::SessionState::Degraded;
                if let Some(store) = self.gateway_store.as_ref() {
                    let session_id_for_event = self.session_id.clone().unwrap_or_default();
                    let event = autonoetic_types::causal_chain::CausalEventRecord {
                        event_id: format!("subtrip-{}", uuid::Uuid::new_v4()),
                        agent_id: self.manifest.agent.id.clone(),
                        session_id: session_id_for_event,
                        turn_id: None,
                        event_seq: 0,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        category: "session".to_string(),
                        action: "session.degraded".to_string(),
                        status: "active".to_string(),
                        enforced_rules: vec!["R++6".to_string()],
                        target: None,
                        payload: Some(serde_json::json!({"reason": "loop_guard_sub_trip_warning"}).to_string()),
                        payload_ref: None,
                        evidence_ref: None,
                        reason: Some("loop_guard_sub_trip_warning".to_string()),
                    };
                    let _ = store.create_causal_event(&event);
                }
                if let Some(ds) = self.degraded_sessions.as_ref() {
                    ds.lock().await.insert(session_id.clone());
                }
            }

            if let Some(ds) = self.degraded_sessions.as_ref() {
                let in_set = ds.lock().await.contains(&session_id);
                if in_set && self.session_state == autonoetic_types::agent::SessionState::Normal {
                    self.session_state = autonoetic_types::agent::SessionState::Degraded;
                } else if !in_set && self.session_state == autonoetic_types::agent::SessionState::Degraded {
                    self.session_state = autonoetic_types::agent::SessionState::Normal;
                }
            }

            if !ri_0_6_snapshot_checked {
                if let Err(e) = self.check_ri_0_6_turn_snapshot(&session_id, &turn_id) {
                    let cp = self.build_checkpoint(
                        history,
                        &turn_id,
                        YieldReason::Error(e.to_string()),
                        None,
                    );
                    self.save_checkpoint_if_possible(&cp);
                    return Err(e);
                }
                ri_0_6_snapshot_checked = true;
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

            // Root session tree budget check (R+4 / R-6.21)
            if let Some(root_budget) = self.root_session_budget.as_ref() {
                if let Err(e) = root_budget.check_pre_llm(root_session_id) {
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
            let mut system_instructions = compose_system_instructions_with_user_context(
                &self.instructions,
                &self.manifest,
                self.manifest
                    .io
                    .as_ref()
                    .and_then(|io| io.output_policy.as_ref()),
                user_context.as_deref(),
            );
            if let Some(notice) = self.build_degradation_notice_tail(&session_id)? {
                system_instructions.push_str("\n\n");
                system_instructions.push_str(&notice);
            }
            // R++1: re-sign the state-attestation tail every turn so the
            // facts in the block (turn counter, pending approvals, budget)
            // reflect the current state, not last-turn's snapshot.
            if let Some(tail) = self.build_state_attestation_tail()? {
                system_instructions.push_str("\n\n");
                system_instructions.push_str(&tail);
            }

            // Remove any existing system messages (could be stale from previous turns)
            history.retain(|m| !matches!(m.role, crate::llm::Role::System));

            // Insert fresh system message at the front
            history.insert(0, Message::system(&system_instructions));

            let tools: Vec<ToolDefinition> = {
                let pending_approvals = self.has_pending_approvals();
                let tier_filter = determine_tool_tier_filter(
                    &self.manifest,
                    self.session_id.as_deref(),
                    pending_approvals,
                    self.session_state,
                );
                let mut t: Vec<ToolDefinition> = mcp_runtime
                    .tool_definitions()?
                    .into_iter()
                    .filter(|def| policy.can_invoke_tool(&def.name).is_allowed())
                    .filter(|def| tier_filter.allows(&def.name))
                    .collect();
                t.extend(
                    self.registry
                        .available_definitions_filtered(&self.manifest, Some(&tier_filter)),
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
                    reasoning_content: None,
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
                if let Err(e) = self
                    .enforce_cost_catalog_preflight(&actual_model, allow_unpriced_budget)
                    .await
                {
                    let cp =
                        self.build_checkpoint(history, &turn_id, YieldReason::BudgetExhausted, None);
                    self.save_checkpoint_if_possible(&cp);
                    return Err(e);
                }
                if let Some(root_budget) = self.root_session_budget.as_ref() {
                    if let Err(e) = root_budget.reserve_llm_round(root_session_id) {
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
                            if let Err(e) = self
                                .enforce_cost_catalog_preflight(fb_model, allow_unpriced_budget)
                                .await
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
                            if let Some(root_budget) = self.root_session_budget.as_ref() {
                                if let Err(e) = root_budget.reserve_llm_round(root_session_id) {
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
                    if let Err(e) = budget.record_llm_completion_with_unpriced_override(
                        &session_id,
                        response.usage.input_tokens,
                        response.usage.output_tokens,
                        estimated_cost_usd,
                        allow_unpriced_budget,
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

            if let Some(root_budget) = self.root_session_budget.as_ref() {
                if !skip_llm {
                    if let Err(e) = root_budget.record_llm_completion_with_unpriced_override(
                        root_session_id,
                        response.usage.input_tokens,
                        response.usage.output_tokens,
                        estimated_cost_usd,
                        allow_unpriced_budget,
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
                response.reasoning_content.as_deref(),
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
                    assistant_msg.reasoning_content = response.reasoning_content.clone();
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

                    if let Some(root_budget) = self.root_session_budget.as_ref() {
                        let root =
                            crate::runtime::content_store::root_session_id(&session_id).to_string();
                        if let Err(e) = root_budget
                            .reserve_tool_invocations(&root, response.tool_calls.len() as u64)
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
                            live_report: self.live_report.clone(),
                            user_id: self.user_id.clone(),
                            artifact_id: self.artifact_id.clone(),
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
                    .with_session_context(self.session_id.clone(), Some(turn_id.clone()))
                    .with_session_state(self.session_state);

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

                        let pending_action = match self.gateway_store.as_ref() {
                            Some(store) => {
                                let approval = store.get_approval(&request_id).map_err(|e| {
                                    anyhow::anyhow!(
                                        "failed to fetch approval {} while saving continuation: {}",
                                        request_id,
                                        e
                                    )
                                })?;
                                let approval = approval.ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "missing approval {} while saving continuation",
                                        request_id
                                    )
                                })?;
                                Some(approval.action)
                            }
                            None => None,
                        };

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
                            pending_action,
                            workflow_id: self.workflow_id.clone(),
                            task_id: self.task_id.clone(),
                            session_id: session_id.clone(),
                            turn_id: turn_id.clone(),
                            suspended_at: chrono::Utc::now().to_rfc3339(),
                            loop_guard_state: self.guard.snapshot(),
                            session_state: self.session_state,
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
                                let error_type = parsed.get("error_type")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| match s {
                                        "validation" => Some(autonoetic_types::tool_error::ToolErrorType::Validation),
                                        "permission" => Some(autonoetic_types::tool_error::ToolErrorType::Permission),
                                        "resource" => Some(autonoetic_types::tool_error::ToolErrorType::Resource),
                                        "execution" => Some(autonoetic_types::tool_error::ToolErrorType::Execution),
                                        "fatal" => Some(autonoetic_types::tool_error::ToolErrorType::Fatal),
                                        "conflict" => Some(autonoetic_types::tool_error::ToolErrorType::Conflict),
                                        "quota_exceeded" => Some(autonoetic_types::tool_error::ToolErrorType::QuotaExceeded),
                                        "not_found" => Some(autonoetic_types::tool_error::ToolErrorType::NotFound),
                                        "timeout" => Some(autonoetic_types::tool_error::ToolErrorType::Timeout),
                                        _ => None,
                                    });
                                if let Some(tc) = response.tool_calls.iter().find(|tc| tc.id == *id)
                                {
                                    self.guard.register_failure(
                                        &tc.name,
                                        &tc.arguments,
                                        error_type.as_ref(),
                                    );
                                }
                            } else if tool_result_counts_as_progress(result) {
                                if let Some(tc) = response.tool_calls.iter().find(|tc| tc.id == *id)
                                {
                                    self.guard.register_progress(&tc.name, &tc.arguments);
                                }
                            }
                            if parsed.get("any_failed") == Some(&serde_json::Value::Bool(true)) {
                                self.guard.register_child_failure();
                            }
                        }
                    }

                    // Keep the transcript index current for live diagnostics such as
                    // session_peek while a child agent is still tool-stepping.
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
                            tracing::warn!("Failed to persist history after tool batch: {}", e);
                        }
                    }

                    let _ = tracer.end_digest_turn();
                    digest_turn_active = false;
                }
                StopReason::EndTurn | StopReason::StopSequence => {
                    if !response.text.trim().is_empty() {
                        let mut assistant_msg = Message::assistant(response.text.clone());
                        assistant_msg.reasoning_content = response.reasoning_content.clone();
                        history.push(assistant_msg);
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

                            // Surface workflow state in the transcript and JSON-RPC `assistant_reply`
                            // (system injection alone is invisible to typical chat UIs).
                            let planner_empty = response.text.trim().is_empty();
                            let note =
                                workflow_status_user_message_for_chat(&summary, planner_empty);
                            let note = disclosure_state.filter_reply(&note);
                            history.push(Message::assistant(note.clone()));
                            workflow_transcript_supplement = Some(note);
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
                                        format!("{}…", safe_prefix_by_bytes(planner_intent, 200))
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
                        let mut assistant_msg = Message::assistant(response.text.clone());
                        assistant_msg.reasoning_content = response.reasoning_content.clone();
                        history.push(assistant_msg);
                    }
                    tracer.log_stopped(&format!("{:?}", response.stop_reason));
                    let _ = tracer.end_digest_turn();
                    break;
                }
            }
        }

        let mut reply = latest_assistant_text.map(|t| disclosure_state.filter_reply(&t));
        if let Some(note) = workflow_transcript_supplement {
            reply = match reply {
                None => Some(note),
                Some(t) if t.trim().is_empty() => Some(note),
                Some(t) => Some(format!("{}\n\n{}", t, note)),
            };
        }

        self.record_ri09_last_word_response_if_applicable(
            &session_id,
            &turn_id,
            &ri_0_9_notice_message_ids,
            reply.as_deref(),
        );

        let outcome = Ok(TurnOutcome::Completed(reply));
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

/// Determine the tool tier filter based on agent manifest configuration and
/// runtime workflow state.
///
/// Three inputs drive the filter:
///
/// 1. **Manifest-declared tiers**: agents can declare `allowed_tool_tiers` to
///    permanently restrict their tool surface.
/// 2. **Pending approvals**: when the session (or any session in the same root)
///    has pending approvals, the tool surface is narrowed to Core + Workflow
///    tiers with `always_include_approval_tools: true`. This prevents agents
///    from launching new specialized operations (web search, revision creation,
///    promotion) while waiting for human approval.
/// 3. **Child session handoff**: child agent sessions (session_id contains `/`)
///    get Core-only tools by default, plus the non-core tiers implied by their
///    manifest capabilities.
///
/// Manifest-declared tiers always take precedence over runtime inference — if an
/// agent explicitly restricts itself, the restriction is honoured.
pub fn determine_tool_tier_filter(
    manifest: &AgentManifest,
    session_id: Option<&str>,
    has_pending_approvals: bool,
    session_state: autonoetic_types::agent::SessionState,
) -> crate::runtime::tools::ToolTierFilter {
    if session_state == autonoetic_types::agent::SessionState::Degraded {
        return crate::runtime::tools::ToolTierFilter::core_only();
    }

    if !manifest.allowed_tool_tiers.is_empty() {
        return crate::runtime::tools::ToolTierFilter {
            allowed_tiers: manifest.allowed_tool_tiers.clone(),
            always_include_approval_tools: true,
        };
    }

    if has_pending_approvals {
        return crate::runtime::tools::ToolTierFilter::core_and_workflow_with_approvals();
    }

    let is_child = session_id.map(|sid| sid.contains('/')).unwrap_or(false);
    if is_child {
        return child_tool_tier_filter_for_manifest(manifest);
    }

    crate::runtime::tools::ToolTierFilter::all()
}

fn child_tool_tier_filter_for_manifest(
    manifest: &AgentManifest,
) -> crate::runtime::tools::ToolTierFilter {
    use autonoetic_types::agent::ToolTier;
    use autonoetic_types::capability::Capability;

    let mut allowed_tiers = vec![ToolTier::Core];

    let needs_workflow = manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            Capability::AgentSpawn { .. }
                | Capability::AgentMessage { .. }
                | Capability::SchedulerAccess { .. }
                | Capability::BackgroundReevaluation { .. }
                | Capability::ApprovalQueue { .. }
                | Capability::SchedulerSignal { .. }
                | Capability::Evaluation { .. }
        )
    });
    if needs_workflow {
        allowed_tiers.push(ToolTier::Workflow);
    }

    let needs_specialized = manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            Capability::AgentRevision { .. }
                | Capability::ConstitutionalProposal { .. }
                | Capability::SkillInstall { .. }
                | Capability::CredentialAccess { .. }
                | Capability::UserProfileAccess { .. }
        )
    });
    if needs_specialized {
        allowed_tiers.push(ToolTier::Specialized);
    }

    crate::runtime::tools::ToolTierFilter {
        allowed_tiers,
        always_include_approval_tools: true,
    }
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
    use autonoetic_types::agent::SessionState;
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
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
        }
    }

    #[test]
    fn child_agent_spawn_capability_exposes_workflow_tools() {
        let manifest = manifest_with_capabilities(vec![Capability::AgentSpawn {
            max_children: 10,
            max_spawn_depth: 0,
        }]);
        let filter = determine_tool_tier_filter(
            &manifest,
            Some("root/agent-factory.default-12345678"),
            false,
            SessionState::Normal,
        );

        assert!(filter.allows("content_write"));
        assert!(filter.allows("agent_spawn"));
        assert!(filter.allows("workflow_wait"));
        assert!(!filter.allows("agent_revision_promote"));
    }

    #[test]
    fn child_agent_revision_capability_exposes_revision_tools() {
        let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }]);
        let filter = determine_tool_tier_filter(
            &manifest,
            Some("root/specialized_builder.default-12345678"),
            false,
            SessionState::Normal,
        );

        assert!(filter.allows("content_read"));
        assert!(filter.allows("agent_revision_create_from_intent"));
        assert!(filter.allows("agent_revision_promote"));
    }

    #[test]
    fn test_apply_prompt_budget_warn_passes_through() {
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
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
                name: "content_write".to_string(),
                description: "Write content".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_search".to_string(),
                description: "Search web".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "agent_spawn".to_string(),
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
        assert!(result_tools.iter().any(|t| t.name == "content_write"));
        assert!(result_tools.iter().any(|t| t.name == "agent_spawn"));
        assert!(!result_tools.iter().any(|t| t.name == "web_search"));
        assert_eq!(result_history.len(), history.len());
    }

    #[test]
    fn test_apply_prompt_budget_fail_returns_error() {
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
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
            name: "content_write".to_string(),
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
            name: "content_write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let long_content = "x".repeat(200);

        // Build history with tool-call exchanges that must stay together:
        // [user, assistant+tool_calls(id="tc1"), tool_result(tc1), user, assistant+tool_calls(id="tc2"), tool_result(tc2), user_final]
        let mut assistant_with_tc1 = Message::assistant(long_content.clone());
        assistant_with_tc1.tool_calls = vec![ToolCall {
            id: "tc1".to_string(),
            name: "content_write".to_string(),
            arguments: "{}".to_string(),
        }];

        let mut assistant_with_tc2 = Message::assistant(long_content.clone());
        assistant_with_tc2.tool_calls = vec![ToolCall {
            id: "tc2".to_string(),
            name: "content_write".to_string(),
            arguments: "{}".to_string(),
        }];

        let history = vec![
            Message::system("System prompt".to_string()),
            Message::user(long_content.clone()),
            assistant_with_tc1,
            Message::tool_result("tc1", "content_write", "ok".to_string()),
            Message::user(long_content.clone()),
            assistant_with_tc2,
            Message::tool_result("tc2", "content_write", "ok".to_string()),
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
                name: "content_write".to_string(),
                description: "Write content".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_search".to_string(),
                description: "Search web".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "agent_spawn".to_string(),
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
        assert!(!result_tools.iter().any(|t| t.name == "web_search"));
    }

    #[test]
    fn test_apply_prompt_budget_section_cap_system_prompt_fails_for_trim_history() {
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
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
            name: "content_write".to_string(),
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
        let manifest = manifest_with_capabilities(vec![Capability::AgentSpawn {
            max_children: 5,
            max_spawn_depth: 0,
        }]);
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
                reasoning_content: None,
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
                    reasoning_content: None,
                    stop_reason: StopReason::Other(String::new()),
                    usage: TokenUsage::default(),
                })
            } else {
                Ok(CompletionResponse {
                    text: "recovered reply".to_string(),
                    tool_calls: vec![],
                    reasoning_content: None,
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
            reasoning_content: None,
            stop_reason: StopReason::Other(String::new()),
            usage: TokenUsage::default(),
        };
        assert!(is_retryable_empty_other_response(&retryable));

        let not_retryable = CompletionResponse {
            text: "has text".to_string(),
            tool_calls: vec![],
            reasoning_content: None,
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
                reasoning_content: None,
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
                reasoning_content: None,
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
                None,
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
            registry.extract_metadata("content_read", "{\"name_or_handle\": \"secrets.txt\"}");
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
                    reasoning_content: None,
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
                    reasoning_content: None,
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
            reasoning_content: None,
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
            output_policy: None,
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
            reasoning_content: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };

        executor.log_output_schema_validation(&response, &mut tracer);

        // EndTurn with valid JSON matching schema should pass
        let mut tracer2 = crate::runtime::session_tracer::SessionTracer::test_tracer();
        let response2 = CompletionResponse {
            text: r#"{"result": "success"}"#.to_string(),
            tool_calls: vec![],
            reasoning_content: None,
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

    #[test]
    fn execute_loop_termination_maps_every_turn_outcome_variant() {
        let completed = ExecuteLoopTermination::from_turn_outcome(&TurnOutcome::Completed(None));
        let suspended = ExecuteLoopTermination::from_turn_outcome(&TurnOutcome::Suspended {
            approval_request_id: "apr-1".to_string(),
            continuation: None,
        });
        let user_input =
            ExecuteLoopTermination::from_turn_outcome(&TurnOutcome::SuspendedUserInput {
                interaction_id: "ui-1".to_string(),
            });
        let escalated = ExecuteLoopTermination::from_turn_outcome(&TurnOutcome::Escalated {
            escalation_request_id: "esc-1".to_string(),
        });

        assert_eq!(completed, ExecuteLoopTermination::AgentRequestedExit);
        assert_eq!(suspended, ExecuteLoopTermination::SuspendedForApproval);
        assert_eq!(user_input, ExecuteLoopTermination::SuspendedForUserInput);
        assert_eq!(
            escalated,
            ExecuteLoopTermination::SuspendedForHumanEscalation
        );
    }

    #[test]
    fn execute_loop_termination_reason_tags_are_closed_and_stable() {
        let reasons = vec![
            ExecuteLoopTermination::AgentRequestedExit.close_reason(),
            ExecuteLoopTermination::SuspendedForApproval.close_reason(),
            ExecuteLoopTermination::SuspendedForUserInput.close_reason(),
            ExecuteLoopTermination::SuspendedForHumanEscalation.close_reason(),
            ExecuteLoopTermination::FatalError.close_reason(),
        ];
        assert_eq!(
            reasons,
            vec![
                "execute_loop_complete",
                "execute_loop_suspended",
                "execute_loop_suspended_user_input",
                "execute_loop_escalated",
                "execute_loop_error",
            ]
        );
    }
}

/// User-visible workflow status for chat, turn completion, and JSON-RPC `assistant_reply`.
/// When the model produced no assistant text, include a truncated "last intent" snippet from
/// the compact summary so the user sees what completed.
fn workflow_status_user_message_for_chat(summary: &str, planner_text_empty: bool) -> String {
    let head = summary
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Workflow updated.");
    let mut out = format!("**Workflow status:** {}", head);
    if !planner_text_empty {
        return out;
    }
    if let Some(pos) = summary.find("last intent") {
        let tail = &summary[pos..];
        if let Some(colon) = tail.find(':') {
            let body = tail[colon + 1..].trim();
            if !body.is_empty() {
                const MAX: usize = 1200;
                let snippet = if body.len() > MAX {
                    format!("{}…", safe_prefix_by_bytes(body, MAX))
                } else {
                    body.to_string()
                };
                out.push_str("\n\n");
                out.push_str(&snippet);
            }
        }
    }
    out
}

/// Normalizes a message the same way [`persist_history_to_content_store`] does before
/// persisting, so we can compare fresh turns to already-stored (redacted) rows.
fn normalize_message_for_persist_snapshot(
    msg: &Message,
    disclosure_state: &DisclosureState,
) -> Message {
    let mut m = msg.clone();
    m.content =
        crate::log_redaction::redact_text_for_logs(&disclosure_state.filter_reply(&m.content));
    for tc in &mut m.tool_calls {
        tc.arguments = crate::log_redaction::redact_text_for_logs(
            &disclosure_state.filter_reply(&tc.arguments),
        );
    }
    m
}

/// Longest `k` such that `merged[len-k..]` matches `incoming[0..k]` after normalizing
/// incoming messages to the persisted snapshot form. Avoids re-appending the full in-memory
/// transcript on every hibernate (which duplicated older turns in the content store).
fn longest_history_suffix_prefix_overlap(
    merged: &[Message],
    incoming: &[Message],
    disclosure_state: &DisclosureState,
) -> usize {
    let max_k = merged.len().min(incoming.len());
    for k in (1..=max_k).rev() {
        let suf = &merged[merged.len() - k..];
        let pre = &incoming[..k];
        if suf.iter().zip(pre.iter()).all(|(persisted, fresh)| {
            *persisted == normalize_message_for_persist_snapshot(fresh, disclosure_state)
        }) {
            return k;
        }
    }
    0
}

/// Persists conversation history to content store at diagnostic checkpoints.
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
        // Skip the live system prompt when merging: persisted history already keeps the
        // first system block; the in-memory `history` repeats it every run.
        let incoming_tail: Vec<Message> = history
            .iter()
            .filter(|m| !matches!(m.role, crate::llm::Role::System))
            .cloned()
            .collect();

        let overlap = longest_history_suffix_prefix_overlap(
            &merged_history,
            &incoming_tail,
            disclosure_state,
        );
        merged_history.extend(incoming_tail[overlap..].iter().cloned());
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
                    let prefix = safe_prefix_by_bytes(&line, remaining);
                    if !prefix.is_empty() {
                        parts.push(prefix.to_string());
                    }
                }
                break;
            }
            parts.push(line);
            total += line_len;
        }
    }
    parts.join("\n")
}

fn safe_prefix_by_bytes(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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
            name: "web_fetch".to_string(),
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

    #[test]
    fn extract_searchable_excerpt_handles_unicode_boundary_without_panic() {
        let msg = Message::user(format!("{}{}", "x".repeat(7992), "─"));
        let excerpt = extract_searchable_excerpt(&[msg]);
        assert!(excerpt.len() <= 8000);
    }

    #[test]
    fn sequential_persist_appends_only_new_tail() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir)?;

        let mut tracer = SessionTracer::test_tracer();
        let disclosure = DisclosureState::default();

        let h1 = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi there"),
        ];
        persist_history_to_content_store(
            temp.path(),
            "sess-merge",
            &h1,
            &gateway_dir,
            &mut tracer,
            &disclosure,
            None,
            None,
            None,
        )?;

        // Second hibernate passes the *full* transcript again (simulates executor state).
        let h2 = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::user("next"),
            Message::assistant("done"),
        ];
        persist_history_to_content_store(
            temp.path(),
            "sess-merge",
            &h2,
            &gateway_dir,
            &mut tracer,
            &disclosure,
            None,
            None,
            None,
        )?;

        let store = ContentStore::new(&gateway_dir)?;
        let bytes = store.read_by_name("sess-merge", "session_history")?;
        let persisted: Vec<Message> = serde_json::from_slice(&bytes)?;

        assert_eq!(
            persisted.len(),
            5,
            "expected sys + 4 non-system, no duplicated hello turn"
        );
        assert_eq!(persisted[1].content, "hello");
        assert_eq!(persisted[2].content, "hi there");
        assert_eq!(persisted[3].content, "next");
        assert_eq!(persisted[4].content, "done");
        Ok(())
    }
}

#[cfg(test)]
mod workflow_status_chat_tests {
    use super::workflow_status_user_message_for_chat;

    #[test]
    fn workflow_chat_planner_nonempty_only_headline() {
        let s = "workflow wf-abc · 2 done [RESUMABLE]\n  last intent (v3): long details here";
        let m = workflow_status_user_message_for_chat(s, false);
        assert!(m.starts_with("**Workflow status:**"));
        assert!(m.contains("wf-abc"));
        assert!(!m.contains("long details"));
    }

    #[test]
    fn workflow_chat_planner_empty_includes_intent_snippet() {
        let s = "workflow wf-abc · 2 done [RESUMABLE]\n  last intent (v3): Done with task.";
        let m = workflow_status_user_message_for_chat(s, true);
        assert!(m.contains("wf-abc"));
        assert!(m.contains("Done with task."));
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

#[cfg(test)]
mod tier_filter_tests {
    use super::determine_tool_tier_filter;
    use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration, SessionState, ToolTier};

    fn test_manifest() -> AgentManifest {
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
                name: "test".to_string(),
                description: "test".to_string(),
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
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
        }
    }

    #[test]
    fn test_root_session_no_pending_approvals_allows_all() {
        let manifest = test_manifest();
        let filter = determine_tool_tier_filter(&manifest, Some("root-session"), false, SessionState::Normal);
        assert!(filter.allows("content_write"));
        assert!(filter.allows("web_search"));
        assert!(filter.allows("agent_spawn"));
        assert!(filter.allows("promotion_record"));
    }

    #[test]
    fn test_child_session_core_only_by_default() {
        let manifest = test_manifest();
        let filter = determine_tool_tier_filter(&manifest, Some("root/child-session"), false, SessionState::Normal);
        assert!(filter.allows("content_write"));
        assert!(filter.allows("sandbox_exec"));
        assert!(!filter.allows("web_search"));
        assert!(!filter.allows("agent_spawn"));
        assert!(!filter.allows("promotion_record"));
    }

    #[test]
    fn test_pending_approvals_restricts_to_core_and_workflow() {
        let manifest = test_manifest();
        let filter = determine_tool_tier_filter(&manifest, Some("root-session"), true, SessionState::Normal);
        assert!(filter.allows("content_write"));
        assert!(filter.allows("sandbox_exec"));
        assert!(filter.allows("agent_spawn"));
        assert!(filter.allows("approval_status"));
        assert!(filter.allows("workflow_state"));
        assert!(!filter.allows("web_search"));
        assert!(!filter.allows("promotion_record"));
        assert!(!filter.allows("agent_revision_create"));
    }

    #[test]
    fn test_manifest_declared_tiers_override_runtime_inference() {
        let mut manifest = test_manifest();
        manifest.allowed_tool_tiers = vec![ToolTier::Core, ToolTier::Specialized];
        let filter = determine_tool_tier_filter(&manifest, Some("root/child"), true, SessionState::Normal);
        assert!(filter.allows("content_write"));
        assert!(filter.allows("web_search"));
        assert!(!filter.allows("agent_spawn"));
        assert!(filter.allows("approval_status"));
    }

    #[test]
    fn test_no_session_id_allows_all() {
        let manifest = test_manifest();
        let filter = determine_tool_tier_filter(&manifest, None, false, SessionState::Normal);
        assert!(filter.allows("web_search"));
    }

    #[test]
    fn test_degraded_session_clamps_to_core_only() {
        let manifest = test_manifest();
        let filter = determine_tool_tier_filter(&manifest, Some("root-session"), false, SessionState::Degraded);
        assert!(filter.allows("content_write"), "core content tools allowed in degraded");
        assert!(filter.allows("sandbox_exec"), "sandbox_exec is core tier, allowed by tier filter");
        assert!(!filter.allows("web_search"), "web_search is specialized, blocked in degraded");
        assert!(!filter.allows("agent_spawn"), "agent_spawn is workflow, blocked in degraded");
        assert!(!filter.allows("promotion_record"), "promotion is specialized, blocked in degraded");
        assert!(!filter.allows("agent_revision_create"), "agent_revision is specialized, blocked in degraded");
    }

    #[test]
    fn test_degraded_overrides_manifest_declared_tiers() {
        let mut manifest = test_manifest();
        manifest.allowed_tool_tiers = vec![ToolTier::Core, ToolTier::Specialized];
        let filter = determine_tool_tier_filter(&manifest, Some("root-session"), false, SessionState::Degraded);
        assert!(filter.allows("content_write"), "core allowed");
        assert!(!filter.allows("web_search"), "specialized blocked despite manifest");
        assert!(!filter.allows("agent_spawn"), "workflow blocked");
    }
}
