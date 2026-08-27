//! Shared gateway execution service for ingress and scheduler-driven runs.

use crate::agent::AgentRepository;
use crate::causal_chain::CausalLogger;
use crate::llm::{build_driver, Message};
use crate::runtime::active_execution_registry::ActiveExecutionRegistry;
use crate::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use crate::runtime::local_model_context::LocalModelContextCache;
use crate::runtime::openrouter_catalog::OpenRouterCatalog;
use crate::runtime::reevaluation_state::execute_scheduled_action;
use crate::runtime::response_validation::{
    log_contract_enforcement_event_to_gateway, log_nested_spawn_to_gateway,
};
use crate::runtime::root_session_budget::RootSessionBudgetRegistry;
use crate::runtime::script_execute::{execute_script_in_sandbox, script_causal_event};
use crate::runtime::session_budget::SessionBudgetRegistry;
use crate::runtime::session_context::SessionContext;
use crate::runtime::session_resume::{
    should_auto_resume_checkpoint_yield_reason, verify_trigger_coherence, TriggerIncoherence,
};
pub use crate::runtime::session_resume::ResumeTrigger;
use crate::runtime::session_report::SessionReportWriter;
use crate::scheduler::gateway_store::default_gateway_host_id;
use autonoetic_types::agent::{AgentManifest, ExecutionMode, LlmExchangeUsage};
use autonoetic_types::background::{ScheduledAction, UserInteractionStatus};
use autonoetic_types::causal_chain::EntryStatus;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::tool_error::tagged;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

/// Inject `approval_ref` into the last tool_call's arguments in the history,
/// then append a user message confirming the approval.  This fixes the
/// LLM-dependent relay bug where the model ignores the text-only hint and
/// retries without `approval_ref`, causing redundant approval requests.
/// Insert `approval_ref` into a tool call's JSON arguments string (#719). Used
/// when the gateway re-issues an approved call mechanically on resume, so the
/// promote/gate tool sees the granted approval exactly as if the agent had
/// passed it. Leaves non-object / non-JSON arguments untouched.
fn inject_approval_ref_into_args(arguments: &str, approval_ref: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "approval_ref".to_string(),
                    serde_json::Value::String(approval_ref.to_string()),
                );
                return serde_json::to_string(&v).unwrap_or_else(|_| arguments.to_string());
            }
            arguments.to_string()
        }
        Err(_) => arguments.to_string(),
    }
}

/// Human-readable rendering of a trigger/YieldReason incoherence (#741).
fn render_trigger_incoherence(inc: &TriggerIncoherence) -> String {
    match inc {
        TriggerIncoherence::InteractionMismatch { expected, got } => {
            format!("checkpoint waits on interaction '{expected}', not '{got}'")
        }
        TriggerIncoherence::ApprovalMismatch { expected, got } => {
            format!("checkpoint waits on approval '{expected}', not '{got}'")
        }
        TriggerIncoherence::WaitingForApproval { request_id } => {
            format!("session is waiting for approval '{request_id}'")
        }
        TriggerIncoherence::WrongYieldReason { got } => {
            format!("checkpoint yield reason {got} does not match the trigger")
        }
        TriggerIncoherence::EmergencyStop => {
            "emergency-stopped sessions are never auto-resumed (R-6.14)".to_string()
        }
    }
}

fn inject_approval_ref_into_history(
    history: &mut Vec<Message>,
    approval_ref: &str,
    target_call_id: Option<&str>,
) {
    // Walk history backwards to find the last assistant message with tool_calls.
    for msg in history.iter_mut().rev() {
        if matches!(msg.role, crate::llm::Role::Assistant) && !msg.tool_calls.is_empty() {
            // When a target call_id is provided (enriched checkpoint), inject
            // into that specific tool call.  Otherwise fall back to the last
            // one in the batch (legacy / non-enriched path).
            let tc = if let Some(id) = target_call_id {
                msg.tool_calls.iter_mut().find(|tc| tc.id == id)
            } else {
                msg.tool_calls.last_mut()
            };
            if let Some(tc) = tc {
                match serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                    Ok(mut args_val) => {
                        if let Some(obj) = args_val.as_object_mut() {
                            obj.insert(
                                "approval_ref".to_string(),
                                serde_json::Value::String(approval_ref.to_string()),
                            );
                        }
                        tc.arguments = serde_json::to_string(&args_val)
                            .unwrap_or_else(|_| tc.arguments.clone());
                    }
                    Err(_) => {
                        // Arguments are not valid JSON — leave them untouched.
                        // The appended user message (below) carries the approval_ref
                        // as a belt-and-suspenders hint.
                    }
                }
            }
            break;
        }
    }
    // Also inject the text hint as a user message (belt-and-suspenders).
    history.push(crate::llm::Message::user(format!(
        "[gateway] Approval `{}` has been granted by the operator. \
         The tool call has been updated with approval_ref=\"{}\".",
        approval_ref, approval_ref
    )));
}

/// Ensures `close_session` runs when `execute_with_history` fails so session report / digest finalize.
///
/// #847: distinguish recoverable yields (saved checkpoint with a resumable
/// `YieldReason`) from real errors before picking the close outcome.
/// `save_and_yield` writes a resumable checkpoint and returns `Err`, so the
/// wrapper sees `Err` for *every* yield — `BudgetExhausted`,
/// `MaxTurnsReached`, `ManualStop`, `Error(_)`. Closing these as
/// `SpawnExecuteError` (the pre-#847 behavior) cascades destructively:
/// - `close_session` calls `delete_session_grants` on the root (line 763),
///   destroying operator-approved grants the resume will need;
/// - `fail_running_tasks_for_session` (line 781) fails the parent's own task;
/// - for a root session, `fail_workflow_for_root_session(mark_workflow_failed=true)`
///   (line 847) fails **every non-terminal task in the workflow**, including
///   in-flight async children, and finalizes their transcripts as `failed`.
///
/// The result: a root planner that trips its own budget after spawning async
/// children sees the entire workflow marked `Failed` instead of a budget
/// pause, and only `retry_workflow_task` can recover.
///
/// The fix: peek at the latest checkpoint. If its `yield_reason` is one of
/// the recoverable yields (per `should_auto_resume_checkpoint_yield_reason`),
/// close with `ExecuteLoopSuspended` — an outcome that is `is_suspended()=true`
/// and `is_error()=false`, so all three failure cascades are skipped. Real
/// errors (no checkpoint, or non-recoverable yield reasons like
/// `EmergencyStop` / `ParentTerminated`) keep the pre-#847 `SpawnExecuteError`
/// behavior.
pub(crate) async fn execute_with_history_close_on_error(
    runtime: &mut AgentExecutor,
    history: &mut Vec<Message>,
) -> anyhow::Result<TurnOutcome> {
    match runtime.execute_with_history(history).await {
        Ok(o) => Ok(o),
        Err(e) => {
            let outcome = close_outcome_for_error(runtime);
            if outcome != SessionCloseOutcome::SpawnExecuteError {
                tracing::info!(
                    target: "lifecycle",
                    session_id = %runtime.session_id.as_deref().unwrap_or(""),
                    yield_reason = ?latest_checkpoint_yield_reason(runtime),
                    "Closing recoverable yield as suspended (not SpawnExecuteError) — workflow and grants preserved (#847)"
                );
            }
            let _ = runtime.close_session(outcome);
            Err(e)
        }
    }
}

/// Pick the close outcome for an `execute_with_history` error based on the
/// latest checkpoint's `yield_reason`. Recoverable yields (per
/// `should_auto_resume_checkpoint_yield_reason`) map to `ExecuteLoopSuspended`
/// so `close_session` skips the grant-deletion, task-failure, and
/// workflow-failure cascades; everything else maps to `SpawnExecuteError`
/// (pre-#847 behavior).
fn close_outcome_for_error(runtime: &AgentExecutor) -> SessionCloseOutcome {
    latest_checkpoint_yield_reason(runtime)
        .as_ref()
        .map(|reason| {
            crate::runtime::session_resume::should_auto_resume_checkpoint_yield_reason(reason)
        })
        .map(|recoverable| {
            if recoverable {
                SessionCloseOutcome::ExecuteLoopSuspended
            } else {
                SessionCloseOutcome::SpawnExecuteError
            }
        })
        .unwrap_or(SessionCloseOutcome::SpawnExecuteError)
}

/// Load the latest checkpoint's `yield_reason` for the current session, if
/// available. Returns `None` when no checkpoint exists (a true spawn-time
/// failure) or when the session has no configured gateway store.
fn latest_checkpoint_yield_reason(
    runtime: &AgentExecutor,
) -> Option<crate::runtime::checkpoint::YieldReason> {
    let config = runtime.config.as_ref()?;
    let session_id = runtime.session_id.as_deref()?;
    let cp = crate::runtime::checkpoint::load_latest_checkpoint(config, session_id)
        .ok()
        .flatten()?;
    Some(cp.yield_reason)
}

use autonoetic_types::session_outcome::SessionCloseOutcome;

struct SessionCloseFlags {
    assistant_reply: Option<String>,
    suspended_for_approval: Option<String>,
    suspended_for_user_input: bool,
    suspended_for_child_wait: bool,
}

impl SessionCloseFlags {
    fn outcome(&self, jsonrpc_spawn: bool) -> SessionCloseOutcome {
        let has_reply = self.assistant_reply.is_some();
        let approval = self.suspended_for_approval.is_some();
        let user_input = self.suspended_for_user_input;
        let child_wait = self.suspended_for_child_wait;
        match (jsonrpc_spawn, has_reply, approval, user_input, child_wait) {
            (true, true, false, false, false) => SessionCloseOutcome::JsonRpcSpawnComplete,
            (true, false, false, false, false) => SessionCloseOutcome::JsonRpcSpawnCompleteEmpty,
            (true, false, true, false, false) => SessionCloseOutcome::JsonRpcSpawnSuspended,
            (true, false, false, true, false) => SessionCloseOutcome::JsonRpcSpawnSuspendedUserInput,
            (true, false, false, false, true) => SessionCloseOutcome::JsonRpcSpawnSuspended,
            (false, true, false, false, false) => SessionCloseOutcome::CheckpointRespawnComplete,
            (false, true, false, true, false) => SessionCloseOutcome::CheckpointRespawnCompleteEmpty,
            (false, false, true, false, false) => SessionCloseOutcome::CheckpointRespawnSuspended,
            (false, false, false, true, false) => {
                SessionCloseOutcome::CheckpointRespawnSuspendedUserInput
            }
            (false, false, false, false, true) => SessionCloseOutcome::CheckpointRespawnSuspended,
            (false, false, false, false, false) => SessionCloseOutcome::CheckpointRespawnCompleteEmpty,
            _ => SessionCloseOutcome::SpawnExecuteError,
        }
    }
}

fn session_close_flags_from_turn_outcome(
    outcome: TurnOutcome,
) -> SessionCloseFlags {
    match outcome {
        TurnOutcome::Completed(reply) => SessionCloseFlags {
            assistant_reply: reply,
            suspended_for_approval: None,
            suspended_for_user_input: false,
            suspended_for_child_wait: false,
        },
        TurnOutcome::Suspended { approval_request_id, .. } => SessionCloseFlags {
            assistant_reply: None,
            suspended_for_approval: Some(approval_request_id),
            suspended_for_user_input: false,
            suspended_for_child_wait: false,
        },
        TurnOutcome::SuspendedUserInput { .. } => SessionCloseFlags {
            assistant_reply: None,
            suspended_for_approval: None,
            suspended_for_user_input: true,
            suspended_for_child_wait: false,
        },
        TurnOutcome::Escalated { .. } => SessionCloseFlags {
            assistant_reply: None,
            suspended_for_approval: None,
            suspended_for_user_input: true,
            suspended_for_child_wait: false,
        },
        TurnOutcome::WaitingForChild => SessionCloseFlags {
            assistant_reply: None,
            suspended_for_approval: None,
            suspended_for_user_input: false,
            suspended_for_child_wait: true,
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub id: String,   // content handle (sha256:...)
    pub name: String, // agent name from SKILL.md frontmatter
    pub description: String,
    pub files: Vec<String>, // list of file names in the artifact
    pub entry_point: Option<String>,
    pub io: Option<serde_json::Value>,
}

/// A single named content item written by a child agent during a spawn.
///
/// Included in `SpawnResult.files` so the caller (parent agent / planner) gets
/// a structured manifest of everything the child produced — no need to mine
/// handles from the free-text reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentFile {
    /// The name the child registered the content under (e.g. "weather_fetcher.py").
    pub name: String,
    /// Canonical SHA-256 content handle — kept for gateway logic; omitted from JSON for agents.
    #[serde(skip_serializing)]
    pub handle: String,
    /// Short 8-hex-char alias for lookup (e.g. "838ddf76").
    pub alias: String,
    /// Stable short ref for agents (`cnt_<alias>`); use with `content.read`, not as a shell path.
    #[serde(default, rename = "ref")]
    pub content_ref: String,
    /// Path where this named file is mounted for `sandbox.exec` (session mounts): `/tmp/<name>`.
    #[serde(default)]
    pub sandbox_path: String,
}

/// Knowledge shared during execution that the caller can access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedKnowledge {
    pub id: String, // memory_id
    pub scope: String,
    pub content_preview: String, // first 100 chars
    pub writer_agent_id: String,
    pub created_at: String,
}

/// Extracts structured-agent artifacts from the content store by looking for SKILL.md files.
///
/// This function:
/// 1. Lists all content names in the session
/// 2. Finds any SKILL.md files
/// 3. Parses YAML frontmatter to extract metadata
/// 4. Creates ArtifactMetadata for each SKILL.md found
pub fn extract_artifacts_from_content_store(
    gateway_dir: &std::path::Path,
    session_id: &str,
) -> anyhow::Result<Vec<ArtifactMetadata>> {
    let store = crate::runtime::content_store::ContentStore::new(gateway_dir)?;
    let names = store.list_names(session_id)?;

    let mut artifacts = Vec::new();

    for name in &names {
        // Look for SKILL.md files
        if name.ends_with("SKILL.md") || name == "SKILL.md" {
            match store.read_by_name(session_id, name) {
                Ok(content_bytes) => {
                    if let Ok(content) = String::from_utf8(content_bytes) {
                        if let Some(metadata) =
                            parse_skill_md_artifact(&store, session_id, name, &content)
                        {
                            artifacts.push(metadata);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "artifacts",
                        name = %name,
                        error = %e,
                        "Failed to read SKILL.md from content store"
                    );
                }
            }
        }
    }

    Ok(artifacts)
}

/// Collects all named content written by an agent during a spawn session.
///
/// Returns one `ContentFile` per named entry in the session manifest.
/// Namespaced names (containing `/` with the shape of a session path) are excluded
/// because those are parent-propagation copies, not original child outputs.
///
/// This gives the calling agent (planner) a structured manifest of everything the
/// child produced — names, short refs, and sandbox paths — without long digests in JSON.
pub fn collect_named_content(gateway_dir: &std::path::Path, session_id: &str) -> Vec<ContentFile> {
    let Ok(store) = crate::runtime::content_store::ContentStore::new(gateway_dir) else {
        return Vec::new();
    };
    let Ok(entries) = store.list_names_with_handles(session_id) else {
        return Vec::new();
    };

    entries
        .into_iter()
        .filter_map(|(name, handle)| {
            // Exclude internal session-snapshot names and namespaced propagation copies.
            // A namespaced copy looks like "some-session-id/filename" where the prefix
            // contains a UUID fragment (hex chars and hyphens). We skip any name whose
            // first path component looks like a session path segment.
            if name.starts_with("snapshot:") {
                return None;
            }
            // If the name contains a '/' and the part before the first '/' looks like a
            // session ID fragment (contains '-' or is long), treat it as a namespaced
            // propagation copy and skip it — the flat version is also registered.
            if let Some(slash_pos) = name.find('/') {
                let prefix = &name[..slash_pos];
                // Session ID segments contain hyphens (e.g. "demo-session", "coder-abc123")
                if prefix.contains('-') || prefix.len() > 12 {
                    return None;
                }
            }
            let alias = crate::runtime::content_store::ContentStore::get_short_alias(&handle);
            let content_ref = format!("cnt_{}", alias);
            let sandbox_path = format!("/tmp/{}", name);
            Some(ContentFile {
                name,
                handle,
                alias,
                content_ref,
                sandbox_path,
            })
        })
        .collect()
}

/// Collects knowledge the writer produced that the target agent may read (visibility + session + expiry).
pub fn collect_shared_knowledge(
    gateway_dir: &std::path::Path,
    target_agent_id: &str,
    writer_agent_id: &str,
    reader_session_id: Option<&str>,
) -> Vec<SharedKnowledge> {
    let Ok(store) = crate::scheduler::gateway_store::GatewayStore::open(gateway_dir) else {
        return Vec::new();
    };
    let Ok(ids) = store.memory_list_ids_owned_by(writer_agent_id) else {
        return Vec::new();
    };
    let now = chrono::Utc::now().to_rfc3339();

    ids.into_iter()
        .filter_map(|id| {
            let m = store.memory_get_unrestricted(&id).ok()??;
            if m.is_expired_at(&now) {
                return None;
            }
            if !m.is_readable_by(target_agent_id, reader_session_id) {
                return None;
            }
            let preview = if m.content.len() > 100 {
                let end = m.content.floor_char_boundary(100);
                format!("{}...", &m.content[..end])
            } else {
                m.content.clone()
            };
            Some(SharedKnowledge {
                id: m.memory_id,
                scope: m.scope,
                content_preview: preview,
                writer_agent_id: m.writer_agent_id,
                created_at: m.created_at,
            })
        })
        .collect()
}

/// Parses SKILL.md content and creates ArtifactMetadata.
///
/// Uses loose/soft validation:
/// - Missing or invalid frontmatter → still creates artifact with defaults
/// - Missing fields → sensible defaults (name from dir, empty description)
/// - This matches the "soft validation" approach for LLM-generated content
fn parse_skill_md_artifact(
    store: &crate::runtime::content_store::ContentStore,
    session_id: &str,
    skill_md_name: &str,
    content: &str,
) -> Option<ArtifactMetadata> {
    // Get all files in the session (needed regardless of parsing)
    let files = store.list_names(session_id).unwrap_or_default();

    // Use the directory of SKILL.md as the artifact ID prefix
    let artifact_dir = if skill_md_name.contains('/') {
        skill_md_name
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .unwrap_or("")
    } else {
        ""
    };

    // Derive default name from directory
    let default_name = artifact_dir
        .split('/')
        .last()
        .unwrap_or("unknown")
        .to_string();

    // Try to parse frontmatter, but use defaults if it fails
    #[derive(Deserialize)]
    struct SkillFrontmatter {
        name: Option<String>,
        description: Option<String>,
        script_entry: Option<String>,
        io: Option<serde_json::Value>,
    }

    let (name, description, script_entry, io) =
        match content.split("---").collect::<Vec<&str>>().get(1) {
            Some(frontmatter) => {
                // Attempt to parse YAML - if it fails, use defaults
                match serde_yaml::from_str::<SkillFrontmatter>(frontmatter) {
                    Ok(fm) => (
                        fm.name.unwrap_or(default_name),
                        fm.description.unwrap_or_default(),
                        fm.script_entry,
                        fm.io,
                    ),
                    Err(e) => {
                        tracing::debug!(
                            target: "artifacts",
                            skill_md = %skill_md_name,
                            error = %e,
                            "Could not parse SKILL.md frontmatter, using defaults"
                        );
                        (default_name, String::new(), None, None)
                    }
                }
            }
            None => {
                // No frontmatter markers - still create artifact with defaults
                tracing::debug!(
                    target: "artifacts",
                    skill_md = %skill_md_name,
                    "SKILL.md has no frontmatter, using defaults"
                );
                (default_name, String::new(), None, None)
            }
        };

    // Filter files that are in the same directory as SKILL.md
    let artifact_files: Vec<String> = files
        .iter()
        .filter(|f| {
            if artifact_dir.is_empty() {
                !f.contains('/')
            } else {
                f.starts_with(artifact_dir)
            }
        })
        .cloned()
        .collect();

    // Compute a combined handle for the artifact (hash of all file handles)
    let mut combined_hash = Sha256::new();
    for file in &artifact_files {
        if let Ok(handle) = store.resolve_name(session_id, file) {
            combined_hash.update(handle.as_bytes());
        }
    }
    let artifact_id = format!("sha256:{:x}", combined_hash.finalize());

    // Always return an artifact if we found the SKILL.md file
    Some(ArtifactMetadata {
        id: artifact_id,
        name,
        description,
        files: artifact_files,
        entry_point: script_entry,
        io,
    })
}

/// Outcome of a `spawn_clarification_for_approval` call. Carries the answer
/// text (also persisted as a gate_message) and the child session ID so
/// callers can attribute / link the audit record.
#[derive(Clone)]
pub struct ClarificationOutcome {
    pub child_session_id: String,
    pub answer: String,
}

impl std::fmt::Debug for ClarificationOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClarificationOutcome")
            .field("child_session_id", &self.child_session_id)
            .field("answer_len", &self.answer.len())
            .finish()
    }
}

#[derive(Debug)]
pub struct SpawnResult {
    pub agent_id: String,
    pub session_id: String,
    pub assistant_reply: Option<String>,
    /// Gateway-owned workflow note rendered separately from agent reply.
    /// This must never be merged into `assistant_reply` because reply may be
    /// schema-constrained (for example JSON via io.returns).
    pub workflow_note: Option<String>,
    pub should_signal_background: bool,
    pub artifacts: Vec<ArtifactMetadata>,
    /// All named content written by the child agent during this spawn.
    /// The calling agent (e.g. planner) can use `name`, `handle`, or `alias`
    /// to read any of these files via `content.read` without parsing reply text.
    pub files: Vec<ContentFile>,
    pub shared_knowledge: Vec<SharedKnowledge>,
    /// Per–LLM-round token usage for this run (JSON-RPC / CLI can surface this).
    pub llm_usage: Vec<LlmExchangeUsage>,
    /// Set when the turn ended by suspending at an approval gate rather than completing.
    /// The continuation has been saved to disk; callers should transition the task to
    /// `AwaitingApproval` using this request ID and release the tokio claim.
    pub suspended_for_approval: Option<String>,
    /// Set when the turn ended suspended for user input or human escalation.
    /// Callers should treat this as non-terminal and avoid completion-only post-processing.
    pub suspended_for_user_input: bool,
    pub suspended_for_child_wait: bool,
}

/// I/O contract captured from the manifest that actually executed a spawn.
///
/// Response validation must key off this struct — never a post-hoc alias
/// re-load — so candidate-revision smoke tests (spawn with `revision_id`
/// before any alias exists) are validated against the exact manifest that
/// ran. An alias lookup resolves to `None` for uninstalled candidates, which
/// used to silently skip `io.returns` enforcement until first production use.
#[derive(Debug, Clone)]
struct ExecutedIoContract {
    returns_schema: Option<serde_json::Value>,
    output_policy: Option<autonoetic_types::agent::OutputPolicy>,
    execution_mode: autonoetic_types::agent::ExecutionMode,
    returns_enforcement: autonoetic_types::agent::IoReturnsEnforcement,
    agent_is_spawn_capable: bool,
}

/// Per-root-session wake hint injected by the operator's TUI after plan approval.
/// While active, `agent_list` returns a blocking error that directs the planner
/// to the single agent identified by the wake message, preventing the
/// post-approval roster loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeHintState {
    pub plan_id: String,
    pub plan_version: u64,
    pub agent_id: String,
    pub step_id: String,
    pub delivered_at_turn: u32,
    pub expires_at_turn: u32,
}

#[derive(Clone)]
pub struct GatewayExecutionService {
    config: Arc<GatewayConfig>,
    http_client: reqwest::Client,
    execution_semaphore: Arc<Semaphore>,
    agent_admission: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    agent_execution_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    session_budget: Arc<SessionBudgetRegistry>,
    root_session_budget: Arc<RootSessionBudgetRegistry>,
    gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    active_executions: Arc<ActiveExecutionRegistry>,
    hook_executor: Arc<crate::scheduler::hooks::HookExecutor>,
    degraded_sessions: Arc<Mutex<std::collections::HashSet<String>>>,
    persona: Option<String>,
    local_model_context_cache: Arc<LocalModelContextCache>,
    wake_hints: Arc<Mutex<HashMap<String, WakeHintState>>>,
    /// Roots for which the budget circuit breaker is currently firing — an
    /// atomic in-process claim so concurrent siblings hitting the exhausted
    /// shared tree budget do not each fire a duplicate cascade (C2).
    budget_breaker_roots: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl GatewayExecutionService {
    pub fn new(
        config: GatewayConfig,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    ) -> Self {
        Self::new_with_persona(config, gateway_store, None)
    }

    pub fn new_with_persona(
        config: GatewayConfig,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        persona: Option<String>,
    ) -> Self {
        if let Err(e) = crate::runtime::tool_tier_registry::initialize_from_startup_path() {
            tracing::warn!(
                target: "autonoetic::tool_tier_registry",
                error = %e,
                "Failed to load tool-tier registry from startup path; using embedded defaults"
            );
        }
        let session_budget = Arc::new(SessionBudgetRegistry::new(config.session_budget.clone()));
        let root_session_budget = Arc::new(RootSessionBudgetRegistry::new(
            config.root_session_budget.clone(),
        ));

        // ── Hook spawn channel ──────────────────────────────────────────
        // Buffer 32 requests; back-pressure beyond that will warn-and-drop
        // (the send is a try_send in the tokio::spawn wrapper).
        let (spawn_tx, mut spawn_rx) =
            tokio::sync::mpsc::channel::<crate::scheduler::hooks::HookSpawnRequest>(32);

        let mut hook_exec = crate::scheduler::hooks::HookExecutor::new(
            config.hooks.clone(),
            gateway_store.clone(),
            config.port,
            config.signal_delivery_timeout_secs,
        );
        hook_exec.set_spawn_tx(spawn_tx);
        let hook_executor = Arc::new(hook_exec);

        if let Some(ref store) = gateway_store {
            store.set_policy_hook_executor(&hook_executor);
            // #722 Stage 2: wire the runtime config into the store so
            // `create_approval()` / interaction creation apply the standalone
            // approval and interaction TTLs. Without this the store's config
            // stays `None`, `enrich_request` sets no `expires_at`, and the
            // expiry sweep never has anything to mark stale/expired.
            store.set_config(Arc::new(config.clone()));
        }

        let svc = Self {
            execution_semaphore: Arc::new(Semaphore::new(config.max_concurrent_spawns.max(1))),
            agent_admission: Arc::new(Mutex::new(HashMap::new())),
            agent_execution_locks: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(config),
            http_client: crate::llm::build_llm_client(),
            session_budget,
            root_session_budget,
            gateway_store,
            active_executions: ActiveExecutionRegistry::new(),
            hook_executor,
            degraded_sessions: Arc::new(Mutex::new(std::collections::HashSet::new())),
            persona,
            local_model_context_cache: Arc::new(LocalModelContextCache::new()),
            wake_hints: Arc::new(Mutex::new(HashMap::new())),
            budget_breaker_roots: Arc::new(Mutex::new(std::collections::HashSet::new())),
        };

        // Spawn the drain task that turns HookSpawnRequests into actual agent runs.
        let drain_svc = svc.clone();
        tokio::spawn(async move {
            while let Some(req) = spawn_rx.recv().await {
                let svc = drain_svc.clone();
                tokio::spawn(async move {
                    // Scope child under root: "root/hook-spawn-uuid"
                    // so root_session_id(), root-budget, emergency stop all work.
                    let child_session_id = if req.root_session_id.is_empty() {
                        req.session_id.clone()
                    } else {
                        format!("{}/{}", req.root_session_id, req.session_id)
                    };
                    tracing::info!(
                        target: "hooks",
                        agent_id = %req.agent_id,
                        session_id = %child_session_id,
                        root_session_id = %req.root_session_id,
                        "agent.spawn hook: executing spawn"
                    );
                    match svc
                        .spawn_agent_once(
                            &req.agent_id,
                            &req.message,
                            &child_session_id,
                            None, // no source_agent_id — gateway-initiated
                            false,
                            Some("hook.agent_spawn"),
                            None,
                            None,
                            None,
                            None,
                            &[],
                        )
                        .await
                    {
                        Ok(result) => {
                            tracing::info!(
                                target: "hooks",
                                agent_id = %req.agent_id,
                                session_id = %child_session_id,
                                reply_len = result.assistant_reply.as_deref().map(|s| s.len()).unwrap_or(0),
                                "agent.spawn hook: spawn completed"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "hooks",
                                agent_id = %req.agent_id,
                                session_id = %child_session_id,
                                error = %e,
                                "agent.spawn hook: spawn failed"
                            );
                        }
                    }
                });
            }
        });

        svc
    }

    /// Probe local model servers for runtime context windows (best-effort).
    pub async fn warm_local_model_context(&self) {
        self.local_model_context_cache
            .warm_from_config(&self.http_client, self.config.as_ref())
            .await;
    }

    pub fn local_model_context_cache(&self) -> Arc<LocalModelContextCache> {
        self.local_model_context_cache.clone()
    }

    fn attach_model_metadata(
        &self,
        runtime: AgentExecutor,
        openrouter_catalog: Arc<OpenRouterCatalog>,
    ) -> AgentExecutor {
        runtime
            .with_openrouter_catalog(Some(openrouter_catalog))
            .with_local_model_context_cache(Some(self.local_model_context_cache.clone()))
    }

    pub fn config(&self) -> Arc<GatewayConfig> {
        Arc::clone(&self.config)
    }

    /// Register a wake hint for the given root session.
    /// While active, `agent_list` will return a blocking error directing the
    /// planner to the single agent identified in the hint.
    pub async fn register_wake_hint(&self, root_session_id: &str, wake_hint: WakeHintState) {
        let mut map = self.wake_hints.lock().await;
        map.insert(root_session_id.to_string(), wake_hint);
    }

    /// Look up the active wake hint for a root session at a given turn.
    /// Returns `None` if no hint is registered or the hint has expired.
    pub async fn active_wake_hint(&self, root_session_id: &str, current_turn: u32) -> Option<WakeHintState> {
        let map = self.wake_hints.lock().await;
        map.get(root_session_id).and_then(|hint| {
            if current_turn <= hint.expires_at_turn {
                Some(hint.clone())
            } else {
                None
            }
        })
    }

    /// Clear the wake hint for the given root session.
    pub async fn clear_wake_hint(&self, root_session_id: &str) {
        let mut map = self.wake_hints.lock().await;
        map.remove(root_session_id);
    }

    pub fn gateway_store(&self) -> Option<Arc<crate::scheduler::gateway_store::GatewayStore>> {
        self.gateway_store.clone()
    }

    pub fn hook_executor(&self) -> Arc<crate::scheduler::hooks::HookExecutor> {
        self.hook_executor.clone()
    }

    pub fn active_executions(&self) -> Arc<ActiveExecutionRegistry> {
        self.active_executions.clone()
    }

    pub fn degraded_sessions(&self) -> Arc<Mutex<std::collections::HashSet<String>>> {
        self.degraded_sessions.clone()
    }

    fn queue_gateway_last_word_notice(
        store: &crate::scheduler::gateway_store::GatewayStore,
        target_session_id: &str,
        trigger: &str,
        reason: &str,
    ) -> anyhow::Result<String> {
        let message_id = autonoetic_types::id_format::short_random_id("msg-");
        let now = chrono::Utc::now().to_rfc3339();
        let message = format!(
            "[Gateway Notice Ri-0.9]\n\
             Trigger: {}\n\
             Reason: {}\n\
             Last-word opportunity is open where practical.",
            trigger, reason
        );

        let record = crate::scheduler::gateway_store::AgentMessageRecord {
            message_id: message_id.clone(),
            sender_session_id: format!("gateway:{}", trigger),
            sender_agent_id: "gateway".to_string(),
            target_pattern: format!("session:{}", target_session_id),
            message: message.clone(),
            created_at: now.clone(),
            // Gateway-authored control message — content-free, unrestricted.
            egress_label: None,
        };
        store.save_agent_message(&record)?;
        store.insert_message_delivery(&message_id, target_session_id)?;

        let signal = crate::scheduler::signal::Signal::AgentMessage {
            message_id: message_id.clone(),
            sender_session_id: record.sender_session_id.clone(),
            sender_agent_id: record.sender_agent_id.clone(),
            message,
            timestamp: now,
        };
        if let Err(e) = crate::scheduler::signal::write_signal(
            Some(store),
            target_session_id,
            &message_id,
            &signal,
        ) {
            tracing::debug!(
                target: "ri_0_9",
                error = %e,
                target_session_id = %target_session_id,
                "Failed to enqueue Ri-0.9 wake signal"
            );
        }

        Ok(message_id)
    }

    fn record_last_word_event(
        store: &crate::scheduler::gateway_store::GatewayStore,
        session_id: &str,
        action: &str,
        payload: serde_json::Value,
    ) -> anyhow::Result<()> {
        store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("ri09-{}", uuid::Uuid::new_v4()),
            agent_id: "gateway".to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "session".to_string(),
            action: action.to_string(),
            status: "active".to_string(),
            enforced_rules: vec!["Ri-0.9".to_string()],
            target: None,
            payload: Some(payload.to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        })
    }

    pub async fn degrade_session(
        &self,
        session_id: &str,
        reason: &str,
    ) -> anyhow::Result<serde_json::Value> {
        self.degrade_session_with_options(session_id, reason, true)
            .await
    }

    pub async fn degrade_session_with_options(
        &self,
        session_id: &str,
        reason: &str,
        notify_where_practical: bool,
    ) -> anyhow::Result<serde_json::Value> {
        let session_id = session_id.trim();
        let reason = reason.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        anyhow::ensure!(!reason.is_empty(), "reason must not be empty");
        let mut last_word_notice_message_id: Option<String> = None;

        if let Some(store) = self.gateway_store.as_ref() {
            if notify_where_practical {
                let msg_id =
                    Self::queue_gateway_last_word_notice(store.as_ref(), session_id, "degrade", reason)?;
                last_word_notice_message_id = Some(msg_id.clone());
                Self::record_last_word_event(
                    store.as_ref(),
                    session_id,
                    "session.last_word_notice",
                    serde_json::json!({
                        "trigger": "degrade",
                        "where_practical": true,
                        "reason": reason,
                        "notice_message_id": msg_id,
                    }),
                )?;
            } else {
                Self::record_last_word_event(
                    store.as_ref(),
                    session_id,
                    "session.last_word_foreclosed",
                    serde_json::json!({
                        "trigger": "degrade",
                        "where_practical": false,
                        "reason": reason,
                    }),
                )?;
            }

            let event = autonoetic_types::causal_chain::CausalEventRecord {
                event_id: format!("degrade-{}", uuid::Uuid::new_v4()),
                agent_id: String::new(),
                session_id: session_id.to_string(),
                turn_id: None,
                event_seq: 0,
                timestamp: chrono::Utc::now().to_rfc3339(),
                category: "session".to_string(),
                action: "session.degraded".to_string(),
                status: "active".to_string(),
                enforced_rules: vec!["P-7.18".to_string()],
                target: None,
                payload: Some(
                    serde_json::json!({
                        "reason": reason,
                        "source": "operator",
                        "notify_where_practical": notify_where_practical,
                        "last_word_notice_message_id": last_word_notice_message_id,
                    })
                    .to_string(),
                ),
                payload_ref: None,
                evidence_ref: None,
                reason: Some(reason.to_string()),
            };
            let _ = store.create_causal_event(&event);
        }
        {
            let mut set = self.degraded_sessions.lock().await;
            set.insert(session_id.to_string());
        }
        Ok(serde_json::json!({
            "ok": true,
            "session_id": session_id,
            "state": "degraded",
            "reason": reason,
            "notify_where_practical": notify_where_practical,
            "last_word_notice_message_id": last_word_notice_message_id,
        }))
    }

    pub async fn clear_session_degradation(
        &self,
        session_id: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let session_id = session_id.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        {
            let mut set = self.degraded_sessions.lock().await;
            set.remove(session_id);
        }
        if let Some(store) = self.gateway_store.as_ref() {
            let event = autonoetic_types::causal_chain::CausalEventRecord {
                event_id: format!("clear-degrade-{}", uuid::Uuid::new_v4()),
                agent_id: String::new(),
                session_id: session_id.to_string(),
                turn_id: None,
                event_seq: 0,
                timestamp: chrono::Utc::now().to_rfc3339(),
                category: "session".to_string(),
                action: "session.degradation_cleared".to_string(),
                status: "active".to_string(),
                enforced_rules: vec!["P-7.18".to_string()],
                target: None,
                payload: None,
                payload_ref: None,
                evidence_ref: None,
                reason: None,
            };
            let _ = store.create_causal_event(&event);
        }
        Ok(serde_json::json!({
            "ok": true,
            "session_id": session_id,
            "state": "normal"
        }))
    }

    pub async fn is_session_degraded(&self, session_id: &str) -> bool {
        self.degraded_sessions.lock().await.contains(session_id)
    }

    fn resolve_inference_agent_id(
        &self,
        session_id: &str,
        agent_id: Option<&str>,
    ) -> anyhow::Result<String> {
        if let Some(id) = agent_id.map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(id.to_string());
        }
        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore required to resolve agent_id"))?;
        let binding = store
            .get_session_agent_binding(session_id)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "agent_id is required when session '{}' has no agent binding",
                    session_id
                )
            })?;
        Ok(binding.agent_id)
    }

    fn resolve_spawn_inference_profile(
        &self,
        agent_id: &str,
        manifest: &autonoetic_types::agent::AgentManifest,
        session_id: &str,
    ) -> anyhow::Result<crate::runtime::inference_profile::ResolvedInferenceProfile> {
        let root = crate::runtime::content_store::root_session_id(session_id);
        let binding = self
            .gateway_store
            .as_ref()
            .and_then(|gs| gs.get_session_inference_binding(root).ok().flatten());
        let profile = crate::runtime::inference_profile::resolve_inference_profile(
            agent_id,
            manifest,
            &self.config,
            binding.as_ref(),
        )?;
        if profile.preset_source
            == crate::runtime::inference_profile::PresetSource::LegacyInline
        {
            tracing::warn!(
                target: "autonoetic::inference",
                agent_id = %agent_id,
                session_id = %session_id,
                "Agent uses legacy inline llm_config; prefer llm_preset in SKILL.md"
            );
        }
        Ok(profile)
    }

    pub fn get_session_inference(
        &self,
        session_id: &str,
        agent_id: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let session_id = session_id.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        let agent_id = self.resolve_inference_agent_id(session_id, agent_id)?;
        let (manifest, _) = self.load_agent_manifest(&agent_id)?;
        let profile = self.resolve_spawn_inference_profile(&agent_id, &manifest, session_id)?;
        let binding = self
            .gateway_store
            .as_ref()
            .and_then(|gs| {
                gs.get_session_inference_binding(crate::runtime::content_store::root_session_id(
                    session_id,
                ))
                .ok()
                .flatten()
            });
        Ok(serde_json::json!({
            "ok": true,
            "session_id": session_id,
            "root_session_id": crate::runtime::content_store::root_session_id(session_id),
            "agent_id": agent_id,
            "preset_name": profile.preset_name,
            "preset_source": profile.snapshot_preset_source(),
            "session_override_preset": profile.session_override_preset,
            "provider": profile.llm_config.provider,
            "model": profile.llm_config.model,
            "is_routing_preset": profile.is_routing_preset,
            "binding": binding,
        }))
    }

    pub fn set_session_inference_override(
        &self,
        session_id: &str,
        agent_id: Option<&str>,
        preset: &str,
        reason: Option<&str>,
        set_by: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let session_id = session_id.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        let preset = preset.trim();
        anyhow::ensure!(!preset.is_empty(), "preset must not be empty");
        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore required for session inference override"))?;
        let agent_id = self.resolve_inference_agent_id(session_id, agent_id)?;
        let (manifest, _) = self.load_agent_manifest(&agent_id)?;
        crate::runtime::inference_profile::validate_inference_override(
            &manifest,
            &self.config,
            preset,
        )?;
        let root = crate::runtime::content_store::root_session_id(session_id);
        let binding = store.upsert_session_inference_binding(
            root,
            Some(preset),
            reason,
            set_by,
        )?;
        let profile = self.resolve_spawn_inference_profile(&agent_id, &manifest, session_id)?;
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("inference-override-{}", uuid::Uuid::new_v4()),
            agent_id: agent_id.clone(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "session".to_string(),
            action: "session.inference_override".to_string(),
            status: "active".to_string(),
            enforced_rules: vec![],
            target: None,
            payload: Some(
                serde_json::json!({
                    "operation": "set",
                    "preset": preset,
                    "reason": reason,
                    "set_by": set_by,
                    "resolved_provider": profile.llm_config.provider,
                    "resolved_model": profile.llm_config.model,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: reason.map(str::to_string),
        };
        let _ = store.create_causal_event(&event);
        Ok(serde_json::json!({
            "ok": true,
            "session_id": session_id,
            "root_session_id": root,
            "binding": binding,
            "resolved": {
                "preset_name": profile.preset_name,
                "provider": profile.llm_config.provider,
                "model": profile.llm_config.model,
            }
        }))
    }

    pub fn clear_session_inference_override(
        &self,
        session_id: &str,
        set_by: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let session_id = session_id.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore required for session inference override"))?;
        let root = crate::runtime::content_store::root_session_id(session_id);
        let cleared = store.delete_session_inference_binding(root)?;
        if cleared {
            let event = autonoetic_types::causal_chain::CausalEventRecord {
                event_id: format!("inference-clear-{}", uuid::Uuid::new_v4()),
                agent_id: String::new(),
                session_id: session_id.to_string(),
                turn_id: None,
                event_seq: 0,
                timestamp: chrono::Utc::now().to_rfc3339(),
                category: "session".to_string(),
                action: "session.inference_override".to_string(),
                status: "active".to_string(),
                enforced_rules: vec![],
                target: None,
                payload: Some(
                    serde_json::json!({ "operation": "clear", "set_by": set_by }).to_string(),
                ),
                payload_ref: None,
                evidence_ref: None,
                reason: None,
            };
            let _ = store.create_causal_event(&event);
        }
        Ok(serde_json::json!({
            "ok": true,
            "session_id": session_id,
            "root_session_id": root,
            "cleared": cleared,
        }))
    }

    // ---- Session-scoped egress policy (RFC data-envelopes §5.4) ----
    //
    // The operator's per-room privacy declaration: "for this session, these
    // named sources stay local." Rules are *added* to the operator-global
    // `egress.rules` and, because label resolution intersects (§4.1), can only
    // restrict. Keyed by root session and deleted when it closes.
    //
    // This is an operator surface, not an agent one: the label plane is
    // gateway-managed metadata that agents never set, strip, or read
    // (Lawful-Executor, RFC §2.1) — hence RPC/CLI only, no native tool.

    /// Resolve the egress label for an incoming user-role turn (RFC §4.5), or
    /// `None` when nothing restricts it.
    ///
    /// Intersects the three declared contributors — session-policy
    /// `default_label`, the operator's per-message mark, and a peer's inbound
    /// federation label. `None` is returned for an unrestricted result because
    /// absence *is* the unrestricted encoding everywhere else in the plane, and
    /// an unrestricted label carries no information worth storing.
    ///
    /// Fails closed on a policy read error: an unreadable policy could have been
    /// restrictive, so the turn is treated as `local_only` rather than shipped
    /// to a remote provider on the strength of a failed lookup.
    fn resolve_ingest_egress_label(
        &self,
        session_id: &str,
        metadata: Option<&serde_json::Value>,
    ) -> Option<autonoetic_types::egress::EgressLabel> {
        let mut policy_default = None;
        let mut policy_read_failed = false;
        if let Some(ref gs) = self.gateway_store {
            let root = crate::runtime::content_store::root_session_id(session_id);
            match gs.get_egress_session_policy(root) {
                Ok(stored) => {
                    policy_default = stored.and_then(|s| s.policy.default_label);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "egress",
                        error = %e,
                        session_id = %session_id,
                        "session egress policy read failed on ingest — failing closed \
                         to local_only for this turn (RFC §2.2)"
                    );
                    policy_read_failed = true;
                }
            }
        }
        crate::runtime::egress_labeler::resolve_ingest_turn_label(
            policy_default,
            policy_read_failed,
            metadata,
        )
    }

    /// Resolve the egress label for a **script-agent run** (RFC §4, #1062).
    ///
    /// The LLM path labels every tool result at the commit boundary
    /// ([`crate::runtime::tool_call_processor`] → `EgressLabeler::label_tool_result`)
    /// and stamps the resulting label onto the durable `execution_traces` row.
    /// The script fast path has no tool-call processor, so its `sandbox_exec`
    /// trace was written with `egress_label: None` — and an unlabeled row
    /// resolves through `egress.legacy_unlabeled`, which defaults to
    /// `unrestricted`. Script agents are precisely the ones returning raw
    /// external data (an IMAP fetch, a scraped page), so the one path that most
    /// needs a label was the one path that had none.
    ///
    /// The label is *derived*, never blanket: it is the intersection of
    ///
    /// - the exec-shaped resolution over the script the agent actually runs
    ///   (operator `egress.rules`, labeled path patterns matched against the
    ///   script source, workspace taint, artifact taint), and
    /// - the ingest label for this turn (session policy default, the operator's
    ///   per-message mark, a peer's inbound federation label).
    ///
    /// `None` ⇒ unrestricted, which is how absence is encoded everywhere else
    /// in the plane. A session-policy read failure fails closed to `local_only`
    /// for the run, matching [`Self::resolve_ingest_egress_label`] and the
    /// turn-scoped narrowing in `build_egress_labeler` (RFC §2.2).
    fn resolve_script_exec_egress_label(
        &self,
        manifest: &autonoetic_types::agent::AgentManifest,
        agent_dir: &std::path::Path,
        gateway_dir: &std::path::Path,
        session_id: &str,
        agent_id: &str,
        script_entry: &str,
        metadata: Option<&serde_json::Value>,
    ) -> Option<autonoetic_types::egress::EgressLabel> {
        crate::runtime::egress_labeler::resolve_script_exec_label(
            &crate::runtime::egress_labeler::ScriptExecLabelRequest {
                egress_config: &self.config.egress,
                // Bundle-declared output floor (RFC §4.1 path 2) — a script
                // bundle can restrict its own outputs; a floor only narrows.
                manifest_floor: manifest
                    .egress
                    .as_ref()
                    .and_then(|e| e.output_label)
                    .map(|named| named.to_label()),
                ingest: self.resolve_ingest_egress_label(session_id, metadata),
                agent_dir,
                gateway_dir,
                session_id,
                agent_id,
                script_entry,
            },
            self.gateway_store.as_ref(),
        )
    }

    // ── Security sentinel operator surface (#1119 tranche 2) ────────────

    fn require_store(&self) -> anyhow::Result<&Arc<crate::scheduler::gateway_store::GatewayStore>> {
        self.gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore required for security commands"))
    }

    /// Snapshot for `security status`: pending counts by severity, all
    /// findings by triage state, last completed sentinel sweep.
    pub fn security_status(&self) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let by_severity = store.count_pending_security_findings_by_severity()?;
        let by_triage = store.count_security_findings_by_triage_state()?;
        let last_sweep = store
            .list_scheduled_jobs_for_owner("security_sentinel", None, None)?
            .into_iter()
            .filter_map(|j| j.last_run_at)
            .max();
        Ok(serde_json::json!({
            "pending_by_severity": by_severity.iter()
                .map(|(s, c)| serde_json::json!({"severity": s, "count": c}))
                .collect::<Vec<_>>(),
            "by_triage_state": by_triage.iter()
                .map(|(s, c)| serde_json::json!({"triage_state": s, "count": c}))
                .collect::<Vec<_>>(),
            "last_sweep_at": last_sweep,
        }))
    }

    fn finding_row_to_value(
        r: &crate::scheduler::gateway_store::security_findings::SecurityFindingRow,
    ) -> serde_json::Value {
        serde_json::json!({
            "finding_id": r.finding_id,
            "severity": r.severity,
            "confidence": r.confidence,
            "finding_type": r.finding_type,
            "reproducibility": r.reproducibility,
            "sentinel_revision_id": r.sentinel_revision_id,
            "baseline_agreed": r.baseline_agreed,
            "triage_state": r.triage_state,
            "triage_reason": r.triage_reason,
            "proposed_remediation": r.proposed_remediation,
            "created_at": r.created_at,
        })
    }

    /// Filtered findings list for `security findings` / bulk-triage dry runs.
    pub fn security_findings(
        &self,
        severity: Option<&str>,
        finding_type: Option<&str>,
        triage: Option<&str>,
        limit: u32,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let store = self.require_store()?;
        Ok(store
            .list_security_findings_filtered(severity, finding_type, triage, limit)?
            .iter()
            .map(Self::finding_row_to_value)
            .collect())
    }

    pub fn security_triage_finding(
        &self,
        finding_id: &str,
        state: autonoetic_types::security::TriageState,
        reason: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.require_store()?
            .update_security_finding_triage(finding_id, state, reason)?;
        Ok(serde_json::json!({ "ok": true, "finding_id": finding_id }))
    }

    /// Bulk-triage every *pending* finding matching the filters (#1119):
    /// mirrors the CLI's previous client-side loop but in one RPC — per-finding
    /// failures are reported individually, never aborting the batch.
    pub fn security_triage_bulk(
        &self,
        state: autonoetic_types::security::TriageState,
        reason: &str,
        severity: Option<&str>,
        finding_type: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let rows =
            store.list_security_findings_filtered(severity, finding_type, Some("pending"), 10_000)?;
        let mut triaged = 0usize;
        let mut failures: Vec<serde_json::Value> = Vec::new();
        for r in &rows {
            match store.update_security_finding_triage(&r.finding_id, state.clone(), Some(reason)) {
                Ok(()) => triaged += 1,
                Err(e) => failures.push(serde_json::json!({
                    "finding_id": r.finding_id,
                    "error": e.to_string(),
                })),
            }
        }
        Ok(serde_json::json!({
            "matched": rows.len(),
            "triaged": triaged,
            "failures": failures,
        }))
    }

    fn pattern_to_value(p: &autonoetic_types::security::ProposedAttackPattern) -> serde_json::Value {
        serde_json::json!({
            "pattern_id": p.pattern_id,
            "category": p.category,
            "status": p.status.to_string(),
            "proposed_by_agent_id": p.proposed_by_agent_id,
            "description": p.description,
            "accepted_check_type": p.accepted_check_type,
            "operator_notes": p.operator_notes,
            "created_at": p.created_at,
            "reviewed_at": p.reviewed_at,
        })
    }

    pub fn security_patterns(
        &self,
        status: Option<&str>,
        limit: u32,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let store = self.require_store()?;
        Ok(store
            .list_attack_patterns(status, limit)?
            .iter()
            .map(Self::pattern_to_value)
            .collect())
    }

    pub fn security_review_pattern(
        &self,
        pattern_id: &str,
        status: autonoetic_types::security::AttackPatternStatus,
        check_type: Option<&str>,
        notes: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.require_store()?
            .review_attack_pattern(pattern_id, status, check_type, notes)?;
        Ok(serde_json::json!({ "ok": true, "pattern_id": pattern_id }))
    }

    // ── Recording / fixture-set operator surface (#1119 tranche 3) ──────

    pub fn recording_sessions(
        &self,
        agent: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<autonoetic_types::recording::RecordingSession>> {
        let store = self.require_store()?;
        Ok(store.list_recording_sessions(agent, limit)?)
    }

    /// The recording session plus its linked fixture set (when present) —
    /// what `recording inspect` renders.
    pub fn recording_session_get(
        &self,
        session_id: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let session = store
            .get_recording_session(session_id)?
            .ok_or_else(|| anyhow::anyhow!("Recording session '{}' not found", session_id))?;
        let fixture_set = match &session.fixture_set_id {
            Some(fs_id) => store.get_fixture_set(fs_id)?,
            None => None,
        };
        Ok(serde_json::json!({
            "session": session,
            "fixture_set": fixture_set,
        }))
    }

    /// Fixture-set lookup for `eval sealed` (#1119 tranche 3): the CLI still
    /// stages fixture files locally, but resolves the set + owning recording
    /// session over RPC.
    pub fn recording_fixture_set(
        &self,
        fixture_set_id: &str,
    ) -> anyhow::Result<autonoetic_types::recording::FixtureSet> {
        let store = self.require_store()?;
        store
            .get_fixture_set(fixture_set_id)?
            .ok_or_else(|| anyhow::anyhow!("Fixture set '{}' not found", fixture_set_id))
    }

    /// Delete a recording session and its linked fixture set (#1119 tranche 3).
    pub fn recording_session_delete(&self, session_id: &str) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let session = store
            .get_recording_session(session_id)?
            .ok_or_else(|| anyhow::anyhow!("Recording session '{}' not found", session_id))?;
        if let Some(fs_id) = &session.fixture_set_id {
            store.delete_fixture_set(fs_id)?;
        }
        let deleted = store.delete_recording_session(session_id)?;
        Ok(serde_json::json!({
            "ok": deleted,
            "session_id": session_id,
            "deleted_fixture_set": session.fixture_set_id,
        }))
    }

    /// Cancel a recording session and emit the operator-cancel causal event
    /// (#1119 tranche 3) — previously emitted CLI-side.
    // ── Escalations operator surface (#1119 close-out) ───────────────────

    /// Pending + per-root stale escalations, mirrors the CLI's historical
    /// aggregation (the RPC form of `gateway escalations list`).
    pub fn escalations_with_stale(
        &self,
    ) -> anyhow::Result<Vec<autonoetic_types::escalation::EscalationMessage>> {
        let store = self.require_store()?;
        let pending = store.list_pending_escalations()?;
        let mut all = pending.clone();
        let root_ids: std::collections::HashSet<String> =
            pending.iter().map(|e| e.root_session_id.clone()).collect();
        for rid in &root_ids {
            all.extend(store.get_stale_escalations_for_root(rid)?);
        }
        Ok(all)
    }

    // ── Approvals operator surface (#1119 tranche 7) ─────────────────────

    /// The global pending-approval list (all roots) — the RPC form of
    /// `scheduler::load_approval_requests` for the CLI approvals surface.
    pub fn pending_approvals(&self) -> anyhow::Result<Vec<autonoetic_types::background::ApprovalRequest>> {
        let store = self.require_store()?;
        Ok(store.get_pending_approvals()?)
    }

    /// Approval statistics for `gateway approvals stats`.
    pub fn approval_stats(
        &self,
        agent_id: Option<&str>,
        root_session_id: Option<&str>,
        since: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        Ok(store.get_approval_stats(agent_id, root_session_id, since)?)
    }

    // ── Trace / observability operator surface (#1119 tranche 6) ────────

    /// Contract-health snapshot — the CLI's `trace contract-health` JSON body
    /// is computed server-side (store + enforcement register) and returned
    /// whole, so the CLI only renders.
    pub fn contract_health(&self, since: Option<&str>) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let health = store.contract_health(since)?;
        let dead = crate::enforcement_register::dead_clauses(&health);
        let registered_count = crate::enforcement_register::principles().len()
            + crate::enforcement_register::rights().len()
            + crate::enforcement_register::obligations().len();
        let leak_summary = store.discretion_leak_summary(since)?;
        Ok(serde_json::json!({
            "since": since,
            "by_clause": health.by_clause.iter().map(|(clause, count)| {
                serde_json::json!({
                    "clause": clause,
                    "count": count,
                    "title": crate::enforcement_register::clause_title(clause),
                    "binds": crate::enforcement_register::binds(clause)
                        .map(|b| b.label()),
                })
            }).collect::<Vec<_>>(),
            "unattributed": health.unattributed,
            "dead_clauses": dead,
            "registered_clause_count": registered_count,
            "discretion_leaks": leak_summary.iter().map(|t| {
                serde_json::json!({
                    "rule_id": t.rule_id,
                    "kind": t.kind,
                    "count": t.count,
                })
            }).collect::<Vec<_>>(),
        }))
    }

    /// Civic-health snapshot — same server-side aggregation rule as
    /// [`Self::contract_health`].
    pub fn civic_health(&self, since: Option<&str>) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let health = store.civic_health(since)?;
        Ok(serde_json::json!({
            "since": since,
            "by_agent": health.by_agent.iter().map(|e| {
                serde_json::json!({
                    "agent_id": e.agent_id.as_str(),
                    "proposals_filed": e.proposals_filed,
                    "proposals_pending": e.proposals_pending,
                    "flags_filed": e.flags_filed,
                    "flags_pending": e.flags_pending,
                    "invitations_issued": e.invitations_issued,
                    "invitations_open": e.invitations_open,
                    "invitations_answered": e.invitations_answered,
                })
            }).collect::<Vec<_>>(),
        }))
    }

    /// Causal-event search for `trace session`/`trace event` display.
    pub fn causal_search(
        &self,
        session_id: Option<&str>,
        agent_id: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let events = store.search_causal_events(session_id, agent_id, limit)?;
        Ok(serde_json::to_value(&events).map_err(|e| anyhow::anyhow!("encode failure: {}", e))?)
    }

    /// Session listing for `trace sessions` — DB-backed like `trace show`.
    pub fn causal_session_summaries(
        &self,
        agent_id: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let summaries = store.summarize_causal_sessions(agent_id)?;
        Ok(serde_json::to_value(&summaries).map_err(|e| anyhow::anyhow!("encode failure: {}", e))?)
    }

    /// User-interaction listing scoped to a session or workflow.
    pub fn user_interactions(
        &self,
        session_id: Option<&str>,
        workflow_id: Option<&str>,
    ) -> anyhow::Result<Vec<autonoetic_types::background::UserInteraction>> {
        let store = self.require_store()?;
        match (session_id, workflow_id) {
            (Some(sid), None) => Ok(store.list_user_interactions_for_session_trace(sid)?),
            (None, Some(wfid)) => Ok(store.list_user_interactions_for_workflow(wfid)?),
            _ => anyhow::bail!("specify exactly one of session_id or workflow_id"),
        }
    }

    /// Fork lineage for `trace fork-tree`: ancestor chain (nearest-first,
    /// capped depth 16 with a visited set) + the descendant tree.
    pub fn fork_tree(&self, session_id: &str) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let root_id = crate::runtime::content_store::root_session_id(session_id).to_string();
        // Ancestor walk mirroring `fork_ancestor_roots`: start at the session's
        // ROOT (lineage is recorded under root ids), advance by the SOURCE's
        // root — a legacy row whose source was recorded as a nested id
        // ("root/T5") would otherwise dead-end the walk one hop early.
        let ancestors = {
            let mut out = Vec::new();
            let mut visited = std::collections::HashSet::new();
            let mut cursor = root_id.clone();
            for _ in 0..16 {
                let Some(record) = store.get_fork_lineage(&cursor)? else {
                    break;
                };
                let source_root =
                    crate::runtime::content_store::root_session_id(&record.source_session_id)
                        .to_string();
                if !visited.insert(source_root.clone()) {
                    break; // cycle guard
                }
                out.push(fork_lineage_value(&record));
                cursor = source_root;
            }
            out
        };
        // Seed the visited set with the target root so a cyclical lineage
        // (child → root) cannot re-introduce the root inside its own
        // descendant tree.
        let mut descendant_visited = std::collections::HashSet::new();
        descendant_visited.insert(root_id.clone());
        let descendants = collect_fork_descendants_value(
            store,
            &root_id,
            0,
            &mut descendant_visited,
        )?;
        Ok(serde_json::json!({
            "session_id": session_id,
            "root_session_id": root_id,
            "ancestors": ancestors,
            "descendants": descendants,
        }))
    }

    // ── Interactions / constitution / egress operators (#1119 tranche 5) ─

    /// Pending user interactions, scoped to a root session or explicit
    /// session (mirrors the old CLI lookups).
    pub fn pending_user_interactions(
        &self,
        root_session_id: Option<&str>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<autonoetic_types::background::UserInteraction>> {
        let store = self.require_store()?;
        match (root_session_id, session_id) {
            (Some(rsid), None) => Ok(store.get_pending_interactions_for_root_session(rsid)?),
            (None, Some(sid)) => Ok(store.get_pending_interactions_for_session(sid)?),
            _ => anyhow::bail!("specify exactly one of root_session_id or session_id"),
        }
    }

    /// Cancel a pending user interaction (operator abort).
    pub fn cancel_user_interaction(
        &self,
        interaction_id: &str,
        reason: &str,
    ) -> anyhow::Result<serde_json::Value> {
        self.require_store()?
            .cancel_user_interaction(interaction_id, reason)?;
        Ok(serde_json::json!({ "ok": true, "interaction_id": interaction_id }))
    }

    /// Constitutional proposals with status/proposer filters (all states,
    /// not just pending — the operator needs history too).
    pub fn constitutional_proposals(
        &self,
        status: Option<&str>,
        proposer: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let rows = store.list_constitutional_proposals(status, proposer, limit)?;
        Ok(serde_json::to_value(&rows).unwrap_or_else(|_| {
            serde_json::json!({"encode_error": true, "rows": rows.len()})
        }))
    }

    /// A single constitutional proposal for `show`.
    pub fn constitutional_proposal(
        &self,
        proposal_id: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        let proposal = store
            .get_constitutional_proposal(proposal_id)?
            .ok_or_else(|| anyhow::anyhow!("No proposal with id '{}'", proposal_id))?;
        Ok(serde_json::to_value(&proposal)?)
    }

    /// Resolve a constitutional proposal to the given decision state.
    pub fn decide_constitutional_proposal(
        &self,
        proposal_id: &str,
        new_status: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let updated = self.require_store()?.decide_constitutional_proposal(
            proposal_id,
            new_status,
            "operator",
            reason,
        )?;
        if !updated {
            anyhow::bail!("No proposal with id '{}'", proposal_id);
        }
        Ok(serde_json::json!({ "ok": true, "proposal_id": proposal_id, "status": new_status }))
    }

    pub fn recording_session_cancel(&self, session_id: &str) -> anyhow::Result<serde_json::Value> {
        let store = self.require_store()?;
        store.stop_recording_session(
            session_id,
            autonoetic_types::recording::RecordingStatus::Cancelled,
        )?;

        let causal_event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            agent_id: String::new(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: chrono::Utc::now().timestamp_millis().max(0) as u64,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "artifact".to_string(),
            action: "artifact.fixture_recording_cancelled".to_string(),
            status: "cancelled".to_string(),
            enforced_rules: vec![],
            target: Some(session_id.to_string()),
            payload: None,
            payload_ref: None,
            evidence_ref: None,
            reason: Some("Operator cancelled via CLI".to_string()),
        };
        let _ = store.create_causal_event(&causal_event);
        Ok(serde_json::json!({ "ok": true, "session_id": session_id }))
    }

    /// Operator rating on a closed session's outcome row (#1119 RPC surface).
    pub fn rate_session_outcome(
        &self,
        session_id: &str,
        thumb: autonoetic_types::session_outcome::OperatorThumb,
        note: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let session_id = session_id.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore required for session rating"))?;
        store.set_session_outcome_operator_rating(session_id, thumb, note)?;
        Ok(serde_json::json!({
            "ok": true,
            "session_id": session_id,
            "thumb": thumb.as_str(),
        }))
    }

    /// The SessionOutcome row for `session show` (#1119 RPC surface).
    pub fn get_session_outcome_row(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let session_id = session_id.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore required for session outcome"))?;
        let outcome = store.get_session_outcome(session_id)?;
        Ok(outcome.map(|o| serde_json::to_value(o)).transpose()?)
    }

    /// Full export payload for `session export` (#1119 RPC surface). The CLI
    /// renders locally — only the store reads happen here, server-side.
    pub fn export_full_session(
        &self,
        session_id: &str,
        opts: &crate::runtime::session_export::ExportOptions,
    ) -> anyhow::Result<serde_json::Value> {
        let session_id = session_id.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore required for session export"))?;
        let export =
            crate::runtime::session_export::export_session(store, &self.config, session_id, opts)?;
        Ok(serde_json::to_value(&export)?)
    }

    pub fn get_session_egress_policy(
        &self,
        session_id: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let session_id = session_id.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore required for session egress policy"))?;
        let root = crate::runtime::content_store::root_session_id(session_id);
        let stored = store.get_egress_session_policy(root)?;
        Ok(serde_json::json!({
            "ok": true,
            "session_id": session_id,
            "root_session_id": root,
            "policy": stored.as_ref().map(|s| s.policy.clone()),
            "set_by": stored.as_ref().map(|s| s.set_by.clone()),
            "created_at": stored.as_ref().map(|s| s.created_at.clone()),
            "updated_at": stored.as_ref().map(|s| s.updated_at.clone()),
        }))
    }

    pub fn set_session_egress_policy(
        &self,
        session_id: &str,
        policy: autonoetic_types::egress::EgressSessionPolicy,
        set_by: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let session_id = session_id.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore required for session egress policy"))?;
        let root = crate::runtime::content_store::root_session_id(session_id).to_string();
        let stored = store.set_egress_session_policy(&root, &policy, set_by)?;
        self.record_egress_policy_event(
            store,
            session_id,
            &root,
            "set",
            set_by,
            serde_json::json!({
                "rule_count": stored.policy.rules.len(),
                "rule_sources": stored
                    .policy
                    .rules
                    .iter()
                    .map(|r| match &r.path {
                        Some(p) => format!("{}:{}", r.source, p),
                        None => r.source.clone(),
                    })
                    .collect::<Vec<_>>(),
                "default_label": stored.policy.default_label,
            }),
        );
        Ok(serde_json::json!({
            "ok": true,
            "session_id": session_id,
            "root_session_id": root,
            "policy": stored.policy,
            "set_by": stored.set_by,
            "updated_at": stored.updated_at,
        }))
    }

    pub fn clear_session_egress_policy(
        &self,
        session_id: &str,
        set_by: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let session_id = session_id.trim();
        anyhow::ensure!(!session_id.is_empty(), "session_id must not be empty");
        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore required for session egress policy"))?;
        let root = crate::runtime::content_store::root_session_id(session_id).to_string();
        let cleared = store.delete_egress_session_policy(&root)?;
        if cleared {
            self.record_egress_policy_event(
                store,
                session_id,
                &root,
                "clear",
                set_by,
                serde_json::Value::Null,
            );
        }
        Ok(serde_json::json!({
            "ok": true,
            "session_id": session_id,
            "root_session_id": root,
            "cleared": cleared,
        }))
    }

    /// Audit the declaration itself. Widening a session's rules is as
    /// consequential as any labeling decision, so it lands in the causal chain
    /// with its operator attribution (I-6) — content-free, like every egress
    /// event (RFC §9).
    fn record_egress_policy_event(
        &self,
        store: &std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>,
        session_id: &str,
        root_session_id: &str,
        operation: &str,
        set_by: &str,
        detail: serde_json::Value,
    ) {
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("egress-session-policy-{}", uuid::Uuid::new_v4()),
            agent_id: String::new(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "egress".to_string(),
            action: "egress.session_policy".to_string(),
            status: "active".to_string(),
            enforced_rules: vec![],
            target: Some(root_session_id.to_string()),
            payload: Some(
                serde_json::json!({
                    "operation": operation,
                    "root_session_id": root_session_id,
                    "set_by": set_by,
                    "detail": detail,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some(format!("egress_session_policy_{operation}")),
        };
        if let Err(e) = store.create_causal_event(&event) {
            tracing::warn!(
                target: "egress_labeler",
                error = %e,
                root_session_id = %root_session_id,
                "failed to emit egress.session_policy causal event"
            );
        }
        // Also surface the declaration in the room timeline (live digest) so the
        // policy change is part of the session narrative, not invisible state —
        // mirrors the `envelope.proposed` emitter. `set_by` values the room/CLI
        // send (`operator`, `operator:tui`, `operator:rpc`) map to the Operator
        // seat; anything else falls through to `decider_seat`.
        let (principal, role) = if set_by == "operator" || set_by.starts_with("operator:") {
            (
                autonoetic_types::principal::Principal::human("operator"),
                autonoetic_types::session_timeline::SessionRole::Operator,
            )
        } else {
            crate::runtime::session_timeline::decider_seat(set_by)
        };
        let timeline_event = crate::runtime::session_timeline::build_timeline_event(
            root_session_id.to_string(),
            session_id.to_string(),
            None,
            &principal,
            &role,
            "egress.session_policy",
            None,
            Some(serde_json::json!({
                "operation": operation,
                "root_session_id": root_session_id,
                "set_by": set_by,
                "detail": detail,
            })),
            autonoetic_types::session_timeline::TimelineRefs::default(),
        );
        if let Err(e) = store.create_live_digest_event(&timeline_event) {
            tracing::warn!(
                target: "session_timeline",
                error = %e,
                root_session_id = %root_session_id,
                "egress.session_policy timeline emit failed"
            );
        }
    }

    /// Operator / gateway / privileged-agent root-session circuit breaker (see Phase 2C).
    pub async fn emergency_stop_root_session(
        &self,
        root_session_id: &str,
        reason: &str,
        requested_by_type: &str,
        requested_by_id: &str,
        trigger_kind: &str,
        source_agent_id: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.emergency_stop_root_session_with_options(
            root_session_id,
            reason,
            requested_by_type,
            requested_by_id,
            trigger_kind,
            source_agent_id,
            false,
        )
        .await
    }

    pub async fn emergency_stop_root_session_with_options(
        &self,
        root_session_id: &str,
        reason: &str,
        requested_by_type: &str,
        requested_by_id: &str,
        trigger_kind: &str,
        source_agent_id: Option<&str>,
        notify_where_practical: bool,
    ) -> anyhow::Result<serde_json::Value> {
        use crate::runtime::checkpoint::{
            load_latest_checkpoint, save_checkpoint, SessionCheckpoint, YieldReason,
        };
        use crate::runtime::guard::LoopGuard;
        use crate::scheduler::gateway_store::EmergencyStopRecord;

        let store = self
            .gateway_store()
            .ok_or_else(|| anyhow::anyhow!("gateway store required for emergency stop"))?;
        let root_session_id = root_session_id.trim();
        anyhow::ensure!(
            !root_session_id.is_empty(),
            "root_session_id must not be empty"
        );
        anyhow::ensure!(!reason.trim().is_empty(), "reason must not be empty");

        let mut last_word_notice_message_id: Option<String> = None;
        if notify_where_practical {
            let msg_id = Self::queue_gateway_last_word_notice(
                store.as_ref(),
                root_session_id,
                "emergency_stop",
                reason,
            )?;
            last_word_notice_message_id = Some(msg_id.clone());
            Self::record_last_word_event(
                store.as_ref(),
                root_session_id,
                "session.last_word_notice",
                serde_json::json!({
                    "trigger": "emergency_stop",
                    "where_practical": true,
                    "trigger_kind": trigger_kind,
                    "reason": reason,
                    "notice_message_id": msg_id,
                }),
            )?;
        } else {
            Self::record_last_word_event(
                store.as_ref(),
                root_session_id,
                "session.last_word_foreclosed",
                serde_json::json!({
                    "trigger": "emergency_stop",
                    "where_practical": false,
                    "trigger_kind": trigger_kind,
                    "reason": reason,
                }),
            )?;
        }

        if let Some(aid) = source_agent_id {
            let repo = AgentRepository::from_config(self.config.as_ref());
            let gateway_dir = crate::execution::gateway_root_dir(self.config.as_ref());
            let loaded =
                repo.get_sync_from_store(aid, &gateway_dir, self.gateway_store.as_deref())?;
            let policy = crate::policy::PolicyEngine::new(loaded.manifest);
            let decision = policy.can_request_emergency_stop();
            if !decision.is_allowed() {
                return Err(tagged::Tagged::permission_with_rules(
                    anyhow::anyhow!(
                        "Permission Denied: agent '{}' cannot request emergency stop",
                        aid
                    ),
                    decision
                        .enforced_rules
                        .into_iter()
                        .map(|rule| rule.to_string())
                        .collect(),
                )
                .into());
            }
        }

        let stop_id = autonoetic_types::id_format::short_random_id("estop-");
        let requested_at = chrono::Utc::now().to_rfc3339();

        let workflow_id = crate::scheduler::workflow_store::resolve_workflow_id_for_root_session(
            self.config.as_ref(),
            root_session_id,
        )?;

        store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            agent_id: source_agent_id.unwrap_or("gateway").to_string(),
            session_id: root_session_id.to_string(),
            turn_id: None,
            event_seq: 0,
            timestamp: requested_at.clone(),
            category: "background".to_string(),
            action: format!("emergency_stop.initiated:{}", stop_id),
            status: "success".to_string(),
            enforced_rules: vec!["R+++3".to_string(), "P-7.1".to_string()],
            target: None,
            payload: Some(
                serde_json::json!({
                    "reason": reason,
                    "trigger_kind": trigger_kind,
                    "requested_by_type": requested_by_type,
                    "requested_by_id": requested_by_id,
                    "notify_where_practical": notify_where_practical,
                    "last_word_notice_message_id": last_word_notice_message_id,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        })?;

        store.insert_emergency_stop(&EmergencyStopRecord {
            stop_id: stop_id.clone(),
            scope_type: "root_session".to_string(),
            scope_id: root_session_id.to_string(),
            root_session_id: root_session_id.to_string(),
            workflow_id: workflow_id.clone(),
            requested_by_type: requested_by_type.to_string(),
            requested_by_id: requested_by_id.to_string(),
            reason: Some(reason.to_string()),
            trigger_kind: trigger_kind.to_string(),
            mode: "immediate".to_string(),
            status: "stopping".to_string(),
            requested_at: requested_at.clone(),
            completed_at: None,
            details_json: None,
        })?;

        // Surface the circuit breaker on the canonical timeline (#413) — the most
        // important operator event; it was causal/record-only, so the room never
        // showed it. Attributed to whoever requested it; always Error altitude.
        {
            let (principal, seat) = crate::runtime::session_timeline::actor_from_kind_id(
                requested_by_type,
                requested_by_id,
            );
            let event = crate::runtime::session_timeline::build_timeline_event(
                root_session_id.to_string(),
                root_session_id.to_string(),
                None,
                &principal,
                &seat,
                "session.emergency_stop",
                None, // base_altitude ⇒ Error
                Some(serde_json::json!({
                    "stop_id": stop_id.clone(),
                    "reason": reason,
                    "trigger_kind": trigger_kind,
                    "requested_by_type": requested_by_type,
                    "requested_by_id": requested_by_id,
                })),
                autonoetic_types::session_timeline::TimelineRefs::default(),
            );
            if let Err(e) = store.create_live_digest_event(&event) {
                tracing::debug!(
                    target: "session_timeline",
                    error = %e,
                    "emergency_stop timeline emit failed"
                );
            }
        }

        let mut details = serde_json::json!({
            "aborted_handles": 0u32,
            "workflow_tasks_aborted": 0u32,
            "queued_removed": 0u32,
            "scheduled_jobs_cancelled": 0u32,
            "notify_where_practical": notify_where_practical,
            "last_word_notice_message_id": last_word_notice_message_id,
        });

        let killed_sandbox = self
            .active_executions
            .kill_sandbox_children_for_root(root_session_id);
        details["killed_sandbox_pids"] = serde_json::json!(&killed_sandbox);

        let mut aborted_handles = 0u32;
        if let Some(ref wf) = workflow_id {
            let tasks = crate::scheduler::workflow_store::list_task_runs_for_workflow(
                self.config.as_ref(),
                Some(store.as_ref()),
                wf,
            )?;
            let tids: Vec<String> = tasks.iter().map(|t| t.task_id.clone()).collect();
            aborted_handles = self.active_executions.abort_workflow_tasks(wf, &tids) as u32;

            let summary = crate::scheduler::workflow_store::apply_emergency_stop_to_workflow(
                self.config.as_ref(),
                Some(store.as_ref()),
                wf,
                &stop_id,
            )?;
            details["workflow_tasks_aborted"] = serde_json::json!(summary.tasks_aborted);
            details["queued_removed"] = serde_json::json!(summary.queued_removed);
        }
        details["aborted_handles"] = serde_json::json!(aborted_handles);

        for approval in store.get_pending_approvals_for_root(root_session_id)? {
            store.record_decision(
                &approval.request_id,
                "cancelled",
                &format!("emergency_stop:{stop_id}"),
                &chrono::Utc::now().to_rfc3339(),
                None,
            )?;
        }

        // Cancel pending escalations for this root session. Without this,
        // the escalation stays "pending" after the linked approval was
        // cancelled, and federation_escalate blocks forever ("already exists")
        // while approval_status returns "not found" — a divergence loop.
        for esc in store.list_pending_escalations()? {
            if esc.root_session_id == root_session_id {
                let _ = store.resolve_escalation(
                    &esc.escalation_id,
                    autonoetic_types::escalation::EscalationStatus::Cancelled,
                    &format!("emergency_stop:{stop_id}"),
                    Some("Cancelled by emergency stop"),
                );
            }
        }

        let cancel_note = format!("emergency_stop:{stop_id} — {reason}");
        for inter in store.get_pending_interactions_for_root_session(root_session_id)? {
            store.cancel_user_interaction(&inter.interaction_id, &cancel_note)?;
        }

        if let Err(e) = store.delete_session_grants(root_session_id) {
            tracing::warn!(
                target: "emergency_stop",
                root_session_id = %root_session_id,
                error = %e,
                "Failed to delete session grants during emergency stop"
            );
        }
        if let Err(e) = store.revoke_session_envelopes_for_root(root_session_id) {
            tracing::warn!(
                target: "emergency_stop",
                root_session_id = %root_session_id,
                error = %e,
                "Failed to revoke session envelopes during emergency stop"
            );
        }
        crate::runtime::egress_labeler::clear_session_egress_policy(
            &store,
            root_session_id,
            "emergency_stop",
        );
        if let Err(e) = store.delete_session_inference_binding(root_session_id) {
            tracing::warn!(
                target: "emergency_stop",
                root_session_id = %root_session_id,
                error = %e,
                "Failed to delete session inference binding during emergency stop"
            );
        }

        let active_jobs_before_cancel: Vec<_> = store
            .list_scheduled_jobs_for_root(root_session_id)?
            .into_iter()
            .filter(|j| {
                matches!(
                    j.status,
                    autonoetic_types::scheduled_job::ScheduledJobStatus::Active
                )
            })
            .collect();

        match store.cancel_scheduled_jobs_for_root(root_session_id) {
            Ok(count) => {
                details["scheduled_jobs_cancelled"] = serde_json::json!(count);
                tracing::info!(
                    target: "emergency_stop",
                    root_session_id = %root_session_id,
                    jobs_cancelled = count,
                    "Cancelled scheduled jobs during emergency stop"
                );
                for j in active_jobs_before_cancel {
                    if let Err(e) =
                        crate::scheduler::workflow_store::append_scheduled_job_cancelled_workflow_event(
                            self.config.as_ref(),
                            store.as_ref(),
                            &j.root_session_id,
                            &j.job_id,
                            &j.owner_agent_id,
                            &j.target_agent_id,
                            &j.cron_expr,
                            &format!("emergency_stop:{stop_id}"),
                        )
                    {
                        tracing::warn!(
                            target: "emergency_stop",
                            error = %e,
                            job_id = %j.job_id,
                            "Failed to append scheduled_job.cancelled workflow event"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "emergency_stop",
                    root_session_id = %root_session_id,
                    error = %e,
                    "Failed to cancel scheduled jobs during emergency stop"
                );
            }
        }

        let wf_lead = workflow_id
            .as_deref()
            .and_then(|wid| {
                crate::scheduler::workflow_store::load_workflow_run(
                    self.config.as_ref(),
                    Some(store.as_ref()),
                    wid,
                )
                .ok()
                .flatten()
            })
            .map(|r| r.lead_agent_id);

        let mut cp = if let Some(existing) =
            load_latest_checkpoint(self.config.as_ref(), root_session_id)?
        {
            existing
        } else {
            let lead = wf_lead
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "cannot write emergency checkpoint for '{}': no session checkpoint and no workflow lead agent",
                        root_session_id
                    )
                })?;
            SessionCheckpoint {
                egress_labels: Default::default(),
                egress_ask: None,
                history: vec![],
                turn_counter: 0,
                loop_guard_state: LoopGuard {
                    max_loops_without_progress: 32,
                    max_tool_failures: 5,
                    max_consecutive_same_progress: 0,
                    max_child_failures: 3,
                    current_loops: 0,
                    tool_failure_counts: std::collections::HashMap::new(),
                    last_progress_fingerprint: None,
                    consecutive_progress_count: 0,
                    child_failure_count: 0,
                    ..Default::default()
                },
                session_state: autonoetic_types::agent::SessionState::Normal,
                tool_tier_escalated: false,
                session_phase: Default::default(),
                discovered_tools: Default::default(),
                blocked_state_event_emitted: false,
                // Emergency-stop checkpoint: no extended instructions were
                // ever loaded in this stub session, so stay unloaded (#1015).
                extended_loaded: false,
                agent_id: lead.to_string(),
                session_id: root_session_id.to_string(),
                turn_id: format!("emergency-{stop_id}"),
                workflow_id: workflow_id.clone(),
                task_id: None,
                runtime_lock_hash: None,
                constitution_version: None,
                constitution_digest: None,
                llm_config_snapshot: None,
                tool_registry_version: None,
                yield_reason: YieldReason::EmergencyStop {
                    stop_id: stop_id.clone(),
                },
                content_store_refs: vec![],
                created_at: chrono::Utc::now().to_rfc3339(),
                pending_tool_state: None,
                llm_rounds_consumed: 0,
                tool_invocations_consumed: 0,
                tokens_consumed: 0,
                estimated_cost_usd: 0.0,
                compression_metadata: None,
                capsule_state: None,
                assistant_message: None,
                pending_action: None,
                suspended_at: None,
                suppress_until_turn: 0,
                trajectory_last_level: None,
                feedback_events: vec![],
            }
        };
        cp.yield_reason = YieldReason::EmergencyStop {
            stop_id: stop_id.clone(),
        };
        cp.turn_id = format!("emergency-{stop_id}");
        cp.created_at = chrono::Utc::now().to_rfc3339();
        if cp.workflow_id.is_none() {
            cp.workflow_id = workflow_id.clone();
        }
        save_checkpoint(self.config.as_ref(), &cp)?;

        let status_final = "stopped";
        store.update_emergency_stop_status(
            &stop_id,
            status_final,
            Some(&chrono::Utc::now().to_rfc3339()),
            Some(&details.to_string()),
        )?;

        // GAP-1C: terminate the root session transcript so it doesn't stay
        // 'active' forever. The orphan reaper checks parent transcript status
        // and can't reap children if the parent looks alive.
        //
        // This must force the terminal lifecycle. An emergency stop typically
        // lands on a session parked at `awaiting_gate` or `hibernated`, and the
        // polite `finalize_session_transcript` preserves both — the root would
        // keep advertising a resumable lifecycle, `find_orphaned_sessions` would
        // never see `terminated:*`, and its children would leak unreaped:
        // exactly what GAP-1C exists to prevent. The stop has already cancelled
        // this session's pending gates.
        let now = chrono::Utc::now().to_rfc3339();
        if let Err(e) = store.terminate_session_transcript(root_session_id, &now, "failed") {
            tracing::warn!(
                target: "execution",
                root_session_id = %root_session_id,
                stop_id = %stop_id,
                error = %e,
                "Failed to finalize root session transcript during emergency stop"
            );
        }

        Ok(serde_json::json!({
            "ok": true,
            "stop_id": stop_id,
            "root_session_id": root_session_id,
            "status": status_final,
            "notify_where_practical": notify_where_practical,
            "last_word_notice_message_id": last_word_notice_message_id,
            "details": details,
        }))
    }

    /// Same pipeline as [`Self::emergency_stop_root_session`], for gateway self-protection paths.
    pub async fn emergency_stop_from_security_policy(
        &self,
        root_session_id: &str,
        reason: &str,
    ) -> anyhow::Result<serde_json::Value> {
        self.emergency_stop_root_session(
            root_session_id,
            reason,
            "gateway",
            "security_policy",
            "security_policy",
            None,
        )
        .await
    }

    /// Root-session-tree budget circuit breaker (C2 / #616).
    ///
    /// When the root-tree budget (P-6.21) is exhausted, the parent's next LLM
    /// call is blocked — but in-flight descendant sessions keep running and keep
    /// burning the already-spent tree budget. This fires the *graceful* emergency
    /// stop cascade exactly once per root to cancel those descendants
    /// (checkpoints + last-word notice + aborts workflow tasks + kills sandbox
    /// children + writes an `EmergencyStopRecord`). Siblings that subsequently
    /// hit the exhausted budget see the existing stop via the pre-flight guard
    /// (`lifecycle.rs` root-session emergency-stop check) and yield `EmergencyStop`.
    ///
    /// **Idempotent:** if a stop already exists for the root, this is a no-op, so
    /// repeated sibling exhaustion does not create a second cascade.
    ///
    /// Only ever called from the root-budget error paths (keyed off
    /// `runtime.root_budget_exhausted`), never from per-session budget.
    ///
    /// `pub` so integration tests can exercise the cascade directly (the
    /// production call site needs a real LLM-driven turn to reach budget
    /// exhaustion).
    pub async fn trigger_root_budget_circuit_breaker(
        &self,
        root_session_id: &str,
        underlying: &anyhow::Error,
    ) {
        let Some(store) = self.gateway_store() else {
            tracing::debug!(
                target: "root_budget_breaker",
                root_session_id = %root_session_id,
                "skip: no gateway store"
            );
            return;
        };

        // Atomic in-process claim: exactly one concurrent sibling per root
        // proceeds. This closes the check-then-act race on the DB lookup below —
        // under a shared exhausted tree budget, multiple siblings hit the limit
        // at once, and without this claim each would observe an empty stop list
        // and fire a duplicate cascade.
        {
            let mut claimed = self.budget_breaker_roots.lock().await;
            if !claimed.insert(root_session_id.to_string()) {
                tracing::debug!(
                    target: "root_budget_breaker",
                    root_session_id = %root_session_id,
                    "skip: budget breaker already claimed by a concurrent sibling"
                );
                return;
            }
        }

        // Run under the claim; release it on every exit (leak-free, and lets a
        // later sibling retry if the cascade itself failed — a subsequent caller
        // then finds the recorded stop via the DB check and no-ops).
        async {
            // A prior emergency stop — from any source — means the cascade
            // already ran or is running; cross-restart idempotency too.
            match store.list_emergency_stops_for_root_session(root_session_id) {
                Ok(stops) if !stops.is_empty() => {
                    tracing::debug!(
                        target: "root_budget_breaker",
                        root_session_id = %root_session_id,
                        existing_stop = %stops[0].stop_id,
                        "skip: root already has an emergency stop (idempotent)"
                    );
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "root_budget_breaker",
                        root_session_id = %root_session_id,
                        error = %e,
                        "could not check existing emergency stops; not firing budget breaker"
                    );
                    return;
                }
            }

            let reason = format!(
                "Root session budget exhausted — cancelling in-flight descendants to \
                 stop tree-budget burn (P-6.21). {}",
                underlying
            );
            tracing::warn!(
                target: "root_budget_breaker",
                root_session_id = %root_session_id,
                "Firing graceful root budget circuit breaker"
            );
            if let Err(e) = self
                .emergency_stop_root_session_with_options(
                    root_session_id,
                    &reason,
                    "gateway",
                    "root_budget_circuit_breaker",
                    "root_budget_exhausted",
                    None,
                    true,
                )
                .await
            {
                tracing::warn!(
                    target: "root_budget_breaker",
                    root_session_id = %root_session_id,
                    error = %e,
                    "root budget circuit breaker failed to cascade"
                );
            }
        }
        .await;

        self.budget_breaker_roots
            .lock()
            .await
            .remove(root_session_id);
    }

    async fn finalize_session(
        &self,
        runtime: &mut AgentExecutor,
        session_id: String,
        agent_id: &str,
        source_agent_id: Option<&str>,
        close_flags: SessionCloseFlags,
        jsonrpc_spawn: bool,
        consumed_checkpoint_turn_id: Option<String>,
    ) -> anyhow::Result<SpawnResult> {
        let close_outcome = close_flags.outcome(jsonrpc_spawn);
        let is_suspended = close_outcome.is_suspended();
        let close_reason = close_outcome.as_str();
        let SessionCloseFlags {
            assistant_reply,
            suspended_for_approval,
            suspended_for_user_input,
            suspended_for_child_wait,
        } = close_flags;
        let digest_turn_count = runtime.turn_counter;
        let gw_dir = crate::execution::gateway_root_dir(&self.config);

        // Cross-agent egress taint (RFC data-envelopes §5.5): record what this
        // session touched (intersection of its label sidecar) so a parent that
        // surfaces its return value — or a sibling it messaged — labels the
        // transferred content and withholds it from a sink the label excludes.
        // Only restrictive taint is stored (absence ⇒ unrestricted). This is the
        // capture half of closing the `LocalAgent` hole; the apply half labels
        // the recipient's tool result / message.
        if let Some(store) = runtime.gateway_store.as_ref() {
            if !runtime.egress_labels.is_empty() {
                let taint = crate::runtime::egress_labeler::session_accumulated_taint(
                    &runtime.egress_labels,
                );
                if !taint.is_unrestricted() {
                    if let Err(e) = store.set_session_egress_taint(&session_id, &taint) {
                        tracing::warn!(
                            target: "egress",
                            error = %e,
                            session_id = %session_id,
                            "failed to record session egress taint (§5.5)"
                        );
                    }
                }
            }
        }

        // Residency: a resident agent that finished its task parks instead of
        // terminating, so peers can still reach it (`agent_message`). Parking
        // means: persist an Idle checkpoint, record the session as addressable,
        // and — critically — do NOT write the outcome row, because that row is
        // what marks a session finished for every downstream reader.
        //
        // Only a clean completion parks. A suspended session is already
        // resumable through its own gate, and re-parking it would race that
        // resume; an errored or escalated one must close and be seen — parking
        // it would withhold the `session_outcomes` row that marks a session
        // finished, so the failure would read as "still running" to every
        // downstream reader (#902 review).
        let park_ttl_secs = if close_outcome.is_clean_completion() {
            runtime.resident_idle_ttl_secs()
        } else {
            None
        };
        let mut parked = false;
        if let (Some(ttl), Some(store)) = (park_ttl_secs, self.gateway_store.as_ref()) {
            if let Some(turn_id) = runtime.park_idle(ttl) {
                let now = chrono::Utc::now();
                let record = crate::scheduler::gateway_store::SessionResidency {
                    session_id: session_id.clone(),
                    root_session_id: crate::runtime::live_digest::base_session_id(&session_id)
                        .to_string(),
                    agent_id: agent_id.to_string(),
                    turn_id,
                    since: now.to_rfc3339(),
                    expires_at: (now + chrono::Duration::seconds(ttl as i64)).to_rfc3339(),
                };
                match store.upsert_session_residency(&record) {
                    Ok(()) => {
                        parked = true;
                        tracing::info!(
                            target: "session_residency",
                            session_id = %session_id,
                            agent_id = %agent_id,
                            ttl_secs = ttl,
                            "Session parked idle and remains addressable"
                        );
                    }
                    Err(e) => {
                        // Failing to record residency must not silently produce
                        // an unreachable parked session: fall through and close
                        // normally instead.
                        tracing::warn!(
                            target: "session_residency",
                            session_id = %session_id,
                            error = %e,
                            "Failed to record residency; closing the session normally"
                        );
                    }
                }
            }
        }

        if !parked {
            if let Some(store) = self.gateway_store.as_ref() {
                // A previously parked session that is now closing for real must
                // stop advertising itself, or the reaper would later "close" a
                // session that is already gone.
                if let Err(e) = store.clear_session_residency(&session_id) {
                    tracing::debug!(
                        target: "session_residency",
                        session_id = %session_id,
                        error = %e,
                        "Failed to clear residency on close"
                    );
                }
                crate::runtime::session_outcome_writer::write_session_outcome_metrics(
                    &runtime,
                    store,
                    &session_id,
                    agent_id,
                );
            }
        }
        if jsonrpc_spawn {
            if let Some(store) = self.gateway_store.as_ref() {
                if close_outcome.is_completed_empty() {
                    if let Ok(tool_count) =
                        store.count_execution_traces_for_session(&session_id)
                    {
                        if let Some(draft) =
                            crate::runtime::operator_activity::classify_session_lifecycle(
                                close_outcome,
                                tool_count.min(u32::MAX as u64) as u32,
                            )
                        {
                            let root_id = crate::runtime::live_digest::base_session_id(
                                &session_id,
                            )
                            .to_string();
                            let record = draft.into_record(
                                root_id,
                                session_id.clone(),
                                agent_id.to_string(),
                                None,
                                None,
                                None,
                                None,
                                None,
                                None,
                            );
                            let rate_limit_per_min = self.config.operator_activity.rate_limit_per_min;
                            match store.insert_operator_activity_throttled(&record, rate_limit_per_min) {
                                Ok(crate::scheduler::gateway_store::OperatorActivityInsert::Dropped) => {
                                    tracing::debug!(
                                        target: "operator_activity",
                                        rate_limit_per_min,
                                        "Session lifecycle operator activity dropped by per-root rate limit"
                                    );
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    tracing::warn!(
                                        target: "operator_activity",
                                        error = %e,
                                        "Failed to persist session lifecycle operator activity"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        runtime.close_session(close_outcome)?;
        {
            let root_id = crate::runtime::live_digest::base_session_id(&session_id).to_string();
            let ctx = autonoetic_types::hooks::HookContext::for_session_closed(
                &root_id,
                &session_id,
                agent_id,
                close_reason,
                digest_turn_count,
                Some(&gw_dir),
            );
            if is_suspended {
                let mut suspended_ctx = ctx.clone();
                suspended_ctx.event = autonoetic_types::hooks::HookEvent::SessionSuspended;
                self.hook_executor.dispatch_async(suspended_ctx);
            } else {
                self.hook_executor.dispatch_async(ctx);
            }
        }
        if jsonrpc_spawn && !is_suspended {
            if let Err(e) = crate::runtime::checkpoint::prune_checkpoints(
                self.config.as_ref(),
                &session_id,
                2,
            ) {
                tracing::debug!(
                    target: "checkpoint",
                    session_id = %session_id,
                    error = %e,
                    "Failed to prune session checkpoints after completion"
                );
            }
        }
        crate::runtime::post_session_digest::maybe_run_post_session_digest(
            self.config.as_ref(),
            &gw_dir,
            self.gateway_store.as_ref(),
            &self.http_client,
            &session_id,
            agent_id,
            digest_turn_count,
            is_suspended,
        )
        .await;
        crate::runtime::session_outcome_writer::maybe_run_outcome_grader(
            self.config.as_ref(),
            &gw_dir,
            self.gateway_store.as_ref(),
            &self.http_client,
            &session_id,
            agent_id,
            digest_turn_count,
            is_suspended,
        )
        .await;
        if let Some(gs) = self.gateway_store.as_ref() {
            let mem_store = crate::runtime::memory::SqliteMemoryStore::new(gs.clone());
            crate::runtime::quality_signal::maybe_emit_quality_signal(
                self.config.as_ref(),
                self.gateway_store.as_ref(),
                &mem_store,
                &session_id,
                agent_id,
                digest_turn_count,
                is_suspended,
            )
            .await;
        }
        let llm_usage = runtime.take_llm_usage_last_run();
        if let Some(ref checkpoint_turn_id) = consumed_checkpoint_turn_id {
            if let Err(e) = crate::runtime::checkpoint::delete_checkpoint(
                &self.config,
                &session_id,
                checkpoint_turn_id,
            ) {
                tracing::warn!(
                    target: "checkpoint",
                    session_id = %session_id,
                    turn_id = %checkpoint_turn_id,
                    error = %e,
                    "Failed to delete consumed checkpoint"
                );
            }
        }
        let workflow_note = if !is_suspended {
            build_gateway_workflow_note(self.config.as_ref(), &session_id, assistant_reply.as_deref())
        } else {
            None
        };
        let artifacts = extract_artifacts_from_content_store(&gw_dir, &session_id).unwrap_or_default();
        let files = collect_named_content(&gw_dir, &session_id);
        let shared_knowledge = collect_shared_knowledge(
            &gw_dir,
            source_agent_id.unwrap_or(agent_id),
            agent_id,
            Some(&session_id),
        );
        Ok(SpawnResult {
            agent_id: agent_id.to_string(),
            session_id,
            assistant_reply,
            workflow_note,
            should_signal_background: jsonrpc_spawn,
            artifacts,
            files,
            shared_knowledge,
            llm_usage,
            suspended_for_approval,
            suspended_for_user_input,
            suspended_for_child_wait,
        })
    }

    /// Handle a detected checkpoint integrity violation (#606): emit a durable
    /// `background.checkpoint`/`checkpoint_tampered` causal event, cancel the
    /// bound approval with reason `integrity_violation`, and surface an
    /// operator-visible alert.
    ///
    /// `approval_request_id` is `Some` when the violation is detected after the
    /// checkpoint loaded successfully (an action-mismatch / TOCTOU
    /// substitution). When it is `None` (HMAC tamper — the checkpoint could not
    /// be read) the bound approval is located by session.
    fn handle_checkpoint_integrity_violation(
        &self,
        session_id: &str,
        agent_id: &str,
        approval_request_id: Option<&str>,
        reason: &str,
    ) {
        if let Some(store) = self.gateway_store.as_ref() {
            record_checkpoint_integrity_violation(
                store,
                session_id,
                agent_id,
                approval_request_id,
                reason,
            );
        } else {
            tracing::error!(
                target: "checkpoint",
                session_id = %session_id,
                reason = %reason,
                "checkpoint integrity violation — no GatewayStore available to record it"
            );
        }
    }

    /// Resume from a loaded checkpoint by dispatching on its `yield_reason`.
    ///
    /// Handles all checkpoint-based resume paths (ApprovalRequired,
    /// UserInputRequired, HumanEscalation, generic auto-resume) in a single
    /// place, eliminating the previous duplication between the task_id=Some and
    /// task_id=None branches.
    async fn resume_from_checkpoint(
        &self,
        runtime: &mut crate::runtime::lifecycle::AgentExecutor,
        session_id: &str,
        message: &str,
        metadata: Option<&serde_json::Value>,
        checkpoint: crate::runtime::checkpoint::SessionCheckpoint,
    ) -> anyhow::Result<(
        crate::runtime::lifecycle::TurnOutcome,
        String,
        Option<String>,
    )> {
        use autonoetic_types::background::UserInteractionStatus;

        // Cancel stale divergence-sentinel interactions from before this resume.
        // The trajectory monitor restarts fresh on resume (sliding windows empty,
        // last_level restored from checkpoint), so old divergence prompts are
        // from a different monitoring context and should not block the operator.
        if let Some(store) = self.gateway_store.as_ref() {
            let root = crate::runtime::content_store::root_session_id(session_id);
            if let Ok(pending) = store.get_pending_interactions_for_root_session(&root) {
                for inter in &pending {
                    if matches!(
                        inter.kind,
                        autonoetic_types::background::UserInteractionKind::DivergenceSentinel,
                    ) {
                        let _ = store.cancel_user_interaction(
                            &inter.interaction_id,
                            "cancelled on session resume — trajectory monitor restarted",
                        );
                    }
                }
            }
        }

        if matches!(
            checkpoint.yield_reason,
            crate::runtime::checkpoint::YieldReason::EmergencyStop { .. }
        ) {
            anyhow::bail!(
                "Cannot auto-resume session '{}' from EmergencyStop checkpoint",
                session_id
            );
        }

        if let crate::runtime::checkpoint::YieldReason::ApprovalRequired {
            approval_request_id: ref rid,
        } = &checkpoint.yield_reason
        {
            let store = self.gateway_store.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "GatewayStore is required to resume approval-required checkpoints"
                )
            })?;
            let req = store.get_approval(rid)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "Approval request '{}' from checkpoint not found in store",
                    rid
                )
            })?;
            let status = req.status.clone();
            match status {
                None => {
                    tracing::info!(
                        target: "checkpoint",
                        session_id = %session_id,
                        approval_request_id = %rid,
                        "Checkpoint blocked by pending approval — re-suspending session"
                    );
                    return Ok((
                        TurnOutcome::Suspended {
                            approval_request_id: rid.clone(),
                        },
                        checkpoint.initial_user_message(),
                        None,
                    ));
                }
                Some(autonoetic_types::background::ApprovalStatus::Stale) => {
                    tracing::info!(
                        target: "checkpoint",
                        session_id = %session_id,
                        approval_request_id = %rid,
                        "Approval expired and is stale — re-suspending session until operator resolves"
                    );
                    return Ok((
                        TurnOutcome::Suspended {
                            approval_request_id: rid.clone(),
                        },
                        checkpoint.initial_user_message(),
                        None,
                    ));
                }
                Some(autonoetic_types::background::ApprovalStatus::Rejected)
                | Some(autonoetic_types::background::ApprovalStatus::Cancelled) => {
                    anyhow::bail!(
                        "Approval '{}' was {:?}; session '{}' cannot continue",
                        rid,
                        status,
                        session_id
                    );
                }
                Some(autonoetic_types::background::ApprovalStatus::Approved) => {
                    // TOCTOU action-equality check: the action stored in the
                    // checkpoint must structurally match the action in the
                    // approval row.  This prevents substitution attacks where
                    // the approval row is swapped between suspend and resume.
                    if let Some(ref cp_action) = checkpoint.pending_action {
                        if cp_action != &req.action {
                            self.handle_checkpoint_integrity_violation(
                                session_id,
                                &runtime.manifest.agent.id,
                                Some(rid),
                                "checkpoint action mismatch — possible substitution attack",
                            );
                            anyhow::bail!(
                                "checkpoint action mismatch: the action stored in the checkpoint does not match the approved action (session '{}')",
                                session_id
                            );
                        }
                    }

                    tracing::info!(
                        target: "checkpoint",
                        agent_id = %runtime.manifest.agent.id,
                        session_id = %session_id,
                        turn_counter = checkpoint.turn_counter,
                        approval_request_id = %rid,
                        "Resuming session from approval-required checkpoint"
                    );
                    let gateway_dir = crate::execution::gateway_root_dir(&self.config);
                    if let Ok(mut report) = crate::runtime::session_report::SessionReportWriter::open(
                        &gateway_dir,
                        session_id,
                        &runtime.manifest.agent.id,
                    ) {
                        let _ = report.record_approval_resolved(
                            rid,
                            "approved",
                            "Resumed from approval-required checkpoint",
                        );
                    }
                    checkpoint.restore_into(runtime);
                    let mut history = checkpoint.history.clone();
                    // Extract the specific call_id that was blocked by the
                    // approval gate, so we can target it precisely.
                    let target_call_id = checkpoint
                        .pending_tool_state
                        .as_ref()
                        .map(|pts| pts.pending_tool_call.call_id.as_str());

                    // #719: a `RevisionPromote` approval checkpoint is re-executed
                    // *mechanically* on resume — the gateway issues the approved
                    // promote itself instead of asking the LLM to re-issue it. The
                    // legacy inject-and-re-issue path is kept for other tools (e.g.
                    // sandbox.exec) whose checkpoint already carries the
                    // `approval_required` response as the tool result.
                    //
                    // Key selection off the approved `req.action` (already loaded
                    // and TOCTOU-checked above) so older checkpoints without
                    // `pending_action` still work, and guard on `pending_tool_state`
                    // so a missing tool state degrades to the legacy path instead of
                    // panicking.
                    let mechanical = matches!(
                        &req.action,
                        ScheduledAction::RevisionPromote { .. }
                    ) && checkpoint.pending_tool_state.is_some();

                    if mechanical {
                        let pts = checkpoint
                            .pending_tool_state
                            .as_ref()
                            .expect("mechanical implies pending_tool_state is Some");
                        // Rebuild the original assistant message with only the
                        // completed calls (drop the pending + never-run remaining
                        // calls; the approved pending call is re-issued via the
                        // synthetic seed message below). Push it only if it still
                        // carries content or a completed call — an assistant
                        // message with neither is a degenerate turn.
                        if let Some(am) = checkpoint.assistant_message.as_deref() {
                            let completed_ids: std::collections::HashSet<&str> = pts
                                .completed_tool_results
                                .iter()
                                .map(|(id, _, _)| id.as_str())
                                .collect();
                            let mut m = am.clone();
                            m.tool_calls
                                .retain(|tc| completed_ids.contains(tc.id.as_str()));
                            if !m.tool_calls.is_empty() || !m.content.trim().is_empty() {
                                history.push(m);
                            }
                        }
                        // Always replay completed tool results, even if the
                        // assistant message was degenerate/absent — they are
                        // logically independent of the assistant prefix.
                        for (call_id, tool_name, result) in &pts.completed_tool_results {
                            history.push(Message::tool_result(call_id, tool_name, result));
                        }
                        // Seed the approved call (with approval_ref injected into
                        // its arguments) for execution at the top of
                        // execute_with_history's loop.
                        let arguments =
                            inject_approval_ref_into_args(&pts.pending_tool_call.arguments, rid);
                        let call = crate::llm::ToolCall {
                            id: pts.pending_tool_call.call_id.clone(),
                            name: pts.pending_tool_call.tool_name.clone(),
                            arguments,
                        };
                        let mut synth = crate::llm::Message::assistant(String::new());
                        synth.tool_calls = vec![call.clone()];
                        runtime.resume_pending_batch = Some((synth, vec![call]));
                    } else {
                        // Legacy / precomputed-result path: replay the assistant
                        // message + any results and nudge the LLM to re-issue.
                        if let Some(am) = checkpoint.assistant_message.as_deref() {
                            let mut call_ids_with_results: std::collections::HashSet<&str> =
                                std::collections::HashSet::new();
                            if let Some(ref pts) = checkpoint.pending_tool_state {
                                for (call_id, _, _) in &pts.completed_tool_results {
                                    call_ids_with_results.insert(call_id.as_str());
                                }
                                if pts.pending_tool_call.approval_response.is_some() {
                                    call_ids_with_results
                                        .insert(&pts.pending_tool_call.call_id);
                                }
                            }

                            let mut filtered_am = am.clone();
                            filtered_am.tool_calls.retain(|tc| {
                                call_ids_with_results.contains(tc.id.as_str())
                                    || call_ids_with_results.is_empty()
                            });
                            history.push(filtered_am);

                            if let Some(ref pts) = checkpoint.pending_tool_state {
                                for (call_id, tool_name, result) in &pts.completed_tool_results {
                                    history.push(Message::tool_result(
                                        call_id, tool_name, result,
                                    ));
                                }
                                if let Some(ref resp) = pts.pending_tool_call.approval_response {
                                    history.push(Message::tool_result(
                                        &pts.pending_tool_call.call_id,
                                        &pts.pending_tool_call.tool_name,
                                        resp,
                                    ));
                                }
                            }
                        }
                        inject_approval_ref_into_history(&mut history, rid, target_call_id);
                    }

                    let initial_msg = checkpoint.initial_user_message();
                    let outcome = execute_with_history_close_on_error(runtime, &mut history).await?;
                    Ok((outcome, initial_msg, Some(checkpoint.turn_id)))
                }
            }
        } else if let crate::runtime::checkpoint::YieldReason::UserInputRequired {
            interaction_id: ref iid,
        } = &checkpoint.yield_reason
        {
            let store = self.gateway_store.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "GatewayStore is required to resume user_ask checkpoints"
                )
            })?;
            let interaction = store
                .get_user_interaction(iid)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "User interaction '{}' from checkpoint not found in store",
                        iid
                    )
                })?;
            match &interaction.status {
                UserInteractionStatus::Pending => {
                    tracing::info!(
                        target: "checkpoint",
                        session_id = %session_id,
                        interaction_id = %iid,
                        "Checkpoint blocked by pending user interaction — re-suspending session"
                    );
                    return Ok((
                        TurnOutcome::SuspendedUserInput {
                            interaction_id: iid.clone(),
                        },
                        checkpoint.initial_user_message(),
                        None,
                    ));
                }
                UserInteractionStatus::Cancelled | UserInteractionStatus::Expired => {
                    anyhow::bail!(
                        "User interaction '{}' is {:?}; cannot resume from checkpoint",
                        iid,
                        interaction.status
                    );
                }
                UserInteractionStatus::Answered => {
                    // Claim is already acquired upstream in resume_from_user_interaction
                    // (execution.rs:2758) before spawn_agent_once calls this function.
                    // The standalone/interaction-resume-claim prevents concurrent
                    // scheduler polls from spawning duplicate executions.  Do NOT
                    // re-acquire it here — that would short-circuit the actual resume
                    // with a silent Completed(None) because the claim is single-use.

                    tracing::info!(
                        target: "user_interaction",
                        session_id = %session_id,
                        interaction_id = %interaction.interaction_id,
                        "Resuming session from user_ask checkpoint with stored answer"
                    );

                    checkpoint.restore_into(runtime);

                    let mut history = checkpoint.history.clone();
                    crate::runtime::session_resume::inject_answered_user_interaction_into_history(
                        &mut history, &checkpoint, &interaction,
                    )?;
                    if let Some(gw) = runtime.gateway_dir.as_ref() {
                        let base =
                            crate::runtime::live_digest::base_session_id(session_id).to_string();
                        let answer_summary = match (
                            interaction.answer_text.as_deref(),
                            interaction.answer_option_id.as_deref(),
                        ) {
                            (Some(t), _) if !t.trim().is_empty() => t.trim().to_string(),
                            (_, Some(oid)) if !oid.is_empty() => {
                                format!("selected option `{oid}`")
                            }
                            _ => "(answered)".to_string(),
                        };
                        crate::runtime::live_digest::append_user_ask_answer_best_effort(
                            gw,
                            &base,
                            &interaction.interaction_id,
                            &answer_summary,
                        );
                    }
                    if !message.trim().is_empty() {
                        history.push(crate::llm::Message::user(message.to_string()));
                    }
                    let initial_msg = checkpoint.initial_user_message();
                    let outcome =
                        execute_with_history_close_on_error(runtime, &mut history).await?;
                    Ok((outcome, initial_msg, Some(checkpoint.turn_id)))
                }
            }
        } else if let crate::runtime::checkpoint::YieldReason::HumanEscalation {
            escalation_request_id: ref esc_rid,
        } = &checkpoint.yield_reason
        {
            let store = self.gateway_store.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "GatewayStore is required to resume escalation checkpoints"
                )
            })?;
            let req = store.get_approval(esc_rid)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "Escalation approval '{}' from checkpoint not found in store",
                    esc_rid
                )
            })?;
            let esc_status = req.status.clone();
                match esc_status {
                None => {
                        tracing::info!(
                        target: "checkpoint",
                        session_id = %session_id,
                        escalation_request_id = %esc_rid,
                        "Checkpoint blocked by pending escalation — re-suspending session"
                    );
                    return Ok((
                        TurnOutcome::Suspended {
                            approval_request_id: esc_rid.clone(),
                        },
                        checkpoint.initial_user_message(),
                        None,
                    ));
                }
                Some(autonoetic_types::background::ApprovalStatus::Stale) => {
                    tracing::info!(
                        target: "checkpoint",
                        session_id = %session_id,
                        escalation_request_id = %esc_rid,
                        "Escalation expired and is stale — re-suspending session until operator resolves"
                    );
                    return Ok((
                        TurnOutcome::Suspended {
                            approval_request_id: esc_rid.clone(),
                        },
                        checkpoint.initial_user_message(),
                        None,
                    ));
                }
                Some(autonoetic_types::background::ApprovalStatus::Rejected)
                | Some(autonoetic_types::background::ApprovalStatus::Cancelled) => {
                    anyhow::bail!(
                        "Escalation approval '{}' was {:?}; session '{}' cannot continue",
                        esc_rid,
                        esc_status,
                        session_id
                    );
                }
                Some(autonoetic_types::background::ApprovalStatus::Approved) => {
                    tracing::info!(
                        target: "checkpoint",
                        agent_id = %runtime.manifest.agent.id,
                        session_id = %session_id,
                        turn_counter = checkpoint.turn_counter,
                        escalation_request_id = %esc_rid,
                        "Resuming session from human escalation checkpoint"
                    );
                    let gateway_dir = crate::execution::gateway_root_dir(&self.config);
                    if let Ok(mut report) = crate::runtime::session_report::SessionReportWriter::open(
                        &gateway_dir,
                        session_id,
                        &runtime.manifest.agent.id,
                    ) {
                        let _ = report.record_approval_resolved(
                            esc_rid,
                            "approved",
                            "Resumed from human escalation checkpoint",
                        );
                    }
                    checkpoint.restore_into(runtime);
                    let mut history = checkpoint.history.clone();
                    inject_session_context_after_system_message(
                        &runtime.agent_dir,
                        session_id,
                        &mut history,
                    );
                    let operator = req.decided_by.as_deref().unwrap_or("operator");
                    let guidance_note = req.decision_reason.as_deref().unwrap_or("");
                    let escalation_msg = if guidance_note.is_empty() {
                        format!(
                            "Operator '{}' approved your escalation. You may now continue.",
                            operator
                        )
                    } else {
                        format!(
                            "Operator '{}' approved your escalation with guidance:\n\n{}",
                            operator, guidance_note
                        )
                    };
                    history.push(crate::llm::Message::system(escalation_msg));
                    let initial_msg = checkpoint.initial_user_message();
                    let outcome = execute_with_history_close_on_error(runtime, &mut history).await?;
                    Ok((outcome, initial_msg, Some(checkpoint.turn_id)))
                }
            }
        } else if is_signal_delivered_for_terminal_workflow(
            &self.config,
            self.gateway_store.as_deref(),
            session_id,
            metadata,
        )? {
            tracing::info!(
                target: "checkpoint",
                session_id = %session_id,
                "Suppressing signal-triggered auto-resume: workflow is terminal"
            );
            Ok((
                crate::runtime::lifecycle::TurnOutcome::Completed(None),
                checkpoint.initial_user_message(),
                Some(checkpoint.turn_id),
            ))
        } else if should_auto_resume_checkpoint_yield_reason(&checkpoint.yield_reason) {
            // A resuming session is executing, not parked. Drop its residency row
            // here or the reaper can close a *running* session: the row keeps the
            // `expires_at` it was parked with, so a message that takes longer than
            // the remaining TTL to handle gets a terminal `session_outcomes` row
            // written underneath it (#902 review). Addressability is unaffected —
            // `list_addressable_sessions_for_agent` covers executing sessions
            // through its unfinished-sessions arm — and re-parking at close
            // re-adds the row with a fresh TTL.
            if let Some(store) = self.gateway_store.as_ref() {
                if let Err(e) = store.clear_session_residency(session_id) {
                    tracing::warn!(
                        target: "session_residency",
                        session_id = %session_id,
                        error = %e,
                        "Failed to clear residency on resume; the reaper may close this session while it runs"
                    );
                }
            }
            tracing::info!(
                target: "checkpoint",
                agent_id = %runtime.manifest.agent.id,
                session_id = %session_id,
                turn_counter = checkpoint.turn_counter,
                yield_reason = ?checkpoint.yield_reason,
                "Resuming session from latest checkpoint"
            );
            checkpoint.restore_into(runtime);

            let mut history = checkpoint.history.clone();

            if matches!(
                checkpoint.yield_reason,
                crate::runtime::checkpoint::YieldReason::WaitingForChild { .. }
            ) {
                if let Some(pts) = checkpoint.pending_tool_state.as_ref() {
                    if let Some(am) = checkpoint.assistant_message.as_deref() {
                        let completed_ids: HashSet<&str> = pts
                            .completed_tool_results
                            .iter()
                            .map(|(id, _, _)| id.as_str())
                            .collect();
                        let mut m = am.clone();
                        m.tool_calls.retain(|tc| completed_ids.contains(tc.id.as_str()));
                        if !m.tool_calls.is_empty() || !m.content.trim().is_empty() {
                            history.push(m);
                        }
                    }
                    for (call_id, tool_name, result) in &pts.completed_tool_results {
                        history.push(crate::llm::Message::tool_result(
                            call_id.clone(),
                            tool_name.clone(),
                            result.clone(),
                        ));
                    }
                    let call = crate::llm::ToolCall {
                        id: pts.pending_tool_call.call_id.clone(),
                        name: pts.pending_tool_call.tool_name.clone(),
                        arguments: pts.pending_tool_call.arguments.clone(),
                    };
                    let mut synth = crate::llm::Message::assistant(String::new());
                    synth.tool_calls = vec![call.clone()];
                    runtime.resume_pending_batch = Some((synth, vec![call]));
                } else {
                    inject_session_context_after_system_message(
                        &runtime.agent_dir,
                        session_id,
                        &mut history,
                    );
                    let (turn_start_messages, resume_message) =
                        gateway_signal_turn_start_context(
                            message,
                            metadata,
                            Some(&self.config),
                            self.gateway_store.as_deref(),
                            &session_id,
                        );
                    observe_signal_phase(&mut runtime.session_phase, message);
                    history.extend(turn_start_messages);
                    history.push(crate::llm::Message::user(resume_message));
                }
            } else {
                inject_session_context_after_system_message(
                    &runtime.agent_dir,
                    session_id,
                    &mut history,
                );
                let (turn_start_messages, resume_message) =
                    gateway_signal_turn_start_context(
                        message,
                        metadata,
                        Some(&self.config),
                        self.gateway_store.as_deref(),
                        &session_id,
                    );
                observe_signal_phase(&mut runtime.session_phase, message);
                history.extend(turn_start_messages);
                history.push(crate::llm::Message::user(resume_message));
            }
            let initial_msg = checkpoint.initial_user_message();

            let outcome = execute_with_history_close_on_error(runtime, &mut history).await?;
            Ok((outcome, initial_msg, Some(checkpoint.turn_id)))
        } else {
            tracing::debug!(
                target: "checkpoint",
                session_id = %session_id,
                yield_reason = ?checkpoint.yield_reason,
                "Skipping checkpoint auto-resume for unsupported yield reason"
            );
            let mut history = build_initial_history(
                &runtime.agent_dir,
                &runtime.instructions,
                &runtime.initial_user_message,
                session_id,
                &runtime.manifest,
                &[],
            );
            let outcome = execute_with_history_close_on_error(runtime, &mut history).await?;
            Ok((outcome, runtime.initial_user_message.clone(), None))
        }
    }

    pub async fn spawn_agent_once(
        &self,
        agent_id: &str,
        message: &str,
        session_id: &str,
        source_agent_id: Option<&str>,
        is_message: bool,
        _ingest_event_type: Option<&str>,
        metadata: Option<&serde_json::Value>,
        // Workflow / task context for turn continuation saves on approval suspension.
        workflow_id: Option<&str>,
        task_id: Option<&str>,
        // Artifact ID whose layers should be auto-mounted in the child's sandbox.
        artifact_id: Option<&str>,
        // Spawn-time credential bindings that override runtime.lock resolution.
        credential_bindings: &[autonoetic_types::runtime_lock::LockedCredentialMount],
    ) -> anyhow::Result<SpawnResult> {
        self.spawn_agent_revision_once(
            agent_id,
            None,
            message,
            session_id,
            source_agent_id,
            is_message,
            _ingest_event_type,
            metadata,
            workflow_id,
            task_id,
            artifact_id,
            credential_bindings,
        )
        .await
    }

    pub async fn spawn_agent_revision_once(
        &self,
        agent_id: &str,
        revision_id: Option<&str>,
        message: &str,
        session_id: &str,
        source_agent_id: Option<&str>,
        is_message: bool,
        _ingest_event_type: Option<&str>,
        metadata: Option<&serde_json::Value>,
        // Workflow / task context for turn continuation saves on approval suspension.
        workflow_id: Option<&str>,
        task_id: Option<&str>,
        // Artifact ID whose layers should be auto-mounted in the child's sandbox.
        artifact_id: Option<&str>,
        // Spawn-time credential bindings that override runtime.lock resolution.
        credential_bindings: &[autonoetic_types::runtime_lock::LockedCredentialMount],
    ) -> anyhow::Result<SpawnResult> {
        let span = tracing::info_span!(
            "spawn_agent_revision_once",
            agent_id = agent_id,
            revision_id = ?revision_id,
            session_id = session_id
        );
        let _enter = span.enter();

        tracing::info!("Spawning agent {} (session: {})", agent_id, session_id);

        anyhow::ensure!(!agent_id.trim().is_empty(), "agent_id must not be empty");
        anyhow::ensure!(!message.trim().is_empty(), "message must not be empty");

        let cred_bindings = credential_bindings.to_vec();

        // Pre-resolve the effective agent_id for lock keying.
        //
        // When a session already has a binding (e.g. the root session is bound
        // to planner.collaborative), `resolve_and_pin_session_with_revision`
        // inside the closure will return the *bound* agent — not the requested
        // `agent_id`.  If we acquire the per-agent execution lock with the
        // requested agent_id while actually running a different agent, we
        // contaminate the lock of an unrelated agent, blocking all its real
        // executions for the entire turn duration.
        //
        // This pre-resolution ensures the lock is keyed by the agent that will
        // actually execute, preventing cross-agent lock contamination.
        let lock_agent_id = if let Some(gs) = self.gateway_store.as_ref() {
            match gs.get_session_agent_binding(session_id) {
                Ok(Some(binding)) => binding.agent_id,
                _ => agent_id.to_string(),
            }
        } else {
            agent_id.to_string()
        };
        if lock_agent_id != agent_id {
            tracing::info!(
                target: "execution",
                requested_agent_id = %agent_id,
                resolved_agent_id = %lock_agent_id,
                session_id = %session_id,
                "Lock key resolved to bound agent (differs from requested)"
            );
        }

        let raw_result = self
            .execute_with_reliability_controls(&lock_agent_id, || async move {
                let repo = AgentRepository::from_config(&self.config);

            // The source-agent capability check gates *agent*-initiated spawns
            // and messages. Operator/gateway-initiated spawns (e.g. `/curate`,
            // which enqueues with source `operator`) have no seeded source agent
            // to load — they are already authoritative. Skip the load for such
            // reserved principals; otherwise `get_sync_from_store` bails with
            // "No alias found for agent 'operator'" and the task dies on start.
            if let Some(source_id) = source_agent_id {
                if source_id != agent_id
                    && !autonoetic_types::principal::is_reserved_non_agent_id(source_id)
                {
                    let gateway_dir = crate::execution::gateway_root_dir(&self.config);
                    let source_loaded = repo.get_sync_from_store(source_id, &gateway_dir, self.gateway_store.as_deref())?;
                    let source_policy = crate::policy::PolicyEngine::new(source_loaded.manifest);

                    if is_message {
                        let decision = source_policy.can_message_agent(agent_id);
                        if !decision.is_allowed() {
                            return Err(tagged::Tagged::permission_with_rules(
                                anyhow::anyhow!(
                                    "Permission Denied: Source agent '{}' lacks 'AgentMessage' capability to message '{}'",
                                    source_id,
                                    agent_id
                                ),
                                decision
                                    .enforced_rules
                                    .into_iter()
                                    .map(|rule| rule.to_string())
                                    .collect(),
                            )
                            .into());
                        }
                    } else {
                        let spawn_limit = source_policy.spawn_agent_limit().ok_or_else(|| {
                            anyhow::anyhow!(
                                "Permission Denied: Source agent '{}' lacks 'AgentSpawn' capability",
                                source_id
                            )
                        })?;
                        anyhow::ensure!(
                            spawn_limit > 0,
                            "Permission Denied: Source agent '{}' exceeded AgentSpawn limit (0) for session '{}'",
                            source_id,
                            session_id
                        );
                        let prior_child_spawns = count_spawned_children_for_source_session(
                            self.config.as_ref(),
                            source_id,
                            session_id,
                        )?;
                        anyhow::ensure!(
                            prior_child_spawns < spawn_limit as usize,
                            "Permission Denied: Source agent '{}' exceeded AgentSpawn limit ({}) for session '{}'",
                            source_id,
                            spawn_limit,
                            session_id
                        );

                        // ── Spawn-chain depth cap (R+3 / P-7.15) ─────────────
                        // `session_id` here is the *target* session (the child's).
                        // For the scheduler path this is always `{parent}/{agent}-{uuid}`,
                        // so its depth = parent_depth + 1. The check below ensures
                        // the child session's depth stays below the ceiling.
                        // The router (JSON-RPC) path is operator-controlled via shared
                        // secret and is not subject to agent manipulation.
                        let child_depth = crate::runtime::live_digest::session_depth(session_id) as u32;
                        let system_ceiling = self.config.max_spawn_depth;
                        let agent_ceiling = source_policy.spawn_depth_limit().unwrap_or(0);
                        let effective_ceiling = if agent_ceiling > 0 {
                            std::cmp::min(agent_ceiling, system_ceiling)
                        } else {
                            system_ceiling
                        };
                        anyhow::ensure!(
                            child_depth < effective_ceiling,
                            "Permission Denied: Source agent '{}' spawn at depth {} would reach ceiling ({}) — max_spawn_depth exceeded for session '{}'",
                            source_id,
                            child_depth,
                            effective_ceiling,
                            session_id
                        );
                    }
                }
            }

            // ─────────────────────────────────────────────────────────────
            // Revision-based resolution (Phase 1d+): sessions execute from
            // pinned immutable revision directories only.
            // ─────────────────────────────────────────────────────────────
            let Some(ref gs) = self.gateway_store else {
                anyhow::bail!(
                    "GatewayStore is required for session resolution. \
                     Agent '{}' cannot be loaded without a gateway store.",
                    agent_id
                );
            };
            let resolve_start = std::time::Instant::now();
            let (agent_ref, _rev, _binding) = repo.resolve_and_pin_session_with_revision(
                session_id,
                session_id, // root_session_id = session_id for single sessions
                agent_id,
                revision_id,
                Some(gs.as_ref()),
                &default_gateway_host_id(),
            )?;
            tracing::info!(
                agent_id = %agent_ref.agent_id,
                revision_id = %agent_ref.revision_id,
                session_id = session_id,
                elapsed_ms = resolve_start.elapsed().as_millis(),
                "Resolved session to pinned revision"
            );
            let gateway_dir = crate::execution::gateway_root_dir(&self.config);
            let mut loaded =
                repo.load_from_revision_dir(&gateway_dir, &agent_ref.agent_id, &agent_ref.revision_id)?;

            // I/O contract of the manifest that will actually execute this turn.
            // Response validation (after this closure) MUST key off the executed
            // manifest — never a post-hoc alias re-load — so candidate-revision
            // smoke tests (spawn with `revision_id`, no alias installed yet) are
            // validated against the exact manifest that ran.
            let executed_io_contract = ExecutedIoContract {
                returns_schema: loaded
                    .manifest
                    .io
                    .as_ref()
                    .and_then(|io| io.returns.clone()),
                output_policy: loaded
                    .manifest
                    .io
                    .as_ref()
                    .and_then(|io| io.output_policy.clone()),
                execution_mode: loaded.manifest.execution_mode,
                returns_enforcement: loaded
                    .manifest
                    .io
                    .as_ref()
                    .map(|io| io.effective_returns_enforcement(loaded.manifest.execution_mode))
                    .unwrap_or_default(),
                agent_is_spawn_capable: loaded.manifest.capabilities.iter().any(|c| {
                    matches!(c, autonoetic_types::capability::Capability::AgentSpawn { .. })
                }),
            };

            if let Some(ref gs) = self.gateway_store {
                // Only seed placeholder for truly new sessions — don't overwrite
                // an existing transcript record (e.g. on checkpoint resume, approval
                // continuation, or chat follow-up), which would reset transcript_handle
                // to None and break session_peek until the next persist cycle.
                let existing = gs
                    .find_transcript_by_session_id(session_id)
                    .ok()
                    .flatten();
                if existing.is_none() {
                    let root_sid =
                        crate::runtime::content_store::root_session_id(session_id).to_string();
                    let placeholder = autonoetic_types::causal_chain::SessionTranscriptRecord {
                        transcript_id: format!("stx-{}", session_id),
                        session_id: session_id.to_string(),
                        root_session_id: root_sid,
                        agent_id: agent_ref.agent_id.clone(),
                        revision_id: Some(agent_ref.revision_id.clone()),
                        user_id: None,
                        started_at: chrono::Utc::now().to_rfc3339(),
                        ended_at: None,
                        status: "active".to_string(),
                        turn_count: 0,
                        transcript_handle: None,
                        excerpt: None,
                        origin_node_id: None,
                    };
                    if let Err(e) = gs.upsert_session_transcript(&placeholder) {
                        tracing::debug!(
                            target: "execution",
                            session_id = %session_id,
                            error = %e,
                            "Failed to seed placeholder transcript (non-fatal)"
                        );
                    }
                }
            }

            // Validate spawn input against target agent's accepts schema (observational).
            // Hard enforcement + structured rejection happens earlier in the agent.spawn
            // tool entry (runtime/tools/agent.rs). This second pass just records a causal
            // breadcrumb for callers that bypassed the tool entry path.
            if let Some(ref io_schema) = loaded.manifest.io {
                if let Some(ref accepts) = io_schema.accepts {
                    let validation = validate_against_schema(message, accepts);
                    tracing::info!(
                        agent_id = agent_id,
                        valid = validation.valid,
                        issues = ?validation.issues,
                        "Input schema validation"
                    );
                }
            }
            // Re-open the session transcript for this turn.  Between turns
            // close_session sets the status to 'completed', which the orphan-
            // child reaper (R+12) treats as "parent terminated" and will cancel
            // any active children.  Resetting to 'active' at the start of every
            // turn prevents immediate orphaning of children spawned during this
            // turn's execution.
            let _ = gs.reopen_session_transcript(session_id);

            // Background signaling is handled by the notification processor
            let should_signal_background = false;
            // Signal notifications for background scheduler if this is an event.ingest call
            if should_signal_background {
                // Signals are now delivered through GatewayStore notifications
                // The scheduler will pick them up on its next poll
            }

            // --- Fast path for script-only agents ---
            if matches!(loaded.manifest.execution_mode, ExecutionMode::Script) {
                let script_entry = loaded.manifest.script_entry.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "Agent '{}' has execution_mode=script but is missing script_entry",
                        agent_id
                    )
                })?;
                let script_path = loaded.dir.join(script_entry);
                if !script_path.exists() {
                    anyhow::bail!(
                        "Script entry point not found: {}",
                        script_path.display()
                    );
                }

                // Open session report for this script agent so it appears in
                // session_overview.md and causal_events — the LLM fast path skips the
                // full SessionTracer, so we wire it up manually here.
                let gateway_dir = crate::execution::gateway_root_dir(&self.config);
                let mut report = SessionReportWriter::open_with_options(
                    &gateway_dir,
                    session_id,
                    agent_id,
                    self.config.session_report.live_html_on_update,
                )
                .ok();
                if let Some(ref mut r) = report {
                    let _ = r.start_session(message);
                    let _ = r.record_tool_requested(
                        "sandbox_exec",
                        &format!("run {}", script_entry),
                        None,
                    );
                }
                script_causal_event(
                    self.gateway_store.as_deref(),
                    agent_id,
                    session_id,
                    1,
                    "started",
                    "success",
                    serde_json::json!({
                        "script_entry": script_entry,
                        "sandbox": loaded.manifest.runtime.sandbox
                    }),
                );

                // Egress label for this run (#1062). Resolved before execution
                // so both the success and failure trace carry it, and recorded
                // as the session's §5.5 taint so a parent that surfaces this
                // script's output inherits the restriction instead of treating
                // it as unrestricted.
                let script_egress_label = self.resolve_script_exec_egress_label(
                    &loaded.manifest,
                    &loaded.dir,
                    &gateway_dir,
                    session_id,
                    agent_id,
                    script_entry,
                    metadata,
                );
                if let (Some(ref gs), Some(ref label)) =
                    (self.gateway_store.as_ref(), script_egress_label.as_ref())
                {
                    if let Err(e) = gs.set_session_egress_taint(session_id, label) {
                        tracing::warn!(
                            target: "egress",
                            error = %e,
                            session_id = %session_id,
                            "failed to record script session egress taint (§5.5)"
                        );
                    }
                }

                // Execute script directly in sandbox
                let trace_started_at = std::time::Instant::now();
                let script_kill_scope = Some((
                    self.active_executions.clone(),
                    crate::runtime::live_digest::base_session_id(session_id).to_string(),
                ));
                let credential_env = if let (Some(ref gs), gw_dir) = (
                    self.gateway_store.as_deref(),
                    crate::execution::gateway_root_dir(&self.config),
                ) {
                    crate::runtime::script_execute::resolve_credential_env_with_bindings(
                        &loaded.dir,
                        &gw_dir,
                        gs,
                        &cred_bindings,
                    )?
                } else {
                    vec![]
                };
                tracing::info!(
                    target: "script_exec",
                    agent_id = %agent_id,
                    session_id = %session_id,
                    "Calling execute_script_in_sandbox"
                );
                let script_exec_start = std::time::Instant::now();
                let script_result = execute_script_in_sandbox(
                    &loaded.dir,
                    &script_path,
                    message,
                    metadata,
                    &loaded.manifest.runtime.sandbox,
                    self.config.as_ref(),
                    script_kill_scope,
                    &loaded.manifest.capabilities,
                    loaded.manifest.script_input_mode,
                    credential_env,
                    &loaded.manifest.runtime.runtime_lock,
                    Some(&gateway_dir),
                )
                .await;
                tracing::info!(
                    target: "script_exec",
                    agent_id = %agent_id,
                    session_id = %session_id,
                    elapsed_ms = script_exec_start.elapsed().as_millis(),
                    success = script_result.is_ok(),
                    "execute_script_in_sandbox returned"
                );

                // Record completion/failure in session report and causal_events
                match &script_result {
                    Ok(output) => {
                        if let Some(ref mut r) = report {
                            let result_json = serde_json::json!({
                                "ok": true,
                                "exit_code": 0,
                                "stdout": &output[..output.len().min(512)],
                            })
                            .to_string();
                            let _ = r.record_tool_completed(
                                "sandbox_exec",
                                &result_json,
                                None,
                                None,
                                None,
                            );
                            let _ = r.finish_session(SessionCloseOutcome::ScriptExecComplete, Some(output));
                        }
                        script_causal_event(
                            self.gateway_store.as_deref(),
                            agent_id,
                            session_id,
                            2,
                            "completed",
                            "success",
                            serde_json::json!({ "result_len": output.len() }),
                        );
                        if let Some(ref gs) = self.gateway_store {
                            let trace = autonoetic_types::causal_chain::ExecutionTraceRecord {
                                trace_id: uuid::Uuid::new_v4().to_string(),
                                event_id: None,
                                agent_id: agent_id.to_string(),
                                session_id: session_id.to_string(),
                                turn_id: None,
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                tool_name: "sandbox_exec".to_string(),
                                command: Some(script_entry.clone()),
                                exit_code: Some(0),
                                stdout: Some(output.clone()),
                                stderr: None,
                                duration_ms: trace_started_at.elapsed().as_millis() as i64,
                                success: 1,
                                error_type: None,
                                error_summary: None,
                                approval_required: Some(0),
                                approval_request_id: None,
                                arguments: Some(format!("run {}", script_entry)),
                                result: Some(output.clone()),
                                egress_label: script_egress_label.clone(),
                                mount_set: None,
                            };
                            let _ = gs.create_execution_trace(&trace);
                        }
                        // Mirror the script's stdout onto the live-digest
                        // timeline as an `agent.message` so it shows inline in
                        // the room TUI at the default altitude. Without this,
                        // script output is captured only in `execution_traces`
                        // (execution_search) and `causal_events`, neither of
                        // which the room reads — see issue #644.
                        crate::runtime::script_execute::emit_script_message_timeline(
                            self.gateway_store.as_deref(),
                            agent_id,
                            session_id,
                            &output,
                        );
                        let ended_at = chrono::Utc::now().to_rfc3339();
                        let _ = gs.finalize_session_transcript(session_id, &ended_at, "completed");
                    }
                    Err(e) => {
                        if let Some(ref mut r) = report {
                            let _ = r.record_execution_failure(
                                "sandbox_exec",
                                &e.to_string(),
                                None,
                                None,
                                None,
                            );
                            let _ = r.finish_session(SessionCloseOutcome::ScriptExecFailed, None);
                        }
                        script_causal_event(
                            self.gateway_store.as_deref(),
                            agent_id,
                            session_id,
                            2,
                            "failed",
                            "error",
                            serde_json::json!({ "error": e.to_string() }),
                        );
                        if let Some(ref gs) = self.gateway_store {
                            let trace = autonoetic_types::causal_chain::ExecutionTraceRecord {
                                trace_id: uuid::Uuid::new_v4().to_string(),
                                event_id: None,
                                agent_id: agent_id.to_string(),
                                session_id: session_id.to_string(),
                                turn_id: None,
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                tool_name: "sandbox_exec".to_string(),
                                command: Some(script_entry.clone()),
                                exit_code: None,
                                stdout: None,
                                stderr: Some(e.to_string()),
                                duration_ms: trace_started_at.elapsed().as_millis() as i64,
                                success: 0,
                                error_type: Some("script_execution_error".to_string()),
                                error_summary: Some(e.to_string()),
                                approval_required: Some(0),
                                approval_request_id: None,
                                arguments: Some(format!("run {}", script_entry)),
                                result: None,
                                egress_label: script_egress_label.clone(),
                                mount_set: None,
                            };
                            let _ = gs.create_execution_trace(&trace);
                        }
                        let ended_at = chrono::Utc::now().to_rfc3339();
                        let _ = gs.finalize_session_transcript(session_id, &ended_at, "failed");
                    }
                }

                // Return result (or error)
                let script_result = script_result?;

                // Extract artifacts from content store
                let artifacts = extract_artifacts_from_content_store(
                    &crate::execution::gateway_root_dir(&self.config),
                    session_id,
                ).unwrap_or_default();

                // Collect all named content written by the child agent
                let files = collect_named_content(
                    &crate::execution::gateway_root_dir(&self.config),
                    session_id,
                );

                // Collect shared knowledge (for script mode, typically empty)
                let shared_knowledge = collect_shared_knowledge(
                    &crate::execution::gateway_root_dir(&self.config),
                    source_agent_id.unwrap_or(agent_id),
                    agent_id,
                    Some(session_id),
                );

                return Ok((
                    SpawnResult {
                        agent_id: agent_id.to_string(),
                        session_id: session_id.to_string(),
                        assistant_reply: Some(script_result),
                        workflow_note: None,
                        should_signal_background,
                        artifacts,
                        files,
                        shared_knowledge,
                        llm_usage: Vec::new(),
                        suspended_for_approval: None,
                        suspended_for_user_input: false,
                        suspended_for_child_wait: false,
                    },
                    executed_io_contract,
                ));
            }

            let inference = self.resolve_spawn_inference_profile(
                agent_id,
                &loaded.manifest,
                session_id,
            )?;
            let driver = build_driver(inference.llm_config.clone(), self.http_client.clone())?;
            // Propagate the resolved llm_config (with context_window_tokens and any
            // overrides) back into the manifest so the context governor can use it.
            loaded.manifest.llm_config = Some(inference.llm_config.clone());

            let openrouter_catalog =
                Arc::new(OpenRouterCatalog::new(self.http_client.clone()));
            let middleware = loaded.manifest.middleware.clone().unwrap_or_default();
            let mut runtime = self.attach_model_metadata(
                AgentExecutor::new(
                loaded.manifest,
                loaded.instructions,
                driver,
                loaded.dir,
                crate::runtime::tools::registry_for_config(Some(self.config.as_ref())),
                self.gateway_store.clone(),
            )
            .with_resolved_inference(inference)
            .with_gateway_dir(crate::execution::gateway_root_dir(&self.config))
            .with_config(self.config.clone())
            .with_session_budget(Some(self.session_budget.clone()))
            .with_root_session_budget(Some(self.root_session_budget.clone()))
            .with_middleware(middleware)
            .with_initial_user_message(message.to_string())
            .with_session_id(session_id.to_string())
            .with_workflow_context(
                workflow_id.map(String::from),
                task_id.map(String::from),
            )
            .with_active_executions(Some(self.active_executions.clone()))
            .with_http_client(self.http_client.clone())
            .with_artifact_id(artifact_id.map(String::from))
            .with_degraded_sessions(Some(self.degraded_sessions.clone()))
        .with_persona(self.persona.clone())
        .with_extended_instructions(loaded.extended_instructions.clone()),
                openrouter_catalog,
            );
            // Ingest-side label birth (RFC §4.5 "User/operator message" and the
            // OFP inbound path). The incoming user turn's label is the
            // intersection of everything declared about it:
            //
            //   session policy `default_label`  (what the operator declared for
            //                                    unlabeled content in this room)
            // ∩ operator per-message mark        (§5.4 rung 3, "this one is private")
            // ∩ peer-supplied inbound label      (federation, fail-closed upstream)
            //
            // Intersection means every contributor can only restrict, so no
            // channel here can widen what another already tightened — the
            // property that lets the per-message mark be honored from any caller
            // without becoming a bypass. Per I-14 these are all *declared
            // inputs*: none of them is model output.
            if let Some(label) = self.resolve_ingest_egress_label(session_id, metadata) {
                if let Some(ref gs) = self.gateway_store {
                    // `restrict_`, not `set_`: an ingest label is an incremental
                    // contribution, and a resumed session may already carry a
                    // more restrictive accumulated taint that must survive.
                    if let Err(e) = gs.restrict_session_egress_taint(session_id, &label) {
                        tracing::warn!(
                            target: "egress",
                            error = %e,
                            session_id = %session_id,
                            "failed to seed ingest session egress taint"
                        );
                    }
                }
                // `initial_ingest_egress_label` labels the first user-role turn
                // of this run, minting the msg id that joins it to the label map
                // the chokepoint reads (§3.4).
                runtime = runtime.with_initial_ingest_egress_label(label);
            }
            // Phase 3: propagate overflow_recovery flag so the governor
            // uses an aggressive reduction pipeline on retry.
            let overflow_recovery = metadata
                .and_then(|m| m.get("overflow_recovery"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if overflow_recovery {
                runtime = runtime.with_overflow_recovery(true);
            }

            // --- Session checkpoint resume ---
            // Continuations are fully replaced by enriched checkpoints.  Every
            // suspension state (approval, budget, hibernation, etc.) is captured
            // in a single SessionCheckpoint.  The dispatch below:
            //   1) With task_id — load checkpoint and let resume_from_checkpoint
            //      handle the approval status (approved / rejected / pending).
            //   2) Without task_id — same checkpoint / fresh-start logic.
            // Run the turn-execution dispatch into a Result so a root-session
            // budget exhaustion (C2 / #616) can fire the one-time graceful root
            // budget circuit breaker before the error propagates. We intentionally
            // do NOT use `?` on the dispatch expression directly: on `Err`, the
            // executor has flagged `runtime.root_budget_exhausted` iff the failing
            // budget check was the ROOT-tree budget (never per-session budget).
            let dispatch_result: anyhow::Result<(
                crate::runtime::lifecycle::TurnOutcome,
                String,
                Option<String>,
            )> = async {
            Ok(if task_id.is_some() {
                let checkpoint = crate::runtime::checkpoint::load_latest_checkpoint_strict(
                    &self.config,
                    session_id,
                )?;
                if let Some(checkpoint) = checkpoint {
                    self.resume_from_checkpoint(
                        &mut runtime,
                        session_id,
                        message,
                        metadata,
                        checkpoint,
                    )
                    .await?
                } else {
                    let (turn_start_messages, initial_message) =
                        gateway_signal_turn_start_context(
                            &runtime.initial_user_message,
                            metadata,
                            Some(&self.config),
                            self.gateway_store.as_deref(),
                            session_id,
                        );
                    let signal_payload = runtime.initial_user_message.clone();
                    observe_signal_phase(&mut runtime.session_phase, &signal_payload);
                    let mut history = build_initial_history(
                        &runtime.agent_dir,
                        &runtime.instructions,
                        &initial_message,
                        session_id,
                        &runtime.manifest,
                        &turn_start_messages,
                    );
                    let outcome = execute_with_history_close_on_error(&mut runtime, &mut history).await?;
                    (outcome, runtime.initial_user_message.clone(), None)
                }
            } else {
                let checkpoint =
                    crate::runtime::checkpoint::load_latest_checkpoint_strict(&self.config, session_id)?;
                if let Some(checkpoint) = checkpoint {
                    // P-6.14: an EmergencyStop checkpoint is never auto-resumed —
                    // not by signals, not by queued dispatches, not by a manual
                    // message (the resume-trigger coherence gate in
                    // session_resume.rs refuses every trigger). `resume_from_checkpoint`
                    // enforces the refusal. The historical "continuing with
                    // preserved context and fresh LoopGuard" branch (125485f5)
                    // bypassed that gate: every queued dispatch after an emergency
                    // stop resumed the stopped session, immediately re-yielded on
                    // the lifecycle pre-flight guard, re-saved the checkpoint with
                    // an incremented turn counter, closed the root session with an
                    // error, and re-fired the workflow-failure cascade
                    // (session-d1d8c2bb churn).
                    self.resume_from_checkpoint(
                        &mut runtime,
                        session_id,
                        message,
                        metadata,
                        checkpoint,
                    )
                    .await?
                } else {
                    let (turn_start_messages, initial_message) =
                        gateway_signal_turn_start_context(
                            &runtime.initial_user_message,
                            metadata,
                            Some(&self.config),
                            self.gateway_store.as_deref(),
                            session_id,
                        );
                    let signal_payload = runtime.initial_user_message.clone();
                    observe_signal_phase(&mut runtime.session_phase, &signal_payload);
                    let mut history = build_initial_history(
                        &runtime.agent_dir,
                        &runtime.instructions,
                        &initial_message,
                        session_id,
                        &runtime.manifest,
                        &turn_start_messages,
                    );
                    let outcome = execute_with_history_close_on_error(&mut runtime, &mut history).await?;
                    (outcome, runtime.initial_user_message.clone(), None)
                }
            })
            }.await;

            let (outcome, resume_initial_message, consumed_checkpoint_turn_id) = match dispatch_result {
                Ok(triple) => triple,
                Err(e) => {
                    // Root-session-tree budget exhausted: fire the graceful root
                    // budget circuit breaker exactly once (idempotent) to cancel
                    // in-flight descendants so they stop burning the already-spent
                    // tree budget (P-6.21). Per-session budget exhaustion never
                    // sets this flag, so it never cascades.
                    if runtime.root_budget_exhausted {
                        let root = crate::runtime::content_store::root_session_id(session_id)
                            .to_string();
                        self.trigger_root_budget_circuit_breaker(&root, &e).await;
                    }
                    // Checkpoint integrity violation (HMAC tamper) on the
                    // latest checkpoint: record the audit trail and revoke the
                    // bound approval before aborting the resume (#606).
                    if crate::runtime::checkpoint::is_integrity_error(&e) {
                        self.handle_checkpoint_integrity_violation(
                            session_id,
                            agent_id,
                            None,
                            "checkpoint HMAC verification failed on resume",
                        );
                    }
                    return Err(e);
                }
            };

            let resolved_session_id = runtime
                .session_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("runtime session_id missing after execution"))?;

            let close_flags = session_close_flags_from_turn_outcome(outcome);

            let is_signal = crate::runtime::session_timeline::is_signal_delivered_chat(metadata);
            persist_session_context_turn(
                &runtime.agent_dir,
                self.gateway_store.as_deref(),
                &resolved_session_id,
                &resume_initial_message,
                close_flags.assistant_reply.as_deref(),
                is_signal,
            );
            let finalized = self.finalize_session(
                &mut runtime,
                resolved_session_id.clone(),
                agent_id,
                source_agent_id,
                close_flags,
                true,
                consumed_checkpoint_turn_id,
            ).await?;
            Ok((finalized, executed_io_contract))
        })
        .await?;
        let (mut result, executed_io) = (raw_result.0, raw_result.1);
        if source_agent_id.is_some() {
            log_nested_spawn_to_gateway(
                self.config.as_ref(),
                session_id,
                source_agent_id,
                agent_id,
                message,
                &result,
            );
        }

        // Response validation gate:
        // - output shape is enforced from manifest `io.returns`.
        // - non-schema runtime constraints are enforced from manifest `io.output_policy`.
        // Validation is skipped for suspended sessions (they haven't finished producing output).
        //
        // The contract comes from the manifest that actually executed the turn
        // (`executed_io`), not from an alias re-load: candidate-revision smoke
        // tests spawn with `revision_id` before any alias exists, so an alias
        // lookup would resolve to None and silently skip validation — the exact
        // gap that let schema-violating agents promote (session-b5c8f091).
        let manifest_returns_schema = executed_io.returns_schema.clone();
        let manifest_output_policy = executed_io.output_policy.clone();
        let manifest_returns_enforcement = executed_io.returns_enforcement;

        if let Some(meta) = metadata {
            if meta.get("response_contract").is_some() {
                return Err(anyhow::anyhow!(
                    "response_contract metadata is no longer supported; declare io.output_policy in the target agent manifest"
                ));
            }
            if meta
                .get("io")
                .and_then(|io| io.get("returns").or_else(|| io.get("output_policy")))
                .is_some()
            {
                return Err(anyhow::anyhow!(
                    "spawn metadata may not override io.returns or io.output_policy; declare them in the target agent manifest"
                ));
            }
        }

        if result.suspended_for_approval.is_none()
            && !result.suspended_for_user_input
            && !result.suspended_for_child_wait
            && (manifest_returns_schema.is_some()
                || (self.config.response_validation.enabled && manifest_output_policy.is_some()))
        {
            let mut output_policy = manifest_output_policy.unwrap_or_default();
            output_policy.normalize();
            let validation_session_id = result.session_id.clone();
            match self
                .validate_and_maybe_repair(
                    agent_id,
                    result,
                    manifest_returns_schema.as_ref(),
                    &output_policy,
                    manifest_returns_enforcement,
                    source_agent_id,
                    workflow_id,
                    task_id,
                    executed_io.agent_is_spawn_capable,
                    None,
                    executed_io.execution_mode,
                )
                .await
            {
                Ok(validated) => {
                    if let Some(expected_schema) = manifest_returns_schema.as_ref() {
                        log_contract_enforcement_event_to_gateway(
                            self.gateway_store.as_deref(),
                            agent_id,
                            &validated.session_id,
                            "io.returns",
                            EntryStatus::Success,
                            source_agent_id,
                            serde_json::json!({
                                "contract": "io.returns",
                                "result": "pass",
                                "expected_schema": expected_schema,
                                "source_agent_id": source_agent_id,
                                "enforcer": "response_validation"
                            }),
                        );
                    }
                    result = validated;
                }
                Err(error) => {
                    if let Some(expected_schema) = manifest_returns_schema.as_ref() {
                        log_contract_enforcement_event_to_gateway(
                            self.gateway_store.as_deref(),
                            agent_id,
                            &validation_session_id,
                            "io.returns",
                            EntryStatus::Denied,
                            source_agent_id,
                            serde_json::json!({
                                "contract": "io.returns",
                                "result": "rejected",
                                "expected_schema": expected_schema,
                                "source_agent_id": source_agent_id,
                                "reason": error.to_string(),
                                "enforcer": "response_validation"
                            }),
                        );
                    }
                    return Err(error);
                }
            }
        }

        result = self.validate_promotion_gate(
            agent_id,
            result,
            metadata,
            source_agent_id,
            workflow_id,
            task_id,
        ).await?;

        Ok(result)
    }

    /// Resume execution after a `user_ask` interaction was answered.
    ///
    /// Loads the interaction to extract session/agent/workflow identity, then
    /// delegates to `spawn_agent_once` which handles checkpoint loading and the
    /// `UserInputRequired` resume branch (answer injection + continued execution).
    ///
    /// Returns a structured error `session_waiting_for_approval:{session}:{id}` when
    /// the latest checkpoint has shifted to `ApprovalRequired` — the scheduler uses
    /// this to defer the resume to the approval path.
    /// #741: the single resume entrypoint. Verifies the trigger is coherent
    /// with the session's latest checkpoint (`verify_trigger_coherence`) and
    /// dispatches to the one reconstruction path (`spawn_agent_once` →
    /// `resume_from_checkpoint`). Callers stop re-deriving "load checkpoint →
    /// branch on YieldReason → rebuild" per trigger kind.
    ///
    /// Validation-repair respawns stay on [`Self::respawn_from_checkpoint`]
    /// deliberately: they *replay a completed turn* (Hibernation checkpoint)
    /// rather than resume a suspended one, with their own repair-budget
    /// accounting.
    pub async fn resume_session(
        &self,
        trigger: ResumeTrigger,
        follow_up_message: Option<&str>,
    ) -> anyhow::Result<SpawnResult> {
        // Match by reference so `trigger` stays borrowable for the coherence
        // checks inside the arms (the `ref`-binding equivalent, made explicit
        // after a review misread — #749).
        match &trigger {
            ResumeTrigger::InteractionAnswered { interaction_id } => {
                self.resume_interaction_inner(interaction_id, follow_up_message)
                    .await
            }
            ResumeTrigger::ApprovalResolved { request_id } => {
                let store = self.gateway_store.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("GatewayStore is required to resume approval-gated sessions")
                })?;
                let req = store.get_approval(request_id)?.ok_or_else(|| {
                    anyhow::anyhow!("Unknown approval request '{}'", request_id)
                })?;
                let session_id = req.session_id.clone();

                // Trigger/YieldReason coherence: the latest checkpoint must be
                // parked on exactly this approval.
                if let Some(cp) = crate::runtime::checkpoint::load_latest_checkpoint(
                    self.config.as_ref(),
                    &session_id,
                )? {
                    if let Err(inc) = verify_trigger_coherence(&trigger, &cp.yield_reason) {
                        anyhow::bail!(
                            "Cannot resume session '{}' for approval '{}': {}",
                            session_id,
                            request_id,
                            render_trigger_incoherence(&inc)
                        );
                    }
                }

                let binding = store.get_session_agent_binding(&session_id)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "No session binding found for approval resume of session '{}'",
                        session_id
                    )
                })?;
                self.spawn_agent_once(
                    &binding.agent_id,
                    follow_up_message.unwrap_or(
                        "[gateway] The pending approval was resolved by the operator.",
                    ),
                    &session_id,
                    None,
                    false,
                    None,
                    None,
                    req.workflow_id.as_deref(),
                    req.task_id.as_deref(),
                    None,
                    &[],
                )
                .await
            }
            ResumeTrigger::Manual { session_id } => {
                let store = self.gateway_store.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("GatewayStore is required to resume sessions")
                })?;
                if let Some(cp) = crate::runtime::checkpoint::load_latest_checkpoint(
                    self.config.as_ref(),
                    session_id,
                )? {
                    if let Err(inc) = verify_trigger_coherence(&trigger, &cp.yield_reason) {
                        anyhow::bail!(
                            "Cannot resume session '{}': {}",
                            session_id,
                            render_trigger_incoherence(&inc)
                        );
                    }
                }
                let binding = store.get_session_agent_binding(session_id)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "No session binding found for manual resume of session '{}'",
                        session_id
                    )
                })?;
                self.spawn_agent_once(
                    &binding.agent_id,
                    follow_up_message.unwrap_or("[operator] Resume the session."),
                    session_id,
                    None,
                    false,
                    None,
                    None,
                    None,
                    None,
                    None,
                    &[],
                )
                .await
            }
        }
    }

    /// Compatibility wrapper — the typed path is
    /// [`Self::resume_session`] with [`ResumeTrigger::InteractionAnswered`].
    pub async fn resume_from_user_interaction(
        &self,
        interaction_id: &str,
        follow_up_user_message: Option<&str>,
    ) -> anyhow::Result<SpawnResult> {
        self.resume_session(
            ResumeTrigger::InteractionAnswered {
                interaction_id: interaction_id.to_string(),
            },
            follow_up_user_message,
        )
        .await
    }

    async fn resume_interaction_inner(
        &self,
        interaction_id: &str,
        follow_up_user_message: Option<&str>,
    ) -> anyhow::Result<SpawnResult> {
        let store = self.gateway_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("GatewayStore is required to resume user interactions")
        })?;

        let interaction = store
            .get_user_interaction(interaction_id)?
            .ok_or_else(|| anyhow::anyhow!("Unknown user interaction '{}'", interaction_id))?;

        if interaction.status != UserInteractionStatus::Answered {
            anyhow::bail!(
                "Interaction '{}' is {:?}; answer it before calling resume_from_user_interaction",
                interaction_id,
                interaction.status
            );
        }

        // If the interaction is bound to a terminal workflow, do not attempt to
        // resume — the session cannot make progress and the agent execution may
        // hang indefinitely.
        if let Some(ref wf_id) = interaction.workflow_id {
            if crate::scheduler::workflow_store::is_workflow_terminal(
                self.config.as_ref(),
                self.gateway_store.as_deref(),
                wf_id,
            )? {
                anyhow::bail!(
                    "Cannot resume from interaction {}: workflow {} is already terminal",
                    interaction_id,
                    wf_id
                );
            }
        }

        // Acquire the resume claim before any spawn attempt. This is the single
        // gate for all resume paths (UserInputRequired, EmergencyStop, etc.) and
        // prevents the scheduler from spawning multiple concurrent executions for
        // the same interaction when polling every ~5s.
        if !store.try_claim_answered_standalone_interaction_resume(interaction_id)? {
            anyhow::bail!(
                "Interaction '{}' is already claimed by another resume attempt",
                interaction_id,
            );
        }

        // Pre-check (#741): trigger/YieldReason coherence via the shared
        // helper. Error strings are preserved verbatim — the scheduler's
        // standalone-interaction sweep machine-matches the
        // `session_waiting_for_approval:<session>:<rid>` prefix.
        use crate::runtime::checkpoint::load_latest_checkpoint;
        if let Some(cp) = load_latest_checkpoint(self.config.as_ref(), &interaction.session_id)? {
            let trigger = ResumeTrigger::InteractionAnswered {
                interaction_id: interaction.interaction_id.clone(),
            };
            match verify_trigger_coherence(&trigger, &cp.yield_reason) {
                Ok(()) => {}
                Err(TriggerIncoherence::InteractionMismatch { expected, got }) => {
                    anyhow::bail!("Checkpoint is for interaction '{}', not '{}'", expected, got);
                }
                Err(TriggerIncoherence::WaitingForApproval { request_id }) => {
                    tracing::debug!(
                        target: "scheduler",
                        interaction_id = %interaction.interaction_id,
                        session_id = %interaction.session_id,
                        approval_request_id = %request_id,
                        "Skipping user-interaction resume: session is now waiting for approval"
                    );
                    return Err(anyhow::anyhow!(
                        "session_waiting_for_approval:{}:{}",
                        interaction.session_id,
                        request_id
                    ));
                }
                Err(TriggerIncoherence::WrongYieldReason { got }) => {
                    anyhow::bail!(
                        "Latest checkpoint for session '{}' is not UserInputRequired (got {})",
                        interaction.session_id,
                        got
                    );
                }
                Err(inc @ (TriggerIncoherence::EmergencyStop
                | TriggerIncoherence::ApprovalMismatch { .. })) => {
                    anyhow::bail!(
                        "Cannot resume session '{}' from interaction '{}': {}",
                        interaction.session_id,
                        interaction.interaction_id,
                        render_trigger_incoherence(&inc)
                    );
                }
            }
        }

        let spawn_result = self
            .spawn_agent_once(
                &interaction.agent_id,
                follow_up_user_message.unwrap_or(
                    "[operator] User answered the pending question via gateway interactions.",
                ),
                &interaction.session_id,
                None,
                false,
                None,
                None,
                interaction.workflow_id.as_deref(),
                interaction.task_id.as_deref(),
                None,
                &[],
            )
            .await;

        // On error, release the claim so the scheduler can retry later.
        if spawn_result.is_err() {
            if let Some(s) = self.gateway_store.as_ref() {
                if let Err(release_err) =
                    s.release_answered_standalone_interaction_resume_claim(interaction_id)
                {
                    tracing::warn!(
                        target: "interaction",
                        interaction_id = %interaction_id,
                        error = %release_err,
                        "Failed to release interaction resume claim after spawn failure"
                    );
                }
            }
        }
        // On success the claim is consumed (single-use) and NOT released, so
        // subsequent scheduler polls will fail the try_claim above and skip.

        spawn_result
    }

    /// Respawn an agent from a previously saved checkpoint.
    ///
    /// Loads the checkpoint for the given session, reconstructs the executor state,
    /// and calls `execute_with_history` with the checkpoint's conversation history.
    ///
    /// Returns the same `SpawnResult` as `spawn_agent_once` but with the checkpoint's
    /// conversation as the starting point instead of a fresh one.
    pub async fn respawn_from_checkpoint(
        &self,
        agent_id: &str,
        session_id: &str,
        additional_message: Option<&str>,
        source_agent_id: Option<&str>,
        workflow_id: Option<&str>,
        task_id: Option<&str>,
        initial_feedback: &[autonoetic_types::trajectory::FeedbackEvent],
    ) -> anyhow::Result<SpawnResult> {
        use crate::runtime::checkpoint::{load_latest_checkpoint, YieldReason};

        let span = tracing::info_span!(
            "respawn_from_checkpoint",
            agent_id = agent_id,
            session_id = session_id
        );
        let _enter = span.enter();

        let checkpoint = load_latest_checkpoint(&self.config, session_id)?
            .ok_or_else(|| anyhow::anyhow!("No checkpoint found for session '{}'", session_id))?;

        tracing::info!(
            target: "checkpoint",
            agent_id = %agent_id,
            session_id = %session_id,
            turn_counter = checkpoint.turn_counter,
            yield_reason = ?checkpoint.yield_reason,
            "Respawning agent from checkpoint"
        );

        // EmergencyStop checkpoints cannot be auto-resumed
        if matches!(checkpoint.yield_reason, YieldReason::EmergencyStop { .. }) {
            anyhow::bail!(
                "Cannot auto-resume from EmergencyStop checkpoint. Manual restart required."
            );
        }

        let repo = AgentRepository::from_config(&self.config);
        let mut loaded = if let Some(ref gs) = self.gateway_store {
            let binding = gs.get_session_agent_binding(session_id)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "No session binding found for checkpoint respawn of session '{}'. \
                     The session may have been started before binding was introduced.",
                    session_id
                )
            })?;
            let gateway_dir = crate::execution::gateway_root_dir(&self.config);
            repo.load_from_revision_dir(&gateway_dir, &binding.agent_id, &binding.revision_id)?
        } else {
            anyhow::bail!(
                "GatewayStore is required for checkpoint respawn. \
                 Agent '{}' cannot be respawned without a gateway store.",
                agent_id
            );
        };

        let inference = self.resolve_spawn_inference_profile(
            agent_id,
            &loaded.manifest,
            session_id,
        )?;
        let driver = build_driver(inference.llm_config.clone(), self.http_client.clone())?;
        // Propagate the resolved llm_config (with context_window_tokens and any
        // overrides) back into the manifest so the context governor can use it.
        loaded.manifest.llm_config = Some(inference.llm_config.clone());

        let openrouter_catalog = Arc::new(OpenRouterCatalog::new(self.http_client.clone()));
        let middleware = loaded.manifest.middleware.clone().unwrap_or_default();
        let mut runtime = self.attach_model_metadata(
            AgentExecutor::new(
            loaded.manifest,
            loaded.instructions,
            driver,
            loaded.dir,
            crate::runtime::tools::registry_for_config(Some(self.config.as_ref())),
            self.gateway_store.clone(),
        )
        .with_resolved_inference(inference)
        .with_gateway_dir(crate::execution::gateway_root_dir(&self.config))
        .with_config(self.config.clone())
        .with_session_budget(Some(self.session_budget.clone()))
        .with_root_session_budget(Some(self.root_session_budget.clone()))
        .with_middleware(middleware)
        .with_session_id(session_id.to_string())
        .with_workflow_context(workflow_id.map(String::from), task_id.map(String::from))
        .with_active_executions(Some(self.active_executions.clone()))
        .with_http_client(self.http_client.clone())
        .with_degraded_sessions(Some(self.degraded_sessions.clone()))
        .with_persona(self.persona.clone())
        .with_extended_instructions(loaded.extended_instructions.clone()),
            openrouter_catalog,
        );

        checkpoint.restore_into(&mut runtime);

        // Replay validation/tool feedback into the restored monitor so the
        // repair turn can be checked for ignored feedback.
        if !initial_feedback.is_empty() {
            let turn = runtime.turn_counter;
            runtime
                .trajectory_monitor
                .record_feedback(turn, initial_feedback);
        }

        // Build history from checkpoint, optionally appending an additional message
        let mut history = checkpoint.history.clone();
        if let Some(msg) = additional_message {
            history.push(Message::user(msg));
        }

        let outcome = execute_with_history_close_on_error(&mut runtime, &mut history).await?;

        let resolved_session_id = runtime
            .session_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("runtime session_id missing after execution"))?;

        let close_flags = session_close_flags_from_turn_outcome(outcome);

        let initial_msg = history
            .iter()
            .find(|m| matches!(m.role, crate::llm::Role::User))
            .map(|m| m.content.clone())
            .unwrap_or_default();

        persist_session_context_turn(
            &runtime.agent_dir,
            self.gateway_store.as_deref(),
            &resolved_session_id,
            &initial_msg,
            close_flags.assistant_reply.as_deref(),
            false,
        );
        self.finalize_session(
            &mut runtime,
            resolved_session_id,
            agent_id,
            source_agent_id,
            close_flags,
            false,
            Some(checkpoint.turn_id.clone()),
        ).await
    }

    /// Spawn a clarification child session of the agent that requested a
    /// pending approval, ask it the operator's question, capture its reply
    /// as a `gate_message` on the original approval, and return the reply.
    ///
    /// See `docs/archived/human-gate-unification-plan.md` §Phase 5 for the
    /// design rationale (gate-state via suspension stays untouched; this
    /// implements the orthogonal "gate-context" axis).
    pub async fn spawn_clarification_for_approval(
        &self,
        approval_id: &str,
        question: &str,
    ) -> anyhow::Result<ClarificationOutcome> {
        use crate::agent::AgentRepository;
        use crate::llm::{build_driver, Message};
        use crate::runtime::lifecycle::{AgentExecutor, TurnOutcome};
        use crate::runtime::openrouter_catalog::OpenRouterCatalog;
        use autonoetic_types::agent::SessionState;

        anyhow::ensure!(
            !question.trim().is_empty(),
            "Clarification question must not be empty"
        );

        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore is required for ask-agent"))?;

        let approval = store
            .get_approval(approval_id)?
            .ok_or_else(|| anyhow::anyhow!("Unknown approval '{}'", approval_id))?;

        anyhow::ensure!(
            !approval.session_id.trim().is_empty(),
            "Approval '{}' has no parent session_id",
            approval_id
        );
        let parent_session_id = approval.session_id.clone();
        let agent_id = approval.agent_id.clone();

        let short = uuid::Uuid::new_v4().to_string();
        let short = &short[..8];
        let child_session_id = format!("{}/{}-clarif-{}", parent_session_id, agent_id, short);

        let gateway_dir = crate::execution::gateway_root_dir(&self.config);
        let repo = AgentRepository::from_config(&self.config);
        let mut loaded = repo
            .get_sync_from_store(&agent_id, &gateway_dir, self.gateway_store.as_deref())
            .map_err(|e| anyhow::anyhow!("Failed to load agent '{}': {}", agent_id, e))?;

        let inference = self.resolve_spawn_inference_profile(
            &agent_id,
            &loaded.manifest,
            &child_session_id,
        )?;
        let driver = build_driver(inference.llm_config.clone(), self.http_client.clone())?;
        // Propagate the resolved llm_config (with context_window_tokens and any
        // overrides) back into the manifest so the context governor can use it.
        loaded.manifest.llm_config = Some(inference.llm_config.clone());

        let redacted_question = crate::log_redaction::redact_text_for_logs(question);
        store.add_gate_message(approval_id, "operator", &redacted_question)?;

        let action_json = serde_json::to_string_pretty(&approval.action)
            .unwrap_or_else(|_| "<action serialization failed>".to_string());
        let approval_reason = approval.reason.as_deref().unwrap_or("(none provided)");

        let message = format!(
            "You are answering a clarification question from an operator who is \
             reviewing a pending approval you requested. This is a read-only \
             clarification turn — your tools have been clamped to inspection \
             only and you cannot trigger any action from this session. The \
             approval itself remains pending on your parent session ({parent}); \
             your reply here does not approve or reject it.\n\n\
             Pending approval ({approval_id}):\n```json\n{action}\n```\n\n\
             Reason recorded on the request: {reason}\n\n\
             Operator question: {question}\n\n\
             Answer concisely and factually. If you cannot answer with the \
             information available to you in this clarification session, say \
             so plainly.",
            parent = parent_session_id,
            approval_id = approval_id,
            action = action_json,
            reason = approval_reason,
            question = question,
        );

        let openrouter_catalog = Arc::new(OpenRouterCatalog::new(self.http_client.clone()));
        let middleware = loaded.manifest.middleware.clone().unwrap_or_default();

        let mut runtime = self.attach_model_metadata(
            AgentExecutor::new(
            loaded.manifest,
            loaded.instructions,
            driver,
            loaded.dir,
            crate::runtime::tools::registry_for_config(Some(self.config.as_ref())),
            self.gateway_store.clone(),
        )
        .with_resolved_inference(inference)
        .with_gateway_dir(crate::execution::gateway_root_dir(&self.config))
        .with_config(self.config.clone())
        .with_session_budget(Some(self.session_budget.clone()))
        .with_root_session_budget(Some(self.root_session_budget.clone()))
        .with_middleware(middleware)
        .with_initial_user_message(message)
        .with_session_id(child_session_id.clone())
        .with_active_executions(Some(self.active_executions.clone()))
        .with_http_client(self.http_client.clone())
        .with_degraded_sessions(Some(self.degraded_sessions.clone()))
        .with_persona(self.persona.clone())
        .with_initial_session_state(SessionState::Clarification)
        .with_extended_instructions(loaded.extended_instructions.clone()),
            openrouter_catalog,
        );

        let mut history: Vec<Message> = Vec::new();
        let outcome = runtime.execute_with_history(&mut history).await;

        match outcome {
            Ok(TurnOutcome::Completed(reply)) => {
                let answer = reply.unwrap_or_else(|| {
                    "(agent produced no reply during the clarification turn)".to_string()
                });
                let sender = format!("agent-clarif:{}", child_session_id);
                let redacted_answer = crate::log_redaction::redact_text_for_logs(&answer);
                store.add_gate_message(approval_id, &sender, &redacted_answer)?;
                Ok(ClarificationOutcome {
                    child_session_id,
                    answer,
                })
            }
            Ok(other) => {
                let detail = format!("clarification turn ended unexpectedly: {:?}", other);
                store.add_gate_message(approval_id, "system", &detail)?;
                anyhow::bail!("{}", detail);
            }
            Err(e) => {
                let detail = format!("clarification turn failed: {}", e);
                store.add_gate_message(approval_id, "system", &detail)?;
                Err(e)
            }
        }
    }

    pub async fn spawn_clarification_for_escalation(
        &self,
        escalation_id: &str,
        role_agent_id: &str,
        question: &str,
    ) -> anyhow::Result<ClarificationOutcome> {
        use crate::agent::AgentRepository;
        use crate::llm::{build_driver, Message};
        use crate::runtime::lifecycle::{AgentExecutor, TurnOutcome};
        use crate::runtime::openrouter_catalog::OpenRouterCatalog;
        use autonoetic_types::agent::SessionState;

        anyhow::ensure!(
            !question.trim().is_empty(),
            "Clarification question must not be empty"
        );
        anyhow::ensure!(
            !role_agent_id.trim().is_empty(),
            "role_agent_id must not be empty"
        );

        let store = self
            .gateway_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GatewayStore is required for ask-role"))?;

        let escalation = store
            .get_escalation(escalation_id)?
            .ok_or_else(|| anyhow::anyhow!("Unknown escalation '{}'", escalation_id))?;

        anyhow::ensure!(
            !escalation.root_session_id.trim().is_empty(),
            "Escalation '{}' has no root_session_id",
            escalation_id
        );

        let verdict_summary = escalation
            .role_verdicts
            .iter()
            .find(|v| v.agent_id == role_agent_id)
            .ok_or_else(|| anyhow::anyhow!(
                "Role agent '{}' not found in escalation '{}'. Available: {}",
                role_agent_id,
                escalation_id,
                escalation.role_verdicts.iter().map(|v| v.agent_id.as_str()).collect::<Vec<_>>().join(", ")
            ))?;

        let parent_session_id = escalation.root_session_id.clone();
        let agent_id = role_agent_id.to_string();

        let short = uuid::Uuid::new_v4().to_string();
        let short = &short[..8];
        let child_session_id = format!("{}/{}-clarif-{}", parent_session_id, agent_id, short);

        let gateway_dir = crate::execution::gateway_root_dir(&self.config);
        let repo = AgentRepository::from_config(&self.config);
        let mut loaded = repo
            .get_sync_from_store(&agent_id, &gateway_dir, self.gateway_store.as_deref())
            .map_err(|e| anyhow::anyhow!("Failed to load agent '{}': {}", agent_id, e))?;

        let inference = self.resolve_spawn_inference_profile(
            &agent_id,
            &loaded.manifest,
            &child_session_id,
        )?;
        let driver = build_driver(inference.llm_config.clone(), self.http_client.clone())?;
        // Propagate the resolved llm_config (with context_window_tokens and any
        // overrides) back into the manifest so the context governor can use it.
        loaded.manifest.llm_config = Some(inference.llm_config.clone());

        let redacted_question = crate::log_redaction::redact_text_for_logs(question);
        store.add_gate_message(escalation_id, "operator", &redacted_question)?;

        let verdict_json = serde_json::to_string_pretty(&verdict_summary)
            .unwrap_or_else(|_| "<verdict serialization failed>".to_string());
        let synthesis = &escalation.planner_synthesis;

        let message = format!(
            "You are answering a clarification question from an operator who is \
             reviewing a federation escalation for promotion. This is a read-only \
             clarification turn — your tools have been clamped to inspection \
             only and you cannot trigger any action from this session.\n\n\
             Escalation ID: {escalation_id}\n\
             Agent being promoted: {agent}\n\
             Artifact: {artifact}\n\
             Planner synthesis: {synthesis}\n\n\
             Your recorded verdict:\n```json\n{verdict}\n```\n\n\
             Operator question: {question}\n\n\
             Answer concisely and factually. If you cannot answer with the \
             information available to you in this clarification session, say \
             so plainly.",
            escalation_id = escalation_id,
            agent = escalation.agent_id,
            artifact = escalation.artifact_id,
            synthesis = synthesis,
            verdict = verdict_json,
            question = question,
        );

        let openrouter_catalog = Arc::new(OpenRouterCatalog::new(self.http_client.clone()));
        let middleware = loaded.manifest.middleware.clone().unwrap_or_default();

        let mut runtime = self.attach_model_metadata(
            AgentExecutor::new(
            loaded.manifest,
            loaded.instructions,
            driver,
            loaded.dir,
            crate::runtime::tools::registry_for_config(Some(self.config.as_ref())),
            self.gateway_store.clone(),
        )
        .with_resolved_inference(inference)
        .with_gateway_dir(crate::execution::gateway_root_dir(&self.config))
        .with_config(self.config.clone())
        .with_session_budget(Some(self.session_budget.clone()))
        .with_root_session_budget(Some(self.root_session_budget.clone()))
        .with_middleware(middleware)
        .with_initial_user_message(message)
        .with_session_id(child_session_id.clone())
        .with_active_executions(Some(self.active_executions.clone()))
        .with_http_client(self.http_client.clone())
        .with_degraded_sessions(Some(self.degraded_sessions.clone()))
        .with_persona(self.persona.clone())
        .with_initial_session_state(SessionState::Clarification)
        .with_extended_instructions(loaded.extended_instructions.clone()),
            openrouter_catalog,
        );

        let mut history: Vec<Message> = Vec::new();
        let outcome = runtime.execute_with_history(&mut history).await;

        match outcome {
            Ok(TurnOutcome::Completed(reply)) => {
                let answer = reply.unwrap_or_else(|| {
                    "(agent produced no reply during the clarification turn)".to_string()
                });
                let sender = format!("agent-clarif:{}", child_session_id);
                let redacted_answer = crate::log_redaction::redact_text_for_logs(&answer);
                store.add_gate_message(escalation_id, &sender, &redacted_answer)?;
                Ok(ClarificationOutcome {
                    child_session_id,
                    answer,
                })
            }
            Ok(other) => {
                let detail = format!("clarification turn ended unexpectedly: {:?}", other);
                store.add_gate_message(escalation_id, "system", &detail)?;
                anyhow::bail!("{}", detail);
            }
            Err(e) => {
                let detail = format!("clarification turn failed: {}", e);
                store.add_gate_message(escalation_id, "system", &detail)?;
                Err(e)
            }
        }
    }

    pub async fn execute_background_action(
        &self,
        agent_id: &str,
        _session_id: &str,
        action: &ScheduledAction,
        agent_workspace_dir: &std::path::Path,
    ) -> anyhow::Result<String> {
        self.execute_with_reliability_controls(agent_id, || async move {
            let skill_path = agent_workspace_dir.join("SKILL.md");
            let skill_content = std::fs::read_to_string(&skill_path)?;
            let (manifest, _instructions) =
                crate::runtime::parser::SkillParser::parse(&skill_content)?;
            anyhow::ensure!(
                manifest.agent.id == agent_id,
                "background workspace '{}' declares agent.id '{}' but scheduler invoked agent_id '{}'",
                agent_workspace_dir.display(),
                manifest.agent.id,
                agent_id
            );
            execute_scheduled_action(
                &manifest,
                agent_workspace_dir,
                action,
                &crate::runtime::tools::registry_for_config(Some(self.config.as_ref())),
                Some(self.config.as_ref()),
                self.gateway_store.clone(),
            )
        })
        .await
    }

    pub fn load_agent_manifest(
        &self,
        agent_id: &str,
    ) -> anyhow::Result<(AgentManifest, std::path::PathBuf)> {
        let repo = AgentRepository::from_config(&self.config);
        let gateway_dir = crate::execution::gateway_root_dir(&self.config);
        let loaded =
            repo.get_sync_from_store(agent_id, &gateway_dir, self.gateway_store.as_deref())?;
        Ok((loaded.manifest, loaded.dir))
    }

    pub async fn execute_with_reliability_controls<F, Fut, T>(
        &self,
        agent_id: &str,
        operation: F,
    ) -> anyhow::Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        let rel_start = std::time::Instant::now();
        tracing::info!(
            target: "reliability",
            agent_id = %agent_id,
            "Acquiring agent admission semaphore"
        );
        let agent_admission = self.agent_admission_semaphore(agent_id).await;
        let _admission_permit = agent_admission.try_acquire_owned().map_err(|_| {
            anyhow::anyhow!(
                "Backpressure: pending execution queue is full for agent '{}'",
                agent_id
            )
        })?;
        tracing::info!(
            target: "reliability",
            agent_id = %agent_id,
            elapsed_ms = rel_start.elapsed().as_millis(),
            "Acquired agent admission semaphore"
        );

        let lock_start = std::time::Instant::now();
        tracing::info!(
            target: "reliability",
            agent_id = %agent_id,
            "Acquiring agent execution lock"
        );
        let agent_lock = self.agent_execution_lock(agent_id).await;
        let _agent_guard = agent_lock.lock().await;
        tracing::info!(
            target: "reliability",
            agent_id = %agent_id,
            elapsed_ms = lock_start.elapsed().as_millis(),
            "Acquired agent execution lock"
        );

        let _execution_permit = self
            .execution_semaphore
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                anyhow::anyhow!(
                    "Backpressure: max concurrent executions reached ({})",
                    self.config.max_concurrent_spawns.max(1)
                )
            })?;

        let op_start = std::time::Instant::now();
        let result = operation().await;
        tracing::info!(
            target: "reliability",
            agent_id = %agent_id,
            elapsed_ms = op_start.elapsed().as_millis(),
            success = result.is_ok(),
            "Reliability-controlled operation completed"
        );
        result
    }

    pub async fn agent_admission_semaphore(&self, agent_id: &str) -> Arc<Semaphore> {
        let mut guards = self.agent_admission.lock().await;
        guards
            .entry(agent_id.to_string())
            .or_insert_with(|| {
                Arc::new(Semaphore::new(
                    self.config.max_pending_spawns_per_agent.max(1),
                ))
            })
            .clone()
    }

    pub fn execution_semaphore(&self) -> Arc<Semaphore> {
        self.execution_semaphore.clone()
    }

    async fn agent_execution_lock(&self, agent_id: &str) -> Arc<Mutex<()>> {
        let mut guards = self.agent_execution_locks.lock().await;
        guards
            .entry(agent_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

fn fork_lineage_value(r: &crate::scheduler::gateway_store::ForkLineageRecord) -> serde_json::Value {
    serde_json::json!({
        "forked_session_id": r.forked_session_id,
        "source_session_id": r.source_session_id,
        "fork_turn": r.fork_turn,
        "branch_message_sha256": r.branch_message_sha256,
        "agent_id": r.agent_id,
        "created_at": r.created_at,
    })
}

/// Recursive descendant-collection for the fork tree, mirroring the CLI's
/// guards: max depth 16 + visited set so a cyclical lineage can't hang.
fn collect_fork_descendants_value(
    store: &Arc<crate::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
    depth: usize,
    visited: &mut std::collections::HashSet<String>,
) -> anyhow::Result<Vec<serde_json::Value>> {
    if depth >= 16 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for record in store.list_fork_children(session_id)? {
        if !visited.insert(record.forked_session_id.clone()) {
            continue;
        }
        let children = collect_fork_descendants_value(
            store,
            &record.forked_session_id,
            depth + 1,
            visited,
        )?;
        let mut v = fork_lineage_value(&record);
        v["children"] = serde_json::Value::Array(children);
        out.push(v);
    }
    Ok(out)
}

/// Process-lifetime cache for the resolved gateway node id (#586). Populated
/// once at startup via [`init_gateway_node_id`]; falls back to a lazy
/// `env::var` read on first access for code paths that don't go through the
/// server `run()` (e.g. in-process tests).
static GATEWAY_NODE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Cache the resolved node id for the lifetime of the process. Called once at
/// gateway startup after identity resolution. Idempotent — later calls are
/// no-ops so the first (authoritative) value wins.
pub fn init_gateway_node_id(node_id: &str) {
    let _ = GATEWAY_NODE_ID.set(node_id.to_string());
}

/// Resolved gateway node id. Cached for the process lifetime so hot-path
/// timeline/event builders avoid a per-event `std::env::var` syscall (#586).
pub fn gateway_actor_id() -> String {
    GATEWAY_NODE_ID
        .get_or_init(|| {
            std::env::var("AUTONOETIC_NODE_ID").unwrap_or_else(|_| "gateway".to_string())
        })
        .clone()
}

/// Record a checkpoint integrity violation (#606) against the given store:
/// emit a durable `background.checkpoint`/`checkpoint_tampered` causal event,
/// revoke the bound approval with reason `integrity_violation`, and surface an
/// operator-visible alert.
///
/// `approval_request_id` is `Some` for action-mismatch (checkpoint loaded, but
/// the bound action differs); `None` for HMAC tamper (checkpoint unreadable),
/// in which case the bound approval is located by session. Exposed as a free
/// function so the behaviour is unit-testable without an executor.
pub fn record_checkpoint_integrity_violation(
    store: &crate::scheduler::gateway_store::GatewayStore,
    session_id: &str,
    agent_id: &str,
    approval_request_id: Option<&str>,
    reason: &str,
) {
    // Resolve the bound approval request id (or locate it by session).
    let resolved_rid = approval_request_id
        .map(|s| s.to_string())
        .or_else(|| {
            store
                .find_latest_open_approval_for_session(session_id)
                .ok()
                .flatten()
        });

    // Revoke the approval. Force-cancel so an already-approved row (the
    // action-mismatch case) is also moved to a terminal state.
    if let Some(ref rid) = resolved_rid {
        match store.cancel_approval_for_integrity_violation(rid) {
            Ok(false) => tracing::warn!(
                target: "checkpoint",
                session_id = %session_id,
                approval_request_id = %rid,
                "integrity-bound approval was already terminal"
            ),
            Err(e) => tracing::warn!(
                target: "checkpoint",
                session_id = %session_id,
                approval_request_id = %rid,
                error = %e,
                "failed to cancel integrity-bound approval"
            ),
            _ => {}
        }
    }

    // Durable audit trail.
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        event_seq: chrono::Utc::now().timestamp_millis().max(0) as u64,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "background.checkpoint".to_string(),
        action: "checkpoint_tampered".to_string(),
        status: "integrity_violation".to_string(),
        enforced_rules: vec![],
        target: resolved_rid.clone(),
        payload: Some(
            serde_json::json!({
                "session_id": session_id,
                "approval_request_id": resolved_rid,
                "reason": reason,
            })
            .to_string(),
        ),
        payload_ref: None,
        evidence_ref: None,
        reason: Some(reason.to_string()),
    };
    if let Err(e) = store.create_causal_event(&event) {
        tracing::error!(
            target: "checkpoint",
            session_id = %session_id,
            approval_request_id = ?resolved_rid,
            error = %e,
            "failed to persist checkpoint_tampered causal event — audit trail may be incomplete"
        );
    }

    tracing::error!(
        target: "checkpoint",
        session_id = %session_id,
        approval_request_id = ?resolved_rid,
        reason = %reason,
        "CHECKPOINT INTEGRITY VIOLATION — bound approval revoked, resume aborted"
    );
}

/// The gateway's own directory: `config.runtime_dir`, verbatim.
///
/// This used to be `config.agents_dir.join(".gateway")`, and 52 sites re-derived
/// that expression inline instead of calling this function — which is how four
/// distinct `.parent()`-hop bugs accumulated (#1145 and the vault/SDK-bridge
/// hops). There is nothing left to derive: the gateway dir is a config field and
/// this function is the single place that names it.
pub fn gateway_root_dir(config: &GatewayConfig) -> std::path::PathBuf {
    config.runtime_dir.clone()
}

pub fn gateway_causal_path(config: &GatewayConfig) -> std::path::PathBuf {
    gateway_root_dir(config)
        .join("history")
        .join("causal_chain.jsonl")
}

/// Initialize a no-op gateway causal logger.
/// The gateway causal chain has been removed - all relevant events are now
/// captured in gateway.db tables (workflow_events, approvals, causal_events).
/// This function is kept for backward compatibility but returns a no-op logger.
pub fn init_gateway_causal_logger(_config: &GatewayConfig) -> anyhow::Result<CausalLogger> {
    // Return a no-op logger that writes to /dev/null
    CausalLogger::new(std::path::PathBuf::from("/dev/null"))
}

pub fn next_event_seq(counter: &mut u64) -> u64 {
    *counter += 1;
    *counter
}

/// Log a gateway causal event (no-op).
/// The gateway causal chain has been removed - all relevant events are now
/// captured in gateway.db tables (workflow_events, approvals, causal_events).
/// This function is kept for backward compatibility but does nothing.
pub fn log_gateway_causal_event(
    _logger: &CausalLogger,
    _actor_id: &str,
    _session_id: &str,
    _event_seq: u64,
    _action: &str,
    _status: EntryStatus,
    _payload: Option<serde_json::Value>,
) {
    // No-op: gateway causal chain events are now captured in gateway.db
}

pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}


fn inject_session_context_after_system_message(
    agent_dir: &std::path::Path,
    session_id: &str,
    history: &mut Vec<Message>,
) {
    let injected: Vec<Message> = match SessionContext::load(agent_dir, session_id).and_then(
        |context| {
            Ok(context
                .render_prompt()
                .map(Message::system)
                .into_iter()
                .collect::<Vec<_>>())
        },
    ) {
        Ok(messages) => messages,
        Err(error) => {
            tracing::warn!(
                error = %error,
                session_id,
                "Failed to load session context for checkpoint resume"
            );
            return;
        }
    };
    if injected.is_empty() {
        return;
    }
    // Match build_initial_history: context sits between the agent's system
    // instructions (first message) and the conversation transcript.
    let insert_pos = if history.first().is_some() { 1 } else { 0 };
    for (offset, msg) in injected.into_iter().enumerate() {
        history.insert(insert_pos + offset, msg);
    }
}

fn build_initial_history(
    agent_dir: &std::path::Path,
    instructions: &str,
    user_message: &str,
    session_id: &str,
    manifest: &autonoetic_types::agent::AgentManifest,
    turn_start_messages: &[Message],
) -> Vec<Message> {
    let mut history = vec![Message::system(
        crate::runtime::lifecycle::compose_system_instructions_with_metadata(
            instructions,
            manifest,
            manifest
                .io
                .as_ref()
                .and_then(|io| io.output_policy.as_ref()),
        ),
    )];
    match SessionContext::load(agent_dir, session_id).and_then(|context| {
        Ok(context
            .render_prompt()
            .map(Message::system)
            .into_iter()
            .collect::<Vec<_>>())
    }) {
        Ok(mut injected) => history.append(&mut injected),
        Err(error) => tracing::warn!(
            error = %error,
            session_id,
            "Failed to load session context; continuing without injected continuity"
        ),
    }
    history.extend(turn_start_messages.iter().cloned());
    history.push(Message::user(user_message.to_string()));
    history
}

fn is_signal_delivered_for_terminal_workflow(
    config: &autonoetic_types::config::GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
    metadata: Option<&serde_json::Value>,
) -> anyhow::Result<bool> {
    let is_signal_delivery = metadata
        .and_then(|value| value.get("signal_delivered"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !is_signal_delivery {
        return Ok(false);
    }
    let root = crate::runtime::content_store::root_session_id(session_id);
    let workflow_id = match store {
        Some(s) => s.resolve_workflow_id(&root)?,
        None => crate::scheduler::workflow_store::resolve_workflow_id_for_root_session(
            config, &root,
        )?,
    };
    let Some(workflow_id) = workflow_id else {
        return Ok(false);
    };
    crate::scheduler::workflow_store::is_workflow_terminal(config, store, &workflow_id)
}

/// Advance `phase` from a gateway signal payload and trace any fact it earns
/// (`docs/internals/prompt/burden-study.md` §7.5).
///
/// Paired with every `gateway_signal_turn_start_context` call. That function
/// renders the signal into turn-start *messages*, so the evidence inside it never
/// becomes a tool result and `SessionPhase::observe` can never see it. This is
/// the derive-at-source half: the gateway advancing the phase where it already
/// holds the child's typed state, rather than waiting for the agent to
/// incidentally call an artifact-domain tool afterwards.
fn observe_signal_phase(phase: &mut crate::runtime::guidance::SessionPhase, signal_json: &str) {
    for fact in phase.observe_gateway_signal(signal_json) {
        tracing::info!(
            target: "autonoetic::session_phase",
            source = "gateway_signal",
            fact = %fact,
            "Session phase advanced from a gateway signal; phase-gated guidance now active"
        );
    }
}

/// One-line outcome statement for a child-state notification (#1095).
///
/// Terminal statuses must lead with the fact: a model with a near-zero
/// reasoning budget repeats its prior stance ("still waiting") when success
/// has to be *inferred* from structured state — exactly what happened in the
/// session-53043b4c smoke (the wake turn answered "still running" about a
/// task that had succeeded one notification earlier). `stale` is
/// terminal-for-join (approval timeouts unblock joins, #722), so a stale
/// child counts as resolved for join wakes. Non-terminal statuses return
/// `None` and keep the neutral framing.
fn child_state_one_liner(notification: &serde_json::Value) -> Option<String> {
    let status = notification.get("child_status")?.as_str()?;
    let task_id = notification
        .get("task_id")
        .and_then(|v| v.as_str())
        .unwrap_or("child task");
    let summary = notification
        .get("summary")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match status {
        "succeeded" | "failed" | "cancelled" | "aborted" | "stale" => {
            let failure_class = notification.get("failure_class").and_then(|v| v.as_str());
            let mut line = match status {
                "failed" => format!(
                    "Child {} FAILED{}.",
                    task_id,
                    failure_class
                        .map(|class| format!(" (class: {})", class))
                        .unwrap_or_default()
                ),
                _ => format!("Child {} {}.", task_id, status.to_uppercase()),
            };
            if let Some(summary) = summary {
                line.push_str(&format!(
                    " Result: {}",
                    summary.chars().take(600).collect::<String>()
                ));
            }
            Some(line)
        }
        _ => None,
    }
}

fn gateway_signal_turn_start_context(
    user_message: &str,
    metadata: Option<&serde_json::Value>,
    config: Option<&autonoetic_types::config::GatewayConfig>,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
) -> (Vec<Message>, String) {
    let is_signal_delivery = metadata
        .and_then(|value| value.get("signal_delivered"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !is_signal_delivery {
        return (Vec::new(), user_message.to_string());
    }

    // Try to inject the workflow summary so the planner doesn't need a
    // separate `workflow_state` tool call on wake (saves one LLM round).
    let workflow_summary = config
        .and_then(|cfg| {
            crate::scheduler::compact_workflow_summary(cfg, store, session_id)
                .ok()
                .flatten()
        });

    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(user_message) else {
        return (Vec::new(), user_message.to_string());
    };

    // Build system messages from the signal payload.
    let mut system_messages = Vec::new();

    let signal_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if signal_type == "child_state_notification" {
        if let Some(notification_value) = parsed.get("notification") {
            let pretty = serde_json::to_string_pretty(notification_value)
                .unwrap_or_else(|_| notification_value.to_string());
            // #1095: terminal wakes lead with the outcome. The structured
            // state stays in the body for detail; the first line states the
            // fact so a low-reasoning-budget model cannot miss it.
            let mut content = format!("[gateway child state notification]\n{}", pretty);
            if let Some(lead) = child_state_one_liner(notification_value) {
                content = format!("{}\n\n{}", lead, content);
            }
            system_messages.push(Message::system(content));
        }
    } else if signal_type == "workflow_join_satisfied" {
        // Include the join payload as a system message so the planner sees
        // which tasks completed and any child summaries embedded in the signal.
        let pretty = serde_json::to_string_pretty(&parsed)
            .unwrap_or_else(|_| parsed.to_string());
        // #1095: multi-child wakes lead with the count and one-line outcomes
        // before the structured body.
        let mut content = format!("[workflow join satisfied]\n{}", pretty);
        if let Some(summaries) = parsed.get("child_summaries").and_then(|v| v.as_array()) {
            // The count reflects the actual join payload, not the number of
            // one-liners that could be rendered — entries that carry no
            // status must not shrink the resolved count (review #1099).
            let lines: Vec<String> = summaries.iter().filter_map(child_state_one_liner).collect();
            if !summaries.is_empty() {
                let mut lead = format!(
                    "All {} child task(s) have resolved:",
                    summaries.len()
                );
                for line in lines {
                    lead.push_str(&format!("\n- {}", line));
                }
                content = format!("{}\n\n{}", lead, content);
            }
        }
        system_messages.push(Message::system(content));
    }

    // Append the workflow status summary (same injection used at hibernate
    // time in lifecycle.rs) so the planner has the full picture without a
    // `workflow_state` round-trip.
    if let Some(ref summary) = workflow_summary {
        if let Some(last) = system_messages.last_mut() {
            last.content.push_str("\n\n[workflow status] ");
            last.content.push_str(summary);
        } else {
            system_messages.push(Message::system(format!(
                "[workflow status] {}",
                summary
            )));
        }
    }

    let user_prompt = if system_messages.is_empty() {
        user_message.to_string()
    } else {
        match signal_type {
            "child_state_notification" => {
                let terminal = parsed
                    .get("notification")
                    .and_then(|n| n.get("child_status"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| {
                        matches!(s, "succeeded" | "failed" | "cancelled" | "aborted")
                    });
                if terminal {
                    // #1095: the outcome is final — explicitly supersede any
                    // prior "I'll wait" stance from the previous turn so the
                    // model does not repeat it.
                    "The child outcome above is final — it supersedes any previous wait stance: do not wait again or re-check this child. Continue with your next action; if this was the last outstanding work, produce your final answer for the operator now. The workflow status above is current — you do not need to call workflow_state."
                        .to_string()
                } else {
                    "Gateway child-state notification delivered. The workflow status above is current — you do not need to call workflow_state. Continue from the current workflow state and use the structured gateway child state above.".to_string()
                }
            }
            "workflow_join_satisfied" => "Workflow join satisfied. The workflow status above is current — you do not need to call workflow_state. The completed-task results above are final — do not re-wait on them. Review the completed tasks and continue planning.".to_string(),
            _ => user_message.to_string(),
        }
    };

    if system_messages.is_empty() {
        (Vec::new(), user_message.to_string())
    } else {
        (system_messages, user_prompt)
    }
}

/// Extract durable facts about agents promoted in `root_session_id` (or its
/// child sessions) from the authoritative `promotion_attempts` SQLite table.
///
/// Unlike `result_summary` (free-form assistant text), this table is written
/// structurally on every `agent_revision_promote` outcome and carries the
/// target `alias_id` + `revision_id` — exactly the antecedent the planner
/// needs to resolve referents like "it" after an install.
fn extract_promotion_facts(
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    root_session_id: &str,
) -> Option<(Vec<crate::runtime::session_context::SessionFact>, Option<String>)> {
    let store = store?;
    let promoted = match store.list_promoted_agents_by_root_session(root_session_id) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                target: "execution",
                error = %error,
                root_session_id,
                "Failed to read promotion_attempts for session-context facts"
            );
            return None;
        }
    };
    if promoted.is_empty() {
        return None;
    }

    let mut facts = Vec::new();
    // Rows are ordered oldest-first; track the latest as the current topic.
    let mut latest_topic: Option<String> = None;
    for agent in &promoted {
        if agent.agent_id.is_empty() {
            continue;
        }
        facts.push(crate::runtime::session_context::SessionFact {
            label: "installed_agent".to_string(),
            value: agent.agent_id.clone(),
            source: "promotion".to_string(),
        });
        latest_topic = Some(format!("{} (installed)", agent.agent_id));
    }

    if facts.is_empty() {
        None
    } else {
        Some((facts, latest_topic))
    }
}

fn persist_session_context_turn(
    agent_dir: &std::path::Path,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
    user_message: &str,
    assistant_reply: Option<&str>,
    is_signal_delivered: bool,
) {
    let result = (|| -> anyhow::Result<()> {
        let mut context = SessionContext::load(agent_dir, session_id)?;
        context.record_turn(user_message, assistant_reply, is_signal_delivered);

        let root_session_id = crate::runtime::content_store::root_session_id(session_id);
        if session_id == root_session_id {
            if let Some((facts, topic)) = extract_promotion_facts(store, session_id) {
                for fact in facts {
                    context.add_fact(fact);
                }
                if let Some(topic) = topic {
                    context.set_current_topic(topic);
                }
            }
        }

        context.save(agent_dir)?;
        Ok(())
    })();
    if let Err(error) = result {
        tracing::warn!(
            error = %error,
            session_id,
            "Failed to persist session context after execution"
        );
    }
}

fn build_gateway_workflow_note(
    config: &GatewayConfig,
    session_id: &str,
    assistant_reply: Option<&str>,
) -> Option<String> {
    let summary = crate::scheduler::compact_workflow_summary(config, None, session_id)
        .ok()
        .flatten()?;
    let planner_empty = assistant_reply
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    Some(crate::runtime::context::workflow_status_user_message_for_chat(
        &summary,
        planner_empty,
    ))
}

fn count_spawned_children_for_source_session(
    _config: &GatewayConfig,
    _source_agent_id: &str,
    _session_id: &str,
) -> anyhow::Result<usize> {
    // Since gateway causal chain is no longer used, we need to query the gateway store
    // For now, return 0 as spawn events are tracked via SessionTracer dual-write
    // A more complete implementation would query the causal_events table
    Ok(0)
}

struct SchemaValidation {
    valid: bool,
    issues: Vec<String>,
}

/// Lightweight schema validation: checks required fields and basic type hints.
/// Logs results but does NOT hard-fail — the LLM can handle minor mismatches.
fn validate_against_schema(input: &str, schema: &serde_json::Value) -> SchemaValidation {
    let mut issues = Vec::new();

    // Try to parse input as JSON; if it's plain text, check if schema expects an object
    let input_value: serde_json::Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => {
            // Plain text input — if schema expects an object with required fields, note the mismatch
            if schema.get("type").and_then(|t| t.as_str()) == Some("object") {
                if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
                    if !required.is_empty() {
                        issues.push(format!(
                            "Input is plain text but schema expects object with required fields: {:?}",
                            required.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()
                        ));
                    }
                }
            }
            return SchemaValidation {
                valid: issues.is_empty(),
                issues,
            };
        }
    };

    // Check type
    if let Some(expected_type) = schema.get("type").and_then(|t| t.as_str()) {
        let actual_type = match &input_value {
            serde_json::Value::Object(_) => "object",
            serde_json::Value::Array(_) => "array",
            serde_json::Value::String(_) => "string",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Null => "null",
        };
        if actual_type != expected_type {
            issues.push(format!(
                "Type mismatch: expected '{}', got '{}'",
                expected_type, actual_type
            ));
        }
    }

    // Check required fields for objects
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        if let Some(obj) = input_value.as_object() {
            for field in required {
                if let Some(field_name) = field.as_str() {
                    if !obj.contains_key(field_name) {
                        issues.push(format!("Missing required field: '{}'", field_name));
                    }
                }
            }
        }
    }

    SchemaValidation {
        valid: issues.is_empty(),
        issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::session_context::session_context_path;

    #[test]
    fn test_build_initial_history_injects_session_context_before_user_message() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let mut context = SessionContext::empty("session-1");
        context.record_turn("remember Atlas", Some("Stored that."), false);
        context
            .save(temp.path())
            .expect("session context should save");

        let manifest = autonoetic_types::agent::AgentManifest {
            remote_access: None,
            version: "1.0".to_string(),
            runtime: autonoetic_types::agent::RuntimeDeclaration {
                mounts: Vec::new(),
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: autonoetic_types::agent::AgentIdentity {
                id: "test-agent".to_string(),
                name: "Test Agent".to_string(),
                description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
            capabilities: vec![],
            llm_overrides: None,
            llm_preset: None,
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
            excluded_tools: vec![],
            sections: Vec::new(),
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
            egress: None,
        };

        let history = build_initial_history(
            temp.path(),
            "System prompt",
            "What did I ask you to remember?",
            "session-1",
            &manifest,
            &[],
        );

        assert_eq!(history.len(), 3);
        assert_eq!(history[0].role.as_str(), "system");
        assert_eq!(history[2].role.as_str(), "user");
        assert!(history[0].content.contains("Foundation Core"));
        assert!(history[0].content.contains("System prompt"));
        assert!(history[1]
            .content
            .contains("Last user message: remember Atlas"));
        assert!(history[1]
            .content
            .contains("Last assistant reply: Stored that."));
    }

    #[test]
    fn child_state_signal_turn_start_context_is_injected_as_system_message() {
        let message = serde_json::json!({
            "type": "child_state_notification",
            "notification": {
                "workflow_id": "wf-1",
                "task_id": "task-1",
                "child_session_id": "root/task-1",
                "child_status": "failed",
                "failure_class": "policy_denied",
                "retry_advice": "do_not_retry",
                "side_effect_state": "none",
                "summary": "approval_rejected"
            }
        })
        .to_string();
        let metadata = serde_json::json!({
            "signal_delivered": true,
            "signal_request_id": "wf-child-test"
        });

        let (turn_start_messages, user_message) =
            gateway_signal_turn_start_context(&message, Some(&metadata), None, None, "test-session");

        assert_eq!(turn_start_messages.len(), 1);
        assert!(matches!(turn_start_messages[0].role, crate::llm::Role::System));
        // #1095: a terminal wake must lead with the outcome, not bury it in
        // the structured state.
        assert!(turn_start_messages[0]
            .content
            .starts_with("Child task-1 FAILED (class: policy_denied). Result: approval_rejected"));
        assert!(turn_start_messages[0]
            .content
            .contains("[gateway child state notification]"));
        assert!(turn_start_messages[0].content.contains("\"task_id\": \"task-1\""));
        assert!(user_message.contains("The child outcome above is final"));
        assert!(user_message.contains("do not wait again or re-check this child"));
        assert!(user_message.contains("produce your final answer for the operator now"));
    }

    #[test]
    fn child_state_success_wake_leads_with_outcome_and_result() {
        let message = serde_json::json!({
            "type": "child_state_notification",
            "notification": {
                "workflow_id": "wf-1",
                "task_id": "task-dd4518e8",
                "child_session_id": "root/task-dd4518e8",
                "child_status": "succeeded",
                "summary": "{\"ok\": true, \"symbol\": \"AAPL\", \"rows\": 5}"
            }
        })
        .to_string();
        let metadata = serde_json::json!({
            "signal_delivered": true,
            "signal_request_id": "wf-child-test"
        });

        let (turn_start_messages, user_message) =
            gateway_signal_turn_start_context(&message, Some(&metadata), None, None, "test-session");

        assert_eq!(turn_start_messages.len(), 1);
        assert!(turn_start_messages[0]
            .content
            .starts_with("Child task-dd4518e8 SUCCEEDED. Result: {\"ok\": true, \"symbol\": \"AAPL\", \"rows\": 5}"));
        assert!(turn_start_messages[0]
            .content
            .contains("[gateway child state notification]"));
        // The structured state is still present for detail.
        assert!(turn_start_messages[0]
            .content
            .contains("\"child_status\": \"succeeded\""));
        assert!(user_message.contains("The child outcome above is final"));
        assert!(user_message.contains("it supersedes any previous wait stance"));
    }

    #[test]
    fn child_state_non_terminal_wake_keeps_neutral_framing() {
        let message = serde_json::json!({
            "type": "child_state_notification",
            "notification": {
                "workflow_id": "wf-1",
                "task_id": "task-1",
                "child_session_id": "root/task-1",
                "child_status": "awaiting_approval"
            }
        })
        .to_string();
        let metadata = serde_json::json!({
            "signal_delivered": true,
            "signal_request_id": "wf-child-test"
        });

        let (turn_start_messages, user_message) =
            gateway_signal_turn_start_context(&message, Some(&metadata), None, None, "test-session");

        assert_eq!(turn_start_messages.len(), 1);
        assert!(!turn_start_messages[0].content.starts_with("Child task-1"));
        assert!(turn_start_messages[0]
            .content
            .starts_with("[gateway child state notification]"));
        assert_eq!(
            user_message,
            "Gateway child-state notification delivered. The workflow status above is current — you do not need to call workflow_state. Continue from the current workflow state and use the structured gateway child state above."
        );
    }

    #[test]
    fn workflow_join_wake_leads_with_count_and_per_child_outcomes() {
        let message = serde_json::json!({
            "type": "workflow_join_satisfied",
            "workflow_id": "wf-1",
            "join_task_ids": ["task-a", "task-b"],
            "child_summaries": [
                {
                    "workflow_id": "wf-1",
                    "task_id": "task-a",
                    "child_session_id": "root/task-a",
                    "child_status": "succeeded",
                    "summary": "{\"ok\": true, \"rows\": 3}"
                },
                {
                    "workflow_id": "wf-1",
                    "task_id": "task-b",
                    "child_session_id": "root/task-b",
                    "child_status": "failed",
                    "failure_class": "policy_denied",
                    "summary": "approval_rejected"
                }
            ]
        })
        .to_string();
        let metadata = serde_json::json!({
            "signal_delivered": true,
            "signal_request_id": "wf-join-test"
        });

        let (turn_start_messages, user_message) =
            gateway_signal_turn_start_context(&message, Some(&metadata), None, None, "test-session");

        assert_eq!(turn_start_messages.len(), 1);
        assert!(turn_start_messages[0]
            .content
            .starts_with("All 2 child task(s) have resolved:"));
        assert!(turn_start_messages[0]
            .content
            .contains("\n- Child task-a SUCCEEDED. Result: {\"ok\": true, \"rows\": 3}"));
        assert!(turn_start_messages[0]
            .content
            .contains("\n- Child task-b FAILED (class: policy_denied). Result: approval_rejected"));
        assert!(turn_start_messages[0]
            .content
            .contains("[workflow join satisfied]"));
        assert!(user_message.contains("The completed-task results above are final — do not re-wait on them"));
        assert!(user_message.contains("Review the completed tasks and continue planning"));
    }

    #[test]
    fn workflow_join_wake_count_reflects_payload_not_rendered_lines() {
        // A stale child is terminal-for-join and must be counted; an entry
        // without a status must not shrink the resolved count (review #1099).
        let message = serde_json::json!({
            "type": "workflow_join_satisfied",
            "workflow_id": "wf-1",
            "join_task_ids": ["task-a", "task-b", "task-c"],
            "child_summaries": [
                {
                    "workflow_id": "wf-1",
                    "task_id": "task-a",
                    "child_session_id": "root/task-a",
                    "child_status": "stale"
                },
                {
                    "workflow_id": "wf-1",
                    "task_id": "task-b",
                    "child_session_id": "root/task-b",
                    "child_status": "succeeded",
                    "summary": "rows: 3"
                },
                {
                    "workflow_id": "wf-1",
                    "task_id": "task-c",
                    "child_session_id": "root/task-c"
                }
            ]
        })
        .to_string();
        let metadata = serde_json::json!({
            "signal_delivered": true,
            "signal_request_id": "wf-join-test"
        });

        let (turn_start_messages, _user_message) =
            gateway_signal_turn_start_context(&message, Some(&metadata), None, None, "test-session");

        assert_eq!(turn_start_messages.len(), 1);
        assert!(turn_start_messages[0]
            .content
            .starts_with("All 3 child task(s) have resolved:"));
        assert!(turn_start_messages[0]
            .content
            .contains("\n- Child task-a STALE."));
        assert!(turn_start_messages[0]
            .content
            .contains("\n- Child task-b SUCCEEDED. Result: rows: 3"));
        // task-c has no status: counted in the header, no bullet.
        assert!(!turn_start_messages[0].content.contains("task-c SUCCEEDED"));
        assert!(!turn_start_messages[0].content.contains("task-c FAILED"));
    }

    #[test]
    fn test_persist_session_context_turn_writes_current_exchange() {
        let temp = tempfile::tempdir().expect("tempdir should create");

        persist_session_context_turn(
            temp.path(),
            None,
            "session-2",
            "hello there",
            Some("general kenobi"),
            false,
        );

        let path = session_context_path(temp.path(), "session-2");
        let body = std::fs::read_to_string(path).expect("session context file should exist");
        assert!(body.contains("\"last_user_message\": \"hello there\""));
        assert!(body.contains("\"last_assistant_reply\": \"general kenobi\""));
    }

    #[test]
    fn test_persist_session_context_turn_skips_signal_user_message() {
        let temp = tempfile::tempdir().expect("tempdir should create");

        persist_session_context_turn(
            temp.path(),
            None,
            "session-signal",
            "real user message",
            Some("reply one"),
            false,
        );
        persist_session_context_turn(
            temp.path(),
            None,
            "session-signal",
            "gateway signal payload",
            Some("reply two"),
            true,
        );

        let path = session_context_path(temp.path(), "session-signal");
        let body = std::fs::read_to_string(path).expect("session context file should exist");
        assert!(body.contains("\"last_user_message\": \"real user message\""));
        assert!(body.contains("\"last_assistant_reply\": \"reply two\""));
    }

    #[test]
    fn test_inject_session_context_into_checkpoint_history_after_system_message() {
        use crate::runtime::session_context::{SessionContext, SessionFact};

        let temp = tempfile::tempdir().expect("tempdir should create");
        let mut context = SessionContext::empty("session-ctx-inject");
        context.set_current_topic("fibonacci-next (installed)".to_string());
        context.add_fact(SessionFact {
            label: "installed_agent".to_string(),
            value: "fibonacci-next".to_string(),
            source: "promotion".to_string(),
        });
        context.save(temp.path()).expect("context should save");

        let mut history = vec![
            Message::system("You are a helpful planner.".to_string()),
            Message::user("call it 5 times".to_string()),
        ];
        inject_session_context_after_system_message(
            temp.path(),
            "session-ctx-inject",
            &mut history,
        );

        assert_eq!(history.len(), 3);
        assert!(matches!(history[0].role, crate::llm::Role::System));
        assert!(history[0].content.contains("You are a helpful planner"));
        assert!(matches!(history[1].role, crate::llm::Role::System));
        assert!(history[1].content.contains("Current focus: fibonacci-next (installed)"));
        assert!(history[1].content.contains("installed_agent: fibonacci-next"));
        assert!(matches!(history[2].role, crate::llm::Role::User));
        assert_eq!(history[2].content, "call it 5 times");
    }

    #[test]
    fn test_extract_promotion_facts_loads_from_store_for_child_session() {
        // The promoting session is a child path under the root session
        // (e.g. "root/specialized_builder.default-xxx"), exactly as written
        // by agent_revision_promote in production. The prefix match must
        // surface it when querying by the bare root session id.
        let dir = tempfile::tempdir().expect("tempdir should create");
        let store = crate::scheduler::gateway_store::GatewayStore::open(dir.path())
            .expect("store should open");
        let root = "session-12d6d198";
        store
            .record_promotion_attempt(
                "patt-1",
                "fibonacci-next",
                "rev-abc",
                "sha256:deadbeef",
                "promoted",
                None,
                None,
                Some(&format!("{root}/specialized_builder.default-zzz")),
                Some("wf-1"),
            )
            .unwrap();

        let (facts, topic) = extract_promotion_facts(Some(&store), root)
            .expect("promotion facts should be present");

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].label, "installed_agent");
        assert_eq!(facts[0].value, "fibonacci-next");
        assert_eq!(facts[0].source, "promotion");
        assert_eq!(topic.as_deref(), Some("fibonacci-next (installed)"));
    }

    #[test]
    fn test_extract_promotion_facts_ignores_rejected_attempts() {
        let dir = tempfile::tempdir().expect("tempdir should create");
        let store = crate::scheduler::gateway_store::GatewayStore::open(dir.path())
            .expect("store should open");
        let root = "session-rejected";
        store
            .record_promotion_attempt(
                "patt-rej",
                "fibonacci-next",
                "rev-abc",
                "sha256:deadbeef",
                "rejected",
                Some("promotion_gate"),
                Some("validation_failed"),
                Some(root),
                Some("wf-1"),
            )
            .unwrap();

        assert!(extract_promotion_facts(Some(&store), root).is_none());
    }

    #[test]
    fn test_extract_promotion_facts_returns_none_without_store() {
        // No gateway store available (e.g. minimal test config): gracefully
        // skip fact injection rather than panic.
        assert!(extract_promotion_facts(None, "session-any").is_none());
    }

    #[test]
    fn test_extract_promotion_facts_tracks_latest_topic() {
        // Multiple promotions in the same root session: the most recent one
        // (by created_at) becomes the current topic, while all are recorded
        // as durable facts.
        let dir = tempfile::tempdir().expect("tempdir should create");
        let store = crate::scheduler::gateway_store::GatewayStore::open(dir.path())
            .expect("store should open");
        let root = "session-multi";
        store
            .record_promotion_attempt(
                "patt-old",
                "agent-old",
                "rev-1",
                "sha256:a",
                "promoted",
                None,
                None,
                Some(root),
                Some("wf-1"),
            )
            .unwrap();
        // Sleep briefly so the second attempt has a strictly-later created_at.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        store
            .record_promotion_attempt(
                "patt-new",
                "agent-new",
                "rev-2",
                "sha256:b",
                "promoted",
                None,
                None,
                Some(&format!("{root}/builder-1")),
                Some("wf-1"),
            )
            .unwrap();

        let (facts, topic) = extract_promotion_facts(Some(&store), root)
            .expect("promotion facts should be present");
        assert_eq!(facts.len(), 2);
        // Topic reflects the latest promotion.
        assert_eq!(topic.as_deref(), Some("agent-new (installed)"));
    }

    #[test]
    fn test_validate_valid_json_input() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string" }
            }
        });
        let input = r#"{"query": "test search"}"#;
        let result = validate_against_schema(input, &schema);
        assert!(
            result.valid,
            "Expected valid, got issues: {:?}",
            result.issues
        );
    }

    #[test]
    fn test_validate_missing_required_field() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["query", "domain"],
            "properties": {
                "query": { "type": "string" },
                "domain": { "type": "string" }
            }
        });
        let input = r#"{"query": "test"}"#;
        let result = validate_against_schema(input, &schema);
        assert!(!result.valid);
        assert!(result.issues.iter().any(|i| i.contains("domain")));
    }

    #[test]
    fn test_validate_type_mismatch() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["count"],
            "properties": {
                "count": { "type": "number" }
            }
        });
        let input = r#"["not", "an", "object"]"#;
        let result = validate_against_schema(input, &schema);
        assert!(!result.valid);
        assert!(result.issues.iter().any(|i| i.contains("Type mismatch")));
    }

    #[test]
    fn test_validate_plain_text_input() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string" }
            }
        });
        let input = "just a plain text query";
        let result = validate_against_schema(input, &schema);
        assert!(!result.valid);
        assert!(result.issues.iter().any(|i| i.contains("plain text")));
    }

    #[test]
    fn session_close_flags_jsonrpc_mapping_is_closed_and_stable() {
        let flags = |assistant_reply, suspended_for_approval, suspended_for_user_input, suspended_for_child_wait| {
            SessionCloseFlags {
                assistant_reply,
                suspended_for_approval,
                suspended_for_user_input,
                suspended_for_child_wait,
            }
        };
        assert_eq!(
            flags(None, Some("apr-1".into()), false, false).outcome(true).as_str(),
            "jsonrpc_spawn_suspended_approval"
        );
        assert_eq!(
            flags(None, None, true, false).outcome(true).as_str(),
            "jsonrpc_spawn_suspended_user_input"
        );
        assert_eq!(
            flags(None, None, false, true).outcome(true).as_str(),
            "jsonrpc_spawn_suspended_approval"
        );
        assert_eq!(
            flags(Some("ok".into()), None, false, false).outcome(true).as_str(),
            "jsonrpc_spawn_complete"
        );
        assert_eq!(
            flags(None, None, false, false).outcome(true).as_str(),
            "jsonrpc_spawn_complete_empty"
        );
    }

    #[test]
    fn session_close_flags_checkpoint_mapping_is_closed_and_stable() {
        let flags = |assistant_reply, suspended_for_approval, suspended_for_user_input, suspended_for_child_wait| {
            SessionCloseFlags {
                assistant_reply,
                suspended_for_approval,
                suspended_for_user_input,
                suspended_for_child_wait,
            }
        };
        assert_eq!(
            flags(None, Some("apr-1".into()), false, false).outcome(false).as_str(),
            "checkpoint_respawn_suspended"
        );
        assert_eq!(
            flags(None, None, true, false).outcome(false).as_str(),
            "checkpoint_respawn_suspended_user_input"
        );
        assert_eq!(
            flags(None, None, false, true).outcome(false).as_str(),
            "checkpoint_respawn_suspended"
        );
        assert_eq!(
            flags(Some("ok".into()), None, false, false).outcome(false).as_str(),
            "checkpoint_respawn_complete"
        );
        assert_eq!(
            flags(None, None, false, false).outcome(false).as_str(),
            "checkpoint_respawn_complete_empty"
        );
    }

    #[test]
    fn inject_approval_ref_into_history_adds_ref_to_last_tool_call() {
        use crate::llm::ToolCall;

        let mut history = vec![
            Message::user("Set up credentials"),
            Message {
                id: None,
                role: crate::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "credential_setup".to_string(),
                    arguments: r#"{"skill_url":"http://localhost:8080/skill.md"}"#.to_string(),
                }],
                tool_call_id: None,
                reasoning_content: None,
                reasoning_details: None,
            },
        ];

        inject_approval_ref_into_history(&mut history, "apr-abc123", None);

        // The assistant message's tool call should now have approval_ref.
        let assistant_msg = &history[1];
        assert_eq!(assistant_msg.role, crate::llm::Role::Assistant);
        let tc = &assistant_msg.tool_calls[0];
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap();
        assert_eq!(args["approval_ref"], "apr-abc123");
        assert_eq!(args["skill_url"], "http://localhost:8080/skill.md");

        // A user message should be appended with the approval notice.
        let user_msg = history.last().unwrap();
        assert_eq!(user_msg.role, crate::llm::Role::User);
        assert!(user_msg.content.contains("apr-abc123"));
    }

    #[test]
    fn inject_approval_ref_preserves_existing_fields() {
        use crate::llm::ToolCall;

        let mut history = vec![
            Message {
                id: None,
                role: crate::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_2".to_string(),
                    name: "sandbox.exec".to_string(),
                    arguments: r#"{"command":"curl http://api.example.com","approval_ref":"apr-old"}"#.to_string(),
                }],
                tool_call_id: None,
                reasoning_content: None,
                reasoning_details: None,
            },
        ];

        inject_approval_ref_into_history(&mut history, "apr-new", None);

        let tc = &history[0].tool_calls[0];
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap();
        // Should be overwritten with the new ref.
        assert_eq!(args["approval_ref"], "apr-new");
        assert_eq!(args["command"], "curl http://api.example.com");
    }

    // ---------------------------------------------------------------------
    // #847 — close_outcome_for_error: recoverable yields must NOT cascade
    // to workflow failure / grant deletion / task failure.
    // ---------------------------------------------------------------------

    use crate::runtime::checkpoint::{save_checkpoint, SessionCheckpoint, YieldReason};
    use crate::runtime::guard::LoopGuard;
    use autonoetic_types::session_outcome::SessionCloseOutcome;
    use std::sync::Arc as StdArc;

    fn test_agent_executor(
        temp: &tempfile::TempDir,
        config: &autonoetic_types::config::GatewayConfig,
        session_id: &str,
    ) -> AgentExecutor {
        let manifest = autonoetic_types::agent::AgentManifest {
            remote_access: None,
            version: "1.0".to_string(),
            runtime: autonoetic_types::agent::RuntimeDeclaration {
                mounts: Vec::new(),
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: autonoetic_types::agent::AgentIdentity {
                id: "test-agent".to_string(),
                name: "test-agent".to_string(),
                description: "test".to_string(),
                singleton: false,
                resident_idle_ttl_secs: None,
            },
            capabilities: vec![],
            llm_overrides: None,
            llm_preset: None,
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
            excluded_tools: vec![],
            sections: Vec::new(),
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
            egress: None,
        };
        // A driver that never gets used (we don't call execute_with_history).
        struct NoopDriver;
        #[async_trait::async_trait]
        impl crate::llm::LlmDriver for NoopDriver {
            async fn complete(
                &self,
                _req: &crate::llm::CompletionRequest,
            ) -> anyhow::Result<crate::llm::CompletionResponse> {
                anyhow::bail!("noop driver should never be called in these tests")
            }
        }
        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            StdArc::new(NoopDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );
        runtime.config = Some(StdArc::new(config.clone()));
        runtime.session_id = Some(session_id.to_string());
        runtime
    }

    fn save_test_checkpoint(
        config: &autonoetic_types::config::GatewayConfig,
        session_id: &str,
        yield_reason: YieldReason,
    ) {
        let checkpoint = SessionCheckpoint {
            egress_labels: Default::default(),
            egress_ask: None,
            history: vec![],
            turn_counter: 1,
            loop_guard_state: LoopGuard::default(),
            session_state: autonoetic_types::agent::SessionState::Normal,
            tool_tier_escalated: false,
            session_phase: Default::default(),
            discovered_tools: Default::default(),
            blocked_state_event_emitted: false,
            extended_loaded: false,
            agent_id: "test-agent".to_string(),
            session_id: session_id.to_string(),
            turn_id: "turn-001".to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 0,
            tool_invocations_consumed: 0,
            tokens_consumed: 0,
            estimated_cost_usd: 0.0,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };
        save_checkpoint(config, &checkpoint).expect("checkpoint should save");
    }

    fn test_config(temp: &tempfile::TempDir) -> autonoetic_types::config::GatewayConfig {
        autonoetic_types::config::GatewayConfig {
            agents_dir: temp.path().join("agents"),
            runtime_dir: temp.path().join("runtime"),
            ..autonoetic_types::config::GatewayConfig::default()
        }
    }

    /// Recoverable yields must close as suspended (not SpawnExecuteError), so
    /// close_session skips the grant-deletion, task-failure, and
    /// workflow-failure cascades.
    ///
    /// Note: `MaxTurnsReached` is intentionally excluded. It is NOT in
    /// `should_auto_resume_checkpoint_yield_reason`'s auto-resume list —
    /// auto-resuming a max-turns-reached session would trip the guard again
    /// immediately. The proper fix for MaxTurnsReached is an
    /// ApprovalRequired-style gate suspension (tracked separately).
    #[test]
    fn close_outcome_for_error_recoverable_yields_close_as_suspended() {
        for reason in [
            YieldReason::BudgetExhausted,
            YieldReason::ManualStop,
            YieldReason::Error("transient LLM error".to_string()),
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let config = test_config(&temp);
            let session_id = format!("session-recoverable-{:?}", reason);
            save_test_checkpoint(&config, &session_id, reason.clone());
            let runtime = test_agent_executor(&temp, &config, &session_id);
            let outcome = close_outcome_for_error(&runtime);
            assert_eq!(
                outcome,
                SessionCloseOutcome::ExecuteLoopSuspended,
                "recoverable yield {:?} must close as suspended (not SpawnExecuteError)",
                reason
            );
        }
    }

    /// Non-recoverable yield reasons must keep the pre-#847 SpawnExecuteError
    /// behavior: EmergencyStop is an intentional operator circuit breaker,
    /// ParentTerminated is an intentional cascade, and MaxTurnsReached is
    /// outside the auto-resume convention (needs operator intervention, not
    /// silent resume — tracked as a separate follow-up).
    #[test]
    fn close_outcome_for_error_non_recoverable_yields_close_as_error() {
        for reason in [
            YieldReason::EmergencyStop {
                stop_id: "stop-1".to_string(),
            },
            YieldReason::ParentTerminated {
                parent_session_id: "parent-1".to_string(),
                reason: "operator".to_string(),
            },
            YieldReason::MaxTurnsReached,
        ] {
            let temp = tempfile::tempdir().expect("tempdir");
            let config = test_config(&temp);
            let session_id = format!("session-nonrecoverable-{:?}", reason);
            save_test_checkpoint(&config, &session_id, reason.clone());
            let runtime = test_agent_executor(&temp, &config, &session_id);
            let outcome = close_outcome_for_error(&runtime);
            assert_eq!(
                outcome,
                SessionCloseOutcome::SpawnExecuteError,
                "non-recoverable yield {:?} must keep SpawnExecuteError",
                reason
            );
        }
    }

    /// No checkpoint means a true spawn-time failure — keep SpawnExecuteError.
    #[test]
    fn close_outcome_for_error_no_checkpoint_closes_as_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = test_config(&temp);
        let runtime = test_agent_executor(&temp, &config, "session-no-checkpoint");
        let outcome = close_outcome_for_error(&runtime);
        assert_eq!(
            outcome,
            SessionCloseOutcome::SpawnExecuteError,
            "no checkpoint must keep SpawnExecuteError (true spawn-time failure)"
        );
    }

    /// Suspended outcome must skip all three failure cascades in
    /// `close_session` (grants deletion, task failure, workflow failure).
    /// This is a property test on `SessionCloseOutcome`'s discriminators.
    #[test]
    fn suspended_outcome_skips_failure_cascades() {
        let suspended = SessionCloseOutcome::ExecuteLoopSuspended;
        assert!(suspended.is_suspended());
        assert!(!suspended.is_error());
        assert!(!suspended.is_completed());

        // The discriminators that drive the cascades in close_session:
        // - line 763: `if !outcome.is_suspended()` → skips delete_session_grants
        // - line 781: `if outcome.is_error()` → skips fail_running_tasks_for_session
        // - line 835: `else if outcome.is_error()` → skips fail_workflow_for_root_session
        let spawn_err = SessionCloseOutcome::SpawnExecuteError;
        assert!(!spawn_err.is_suspended());
        assert!(spawn_err.is_error());
        assert!(!spawn_err.is_completed());
    }
}
