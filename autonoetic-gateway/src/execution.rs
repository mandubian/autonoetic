//! Shared gateway execution service for ingress and scheduler-driven runs.

use crate::agent::AgentRepository;
use crate::causal_chain::CausalLogger;
use crate::llm::{build_driver, Message};
use crate::runtime::active_execution_registry::ActiveExecutionRegistry;
use crate::runtime::lifecycle::AgentExecutor;
use crate::runtime::openrouter_catalog::OpenRouterCatalog;
use crate::runtime::reevaluation_state::execute_scheduled_action;
use crate::runtime::session_budget::SessionBudgetRegistry;
use crate::runtime::session_context::SessionContext;
use crate::runtime::live_digest::base_session_id;
use autonoetic_types::agent::{AgentManifest, ExecutionMode, LlmExchangeUsage};
use autonoetic_types::background::{ScheduledAction, UserInteraction, UserInteractionStatus};
use autonoetic_types::causal_chain::EntryStatus;
use autonoetic_types::config::GatewayConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

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
    /// Full SHA-256 content handle (e.g. "sha256:838ddf76...").
    pub handle: String,
    /// Short 8-hex-char alias for LLM-friendly lookup (e.g. "838ddf76").
    pub alias: String,
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
/// child produced — with names, handles, and short aliases — without having to parse
/// the child's free-text reply.
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
            Some(ContentFile {
                name,
                handle,
                alias,
            })
        })
        .collect()
}

/// Collects knowledge that was shared with a specific agent during execution.
///
/// Queries the Tier 2 memory for records that:
/// 1. Have visibility "shared" or "global"
/// 2. Include the target_agent_id in allowed_agents
/// 3. Were created or updated recently (within this session)
pub fn collect_shared_knowledge(
    gateway_dir: &std::path::Path,
    target_agent_id: &str,
    writer_agent_id: &str,
) -> Vec<SharedKnowledge> {
    let Ok(mem) = crate::runtime::memory::Tier2Memory::new(gateway_dir, writer_agent_id) else {
        return Vec::new();
    };

    // Get all memories owned by the writer agent
    let Ok(all_memories) = mem.list_memories() else {
        return Vec::new();
    };

    // Filter to those shared with the target agent
    all_memories
        .into_iter()
        .filter(|m| match &m.visibility {
            autonoetic_types::memory::MemoryVisibility::Global => true,
            autonoetic_types::memory::MemoryVisibility::Shared => {
                m.allowed_agents.contains(&target_agent_id.to_string())
            }
            autonoetic_types::memory::MemoryVisibility::Private => false,
        })
        .map(|m| {
            let preview = if m.content.len() > 100 {
                format!("{}...", &m.content[..100])
            } else {
                m.content.clone()
            };
            SharedKnowledge {
                id: m.memory_id,
                scope: m.scope,
                content_preview: preview,
                writer_agent_id: m.writer_agent_id,
                created_at: m.created_at,
            }
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

#[derive(Debug)]
pub struct SpawnResult {
    pub agent_id: String,
    pub session_id: String,
    pub assistant_reply: Option<String>,
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
}

#[derive(Clone)]
pub struct GatewayExecutionService {
    config: Arc<GatewayConfig>,
    http_client: reqwest::Client,
    execution_semaphore: Arc<Semaphore>,
    agent_admission: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    agent_execution_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Shared per-session budget counters for all spawns using this gateway process.
    session_budget: Arc<SessionBudgetRegistry>,
    gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    active_executions: Arc<ActiveExecutionRegistry>,
}

impl GatewayExecutionService {
    pub fn new(
        config: GatewayConfig,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    ) -> Self {
        let session_budget = Arc::new(SessionBudgetRegistry::new(config.session_budget.clone()));
        Self {
            execution_semaphore: Arc::new(Semaphore::new(config.max_concurrent_spawns.max(1))),
            agent_admission: Arc::new(Mutex::new(HashMap::new())),
            agent_execution_locks: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(config),
            http_client: reqwest::Client::new(),
            session_budget,
            gateway_store,
            active_executions: ActiveExecutionRegistry::new(),
        }
    }

    pub fn config(&self) -> Arc<GatewayConfig> {
        self.config.clone()
    }

    pub fn gateway_store(&self) -> Option<Arc<crate::scheduler::gateway_store::GatewayStore>> {
        self.gateway_store.clone()
    }

    pub fn active_executions(&self) -> Arc<ActiveExecutionRegistry> {
        self.active_executions.clone()
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
        use crate::runtime::checkpoint::{load_latest_checkpoint, save_checkpoint, SessionCheckpoint, YieldReason};
        use crate::runtime::guard::LoopGuardState;
        use crate::scheduler::gateway_store::EmergencyStopRecord;

        let store = self
            .gateway_store()
            .ok_or_else(|| anyhow::anyhow!("gateway store required for emergency stop"))?;
        let root_session_id = root_session_id.trim();
        anyhow::ensure!(!root_session_id.is_empty(), "root_session_id must not be empty");
        anyhow::ensure!(!reason.trim().is_empty(), "reason must not be empty");

        if let Some(aid) = source_agent_id {
            let repo = AgentRepository::from_config(self.config.as_ref());
            let loaded = repo.get_sync(aid)?;
            let policy = crate::policy::PolicyEngine::new(loaded.manifest);
            anyhow::ensure!(
                policy.can_request_emergency_stop(),
                "Permission Denied: agent '{}' cannot request emergency stop",
                aid
            );
        }

        let stop_id = format!("estop-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let requested_at = chrono::Utc::now().to_rfc3339();

        let workflow_id = crate::scheduler::workflow_store::resolve_workflow_id_for_root_session(
            self.config.as_ref(),
            root_session_id,
        )?;

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
            )?;
        }

        let cancel_note = format!("emergency_stop:{stop_id} — {reason}");
        for inter in store.get_pending_interactions_for_root_session(root_session_id)? {
            store.cancel_user_interaction(&inter.interaction_id, &cancel_note)?;
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
                    current_loops: 0,
                    last_failure_hash: None,
                    consecutive_failures: 0,
                },
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

    pub async fn spawn_agent_once(
        &self,
        agent_id: &str,
        message: &str,
        session_id: &str,
        source_agent_id: Option<&str>,
        is_message: bool,
        ingest_event_type: Option<&str>,
        metadata: Option<&serde_json::Value>,
        // Workflow / task context for turn continuation saves on approval suspension.
        workflow_id: Option<&str>,
        task_id: Option<&str>,
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

        let mut result = self
            .execute_with_reliability_controls(agent_id, || async move {
                let repo = AgentRepository::from_config(&self.config);

            if let Some(source_id) = source_agent_id {
                if source_id != agent_id {
                    let source_loaded = repo.get_sync(source_id)?;
                    let source_policy = crate::policy::PolicyEngine::new(source_loaded.manifest);

                    if is_message {
                        anyhow::ensure!(
                            source_policy.can_message_agent(agent_id),
                            "Permission Denied: Source agent '{}' lacks 'AgentMessage' capability to message '{}'",
                            source_id,
                            agent_id
                        );
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
                    }
                }
            }

            let loaded = repo.get_sync(agent_id)?;

            // Validate spawn input against target agent's accepts schema (informational only)
            if let Some(ref io_schema) = loaded.manifest.io {
                if let Some(ref accepts) = io_schema.accepts {
                    let validation = validate_against_schema(message, accepts);
                    tracing::info!(
                        agent_id = agent_id,
                        valid = validation.valid,
                        issues = ?validation.issues,
                        "Input schema validation"
                    );
                    if let Err(error) = log_input_schema_validation_to_gateway(
                        self.config.as_ref(),
                        session_id,
                        source_agent_id,
                        agent_id,
                        message,
                        &validation,
                    ) {
                        tracing::warn!(
                            error = %error,
                            agent_id = agent_id,
                            session_id = session_id,
                            "Failed to append input schema validation to gateway causal chain"
                        );
                    }
                }
            }
            // Determine if background signaling is needed
            let should_signal_background = ingest_event_type.is_some()
                && loaded
                    .manifest
                    .background
                    .as_ref()
                    .map(|bg| bg.enabled && bg.wake_predicates.new_messages)
                    .unwrap_or(false);
            // Signal inbox for background scheduler if this is an event.ingest call
            if should_signal_background {
                let event_type = ingest_event_type.unwrap();
                let _ = crate::scheduler::append_inbox_event(
                    &self.config,
                    agent_id,
                    crate::router::ingress_wake_signal_internal(event_type, session_id),
                    Some(session_id),
                );
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

                // Log script start to gateway causal chain
                let gateway_logger = init_gateway_causal_logger(self.config.as_ref())?;
                log_gateway_causal_event(
                    &gateway_logger,
                    agent_id,
                    session_id,
                    1,
                    "script.started",
                    EntryStatus::Success,
                    Some(serde_json::json!({
                        "script_entry": script_entry,
                        "sandbox": loaded.manifest.runtime.sandbox
                    })),
                );

                // Execute script directly in sandbox
                let script_kill_scope = Some((
                    self.active_executions.clone(),
                    crate::runtime::live_digest::base_session_id(session_id).to_string(),
                ));
                let script_result = execute_script_in_sandbox(
                    &loaded.dir,
                    &script_path,
                    message,
                    &loaded.manifest.runtime.sandbox,
                    self.config.as_ref(),
                    script_kill_scope,
                    &loaded.manifest.capabilities,
                )
                .await;

                // Log script completion/failure
                match &script_result {
                    Ok(result) => {
                        log_gateway_causal_event(
                            &gateway_logger,
                            agent_id,
                            session_id,
                            2,
                            "script.completed",
                            EntryStatus::Success,
                            Some(serde_json::json!({
                                "result_len": result.len()
                            })),
                        );
                    }
                    Err(e) => {
                        log_gateway_causal_event(
                            &gateway_logger,
                            agent_id,
                            session_id,
                            2,
                            "script.failed",
                            EntryStatus::Error,
                            Some(serde_json::json!({
                                "error": e.to_string()
                            })),
                        );
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
                );

                return Ok(SpawnResult {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    assistant_reply: Some(script_result),
                    should_signal_background,
                    artifacts,
                    files,
                    shared_knowledge,
                    llm_usage: Vec::new(),
                    suspended_for_approval: None,
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
            let mut runtime = AgentExecutor::new(
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
            .with_openrouter_catalog(Some(openrouter_catalog))
            .with_middleware(middleware)
            .with_initial_user_message(message.to_string())
            .with_session_id(session_id.to_string())
            .with_workflow_context(
                workflow_id.map(String::from),
                task_id.map(String::from),
            )
            .with_active_executions(Some(self.active_executions.clone()));

            use crate::runtime::lifecycle::TurnOutcome;

            // --- Turn continuation / checkpoint resume ---
            // Priority order:
            // 1) Turn continuation (approval-unblocked workflow task)
            // 2) Session checkpoint (hibernation/budget/max-turns/manual/error)
            // 3) Fresh start
            let (outcome, resume_initial_message, consumed_checkpoint_turn_id) = if let Some(t_id) = task_id {
                if let Ok(Some(cont)) = crate::runtime::continuation::load_continuation(&self.config, t_id) {
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
                            };
                            match crate::runtime::continuation::execute_approved_action(
                                &decision,
                                &runtime.manifest,
                                &runtime.agent_dir,
                                runtime.gateway_dir.as_deref(),
                                Some(&cont.session_id),
                                &self.config,
                                self.gateway_store.clone(),
                            ) {
                                Ok(r) => {
                                    tracing::info!(
                                        target: "continuation",
                                        request_id = %cont.approval_request_id,
                                        result_preview = %r.chars().take(100).collect::<String>(),
                                        "Approved action executed successfully"
                                    );
                                    r
                                },
                                Err(e) => {
                                    tracing::error!(
                                        target: "continuation",
                                        request_id = %cont.approval_request_id,
                                        error = %e,
                                        "Failed to execute approved action"
                                    );
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
                        );
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
                    runtime.turn_counter = cont.turn_id
                        .trim_start_matches("turn-")
                        .parse()
                        .unwrap_or(0);

                    // Delete the continuation file — we are now live.
                    let _ = crate::runtime::continuation::delete_continuation(&self.config, t_id);

                    let outcome = runtime.execute_with_history(&mut history).await?;
                    (outcome, initial_msg, None)
                } else {
                    // No continuation on disk — optionally resume from latest checkpoint.
                    let checkpoint = crate::runtime::checkpoint::load_latest_checkpoint(
                        &self.config,
                        session_id,
                    )?;
                    if let Some(checkpoint) = checkpoint {
                        if matches!(
                            checkpoint.yield_reason,
                            crate::runtime::checkpoint::YieldReason::EmergencyStop { .. }
                        ) {
                            anyhow::bail!(
                                "Cannot auto-resume session '{}' from EmergencyStop checkpoint",
                                session_id
                            );
                        }
                        if let crate::runtime::checkpoint::YieldReason::UserInputRequired {
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
                                    anyhow::bail!(
                                        "Session '{}' is waiting for user interaction '{}'; answer it before spawning",
                                        session_id,
                                        iid
                                    );
                                }
                                UserInteractionStatus::Cancelled | UserInteractionStatus::Expired => {
                                    anyhow::bail!(
                                        "User interaction '{}' is {:?}; cannot resume from checkpoint",
                                        iid,
                                        interaction.status
                                    );
                                }
                                UserInteractionStatus::Answered => {
                                    resume_answered_user_interaction_from_loaded_checkpoint(
                                        &mut runtime,
                                        session_id,
                                        message,
                                        checkpoint,
                                        &interaction,
                                    )
                                    .await?
                                }
                            }
                        } else if should_auto_resume_checkpoint_yield_reason(&checkpoint.yield_reason)
                        {
                            tracing::info!(
                                target: "checkpoint",
                                agent_id = %runtime.manifest.agent.id,
                                session_id = %session_id,
                                turn_counter = checkpoint.turn_counter,
                                yield_reason = ?checkpoint.yield_reason,
                                "Resuming session from latest checkpoint"
                            );
                            runtime.guard = crate::runtime::guard::LoopGuard::restore(
                                checkpoint.loop_guard_state.clone(),
                            );
                            runtime.session_started = true;
                            runtime.turn_counter = checkpoint.turn_counter;
                            runtime.runtime_lock_hash = checkpoint.runtime_lock_hash.clone();

                            let mut history = checkpoint.history.clone();
                            history.push(Message::user(message.to_string()));
                            let initial_msg = checkpoint
                                .history
                                .iter()
                                .find(|m| matches!(m.role, crate::llm::Role::User))
                                .map(|m| m.content.clone())
                                .unwrap_or_default();

                            let outcome = runtime.execute_with_history(&mut history).await?;
                            (outcome, initial_msg, Some(checkpoint.turn_id))
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
                                runtime.manifest.response_contract.as_ref(),
                            );
                            let outcome = runtime.execute_with_history(&mut history).await?;
                            (outcome, runtime.initial_user_message.clone(), None)
                        }
                    } else {
                        let mut history = build_initial_history(
                            &runtime.agent_dir,
                            &runtime.instructions,
                            &runtime.initial_user_message,
                            session_id,
                            runtime.manifest.response_contract.as_ref(),
                        );
                        let outcome = runtime.execute_with_history(&mut history).await?;
                        (outcome, runtime.initial_user_message.clone(), None)
                    }
                }
            } else {
                let checkpoint =
                    crate::runtime::checkpoint::load_latest_checkpoint(&self.config, session_id)?;
                if let Some(checkpoint) = checkpoint {
                    if matches!(
                        checkpoint.yield_reason,
                        crate::runtime::checkpoint::YieldReason::EmergencyStop { .. }
                    ) {
                        anyhow::bail!(
                            "Cannot auto-resume session '{}' from EmergencyStop checkpoint",
                            session_id
                        );
                    }
                    if let crate::runtime::checkpoint::YieldReason::UserInputRequired {
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
                                anyhow::bail!(
                                    "Session '{}' is waiting for user interaction '{}'; answer it before spawning",
                                    session_id,
                                    iid
                                );
                            }
                            UserInteractionStatus::Cancelled | UserInteractionStatus::Expired => {
                                anyhow::bail!(
                                    "User interaction '{}' is {:?}; cannot resume from checkpoint",
                                    iid,
                                    interaction.status
                                );
                            }
                            UserInteractionStatus::Answered => {
                                resume_answered_user_interaction_from_loaded_checkpoint(
                                    &mut runtime,
                                    session_id,
                                    message,
                                    checkpoint,
                                    &interaction,
                                )
                                .await?
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
                        runtime.guard = crate::runtime::guard::LoopGuard::restore(
                            checkpoint.loop_guard_state.clone(),
                        );
                        runtime.session_started = true;
                        runtime.turn_counter = checkpoint.turn_counter;
                        runtime.runtime_lock_hash = checkpoint.runtime_lock_hash.clone();

                        let mut history = checkpoint.history.clone();
                        history.push(Message::user(message.to_string()));
                        let initial_msg = checkpoint
                            .history
                            .iter()
                            .find(|m| matches!(m.role, crate::llm::Role::User))
                            .map(|m| m.content.clone())
                            .unwrap_or_default();

                        let outcome = runtime.execute_with_history(&mut history).await?;
                        (outcome, initial_msg, Some(checkpoint.turn_id))
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
                            runtime.manifest.response_contract.as_ref(),
                        );
                        let outcome = runtime.execute_with_history(&mut history).await?;
                        (outcome, runtime.initial_user_message.clone(), None)
                    }
                } else {
                    let mut history = build_initial_history(
                        &runtime.agent_dir,
                        &runtime.instructions,
                        &runtime.initial_user_message,
                        session_id,
                        runtime.manifest.response_contract.as_ref(),
                    );
                    let outcome = runtime.execute_with_history(&mut history).await?;
                    (outcome, runtime.initial_user_message.clone(), None)
                }
            };

            let resolved_session_id = runtime
                .session_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("runtime session_id missing after execution"))?;

            let (assistant_reply, suspended_for_approval) = match outcome {
                TurnOutcome::Completed(reply) => (reply, None),
                TurnOutcome::Suspended { approval_request_id, .. } => {
                    // Continuation already saved by execute_with_history.
                    (None, Some(approval_request_id))
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
            let close_reason = if suspended_for_approval.is_some() {
                "jsonrpc_spawn_suspended_approval"
            } else if assistant_reply.is_some() {
                "jsonrpc_spawn_complete"
            } else {
                "jsonrpc_spawn_complete_empty"
            };
            let digest_turn_count = runtime.turn_counter;
            runtime.close_session(close_reason)?;
            crate::runtime::post_session_digest::maybe_run_post_session_digest(
                self.config.as_ref(),
                &self.config.agents_dir.join(".gateway"),
                self.gateway_store.as_ref(),
                &self.http_client,
                &resolved_session_id,
                agent_id,
                digest_turn_count,
                suspended_for_approval.is_some(),
            )
            .await;
            let llm_usage = runtime.take_llm_usage_last_run();

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
            );

            Ok(SpawnResult {
                agent_id: agent_id.to_string(),
                session_id: resolved_session_id,
                assistant_reply,
                should_signal_background,
                artifacts,
                files,
                shared_knowledge,
                llm_usage,
                suspended_for_approval,
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

        // Response validation gate: check the result against the response contract declared
        // in spawn metadata; run bounded repair loop when repair_enabled is set.
        // Fallback: when the caller supplies no metadata contract, use the contract declared
        // in the agent's own SKILL.md frontmatter (loaded via AgentRepository).
        // Validation is skipped for suspended sessions (they haven't finished producing output).
        if self.config.response_validation.enabled && result.suspended_for_approval.is_none() {
            // Resolve effective contract: caller-supplied metadata first, then manifest default.
            let manifest_contract: Option<serde_json::Value> = if metadata
                .and_then(|m| m.get("response_contract"))
                .is_none()
            {
                AgentRepository::from_config(&self.config)
                    .get_sync(agent_id)
                    .ok()
                    .and_then(|loaded| loaded.manifest.response_contract)
            } else {
                None
            };
            let effective_metadata: Option<serde_json::Value> = if manifest_contract.is_some() {
                Some(serde_json::json!({ "response_contract": manifest_contract }))
            } else {
                None
            };
            let metadata_ref: Option<&serde_json::Value> = effective_metadata
                .as_ref()
                .or(metadata);
            match crate::runtime::response_validation::parse_response_contract(metadata_ref) {
                Ok(Some(contract)) => {
                    result = self
                        .validate_and_maybe_repair(
                            agent_id,
                            result,
                            &contract,
                            source_agent_id,
                            workflow_id,
                            task_id,
                        )
                        .await?;
                }
                Ok(None) => {} // no contract in metadata — skip validation
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "invalid response_contract in metadata: {}",
                        e
                    ));
                }
            }
        }

        // Promotion record gate: if metadata requires a promotion.record, verify
        // the PromotionStore has a matching record before returning the result.
        // Two failure modes:
        //   1. promotion_record_missing — agent forgot to call promotion.record → repairable
        //   2. promotion_record_failed  — evaluator/auditor passed=false → terminal
        if result.suspended_for_approval.is_none() {
            let require_promotion = metadata
                .and_then(|m| m.get("require_promotion_record"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if require_promotion {
                let promotion_artifact_id = metadata
                    .and_then(|m| m.get("promotion_artifact_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let promotion_role = metadata
                    .and_then(|m| m.get("promotion_role"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("evaluator");

                let gateway_dir = self.config.agents_dir.join(".gateway");
                let promotion_violations =
                    crate::runtime::response_validation::validate_promotion_record(
                        Some(&gateway_dir),
                        promotion_artifact_id,
                        promotion_role,
                    );

                if !promotion_violations.is_empty() {
                    let repair_enabled = self.config.response_validation.repair_enabled;
                    let is_missing = promotion_violations
                        .iter()
                        .any(|v| v.rule == "promotion_record_missing");

                    // pass=false is terminal — no point repairing
                    // missing record is repairable if repair is enabled
                    if is_missing && repair_enabled {
                        let max_repair_rounds: usize = 2;
                        let deadline = std::time::Instant::now()
                            + std::time::Duration::from_millis(5000);

                        for attempt in 1..=max_repair_rounds {
                            if std::time::Instant::now() >= deadline {
                                break;
                            }

                            let repair_msg = crate::runtime::response_validation::build_repair_prompt(
                                &promotion_violations,
                                attempt,
                                max_repair_rounds,
                            );

                            tracing::info!(
                                target: "promotion_validation",
                                agent_id = %agent_id,
                                session_id = %result.session_id,
                                attempt,
                                "promotion.record repair attempt"
                            );

                            let repaired = match self
                                .respawn_from_checkpoint(
                                    agent_id,
                                    &result.session_id,
                                    Some(&repair_msg),
                                    source_agent_id,
                                    workflow_id,
                                    task_id,
                                )
                                .await
                            {
                                Ok(r) => r,
                                Err(e) => {
                                    tracing::warn!(
                                        target: "promotion_validation",
                                        agent_id = %agent_id,
                                        error = %e,
                                        "promotion.record repair: respawn failed"
                                    );
                                    break;
                                }
                            };

                            if repaired.suspended_for_approval.is_some() {
                                break;
                            }

                            let remaining = crate::runtime::response_validation::validate_promotion_record(
                                Some(&gateway_dir),
                                promotion_artifact_id,
                                promotion_role,
                            );
                            result = repaired;

                            if remaining.is_empty() {
                                tracing::info!(
                                    target: "promotion_validation",
                                    agent_id = %agent_id,
                                    session_id = %result.session_id,
                                    attempt,
                                    "promotion.record repair succeeded"
                                );
                                return Ok(result);
                            }

                            // If the agent recorded pass=false, stop repairing
                            if remaining.iter().any(|v| v.rule == "promotion_record_failed") {
                                break;
                            }
                        }
                    }

                    // Final failure — return error with violations
                    let summary: String = promotion_violations
                        .iter()
                        .map(|v| format!("[{}] {}", v.rule, v.message))
                        .collect::<Vec<_>>()
                        .join("; ");
                    let hints: String = promotion_violations
                        .iter()
                        .map(|v| v.repair_hint.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(anyhow::anyhow!(
                        "execution — {} Repair hints: {}",
                        summary,
                        hints
                    ));
                }
            }
        }

        Ok(result)
    }

    /// Validate a `SpawnResult` against a `ResponseContract` and, when repair is enabled,
    /// re-enter the child agent session (via its hibernation checkpoint) to give the agent
    /// a bounded number of repair attempts.
    ///
    /// Repair feedback is injected as a structured user message so the agent can call its
    /// normal tools (`content.write`, `artifact.build`, etc.) to fix the issue.  After each
    /// repair turn the fresh durable state is re-collected and re-validated.
    ///
    /// Hard guards enforced:
    /// - `contract.validation_max_loops` — total rounds including the initial execution
    ///   (repair attempts = max_loops - 1; default: 1, meaning no repair).
    /// - `contract.validation_max_duration_ms` — wall-clock deadline across all repair rounds.
    /// - Session suspension during repair (approval gate / user.ask) → repair aborted.
    async fn validate_and_maybe_repair(
        &self,
        agent_id: &str,
        mut result: SpawnResult,
        contract: &autonoetic_types::agent::ResponseContract,
        source_agent_id: Option<&str>,
        workflow_id: Option<&str>,
        task_id: Option<&str>,
    ) -> anyhow::Result<SpawnResult> {
        use crate::runtime::response_validation::{
            build_repair_prompt, validate_session_evidence, validate_spawn_response,
            violations_to_final_error,
        };

        let max_loops = (contract.validation_max_loops as usize).max(1);
        let max_duration_ms = contract.validation_max_duration_ms;
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(max_duration_ms as u64);
        let repair_enabled = self.config.response_validation.repair_enabled;

        // Initial validation.
        let gateway_dir = self.config.agents_dir.join(".gateway");

        let mut violations = validate_spawn_response(&result, contract, Some(&gateway_dir));
        violations.extend(validate_session_evidence(
            self.gateway_store.as_deref(),
            &result.session_id,
            contract,
        ));
        if violations.is_empty() {
            tracing::debug!(
                target: "response_validation",
                agent_id = %agent_id,
                session_id = %result.session_id,
                "response.validation.pass"
            );
            return Ok(result);
        }

        tracing::warn!(
            target: "response_validation",
            agent_id = %agent_id,
            session_id = %result.session_id,
            violation_count = violations.len(),
            "response.validation.fail"
        );

        // When repair is disabled or only one loop is allowed, fail immediately.
        // Include session context in the error when repair mode is on so the caller
        // can identify the session for higher-level recovery.
        if !repair_enabled || max_loops <= 1 {
            return Err(violations_to_final_error(
                &violations,
                &result.session_id,
                repair_enabled, // include context when repair mode active
            ));
        }

        // Repair loop: attempt up to (max_loops - 1) rounds.
        let max_repair_rounds = max_loops - 1;
        for attempt in 1..=max_repair_rounds {
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    target: "response_validation",
                    agent_id = %agent_id,
                    attempt = attempt,
                    "response.repair.exhausted: deadline reached"
                );
                return Err(violations_to_final_error(&violations, &result.session_id, true));
            }

            let repair_msg = build_repair_prompt(&violations, attempt, max_repair_rounds);

            tracing::info!(
                target: "response_validation",
                agent_id = %agent_id,
                session_id = %result.session_id,
                attempt = attempt,
                max_repair_rounds = max_repair_rounds,
                "response.repair.start"
            );

            let repaired = match self
                .respawn_from_checkpoint(
                    agent_id,
                    &result.session_id,
                    Some(&repair_msg),
                    source_agent_id,
                    workflow_id,
                    task_id,
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        target: "response_validation",
                        agent_id = %agent_id,
                        error = %e,
                        "response.repair.error: respawn failed"
                    );
                    return Err(violations_to_final_error(&violations, &result.session_id, true));
                }
            };

            // If the agent suspended for approval during repair we cannot continue the
            // repair loop — abort and surface the original violations.
            if repaired.suspended_for_approval.is_some() {
                tracing::warn!(
                    target: "response_validation",
                    agent_id = %agent_id,
                    "response.repair.aborted: session suspended for approval during repair"
                );
                return Err(anyhow::anyhow!(
                    "repair aborted: agent suspended for approval during repair; session: {}",
                    result.session_id
                ));
            }

            // Check if the agent ended with a user interaction (user.ask) suspension.
            // If the latest checkpoint is UserInputRequired (not yet answered), abort repair.
            if let Ok(Some(cp)) =
                crate::runtime::checkpoint::load_latest_checkpoint(&self.config, &repaired.session_id)
            {
                if matches!(
                    cp.yield_reason,
                    crate::runtime::checkpoint::YieldReason::UserInputRequired { .. }
                ) {
                    tracing::warn!(
                        target: "response_validation",
                        agent_id = %agent_id,
                        session_id = %repaired.session_id,
                        "response.repair.aborted: session suspended for user interaction during repair"
                    );
                    return Err(anyhow::anyhow!(
                        "repair aborted: agent suspended for user interaction during repair; session: {}",
                        result.session_id
                    ));
                }
            }

            // Check deadline after respawn completes (hard enforcement post-respawn).
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    target: "response_validation",
                    agent_id = %agent_id,
                    attempt = attempt,
                    "response.repair.exhausted: deadline reached after respawn"
                );
                return Err(violations_to_final_error(&violations, &result.session_id, true));
            }

            // Re-validate against the fresh state returned by respawn_from_checkpoint.
            violations = validate_spawn_response(&repaired, contract, Some(&gateway_dir));
            violations.extend(validate_session_evidence(
                self.gateway_store.as_deref(),
                &repaired.session_id,
                contract,
            ));
            result = repaired;

            if violations.is_empty() {
                tracing::info!(
                    target: "response_validation",
                    agent_id = %agent_id,
                    session_id = %result.session_id,
                    attempt = attempt,
                    "response.repair.pass"
                );
                return Ok(result);
            }

            tracing::warn!(
                target: "response_validation",
                agent_id = %agent_id,
                attempt = attempt,
                violation_count = violations.len(),
                "response.repair.fail"
            );
        }

        tracing::warn!(
            target: "response_validation",
            agent_id = %agent_id,
            "response.repair.exhausted: max_loops reached"
        );
        Err(violations_to_final_error(&violations, &result.session_id, true))
    }

    /// Resume execution after a `user.ask` interaction was answered in the gateway store.
    ///
    /// Validates the latest session checkpoint is a `UserInputRequired` yield for this
    /// `interaction_id`, then runs the normal spawn path which injects the stored answer as the
    /// pending `user.ask` tool result and continues the agent loop.
    pub async fn resume_from_user_interaction(
        &self,
        interaction_id: &str,
        follow_up_user_message: Option<&str>,
    ) -> anyhow::Result<SpawnResult> {
        use crate::runtime::checkpoint::{load_latest_checkpoint, YieldReason};

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

        let checkpoint = load_latest_checkpoint(self.config.as_ref(), &interaction.session_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("No checkpoint for session '{}'", interaction.session_id)
            })?;

        match &checkpoint.yield_reason {
            YieldReason::UserInputRequired {
                interaction_id: cid,
            } => {
                anyhow::ensure!(
                    cid == &interaction.interaction_id,
                    "Checkpoint is for interaction '{}', not '{}'",
                    cid,
                    interaction.interaction_id
                );
            }
            other => {
                anyhow::bail!(
                    "Latest checkpoint for session '{}' is not UserInputRequired (got {:?})",
                    interaction.session_id,
                    other
                );
            }
        }

        self.spawn_agent_once(
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
        )
        .await
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
        let loaded = repo.get_sync(agent_id)?;

        let llm_config = loaded
            .manifest
            .llm_config
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' is missing llm_config", agent_id))?;
        let driver = build_driver(llm_config, self.http_client.clone())?;

        let openrouter_catalog = Arc::new(OpenRouterCatalog::new(self.http_client.clone()));
        let middleware = loaded.manifest.middleware.clone().unwrap_or_default();
        let mut runtime = AgentExecutor::new(
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
        .with_openrouter_catalog(Some(openrouter_catalog))
        .with_middleware(middleware)
        .with_session_id(session_id.to_string())
        .with_workflow_context(workflow_id.map(String::from), task_id.map(String::from))
        .with_active_executions(Some(self.active_executions.clone()));

        // Restore executor state from checkpoint
        runtime.guard =
            crate::runtime::guard::LoopGuard::restore(checkpoint.loop_guard_state.clone());
        runtime.session_started = true;
        runtime.turn_counter = checkpoint.turn_counter;
        runtime.runtime_lock_hash = checkpoint.runtime_lock_hash.clone();

        // Build history from checkpoint, optionally appending an additional message
        let mut history = checkpoint.history.clone();
        if let Some(msg) = additional_message {
            history.push(Message::user(msg));
        }

        let outcome = runtime.execute_with_history(&mut history).await?;

        let resolved_session_id = runtime
            .session_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("runtime session_id missing after execution"))?;

        let (assistant_reply, suspended_for_approval) = match outcome {
            TurnOutcome::Completed(reply) => (reply, None),
            TurnOutcome::Suspended {
                approval_request_id,
                ..
            } => (None, Some(approval_request_id)),
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
        let close_reason = if suspended_for_approval.is_some() {
            "checkpoint_respawn_suspended"
        } else if assistant_reply.is_some() {
            "checkpoint_respawn_complete"
        } else {
            "checkpoint_respawn_complete_empty"
        };
        let digest_turn_count = runtime.turn_counter;
        runtime.close_session(close_reason)?;
        crate::runtime::post_session_digest::maybe_run_post_session_digest(
            self.config.as_ref(),
            &self.config.agents_dir.join(".gateway"),
            self.gateway_store.as_ref(),
            &self.http_client,
            &resolved_session_id,
            agent_id,
            digest_turn_count,
            suspended_for_approval.is_some(),
        )
        .await;
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

        Ok(SpawnResult {
            agent_id: agent_id.to_string(),
            session_id: resolved_session_id,
            assistant_reply,
            should_signal_background: false,
            artifacts,
            files,
            shared_knowledge,
            llm_usage,
            suspended_for_approval,
        })
    }

    pub async fn execute_background_action(
        &self,
        agent_id: &str,
        _session_id: &str,
        action: &ScheduledAction,
    ) -> anyhow::Result<String> {
        self.execute_with_reliability_controls(agent_id, || async move {
            let (manifest, agent_dir) = self.load_agent_manifest(agent_id)?;
            execute_scheduled_action(
                &manifest,
                &agent_dir,
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
        let loaded = repo.get_sync(agent_id)?;
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

/// Logs agent.spawn.requested and agent.spawn.completed to the gateway causal chain for nested
/// delegations (when source_agent_id is set), so the gateway log shows the full delegation tree.
fn log_nested_spawn_to_gateway(
    config: &GatewayConfig,
    session_id: &str,
    source_agent_id: Option<&str>,
    agent_id: &str,
    message: &str,
    result: &SpawnResult,
) {
    let logger = match init_gateway_causal_logger(config) {
        Ok(l) => l,
        Err(_) => return,
    };
    let path = logger.path().to_path_buf();
    let entries = match CausalLogger::read_entries(&path) {
        Ok(e) => e,
        Err(err) => {
            if path.exists() {
                tracing::warn!(
                    error = %err,
                    "Failed to read existing gateway causal entries before input schema log"
                );
                return;
            }
            Vec::new()
        }
    };
    let mut seq = entries.last().map(|e| e.event_seq + 1).unwrap_or(1);
    let requested_data = serde_json::json!({
        "agent_id": agent_id,
        "source_agent_id": source_agent_id,
        "session_id": session_id,
        "message_len": message.len(),
        "message_sha256": sha256_hex(message),
    });
    log_gateway_causal_event(
        &logger,
        &gateway_actor_id(),
        session_id,
        seq,
        "agent.spawn.requested",
        EntryStatus::Success,
        Some(requested_data),
    );
    seq += 1;
    let completed_data = serde_json::json!({
        "agent_id": result.agent_id,
        "source_agent_id": source_agent_id,
        "session_id": result.session_id,
        "assistant_reply_len": result.assistant_reply.as_ref().map(|s| s.len()).unwrap_or(0),
        "assistant_reply_sha256": result.assistant_reply.as_ref().map(|s| sha256_hex(s)),
        "llm_usage": result.llm_usage,
    });
    log_gateway_causal_event(
        &logger,
        &gateway_actor_id(),
        session_id,
        seq,
        "agent.spawn.completed",
        EntryStatus::Success,
        Some(completed_data),
    );
}

fn log_input_schema_validation_to_gateway(
    _config: &GatewayConfig,
    _session_id: &str,
    _source_agent_id: Option<&str>,
    _agent_id: &str,
    _message: &str,
    _validation: &SchemaValidation,
) -> anyhow::Result<()> {
    // No-op: gateway causal chain events are now captured in gateway.db
    Ok(())
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

/// [DEPRECATED] This function is no longer called as gateway causal chain events are now captured in gateway.db.
fn _deprecated_update_session_index(
    logger: &CausalLogger,
    actor_id: &str,
    session_id: &str,
    event_seq: u64,
    action: &str,
    status: &EntryStatus,
    payload: Option<&serde_json::Value>,
) -> anyhow::Result<()> {
    let index_path = logger
        .path()
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Logger path has no parent"))?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Logger path has no grandparent"))?
        .join("sessions")
        .join(session_id)
        .join("index.json");

    let mut index = if index_path.exists() {
        serde_json::from_str::<SessionIndex>(&std::fs::read_to_string(&index_path)?)?
    } else {
        SessionIndex {
            session_id: session_id.to_string(),
            first_timestamp: None,
            last_timestamp: None,
            events: vec![],
        }
    };

    let timestamp = chrono::Utc::now().to_rfc3339();
    if index.first_timestamp.is_none() {
        index.first_timestamp = Some(timestamp.clone());
    }
    index.last_timestamp = Some(timestamp.clone());

    let log_id = format!("{}:{}:{}", actor_id, session_id, event_seq);

    let event_ref = SessionEventRef {
        log_id: log_id.clone(),
        agent_id: actor_id.to_string(),
        timestamp: timestamp.clone(),
        category: "gateway".to_string(),
        action: action.to_string(),
        status: status.clone(),
        causal_hash: payload
            .and_then(|p| p.get("causal_hash").and_then(|h| h.as_str()))
            .map(String::from),
    };
    index.events.push(event_ref);

    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&index_path, serde_json::to_string_pretty(&index)?)?;

    Ok(())
}

/// [DEPRECATED] This struct is no longer used as gateway causal chain events are now captured in gateway.db.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionIndex {
    session_id: String,
    first_timestamp: Option<String>,
    last_timestamp: Option<String>,
    events: Vec<SessionEventRef>,
}

/// [DEPRECATED] This struct is no longer used as gateway causal chain events are now captured in gateway.db.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionEventRef {
    log_id: String,
    agent_id: String,
    timestamp: String,
    category: String,
    action: String,
    status: EntryStatus,
    causal_hash: Option<String>,
}

pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn should_auto_resume_checkpoint_yield_reason(
    yield_reason: &crate::runtime::checkpoint::YieldReason,
) -> bool {
    use crate::runtime::checkpoint::YieldReason;
    matches!(
        yield_reason,
        YieldReason::Hibernation
            | YieldReason::BudgetExhausted
            | YieldReason::MaxTurnsReached
            | YieldReason::ManualStop
            | YieldReason::Error(_)
    )
}

fn build_user_ask_answer_tool_result_json(interaction: &UserInteraction) -> anyhow::Result<String> {
    if interaction.status != UserInteractionStatus::Answered {
        anyhow::bail!(
            "user interaction {} is not answered ({:?})",
            interaction.interaction_id,
            interaction.status
        );
    }
    let selected_value = match &interaction.answer_option_id {
        Some(oid) => interaction
            .options
            .iter()
            .find(|o| &o.id == oid)
            .map(|o| o.value.clone()),
        None => None,
    };
    Ok(serde_json::json!({
        "ok": true,
        "interaction_id": interaction.interaction_id,
        "status": "answered",
        "question": interaction.question,
        "kind": interaction.kind.as_str(),
        "answer_text": interaction.answer_text,
        "answer_option_id": interaction.answer_option_id,
        "selected_value": selected_value,
    })
    .to_string())
}

fn resolve_pending_user_ask_call(
    checkpoint: &crate::runtime::checkpoint::SessionCheckpoint,
) -> anyhow::Result<(String, String)> {
    if let Some(ref pts) = checkpoint.pending_tool_state {
        return Ok((
            pts.pending_tool_call.call_id.clone(),
            pts.pending_tool_call.tool_name.clone(),
        ));
    }
    pending_user_ask_call_from_history(&checkpoint.history)
}

fn pending_user_ask_call_from_history(history: &[Message]) -> anyhow::Result<(String, String)> {
    use crate::llm::Role;
    let i = history
        .iter()
        .rposition(|m| matches!(m.role, Role::Assistant) && !m.tool_calls.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("checkpoint history has no assistant message with tool calls")
        })?;
    let assistant = &history[i];
    let mut j = i + 1;
    let mut tc_idx = 0usize;
    while tc_idx < assistant.tool_calls.len() && j < history.len() {
        let m = &history[j];
        if matches!(m.role, Role::Tool)
            && m.tool_call_id.as_deref() == Some(assistant.tool_calls[tc_idx].id.as_str())
        {
            tc_idx += 1;
            j += 1;
        } else {
            break;
        }
    }
    if tc_idx >= assistant.tool_calls.len() {
        anyhow::bail!("checkpoint history has no pending tool call (batch missing result)");
    }
    let tc = &assistant.tool_calls[tc_idx];
    if tc.name != "user.ask" {
        anyhow::bail!(
            "expected pending tool user.ask for UserInputRequired checkpoint, found {}",
            tc.name
        );
    }
    Ok((tc.id.clone(), tc.name.clone()))
}

fn inject_answered_user_interaction_into_history(
    history: &mut Vec<Message>,
    checkpoint: &crate::runtime::checkpoint::SessionCheckpoint,
    interaction: &UserInteraction,
) -> anyhow::Result<()> {
    let (call_id, tool_name) = resolve_pending_user_ask_call(checkpoint)?;
    let json = build_user_ask_answer_tool_result_json(interaction)?;
    history.push(Message::tool_result(call_id, tool_name, json));
    Ok(())
}

async fn resume_answered_user_interaction_from_loaded_checkpoint(
    runtime: &mut AgentExecutor,
    session_id: &str,
    message: &str,
    checkpoint: crate::runtime::checkpoint::SessionCheckpoint,
    interaction: &UserInteraction,
) -> anyhow::Result<(
    crate::runtime::lifecycle::TurnOutcome,
    String,
    Option<String>,
)> {
    anyhow::ensure!(
        interaction.session_id == session_id,
        "interaction session_id '{}' does not match spawn session_id '{}'",
        interaction.session_id,
        session_id
    );
    anyhow::ensure!(
        interaction.agent_id == runtime.manifest.agent.id,
        "interaction agent_id '{}' does not match spawned agent '{}'",
        interaction.agent_id,
        runtime.manifest.agent.id
    );

    let yield_iid = match &checkpoint.yield_reason {
        crate::runtime::checkpoint::YieldReason::UserInputRequired { interaction_id } => {
            interaction_id.clone()
        }
        _ => anyhow::bail!("checkpoint yield reason is not UserInputRequired"),
    };
    anyhow::ensure!(
        yield_iid == interaction.interaction_id,
        "checkpoint interaction_id '{}' does not match row '{}'",
        yield_iid,
        interaction.interaction_id
    );

    tracing::info!(
        target: "user_interaction",
        session_id = %session_id,
        interaction_id = %interaction.interaction_id,
        "Resuming session from user.ask checkpoint with stored answer"
    );

    runtime.guard = crate::runtime::guard::LoopGuard::restore(checkpoint.loop_guard_state.clone());
    runtime.session_started = true;
    runtime.turn_counter = checkpoint.turn_counter;
    runtime.runtime_lock_hash = checkpoint.runtime_lock_hash.clone();

    let mut history = checkpoint.history.clone();
    inject_answered_user_interaction_into_history(&mut history, &checkpoint, interaction)?;
    if let Some(gw) = runtime.gateway_dir.as_ref() {
        let base = base_session_id(session_id).to_string();
        let answer_summary = match (
            interaction.answer_text.as_deref(),
            interaction.answer_option_id.as_deref(),
        ) {
            (Some(t), _) if !t.trim().is_empty() => t.trim().to_string(),
            (_, Some(oid)) if !oid.is_empty() => format!("selected option `{oid}`"),
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
        history.push(Message::user(message.to_string()));
    }

    let initial_msg = checkpoint
        .history
        .iter()
        .find(|m| matches!(m.role, crate::llm::Role::User))
        .map(|m| m.content.clone())
        .unwrap_or_default();

    let outcome = runtime.execute_with_history(&mut history).await?;
    Ok((outcome, initial_msg, Some(checkpoint.turn_id)))
}

fn build_initial_history(
    agent_dir: &std::path::Path,
    instructions: &str,
    user_message: &str,
    session_id: &str,
    response_contract: Option<&serde_json::Value>,
) -> Vec<Message> {
    let mut history = vec![Message::system(
        crate::runtime::lifecycle::compose_system_instructions_with_metadata(instructions, response_contract)
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
    history.push(Message::user(user_message.to_string()));
    history
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

        let history = build_initial_history(
            temp.path(),
            "System prompt",
            "What did I ask you to remember?",
            "session-1",
            None,
        );

        assert_eq!(history.len(), 3);
        assert_eq!(history[0].role.as_str(), "system");
        assert_eq!(history[2].role.as_str(), "user");
        assert!(history[0]
            .content
            .contains("Autonoetic Gateway Foundation Rules"));
        assert!(history[0].content.contains("System prompt"));
        assert!(history[1]
            .content
            .contains("Last user message: remember Atlas"));
        assert!(history[1]
            .content
            .contains("Last assistant reply: Stored that."));
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
    fn test_log_input_schema_validation_to_gateway_is_noop() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let mut config = GatewayConfig::default();
        config.agents_dir = temp.path().join("agents");

        let validation = SchemaValidation {
            valid: false,
            issues: vec!["Missing required field: 'query'".to_string()],
        };
        log_input_schema_validation_to_gateway(
            &config,
            "session-3",
            Some("planner.default"),
            "researcher.default",
            "plain text query",
            &validation,
        )
        .expect("schema validation event should log (no-op now)");

        // Gateway causal chain is no longer used - function is a no-op
        // Relevant data is captured in gateway.db causal_events table via SessionTracer
    }
}

/// Execute a script agent directly in sandbox, bypassing the LLM.
async fn execute_script_in_sandbox(
    agent_dir: &PathBuf,
    script_path: &PathBuf,
    input_payload: &str,
    sandbox_type: &str,
    _config: &GatewayConfig,
    sandbox_kill: Option<(
        std::sync::Arc<crate::runtime::active_execution_registry::ActiveExecutionRegistry>,
        String,
    )>,
    capabilities: &[autonoetic_types::capability::Capability],
) -> anyhow::Result<String> {
    use std::io::Write;

    tracing::info!(
        agent_dir = %agent_dir.display(),
        script = %script_path.display(),
        sandbox = %sandbox_type,
        "Executing script agent"
    );

    let driver = crate::sandbox::SandboxDriverKind::parse(sandbox_type)?;
    let overrides = crate::sandbox::BwrapIsolationOverrides::from_capabilities(capabilities);
    let entrypoint = script_path.to_string_lossy().to_string();

    let mut runner = crate::sandbox::SandboxRunner::spawn_with_driver_and_dependencies(
        driver,
        &agent_dir.to_string_lossy(),
        &entrypoint,
        None,
        Some(&overrides),
    )?;

    let _script_sandbox_guard = sandbox_kill.as_ref().and_then(|(reg, root)| {
        let pid = runner.process.id();
        (pid > 0).then(|| reg.register_sandbox_child_pid(root, pid))
    });

    if let Some(mut stdin) = runner.process.stdin.take() {
        stdin
            .write_all(input_payload.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to write to script stdin: {}", e))?;
    }

    let output = tokio::task::spawn_blocking(move || {
        runner.process.wait_with_output()
    })
    .await
    .map_err(|e| anyhow::anyhow!("Task join error: {}", e))?
    .map_err(|e| anyhow::anyhow!("Failed to execute script: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        tracing::error!(stderr = %stderr, stdout = %stdout, status = ?output.status.code(), "Script execution failed");
        anyhow::bail!(
            "Script execution failed with code {:?}: stdout={}, stderr={}",
            output.status.code(),
            stdout,
            stderr
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    tracing::info!(stdout_len = stdout.len(), "Script execution completed");

    Ok(stdout)
}

#[cfg(test)]
#[test]
fn pending_user_ask_call_from_history_finds_first_missing_result() {
    use crate::llm::ToolCall;
    let mut a = Message::assistant("");
    a.tool_calls = vec![
        ToolCall {
            id: "c1".into(),
            name: "noop".into(),
            arguments: "{}".into(),
        },
        ToolCall {
            id: "c2".into(),
            name: "user.ask".into(),
            arguments: "{}".into(),
        },
    ];
    let history = vec![
        Message::user("hi"),
        a,
        Message::tool_result("c1", "noop", r#"{"ok":true}"#),
    ];
    let (id, name) = pending_user_ask_call_from_history(&history).unwrap();
    assert_eq!(id, "c2");
    assert_eq!(name, "user.ask");
}

#[cfg(test)]
#[test]
fn resolve_pending_prefers_checkpoint_pending_tool_state() {
    use crate::runtime::checkpoint::{
        PendingToolCall, PendingToolState, SessionCheckpoint, YieldReason,
    };
    use crate::runtime::guard::LoopGuardState;
    let pts = PendingToolState {
        completed_tool_results: vec![],
        pending_tool_call: PendingToolCall {
            call_id: "tid-99".into(),
            tool_name: "user.ask".into(),
            arguments: "{}".into(),
            approval_response: None,
        },
        remaining_tool_calls: vec![],
    };
    let cp = SessionCheckpoint {
        history: vec![],
        turn_counter: 0,
        loop_guard_state: LoopGuardState {
            max_loops_without_progress: 1,
            current_loops: 0,
            last_failure_hash: None,
            consecutive_failures: 0,
        },
        agent_id: "a".into(),
        session_id: "s".into(),
        turn_id: "turn-1".into(),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: YieldReason::UserInputRequired {
            interaction_id: "ui-x".into(),
        },
        content_store_refs: vec![],
        created_at: "".into(),
        pending_tool_state: Some(pts),
        llm_rounds_consumed: 0,
        tool_invocations_consumed: 0,
        tokens_consumed: 0,
        estimated_cost_usd: 0.0,
    };
    let (id, name) = resolve_pending_user_ask_call(&cp).unwrap();
    assert_eq!(id, "tid-99");
    assert_eq!(name, "user.ask");
}

#[cfg(test)]
#[test]
fn build_user_ask_answer_includes_selected_value() {
    use autonoetic_types::background::{
        UserInteraction, UserInteractionKind, UserInteractionStatus,
    };
    let interaction = UserInteraction {
        interaction_id: "ui-abc".into(),
        session_id: "s1".into(),
        root_session_id: "s1".into(),
        agent_id: "ag1".into(),
        turn_id: "t1".into(),
        kind: UserInteractionKind::Decision,
        question: "Pick one".into(),
        context: None,
        options: vec![autonoetic_types::background::UserInteractionOption {
            id: "opt-a".into(),
            label: "A".into(),
            value: "alpha".into(),
        }],
        allow_freeform: false,
        status: UserInteractionStatus::Answered,
        answer_option_id: Some("opt-a".into()),
        answer_text: None,
        answered_by: Some("cli".into()),
        created_at: "".into(),
        answered_at: None,
        expires_at: None,
        workflow_id: None,
        task_id: None,
        checkpoint_turn_id: None,
    };
    let json = build_user_ask_answer_tool_result_json(&interaction).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["selected_value"], "alpha");
    assert_eq!(v["answer_option_id"], "opt-a");
}
