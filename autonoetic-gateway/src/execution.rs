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
    should_auto_resume_checkpoint_yield_reason,
};
use crate::runtime::session_report::SessionReportWriter;
use crate::scheduler::gateway_store::default_gateway_host_id;
use autonoetic_types::agent::{AgentManifest, ExecutionMode, LlmExchangeUsage};
use autonoetic_types::background::{ScheduledAction, UserInteractionStatus};
use autonoetic_types::causal_chain::EntryStatus;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::tool_error::tagged;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

/// Inject `approval_ref` into the last tool_call's arguments in the history,
/// then append a user message confirming the approval.  This fixes the
/// LLM-dependent relay bug where the model ignores the text-only hint and
/// retries without `approval_ref`, causing redundant approval requests.
fn inject_approval_ref_into_history(history: &mut Vec<Message>, approval_ref: &str) {
    // Walk history backwards to find the last assistant message with tool_calls.
    for msg in history.iter_mut().rev() {
        if matches!(msg.role, crate::llm::Role::Assistant) && !msg.tool_calls.is_empty() {
            // Inject approval_ref into the last tool call's arguments.
            if let Some(tc) = msg.tool_calls.last_mut() {
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
pub(crate) async fn execute_with_history_close_on_error(
    runtime: &mut AgentExecutor,
    history: &mut Vec<Message>,
) -> anyhow::Result<TurnOutcome> {
    match runtime.execute_with_history(history).await {
        Ok(o) => Ok(o),
        Err(e) => {
            let _ = runtime.close_session(SessionCloseReason::SpawnExecuteError.as_str());
            Err(e)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionCloseReason {
    SpawnExecuteError,
    JsonRpcSpawnSuspendedApproval,
    JsonRpcSpawnSuspendedUserInput,
    JsonRpcSpawnComplete,
    JsonRpcSpawnCompleteEmpty,
    CheckpointRespawnSuspendedApproval,
    CheckpointRespawnSuspendedUserInput,
    CheckpointRespawnComplete,
    CheckpointRespawnCompleteEmpty,
}

impl SessionCloseReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SpawnExecuteError => "spawn_execute_error",
            Self::JsonRpcSpawnSuspendedApproval => "jsonrpc_spawn_suspended_approval",
            Self::JsonRpcSpawnSuspendedUserInput => "jsonrpc_spawn_suspended_user_input",
            Self::JsonRpcSpawnComplete => "jsonrpc_spawn_complete",
            Self::JsonRpcSpawnCompleteEmpty => "jsonrpc_spawn_complete_empty",
            Self::CheckpointRespawnSuspendedApproval => "checkpoint_respawn_suspended",
            Self::CheckpointRespawnSuspendedUserInput => {
                "checkpoint_respawn_suspended_user_input"
            }
            Self::CheckpointRespawnComplete => "checkpoint_respawn_complete",
            Self::CheckpointRespawnCompleteEmpty => "checkpoint_respawn_complete_empty",
        }
    }

    fn for_jsonrpc_spawn(
        suspended_for_approval: bool,
        suspended_for_user_input: bool,
        has_assistant_reply: bool,
    ) -> Self {
        if suspended_for_approval {
            Self::JsonRpcSpawnSuspendedApproval
        } else if suspended_for_user_input {
            Self::JsonRpcSpawnSuspendedUserInput
        } else if has_assistant_reply {
            Self::JsonRpcSpawnComplete
        } else {
            Self::JsonRpcSpawnCompleteEmpty
        }
    }

    fn for_checkpoint_respawn(
        suspended_for_approval: bool,
        suspended_for_user_input: bool,
        has_assistant_reply: bool,
    ) -> Self {
        if suspended_for_approval {
            Self::CheckpointRespawnSuspendedApproval
        } else if suspended_for_user_input {
            Self::CheckpointRespawnSuspendedUserInput
        } else if has_assistant_reply {
            Self::CheckpointRespawnComplete
        } else {
            Self::CheckpointRespawnCompleteEmpty
        }
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
        let message_id = format!("msg-{}", &uuid::Uuid::new_v4().to_string()[..8]);
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
        use crate::runtime::guard::LoopGuardState;
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

        let stop_id = format!("estop-{}", &uuid::Uuid::new_v4().to_string()[..8]);
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
                history: vec![],
                turn_counter: 0,
                loop_guard_state: LoopGuardState {
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
                discovered_tools: Default::default(),
                agent_id: lead.to_string(),
                session_id: root_session_id.to_string(),
                turn_id: format!("emergency-{stop_id}"),
                workflow_id: workflow_id.clone(),
                task_id: None,
                runtime_lock_hash: None,
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
                            continuation: None,
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
                    tracing::info!(
                        target: "checkpoint",
                        agent_id = %runtime.manifest.agent.id,
                        session_id = %session_id,
                        turn_counter = checkpoint.turn_counter,
                        approval_request_id = %rid,
                        "Resuming session from approval-required checkpoint"
                    );
                    let gateway_dir = self.config.agents_dir.join(".gateway");
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
                    inject_approval_ref_into_history(&mut history, rid);
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
                    "GatewayStore is required to resume user.ask checkpoints"
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
                        "Resuming session from user.ask checkpoint with stored answer"
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
                            continuation: None,
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
                    let gateway_dir = self.config.agents_dir.join(".gateway");
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
        } else if should_auto_resume_checkpoint_yield_reason(&checkpoint.yield_reason) {
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
            let (turn_start_messages, resume_message) =
                gateway_signal_turn_start_context(message, metadata);
            history.extend(turn_start_messages);
            history.push(crate::llm::Message::user(resume_message));
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
        let span = tracing::info_span!(
            "spawn_agent_once",
            agent_id = agent_id,
            session_id = session_id
        );
        let _enter = span.enter();

        tracing::info!("Spawning agent {} (session: {})", agent_id, session_id);

        anyhow::ensure!(!agent_id.trim().is_empty(), "agent_id must not be empty");
        anyhow::ensure!(!message.trim().is_empty(), "message must not be empty");

        let cred_bindings = credential_bindings.to_vec();
        let mut result = self
            .execute_with_reliability_controls(agent_id, || async move {
                let repo = AgentRepository::from_config(&self.config);

            if let Some(source_id) = source_agent_id {
                if source_id != agent_id {
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
            let (agent_ref, _rev, _binding) = repo.resolve_and_pin_session(
                session_id,
                session_id, // root_session_id = session_id for single sessions
                agent_id,
                Some(gs.as_ref()),
                &default_gateway_host_id(),
            )?;
            tracing::info!(
                agent_id = %agent_ref.agent_id,
                revision_id = %agent_ref.revision_id,
                session_id = session_id,
                "Resolved session to pinned revision"
            );
            let gateway_dir = crate::execution::gateway_root_dir(&self.config);
            let loaded =
                repo.load_from_revision_dir(&gateway_dir, &agent_ref.agent_id, &agent_ref.revision_id)?;

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
                let gateway_dir = self.config.agents_dir.join(".gateway");
                let mut report =
                    SessionReportWriter::open(&gateway_dir, session_id, agent_id).ok();
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
                    )
                } else {
                    vec![]
                };
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
                            let _ = r.finish_session("script_exec_complete", Some(output));
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
                            };
                            let _ = gs.create_execution_trace(&trace);
                        }
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
                            let _ = r.finish_session("script_exec_failed", None);
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
                    &self.config.agents_dir.join(".gateway"),
                    session_id,
                ).unwrap_or_default();

                // Collect all named content written by the child agent
                let files = collect_named_content(
                    &self.config.agents_dir.join(".gateway"),
                    session_id,
                );

                // Collect shared knowledge (for script mode, typically empty)
                let shared_knowledge = collect_shared_knowledge(
                    &self.config.agents_dir.join(".gateway"),
                    source_agent_id.unwrap_or(agent_id),
                    agent_id,
                    Some(session_id),
                );

                return Ok(SpawnResult {
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
                });
            }

            let llm_config = loaded
                .manifest
                .llm_config
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Agent '{}' is missing llm_config", agent_id))?;
            let driver = build_driver(llm_config, self.http_client.clone())?;

            let openrouter_catalog =
                Arc::new(OpenRouterCatalog::new(self.http_client.clone()));
            let middleware = loaded.manifest.middleware.clone().unwrap_or_default();
            let mut runtime = self.attach_model_metadata(
                AgentExecutor::new(
                loaded.manifest,
                loaded.instructions,
                driver,
                loaded.dir,
                crate::runtime::tools::default_registry(),
                self.gateway_store.clone(),
            )
            .with_gateway_dir(self.config.agents_dir.join(".gateway"))
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
            // Phase 3: propagate overflow_recovery flag so the governor
            // uses an aggressive reduction pipeline on retry.
            let overflow_recovery = metadata
                .and_then(|m| m.get("overflow_recovery"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if overflow_recovery {
                runtime = runtime.with_overflow_recovery(true);
            }

            use crate::runtime::checkpoint::YieldReason;
            use crate::runtime::lifecycle::TurnOutcome;

            // --- Turn continuation / checkpoint resume ---
            // Priority order:
            // 1) Turn continuation (approval-unblocked workflow task)
            // 2) Session checkpoint (hibernation/budget/max-turns/manual/error)
            // 3) Fresh start
            let (outcome, resume_initial_message, consumed_checkpoint_turn_id) = if let Some(t_id) = task_id {
                let load_result = crate::runtime::continuation::load_continuation(&self.config, t_id);
                if let Err(ref e) = load_result {
                    if crate::runtime::continuation::is_integrity_error(e) {
                        tracing::error!(
                            target: "continuation",
                            task_id = %t_id,
                            error = %e,
                            "Continuation integrity violation — tampering suspected"
                        );
                        if let Some(store) = self.gateway_store.as_ref() {
                            let pending = store.get_pending_approvals().unwrap_or_default();
                            let matching: Vec<String> = pending
                                .iter()
                                .filter(|p| p.task_id.as_deref() == Some(t_id))
                                .map(|p| p.request_id.clone())
                                .collect();
                            if !matching.is_empty() {
                                let cancelled_at = chrono::Utc::now().to_rfc3339();
                                for rid in &matching {
                                    let _ = store.cancel_approval(rid, "gateway", &cancelled_at);
                                }
                                let _ = store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
                                    event_id: uuid::Uuid::new_v4().to_string(),
                                    agent_id: "gateway".to_string(),
                                    session_id: String::new(),
                                    turn_id: None,
                                    event_seq: 0,
                                    timestamp: cancelled_at.clone(),
                                    category: "background".to_string(),
                                    action: "continuation_tampered".to_string(),
                                    status: "error".to_string(),
                                    enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
                                    target: None,
                                    payload: Some(serde_json::json!({
                                        "task_id": t_id,
                                        "reason": "integrity_violation",
                                        "approval_request_ids": matching,
                                    }).to_string()),
                                    payload_ref: None,
                                    evidence_ref: None,
                                    reason: Some("HMAC mismatch on continuation load".to_string()),
                                });
                            }
                        }
                        anyhow::bail!("{}", e);
                    } else {
                        tracing::error!(
                            target: "continuation",
                            task_id = %t_id,
                            error = %e,
                            "Failed to load continuation"
                        );
                        anyhow::bail!("failed to load continuation for task '{}': {}", t_id, e);
                    }
                }
                if let Some(cont) = load_result.unwrap() {
                    tracing::info!(
                        target: "continuation",
                        task_id = %t_id,
                        approval_request_id = %cont.approval_request_id,
                        "Resuming turn from continuation after approval resolution"
                    );

                    // Fetch the approval decision from the gateway store.
                    let approval_req = self.gateway_store
                        .as_ref()
                        .and_then(|store| store.get_approval(&cont.approval_request_id).ok().flatten());

                    // Action-equality check: signed continuations must carry a pending_action
                    // that structurally equals the action from the approval row.  This prevents
                    // TOCTOU substitution where the approval row is swapped to a different action
                    // between suspension and resume.  Legacy unsigned continuations (no
                    // pending_action) are still accepted.
                    if let Some(ref req) = approval_req {
                        match cont.pending_action.as_ref() {
                            None => {
                                tracing::warn!(
                                    target: "continuation",
                                    task_id = %t_id,
                                    "Continuation missing pending_action — skipping action-equality check (legacy?)"
                                );
                            }
                            Some(pending) => {
                                if pending != &req.action {
                                    tracing::error!(
                                        target: "continuation",
                                        task_id = %t_id,
                                        approval_request_id = %cont.approval_request_id,
                                        "Action mismatch between continuation and approval row — possible substitution attack"
                                    );
                                    let _ = crate::runtime::continuation::delete_continuation(&self.config, t_id);
                                    anyhow::bail!(
                                        "continuation action mismatch: the action stored in the continuation does not match the approved action (task '{}')",
                                        t_id
                                    );
                                }
                            }
                        }
                    }

                    let approved_result = match approval_req {
                        Some(ref req) if req.status == Some(autonoetic_types::background::ApprovalStatus::Approved) => {
                            tracing::info!(
                                target: "continuation",
                                request_id = %cont.approval_request_id,
                                task_id = %t_id,
                                "Approval found - executing approved action"
                            );
                            let decision = autonoetic_types::background::ApprovalDecision {
                                request_id: req.request_id.clone(),
                                agent_id: req.agent_id.clone(),
                                session_id: req.session_id.clone(),
                                action: req.action.clone(),
                                status: autonoetic_types::background::ApprovalStatus::Approved,
                                decided_at: req.decided_at.clone().unwrap_or_default(),
                                decided_by: req.decided_by.clone().unwrap_or_default(),
                                reason: req.reason.clone(),
                                root_session_id: req.root_session_id.clone(),
                                workflow_id: req.workflow_id.clone(),
                                task_id: req.task_id.clone(),
                                approval_level: autonoetic_types::background::ApprovalLevel::Operator,
                            };
                            match crate::runtime::continuation::execute_approved_action(
                                &decision,
                                &runtime.manifest,
                                &runtime.agent_dir,
                                runtime.gateway_dir.as_deref(),
                                Some(&cont.session_id),
                                &self.config,
                                self.gateway_store.clone(),
                                Some(&cont.pending_tool_call),
                            ) {
                                Ok(r) => {
                                    tracing::info!(
                                        target: "continuation",
                                        request_id = %cont.approval_request_id,
                                        result_preview = %r.chars().take(100).collect::<String>(),
                                        "Approved action executed successfully"
                                    );
                                    let gateway_dir = self.config.agents_dir.join(".gateway");
                                    if let Ok(mut report) = SessionReportWriter::open(
                                        &gateway_dir,
                                        &cont.session_id,
                                        &runtime.manifest.agent.id,
                                    ) {
                                        let summary = format!(
                                            "Approved action executed: {}",
                                            r.chars().take(100).collect::<String>()
                                        );
                                        let _ = report.record_approval_resolved(
                                            &cont.approval_request_id,
                                            "approved",
                                            &summary,
                                        );
                                    }
                                    r
                                },
                                Err(e) => {
                                    tracing::error!(
                                        target: "continuation",
                                        request_id = %cont.approval_request_id,
                                        error = %e,
                                        "Failed to execute approved action"
                                    );
                                    let gateway_dir = self.config.agents_dir.join(".gateway");
                                    if let Ok(mut report) = SessionReportWriter::open(
                                        &gateway_dir,
                                        &cont.session_id,
                                        &runtime.manifest.agent.id,
                                    ) {
                                        let _ = report.record_approval_resolved(
                                            &cont.approval_request_id,
                                            "approved",
                                            &format!("Approved but execution failed: {}", e),
                                        );
                                    }
                                    serde_json::json!({
                                        "ok": false,
                                        "error": e.to_string(),
                                        "approval_ref": cont.approval_request_id,
                                    }).to_string()
                                }
                            }
                        }
                        Some(_) => {
                            // Rejected
                            let gateway_dir = self.config.agents_dir.join(".gateway");
                            if let Ok(mut report) = SessionReportWriter::open(
                                &gateway_dir,
                                &cont.session_id,
                                &runtime.manifest.agent.id,
                            ) {
                                let _ = report.record_approval_resolved(
                                    &cont.approval_request_id,
                                    "rejected",
                                    "Approval rejected by operator",
                                );
                            }
                            serde_json::json!({
                                "ok": false,
                                "approval_rejected": true,
                                "request_id": cont.approval_request_id,
                            }).to_string()
                        }
                        None => {
                            serde_json::json!({
                                "ok": false,
                                "error": "approval_decision_not_found",
                                "request_id": cont.approval_request_id,
                            }).to_string()
                        }
                    };

                    // Restore session state before executing remaining tool calls.
                    runtime.session_state = cont.session_state;

                    // Execute remaining tool calls from the original batch.
                    let remaining_results = if !cont.remaining_tool_calls.is_empty() {
                        let mut mcp_rt = crate::runtime::mcp::McpToolRuntime::from_env().await?;
                        let registry = crate::runtime::tools::default_registry();
                        let mut ds = crate::runtime::disclosure::DisclosureState::default();
                        let mut proc = crate::runtime::tool_call_processor::ToolCallProcessor::new(
                            &mut mcp_rt,
                            &registry,
                            &runtime.manifest,
                            &mut ds,
                            None,
                            Some(&self.config),
                            self.gateway_store.clone(),
                            None,
                        ).with_session_context(
                            Some(cont.session_id.clone()),
                            Some(cont.turn_id.clone()),
                        ).with_session_state(runtime.session_state);
                        let mut tracer = crate::runtime::session_tracer::SessionTracer::new_with_evidence_mode(
                            &runtime.agent_dir,
                            &runtime.manifest.agent.id,
                            &cont.session_id,
                            &self.config.evidence_mode,
                        )?;
                        let (_, results) = proc
                            .process_tool_calls(
                                &cont.remaining_tool_calls,
                                &runtime.agent_dir,
                                runtime.gateway_dir.as_deref(),
                                &mut tracer,
                            )
                            .await
                            .unwrap_or_default();
                        results
                    } else {
                        vec![]
                    };

                    // Reconstruct conversation history and restore guard state.
                    let mut history = crate::runtime::continuation::reconstruct_history(
                        &cont,
                        approved_result,
                        remaining_results,
                    );

                    let initial_msg = cont.history
                        .iter()
                        .find(|m| matches!(m.role, crate::llm::Role::User))
                        .map(|m| m.content.clone())
                        .unwrap_or_default();

                    runtime.guard = crate::runtime::guard::LoopGuard::restore(cont.loop_guard_state.clone());
                    runtime.session_id = Some(cont.session_id.clone());
                    runtime.session_started = true;
                    runtime.tool_tier_escalated = cont.tool_tier_escalated;
                    runtime.discovered_tools = cont.discovered_tools.clone();
                    runtime.turn_counter = cont.turn_id
                        .trim_start_matches("turn-")
                        .parse()
                        .unwrap_or(0);

                    // Delete the continuation file — we are now live.
                    let _ = crate::runtime::continuation::delete_continuation(&self.config, t_id);

                    let outcome = execute_with_history_close_on_error(&mut runtime, &mut history).await?;
                    (outcome, initial_msg, None)
                } else {
                    // No continuation on disk — optionally resume from latest checkpoint.
                    let checkpoint = crate::runtime::checkpoint::load_latest_checkpoint(
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
                            gateway_signal_turn_start_context(&runtime.initial_user_message, metadata);
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
                }
            } else {
                let checkpoint =
                    crate::runtime::checkpoint::load_latest_checkpoint(&self.config, session_id)?;
                if let Some(checkpoint) = checkpoint {
                    if matches!(
                        checkpoint.yield_reason,
                        YieldReason::EmergencyStop { .. }
                    ) {
                        tracing::warn!(
                            target: "execution",
                            session_id = %session_id,
                            turn_counter = checkpoint.turn_counter,
                            "Session was emergency-stopped — continuing with preserved context and fresh LoopGuard"
                        );
                        // Restore session state from checkpoint (turn counter, session
                        // state, etc.) but replace the guard with a fresh one so that
                        // accumulated failure budgets don't immediately re-trip.
                        checkpoint.restore_into(&mut runtime);
                        runtime.guard = crate::runtime::tool_dispatch::loop_guard_from_config_and_manifest(
                            runtime.config.as_deref(),
                            &runtime.agent_dir,
                        );
                        // If the incoming message is already the last user message in
                        // the checkpoint history, don't duplicate it.
                        let mut history = checkpoint.history.clone();
                        let last_user = history.iter().rev().find(|m| m.role == crate::llm::Role::User);
                        let should_append = match last_user {
                            Some(last) => last.content != message,
                            None => true,
                        };
                        if should_append {
                            history.push(crate::llm::Message::user(message));
                        }
                        let initial_msg = checkpoint.initial_user_message();
                        let outcome = execute_with_history_close_on_error(&mut runtime, &mut history).await?;
                        (outcome, initial_msg, Some(checkpoint.turn_id))
                    } else {
                        self.resume_from_checkpoint(
                            &mut runtime,
                            session_id,
                            message,
                            metadata,
                            checkpoint,
                        )
                        .await?
                    }
                } else {
                    let (turn_start_messages, initial_message) =
                        gateway_signal_turn_start_context(&runtime.initial_user_message, metadata);
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
            };

            let resolved_session_id = runtime
                .session_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("runtime session_id missing after execution"))?;

            let (assistant_reply, suspended_for_approval, suspended_for_user_input) = match outcome {
                TurnOutcome::Completed(reply) => (reply, None, false),
                TurnOutcome::Suspended { approval_request_id, .. } => {
                    // Continuation already saved by execute_with_history.
                    (None, Some(approval_request_id), false)
                }
                TurnOutcome::SuspendedUserInput { interaction_id: _ } => {
                    // Checkpoint already saved by execute_with_history with
                    // YieldReason::UserInputRequired. Signal that the session
                    // is blocked on user input (not "completed empty").
                    (None, None, true)
                }
                TurnOutcome::Escalated { .. } => {
                    // Checkpoint already saved by execute_with_history with
                    // YieldReason::HumanEscalation. Signal that the session
                    // is blocked on operator approval.
                    (None, None, true)
                }
            };

            if let Some(checkpoint_turn_id) = consumed_checkpoint_turn_id {
                if let Err(e) = crate::runtime::checkpoint::delete_checkpoint(
                    &self.config,
                    session_id,
                    &checkpoint_turn_id,
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

            persist_session_context_turn(
                &runtime.agent_dir,
                &resolved_session_id,
                &resume_initial_message,
                assistant_reply.as_deref(),
            );
            let close_reason = SessionCloseReason::for_jsonrpc_spawn(
                suspended_for_approval.is_some(),
                suspended_for_user_input,
                assistant_reply.is_some(),
            )
            .as_str();
            let digest_turn_count = runtime.turn_counter;
            // Write the auto-populated SessionOutcome row before close_session
            // so we capture the runtime state in one place. The metrics are
            // session-end snapshots (cumulative cost, tokens, turns,
            // wall-clock) — best-effort write, errors logged but not
            // propagated. Self-Improvement loop P0 (#245).
            if let Some(store) = self.gateway_store.as_ref() {
                crate::runtime::session_outcome_writer::write_session_outcome_metrics(
                    &runtime,
                    store,
                    &resolved_session_id,
                    agent_id,
                );
                if close_reason == "jsonrpc_spawn_complete_empty" {
                    if let Ok(tool_count) =
                        store.count_execution_traces_for_session(&resolved_session_id)
                    {
                        if let Some(draft) =
                            crate::runtime::operator_activity::classify_session_lifecycle(
                                close_reason,
                                tool_count.min(u32::MAX as u64) as u32,
                            )
                        {
                            let root_id = crate::runtime::live_digest::base_session_id(
                                &resolved_session_id,
                            )
                            .to_string();
                            let record = draft.into_record(
                                root_id,
                                resolved_session_id.clone(),
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
            runtime.close_session(close_reason)?;
            {
                let root_id = crate::runtime::live_digest::base_session_id(&resolved_session_id).to_string();
                let is_suspended = suspended_for_approval.is_some() || suspended_for_user_input;
                let gw_dir = self.config.agents_dir.join(".gateway");
                let ctx = autonoetic_types::hooks::HookContext::for_session_closed(
                    &root_id,
                    &resolved_session_id,
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
            let is_suspended = suspended_for_approval.is_some() || suspended_for_user_input;
            if !is_suspended {
                if let Err(e) = crate::runtime::checkpoint::prune_checkpoints(
                    self.config.as_ref(),
                    &resolved_session_id,
                    2,
                ) {
                    tracing::debug!(
                        target: "checkpoint",
                        session_id = %resolved_session_id,
                        error = %e,
                        "Failed to prune session checkpoints after completion"
                    );
                }
            }
            crate::runtime::post_session_digest::maybe_run_post_session_digest(
                self.config.as_ref(),
                &self.config.agents_dir.join(".gateway"),
                self.gateway_store.as_ref(),
                &self.http_client,
                &resolved_session_id,
                agent_id,
                digest_turn_count,
                is_suspended,
            )
            .await;
            // Outcome grader: attach an LLM-judged Completion verdict to
            // the auto-populated SessionOutcome row. Off by default; gated
            // on `outcome_grader.enabled`. Runs after the post-session
            // digest so the SessionOverview snapshot the grader sees
            // includes the latest digest tail. Self-Improvement loop P0.
            crate::runtime::session_outcome_writer::maybe_run_outcome_grader(
                self.config.as_ref(),
                &self.config.agents_dir.join(".gateway"),
                self.gateway_store.as_ref(),
                &self.http_client,
                &resolved_session_id,
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
                    &resolved_session_id,
                    agent_id,
                    digest_turn_count,
                    is_suspended,
                )
                .await;
            }
            let llm_usage = runtime.take_llm_usage_last_run();
            let workflow_note = if suspended_for_approval.is_none() && !suspended_for_user_input {
                build_gateway_workflow_note(self.config.as_ref(), &resolved_session_id, assistant_reply.as_deref())
            } else {
                None
            };

            // Extract artifacts from content store
            let artifacts = extract_artifacts_from_content_store(
                &self.config.agents_dir.join(".gateway"),
                &resolved_session_id,
            ).unwrap_or_default();

            // Collect all named content written by the child agent
            let files = collect_named_content(
                &self.config.agents_dir.join(".gateway"),
                &resolved_session_id,
            );

            // Collect knowledge shared with the caller
            let shared_knowledge = collect_shared_knowledge(
                &self.config.agents_dir.join(".gateway"),
                source_agent_id.unwrap_or(agent_id),
                agent_id,
                Some(&resolved_session_id),
            );

            Ok(SpawnResult {
                agent_id: agent_id.to_string(),
                session_id: resolved_session_id,
                assistant_reply,
                workflow_note,
                should_signal_background,
                artifacts,
                files,
                shared_knowledge,
                llm_usage,
                suspended_for_approval,
                suspended_for_user_input,
            })
        })
        .await?;
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
        let gateway_dir = crate::execution::gateway_root_dir(&self.config);
        let manifest_loaded = {
            let repo = AgentRepository::from_config(&self.config);
            repo.get_sync_from_store(agent_id, &gateway_dir, self.gateway_store.as_deref())
                .ok()
                .map(|loaded| loaded.manifest)
        };
        let manifest_returns_schema = manifest_loaded
            .as_ref()
            .and_then(|manifest| manifest.io.as_ref())
            .and_then(|io| io.returns.clone());
        let manifest_output_policy = manifest_loaded
            .as_ref()
            .and_then(|manifest| manifest.io.as_ref())
            .and_then(|io| io.output_policy.clone());
        let manifest_execution_mode = manifest_loaded
            .as_ref()
            .map(|m| m.execution_mode)
            .unwrap_or_default();
        let manifest_returns_enforcement = manifest_loaded
            .as_ref()
            .and_then(|manifest| manifest.io.as_ref())
            .map(|io| {
                io.effective_returns_enforcement(manifest_execution_mode)
            })
            .unwrap_or_default();

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

    /// Resume execution after a `user.ask` interaction was answered.
    ///
    /// Loads the interaction to extract session/agent/workflow identity, then
    /// delegates to `spawn_agent_once` which handles checkpoint loading and the
    /// `UserInputRequired` resume branch (answer injection + continued execution).
    ///
    /// Returns a structured error `session_waiting_for_approval:{session}:{id}` when
    /// the latest checkpoint has shifted to `ApprovalRequired` — the scheduler uses
    /// this to defer the resume to the approval path.
    pub async fn resume_from_user_interaction(
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

        // Pre-check: if the latest checkpoint shifted away from UserInputRequired,
        // return early so the scheduler can defer or report appropriately.
        use crate::runtime::checkpoint::{load_latest_checkpoint, YieldReason};
        if let Some(cp) = load_latest_checkpoint(self.config.as_ref(), &interaction.session_id)? {
            match &cp.yield_reason {
                YieldReason::UserInputRequired { interaction_id: cid } => {
                    anyhow::ensure!(
                        cid == &interaction.interaction_id,
                        "Checkpoint is for interaction '{}', not '{}'",
                        cid,
                        interaction.interaction_id
                    );
                }
                YieldReason::ApprovalRequired { approval_request_id } => {
                    tracing::debug!(
                        target: "scheduler",
                        interaction_id = %interaction.interaction_id,
                        session_id = %interaction.session_id,
                        approval_request_id = %approval_request_id,
                        "Skipping user-interaction resume: session is now waiting for approval"
                    );
                    return Err(anyhow::anyhow!(
                        "session_waiting_for_approval:{}:{}",
                        interaction.session_id,
                        approval_request_id
                    ));
                }
                other => {
                    anyhow::bail!(
                        "Latest checkpoint for session '{}' is not UserInputRequired (got {:?})",
                        interaction.session_id,
                        other
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
    ) -> anyhow::Result<SpawnResult> {
        use crate::runtime::checkpoint::{load_latest_checkpoint, YieldReason};
        use crate::runtime::lifecycle::TurnOutcome;

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
        let loaded = if let Some(ref gs) = self.gateway_store {
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

        let llm_config = loaded
            .manifest
            .llm_config
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' is missing llm_config", agent_id))?;
        let driver = build_driver(llm_config, self.http_client.clone())?;

        let openrouter_catalog = Arc::new(OpenRouterCatalog::new(self.http_client.clone()));
        let middleware = loaded.manifest.middleware.clone().unwrap_or_default();
        let mut runtime = self.attach_model_metadata(
            AgentExecutor::new(
            loaded.manifest,
            loaded.instructions,
            driver,
            loaded.dir,
            crate::runtime::tools::default_registry(),
            self.gateway_store.clone(),
        )
        .with_gateway_dir(self.config.agents_dir.join(".gateway"))
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

        let (assistant_reply, suspended_for_approval, suspended_for_user_input) = match outcome {
            TurnOutcome::Completed(reply) => (reply, None, false),
            TurnOutcome::Suspended {
                approval_request_id,
                ..
            } => (None, Some(approval_request_id), false),
            TurnOutcome::SuspendedUserInput { interaction_id: _ } => (None, None, true),
            TurnOutcome::Escalated { .. } => (None, None, true),
        };

        let initial_msg = history
            .iter()
            .find(|m| matches!(m.role, crate::llm::Role::User))
            .map(|m| m.content.clone())
            .unwrap_or_default();

        persist_session_context_turn(
            &runtime.agent_dir,
            &resolved_session_id,
            &initial_msg,
            assistant_reply.as_deref(),
        );
        let close_reason = SessionCloseReason::for_checkpoint_respawn(
            suspended_for_approval.is_some(),
            suspended_for_user_input,
            assistant_reply.is_some(),
        )
        .as_str();
        let digest_turn_count = runtime.turn_counter;
        // SessionOutcome metrics — auto-populated on every session close
        // (Self-Improvement P0 #245). Mirrors the spawn-path call above.
        if let Some(store) = self.gateway_store.as_ref() {
            crate::runtime::session_outcome_writer::write_session_outcome_metrics(
                &runtime,
                store,
                &resolved_session_id,
                agent_id,
            );
        }
        runtime.close_session(close_reason)?;
        {
            let root_id =
                crate::runtime::live_digest::base_session_id(&resolved_session_id).to_string();
            let is_suspended = suspended_for_approval.is_some() || suspended_for_user_input;
            let ctx = autonoetic_types::hooks::HookContext::for_session_closed(
                &root_id,
                &resolved_session_id,
                agent_id,
                close_reason,
                digest_turn_count,
                Some(&self.config.agents_dir.join(".gateway")),
            );
            if is_suspended {
                let mut suspended_ctx = ctx.clone();
                suspended_ctx.event = autonoetic_types::hooks::HookEvent::SessionSuspended;
                self.hook_executor.dispatch_async(suspended_ctx);
            } else {
                self.hook_executor.dispatch_async(ctx);
            }
        }
        let is_checkpoint_suspended = suspended_for_approval.is_some() || suspended_for_user_input;
        crate::runtime::post_session_digest::maybe_run_post_session_digest(
            self.config.as_ref(),
            &self.config.agents_dir.join(".gateway"),
            self.gateway_store.as_ref(),
            &self.http_client,
            &resolved_session_id,
            agent_id,
            digest_turn_count,
            is_checkpoint_suspended,
        )
        .await;
        crate::runtime::session_outcome_writer::maybe_run_outcome_grader(
            self.config.as_ref(),
            &self.config.agents_dir.join(".gateway"),
            self.gateway_store.as_ref(),
            &self.http_client,
            &resolved_session_id,
            agent_id,
            digest_turn_count,
            is_checkpoint_suspended,
        )
        .await;
        if let Some(gs) = self.gateway_store.as_ref() {
            let mem_store = crate::runtime::memory::SqliteMemoryStore::new(gs.clone());
            crate::runtime::quality_signal::maybe_emit_quality_signal(
                self.config.as_ref(),
                self.gateway_store.as_ref(),
                &mem_store,
                &resolved_session_id,
                agent_id,
                digest_turn_count,
                is_checkpoint_suspended,
            )
            .await;
        }
        let llm_usage = runtime.take_llm_usage_last_run();

        let artifacts = extract_artifacts_from_content_store(
            &self.config.agents_dir.join(".gateway"),
            &resolved_session_id,
        )
        .unwrap_or_default();

        let files = collect_named_content(
            &self.config.agents_dir.join(".gateway"),
            &resolved_session_id,
        );

        let shared_knowledge = collect_shared_knowledge(
            &self.config.agents_dir.join(".gateway"),
            source_agent_id.unwrap_or(agent_id),
            agent_id,
            Some(&resolved_session_id),
        );

        // Delete consumed checkpoint only after successful resume execution.
        if let Err(e) = crate::runtime::checkpoint::delete_checkpoint(
            &self.config,
            session_id,
            &checkpoint.turn_id,
        ) {
            tracing::warn!(
                target: "checkpoint",
                session_id = %session_id,
                turn_id = %checkpoint.turn_id,
                error = %e,
                "Failed to delete consumed checkpoint"
            );
        }

        let workflow_note = build_gateway_workflow_note(
            self.config.as_ref(),
            &resolved_session_id,
            assistant_reply.as_deref(),
        );

        Ok(SpawnResult {
            agent_id: agent_id.to_string(),
            session_id: resolved_session_id,
            assistant_reply,
            workflow_note,
            should_signal_background: false,
            artifacts,
            files,
            shared_knowledge,
            llm_usage,
            suspended_for_approval,
            suspended_for_user_input,
        })
    }

    /// Spawn a clarification child session of the agent that requested a
    /// pending approval, ask it the operator's question, capture its reply
    /// as a `gate_message` on the original approval, and return the reply.
    ///
    /// See `docs/design/human-gate-unification-plan.md` §Phase 5 for the
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
        let loaded = repo
            .get_sync_from_store(&agent_id, &gateway_dir, self.gateway_store.as_deref())
            .map_err(|e| anyhow::anyhow!("Failed to load agent '{}': {}", agent_id, e))?;

        let llm_config = loaded
            .manifest
            .llm_config
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' is missing llm_config", agent_id))?;
        let driver = build_driver(llm_config, self.http_client.clone())?;

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
            crate::runtime::tools::default_registry(),
            self.gateway_store.clone(),
        )
        .with_gateway_dir(self.config.agents_dir.join(".gateway"))
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
        let loaded = repo
            .get_sync_from_store(&agent_id, &gateway_dir, self.gateway_store.as_deref())
            .map_err(|e| anyhow::anyhow!("Failed to load agent '{}': {}", agent_id, e))?;

        let llm_config = loaded
            .manifest
            .llm_config
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' is missing llm_config", agent_id))?;
        let driver = build_driver(llm_config, self.http_client.clone())?;

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
            crate::runtime::tools::default_registry(),
            self.gateway_store.clone(),
        )
        .with_gateway_dir(self.config.agents_dir.join(".gateway"))
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
                &crate::runtime::tools::default_registry(),
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
        let agent_admission = self.agent_admission_semaphore(agent_id).await;
        let _admission_permit = agent_admission.try_acquire_owned().map_err(|_| {
            anyhow::anyhow!(
                "Backpressure: pending execution queue is full for agent '{}'",
                agent_id
            )
        })?;

        let agent_lock = self.agent_execution_lock(agent_id).await;
        let _agent_guard = agent_lock.lock().await;

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

        operation().await
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

pub fn gateway_actor_id() -> String {
    std::env::var("AUTONOETIC_NODE_ID").unwrap_or_else(|_| "gateway".to_string())
}

pub fn gateway_root_dir(config: &GatewayConfig) -> std::path::PathBuf {
    config.agents_dir.join(".gateway")
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

fn gateway_signal_turn_start_context(
    user_message: &str,
    metadata: Option<&serde_json::Value>,
) -> (Vec<Message>, String) {
    let is_signal_delivery = metadata
        .and_then(|value| value.get("signal_delivered"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !is_signal_delivery {
        return (Vec::new(), user_message.to_string());
    }

    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(user_message) else {
        return (Vec::new(), user_message.to_string());
    };
    if parsed.get("type").and_then(|value| value.as_str()) != Some("child_state_notification") {
        return (Vec::new(), user_message.to_string());
    }

    let Some(notification_value) = parsed.get("notification") else {
        return (Vec::new(), user_message.to_string());
    };
    let Ok(notification) =
        serde_json::from_value::<autonoetic_types::workflow::ChildStateNotification>(
            notification_value.clone(),
        )
    else {
        return (Vec::new(), user_message.to_string());
    };

    let pretty = serde_json::to_string_pretty(&notification)
        .unwrap_or_else(|_| notification_value.to_string());
    (
        vec![Message::system(format!(
            "[gateway child state notification]\n{}",
            pretty
        ))],
        "Gateway child-state notification delivered. Continue from the current workflow state and use the structured gateway child state above.".to_string(),
    )
}

fn persist_session_context_turn(
    agent_dir: &std::path::Path,
    session_id: &str,
    user_message: &str,
    assistant_reply: Option<&str>,
) {
    let result = (|| -> anyhow::Result<()> {
        let mut context = SessionContext::load(agent_dir, session_id)?;
        context.record_turn(user_message, assistant_reply);
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
        context.record_turn("remember Atlas", Some("Stored that."));
        context
            .save(temp.path())
            .expect("session context should save");

        let manifest = autonoetic_types::agent::AgentManifest {
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
                id: "test-agent".to_string(),
                name: "Test Agent".to_string(),
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
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
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
            gateway_signal_turn_start_context(&message, Some(&metadata));

        assert_eq!(turn_start_messages.len(), 1);
        assert!(matches!(turn_start_messages[0].role, crate::llm::Role::System));
        assert!(turn_start_messages[0]
            .content
            .contains("[gateway child state notification]"));
        assert!(turn_start_messages[0].content.contains("\"task_id\": \"task-1\""));
        assert_eq!(
            user_message,
            "Gateway child-state notification delivered. Continue from the current workflow state and use the structured gateway child state above."
        );
    }

    #[test]
    fn test_persist_session_context_turn_writes_current_exchange() {
        let temp = tempfile::tempdir().expect("tempdir should create");

        persist_session_context_turn(
            temp.path(),
            "session-2",
            "hello there",
            Some("general kenobi"),
        );

        let path = session_context_path(temp.path(), "session-2");
        let body = std::fs::read_to_string(path).expect("session context file should exist");
        assert!(body.contains("\"last_user_message\": \"hello there\""));
        assert!(body.contains("\"last_assistant_reply\": \"general kenobi\""));
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
    fn session_close_reason_jsonrpc_mapping_is_closed_and_stable() {
        assert_eq!(
            SessionCloseReason::for_jsonrpc_spawn(true, false, false).as_str(),
            "jsonrpc_spawn_suspended_approval"
        );
        assert_eq!(
            SessionCloseReason::for_jsonrpc_spawn(false, true, false).as_str(),
            "jsonrpc_spawn_suspended_user_input"
        );
        assert_eq!(
            SessionCloseReason::for_jsonrpc_spawn(false, false, true).as_str(),
            "jsonrpc_spawn_complete"
        );
        assert_eq!(
            SessionCloseReason::for_jsonrpc_spawn(false, false, false).as_str(),
            "jsonrpc_spawn_complete_empty"
        );
    }

    #[test]
    fn session_close_reason_checkpoint_mapping_is_closed_and_stable() {
        assert_eq!(
            SessionCloseReason::for_checkpoint_respawn(true, false, false).as_str(),
            "checkpoint_respawn_suspended"
        );
        assert_eq!(
            SessionCloseReason::for_checkpoint_respawn(false, true, false).as_str(),
            "checkpoint_respawn_suspended_user_input"
        );
        assert_eq!(
            SessionCloseReason::for_checkpoint_respawn(false, false, true).as_str(),
            "checkpoint_respawn_complete"
        );
        assert_eq!(
            SessionCloseReason::for_checkpoint_respawn(false, false, false).as_str(),
            "checkpoint_respawn_complete_empty"
        );
    }

    #[test]
    fn inject_approval_ref_into_history_adds_ref_to_last_tool_call() {
        use crate::llm::ToolCall;

        let mut history = vec![
            Message::user("Set up credentials"),
            Message {
                role: crate::llm::Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "credential.setup".to_string(),
                    arguments: r#"{"skill_url":"http://localhost:8080/skill.md"}"#.to_string(),
                }],
                tool_call_id: None,
                reasoning_content: None,
                reasoning_details: None,
            },
        ];

        inject_approval_ref_into_history(&mut history, "apr-abc123");

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

        inject_approval_ref_into_history(&mut history, "apr-new");

        let tc = &history[0].tool_calls[0];
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap();
        // Should be overwritten with the new ref.
        assert_eq!(args["approval_ref"], "apr-new");
        assert_eq!(args["command"], "curl http://api.example.com");
    }
}
