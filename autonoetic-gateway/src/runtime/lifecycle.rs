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
use crate::runtime::context::{
    compose_system_instructions_full, inline_extended, safe_prefix_by_bytes,
    workflow_status_user_message_for_chat,
};
pub(crate) use crate::runtime::context::compose_system_instructions_with_metadata;
use crate::runtime::disclosure::DisclosureState;
use crate::runtime::guard::LoopGuard;
use crate::runtime::history_persist::persist_history_to_content_store;
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
use std::path::PathBuf;
use std::sync::Arc;

use crate::runtime::budget_tracker::{
    emit_context_pressure_high_if_warranted, input_tokens_as_context_pct,
    is_retryable_empty_other_response, max_other_empty_retries,
};
use crate::runtime::context_governor::resolver::resolve_context_window_for_run;
use crate::runtime::trajectory_monitor::{ToolObservation, TrajectoryMonitor};

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
    /// Current state capsule (Phase 2), set by CapsuleStrategy after compression.
    pub capsule_state: Option<crate::runtime::context_governor::capsule::StateCapsule>,
    /// Shared HTTP client for compression and other gateway-side operations.
    pub http_client: reqwest::Client,
    /// User ID for profile binding resolution (if authenticated).
    pub user_id: Option<String>,
    /// Artifact ID whose layers should be auto-mounted into sandbox.exec calls.
    /// Set when a parent agent spawns this agent with an artifact reference
    /// (typically for evaluator sessions that need packager's dependency layers).
    pub artifact_id: Option<String>,
    /// Previous turn's Ri-0.6 capability snapshot for narrowing checks.
    pub(crate) ri_0_6_previous_snapshot: Option<crate::runtime::tool_dispatch::Ri06CapabilitySnapshot>,
    /// Cross-agent persona text loaded from `persona.md`. Injected into every
    /// agent's system prompt between the foundation and agent-specific instructions.
    pub persona: Option<String>,
    /// When true, the context governor uses an aggressive reduction pipeline
    /// that skips CompressionStrategy and goes straight to TrimHistory.
    /// Set by the scheduler on overflow retry.
    pub overflow_recovery: bool,
    /// Optional extended instructions (after `<!-- extended -->` in SKILL.md).
    /// Written to content store for on-demand retrieval by the agent.
    pub extended_instructions: Option<String>,

    /// In-session divergence monitor (Sentinel P1). Observes LoopGuard
    /// pressure, digest stall, repetition entropy, error bursts, and
    /// context pressure. Emits `divergence.*` causal events on level
    /// transitions.
    pub trajectory_monitor: TrajectoryMonitor,

    /// Context utilization fraction from the most recent prompt budget
    /// computation. Passed into the trajectory monitor each turn.
    pub last_context_utilization: Option<f32>,
}

use crate::runtime::tool_dispatch::{
    loop_guard_from_config_and_manifest, tool_result_counts_as_progress,
};
pub use crate::runtime::tool_dispatch::determine_tool_tier_filter;

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
            capsule_state: None,
            http_client: reqwest::Client::new(),
            user_id: None,
            artifact_id: None,
            ri_0_6_previous_snapshot: None,
            persona: None,
            overflow_recovery: false,
            extended_instructions: None,
            trajectory_monitor: TrajectoryMonitor::new(Default::default()),
            last_context_utilization: None,
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
        self.trajectory_monitor = TrajectoryMonitor::new(config.trajectory.clone());
        self.config = Some(config);
        self
    }

    pub fn with_persona(mut self, persona: Option<String>) -> Self {
        self.persona = persona;
        self
    }

    /// Phase 3: Enable aggressive context governor pipeline for overflow retry.
    pub fn with_overflow_recovery(mut self, enabled: bool) -> Self {
        self.overflow_recovery = enabled;
        self
    }

    /// Set optional extended instructions for on-demand retrieval.
    pub fn with_extended_instructions(mut self, instructions: Option<String>) -> Self {
        self.extended_instructions = instructions;
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

    /// Set the session's purpose state at creation. Use
    /// `SessionState::Clarification` for ask-agent child spawns so the tool
    /// tier is clamped read-only from the first turn — see
    /// `docs/design/human-gate-unification-plan.md` §Phase 5.
    pub fn with_initial_session_state(
        mut self,
        state: autonoetic_types::agent::SessionState,
    ) -> Self {
        self.session_state = state;
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
            code_excerpts: None,
            risk_summary: None,
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

        // Attempt workflow completion when root session closes normally.
        let is_root = crate::runtime::content_store::root_session_id(&session_id) == session_id;
        if !reason.contains("suspended") && is_root {
            if let Some(cfg) = self.config.as_deref() {
                if let Err(e) = crate::scheduler::workflow_store::try_complete_workflow(
                    cfg,
                    self.gateway_store.as_deref(),
                    &session_id,
                ) {
                    tracing::warn!(
                        target: "workflow",
                        error = %e,
                        session_id = %session_id,
                        "Failed to attempt workflow completion on session close"
                    );
                }
            }
        }

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
            capsule_state: self.capsule_state.clone(),
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
        let memory_context = self.build_memory_context_snippet();
        let inlined_instructions = inline_extended(&self.instructions, self.extended_instructions.as_deref());
        let mut system_instructions = compose_system_instructions_full(
            &inlined_instructions,
            &self.manifest,
            self.manifest
                .io
                .as_ref()
                .and_then(|io| io.output_policy.as_ref()),
            user_context.as_deref(),
            self.persona.as_deref(),
        );
        if let Some(ref snippet) = memory_context {
            system_instructions.push_str("\n\n");
            system_instructions.push_str(snippet);
        }
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

        // RFC scope 5.1 refuse-boot guard: a session whose manifest declares
        // `sandbox_network: recording` cannot start unless the gateway config
        // has explicitly enabled recording. This prevents an agent's manifest
        // from silently switching on live-traffic capture.
        if matches!(
            self.manifest.sandbox_network,
            autonoetic_types::agent::SandboxNetworkPolicy::Recording
        ) {
            let allowed = self
                .config
                .as_ref()
                .map(|c| c.sandbox.allow_recording)
                .unwrap_or(false);
            anyhow::ensure!(
                allowed,
                "Session refused to start: manifest declares \
                 sandbox_network: recording, but gateway config does not \
                 enable recording (set gateway.sandbox.allow_recording: true \
                 to permit fixture-capture sessions). Agent '{}'.",
                self.manifest.agent.id
            );
        }

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
                        enforced_rules: vec!["R-7.18".to_string()],
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
            let memory_context = self.build_memory_context_snippet();
            let inlined_instructions = inline_extended(&self.instructions, self.extended_instructions.as_deref());
            let mut system_instructions = compose_system_instructions_full(
                &inlined_instructions,
                &self.manifest,
                self.manifest
                    .io
                    .as_ref()
                    .and_then(|io| io.output_policy.as_ref()),
                user_context.as_deref(),
                self.persona.as_deref(),
            );
            if let Some(ref snippet) = memory_context {
                system_instructions.push_str("\n\n");
                system_instructions.push_str(snippet);
            }
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

            let mut tools: Vec<ToolDefinition> = {
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

            // Emit pressure-high causal event before reduction so operators
            // see warnings even when no enforcement action is needed.
            emit_context_pressure_high_if_warranted(
                &budget_breakdown,
                self.config.as_ref().map(|c| &**c),
                &mut tracer,
            );

            // Stash context utilization for the trajectory monitor.
            self.last_context_utilization = budget_breakdown.utilization_pct.map(|v| v as f32);

            // --- Budget Enforcement + Context Compression (Context Governor) ---
            {
                use crate::runtime::context_governor::{
                    ContextGovernor, GovernorConfig,
                    strategies::GovernorResult,
                };
                let margin = self.config.as_ref()
                    .map(|c| c.prompt_budget.margin_tokens as usize)
                    .unwrap_or(4096);
                let effective_limit = budget_breakdown
                    .context_window
                    .map(|w| w.saturating_sub(margin))
                    .unwrap_or(budget_breakdown.total_tokens + 1);
                let compression_cfg = self.config.as_ref().map(|c| &c.context_compression);
                let mut ctx = crate::runtime::context_governor::strategies::GovernorContext::new(
                    history.clone(),
                    tools.clone(),
                    budget_breakdown.clone(),
                    effective_limit,
                    self.turn_counter.saturating_sub(1),
                    session_id.clone(),
                    Some(self.compression_metadata.clone()),
                    self.config.as_ref().map(|c| c.prompt_budget.clone())
                        .unwrap_or_default(),
                    compression_cfg.cloned(),
                    self.manifest.compression.clone(),
                );
                let governor = if self.overflow_recovery {
                    tracing::info!(
                        target: "autonoetic::context_governor",
                        "Using aggressive governor pipeline (overflow recovery)"
                    );
                    ContextGovernor::new_aggressive(&GovernorConfig {
                        http_client: self.http_client.clone(),
                        presets: self.config.as_ref().map(|c| c.llm_presets.clone())
                            .unwrap_or_default(),
                        gateway_dir: self.gateway_dir.clone(),
                    })
                } else {
                    ContextGovernor::new(&GovernorConfig {
                        http_client: self.http_client.clone(),
                        presets: self.config.as_ref().map(|c| c.llm_presets.clone())
                            .unwrap_or_default(),
                        gateway_dir: self.gateway_dir.clone(),
                    })
                };
                match governor.govern(&mut ctx).await {
                    Ok(GovernorResult::Recovered { actions_taken }) => {
                        tracing::info!(
                            target: "autonoetic::context_governor",
                            actions = ?actions_taken,
                            "ContextGovernor recovered within budget"
                        );
                        if ctx.compression_metadata.as_ref().map(|m| m.compression_count > self.compression_metadata.compression_count).unwrap_or(false) {
                            if let Some(meta) = ctx.compression_metadata.clone() {
                                self.compression_metadata = meta;
                            }
                        }
                        // Once set, capsule_state is never cleared back to None —
                        // the latest capsule represents current session compression state.
                        if ctx.capsule_state.is_some() {
                            self.capsule_state = ctx.capsule_state.clone();
                        }
                    }
                    Ok(GovernorResult::Overflow(diag)) => {
                        tracing::warn!(
                            target: "autonoetic::context_governor",
                            diagnostic = ?diag,
                            "ContextGovernor exhausted — all strategies failed"
                        );
                    }
                    Ok(GovernorResult::WithinBudget) => {}
                    Err(e) => {
                        tracing::warn!(
                            target: "autonoetic::context_governor",
                            error = %e,
                            "ContextGovernor error, falling through without reduction"
                        );
                    }
                }
                *history = ctx.history;
                tools = ctx.tools;
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

                    // ── Trajectory Monitor ──────────────────────────────────────
                    // After guard updates, recompute health and emit divergence
                    // events on level transitions.
                    {
                        use crate::runtime::trajectory_monitor::fingerprint_tool_call;
                        use crate::runtime::trajectory_health::{
                            build_event_payload, DIVERGENCE_CATEGORY,
                        };
                        use autonoetic_types::causal_chain::EntryStatus;

                        let observations: Vec<ToolObservation> = results
                            .iter()
                            .filter_map(|(id, _name, result)| {
                                let tc = response.tool_calls.iter().find(|tc| tc.id == *id)?;
                                let fp = fingerprint_tool_call(&tc.name, &tc.arguments);
                                let failed = serde_json::from_str::<serde_json::Value>(result)
                                    .ok()
                                    .and_then(|v| v.get("ok")?.as_bool())
                                    .map(|ok| !ok)
                                    .unwrap_or(false);
                                Some(ToolObservation {
                                    fingerprint: fp,
                                    failed,
                                })
                            })
                            .collect();

                        let result = self.trajectory_monitor.tick(
                            self.turn_counter,
                            &observations,
                            self.last_context_utilization,
                            &self.guard.snapshot(),
                        );

                        if result.level_changed {
                            if let Some(payload) = build_event_payload(&result.health) {
                                let action = result.health.causal_action().unwrap_or("observed");
                                if let Err(e) = tracer.log_event(
                                    DIVERGENCE_CATEGORY,
                                    action,
                                    EntryStatus::Success,
                                    Some(payload),
                                ) {
                                    tracing::warn!(
                                        target: "autonoetic::trajectory",
                                        error = %e,
                                        level = %result.health.level_str(),
                                        "Failed to log divergence event"
                                    );
                                }
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
    use crate::runtime::context::compose_foundation;
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
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
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
