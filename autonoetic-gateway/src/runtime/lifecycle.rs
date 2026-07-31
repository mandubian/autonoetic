//! Agent Execution Lifecycle.
//!
//! Manages Wake -> Context Assembly -> Reasoning -> Act -> Hibernate.

use crate::llm::{CompletionRequest, LlmDriver, Message, StopReason, ToolCall, ToolDefinition};
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
use crate::runtime::local_model_context::LocalModelContextCache;
use crate::runtime::human_gate::{
    DecisionContext, GateKind, GateRequest, GateResult, GateService, MatchStrategy,
};
use crate::runtime::reevaluation_state::persist_reevaluation_state;
use crate::runtime::session_budget::SessionBudgetRegistry;
use crate::runtime::session_tracer::{EvidenceMode, SessionTracer};
use crate::runtime::store::SecretStoreRuntime;
use crate::runtime::tool_call_processor::ToolCallProcessor;
use autonoetic_types::agent::{AgentManifest, LlmExchangeUsage, Middleware};
use autonoetic_types::background::ScheduledAction;
use autonoetic_types::config::{GatewayConfig, TrajectoryConfig};
use autonoetic_types::disclosure::DisclosurePolicy;
use autonoetic_types::session_outcome::SessionCloseOutcome;
use std::path::PathBuf;
use std::sync::Arc;

use crate::runtime::budget_tracker::{
    emit_context_pressure_high_if_warranted, input_tokens_as_context_pct,
    is_retryable_empty_other_response, max_other_empty_retries,
};
use crate::runtime::context_governor::resolver::resolve_context_window_for_run;
use crate::runtime::prompt_budget::{
    sanitize_history_for_request, HistorySanitizeOptions,
    truncate_tool_result as truncate_tool_result_once,
};
use crate::runtime::trajectory_monitor::{ToolObservation, TrajectoryMonitor};
use autonoetic_types::tool_error::ToolErrorType;
use autonoetic_types::trajectory::FeedbackEvent;

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

    /// The turn was suspended at an approval boundary.  The enriched checkpoint
    /// has already been saved; the caller should set the task to
    /// `AwaitingApproval` and release the tokio task / claim — no resources
    /// need to be held while waiting for the operator.
    Suspended {
        approval_request_id: String,
    },

    /// The turn was suspended because a user interaction is pending.
    /// The checkpoint has already been saved by `execute_with_history`;
    /// the caller should record this outcome so the session is visible
    /// as blocked on user input (not "completed empty").
    SuspendedUserInput { interaction_id: String },

    /// The turn was suspended because an escalation requires human review.
    Escalated { escalation_request_id: String },

    WaitingForChild,
}

/// Map a `TurnOutcome` to the close reason used by the direct
/// `execute_with_history` / `execute_loop` path.
pub fn session_close_outcome_from_turn_outcome(
    outcome: &TurnOutcome,
) -> SessionCloseOutcome {
    match outcome {
        TurnOutcome::Completed(Some(_)) => SessionCloseOutcome::ExecuteLoopComplete,
        TurnOutcome::Completed(None) => SessionCloseOutcome::ExecuteLoopComplete,
        TurnOutcome::Suspended { .. } => SessionCloseOutcome::ExecuteLoopSuspended,
        TurnOutcome::SuspendedUserInput { .. } => {
            SessionCloseOutcome::ExecuteLoopSuspendedUserInput
        }
        TurnOutcome::Escalated { .. } => SessionCloseOutcome::ExecuteLoopEscalated,
        TurnOutcome::WaitingForChild => SessionCloseOutcome::ExecuteLoopSuspended,
    }
}

/// Pre-send overflow guard. When the context governor is exhausted
/// (`GovernorResult::Overflow`), decide whether the post-reduction prompt would
/// still exceed the model's assumed context window (`effective_limit + margin`).
///
/// Returns a `context_overflow:`-tagged error (so the scheduler's recovery
/// retries with the aggressive pipeline instead of sending a doomed request)
/// when the estimate exceeds the window; `None` when the prompt is still under
/// the window (only within the safety margin) and may be sent.
fn overflow_presend_block(
    estimated_tokens: usize,
    effective_limit: usize,
    margin: usize,
) -> Option<anyhow::Error> {
    let assumed_window = effective_limit.saturating_add(margin);
    if estimated_tokens > assumed_window {
        Some(anyhow::anyhow!(
            "context_overflow: context governor exhausted — estimated {} tokens exceeds model context window ~{} (effective_limit {} + margin {}); not sending",
            estimated_tokens,
            assumed_window,
            effective_limit,
            margin
        ))
    } else {
        None
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
    /// Egress labels accumulated for tool results this session, keyed by
    /// `tool_call_id` (monotonic — once a tool result is labeled, it stays
    /// labeled for every future completion in the session). Attached to each
    /// completion request's metadata so the `EgressChokepointDriver` (RFC §5.2)
    /// substitutes indications for content whose label excludes the target
    /// sink. Empty for unconfigured deployments — the chokepoint is then a
    /// zero-cost pass-through.
    pub egress_labels: std::collections::HashMap<String, autonoetic_types::egress::EgressLabel>,
    /// A filed pin×taint conflict ask (RFC §5.3 / #968) whose answer still
    /// shapes this turn's routing (see `EgressAskState`). Checkpointed so the
    /// resumed turn honors the operator's choice; cleared when the turn
    /// changes.
    pub egress_ask_state: Option<crate::runtime::egress_labeler::EgressAskState>,
    /// Initial taint for the first user turn (OFP inbound `agent_message`, RFC §7).
    pub initial_ingest_egress_label: Option<autonoetic_types::egress::EgressLabel>,
    /// Taint of the tool batch produced since the last completion (RFC §5.3):
    /// the intersection of the labels of the results the previous turn added.
    /// Drives taint-following routing for the *next* completion — a tainted
    /// batch makes only eligible presets candidates. `unrestricted` when the
    /// previous turn added no labeled results (the common, clean case), which
    /// is the fast no-op path. Transient per-turn state, not persisted: on
    /// resume a session starts with an unrestricted batch and the accumulated
    /// `egress_labels` still drive chokepoint withholding for older content.
    pub pending_batch_taint: autonoetic_types::egress::EgressLabel,
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
    /// Probed context windows for local OpenAI-compatible model servers.
    pub local_model_context_cache: Option<Arc<LocalModelContextCache>>,
    pub gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    /// Workflow / task context for enriched checkpoint on suspension.
    pub workflow_id: Option<String>,
    pub task_id: Option<String>,
    /// SHA-256 of runtime.lock content, captured at session start for reproducibility.
    pub runtime_lock_hash: Option<String>,
    /// Constitution version that admitted this session (#821), captured
    /// once at session start (or restored from a checkpoint on resume).
    /// `None` when the constitution runtime was never initialized.
    pub constitution_version: Option<String>,
    /// Constitution digest paired with `constitution_version` above.
    pub constitution_digest: Option<String>,
    /// One-paragraph notice built by the drift check below when the running
    /// gateway's constitution has changed since this session's pin. Injected
    /// into the system prompt exactly once (consumed via `.take()`), then
    /// cleared — never serialized into a checkpoint.
    pub(crate) constitution_drift_notice: Option<String>,
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

    /// Whether the Ri-0.6 capability snapshot check has already run this session.
    pub(crate) ri_0_6_snapshot_checked: bool,
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
    pub blocked_state_event_emitted: bool,

    /// Transient resume seed (#719): a single operator-approved tool call to
    /// execute mechanically at the start of the next `execute_with_history`,
    /// instead of asking the LLM to re-issue it. Set by
    /// `resume_from_checkpoint` for an approval-gated call that has no
    /// precomputed result (the promote / capability-delta case); taken and
    /// cleared on the first loop iteration. Never serialized into a checkpoint.
    pub(crate) resume_pending_batch: Option<(Message, Vec<crate::llm::ToolCall>)>,

    /// Parsed once from SKILL.md; used instead of re-reading the file every turn.
    pub loop_guard_declaration: Option<autonoetic_types::agent::LoopGuardDeclaration>,

    /// In-session divergence monitor (Sentinel P1). Observes LoopGuard
    /// pressure, digest stall, repetition entropy, error bursts, and
    /// context pressure. Emits `divergence.*` causal events on level
    /// transitions.
    pub trajectory_monitor: TrajectoryMonitor,

    /// Context utilization fraction from the most recent prompt budget
    /// computation. Passed into the trajectory monitor each turn.
    pub last_context_utilization: Option<f32>,

    /// Shared suppression target for `sentinel.suppress`. The tool writes a
    /// turn counter here; the lifecycle reads it before emitting divergence
    /// messages. A value of `0` means no suppression is active.
    pub suppress_until_turn: Arc<AtomicU64>,

    /// Tracks whether the session has escalated from Core+Workflow to all tiers.
    /// Set to true when `progressive_tool_disclosure` is enabled and the agent
    /// attempts to use a Specialized tool. Once escalated, stays escalated for
    /// the rest of the session (including across approval suspension/resume).
    pub tool_tier_escalated: bool,

    /// Tool names explicitly discovered via `tool_discover`. These tools are
    /// included in subsequent turns even if they would be filtered by tier.
    pub discovered_tools: std::collections::HashSet<String>,

    /// Shared handle for `tool_discover` to write newly discovered tool names
    /// from within the `NativeTool::execute` context. Drained after each tool
    /// batch by the lifecycle loop into `discovered_tools`.
    pub discovered_tools_writer: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,

    /// When true, we already emitted a `context.pressure_high` workflow event at the current
    /// pressure level. Cleared when estimated tokens drop below the threshold (85% of
    /// effective_limit), so the TUI sees a fresh warning on each pressure buildup cycle.
    pub pressure_high_warned: bool,

    /// Resolved inference profile for this spawn (preset + concrete llm_config).
    pub resolved_inference:
        Option<crate::runtime::inference_profile::ResolvedInferenceProfile>,

    /// Set to `true` when a budget *pre-check, reservation, or recording* failed
    /// specifically against the **root-session-tree** budget
    /// (`self.root_session_budget`), not the per-session budget. The service
    /// layer reads this flag after the turn returns its budget-exhausted error
    /// to fire the one-time graceful "root budget circuit breaker" (C2 / #616).
    /// Per-session budget exhaustion never sets this flag, so it never cascades.
    pub root_budget_exhausted: bool,
}

use crate::runtime::tool_dispatch::{
    effective_max_session_turns, effective_max_session_turns_hard,
    loop_guard_from_config_and_manifest, tool_result_counts_as_progress,
};
pub use crate::runtime::tool_dispatch::determine_tool_tier_filter;
use std::sync::atomic::AtomicU64;

fn is_signal_derived_exit(value: &serde_json::Value) -> bool {
    value.get("ok").and_then(|v| v.as_bool()) == Some(false)
        && value
            .get("exit_code")
            .and_then(|v| v.as_i64())
            .map_or(false, |code| code >= 128)
}

/// Best-effort normalization of an error message so semantically identical
/// errors compare equal even when incidental details (ids, paths, timestamps)
/// vary. Used to build stable `FeedbackEvent::ToolError` signatures.
fn normalize_error_signature(message: &str) -> String {
    let mut s = message.to_lowercase();
    // Replace UUID-like hex strings.
    s = regex::Regex::new(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .map(|re| re.replace_all(&s, "[uuid]").into_owned())
        .unwrap_or(s);
    // Replace long hex tokens.
    s = regex::Regex::new(r"\b[0-9a-f]{16,}\b")
        .map(|re| re.replace_all(&s, "[hex]").into_owned())
        .unwrap_or(s);
    // Replace numeric values.
    s = regex::Regex::new(r"\b\d+\b")
        .map(|re| re.replace_all(&s, "[n]").into_owned())
        .unwrap_or(s);
    // Collapse whitespace.
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decision + payload for a constitution drift notice (#821). Pure so it can
/// be unit-tested without a tracer/gateway_store: compares the session's
/// pinned constitution (captured at session start, or restored from its
/// checkpoint on resume) against the currently running gateway's
/// constitution. Returns `None` when there is nothing to notice — no prior
/// pin (fresh session, or a session that predates this feature), or the
/// pins already match.
///
/// Constitution drift is always a **notice**, never a block (Ri-0.5
/// notice-before-degradation; non-retroactivity is about knowing, not
/// freezing) — unlike `runtime_lock` drift, which can bail the session.
pub(crate) struct ConstitutionDriftNotice {
    /// Causal-event payload: pinned/current version+digest, tagged
    /// `enforced_rules: ["Ri-0.5"]` so `session_tracer::log_event` attributes
    /// the event to Ri-0.5 for contract-health tallying.
    pub payload: serde_json::Value,
    /// One-paragraph text injected into the system prompt for the next turn.
    pub notice_text: String,
}

pub(crate) fn detect_constitution_drift(
    pinned_version: Option<&str>,
    pinned_digest: Option<&str>,
    current_version: &str,
    current_digest: &str,
) -> Option<ConstitutionDriftNotice> {
    let (pinned_version, pinned_digest) = match (pinned_version, pinned_digest) {
        (Some(v), Some(d)) => (v, d),
        _ => return None,
    };
    if pinned_digest == current_digest {
        return None;
    }
    let payload = serde_json::json!({
        "pinned_version": pinned_version,
        "pinned_digest": pinned_digest,
        "current_version": current_version,
        "current_digest": current_digest,
        "enforced_rules": ["Ri-0.5"],
    });
    fn short(d: &str) -> &str {
        &d[..d.len().min(12)]
    }
    let notice_text = format!(
        "---\n\nConstitution Drift Notice (Ri-0.5)\n\n\
         The law changed while this session was suspended: from version {} ({}) \
         to version {} ({}). The current attestation block in this system prompt \
         is authoritative going forward.\n",
        pinned_version,
        short(pinned_digest),
        current_version,
        short(current_digest),
    );
    Some(ConstitutionDriftNotice { payload, notice_text })
}

/// Mint a stable `msg_<id>` on an assistant message being committed to history
/// and attach the turn's LLM-response label (RFC §4.5), so a later request to a
/// sink the label excludes withholds it via the chokepoint. No-op for a clean
/// turn (`response_label` = `None`) — the message stays id-less and unlabeled,
/// so unconfigured deployments pay nothing. Free function (not a method) so it
/// borrows only `msg` and the label map, never all of `&mut self`.
fn commit_assistant_egress(
    msg: &mut Message,
    response_label: &Option<autonoetic_types::egress::EgressLabel>,
    labels: &mut std::collections::HashMap<String, autonoetic_types::egress::EgressLabel>,
) {
    let Some(label) = response_label else {
        return;
    };
    let id = msg
        .id
        .get_or_insert_with(|| autonoetic_types::id_format::short_random_id("msg_"))
        .clone();
    labels.insert(id, label.clone());
}

/// Outcome of taint-following routing for one completion (RFC §5.3), produced
/// by [`AgentExecutor::plan_egress_routing`].
struct EgressRoutingSelection {
    /// Driver to run the primary completion on. `None` keeps `self.llm` (the
    /// primary is already eligible for the batch); `Some` is a driver rebuilt
    /// for the eligible preset the batch forced a switch to.
    primary_driver: Option<std::sync::Arc<dyn LlmDriver>>,
    /// The egress class actually in force for this completion (the rerouted
    /// preset's, or the primary's) — the source of truth for the chokepoint
    /// audit sink.
    effective_class: Option<autonoetic_types::egress::EgressClass>,
    /// The model name actually running this completion after a reroute (`None`
    /// keeps the primary's `routed_model`), so cost/tracing attribute it right.
    effective_model: Option<String>,
    /// The failover chain filtered to presets eligible for the batch.
    fallback_chain: Vec<(String, String, String)>,
    /// `Some(reason)` when a tainted batch has no eligible provider — the
    /// caller refuses the turn with `egress_no_eligible_provider` (RFC §5.3).
    refuse_reason: Option<String>,
    /// `Some(interaction_id)` when a pinned primary conflicted with the batch
    /// taint and the gateway filed the three-way inline ask (RFC §5.3 / #968):
    /// the caller suspends the turn on `UserInputRequired` and the resumed
    /// turn honors the operator's answer at routing time.
    pending_ask: Option<String>,
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
        let loop_guard_declaration =
            crate::runtime::tool_dispatch::load_manifest_loop_guard_declaration(&agent_dir,
            );
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
            egress_labels: std::collections::HashMap::new(),
            egress_ask_state: None,
            initial_ingest_egress_label: None,
            pending_batch_taint: autonoetic_types::egress::EgressLabel::unrestricted(),
            config: None,
            session_budget: None,
            root_session_budget: None,
            middleware: manifest.middleware.clone().unwrap_or_default(),
            llm_usage_last_run: Vec::new(),
            openrouter_catalog: None,
            local_model_context_cache: None,
            gateway_store,
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            constitution_drift_notice: None,
            drift_checked: false,
            active_executions: None,
            live_digest: None,
            live_report: None,
            last_history: Vec::new(),
            session_started_at: None,
            compression_metadata: Default::default(),
            capsule_state: None,
            http_client: crate::llm::build_llm_client(),
            user_id: None,
            artifact_id: None,
            ri_0_6_previous_snapshot: None,
            ri_0_6_snapshot_checked: false,
            persona: None,
            overflow_recovery: false,
            extended_instructions: None,
            blocked_state_event_emitted: false,
            resume_pending_batch: None,
            loop_guard_declaration,
            trajectory_monitor: TrajectoryMonitor::new(Default::default()),
            last_context_utilization: None,
            suppress_until_turn: Arc::new(AtomicU64::new(0)),
            tool_tier_escalated: false,
            discovered_tools: std::collections::HashSet::new(),
            discovered_tools_writer: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            pressure_high_warned: false,
            resolved_inference: None,
            root_budget_exhausted: false,
        }
    }

    pub fn with_resolved_inference(
        mut self,
        profile: crate::runtime::inference_profile::ResolvedInferenceProfile,
    ) -> Self {
        self.resolved_inference = Some(profile);
        self
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
        self.guard = loop_guard_from_config_and_manifest(
            Some(config.as_ref()),
            &self.agent_dir,
            self.loop_guard_declaration.as_ref(),
            self.manifest.execution_mode,
        );
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

    pub fn with_local_model_context_cache(
        mut self,
        cache: Option<Arc<LocalModelContextCache>>,
    ) -> Self {
        self.local_model_context_cache = cache;
        self
    }

    pub fn with_initial_user_message(mut self, message: impl Into<String>) -> Self {
        self.initial_user_message = message.into();
        self
    }

    /// Seed the first user message with an ingress egress label (OFP federation).
    pub fn with_initial_ingest_egress_label(
        mut self,
        label: autonoetic_types::egress::EgressLabel,
    ) -> Self {
        self.initial_ingest_egress_label = Some(label);
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
        crate::runtime::checkpoint::turn_id_for(self.turn_counter)
    }

    /// Detect a terminal-workflow error returned by `agent_spawn` and extract
    /// the workflow id. These errors are deterministic: retrying the same call
    /// can never succeed while the workflow stays terminal.
    fn detect_terminal_workflow_error(tool_name: &str, result_json: &str) -> Option<String> {
        if tool_name != "agent_spawn" && tool_name != "agent.spawn" {
            return None;
        }
        let parsed = serde_json::from_str::<serde_json::Value>(result_json).ok()?;
        if parsed.get("ok").and_then(|v| v.as_bool()) != Some(false) {
            return None;
        }
        let message = parsed.get("message")?.as_str()?;
        let lower = message.to_ascii_lowercase();
        if !lower.contains("already terminal") || !lower.contains("workflow") {
            return None;
        }
        // Message format: "Cannot delegate (agent.spawn): workflow <id> is already terminal ..."
        message
            .split("workflow ")
            .nth(1)?
            .split_whitespace()
            .next()
            .map(|s| s.to_string())
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

    fn check_session_continue_gate(
        &self,
        cfg: &GatewayConfig,
        session_id: &str,
        max_turns: u32,
        blocked_turn: u64,
        turn_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some(store) = self.gateway_store.as_ref() else {
            anyhow::bail!("GatewayStore is required for max-session-turn approval gating");
        };
        let root_session_id =
            crate::runtime::content_store::root_session_id(session_id).to_string();
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

        let gate_service = GateService::new(store.clone());
        let gate_req = GateRequest {
            kind: GateKind::Approval {
                action: action.clone(),
                targets: Vec::new(),
                match_strategy: MatchStrategy::ExactPayload,
            },
            manifest: &self.manifest,
            session_id: Some(session_id),
            run_context: None,
            config: Some(cfg),
            context: DecisionContext::tier2(
                format!(
                    "Session {} reached max_session_turns={} at turn {}",
                    session_id, max_turns, blocked_turn
                ),
                "Hard session-level turn limit reached",
                format!("Approving grants one additional window of {} turns", max_turns),
                "Approve if the session should continue; reject to end it",
            ),
            summary: format!("Session {} turn limit (turn {})", session_id, blocked_turn),
            approval_ref: None,
            request_id: None,
            pre_validated: false,
            cache_backfill: None,
            turn_id: Some(turn_id),
        };

        match gate_service.check(gate_req)? {
            GateResult::AlreadyPending { gate_id, .. }
            | GateResult::Suspended { gate_id, .. } => Ok(Some(gate_id)),
            GateResult::Cleared { source, .. } => {
                tracing::info!(
                    target: "lifecycle",
                    agent_id = %self.manifest.agent.id,
                    session_id = %session_id,
                    source = ?source,
                    "Session continue gate cleared via GateService"
                );
                Ok(None)
            }
            GateResult::PolicyAllowed => Ok(None),
        }
    }

    /// Emit the causal + timeline events for a `max_session_turns_hard` trip
    /// (issue #854). The session terminates via `YieldReason::MaxTurnsReached`,
    /// a declared budget-exhaustion termination reason under Ri-0.12; these
    /// events make *why* the session was terminated attributable in the causal
    /// chain, in contract-health (via the enforcement register), and on the
    /// room timeline — mirroring the `loop_guard.tripped` emission.
    fn emit_session_turn_hard_cap_event(
        &self,
        session_id: &str,
        turn_id: &str,
        soft_limit: u32,
        hard_cap: u32,
        blocked_turn: u64,
    ) {
        let Some(store) = self.gateway_store.as_ref() else {
            return;
        };
        let root = crate::runtime::content_store::root_session_id(session_id).to_string();
        let payload = serde_json::json!({
            "reason": "max_session_turns_hard_reached",
            "max_session_turns": soft_limit,
            "max_session_turns_hard": hard_cap,
            "turn_counter": blocked_turn,
            "root_session_id": root,
            "clause": crate::enforcement_register::clause_of_rule("Ri-0.12"),
        });
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("turnhardcap-{}", uuid::Uuid::new_v4()),
            agent_id: self.manifest.agent.id.clone(),
            session_id: session_id.to_string(),
            turn_id: Some(turn_id.to_string()),
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "session".to_string(),
            action: "session.turn_hard_cap".to_string(),
            status: "active".to_string(),
            enforced_rules: vec!["Ri-0.12".to_string()],
            target: None,
            payload: Some(payload.to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: Some("max_session_turns_hard_reached".to_string()),
        };
        if let Err(err) = store.create_causal_event(&event) {
            tracing::warn!(
                target: "lifecycle",
                error = %err,
                "failed to emit session.turn_hard_cap causal event"
            );
        }

        // Surface on the canonical timeline so the room shows *why* the session
        // terminated, carrying the rule ID as a first-class ref.
        let principal =
            autonoetic_types::principal::Principal::agent(self.manifest.agent.id.clone());
        let role = crate::runtime::session_timeline::derive_role(&self.manifest.agent.id);
        let tl = crate::runtime::session_timeline::build_timeline_event(
            root,
            session_id.to_string(),
            Some(turn_id.to_string()),
            &principal,
            &role,
            "session.turn_hard_cap",
            None, // base_altitude ⇒ Error
            Some(serde_json::json!({
                "max_session_turns": soft_limit,
                "max_session_turns_hard": hard_cap,
                "turn_counter": blocked_turn,
            })),
            autonoetic_types::session_timeline::TimelineRefs {
                enforced_rules: vec!["Ri-0.12".to_string()],
                ..Default::default()
            },
        );
        if let Err(err) = store.create_live_digest_event(&tl) {
            tracing::debug!(
                target: "session_timeline",
                error = %err,
                "session.turn_hard_cap timeline emit failed"
            );
        }
    }

    /// Option 3 (issue #854): emit an **observational** causal + timeline event,
    /// keyed to the **root** session, recording that a delegated child session
    /// has crossed into another `max_session_turns` continuation window. This
    /// gives the operator/planner root-level visibility that a descendant has
    /// been running for N continuation windows — a long-running child that the
    /// root would otherwise only see as an isolated per-child approval.
    ///
    /// The causal event carries the baseline attribution placeholder (not an
    /// enforcement rule), so it is *observability, not enforcement*: it does not
    /// inflate any clause's contract-health tally.
    #[allow(clippy::too_many_arguments)]
    fn emit_continuation_window_extended_event(
        &self,
        session_id: &str,
        turn_id: &str,
        soft_limit: u32,
        hard_cap: u32,
        approved_windows: u64,
        blocked_turn: u64,
        approval_request_id: &str,
    ) {
        let Some(store) = self.gateway_store.as_ref() else {
            return;
        };
        let root = crate::runtime::content_store::root_session_id(session_id).to_string();
        // The child has already cleared `approved_windows`, so this request is
        // for window N+1.
        let requested_window = approved_windows.saturating_add(1);
        let payload = serde_json::json!({
            "child_session_id": session_id,
            // `turn_id` is session-scoped; this event is keyed to the ROOT
            // session, so the child's turn lives in the payload rather than the
            // event's `turn_id` field (which would otherwise mix a child turn
            // into a root-session event).
            "child_turn_id": turn_id,
            "child_agent_id": self.manifest.agent.id,
            "approved_windows": approved_windows,
            "requested_window": requested_window,
            "max_session_turns": soft_limit,
            "max_session_turns_hard": hard_cap,
            "turn_counter": blocked_turn,
            "approval_request_id": approval_request_id,
        });
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("contwindow-{}", uuid::Uuid::new_v4()),
            agent_id: self.manifest.agent.id.clone(),
            // Keyed to the root session so it surfaces at the root, not buried
            // in the child's own trace.
            session_id: root.clone(),
            // None: this event belongs to the root session, but the child turn
            // is session-scoped — see `child_turn_id` in the payload.
            turn_id: None,
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "session".to_string(),
            action: "session.continuation_window_extended".to_string(),
            status: "active".to_string(),
            enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
            target: None,
            payload: Some(payload.to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: Some("delegated_continuation_window".to_string()),
        };
        if let Err(err) = store.create_causal_event(&event) {
            tracing::warn!(
                target: "lifecycle",
                error = %err,
                "failed to emit session.continuation_window_extended causal event"
            );
        }

        let principal =
            autonoetic_types::principal::Principal::agent(self.manifest.agent.id.clone());
        let role = crate::runtime::session_timeline::derive_role(&self.manifest.agent.id);
        let tl = crate::runtime::session_timeline::build_timeline_event(
            root,
            session_id.to_string(),
            Some(turn_id.to_string()),
            &principal,
            &role,
            "session.continuation_window_extended",
            None,
            Some(serde_json::json!({
                "child_session_id": session_id,
                "approved_windows": approved_windows,
                "requested_window": requested_window,
                "max_session_turns_hard": hard_cap,
            })),
            autonoetic_types::session_timeline::TimelineRefs::default(),
        );
        if let Err(err) = store.create_live_digest_event(&tl) {
            tracing::debug!(
                target: "session_timeline",
                error = %err,
                "session.continuation_window_extended timeline emit failed"
            );
        }
    }

    /// Residency TTL declared by this agent's manifest, if it opted in.
    ///
    /// `Some(ttl)` means a completed session parks in [`YieldReason::Idle`]
    /// and stays addressable by `agent_message` instead of terminating.
    pub fn resident_idle_ttl_secs(&self) -> Option<u64> {
        self.manifest
            .agent
            .resident_idle_ttl_secs
            .filter(|ttl| *ttl > 0)
    }

    /// Park a finished-but-resident session: persist an [`YieldReason::Idle`]
    /// checkpoint so an inbound message can resume it, and return the turn id
    /// the residency row should point at.
    ///
    /// Returns `None` when there is nothing to park (no config, or the session
    /// never started), in which case the caller must close normally rather than
    /// leave an unreachable residency row behind.
    pub fn park_idle(&self, ttl_secs: u64) -> Option<String> {
        if !self.session_started || self.config.is_none() {
            return None;
        }
        let turn_id = crate::runtime::checkpoint::turn_id_for(self.turn_counter);
        let history = self.last_history.clone();
        let cp = self.save_yield_checkpoint(
            &history,
            &turn_id,
            YieldReason::Idle {
                since: chrono::Utc::now().to_rfc3339(),
                ttl_secs,
            },
            None,
        );
        Some(cp.turn_id)
    }

    pub fn close_session(&mut self, outcome: SessionCloseOutcome) -> anyhow::Result<()> {
        if !self.session_started {
            return Ok(());
        }
        let session_id = self.ensure_session_id();
        let reason = outcome.as_str();
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
                    let status = if outcome.is_suspended() {
                        "suspended"
                    } else if outcome.is_error() {
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

        if !outcome.is_suspended() {
            let root_sid = crate::runtime::content_store::root_session_id(&session_id);
            let is_root = root_sid == session_id;
            if is_root {
                if let Some(gs) = self.gateway_store.as_ref() {
                    if let Err(e) = gs.delete_session_grants(&root_sid) {
                        tracing::warn!(
                            root_session_id = %root_sid,
                            error = %e,
                            "Failed to delete session grants on session close"
                        );
                    }
                    // Egress: the session's `egress_policy` dies with the root
                    // session (RFC data-envelopes §5.4). Only on a real close —
                    // a suspended session resumes and must keep its rules.
                    crate::runtime::egress_labeler::clear_session_egress_policy(
                        gs,
                        &root_sid,
                        "session_close",
                    );
                }
            }
            // #853: free this session's per-host probe budget. Keyed by the
            // exact session id, so a closing child releases its own budget
            // (not just the root) — a re-spawn then starts fresh.
            if let Some(gs) = self.gateway_store.as_ref() {
                gs.host_probe_budget.clear_session(&session_id);
            }
        }

        // Transition workflow tasks to Failed when a child session dies
        // abnormally. Without this, tasks stay Running forever (#670).
        if outcome.is_error() {
            if let Some(cfg) = self.config.as_deref() {
                if let Err(e) = crate::scheduler::workflow_store::fail_running_tasks_for_session(
                    cfg,
                    self.gateway_store.as_deref(),
                    &session_id,
                    outcome.as_str(),
                ) {
                    tracing::warn!(
                        target: "workflow",
                        session_id = %session_id,
                        error = %e,
                        "Failed to transition workflow tasks after session termination"
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
                let _ = g.finish_session(outcome, latest_assistant);
            }
        }
        let mut tracer = SessionTracer::new(&self.agent_dir, &self.manifest.agent.id, &session_id)?;
        tracer.log_session_end(reason);

        // Attempt workflow completion when root session closes normally.
        let is_root = crate::runtime::content_store::root_session_id(&session_id) == session_id;
        if is_root {
            if let Some(cfg) = self.config.as_deref() {
                if outcome.is_completed() {
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
                } else if outcome.is_error() {
                    // GAP-1B: root session closed with error — fail pending tasks so
                    // they don't stay Running forever against a dead root. We only
                    // mark the workflow itself terminal for unrecoverable spawn-time
                    // or script-mode errors; ExecuteLoopError is recoverable (e.g.
                    // LLM failure, context overflow) and leaves the workflow intact
                    // so the scheduler or operator can resume.
                    let mark_workflow_terminal = !matches!(
                        outcome,
                        SessionCloseOutcome::ExecuteLoopError
                    );
                    if let Err(e) =
                        crate::scheduler::workflow_store::fail_workflow_for_root_session(
                            cfg,
                            self.gateway_store.as_deref(),
                            &session_id,
                            reason,
                            mark_workflow_terminal,
                        )
                    {
                        tracing::warn!(
                            target: "workflow",
                            error = %e,
                            session_id = %session_id,
                            "Failed to fail workflow tasks on root session error"
                        );
                    }
                }
            }
        }

        self.session_started = false;
        self.session_id = None;
        self.turn_counter = 0;
        self.ri_0_6_previous_snapshot = None;
        Ok(())
    }

    /// Build an `LlmDriver` for a named preset from `llm_presets` (RFC §5.3
    /// taint-following reroute). Mirrors the fallback loop's per-preset driver
    /// build. Returns `None` when the preset is unknown or lacks a
    /// provider/model (can't be built) — the caller then treats the batch as
    /// having no eligible provider and refuses.
    fn build_driver_for_preset(
        &self,
        preset_name: &str,
    ) -> Option<std::sync::Arc<dyn LlmDriver>> {
        let cfg = self.config.as_ref()?;
        let preset = cfg.llm_presets.get(preset_name)?;
        let llm_config = autonoetic_types::agent::LlmConfig {
            provider: preset.provider.clone()?,
            model: preset.model.clone()?,
            temperature: preset.temperature.unwrap_or(0.0),
            fallback_provider: None,
            fallback_model: None,
            chat_only: preset.chat_only.unwrap_or(false),
            context_window_tokens: preset.context_window_tokens,
            base_url: preset.base_url.clone(),
            api_key_env: preset.api_key_env.clone(),
            routing_preset: Some(preset_name.to_string()),
            thinking: preset.thinking.clone(),
            egress_class: preset.egress_class,
        };
        crate::llm::build_driver(llm_config, self.http_client.clone()).ok()
    }

    /// Taint-following routing for one completion (RFC §5.3).
    ///
    /// Given the batch taint accumulated since the last completion
    /// batch taint (`batch`), pick the driver this completion must run on and
    /// filter the failover chain to eligible presets. Emits
    /// `egress.provider_selected` (RFC §9.1). Kept `#[inline(never)]` so its
    /// locals stay out of the razor-thin `execute_with_history` poll frame
    /// (#884/#916).
    ///
    /// `routed_preset` is the primary's *preset* identity for the audit event
    /// (not a model name), so "why did turn N run on this provider?" reads the
    /// same whether or not a reroute happened. Called when the batch is
    /// restricted **or** a session `provider_constraint` is set (RFC §5.4 rung
    /// 1 constrains selection even for clean batches); only an unrestricted
    /// batch with no constraint keeps the primary driver and the full failover
    /// chain with zero cost.
    ///
    /// `primary_pinned` (#968) tells the plane whether the primary came from a
    /// pin (agent manifest `llm_preset`, session override, or legacy fixed
    /// model) rather than a per-completion routing strategy. A pinned primary
    /// that conflicts with the batch taint files the three-way inline ask
    /// (declassify / run local / abort) and suspends the turn — never a silent
    /// downgrade, never a dead-end refusal. Unpinned primaries keep the
    /// automatic taint-following reroute.
    #[inline(never)]
    fn plan_egress_routing(
        &mut self,
        batch: &autonoetic_types::egress::EgressLabel,
        routed_preset: &str,
        primary_class: Option<autonoetic_types::egress::EgressClass>,
        fallback_chain: &[(String, String, String)],
        session_id: &str,
        turn_id: &str,
        provider_constraint: Option<autonoetic_types::egress::ProviderConstraint>,
        primary_pinned: bool,
    ) -> EgressRoutingSelection {
        use crate::runtime::egress_labeler as el;
        let presets = self.config.as_ref().map(|c| &c.llm_presets);
        let root_session = crate::runtime::content_store::root_session_id(session_id);

        // Set when a resumed completion was unblocked by an answered ask; the
        // resolution emit below records the outcome (RFC §9.1 "inline-ask
        // outcome", #968).
        let mut ask_outcome: Option<serde_json::Value> = None;

        // ── Resumed ask (RFC §5.3 / #968) ─────────────────────────────────
        // A turn suspended on a filed pin×taint ask re-enters routing with the
        // ask state carried in its checkpoint; honor the operator's answer
        // without re-deriving the (already consumed) batch taint. Turn-scoped:
        // a state from a previous turn is stale and dropped.
        if let Some(state) = self.egress_ask_state.clone() {
            if state.turn_id != turn_id {
                self.egress_ask_state = None;
            } else if let Some(interaction) = self
                .gateway_store
                .as_ref()
                .and_then(|store| store.get_user_interaction(&state.interaction_id).ok().flatten())
            {
                match interaction.answer_option_id.as_deref() {
                    Some(el::egress_ask_options::RUN_LOCAL) => {
                        if let Some(local) = state.local_preset.clone() {
                            // The operator picked the offered local preset for
                            // the rest of this turn — build it and run on it
                            // directly (it was buildable at filing time:
                            // candidates are buildable by construction).
                            let emit_plan = el::EgressRoutingPlan {
                                batch: state.batch.clone(),
                                provider_constraint,
                                primary_eligible: false,
                                eligible: vec![local.name.clone()],
                                reroute_to: Some(local.clone()),
                            };
                            let inline = serde_json::json!({
                                "status": "answered",
                                "outcome": el::egress_ask_options::RUN_LOCAL,
                                "interaction_id": state.interaction_id,
                            });
                            let emit = |chosen: Option<&str>| {
                                if let Some(store) = self.gateway_store.as_ref() {
                                    el::emit_provider_selected(
                                        store,
                                        session_id,
                                        &self.manifest.agent.id,
                                        Some(turn_id),
                                        &emit_plan,
                                        chosen,
                                        &[],
                                        false,
                                        false,
                                        Some(&inline),
                                    );
                                }
                            };
                            return match self.build_driver_for_preset(&local.name) {
                                Some(driver) => {
                                    emit(Some(&local.name));
                                    let effective_model = presets
                                        .and_then(|m| m.get(&local.name))
                                        .and_then(|p| p.model.clone());
                                    EgressRoutingSelection {
                                        primary_driver: Some(driver),
                                        effective_class: local.egress_class,
                                        effective_model,
                                        fallback_chain: fallback_chain.to_vec(),
                                        refuse_reason: None,
                                        pending_ask: None,
                                    }
                                }
                                None => {
                                    emit(None);
                                    EgressRoutingSelection {
                                        primary_driver: None,
                                        effective_class: primary_class,
                                        effective_model: None,
                                        fallback_chain: fallback_chain.to_vec(),
                                        refuse_reason: Some(format!(
                                            "egress_ask_unbuildable: the operator chose local \
                                             preset '{}' but it could not be built (missing \
                                             provider/model).",
                                            local.name
                                        )),
                                        pending_ask: None,
                                    }
                                }
                            };
                        }
                        self.egress_ask_state = None;
                    }
                    Some(el::egress_ask_options::DECLASSIFY) => {
                        // The grant was materialized at answer time; the normal
                        // flow below sees the session declassified and keeps
                        // the pinned primary.
                        self.egress_ask_state = None;
                        ask_outcome = Some(serde_json::json!({
                            "status": "answered",
                            "outcome": el::egress_ask_options::DECLASSIFY,
                            "interaction_id": state.interaction_id,
                        }));
                    }
                    Some(el::egress_ask_options::ABORT) => {
                        self.egress_ask_state = None;
                        let emit_plan = el::EgressRoutingPlan {
                            batch: state.batch.clone(),
                            provider_constraint,
                            primary_eligible: false,
                            eligible: Vec::new(),
                            reroute_to: None,
                        };
                        let inline = serde_json::json!({
                            "status": "answered",
                            "outcome": el::egress_ask_options::ABORT,
                            "interaction_id": state.interaction_id,
                        });
                        if let Some(store) = self.gateway_store.as_ref() {
                            el::emit_provider_selected(
                                store,
                                session_id,
                                &self.manifest.agent.id,
                                Some(turn_id),
                                &emit_plan,
                                None,
                                &[],
                                false,
                                false,
                                Some(&inline),
                            );
                        }
                        return EgressRoutingSelection {
                            primary_driver: None,
                            effective_class: primary_class,
                            effective_model: None,
                            fallback_chain: Vec::new(),
                            refuse_reason: Some(format!(
                                "egress_aborted_by_operator: the operator chose to abort this \
                                 turn (pinned preset '{}' conflicts with batch {}).",
                                state.pinned_preset,
                                autonoetic_types::egress::label_display_name(&state.batch)
                            )),
                            pending_ask: None,
                        };
                    }
                    _ => {
                        // Still pending or unknown answer — the turn resumed
                        // without a usable choice; keep waiting on the ask.
                        return EgressRoutingSelection {
                            primary_driver: None,
                            effective_class: primary_class,
                            effective_model: None,
                            fallback_chain: Vec::new(),
                            refuse_reason: None,
                            pending_ask: Some(state.interaction_id.clone()),
                        };
                    }
                }
            }
        }

        // Candidates are **buildable fixed presets only**: skip routing presets
        // (no direct provider/model) and presets missing provider/model, so the
        // reroute pick is always something `build_driver_for_preset` can build.
        // Otherwise a deterministic pick of an unbuildable preset would refuse
        // the turn even when another eligible, buildable preset exists.
        let candidates: Vec<el::PresetCandidate> = presets
            .map(|m| {
                m.iter()
                    .filter(|(_, p)| {
                        p.routing.is_none() && p.provider.is_some() && p.model.is_some()
                    })
                    .map(|(name, p)| el::PresetCandidate {
                        name: name.clone(),
                        egress_class: p.egress_class,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut plan =
            el::plan_taint_following_route(batch, primary_class, &candidates, provider_constraint);

        // RFC §8 consumption: an active session-wide declassification to
        // RemoteModel lifts the routing restriction — the operator explicitly
        // widened this session's LLM egress, so every buildable preset is
        // eligible and the failover chain stays whole. A `provider_constraint`
        // (RFC §5.4 rung 1) outranks the grant: an operator who pinned the room
        // local wins over an earlier declassification until they clear it.
        // Host-scoped grants never apply to model routing, and per-envelope
        // grants are a chokepoint-level concern (not yet consulted).
        let declassified_remote = provider_constraint.is_none()
            && self.gateway_store.as_ref().is_some_and(|store| {
                el::session_sink_declassified(
                    store,
                    session_id,
                    root_session,
                    autonoetic_types::egress::Sink::RemoteModel,
                )
            });
        if declassified_remote {
            if !plan.primary_eligible {
                plan.primary_eligible = true;
                plan.reroute_to = None;
            }
            // Audit accuracy: the grant makes every buildable preset eligible
            // whatever the primary — the eligible set must reflect that even
            // when no reroute was needed (e.g. a local primary), since the
            // failover chain is unfiltered in this mode.
            plan.eligible = candidates.iter().map(|c| c.name.clone()).collect();
        }

        // Filter the failover chain to eligible presets: a tainted turn must
        // never fail over into an ineligible (e.g. all-indications remote)
        // context — worse than refusing (RFC §5.3). Uses the plan's
        // **effective** batch so a provider constraint filters the chain too;
        // skipped entirely when the session is declassified to remote.
        let mut fallback_skipped: Vec<String> = Vec::new();
        let filtered_fallback: Vec<(String, String, String)> = if declassified_remote {
            fallback_chain.to_vec()
        } else {
            fallback_chain
                .iter()
                .filter(|(preset, _, _)| {
                    let class = presets.and_then(|m| m.get(preset)).and_then(|p| p.egress_class);
                    let ok = el::preset_batch_eligible(&plan.batch, class);
                    if !ok {
                        fallback_skipped.push(preset.clone());
                    }
                    ok
                })
                .cloned()
                .collect()
        };

        let emit = |chosen: Option<&str>, rerouted: bool, inline_ask: Option<&serde_json::Value>| {
            if let Some(store) = self.gateway_store.as_ref() {
                el::emit_provider_selected(
                    store,
                    session_id,
                    &self.manifest.agent.id,
                    Some(turn_id),
                    &plan,
                    chosen,
                    &fallback_skipped,
                    rerouted,
                    declassified_remote,
                    inline_ask.or(ask_outcome.as_ref()),
                );
            }
        };

        // ── Pinned × taint conflict (RFC §5.3 / #968) ─────────────────────
        // A pinned primary (agent manifest `llm_preset`, session override, or
        // legacy fixed model) that is not cleared for the batch is a conflict
        // the operator must resolve: file the three-way ask (declassify / run
        // this turn on local preset X / abort) and suspend — never silently
        // downgrade (a discretion leak) and never hard-refuse without a path
        // forward. Unpinned primaries (routing strategies) keep the automatic
        // taint-following behavior below.
        if primary_pinned && !plan.primary_eligible {
            let local_candidate = plan.reroute_to.clone();
            let ask_id = self.gateway_store.as_ref().and_then(|store| {
                el::file_egress_pin_ask(
                    store,
                    session_id,
                    root_session,
                    &self.manifest.agent.id,
                    turn_id,
                    &plan.batch,
                    routed_preset,
                    local_candidate.as_ref(),
                )
            });
            if let Some(ask_id) = ask_id {
                self.egress_ask_state = Some(el::EgressAskState {
                    interaction_id: ask_id.clone(),
                    turn_id: turn_id.to_string(),
                    batch: plan.batch.clone(),
                    pinned_preset: routed_preset.to_string(),
                    local_preset: local_candidate.clone(),
                });
                let inline = serde_json::json!({
                    "status": "filed",
                    "interaction_id": ask_id,
                    "options": el::egress_ask_options_payload(local_candidate.as_ref()),
                });
                emit(None, false, Some(&inline));
                return EgressRoutingSelection {
                    primary_driver: None,
                    effective_class: primary_class,
                    effective_model: None,
                    fallback_chain: filtered_fallback,
                    refuse_reason: None,
                    pending_ask: Some(ask_id),
                };
            }
            // Filing failed (flood cap, store error) — never dead-end the
            // turn: fall through to the pre-ask behavior (taint-following
            // reroute or refuse-with-offer).
            tracing::warn!(
                target: "egress",
                session_id = %session_id,
                "failed to file egress pin×taint ask — falling back to taint-following behavior"
            );
        }

        // No eligible provider for a tainted batch → refuse with a path forward.
        if plan.no_eligible_provider() {
            emit(None, false, None);
            // RFC §5.3's path forward, made concrete: file (or reuse) a
            // pending EgressDeclassify request so the operator's way out is a
            // single approval. Skipped when no buildable preset exists at all
            // (declassification cannot help then).
            let offer = if candidates.is_empty() {
                None
            } else {
                self.gateway_store.as_ref().and_then(|store| {
                    self.config.as_ref().and_then(|cfg| {
                        el::file_declassify_offer(
                            store,
                            cfg,
                            session_id,
                            root_session,
                            &self.manifest.agent.id,
                            &plan.batch,
                        )
                    })
                })
            };
            let offer_note = match &offer {
                Some(id) => format!(
                    " A declassification request ({id}) is pending — approve it with \
                     `autonoetic gateway approvals approve {id}` to run this session on a \
                     remote preset."
                ),
                None => String::new(),
            };
            return EgressRoutingSelection {
                primary_driver: None,
                effective_class: primary_class,
                effective_model: None,
                fallback_chain: filtered_fallback,
                refuse_reason: Some(format!(
                    "egress_no_eligible_provider: this turn's new data is labeled {} and no \
                     configured LLM preset is cleared for it. Configure a local preset \
                     (egress_class: local), declassify the specific envelopes, or abort.{}",
                    autonoetic_types::egress::label_display_name(batch),
                    offer_note
                )),
                pending_ask: None,
            };
        }

        // Reroute the primary onto an eligible preset when it isn't cleared.
        match plan.reroute_to.clone() {
            Some(cand) => match self.build_driver_for_preset(&cand.name) {
                Some(driver) => {
                    emit(Some(&cand.name), true, None);
                    let effective_model = presets
                        .and_then(|m| m.get(&cand.name))
                        .and_then(|p| p.model.clone());
                    EgressRoutingSelection {
                        primary_driver: Some(driver),
                        effective_class: cand.egress_class,
                        effective_model,
                        fallback_chain: filtered_fallback,
                        refuse_reason: None,
                        pending_ask: None,
                    }
                }
                None => {
                    emit(None, false, None);
                    EgressRoutingSelection {
                        primary_driver: None,
                        effective_class: primary_class,
                        effective_model: None,
                        fallback_chain: filtered_fallback,
                        refuse_reason: Some(format!(
                            "egress_no_eligible_provider: the only eligible preset '{}' for \
                             batch {} could not be built (missing provider/model).",
                            cand.name,
                            autonoetic_types::egress::label_display_name(batch)
                        )),
                        pending_ask: None,
                    }
                }
            },
            None => {
                // Primary already eligible — keep `self.llm`.
                emit(Some(routed_preset), false, None);
                EgressRoutingSelection {
                    primary_driver: None,
                    effective_class: primary_class,
                    effective_model: None,
                    fallback_chain: filtered_fallback,
                    refuse_reason: None,
                    pending_ask: None,
                }
            }
        }
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
            .resolved_inference
            .as_ref()
            .map(LlmConfigSnapshot::from_inference_profile)
            .or_else(|| {
                self.manifest
                    .llm_config
                    .as_ref()
                    .map(LlmConfigSnapshot::from_config)
            });

        // Gather budget counters from the session budget registry
        let (llm_rounds, tokens, cost) = self
            .session_budget
            .as_ref()
            .and_then(|b| b.snapshot_counters(&self.session_id.clone().unwrap_or_default()))
            .unwrap_or((0, 0, 0.0));

        SessionCheckpoint {
            // Persist the accumulated egress label sidecar so a resumed or
            // forked session withholds the same labeled content this live
            // session would (RFC data-envelopes §3.4). Clone is empty for
            // unconfigured deployments.
            egress_labels: self.egress_labels.clone(),
            egress_ask: self.egress_ask_state.clone(),
            history: history.to_vec(),
            turn_counter: self.turn_counter,
            loop_guard_state: self.guard.snapshot(),
            session_state: self.session_state,
            tool_tier_escalated: self.tool_tier_escalated,
            discovered_tools: Default::default(),
            blocked_state_event_emitted: self.blocked_state_event_emitted,
            agent_id: self.manifest.agent.id.clone(),
            session_id: self.session_id.clone().unwrap_or_default(),
            turn_id: turn_id.to_string(),
            workflow_id: self.workflow_id.clone(),
            task_id: self.task_id.clone(),
            runtime_lock_hash: self.runtime_lock_hash.clone(),
            constitution_version: self.constitution_version.clone(),
            constitution_digest: self.constitution_digest.clone(),
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
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: self.suppress_until_turn.load(std::sync::atomic::Ordering::Relaxed),
            trajectory_last_level: self.trajectory_monitor.last_level_as_string(),
            feedback_events: self.trajectory_monitor.feedback_snapshot(),
        }
    }

    /// Build and persist a checkpoint for the given yield reason.
    ///
    /// Single entry point for all checkpoint saves — replaces every inline
    /// `build_checkpoint` + `save_checkpoint_if_possible` pair in the execute loop.
    fn save_yield_checkpoint(
        &self,
        history: &[Message],
        turn_id: &str,
        yield_reason: YieldReason,
        pending_tool_state: Option<PendingToolState>,
    ) -> SessionCheckpoint {
        let cp = self.build_checkpoint(history, turn_id, yield_reason, pending_tool_state);
        if let Some(config) = self.config.as_ref() {
            if let Err(e) = save_checkpoint(config, &cp) {
                tracing::warn!(
                    target: "checkpoint",
                    session_id = %cp.session_id,
                    turn_id = %cp.turn_id,
                    error = %e,
                    "Failed to save session checkpoint"
                );
            }
        }
        // #742: set the session lifecycle state based on the yield reason.
        if let Some(gs) = self.gateway_store.as_ref() {
            let lifecycle = match &cp.yield_reason {
                YieldReason::ApprovalRequired { .. }
                | YieldReason::UserInputRequired { .. }
                | YieldReason::HumanEscalation { .. } => "awaiting_gate",
                // Distinct from "hibernated": nothing is being waited on. The
                // session is finished with its task and parked only so peers
                // can still reach it.
                YieldReason::Idle { .. } => "idle",
                _ => "hibernated", // Hibernation, WaitingForChild, Error, BudgetExhausted, etc.
            };
            if let Err(e) = gs.set_session_lifecycle_state(&cp.session_id, lifecycle) {
                tracing::warn!(
                    target: "lifecycle",
                    session_id = %cp.session_id,
                    lifecycle_state = %lifecycle,
                    error = %e,
                    "Failed to persist lifecycle state on yield"
                );
            }
        }
        cp
    }
    /// Save a yield checkpoint and return the original error.
    ///
    /// Centralises the recurring `save_yield_checkpoint` + `return Err(e)` pattern.
    /// Save a yield checkpoint and return the original error.
    ///
    /// Centralises the recurring `save_yield_checkpoint` + `return Err(e)` pattern.
    fn save_and_yield(
        &self,
        history: &[Message],
        turn_id: &str,
        reason: YieldReason,
        pending: Option<PendingToolState>,
        err: anyhow::Error,
    ) -> anyhow::Error {
        let _ = self.save_yield_checkpoint(history, turn_id, reason, pending);
        err
    }

    /// Root-session-tree budget exhaustion (C2 / #616). Identical to
    /// [`Self::save_and_yield`] with `YieldReason::BudgetExhausted`, but also
    /// records that the failure came from the **root** budget so the service
    /// layer can fire the one-time graceful root budget circuit breaker once the
    /// turn returns. Keyed off WHICH check failed — only the
    /// `self.root_session_budget` paths call this — never the per-session budget.
    fn save_and_yield_root_budget(
        &mut self,
        history: &[Message],
        turn_id: &str,
        err: anyhow::Error,
    ) -> anyhow::Error {
        self.root_budget_exhausted = true;
        let _ = self.save_yield_checkpoint(history, turn_id, YieldReason::BudgetExhausted, None);
        err
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

    /// The tool tier filter for the current turn — shared by the guidance
    /// gather and the advertised tool list so the two agree.
    fn compute_tier_filter(&self) -> crate::runtime::tools::ToolTierFilter {
        let pending_approvals = self.has_pending_approvals();
        let progressive = self
            .config
            .as_ref()
            .map(|c| c.prompt_budget.progressive_tool_disclosure)
            .unwrap_or(false);
        determine_tool_tier_filter(
            &self.manifest,
            self.session_id.as_deref(),
            pending_approvals,
            self.session_state,
            if progressive { self.tool_tier_escalated } else { true },
        )
    }

    /// Render tool-contributed prompt guidance (#463/#464) for the native tools
    /// that pass `tier_filter`, plus the builtin blocks. Empty when none apply.
    /// `active_tool_names` must be the FINAL advertised tool set (post
    /// MCP-merge/dedupe/cap), so `ToolPresent` gating matches what the model
    /// actually sees rather than the native-only candidate set.
    /// The concrete model id for this spawn. Agents declare a **preset**
    /// (`llm_preset`), not a pinned model — so the source of truth is the
    /// resolved inference profile (preset → concrete `llm_config`), not
    /// `manifest.llm_config` (normally `None`). Falls back to a legacy explicit
    /// `manifest.llm_config.model`, then `"unknown"`. Shared by tracing/
    /// context-window sizing and guidance (`model_family`).
    fn resolved_model_id(&self) -> String {
        // Preferred: the resolved profile's concrete model (from the preset).
        if let Some(profile) = self.resolved_inference.as_ref() {
            let m = profile.llm_config.model.trim();
            if !m.is_empty() && m != "unknown" {
                return m.to_string();
            }
        }
        // Legacy fallback: an explicit pinned model in the manifest.
        self.manifest
            .llm_config
            .as_ref()
            .map(|c| c.model.clone())
            .filter(|m| !m.is_empty() && m != "unknown")
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn render_tool_guidance(
        &self,
        tier_filter: &crate::runtime::tools::ToolTierFilter,
        active_tool_names: &[String],
    ) -> String {
        let mut blocks = self.registry.collect_guidance_blocks(&self.manifest, tier_filter);
        blocks.extend(crate::runtime::guidance::builtin_blocks());
        // Resolve the model id (preset-aware) so `ModelFamily` matches even for
        // preset-configured agents whose `llm_config.model` is empty (#465/#479).
        // `ModelFamily` substring-matches it (e.g. ["claude"] matches
        // "claude-opus-4-8"); "unknown"/empty → None so family guidance just
        // doesn't fire.
        let model = self.resolved_model_id();
        let model_family = match model.as_str() {
            "" | "unknown" => None,
            m => Some(m),
        };
        let ctx = crate::runtime::guidance::GuidanceContext {
            capabilities: &self.manifest.capabilities,
            active_tool_names,
            model_family,
            role: crate::runtime::context::role_from_manifest(&self.manifest),
        };
        crate::runtime::guidance::compose_guidance(&blocks, &ctx)
    }

    /// Run the agent loop until completion or guard trip.
    pub async fn execute_loop(&mut self) -> anyhow::Result<()> {
        let user_context = self.build_user_context_snippet();
        let memory_context = self.build_memory_context_snippet();
        let inlined_instructions = inline_extended(&self.instructions, self.extended_instructions.as_deref());
        // This pre-loop system message is replaced by the per-turn compose before
        // the model sees it, so ToolPresent gating (empty here) doesn't matter.
        let guidance_rendered = self.render_tool_guidance(&self.compute_tier_filter(), &[]);
        let mut system_instructions = compose_system_instructions_full(
            &inlined_instructions,
            &self.manifest,
            self.manifest
                .io
                .as_ref()
                .and_then(|io| io.output_policy.as_ref()),
            user_context.as_deref(),
            self.persona.as_deref(),
            Some(guidance_rendered.as_str()),
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
                let close_outcome = session_close_outcome_from_turn_outcome(&outcome);
                let _ = self.close_session(close_outcome);
                Ok(())
            }
            Err(e) => {
                let _ = self.close_session(SessionCloseOutcome::ExecuteLoopError);
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

        self.guard = loop_guard_from_config_and_manifest(
            self.config.as_deref(),
            &self.agent_dir,
            self.loop_guard_declaration.as_ref(),
            self.manifest.execution_mode,
        );
        self.llm_usage_last_run.clear();
        let session_id = self.ensure_session_id();
        let turn_id = self.next_turn_id();

        // OFP inbound federation: label the spawned session's first user turn
        // with the peer-supplied egress label (fail-closed default applied at
        // the wire handler). Mirrors local `agent_message` ingest (RFC §5.5).
        if let Some(label) = self.initial_ingest_egress_label.take() {
            if !label.is_unrestricted() {
                for msg in history.iter_mut() {
                    if msg.role == crate::llm::Role::User && msg.id.is_none() {
                        let mid = autonoetic_types::id_format::short_random_id("msg_");
                        msg.id = Some(mid.clone());
                        self.egress_labels.insert(mid, label.clone());
                        break;
                    }
                }
            }
        }

        // Session-level turn limits (issue #854).
        //
        // `max_session_turns` is a *soft* limit: reaching it raises a
        // `SessionContinue` approval and each operator clearance grants one more
        // window of that size. `max_session_turns_hard` is the *absolute*
        // ceiling those continuation approvals **cannot** lift — crossing it
        // terminates the session via `YieldReason::MaxTurnsReached` (a declared
        // budget-exhaustion termination reason under Ri-0.12). Only
        // emergency-stop or operator revoke can intervene past the hard cap;
        // the soft approval gate cannot. This binds delegated (child) sessions
        // exactly as it binds the root — the hard cap is not clearable by any
        // approval, visible to the operator or not.
        if let Some(cfg) = &self.config {
            let effective_turns = effective_max_session_turns(
                cfg.max_session_turns,
                self.loop_guard_declaration.as_ref(),
            );
            if effective_turns > 0 {
                let hard_cap = effective_max_session_turns_hard(
                    cfg.max_session_turns,
                    cfg.max_session_turns_hard,
                    self.loop_guard_declaration.as_ref(),
                );
                // turn_counter already includes the in-flight turn (next_turn_id
                // incremented above), so we trip only when *attempting* turn N+1
                // for an allowance of N.
                //
                // The hard cap is checked BEFORE the soft approval gate so no
                // number of continuation approvals can push execution past it:
                // once turn_counter exceeds `hard_cap` the session terminates
                // unconditionally.
                if hard_cap > 0 && self.turn_counter > hard_cap as u64 {
                    let blocked_turn = self.turn_counter;
                    tracing::warn!(
                        agent_id = %self.manifest.agent.id,
                        session_id = %session_id,
                        turn_counter = blocked_turn,
                        max_turns = effective_turns,
                        max_turns_hard = hard_cap,
                        "Session exceeded max_session_turns_hard; terminating \
                         (continuation approvals cannot lift the hard cap)"
                    );
                    self.emit_session_turn_hard_cap_event(
                        &session_id,
                        &turn_id,
                        effective_turns,
                        hard_cap,
                        blocked_turn,
                    );
                    let err = anyhow::anyhow!(
                        "Session {} exceeded max_session_turns_hard={} at turn {} \
                         (continuation approvals cannot lift the hard cap; only \
                         emergency-stop or operator revoke can intervene)",
                        session_id,
                        hard_cap,
                        blocked_turn
                    );
                    return Err(self.save_and_yield(
                        history,
                        &turn_id,
                        YieldReason::MaxTurnsReached,
                        None,
                        err,
                    ));
                }

                let approved_windows = self.approved_session_continue_count(&session_id)?;
                let allowed_turns =
                    (effective_turns as u64).saturating_mul(1 + approved_windows);
                if self.turn_counter > allowed_turns {
                    let blocked_turn = self.turn_counter;
                    match self.check_session_continue_gate(
                        cfg,
                        &session_id,
                        effective_turns,
                        blocked_turn,
                        &turn_id,
                    )? {
                        Some(request_id) => {
                            // Do not consume a turn when execution is blocked at the approval gate.
                            self.turn_counter = self.turn_counter.saturating_sub(1);
                            tracing::warn!(
                                agent_id = %self.manifest.agent.id,
                                session_id = %session_id,
                                turn_counter = blocked_turn,
                                max_turns = effective_turns,
                                max_turns_hard = hard_cap,
                                approved_windows = approved_windows,
                                approval_request_id = %request_id,
                                "Session reached max turns limit; approval required to continue"
                            );
                            // Option 3 (issue #854): when a *delegated* (child)
                            // session requests a 2nd+ continuation window, surface
                            // it at the root session so the operator/planner can
                            // see the child has been running for N continuation
                            // windows — visibility the per-child approval alone
                            // does not give the root when the child is deep in
                            // the tree.
                            if approved_windows >= 1
                                && crate::runtime::content_store::root_session_id(&session_id)
                                    != session_id.as_str()
                            {
                                self.emit_continuation_window_extended_event(
                                    &session_id,
                                    &turn_id,
                                    effective_turns,
                                    hard_cap,
                                    approved_windows,
                                    blocked_turn,
                                    &request_id,
                                );
                            }
                            let _ = self.save_yield_checkpoint(
                                history,
                                &turn_id,
                                YieldReason::ApprovalRequired {
                                    approval_request_id: request_id.clone(),
                                },
                                None,
                            );
                            return Ok(TurnOutcome::Suspended {
                                approval_request_id: request_id,
                            });
                        }
                        None => {
                            // Gate cleared via existing approval, session grant, or policy;
                            // continue this turn without suspending.
                            tracing::info!(
                                agent_id = %self.manifest.agent.id,
                                session_id = %session_id,
                                turn_counter = blocked_turn,
                                max_turns = effective_turns,
                                approved_windows = approved_windows,
                                "Session continue gate cleared; continuing without suspension"
                            );
                        }
                    }
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
                let live_html_on_update = self
                    .config
                    .as_ref()
                    .map(|cfg| cfg.session_report.live_html_on_update)
                    .unwrap_or(false);
                match crate::runtime::session_report::SessionReportWriter::open_with_options(
                    gw,
                    &session_id,
                    agent_id,
                    live_html_on_update,
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
                    let drift_payload = serde_json::json!({
                        "drift_field": drift_field,
                        "locked_build_sha256": drift.locked_build_sha256,
                        "current_build_sha256": drift.current_build_sha256,
                        "locked_binary_sha256": drift.locked_binary_sha256,
                        "current_binary_sha256": drift.current_binary_sha256,
                        "override": allow,
                    });
                    let _ = tracer.log_event(
                        "runtime_lock_drift",
                        if allow { "override" } else { "rejected" },
                        status,
                        Some(drift_payload.clone()),
                    );
                    // Also surface it on the canonical timeline so the room shows
                    // it (#367) — previously causal-only. Emitted before the
                    // reject `bail!` below so a drift-killed session isn't silent.
                    tracer.record_runtime_lock_drift(drift_payload, allow);
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
                        crate::runtime_lock::DriftSkippedReason::LockUnpinned => {
                            ("lock_unpinned", "runtime.lock has no pinned build sha256")
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

            // Constitution drift notice (#821): mirrors the runtime_lock
            // drift check above in shape (same `drift_checked` gate, same
            // causal-event style) but NEVER bails — Ri-0.5
            // notice-before-degradation and non-retroactivity are about the
            // agent *knowing* the law changed, not about freezing it.
            if let Some((current_version, current_digest)) =
                crate::constitution_digest::try_constitution_pin()
            {
                if let Some(notice) = detect_constitution_drift(
                    self.constitution_version.as_deref(),
                    self.constitution_digest.as_deref(),
                    &current_version,
                    &current_digest,
                ) {
                    let _ = tracer.log_event(
                        "constitution_drift",
                        "notice",
                        autonoetic_types::causal_chain::EntryStatus::Success,
                        Some(notice.payload),
                    );
                    self.constitution_drift_notice = Some(notice.notice_text);
                    // Adopt the new pin now that the session knowingly runs
                    // under the new law — notice once per change, not every
                    // turn: the next resume's pin already matches current.
                    self.constitution_version = Some(current_version);
                    self.constitution_digest = Some(current_digest);
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

            // Capture the constitution pin (version + digest) at session
            // start (#821), mirroring runtime_lock_hash: records which law
            // admitted this session. Only fires for a genuinely fresh
            // session — a resumed session already has its pin restored by
            // `SessionCheckpoint::restore_into`, and the drift check above
            // runs before this block, so it has already adopted any new pin
            // by the time we get here. `None` when the constitution runtime
            // was never initialized (common in unit tests).
            if self.constitution_version.is_none() {
                if let Some((version, digest)) = crate::constitution_digest::try_constitution_pin()
                {
                    self.constitution_version = Some(version);
                    self.constitution_digest = Some(digest);
                }
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
                    // Cross-agent taint apply (RFC §5.5, slice 4b): if the sender
                    // stamped the payload with a restrictive taint, label this
                    // ingested message so the chokepoint withholds it from a sink
                    // the taint excludes — a tainted sibling's content can't reach
                    // a remote-pinned recipient. Mint a stable msg id (§3.4) as
                    // the label's join key.
                    let mut user_msg = Message::user(text.clone());
                    if let Some(label) = msg
                        .egress_label
                        .as_ref()
                        .filter(|l| !l.is_unrestricted())
                    {
                        let mid = autonoetic_types::id_format::short_random_id("msg_");
                        user_msg.id = Some(mid.clone());
                        self.egress_labels.insert(mid, label.clone());
                    }
                    history.push(user_msg);

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

                    // Surface peer traffic to the operator (#896). The causal
                    // event above is the audit record; without a timeline row an
                    // agent-to-agent message reads as anonymous user text in the
                    // room, indistinguishable from something the operator typed.
                    // Emitted here rather than at the ingest, because this is
                    // where the sender and body are both known.
                    let event = crate::runtime::session_timeline::peer_message_event(
                        &session_id,
                        &msg.sender_agent_id,
                        &msg.sender_session_id,
                        &msg.message_id,
                        &crate::log_redaction::redact_text_for_logs(&msg.message),
                    );
                    if let Err(e) = store.create_live_digest_event(&event) {
                        tracing::debug!(
                            target: "session_timeline",
                            error = %e,
                            message_id = %msg.message_id,
                            "peer message timeline emit failed"
                        );
                    }

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

        let model = self.resolved_model_id();
        let temperature = self
            .manifest
            .llm_config
            .as_ref()
            .map(|c| c.temperature as f32);
        let context_window_resolved = resolve_context_window_for_run(
            &self.manifest,
            &model,
            self.openrouter_catalog.as_ref(),
            self.local_model_context_cache.as_ref(),
            self.config.as_deref(),
        )
        .await;
        if context_window_resolved.is_none() {
            tracing::warn!(
                target: "autonoetic::prompt_budget",
                agent_id = %self.manifest.agent.id,
                model = %model,
                "Context window size is UNKNOWN for model '{}'. Falling back to conservative \
                 default ({} tokens). If the system prompt + tool definitions alone exceed this, \
                 the context governor will fail on every turn. Set 'context_window_tokens' in the \
                 llm_preset configuration or the AUTONOETIC_LLM_CONTEXT_WINDOW environment variable.",
                model,
                crate::runtime::prompt_budget::FALLBACK_CONTEXT_WINDOW,
            );
        }
        let mut latest_assistant_text: Option<String> = None;
        // Tracks whether the EndTurn branch decided to suspend as WaitingForChild
        // (pending async children). The post-loop outcome builder inspects this
        // to return `TurnOutcome::WaitingForChild` instead of `Completed`. Without
        // it, an agent that spawns async children and ends its turn would be
        // marked task-completed prematurely — its own follow-up steps (smoke
        // test, promote, ...) would never run (#845).
        let mut end_turn_waiting_for_child = false;
        let has_declared_output_contract = self
            .manifest
            .io
            .as_ref()
            .and_then(|io| io.returns.as_ref())
            .is_some();
        let policy = PolicyEngine::new(self.manifest.clone());
        let max_empty_other_retries = max_other_empty_retries();
        let mut empty_other_retries_used = 0usize;
        let mut digest_turn_active = false;
        self.ri_0_6_snapshot_checked = false;
        let root_session_id = crate::runtime::content_store::root_session_id(&session_id);
        let allow_unpriced_budget = self.manifest.capabilities.iter().any(|c| {
            matches!(
                c,
                autonoetic_types::capability::Capability::BudgetNoPriceAvailableAllow
            )
        });

        loop {
            // #719: mechanical re-execution of an operator-approved call on
            // resume. This MUST run before pre_turn_checks: the promote was
            // already operator-approved, so budget / loop-guard checks (designed
            // for LLM reasoning loops, not for executing already-authorized
            // actions) must not prevent it. Budget is still enforced inside
            // handle_tool_batch via reserve_tool_invocations, and a successful
            // promote calls register_progress so the guard won't trip. If the
            // promote suspends again (second gate), handle_tool_batch saves a
            // fresh checkpoint with the new pending state.
            //
            // Before this ordering, pre_turn_checks could save_and_yield with
            // pending_tool_state=None on a budget/guard trip, clobbering the
            // approval checkpoint and silently losing the approved promote.
            if let Some((assistant_msg, pending_calls)) = self.resume_pending_batch.take() {
                if !digest_turn_active {
                    tracer.start_digest_turn()?;
                    digest_turn_active = true;
                }
                if let Some(outcome) = self
                    .handle_tool_batch(
                        pending_calls,
                        history,
                        &turn_id,
                        &mut tracer,
                        &mut mcp_runtime,
                        &mut disclosure_state,
                        secret_store.as_mut(),
                        &active_agent_dir,
                        assistant_msg,
                        &mut digest_turn_active,
                    )
                    .await?
                {
                    return Ok(outcome);
                }
                continue;
            }

            if let Some(outcome) = self.pre_turn_checks(history, &turn_id).await? {
                return Ok(outcome);
            }


            if !digest_turn_active {
                tracer.start_digest_turn()?;
                digest_turn_active = true;
            }

            // The tool list (below) and tool-contributed guidance share this tier
            // filter; guidance is rendered AFTER the tool list so `ToolPresent`
            // gates on the final advertised set (#463/#464).
            let tier_filter = self.compute_tier_filter();

            let mut tools: Vec<ToolDefinition> = {
                let mut t: Vec<ToolDefinition> = mcp_runtime
                    .tool_definitions()?
                    .into_iter()
                    .filter(|def| policy.can_invoke_tool(&def.name).is_allowed())
                    .filter(|def| tier_filter.allows(&def.name))
                    .filter(|def| !crate::runtime::tools::is_tool_excluded_public(&def.name, &self.manifest))
                    .collect();
                t.extend(
                    self.registry
                        .available_definitions_filtered(&self.manifest, Some(&tier_filter)),
                );
                // Add tools explicitly discovered via tool_discover, bypassing tier filter.
                // Native capability gating already ran in available_definitions_filtered
                // (is_available). can_invoke_tool is intentionally NOT re-applied here:
                // it gates SandboxFunctions MCP prefixes (mcp_*), not native tool names,
                // and would silently drop tier-discovered tools like web_search.
                //
                // Exception: in degraded mode (P-7.18), the tier filter is a safety
                // boundary — discovered tools must NOT bypass it. Otherwise the root
                // session can use previously-discovered Workflow/Specialized tools
                // (agent_spawn, web_search, etc.) even after degradation, defeating
                // the purpose of the circuit breaker. promotion_record is still
                // allowed via the degraded filter's own exemption for promotion-gate
                // agents.
                let skip_discover_bypass = self.session_state
                    == autonoetic_types::agent::SessionState::Degraded;
                if !self.discovered_tools.is_empty() && !skip_discover_bypass {
                    let all_defs: Vec<ToolDefinition> = self.registry
                        .available_definitions_filtered(&self.manifest, None);
                    for def in &all_defs {
                        if t.iter().any(|d| d.name == def.name) {
                            continue;
                        }
                        let matches = self.discovered_tools.iter().any(|pattern| {
                            if let Some(prefix) = pattern.strip_suffix('*') {
                                def.name.starts_with(prefix)
                            } else {
                                def.name == *pattern
                            }
                        });
                        if matches {
                            t.push(def.clone());
                        }
                    }
                }
                // Deduplicate by name (MCP tools may overlap with native tools).
                // First occurrence (MCP) wins; native duplicates are dropped.
                {
                    let mut seen = std::collections::HashSet::new();
                    t.retain(|def| seen.insert(def.name.clone()));
                }
                // Cap tool count: drop lowest-priority tier tools first when
                // the deduplicated list exceeds max_tool_definitions.
                // Tools matched by tool_discover patterns are never dropped.
                let max_tools = self
                    .config
                    .as_ref()
                    .map(|c| c.prompt_budget.max_tool_definitions)
                    .unwrap_or(0);
                if max_tools > 0 && t.len() > max_tools {
                    t = crate::runtime::prompt_budget::cap_tool_definitions_preserving_discovered(
                        t,
                        max_tools,
                        &self.discovered_tools,
                    );
                }
                t
            };

            // Update system message — composed after the tool list so
            // tool-contributed guidance (#463/#464) gates `ToolPresent` on the
            // FINAL advertised tool set the model actually sees.
            let advertised_tool_names: Vec<String> =
                tools.iter().map(|t| t.name.clone()).collect();
            let guidance_rendered = self.render_tool_guidance(&tier_filter, &advertised_tool_names);
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
                Some(guidance_rendered.as_str()),
            );
            // Prompt-cache boundary (#): everything composed so far — foundation
            // doctrine, SKILL instructions, tool/builtin guidance, output
            // contract, persona, user context — is byte-identical across turns
            // in this session, so it is safe to mark as a provider cache prefix.
            // The volatile tails appended below (memory context, degradation
            // notice, per-turn re-signed state attestation) must NOT be cached.
            let system_cache_prefix_bytes = if self
                .config
                .as_ref()
                .map(|c| c.prompt_budget.prompt_cache_enabled)
                .unwrap_or(true)
            {
                Some(system_instructions.len())
            } else {
                None
            };
            if let Some(ref snippet) = memory_context {
                system_instructions.push_str("\n\n");
                system_instructions.push_str(snippet);
            }
            if let Some(notice) = self.build_degradation_notice_tail(&session_id)? {
                system_instructions.push_str("\n\n");
                system_instructions.push_str(&notice);
            }
            // Constitution drift notice (#821): a one-shot system-instruction
            // tail, injected the wake it is detected and then `.take()`n so
            // it does not repeat on every subsequent turn (unlike the
            // degradation notice above, which re-queries persisted state
            // every turn because degraded-mode is an ongoing condition —
            // drift is a single fact to acknowledge, not a standing state).
            if let Some(notice) = self.constitution_drift_notice.take() {
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
            // Exactly one system message, at the front (stale ones removed).
            history.retain(|m| !matches!(m.role, crate::llm::Role::System));
            history.insert(0, Message::system(&system_instructions));

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
            // `utilization_pct` is a 0-100 percentage; the monitor expects a
            // 0.0-1.0 fraction.
            self.last_context_utilization = budget_breakdown.utilization_pct.map(|v| v as f32 / 100.0);

            // --- Budget Enforcement + Context Compression (Context Governor) ---
            //
            // The governor only needs to run when the prompt exceeds either the
            // hard effective limit or the configured soft budget. On the common
            // under-budget path we skip the GovernorContext allocation entirely
            // to avoid cloning the full history + tools every round.
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
                    .unwrap_or_else(|| {
                        let default_window: usize =
                            crate::runtime::prompt_budget::FALLBACK_CONTEXT_WINDOW;
                        default_window.saturating_sub(margin)
                    });
                let soft_budget = self.config.as_ref()
                    .and_then(|c| c.prompt_budget.soft_budget_tokens)
                    .map(|sb| sb as usize);
                let total_tokens = budget_breakdown.total_tokens;
                let hard_ok = total_tokens <= effective_limit;
                let soft_ok = soft_budget.map(|sb| total_tokens <= sb).unwrap_or(true);

                if !hard_ok || !soft_ok {
                    let compression_cfg = self.config.as_ref().map(|c| &c.context_compression);
                    let plan_anchor = self
                        .gateway_store
                        .as_ref()
                        .and_then(|store| {
                            // `session_id` may be a child/forked id ("root/x"); the
                            // workflow index is keyed on the *root* id.
                            let root_session_id =
                                crate::runtime::content_store::root_session_id(&session_id)
                                    .to_string();
                            let wf_id = self.workflow_id.clone().or_else(|| {
                                crate::scheduler::resolve_workflow_id_for_root_session(
                                    self.config.as_ref()?,
                                    &root_session_id,
                                )
                                .ok()
                                .flatten()
                            })?;
                            let plan = store.load_active_plan_for_workflow(&wf_id).ok().flatten()?;
                            Some(plan.compact_summary())
                        });
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
                        plan_anchor,
                        self.capsule_state.clone(),
                    );
                    // Thread the session's egress labels into the governor so
                    // the capsule strategy's compression-eligibility gate can
                    // refuse to summarize a local_only-tainted band on a remote
                    // compression preset (RFC §5.7 rule 1).
                    ctx.egress_labels = self.egress_labels.clone();
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
                            gateway_store: self.gateway_store.clone(),
                            agent_id: Some(self.manifest.agent.id.clone()),
                        })
                    } else {
                        ContextGovernor::new(&GovernorConfig {
                            http_client: self.http_client.clone(),
                            presets: self.config.as_ref().map(|c| c.llm_presets.clone())
                                .unwrap_or_default(),
                            gateway_dir: self.gateway_dir.clone(),
                            gateway_store: self.gateway_store.clone(),
                            agent_id: Some(self.manifest.agent.id.clone()),
                        })
                    };
                    match governor.govern(&mut ctx).await {
                        Ok(GovernorResult::Recovered { actions_taken }) => {
                            tracing::info!(
                                target: "autonoetic::context_governor",
                                actions = ?actions_taken,
                                "ContextGovernor recovered within budget"
                            );
                            // #842: surface governor activity in the session
                            // report so operators can quantify savings.
                            if let Some(report) = &self.live_report {
                                let strategy_names: Vec<String> = actions_taken
                                    .iter()
                                    .map(|a| a.strategy.clone())
                                    .collect();
                                let mut writer = report.lock().unwrap_or_else(|e| e.into_inner());
                                if let Err(e) = writer.record_context_governor(
                                    total_tokens,
                                    ctx.breakdown.total_tokens,
                                    &strategy_names,
                                ) {
                                    tracing::warn!(
                                        target: "session_report",
                                        error = %e,
                                        "Failed to record context governor metrics"
                                    );
                                }
                            }
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
                            // Don't knowingly send a prompt that exceeds the model's
                            // context window. Use the POST-governor estimate
                            // (`ctx.breakdown.total_tokens`, after every reduction
                            // strategy ran) — not the pre-governor `budget_breakdown`.
                            // If it still exceeds the assumed window
                            // (`effective_limit + margin`), sending is a guaranteed
                            // provider context-overflow, so surface a
                            // `context_overflow:`-tagged error here to route into the
                            // scheduler's recovery (retry once with the aggressive
                            // pipeline; a second overflow is terminal) instead of
                            // paying a round-trip for a 500 we can already predict.
                            // Prompts only within the safety margin (still under the
                            // window) fall through and are sent as before.
                            let post_governor_tokens = ctx.breakdown.total_tokens;
                            if let Some(err) =
                                overflow_presend_block(post_governor_tokens, effective_limit, margin)
                            {
                                let _ = tracer.log_event(
                                    "context_governor",
                                    "overflow_blocked_send",
                                    autonoetic_types::causal_chain::EntryStatus::Error,
                                    Some(serde_json::json!({
                                        "estimated_tokens": post_governor_tokens,
                                        "assumed_window": effective_limit.saturating_add(margin),
                                        "effective_limit": effective_limit,
                                        "margin_tokens": margin,
                                        "overflow_recovery": self.overflow_recovery,
                                    })),
                                );
                                return Err(err);
                            }
                        }
                        Ok(GovernorResult::WithinBudget) => {
                            // The ContextGovernor can return WithinBudget when a soft
                            // budget is configured but the prompt is already within
                            // both budgets after accounting for rounding. Treat the
                            // same as the local under-budget fast path for pressure
                            // warnings below.
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "autonoetic::context_governor",
                                error = %e,
                                "ContextGovernor error, falling through without reduction"
                            );
                            let _ = tracer.log_event(
                                "context_governor",
                                "error",
                                autonoetic_types::causal_chain::EntryStatus::Error,
                                Some(serde_json::json!({
                                    "error": crate::log_redaction::redact_text_for_logs(&e.to_string()),
                                })),
                            );
                        }
                    }
                    *history = ctx.history;
                    tools = ctx.tools;
                    // Round-trip synthesized compression-block labels from the
                    // governor (RFC §5.7 rule 2) so the next completion's
                    // chokepoint / routing see them.
                    self.egress_labels = ctx.egress_labels;
                } else {
                    tracing::debug!(
                        target: "autonoetic::context_governor",
                        total_tokens,
                        effective_limit,
                        soft_budget,
                        "Context is within budget; skipping governor"
                    );
                }

                // Emit a TUI-visible warning card when the estimated prompt is
                // close to overflowing. Uses a dedup flag so it fires once per
                // pressure buildup cycle. This is independent of whether the
                // governor ran.
                if effective_limit > 0 {
                    let ratio = total_tokens as f64 / effective_limit as f64;
                    if ratio >= 0.85 {
                        if !self.pressure_high_warned {
                            self.pressure_high_warned = true;
                            if let (Some(config), Some(store), Some(wf_id)) =
                                (self.config.as_deref(), self.gateway_store.as_deref(), self.workflow_id.as_deref())
                            {
                                let pct = (ratio * 100.0) as u32;
                                let _ = crate::scheduler::append_workflow_event(
                                    config,
                                    Some(store),
                                    &autonoetic_types::workflow::WorkflowEventRecord {
                                        event_id: autonoetic_types::id_format::short_random_id("ctxp-"),
                                        workflow_id: wf_id.to_string(),
                                        task_id: self.task_id.clone(),
                                        event_type: "context.pressure_high".to_string(),
                                        agent_id: Some(self.manifest.agent.id.clone()),
                                        payload: serde_json::json!({
                                            "status": "pressure_high",
                                            "estimated_tokens": total_tokens,
                                            "effective_limit": effective_limit,
                                            "utilization_pct": pct,
                                            "context_window": budget_breakdown.context_window,
                                            "margin_tokens": margin,
                                        }),
                                        occurred_at: chrono::Utc::now().to_rfc3339(),
                                    },
                                );
                            }
                        }
                    } else if ratio < 0.70 {
                        self.pressure_high_warned = false;
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
                egress_class: None,
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

            // RFC §8: when the operator declassified this session to
            // RemoteModel, the chokepoint bypass flag rides alongside the
            // label map — authoritative gateway state, recomputed per request
            // from the grant table.
            let declassified_sinks_meta: Option<serde_json::Value> = if self
                .egress_labels
                .is_empty()
            {
                None
            } else {
                self.gateway_store.as_ref().and_then(|store| {
                    let root = crate::runtime::content_store::root_session_id(&session_id);
                    crate::runtime::egress_labeler::session_sink_declassified(
                        store,
                        &session_id,
                        root,
                        autonoetic_types::egress::Sink::RemoteModel,
                    )
                    .then(|| serde_json::json!(["remote_model"]))
                })
            };

            let req = CompletionRequest {
                model: routed_model.clone(),
                // Sanitize the wire-format history: strip reasoning content
                // and truncate large tool results, while leaving the stored
                // `history` untouched for checkpoints and exports.
                messages: sanitize_history_for_request(
                    history,
                    &HistorySanitizeOptions {
                        strip_reasoning: self
                            .config
                            .as_ref()
                            .map(|c| c.prompt_budget.strip_reasoning_from_request)
                            .unwrap_or(false),
                        max_tool_result_chars: self
                            .config
                            .as_ref()
                            .map(|c| c.prompt_budget.max_tool_result_chars)
                            .unwrap_or(2000),
                        dedup_tool_results: self
                            .config
                            .as_ref()
                            .map(|c| c.prompt_budget.dedup_tool_results)
                            .unwrap_or(true),
                        collapse_repeated_errors: self
                            .config
                            .as_ref()
                            .map(|c| c.prompt_budget.collapse_repeated_errors)
                            .unwrap_or(true),
                    },
                ),
                tools,
                max_tokens: None,
                temperature,
                // Attach the session's egress label map (RFC §5.2) so the
                // EgressChokepointDriver wrapper can substitute indications for
                // tool-result messages whose label excludes the target sink.
                // Only attached when labels exist — keeps the common
                // (unconfigured) case at zero cost (metadata stays None → the
                // wrapper's fast path fires, no clone). The declassified-sinks
                // bypass flag (RFC §8) rides alongside when set.
                metadata: if self.egress_labels.is_empty() {
                    None
                } else {
                    let mut m = std::collections::HashMap::new();
                    m.insert(
                        crate::llm::egress_chokepoint::EGRESS_LABELS_KEY.to_string(),
                        serde_json::to_value(&self.egress_labels).unwrap_or_default(),
                    );
                    if let Some(v) = &declassified_sinks_meta {
                        m.insert(
                            crate::llm::egress_chokepoint::EGRESS_DECLASSIFIED_SINKS_KEY
                                .to_string(),
                            v.clone(),
                        );
                    }
                    Some(m)
                },
                thinking: routed_llm_cfg.thinking.clone(),
                // Stable per-session key so providers that support prompt
                // caching reuse the cached prompt prefix across turns.
                prompt_cache_key: Some(session_id.clone()),
                // Cache boundary: the leading `system_cache_prefix_bytes` of the
                // system message are stable across turns; cache-capable drivers
                // put a cache_control breakpoint there (Anthropic / OpenRouter
                // Claude+Gemini). Clamped to the actual system-message length in
                // case sanitization changed it.
                system_cache_prefix_bytes: system_cache_prefix_bytes.map(|n| {
                    n.min(
                        history
                            .iter()
                            .find(|m| m.role == crate::llm::Role::System)
                            .map(|m| m.content.len())
                            .unwrap_or(0),
                    )
                }),
            };

            // --- Pre-process hook: transform input before LLM call ---
            let pre_hook = self.middleware.pre_process.as_ref();
            let mut req = if let Some(pre_hook) = pre_hook {
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
            // Egress security boundary (RFC §5.2): the pre-hook can replace the
            // entire CompletionRequest, which would drop the egress-label
            // metadata and let labeled content reach a disallowed sink. Re-
            // attach the session's label map whenever it's missing, so hooks
            // cannot accidentally or intentionally strip the chokepoint. The
            // label map is the gateway's, not the hook's, to manage.
            if !self.egress_labels.is_empty() {
                let needs_reattach = req
                    .metadata
                    .as_ref()
                    .map(|m| !m.contains_key(crate::llm::egress_chokepoint::EGRESS_LABELS_KEY))
                    .unwrap_or(true);
                if needs_reattach {
                    let meta = req.metadata.get_or_insert_with(std::collections::HashMap::new);
                    meta.insert(
                        crate::llm::egress_chokepoint::EGRESS_LABELS_KEY.to_string(),
                        serde_json::to_value(&self.egress_labels).unwrap_or_default(),
                    );
                }
                // The bypass flag is authoritative gateway state as well:
                // overwrite whatever the pre-hook left, so a hook can neither
                // strip the label map nor forge a declassification bypass
                // (RFC §2.1 — the label plane is gateway-managed).
                let meta = req.metadata.get_or_insert_with(std::collections::HashMap::new);
                match &declassified_sinks_meta {
                    Some(v) => {
                        meta.insert(
                            crate::llm::egress_chokepoint::EGRESS_DECLASSIFIED_SINKS_KEY
                                .to_string(),
                            v.clone(),
                        );
                    }
                    None => {
                        meta.remove(
                            crate::llm::egress_chokepoint::EGRESS_DECLASSIFIED_SINKS_KEY,
                        );
                    }
                }
            }

            // --- Skip LLM if signaled by pre-process hook ---
            // The hook can return a response in metadata.assistant_reply and set metadata.skip_llm: true
            let skip_llm = req
                .metadata
                .as_ref()
                .and_then(|m| m.get("skip_llm"))
                .and_then(|v| v.as_bool())
                == Some(true);

            let mut actual_model = routed_model.clone();
            // LLM-response label (RFC §4.5): the label the assistant message
            // this completion produces will carry — the intersection of the
            // labels of the envelopes actually *included* in the request (i.e.
            // cleared for the sink the completion ran on; withheld ones were
            // replaced by indications and so did not shape the output). Computed
            // inside the completion branch below where the effective sink is
            // known, then attached to the committed assistant message so a later
            // request to an ineligible sink withholds it (the tainted-summary
            // case, §5.6 step 4). `None` for a clean turn.
            let mut response_egress_label: Option<autonoetic_types::egress::EgressLabel> = None;
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
                    reasoning_details: None,
                    usage: crate::llm::TokenUsage::default(),
                    stop_reason: crate::llm::StopReason::EndTurn,
                }
            } else {
                tracing::debug!("Calling LLM");
                let fallback_chain: Vec<(String, String, String)> = routing_decision_json
                    .as_ref()
                    .map(|d| d.fallback_chain.clone())
                    .unwrap_or_default();

                // Taint-following routing (RFC §5.3): when the batch produced
                // since the last completion carries taint, pick an eligible
                // driver for THIS completion and filter the failover chain to
                // eligible presets; a clean (unrestricted) batch is a zero-cost
                // no-op that keeps the primary driver and the full chain.
                //
                // Consume the batch here — replace it with `unrestricted` so a
                // completion that ends the turn WITHOUT tool use (EndTurn /
                // StopSequence, which never reaches the tool-batch merge below)
                // doesn't leave a stale taint that would wrongly route the next
                // completion. The merge below re-arms it only if this turn's
                // tools produce fresh labeled results.
                let batch_taint = std::mem::replace(
                    &mut self.pending_batch_taint,
                    autonoetic_types::egress::EgressLabel::unrestricted(),
                );
                let primary_egress_class = routed_llm_cfg.egress_class;
                // The primary's *preset* identity for the audit event (not a
                // model name), so `chosen_preset` is consistent with the
                // reroute path and "why did turn N run here?" reads uniformly.
                let primary_preset_label: String = matched_entry
                    .as_ref()
                    .map(|e| e.preset_name.clone())
                    .or_else(|| {
                        self.manifest
                            .llm_config
                            .as_ref()
                            .and_then(|c| c.routing_preset.clone())
                    })
                    .unwrap_or_else(|| routed_model.clone());
                let (primary_egress_driver, egress_effective_class, fallback_chain) = {
                    // RFC §5.4 rung 1: a session-policy provider constraint
                    // (`local_only`) restricts provider *selection* even for
                    // clean batches, so the unrestricted fast path no longer
                    // applies. One indexed store read per completion.
                    let provider_constraint = match self.gateway_store.as_ref() {
                        None => None,
                        Some(store) => match store.get_egress_session_policy(
                            crate::runtime::content_store::root_session_id(&session_id),
                        ) {
                            Ok(stored) => stored.and_then(|s| s.policy.provider_constraint),
                            Err(e) => {
                                // Fail closed (RFC §2.2): an unreadable policy
                                // must not silently drop a declared constraint
                                // and route a private session remote — route as
                                // if `local_only` until the store reads again.
                                tracing::warn!(
                                    target: "egress",
                                    error = %e,
                                    session_id = %session_id,
                                    "session egress policy read failed — treating provider_constraint as local_only"
                                );
                                Some(autonoetic_types::egress::ProviderConstraint::LocalOnly)
                            }
                        },
                    };
                    if batch_taint.is_unrestricted() && provider_constraint.is_none() {
                        (None, primary_egress_class, fallback_chain)
                    } else {
                        // #968: the primary is *pinned* when it did not come
                        // from a per-completion routing strategy (agent
                        // manifest `llm_preset`, session override, or legacy
                        // fixed model) — a pinned primary × tainted batch is a
                        // conflict the operator must resolve, not a silent
                        // reroute.
                        let primary_pinned = match self.resolved_inference.as_ref() {
                            Some(profile) => !profile.is_routing_preset,
                            None => self
                                .manifest
                                .llm_config
                                .as_ref()
                                .and_then(|c| c.routing_preset.clone())
                                .is_none(),
                        };
                        let sel = self.plan_egress_routing(
                            &batch_taint,
                            &primary_preset_label,
                            primary_egress_class,
                            &fallback_chain,
                            &session_id,
                            &turn_id,
                            provider_constraint,
                            primary_pinned,
                        );
                        if let Some(ask_id) = sel.pending_ask {
                            // Pinned preset × tainted batch — the operator must
                            // choose (RFC §5.3 / #968): suspend the turn on the
                            // filed interaction; it resumes when answered and
                            // honors the choice at routing time.
                            return Err(self.save_and_yield(
                                history,
                                &turn_id,
                                YieldReason::UserInputRequired {
                                    interaction_id: ask_id,
                                },
                                None,
                                anyhow::anyhow!(
                                    "egress_pin_ask_filed: the pinned preset conflicts with \
                                     this turn's batch taint; the operator must choose \
                                     (declassify / run local / abort)."
                                ),
                            ));
                        }
                        if let Some(reason) = sel.refuse_reason {
                            // No eligible provider for a tainted batch — refuse
                            // the turn with a path forward, never ship taint to
                            // an ineligible provider (RFC §5.3, fail-closed).
                            return Err(self.save_and_yield(
                                history,
                                &turn_id,
                                YieldReason::Error(reason.clone()),
                                None,
                                anyhow::anyhow!(reason),
                            ));
                        }
                        // A reroute runs on a different model — reflect it in
                        // cost/tracing (the driver ignores `req.model` and uses
                        // its own).
                        if let Some(m) = sel.effective_model.clone() {
                            actual_model = m;
                        }
                        (sel.primary_driver, sel.effective_class, sel.fallback_chain)
                    }
                };

                let mut last_err = None;
                if let Err(e) = self
                    .enforce_cost_catalog_preflight(&actual_model, allow_unpriced_budget)
                    .await
                {
                    return Err(self.save_and_yield(history, &turn_id, YieldReason::BudgetExhausted, None, e));
                }
                if let Some(root_budget) = self.root_session_budget.clone() {
                    if let Err(e) = root_budget.reserve_llm_round(root_session_id) {
                        return Err(self.save_and_yield_root_budget(history, &turn_id, e));
                    }
                }
                // Run the primary completion on the taint-following driver when
                // the batch forced a reroute; otherwise the session's primary
                // (`self.llm`). Both are already wrapped by the egress
                // chokepoint at build time (RFC §5.2), so filtering applies
                // either way.
                let primary_driver = primary_egress_driver.as_ref().unwrap_or(&self.llm);
                let response = primary_driver.complete(&req).await;
                // Egress chokepoint audit (RFC §9.1): emit the causal events
                // for what the wrapper withheld. Only fires when labels are
                // attached to this request (unconfigured deployments skip
                // entirely). Best-effort — a failed write is logged, not fatal.
                if !self.egress_labels.is_empty() {
                    if let Some(store) = self.gateway_store.as_ref() {
                        // Use the *effective* class (rerouted preset's, or the
                        // primary's) so the audit records the sink content
                        // actually reached.
                        let sink = egress_effective_class
                            .map(|c| c.as_sink())
                            .unwrap_or(autonoetic_types::egress::Sink::RemoteModel);
                        let report = crate::llm::egress_chokepoint::compute_filter_report(
                            &req, sink,
                        );
                        crate::runtime::egress_labeler::emit_chokepoint_events(
                            store,
                            &report,
                            &routed_model,
                            &session_id,
                            &self.manifest.agent.id,
                            Some(&turn_id),
                        );
                        // Filtered wire view (RFC §9.2): record the per-request
                        // "what left" summary on the session tracer so it sits
                        // alongside the response log, not only in gateway.db.
                        // Best-effort — a failed log is not fatal.
                        let _ = tracer.log_egress_request_filtered(&routed_model, &report);
                    }
                }
                // Compute the LLM-response label (RFC §4.5) for the assistant
                // message this completion will produce: the intersection of the
                // labels of the envelopes cleared for this sink (those actually
                // included — withheld ones were replaced by indications and did
                // not shape the output). A local turn that saw `local_only`
                // email yields a `local_only` response; a remote turn that saw
                // only indications yields a clean one.
                if !self.egress_labels.is_empty() {
                    let sink = egress_effective_class
                        .map(|c| c.as_sink())
                        .unwrap_or(autonoetic_types::egress::Sink::RemoteModel);
                    let mut acc = autonoetic_types::egress::EgressLabel::unrestricted();
                    let mut any = false;
                    for m in history.iter() {
                        if let Some(key) =
                            crate::runtime::egress_labeler::message_egress_key(m)
                        {
                            if let Some(lbl) = self.egress_labels.get(key) {
                                if lbl.allows(sink) {
                                    acc = acc.restrict(lbl);
                                    any = true;
                                }
                            }
                        }
                    }
                    if any && !acc.is_unrestricted() {
                        response_egress_label = Some(acc);
                    }
                }
                match response {
                    Ok(resp) => {
                        self.guard.register_llm_success();
                        resp
                    }
                    Err(e) => {
                        self.guard.register_llm_failure();
                        let _ = tracer.log_llm_request_failed(&actual_model, &e);

                        // RFC #779 Part E.2: only fail over on transient errors.
                        // A 400/401/403 is deterministic — the same request to a
                        // different provider will fail differently, not succeed.
                        if !crate::llm::is_failover_eligible_error(&e) {
                            return Err(e);
                        }

                        if fallback_chain.is_empty() {
                            return Err(e);
                        }
                        tracing::warn!(
                            target: "autonoetic::model_routing",
                            original_model = %routed_model,
                            error = %e,
                            "Primary model failed with transient error, trying fallback chain"
                        );
                        last_err = Some(e);
                        let mut final_response = None;
                        for (fb_preset, fb_provider, fb_model) in &fallback_chain {
                            // RFC #779 Part E.2: cross-provider failover is now
                            // allowed. The same-provider restriction has been
                            // removed — if the primary provider is down, the
                            // whole point is to try a different one.
                            //
                            // The drivers use their own `provider.model`, not
                            // `request.model`, so we must build a new driver
                            // for each fallback entry. The preset name is the
                            // key into `llm_presets` in the gateway config.
                            let cross_provider = *fb_provider != routed_llm_cfg.provider;
                            tracing::info!(
                                target: "autonoetic::model_routing",
                                fallback_model = %fb_model,
                                fallback_provider = %fb_provider,
                                fallback_preset = %fb_preset,
                                cross_provider = cross_provider,
                                "Trying fallback model"
                            );

                            // Build a driver for this fallback entry.
                            let fb_driver = match self.config.as_ref() {
                                Some(cfg) => {
                                    let fb_config = cfg.llm_presets.get(fb_preset)
                                        .map(|preset| autonoetic_types::agent::LlmConfig {
                                            provider: preset.provider.clone().unwrap_or_else(|| fb_provider.clone()),
                                            model: preset.model.clone().unwrap_or_else(|| fb_model.clone()),
                                            temperature: preset.temperature.unwrap_or(0.0),
                                            fallback_provider: None,
                                            fallback_model: None,
                                            chat_only: preset.chat_only.unwrap_or(false),
                                            context_window_tokens: preset.context_window_tokens,
                                            base_url: preset.base_url.clone(),
                                            api_key_env: preset.api_key_env.clone(),
                                            routing_preset: Some(fb_preset.clone()),
                                            thinking: preset.thinking.clone(),
                                            egress_class: preset.egress_class,
                                        });
                                    match fb_config {
                                        Some(config) => {
                                            match crate::llm::build_driver(
                                                config,
                                                self.http_client.clone(),
                                            ) {
                                                Ok(driver) => driver,
                                                Err(e) => {
                                                    tracing::warn!(
                                                        target: "autonoetic::model_routing",
                                                        fallback_preset = %fb_preset,
                                                        error = %e,
                                                        "Failed to build fallback driver, skipping"
                                                    );
                                                    continue;
                                                }
                                            }
                                        }
                                        None => {
                                            tracing::warn!(
                                                target: "autonoetic::model_routing",
                                                fallback_preset = %fb_preset,
                                                "Preset not found in llm_presets, skipping"
                                            );
                                            continue;
                                        }
                                    }
                                }
                                None => {
                                    tracing::warn!(
                                        target: "autonoetic::model_routing",
                                        "No gateway config available, cannot build fallback driver"
                                    );
                                    continue;
                                }
                            };

                            if let Err(e) = self
                                .enforce_cost_catalog_preflight(fb_model, allow_unpriced_budget)
                                .await
                            {
                                return Err(self.save_and_yield(
                                    history,
                                    &turn_id,
                                    YieldReason::BudgetExhausted,
                                    None,
                                    e,
                                ));
                            }
                            if let Some(root_budget) = self.root_session_budget.clone() {
                                if let Err(e) = root_budget.reserve_llm_round(root_session_id) {
                                    return Err(self.save_and_yield_root_budget(
                                        history, &turn_id, e,
                                    ));
                                }
                            }
                            match fb_driver.complete(&req).await {
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
                                    let _ = tracer.log_llm_request_failed(fb_model, &e);
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
                        return Err(self.save_and_yield(
                            history,
                            &turn_id,
                            YieldReason::BudgetExhausted,
                            None,
                            e,
                        ));
                    }
                }
            }

            if let Some(root_budget) = self.root_session_budget.clone() {
                if !skip_llm {
                    if let Err(e) = root_budget.record_llm_completion_with_unpriced_override(
                        root_session_id,
                        response.usage.input_tokens,
                        response.usage.output_tokens,
                        estimated_cost_usd,
                        allow_unpriced_budget,
                    ) {
                        return Err(self.save_and_yield_root_budget(history, &turn_id, e));
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
                    reasoning_tokens: response.usage.reasoning_tokens,
                    cached_tokens: response.usage.cached_tokens,
                });
                tracing::info!(
                    target: "autonoetic.llm",
                    agent_id = %self.manifest.agent.id,
                    session_id = %session_id,
                    model = %actual_model,
                    input_tokens = response.usage.input_tokens,
                    output_tokens = response.usage.output_tokens,
                    reasoning_tokens = response.usage.reasoning_tokens,
                    cached_tokens = response.usage.cached_tokens,
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

            // Detect empty LLM responses (Ok but zero output tokens and no text).
            // This catches providers that silently return nothing instead of an error.
            // The narrower `is_retryable_empty_other_response` retry above handles
            // the Other("") case with automatic retry; this logs *any* empty result
            // that survived that retry (or had a different stop reason).
            if response.text.trim().is_empty()
                && response.tool_calls.is_empty()
                && response.usage.output_tokens == 0
            {
                tracing::warn!(
                    target: "autonoetic::llm",
                    model = %actual_model,
                    stop_reason = ?response.stop_reason,
                    input_tokens = response.usage.input_tokens,
                    output_tokens = response.usage.output_tokens,
                    "LLM returned empty response (zero output tokens, no text, no tool calls)"
                );
                let _ = tracer.log_llm_empty_response(
                    &actual_model,
                    &format!("{:?}", response.stop_reason),
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                );
            }

            // Strip inline <think> reasoning blocks (minimax-m3, DeepSeek, Qwen)
            // before the text enters history, reply capture, or tool-call context.
            // Native thinking (Anthropic) arrives via reasoning_content, not inline.
            // Only allocate when the model actually emitted a <think> tag; this is
            // the common-case fast path.
            let clean_text: String = if response.text.contains("<think>") {
                crate::runtime::response_validation::strip_think_blocks(&response.text)
                    .into_owned()
            } else {
                response.text.clone()
            };

            if !clean_text.trim().is_empty() {
                latest_assistant_text = Some(clean_text.clone());
            }

            match response.stop_reason {
                StopReason::ToolUse => {
                    let mut assistant_msg = Message::assistant(clean_text.clone());
                    assistant_msg.reasoning_content = response.reasoning_content.clone();
                    assistant_msg.reasoning_details = response.reasoning_details.clone();
                    assistant_msg.tool_calls = response.tool_calls.clone();
                    // LLM-response label (RFC §4.5): tag + id the assistant
                    // message before `handle_tool_batch` commits it to history.
                    commit_assistant_egress(
                        &mut assistant_msg,
                        &response_egress_label,
                        &mut self.egress_labels,
                    );

                    if let Some(outcome) = self
                        .handle_tool_batch(
                            response.tool_calls,
                            history,
                            &turn_id,
                            &mut tracer,
                            &mut mcp_runtime,
                            &mut disclosure_state,
                            secret_store.as_mut(),
                            &active_agent_dir,
                            assistant_msg,
                            &mut digest_turn_active,
                        )
                        .await?
                    {
                        return Ok(outcome);
                    }
                }
                StopReason::EndTurn | StopReason::StopSequence => {
                    if !clean_text.trim().is_empty() {
                        let mut assistant_msg = Message::assistant(clean_text.clone());
                        assistant_msg.reasoning_content = response.reasoning_content.clone();
                        assistant_msg.reasoning_details = response.reasoning_details.clone();
                        // LLM-response label (RFC §4.5): a local summary of
                        // `local_only` email is itself `local_only`, so a later
                        // remote request withholds it (scenario §5.6 step 4).
                        commit_assistant_egress(
                            &mut assistant_msg,
                            &response_egress_label,
                            &mut self.egress_labels,
                        );

                        history.push(assistant_msg);
                    }
                    tracer.log_hibernate(&format!("{:?}", response.stop_reason));

                    // Inject compact workflow summary if any tasks are tracked
                    let mut turn_yield_reason = YieldReason::Hibernation;
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

                            // Surface workflow state in transcript/user reply only for agents
                            // without strict output contracts. If `io.returns` exists, appending
                            // human-readable status text can invalidate JSON-only outputs.
                            if !has_declared_output_contract {
                                let planner_empty = response.text.trim().is_empty();
                                let note =
                                    workflow_status_user_message_for_chat(&summary, planner_empty);
                                let note = disclosure_state.filter_reply(&note);
                                history.push(Message::assistant(note.clone()));
                            }
                        }

                        if let Some(waiting_reason) = waiting_for_child_yield_reason(
                            cfg,
                            self.gateway_store.as_deref(),
                            &session_id,
                        ) {
                            turn_yield_reason = waiting_reason;
                            // Propagate the suspension decision to the post-loop
                            // outcome builder. The checkpoint below is already
                            // labelled `WaitingForChild`; this flag ensures the
                            // returned `TurnOutcome` matches so the scheduler
                            // keeps the task non-terminal (Ri-0.14).
                            if matches!(
                                turn_yield_reason,
                                YieldReason::WaitingForChild { .. }
                            ) {
                                end_turn_waiting_for_child = true;
                            }
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
                    let _ = self.save_yield_checkpoint(history, &turn_id, turn_yield_reason, None);
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
                        assistant_msg.reasoning_details = response.reasoning_details.clone();
                        // LLM-response label (RFC §4.5), as in the EndTurn arm.
                        commit_assistant_egress(
                            &mut assistant_msg,
                            &response_egress_label,
                            &mut self.egress_labels,
                        );

                        history.push(assistant_msg);
                    }

                    // #846: same premature-complete guard as EndTurn (issue
                    // #845). Truncated (`MaxTokens`) or non-standard
                    // (`Other("content_filter")` / `Other("model_length")` /
                    // …) provider stops can still occur after the agent has
                    // spawned async children. Without this check the loop
                    // `break`s and the post-loop outcome builder returns
                    // `Completed`, which the scheduler marks `Succeeded` —
                    // orphaning the in-flight children and losing their
                    // follow-up work (smoke-test → promote, etc.).
                    //
                    // This arm previously saved no checkpoint at all, so a
                    // crash here also lost the partial turn. Mirror the
                    // EndTurn arm: consult `waiting_for_child_yield_reason`
                    // and, when pending children exist, save a
                    // `WaitingForChild` checkpoint and flag the post-loop
                    // outcome builder to return `TurnOutcome::WaitingForChild`.
                    if let Some(cfg) = self.config.as_ref() {
                        if let Some(waiting_reason) = waiting_for_child_yield_reason(
                            cfg,
                            self.gateway_store.as_deref(),
                            &session_id,
                        ) {
                            let _ = self.save_yield_checkpoint(
                                history,
                                &turn_id,
                                waiting_reason,
                                None,
                            );
                            if let Some(config) = self.config.as_ref() {
                                let _ = prune_checkpoints(config, &session_id, 3);
                            }
                            end_turn_waiting_for_child = true;
                        }
                    }

                    tracer.log_stopped(&format!("{:?}", response.stop_reason));
                    let _ = tracer.end_digest_turn();
                    break;
                }
            }
        }

        // Keep agent reply payload strictly equal to model output (disclosure-filtered).
        // Gateway-generated workflow notes are tracked in history/events, not appended
        // to the returned assistant reply payload.
        let reply = latest_assistant_text.map(|t| disclosure_state.filter_reply(&t));

        self.record_ri09_last_word_response_if_applicable(
            &session_id,
            &turn_id,
            &ri_0_9_notice_message_ids,
            reply.as_deref(),
        );

        // If the EndTurn branch suspended with pending async children, return
        // the WaitingForChild outcome so the scheduler keeps the task non-terminal
        // and the auto-resume machinery can wake the parent when a child
        // transitions. The reply text is intentionally dropped: it was an
        // in-progress narrative ("dispatched → waiting → next-step"), not the
        // task's final output. The parent's final reply is produced on resume
        // once the children it was waiting on have resolved.
        let outcome = if end_turn_waiting_for_child {
            Ok(TurnOutcome::WaitingForChild)
        } else {
            Ok(TurnOutcome::Completed(reply))
        };
        self.last_history = history.clone();
        outcome
    }

    /// Runs before each LLM call. Returns `Ok(None)` if the turn should
    /// proceed. If a gate trips (budget exhausted, emergency stop, max turns,
    /// etc.) the helper saves a yield checkpoint and returns the error so the
    /// caller can propagate it.
    pub async fn pre_turn_checks(
        &mut self,
        history: &mut Vec<Message>,
        turn_id: &str,
    ) -> anyhow::Result<Option<TurnOutcome>> {
        let session_id = self.ensure_session_id();
        let root_session_id = crate::runtime::content_store::root_session_id(&session_id);
            // Loop guard check — save checkpoint before propagating max-turns error.
            // When the guard trips, emit a `loop_guard.tripped` causal event with
            // the structured trip reason (issue #287) so the divergence sentinel
            // and operators can see *why* the session terminated, not just that
            // it did.
            if let Err(e) = self.guard.check_loop() {
                if let (Some(reason), Some(store)) =
                    (self.guard.last_trip_reason(), self.gateway_store.as_ref())
                {
                    // Attribute the trip to its constitutional clause (a
                    // principle *or* a right, via the enforcement register) in
                    // addition to the rule ID, so the detection loop can
                    // correlate breaches by clause, not just by rule string
                    // (#302).
                    let payload = serde_json::json!({
                        "reason": reason.code(),
                        "detail": format!("{:?}", reason),
                        "rule_id": reason.rule_id(),
                        "clause": crate::enforcement_register::clause_of_rule(reason.rule_id()),
                    });
                    let session_id_for_event =
                        self.session_id.clone().unwrap_or_default();
                    let event = autonoetic_types::causal_chain::CausalEventRecord {
                        event_id: format!("loopguard-{}", uuid::Uuid::new_v4()),
                        agent_id: self.manifest.agent.id.clone(),
                        session_id: session_id_for_event,
                        turn_id: Some(turn_id.to_string()),
                        event_seq: 0,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        category: "loop_guard".to_string(),
                        action: "tripped".to_string(),
                        status: "active".to_string(),
                        // Attribute the trip to the rule whose text actually
                        // describes it (P-7.5 failure budget / P-7.7 no
                        // successful result / P-7.19 no semantic progress /
                        // P-7.20 child-failure budget), not a blanket P-7.7.
                        enforced_rules: vec![reason.rule_id().to_string()],
                        target: None,
                        payload: Some(payload.to_string()),
                        payload_ref: None,
                        evidence_ref: None,
                        reason: Some(reason.code().to_string()),
                    };
                    if let Err(err) = store.create_causal_event(&event) {
                        tracing::warn!(
                            target: "loop_guard",
                            error = %err,
                            "failed to emit loop_guard.tripped causal event"
                        );
                    }

                    // Also surface it on the canonical timeline so the room shows
                    // *why* the session was terminated, carrying the rule ID as a
                    // first-class ref (was causal-only — invisible in the room).
                    let sid = self.session_id.clone().unwrap_or_default();
                    let root = crate::runtime::content_store::root_session_id(&sid).to_string();
                    let principal =
                        autonoetic_types::principal::Principal::agent(self.manifest.agent.id.clone());
                    let role = crate::runtime::session_timeline::derive_role(&self.manifest.agent.id);
                    let tl = crate::runtime::session_timeline::build_timeline_event(
                        root,
                        sid,
                        Some(turn_id.to_string()),
                        &principal,
                        &role,
                        "guard.tripped",
                        None, // base_altitude ⇒ Error
                        Some(serde_json::json!({
                            "reason": reason.code(),
                            "rule_id": reason.rule_id(),
                        })),
                        autonoetic_types::session_timeline::TimelineRefs {
                            enforced_rules: vec![reason.rule_id().to_string()],
                            ..Default::default()
                        },
                    );
                    if let Err(err) = store.create_live_digest_event(&tl) {
                        tracing::debug!(target: "session_timeline", error = %err, "guard.tripped timeline emit failed");
                    }
                }
                return Err(self.save_and_yield(history, turn_id, YieldReason::MaxTurnsReached, None, e));
            }

            if self.session_state == autonoetic_types::agent::SessionState::Normal
                && self.guard.is_sub_trip_warning()
            {
                self.session_state = autonoetic_types::agent::SessionState::Degraded;
                if let Some(store) = self.gateway_store.as_ref() {
                    let event = autonoetic_types::causal_chain::CausalEventRecord {
                        event_id: format!("subtrip-{}", uuid::Uuid::new_v4()),
                        agent_id: self.manifest.agent.id.clone(),
                        session_id: session_id.clone(),
                        turn_id: None,
                        event_seq: 0,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        category: "session".to_string(),
                        action: "session.degraded".to_string(),
                        status: "active".to_string(),
                        enforced_rules: vec!["P-7.18".to_string()],
                        target: None,
                        payload: Some(serde_json::json!({"reason": "loop_guard_sub_trip_warning"}).to_string()),
                        payload_ref: None,
                        evidence_ref: None,
                        reason: Some("loop_guard_sub_trip_warning".to_string()),
                    };
                    let _ = store.create_causal_event(&event);

                    // Surface on the canonical timeline so the room shows
                    // *why* the session was degraded — matching the visibility
                    // already given to full guard trips (guard.tripped).
                    let root = crate::runtime::content_store::root_session_id(&session_id).to_string();
                    let principal =
                        autonoetic_types::principal::Principal::agent(self.manifest.agent.id.clone());
                    let role = crate::runtime::session_timeline::derive_role(&self.manifest.agent.id);
                    let tl = crate::runtime::session_timeline::build_timeline_event(
                        root,
                        session_id.clone(),
                        None,
                        &principal,
                        &role,
                        "session.degraded",
                        None,
                        Some(serde_json::json!({
                            "reason": "loop_guard_sub_trip_warning",
                            "rule_id": "P-7.18",
                        })),
                        autonoetic_types::session_timeline::TimelineRefs {
                            enforced_rules: vec!["P-7.18".to_string()],
                            ..Default::default()
                        },
                    );
                    if let Err(err) = store.create_live_digest_event(&tl) {
                        tracing::debug!(target: "session_timeline", error = %err, "session.degraded timeline emit failed");
                    }
                }
                if let Some(ds) = self.degraded_sessions.as_ref() {
                    ds.lock().await.insert(session_id.clone());
                }
            }

            if let Some(ds) = self.degraded_sessions.as_ref() {
                let set = ds.lock().await;
                let in_set = set.contains(&session_id)
                    || set.contains(crate::runtime::content_store::root_session_id(&session_id));
                if in_set && self.session_state == autonoetic_types::agent::SessionState::Normal {
                    self.session_state = autonoetic_types::agent::SessionState::Degraded;
                } else if !in_set && self.session_state == autonoetic_types::agent::SessionState::Degraded {
                    self.session_state = autonoetic_types::agent::SessionState::Normal;
                }
            }

            if !self.ri_0_6_snapshot_checked {
                if let Err(e) = self.check_ri_0_6_turn_snapshot(&session_id, turn_id) {
                    return Err(self.save_and_yield(
                        history,
                        turn_id,
                        YieldReason::Error(e.to_string()),
                        None,
                        e,
                    ));
                }
                self.ri_0_6_snapshot_checked = true;
            }

            // Budget check — save checkpoint before propagating budget-exhausted error
            if let Some(budget) = self.session_budget.as_ref() {
                if let Err(e) = budget.check_pre_llm(&session_id) {
                    return Err(self.save_and_yield(
                        history,
                        turn_id,
                        YieldReason::BudgetExhausted,
                        None,
                        e,
                    ));
                }
            }

            // Root session tree budget check (R+4 / P-6.21)
            if let Some(root_budget) = self.root_session_budget.clone() {
                if let Err(e) = root_budget.check_pre_llm(root_session_id) {
                    return Err(self.save_and_yield_root_budget(history, turn_id, e));
                }
            }

            // Emergency-stop pre-flight: if the root session has been
            // emergency-stopped (by operator, security policy, or budget
            // circuit breaker), terminate this loop immediately instead of
            // spending another LLM turn. The external abort (AbortHandle) may
            // not have reached this task yet, so this cooperative check closes
            // the race window.
            if let Some(store) = self.gateway_store.as_ref() {
                if let Ok(stops) = store.list_emergency_stops_for_root_session(root_session_id) {
                    if !stops.is_empty() {
                        return Err(self.save_and_yield(
                            history,
                            turn_id,
                            YieldReason::EmergencyStop {
                                stop_id: stops[0].stop_id.clone(),
                            },
                            None,
                            anyhow::anyhow!(
                                "emergency_stop: root session '{}' was emergency-stopped",
                                root_session_id
                            ),
                        ));
                    }
                }
            }

        Ok(None)
    }

    /// Processes a batch of tool calls from the LLM. Returns `Some(TurnOutcome)`
    /// if the turn should suspend (approval/user-input/escalation), or `None`
    /// Truncate a tool result once, at push time, using JSON-aware
    /// truncation. This avoids re-parsing every tool result as JSON on every
    /// subsequent turn via `sanitize_history_for_request`.
    fn truncate_result(&self, result: &str) -> String {
        let max_chars = self
            .config
            .as_ref()
            .map(|c| c.prompt_budget.max_tool_result_chars)
            .unwrap_or(4000);
        if max_chars > 0 && result.chars().count() > max_chars {
            truncate_tool_result_once(result, max_chars)
        } else {
            result.to_string()
        }
    }

    /// if the batch completed and the loop should continue.
    pub async fn handle_tool_batch(
        &mut self,
        tool_calls: Vec<ToolCall>,
        history: &mut Vec<Message>,
        turn_id: &str,
        tracer: &mut SessionTracer,
        mcp_runtime: &mut McpToolRuntime,
        disclosure_state: &mut DisclosureState,
        secret_store: Option<&mut SecretStoreRuntime>,
        active_agent_dir: &std::path::Path,
        assistant_msg: Message,
        digest_turn_active: &mut bool,
    ) -> anyhow::Result<Option<TurnOutcome>> {
        let session_id = self.ensure_session_id();
            if let Some(budget) = self.session_budget.as_ref() {
                if let Err(e) = budget
                    .reserve_tool_invocations(&session_id, tool_calls.len() as u64)
                {
                    return Err(self.save_and_yield(
                        history,
                        turn_id,
                        YieldReason::BudgetExhausted,
                        None,
                        e,
                    ));
                }
            }

            if let Some(root_budget) = self.root_session_budget.clone() {
                let root =
                    crate::runtime::content_store::root_session_id(&session_id).to_string();
                if let Err(e) = root_budget
                    .reserve_tool_invocations(&root, tool_calls.len() as u64)
                {
                    return Err(self.save_and_yield_root_budget(history, turn_id, e));
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
                    sentinel_suppress_target: Some(self.suppress_until_turn.clone()),
                    discovered_tools: Some(self.discovered_tools_writer.clone()),
                    tool_discovery_catalog: Some(std::sync::Arc::new(
                        crate::runtime::active_execution_registry::NativeToolDiscoveryCatalog {
                            registered: self.registry.registered_tool_names(),
                            available: self.registry.available_tool_names(&self.manifest),
                        },
                    )),
                    wake_hint: None,
                    wake_hints_map: None,
                    // The sender's accumulated taint (RFC §5.5) — `agent_message`
                    // stamps its payload with this so a remote-pinned recipient
                    // withholds tainted content (closes the `LocalAgent` hole).
                    egress_taint: {
                        let t = crate::runtime::egress_labeler::session_accumulated_taint(
                            &self.egress_labels,
                        );
                        (!t.is_unrestricted()).then_some(t)
                    },
                    // Stored-content recall sink (RFC §6): when the session is
                    // already local-tainted, tools may return local_only content;
                    // otherwise fail closed to RemoteModel (None → remote).
                    egress_query_sink: {
                        let t = crate::runtime::egress_labeler::session_accumulated_taint(
                            &self.egress_labels,
                        );
                        if t.allows(autonoetic_types::egress::Sink::RemoteModel) {
                            None
                        } else {
                            Some(autonoetic_types::egress::Sink::LocalModel)
                        }
                    },
                }
            });
            let mut processor = ToolCallProcessor::new(
                mcp_runtime,
                &self.registry,
                &self.manifest,
                disclosure_state,
                secret_store,
                self.config.as_deref(),
                self.gateway_store.clone(),
                tool_run_ctx,
            )
            .with_session_context(self.session_id.clone(), Some(turn_id.to_string()))
            .with_session_state(self.session_state);

            let (_had_any_success, results) = processor
                .process_tool_calls(
                    &tool_calls,
                    &active_agent_dir,
                    self.gateway_dir.as_deref(),
                    tracer,
                )
                .await?;
            // Merge this turn's egress labels into the session-wide map. Labels
            // are monotonic (RFC §2.4) — a tool result labeled this turn stays
            // labeled for every future completion in the session. The map is
            // attached to each completion request's metadata so the
            // EgressChokepointDriver (RFC §5.2) can withhold content whose
            // label excludes the target sink.
            //
            // Also capture this turn's *batch taint* (RFC §5.3): the
            // intersection of the labels this turn just added. It steers
            // taint-following routing for the NEXT completion — a tainted batch
            // makes only eligible presets candidates. An empty delta leaves the
            // batch `unrestricted` (the fast no-op path), which correctly
            // resets the batch after a clean turn.
            let mut batch_taint = autonoetic_types::egress::EgressLabel::unrestricted();
            for (k, v) in processor.take_egress_labels() {
                batch_taint = batch_taint.restrict(&v);
                self.egress_labels.insert(k, v);
            }
            self.pending_batch_taint = batch_taint;

            // Hard-trip the LoopGuard if any tool call returned a deterministic
            // terminal-workflow error. Retrying agent_spawn against a terminal
            // workflow can never succeed, so stop the turn immediately rather
            // than letting the agent burn its tool-failure budget.
            for (_id, tool_name, result_json) in &results {
                if let Some(workflow_id) = Self::detect_terminal_workflow_error(tool_name, result_json) {
                    self.guard.trip(
                        crate::runtime::guard::LoopGuardTripReason::WorkflowTerminal {
                            workflow_id: workflow_id.clone(),
                        },
                    );
                    tracing::info!(
                        target: "loop_guard",
                        session_id = %session_id,
                        workflow_id = %workflow_id,
                        tool = %tool_name,
                        "LoopGuard hard-trip: tool returned terminal-workflow error"
                    );
                    break;
                }
            }

            // Progressive tool disclosure: if the agent used any Specialized-tier
            // tool, escalate the session so subsequent turns see all tiers.
            if !self.tool_tier_escalated {
                for (_id, tool_name, _result) in &results {
                    if matches!(
                        crate::runtime::prompt_budget::tool_tier(tool_name),
                        autonoetic_types::agent::ToolTier::Specialized,
                    ) {
                        self.tool_tier_escalated = true;
                        tracing::info!(
                            target: "autonoetic::tool_disclosure",
                            tool = %tool_name,
                            "Session escalated to all tool tiers"
                        );
                        break;
                    }
                }
            }

            // Drain discovered tools from the writer (written by tool_discover).
            {
                let mut writer = self.discovered_tools_writer.lock().unwrap_or_else(|e| e.into_inner());
                if !writer.is_empty() {
                    let count = writer.len();
                    self.discovered_tools.extend(writer.drain());
                    tracing::info!(
                        target: "autonoetic::tool_discover",
                        count,
                        total = self.discovered_tools.len(),
                        "Discovered tools merged into session surface"
                    );
                }
            }

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
                let completed_results = results[..results.len() - 1].to_vec();
                let remaining_calls = tool_calls[results.len()..].to_vec();

                let pending_tc = tool_calls
                    .iter()
                    .find(|tc| tc.id == pending_call_id)
                    .expect("pending call id must match a tool call in the response");

                let pending_action = match self.gateway_store.as_ref() {
                    Some(store) => {
                        let approval = store.get_approval(&request_id).map_err(|e| {
                            anyhow::anyhow!(
                                "failed to fetch approval {} while saving checkpoint: {}",
                                request_id,
                                e
                            )
                        })?;
                        let approval = approval.ok_or_else(|| {
                            anyhow::anyhow!(
                                "missing approval {} while saving checkpoint",
                                request_id
                            )
                        })?;
                        Some(approval.action)
                    }
                    None => None,
                };

                let pending_tool_state = PendingToolState {
                    completed_tool_results: completed_results,
                    pending_tool_call: PendingToolCall {
                        call_id: pending_call_id,
                        tool_name: pending_tc.name.clone(),
                        arguments: pending_tc.arguments.clone(),
                        approval_response: Some(approval_response),
                    },
                    remaining_tool_calls: remaining_calls,
                };

                // Build enriched checkpoint with all suspension state.
                let mut cp = self.build_checkpoint(
                    history,
                    turn_id,
                    YieldReason::ApprovalRequired {
                        approval_request_id: request_id.clone(),
                    },
                    Some(pending_tool_state),
                );
                cp.assistant_message = Some(Box::new(assistant_msg));
                cp.pending_action = pending_action;
                cp.suspended_at = Some(chrono::Utc::now().to_rfc3339());

                if let Some(config) = self.config.as_ref() {
                    if let Err(e) = save_checkpoint(config, &cp) {
                        tracing::warn!(
                            target: "checkpoint",
                            session_id = %session_id,
                            turn_id = %turn_id,
                            approval_request_id = %request_id,
                            error = %e,
                            "Failed to save enriched approval checkpoint"
                        );
                    }
                }

                let _ = tracer.end_digest_turn();
                return Ok(Some(TurnOutcome::Suspended {
                    approval_request_id: request_id,
                }));
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
                let remaining_calls = tool_calls[results.len()..].to_vec();

                let pending_tc = tool_calls
                    .iter()
                    .find(|tc| tc.id == pending_call_id)
                    .expect("pending user interaction call id must match a tool call");

                history.push(assistant_msg);
                for (id, name, result) in &completed_results {
                    history.push(Message::tool_result(
                        id.clone(),
                        name.clone(),
                        self.truncate_result(result),
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

                let _ = self.save_yield_checkpoint(
                    history,
                    turn_id,
                    YieldReason::UserInputRequired {
                        interaction_id: interaction_id.clone(),
                    },
                    pending_tool_state,
                );

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
                return Ok(Some(TurnOutcome::SuspendedUserInput {
                    interaction_id: interaction_id.clone(),
                }));
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
                if let Some((_, tool_name, result_json)) = results.last() {
                    self.guard.register_irrecoverable(tool_name, result_json);
                }

                let _ = self.save_yield_checkpoint(
                    history,
                    turn_id,
                    YieldReason::HumanEscalation {
                        escalation_request_id: request_id.clone(),
                    },
                    None,
                );

                tracing::info!(
                    target: "escalation",
                    agent_id = %self.manifest.agent.id,
                    session_id = %session_id,
                    escalation_request_id = %request_id,
                    "Turn suspended for human escalation"
                );

                let _ = tracer.end_digest_turn();
                return Ok(Some(TurnOutcome::Escalated {
                    escalation_request_id: request_id,
                }));
            }

            // Scan the WHOLE batch for a `waiting_for_child` result, not just
            // the last one: a `workflow_wait` earlier in a parallel tool batch
            // must suspend the turn too. When several results carry the flag,
            // the last one wins; every other executed result is committed as
            // completed so it is not re-run on resume.
            let waiting_for_child_info = results
                .iter()
                .enumerate()
                .rev()
                .find_map(|(idx, (id, _name, result_json))| {
                    let parsed = serde_json::from_str::<serde_json::Value>(result_json).ok()?;
                    if parsed.get("waiting_for_child").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let workflow_id = parsed
                            .get("workflow_id")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                            .or_else(|| self.workflow_id.clone())
                            .unwrap_or_default();
                        Some((idx, id.clone(), workflow_id))
                    } else {
                        None
                    }
                });

            if let Some((pending_idx, pending_call_id, workflow_id)) = waiting_for_child_info {
                let completed_results: Vec<_> = results
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != pending_idx)
                    .map(|(_, r)| r.clone())
                    .collect();
                let remaining_calls = tool_calls[results.len()..].to_vec();

                let pending_tc = tool_calls
                    .iter()
                    .find(|tc| tc.id == pending_call_id)
                    .expect("waiting_for_child call id must match a tool call in the response");

                let pending_tool_state = PendingToolState {
                    completed_tool_results: completed_results,
                    pending_tool_call: PendingToolCall {
                        call_id: pending_call_id,
                        tool_name: pending_tc.name.clone(),
                        arguments: pending_tc.arguments.clone(),
                        approval_response: None,
                    },
                    remaining_tool_calls: remaining_calls,
                };

                let yield_reason = YieldReason::WaitingForChild {
                    workflow_id,
                    task_id: self.task_id.clone(),
                };

                let mut cp = self.build_checkpoint(
                    history,
                    turn_id,
                    yield_reason,
                    Some(pending_tool_state),
                );
                cp.assistant_message = Some(Box::new(assistant_msg));

                if let Some(config) = self.config.as_ref() {
                    if let Err(e) = save_checkpoint(config, &cp) {
                        tracing::warn!(
                            target: "checkpoint",
                            session_id = %session_id,
                            turn_id = %turn_id,
                            error = %e,
                            "Failed to save enriched waiting_for_child checkpoint"
                        );
                    }
                }
                if let Some(gs) = self.gateway_store.as_ref() {
                    let lifecycle = "hibernated";
                    if let Err(e) = gs.set_session_lifecycle_state(&cp.session_id, lifecycle) {
                        tracing::warn!(
                            target: "lifecycle",
                            session_id = %cp.session_id,
                            lifecycle_state = %lifecycle,
                            error = %e,
                            "Failed to persist lifecycle state on yield"
                        );
                    }
                }

                let _ = tracer.end_digest_turn();
                return Ok(Some(TurnOutcome::WaitingForChild));
            }

            // No approval or interaction required — commit assistant message + tool results to history.
            history.push(assistant_msg);
            let mut tool_feedback_events: Vec<FeedbackEvent> = Vec::new();
            for (id, _name, result) in &results {
                history.push(Message::tool_result(
                    id.clone(),
                    _name.clone(),
                    self.truncate_result(result),
                ));
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result) {
                    // Recurring-error detector (#703): feed every result — the
                    // method no-ops on non-errors and fingerprints both `ok:false`
                    // errors and `any_failed` child failures (via failure_summary),
                    // so one unrecoverable cause surfacing through different tools
                    // trips the guard even when no single tool's budget is hit.
                    self.guard.register_error(_name, result);
                    if parsed.get("ok") == Some(&serde_json::Value::Bool(false)) {
                        let error_type = parsed.get("error_type")
                            .and_then(|v| v.as_str())
                            .and_then(|s| match s {
                                "validation" => Some(ToolErrorType::Validation),
                                "permission" => Some(ToolErrorType::Permission),
                                "resource" => Some(ToolErrorType::Resource),
                                "execution" => Some(ToolErrorType::Execution),
                                "fatal" => Some(ToolErrorType::Fatal),
                                "conflict" => Some(ToolErrorType::Conflict),
                                "quota_exceeded" => Some(ToolErrorType::QuotaExceeded),
                                "not_found" => Some(ToolErrorType::NotFound),
                                "timeout" => Some(ToolErrorType::Timeout),
                                "sandbox_unavailable" => Some(ToolErrorType::SandboxUnavailable),
                                _ => None,
                            });
                        if let Some(tc) = tool_calls.iter().find(|tc| tc.id == *id) {
                            if let Some(et) = error_type.clone() {
                                let message_signature = normalize_error_signature(
                                    parsed.get("message").and_then(|v| v.as_str()).unwrap_or(""),
                                );
                                tool_feedback_events.push(FeedbackEvent::ToolError {
                                    tool: tc.name.clone(),
                                    error_type: et,
                                    message_signature,
                                });
                            }
                        }
                        let signal_derived = is_signal_derived_exit(&parsed);
                        let irrecoverable = error_type
                            .as_ref()
                            .map(crate::runtime::guard::LoopGuard::is_irrecoverable)
                            .unwrap_or(false)
                            || signal_derived;
                        if let Some(tc) = tool_calls.iter().find(|tc| tc.id == *id)
                        {
                            if irrecoverable {
                                // #718: irrecoverable rejections are excluded
                                // from the per-tool failure budget (retrying
                                // can't fix them), but re-issuing the *same*
                                // call for the *same* deterministic rejection
                                // is a no-progress loop (P-7.7). Count it; the
                                // guard trips once the same (tool, error)
                                // rejection recurs past its threshold.
                                self.guard.register_irrecoverable(&tc.name, result);
                                if !self.blocked_state_event_emitted {
                                    let payload = serde_json::json!({
                                        "tool": tc.name,
                                        "error_type": error_type.as_ref().map(|e| e.to_string()),
                                        "exit_code": parsed.get("exit_code").and_then(|v| v.as_i64()),
                                        "signal_derived": signal_derived,
                                        "message": "The agent is blocked by a gateway-side irrecoverable condition, not diverging.",
                                    });
                                    if let Err(e) = tracer.log_event(
                                        "operator_alert",
                                        "blocked_state",
                                        autonoetic_types::causal_chain::EntryStatus::Success,
                                        Some(payload),
                                    ) {
                                        tracing::warn!(
                                            target: "autonoetic::trajectory",
                                            error = %e,
                                            "Failed to log blocked_state operator alert"
                                        );
                                    }
                                    self.blocked_state_event_emitted = true;
                                }
                            } else {
                                self.guard.register_failure(
                                    &tc.name,
                                    &tc.arguments,
                                    error_type.as_ref(),
                                );
                            }
                        }
                    } else if tool_result_counts_as_progress(result) {
                        if let Some(tc) = tool_calls.iter().find(|tc| tc.id == *id)
                        {
                            // Suppress progress reset for stagnant
                            // no-op polls (e.g. workflow_wait that
                            // returned "still running" after 0s). These
                            // carry no new information and should
                            // advance the no-progress counter instead
                            // of resetting it (issue: polling churn).
                            if crate::runtime::tool_dispatch::is_stagnant_poll(
                                &tc.name,
                                result,
                            ) {
                                continue;
                            }
                            // Tools may opt into terminal-progress
                            // semantics by stamping
                            // `side_effect_state: "committed"` in
                            // their result (P-5.14 / P-6.26).
                            // Terminal events clear the
                            // rotating-polling window — a real
                            // side effect just landed, so any prior
                            // monotony is stale (issue #287).
                            let terminal = parsed
                                .get("side_effect_state")
                                .and_then(|v| v.as_str())
                                == Some("committed");
                            // Reading artifact/content file bytes is
                            // substantive progress for review agents
                            // (static_evaluator, auditor, etc.). Keep
                            // metadata/files resolves as read-only
                            // probes so a planner cannot reset the
                            // guard by re-listing artifacts.
                            let is_resolve_content_read =
                                crate::runtime::tool_dispatch::is_resolve_content_read(
                                    &tc.name,
                                    &tc.arguments,
                                );
                            if crate::runtime::tool_dispatch::is_read_only_tool(&tc.name)
                                && !is_resolve_content_read
                            {
                                // Read-only probes advance no workflow — track
                                // for rotating-polling detection but do not
                                // reset the no-progress counter (#701).
                                self.guard
                                    .register_readonly_progress(&tc.name, &tc.arguments);
                            } else if terminal {
                                self.guard
                                    .register_progress_terminal(&tc.name, &tc.arguments);
                            } else {
                                self.guard
                                    .register_progress(&tc.name, &tc.arguments);
                            }

                            // RFC #776 Part B.4: track spawn structural identity
                            // to catch delegation loops (parent re-spawning the
                            // same child with the same contract + input).
                            if tc.name == "agent_spawn" {
                                if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                                    let spawn_agent_id = args.get("agent_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let message_str = args.get("message")
                                        .map(|v| v.to_string())
                                        .unwrap_or_default();
                                    let expected: Vec<String> = args
                                        .pointer("/metadata/expected_outputs")
                                        .and_then(|v| v.as_array())
                                        .map(|arr| arr.iter()
                                            .filter_map(|v| v.as_str().map(str::to_string))
                                            .collect())
                                        .unwrap_or_default();
                                    if !spawn_agent_id.is_empty() {
                                        if let Some(reason) = self.guard
                                            .register_spawn_attempt(
                                                spawn_agent_id,
                                                &expected,
                                                &message_str,
                                            )
                                        {
                                            tracing::warn!(
                                                target: "autonoetic::guard",
                                                reason = ?reason,
                                                "Spawn identity loop guard tripped"
                                            );
                                            self.guard.trip(reason);
                                        }
                                    }
                                }
                            }
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
            'trajectory_monitor: {
                use crate::runtime::trajectory_monitor::fingerprint_tool_call;
                use crate::runtime::trajectory_health::{
                    build_event_payload, TrajectoryHealth, DIVERGENCE_CATEGORY,
                };
                use autonoetic_types::causal_chain::EntryStatus;

                // Clarification is a single read-only Q&A turn (ask-agent
                // spawns). It must not be subject to divergence escalation: a
                // clarification child that loops on read-only inspection tools
                // would otherwise be classified Critical and spawn another
                // clarification, forming a clarify→diverge→clarify chain. The
                // LoopGuard's hard limits (max_session_turns) still bound it.
                // (RFC: unit-test-runner-divergence-loop, Change 3 / Option A)
                if self.session_state
                    == autonoetic_types::agent::SessionState::Clarification
                {
                    break 'trajectory_monitor;
                }

                let observations: Vec<ToolObservation> = results
                    .iter()
                    .filter_map(|(id, _name, result)| {
                        let tc = tool_calls.iter().find(|tc| tc.id == *id)?;
                        let fp = fingerprint_tool_call(&tc.name, &tc.arguments);
                        let parsed = serde_json::from_str::<serde_json::Value>(result).ok();
                        let failed = parsed.as_ref().map_or(false, |v| {
                            // A tool failure is signalled by `ok: false`. We do
                            // NOT treat a non-zero `exit_code` as a failure when
                            // the tool reports `ok: true`: for sandbox/exec tools
                            // a non-zero exit code is a DOMAIN result (e.g. a unit
                            // test suite that failed), not a tool malfunction, and
                            // must not drive divergence. Tools that genuinely
                            // failed set `ok: false`. (RFC: unit-test-runner-
                            // divergence-loop)
                            v.get("ok").and_then(|o| o.as_bool()) == Some(false)
                        });
                        Some(ToolObservation {
                            fingerprint: fp,
                            failed,
                        })
                    })
                    .collect();

                if !tool_feedback_events.is_empty() {
                    self.trajectory_monitor
                        .record_feedback(self.turn_counter, &tool_feedback_events);
                }

                let result = self.trajectory_monitor.tick(
                    self.turn_counter,
                    &observations,
                    &tool_feedback_events,
                    self.last_context_utilization,
                    &self.guard.snapshot(),
                );

                // RFC D.5 — extend Sentinel suppression when the agent is
                // incorporating feedback. The tick result requests a new target
                // turn; apply it if it extends the current suppression window.
                if let Some(requested) = result.suppress_until_turn {
                    use std::sync::atomic::Ordering;
                    let current = self.suppress_until_turn.load(Ordering::Relaxed);
                    if requested > current {
                        self.suppress_until_turn.store(requested, Ordering::Relaxed);
                    }
                }

                // RFC D.5 — when suppression is active, skip all Sentinel
                // escalation surfaces (causal event, planner message, operator
                // notification) for this turn.
                use std::sync::atomic::Ordering;
                let suppressed =
                    self.turn_counter < self.suppress_until_turn.load(Ordering::Relaxed);

                if result.level_changed && !suppressed {
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

                    // ── P2: Planner messaging & operator escalation ──────
                    let cfg = self.config.as_ref();

                    match &result.health {
                        TrajectoryHealth::Diverging { .. }
                        | TrajectoryHealth::Critical { .. }
                        | TrajectoryHealth::Blocked { .. } => {
                            if let Some(store) = self.gateway_store.as_ref() {
                                let root_sid = crate::runtime::content_store::root_session_id(&session_id).to_string();
                                Self::send_divergence_notice(
                                    store,
                                    &root_sid,
                                    self.turn_counter,
                                    &self.manifest.agent.id,
                                    result.health.level_str(),
                                    &self.suppress_until_turn,
                                    cfg.map(|c| c.trajectory.notify_planner).unwrap_or(true),
                                );

                                // The Sentinel is a participant in the room, not
                                // chrome: its intervention lands on the canonical
                                // timeline under the Sentinel seat (#363 P1, RFC §3.2).
                                let is_critical =
                                    matches!(result.health, TrajectoryHealth::Critical { .. });
                                let principal =
                                    autonoetic_types::principal::Principal::agent("sentinel");
                                let event = crate::runtime::session_timeline::build_timeline_event(
                                    root_sid.clone(),
                                    session_id.to_string(),
                                    Some(turn_id.to_string()),
                                    &principal,
                                    &autonoetic_types::session_timeline::SessionRole::Sentinel,
                                    "divergence.intervention",
                                    Some(if is_critical {
                                        autonoetic_types::session_timeline::Altitude::Error
                                    } else {
                                        autonoetic_types::session_timeline::Altitude::Attention
                                    }),
                                    Some(serde_json::json!({
                                        "monitored_agent": self.manifest.agent.id,
                                        "level": result.health.level_str(),
                                        "turn": self.turn_counter,
                                    })),
                                    autonoetic_types::session_timeline::TimelineRefs::default(),
                                );
                                if let Err(e) = store.create_live_digest_event(&event) {
                                    tracing::debug!(target: "session_timeline", error = %e, "divergence timeline emit failed");
                                }
                            }

                            // Critical surfaces as a passive operator-activity
                            // advisory (Phase 2 D.7a). The Sentinel no longer pushes
                            // an answer-demanding UserInteraction; the operator may
                            // stop the session explicitly via the TUI instead.
                            if matches!(result.health, TrajectoryHealth::Critical { .. }) {
                                let notify_operator = cfg
                                    .map(|c| c.trajectory.notify_operator)
                                    .unwrap_or(true);
                                if notify_operator {
                                    if let Err(e) = tracer.log_event(
                                        "operator_alert",
                                        "critical_divergence",
                                        EntryStatus::Success,
                                        Some(serde_json::json!({
                                            "level": "critical",
                                            "turn": self.turn_counter,
                                            "agent_id": self.manifest.agent.id,
                                            "message": "Trajectory divergence has reached critical level. Review divergence.* events in the causal chain.",
                                        })),
                                    ) {
                                        tracing::warn!(target: "autonoetic::trajectory", error = %e, "Failed to log operator_alert event");
                                    }

                                    if let Some(store) = self.gateway_store.as_ref() {
                                        let root_sid = crate::runtime::content_store::root_session_id(&session_id).to_string();
                                        self.emit_critical_sentinel_operator_activity(
                                            store,
                                            &session_id,
                                            root_sid,
                                            turn_id,
                                            &result.health,
                                        );
                                    }
                                }
                            }
                        }
                        _ => {}
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
                    tracer,
                    &disclosure_state,
                    self.gateway_store.as_deref(),
                    Some(&self.manifest.agent.id),
                    self.session_started_at.as_deref(),
                ) {
                    tracing::warn!("Failed to persist history after tool batch: {}", e);
                }
            }

            let _ = tracer.end_digest_turn();
            *digest_turn_active = false;
            return Ok(None);
    }

    /// Send a divergence notice to the root planner if not suppressed.
    /// Returns true if a message was sent, false if suppressed or
    /// notify_planner is false. Errors during persistence are logged
    /// but not returned (best-effort delivery).
    pub fn send_divergence_notice(
        store: &crate::scheduler::gateway_store::GatewayStore,
        root_session_id: &str,
        turn_counter: u64,
        agent_id: &str,
        level: &str,
        suppress_until: &AtomicU64,
        notify_planner: bool,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if turn_counter < suppress_until.load(Ordering::Relaxed) {
            return false;
        }
        if !notify_planner {
            return false;
        }

        let now = chrono::Utc::now().to_rfc3339();
        let msg_id = autonoetic_types::id_format::short_random_id("msg-");
        let message = format!(
            "[Sentinel Notice]\n\
             Level: {}\n\
             Turn: {}\n\
             Agent: {}\n\
             The trajectory monitor has detected a divergence pattern. \
             Review the causal chain for divergence.* events.",
            level, turn_counter, agent_id,
        );
        let record = crate::scheduler::gateway_store::AgentMessageRecord {
            message_id: msg_id.clone(),
            sender_session_id: "gateway:sentinel".to_string(),
            sender_agent_id: "gateway".to_string(),
            target_pattern: format!("session:{}", root_session_id),
            message,
            created_at: now.clone(),
            // Gateway-authored Sentinel notice — content-free, unrestricted.
            egress_label: None,
        };
        if let Err(e) = store.save_agent_message(&record) {
            tracing::warn!(target: "autonoetic::trajectory", error = %e, "Failed to save divergence planner message");
            return false;
        }
        if let Err(e) = store.insert_message_delivery(&msg_id, root_session_id) {
            tracing::warn!(target: "autonoetic::trajectory", error = %e, "Failed to insert divergence message delivery");
            return false;
        }
        let signal = crate::scheduler::signal::Signal::AgentMessage {
            message_id: msg_id.clone(),
            sender_session_id: "gateway:sentinel".to_string(),
            sender_agent_id: "gateway".to_string(),
            message: record.message,
            timestamp: now,
        };
        if let Err(e) = crate::scheduler::signal::write_signal(
            Some(store), root_session_id, &msg_id, &signal,
        ) {
            tracing::warn!(target: "autonoetic::trajectory", error = %e, "Failed to write divergence wake signal");
        }
        true
    }

    /// Emit a passive operator-activity advisory for a Critical Sentinel verdict.
    /// Phase 2 D.7a replacement for the pushed DivergenceSentinel UserInteraction.
    pub fn emit_critical_sentinel_operator_activity(
        &self,
        store: &crate::scheduler::gateway_store::GatewayStore,
        session_id: &str,
        root_session_id: String,
        turn_id: &str,
        health: &crate::runtime::trajectory_health::TrajectoryHealth,
    ) {
        let crate::runtime::trajectory_health::TrajectoryHealth::Critical { signals } = health else {
            return;
        };
        let draft = crate::runtime::operator_activity::classify_sentinel_notice(
            health.level_str(),
            &self.manifest.agent.id,
            self.turn_counter,
            signals,
        );
        let record = draft.into_record(
            root_session_id,
            session_id.to_string(),
            self.manifest.agent.id.clone(),
            self.workflow_id.clone(),
            self.task_id.clone(),
            Some(turn_id.to_string()),
            None,
            None,
            None,
        );
        let rate_limit_per_min = self
            .config
            .as_ref()
            .map(|c| c.operator_activity.rate_limit_per_min)
            .unwrap_or_else(|| autonoetic_types::config::OperatorActivityConfig::default().rate_limit_per_min);
        if let Err(e) = store.insert_operator_activity_throttled(&record, rate_limit_per_min) {
            tracing::warn!(target = "autonoetic::trajectory", error = %e, "Failed to insert sentinel_notice operator_activity");
        }
    }
}

/// Decide whether a session ending its turn must suspend as `WaitingForChild`
/// instead of completing.
///
/// The predicate is **parent-scoped and status-authoritative**: it returns
/// `Some` only when the session itself spawned workflow tasks that are still
/// non-terminal (`session_has_non_terminal_children`). It deliberately does
/// NOT:
/// - consult `WorkflowRun.active_task_ids`/`queued_task_ids` (denormalized
///   lists that drift — `save_task_run` appends on every save, `dequeue_task`
///   is the only remover), or
/// - park on *siblings*/cousins in the same workflow. A leaf task that
///   finishes while a sibling is still running completes normally; parking it
///   deadlocked the workflow (session-d484ea13: static_evaluator parked on the
///   auditor sibling, and the parent-only wake could never match it).
///
/// The wake side (`wake_paused_child_wait_tasks`) and the scheduler janitor
/// (`reconcile_paused_child_wait_tasks`) evaluate the SAME predicate, so a
/// task is only ever parked when a future child terminal transition — or the
/// janitor — can wake it.
fn waiting_for_child_yield_reason(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
) -> Option<YieldReason> {
    let root_session_id = crate::runtime::content_store::root_session_id(session_id);
    let workflow_id = crate::scheduler::resolve_workflow_id_for_root_session(
        config,
        &root_session_id,
    )
    .ok()??;

    let has_children = crate::scheduler::workflow_store::session_has_non_terminal_children(
        config,
        store,
        &workflow_id,
        session_id,
    )
    .ok()?;

    if !has_children {
        return None;
    }

    Some(YieldReason::WaitingForChild {
        workflow_id,
        task_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::SessionState;

    // -- overflow_presend_block ------------------------------------------------

    #[test]
    fn detect_terminal_workflow_error_detects_agent_spawn_rejection() {
        let result = r#"{"ok":false,"error_type":"execution","message":"Cannot delegate (agent.spawn): workflow wf-123 is already terminal (failed). No new tasks can be spawned."}"#;
        assert_eq!(
            AgentExecutor::detect_terminal_workflow_error("agent_spawn", result),
            Some("wf-123".to_string())
        );
        assert_eq!(
            AgentExecutor::detect_terminal_workflow_error("agent.spawn", result),
            Some("wf-123".to_string())
        );
        assert_eq!(
            AgentExecutor::detect_terminal_workflow_error("agent_spawn", r#"{"ok":true}"#),
            None
        );
        assert_eq!(
            AgentExecutor::detect_terminal_workflow_error("content_read", result),
            None
        );
    }

    // -- detect_constitution_drift (#821) ---------------------------------------

    #[test]
    fn constitution_drift_no_prior_pin_is_not_a_drift() {
        // Fresh session (or one that predates #821): nothing pinned yet, so
        // there is nothing to compare against — must not fabricate a notice.
        assert!(detect_constitution_drift(None, None, "2026.06.05", "digest-a").is_none());
    }

    #[test]
    fn constitution_drift_matching_pin_is_not_a_drift() {
        assert!(detect_constitution_drift(
            Some("2026.06.05"),
            Some("digest-a"),
            "2026.06.05",
            "digest-a",
        )
        .is_none());
    }

    #[test]
    fn constitution_drift_changed_digest_produces_notice_and_payload() {
        let notice = detect_constitution_drift(
            Some("2026.06.05"),
            Some("digest-old-0123456789"),
            "2026.07.01",
            "digest-new-9876543210",
        )
        .expect("changed digest must be detected as drift");

        assert_eq!(notice.payload["pinned_version"], "2026.06.05");
        assert_eq!(notice.payload["pinned_digest"], "digest-old-0123456789");
        assert_eq!(notice.payload["current_version"], "2026.07.01");
        assert_eq!(notice.payload["current_digest"], "digest-new-9876543210");
        assert_eq!(notice.payload["enforced_rules"], serde_json::json!(["Ri-0.5"]));

        // Never blocking: the notice is text for the agent, not an error —
        // and it must state old version, new version, and be legible.
        assert!(notice.notice_text.contains("Ri-0.5"));
        assert!(notice.notice_text.contains("2026.06.05"));
        assert!(notice.notice_text.contains("2026.07.01"));
        assert!(notice.notice_text.contains("law changed"));
    }

    #[test]
    fn constitution_drift_same_version_different_digest_still_drifts() {
        // Digest is the source of truth (a version string could be reused by
        // mistake); compare on digest, not version label.
        let notice = detect_constitution_drift(
            Some("2026.06.05"),
            Some("digest-a"),
            "2026.06.05",
            "digest-b",
        );
        assert!(notice.is_some());
    }

    #[test]
    fn overflow_presend_block_errors_with_context_overflow_tag_when_over_window() {
        // effective_limit 30000 + margin 2000 → assumed window 32000.
        let err = overflow_presend_block(33_000, 30_000, 2_000)
            .expect("over-window estimate must block");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("context_overflow:"),
            "blocked error must be tagged for the scheduler's overflow recovery: {msg}"
        );
    }

    #[test]
    fn overflow_presend_block_allows_send_within_safety_margin() {
        // 31000 is over effective_limit (30000) but under the window (32000) —
        // only within the safety margin, so it is NOT blocked (sent as before).
        assert!(overflow_presend_block(31_000, 30_000, 2_000).is_none());
    }

    #[test]
    fn overflow_presend_block_boundary_at_window_is_allowed() {
        // Exactly at the assumed window is not "exceeds" — allowed.
        assert!(overflow_presend_block(32_000, 30_000, 2_000).is_none());
        // One token over the window blocks.
        assert!(overflow_presend_block(32_001, 30_000, 2_000).is_some());
    }

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
            singleton: false,
            resident_idle_ttl_secs: None,
        },
            capabilities,
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
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
            egress: None,
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
            true,
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
            true,
        );

        assert!(filter.allows("resolve"));
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
    fn test_compose_foundation_includes_sdk_for_code_execution() {
        let manifest = manifest_with_capabilities(vec![Capability::CodeExecution {
            patterns: vec!["python3 ".to_string()],
            commands: vec![],
        }]);
        let foundation = compose_foundation(&manifest);
        assert!(foundation.contains("# SDK Reference"));
    }

    #[test]
    fn test_compose_foundation_includes_sdk_for_agent_spawn() {
        let manifest = manifest_with_capabilities(vec![Capability::AgentSpawn {
            max_children: 5,
            max_spawn_depth: 0,
        }]);
        let foundation = compose_foundation(&manifest);
        assert!(foundation.contains("# SDK Reference"));
    }

    #[test]
    fn test_compose_foundation_includes_sdk_for_architect_role() {
        let mut manifest = manifest_with_capabilities(vec![Capability::WriteAccess {
            scopes: vec!["skills/*".to_string()],
        }]);
        manifest.agent.id = "architect.default".to_string();
        let foundation = compose_foundation(&manifest);
        assert!(foundation.contains("# SDK Reference"));
    }

    #[test]
    fn test_compose_foundation_includes_sdk_for_static_evaluator_role() {
        let mut manifest = manifest_with_capabilities(vec![]);
        manifest.agent.id = "static_evaluator.default".to_string();
        let foundation = compose_foundation(&manifest);
        assert!(foundation.contains("# SDK Reference"));
    }

    #[test]
    fn test_compose_foundation_excludes_sdk_for_minimal_reasoning_agent() {
        let manifest = manifest_with_capabilities(vec![]);
        let mut manifest = manifest;
        manifest.execution_mode = autonoetic_types::agent::ExecutionMode::Reasoning;
        manifest.agent.id = "unit_test_runner.default".to_string();
        let foundation = compose_foundation(&manifest);
        assert!(!foundation.contains("# SDK Reference"));
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
                reasoning_details: None,
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
                    reasoning_details: None,
                    stop_reason: StopReason::Other(String::new()),
                    usage: TokenUsage::default(),
                })
            } else {
                Ok(CompletionResponse {
                    text: "recovered reply".to_string(),
                    tool_calls: vec![],
                    reasoning_content: None,
                    reasoning_details: None,
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

    struct MaxTokensDriver;

    #[async_trait::async_trait]
    impl LlmDriver for MaxTokensDriver {
        async fn complete(
            &self,
            _request: &CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            Ok(CompletionResponse {
                text: "partial narrative, then the model got truncated…".to_string(),
                tool_calls: vec![],
                reasoning_content: None,
                reasoning_details: None,
                stop_reason: StopReason::MaxTokens,
                usage: TokenUsage::default(),
            })
        }
    }

    /// #846: the `StopReason::MaxTokens | Other(_)` arm must consult
    /// `waiting_for_child_yield_reason` (the same check the EndTurn arm
    /// performs) so a parent that spawned async children and then got
    /// truncated is NOT returned as `TurnOutcome::Completed`. Otherwise the
    /// scheduler would mark the task `Succeeded` while its children are
    /// still running — same premature-complete pattern as #845's EndTurn bug.
    #[tokio::test]
    async fn test_execute_with_history_maxtokens_suspends_when_children_pending() {
        use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};

        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(agents_dir.join("test-agent"))
            .expect("agent dir should create");

        let config = autonoetic_types::config::GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..autonoetic_types::config::GatewayConfig::default()
        };
        let gateway_dir = crate::execution::gateway_root_dir(&config);
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
        let store = Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
                .expect("store should open"),
        );

        // Seed a workflow with an active (non-terminal) task so that
        // `waiting_for_child_yield_reason` returns Some(WaitingForChild).
        // `ensure_workflow_for_root_session` also writes the root→workflow
        // index that `resolve_workflow_id_for_root_session` consults.
        let root_session = "root-maxtokens";
        let run = crate::scheduler::workflow_store::ensure_workflow_for_root_session(
            &config,
            Some(store.as_ref()),
            root_session,
            Some("test-agent"),
        )
        .expect("workflow should be created");
        let wf_id = run.workflow_id.clone();
        // Mark the workflow as WaitingChildren with an active task so the
        // helper's `is_waiting` check fires.
        let mut run = crate::scheduler::workflow_store::load_workflow_run(
            &config,
            Some(store.as_ref()),
            &wf_id,
        )
        .expect("workflow should load")
        .expect("workflow should exist");
        run.status = WorkflowRunStatus::WaitingChildren;
        run.active_task_ids = vec!["task-pending".to_string()];
        run.join_task_ids = vec!["task-pending".to_string()];
        crate::scheduler::workflow_store::save_workflow_run(
            &config,
            Some(store.as_ref()),
            &run,
        )
        .expect("workflow should save");

        let task = TaskRun {
            task_id: "task-pending".to_string(),
            workflow_id: wf_id.clone(),
            agent_id: "child-agent".to_string(),
            session_id: format!("{root_session}/child-agent-1"),
            parent_session_id: root_session.to_string(),
            status: TaskRunStatus::Running,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: Some("test-agent".to_string()),
            result_summary: None,
            join_group: None,
            message: Some("pending child".to_string()),
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        crate::scheduler::workflow_store::save_task_run(
            &config,
            Some(store.as_ref()),
            &task,
        )
        .expect("task should save");

        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(MaxTokensDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );
        runtime.config = Some(Arc::new(config));
        runtime.gateway_store = Some(store);
        // The reasoning loop reads `session_id` from the constructor path; set
        // it explicitly so `waiting_for_child_yield_reason` resolves the
        // workflow for this session.
        runtime.session_id = Some(root_session.to_string());

        let mut history = vec![
            Message::system("System prompt"),
            Message::user("Hello"),
        ];
        let outcome = runtime
            .execute_with_history(&mut history)
            .await
            .expect("execution should succeed");

        match outcome {
            TurnOutcome::WaitingForChild => {}
            other => panic!(
                "expected WaitingForChild on MaxTokens with pending async children (#846), got {:?}",
                other
            ),
        }
    }

    /// Negative case for #846: with NO pending async children, `MaxTokens`
    /// still completes normally — the new guard must not over-suspend.
    #[tokio::test]
    async fn test_execute_with_history_maxtokens_completes_without_children() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(agents_dir.join("test-agent"))
            .expect("agent dir should create");

        let config = autonoetic_types::config::GatewayConfig {
            agents_dir,
            ..autonoetic_types::config::GatewayConfig::default()
        };
        let gateway_dir = crate::execution::gateway_root_dir(&config);
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
        let store = Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
                .expect("store should open"),
        );

        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(MaxTokensDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );
        runtime.config = Some(Arc::new(config));
        runtime.gateway_store = Some(store);

        let mut history = vec![
            Message::system("System prompt"),
            Message::user("Hello"),
        ];
        let outcome = runtime
            .execute_with_history(&mut history)
            .await
            .expect("execution should succeed");

        match outcome {
            TurnOutcome::Completed(_) => {}
            other => panic!(
                "expected Completed on MaxTokens without pending children, got {:?}",
                other
            ),
        }
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
                reasoning_details: None,
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
        assert!(system.contains("content_write(name, content)"));
        assert!(system.contains("Agent local rules"));
    }

    #[tokio::test]
    async fn test_execute_loop_includes_content_patch_guidance_for_write_access() {
        // WriteAccess ⇒ content_patch is available and contributes its block.
        let manifest =
            manifest_with_capabilities(vec![Capability::WriteAccess { scopes: vec!["*".to_string()] }]);
        let temp = tempdir().expect("tempdir should create");
        let captured = Arc::new(Mutex::new(None));
        let driver = CaptureSystemDriver { system_message: Arc::clone(&captured) };
        let mut runtime = AgentExecutor::new(
            manifest,
            "rules".to_string(),
            Arc::new(driver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );
        runtime.execute_loop().await.expect("execution should succeed");
        let system = captured.lock().unwrap().clone().expect("system message captured");
        assert!(system.contains("Guidance"), "guidance section missing");
        assert!(
            system.contains("Editing existing content"),
            "content_patch guidance block missing"
        );
    }

    #[tokio::test]
    async fn test_execute_loop_omits_content_patch_guidance_without_write_access() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let captured = Arc::new(Mutex::new(None));
        let driver = CaptureSystemDriver { system_message: Arc::clone(&captured) };
        let mut runtime = AgentExecutor::new(
            manifest,
            "rules".to_string(),
            Arc::new(driver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );
        runtime.execute_loop().await.expect("execution should succeed");
        let system = captured.lock().unwrap().clone().expect("system message captured");
        assert!(
            !system.contains("Editing existing content"),
            "content_patch guidance must be absent without WriteAccess"
        );
    }

    #[tokio::test]
    async fn test_execute_loop_includes_migrated_tool_doctrine() {
        // WriteAccess ⇒ content_write block; CodeExecution ⇒ sandbox_exec
        // forbidden-commands + approval blocks; ReadAccess ⇒ workflow_state
        // resumption kernel (#466 migration from per-SKILL.md prose).
        let mut manifest = manifest_with_capabilities(vec![
            Capability::WriteAccess { scopes: vec!["*".to_string()] },
            Capability::CodeExecution { patterns: vec![], commands: vec![] },
            Capability::ReadAccess { scopes: vec!["*".to_string()] },
            Capability::AgentSpawn { max_children: 5, max_spawn_depth: 3 },
        ]);
        // Explicit Claude model → resolved_model_id → model_family, so the
        // content_patch Claude edit-format hint fires (#465/#479).
        manifest.llm_config = Some(autonoetic_types::agent::LlmConfig {
            provider: "anthropic".to_string(),
            model: "claude-opus-4-8".to_string(),
            temperature: 0.0,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
            thinking: None,
            egress_class: None,
        });
        let temp = tempdir().expect("tempdir should create");
        let captured = Arc::new(Mutex::new(None));
        let driver = CaptureSystemDriver { system_message: Arc::clone(&captured) };
        let mut runtime = AgentExecutor::new(
            manifest,
            "rules".to_string(),
            Arc::new(driver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            None,
        );
        runtime.execute_loop().await.expect("execution should succeed");
        let system = captured.lock().unwrap().clone().expect("system message captured");
        assert!(
            system.contains("`content_write` requires both `name` and `content`"),
            "content_write protocol block missing"
        );
        assert!(
            system.contains("Forbidden shell commands"),
            "sandbox_exec forbidden-commands block missing"
        );
        assert!(
            system.contains("Approval continuation"),
            "exec approval-continuation block missing"
        );
        assert!(
            system.contains("On any wake-up"),
            "workflow_state resumption kernel missing"
        );
        assert!(
            system.contains("prefer `mode=\"replace\"`"),
            "Claude-family edit-format hint missing (model_family resolution)"
        );
        assert!(
            system.contains("Coordinating children"),
            "agent_spawn orchestration kernel missing"
        );
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
                reasoning_details: None,
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
            } => approval_request_id,
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

    /// Issue #854: `max_session_turns_hard` is an absolute ceiling that
    /// terminates the session **without** raising a clearable approval — so a
    /// delegated child cannot extend past it even if it never surfaces an
    /// approval to the operator. With soft == hard == 1, the second turn trips
    /// the hard cap directly (no `SessionContinue` approval is ever minted).
    #[tokio::test]
    async fn test_max_session_turns_hard_cap_terminates_without_approval() {
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
        cfg.max_session_turns_hard = Some(1);

        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            Some(store.clone()),
        )
        .with_config(Arc::new(cfg))
        // Delegated child session (session_id contains '/').
        .with_session_id("root-hardcap/researcher.default-abcd1234");

        let mut history = vec![Message::system("System prompt"), Message::user("Turn 1")];
        let first = runtime
            .execute_with_history(&mut history)
            .await
            .expect("first turn should execute");
        assert!(matches!(first, TurnOutcome::Completed(_)));

        history.push(Message::user("Turn 2"));
        let second = runtime.execute_with_history(&mut history).await;
        let err = second.expect_err("second turn should hard-terminate, not suspend");
        let msg = err.to_string();
        assert!(
            msg.contains("max_session_turns_hard=1"),
            "hard-cap error should name the ceiling, got: {msg}"
        );

        // The hard cap is checked BEFORE the soft gate, so NO SessionContinue
        // approval is minted — there is nothing for an operator (or the auto
        // continuation path) to clear.
        let pending = store
            .get_pending_approvals()
            .expect("pending approval lookup should succeed");
        assert!(
            pending.is_empty(),
            "hard-cap trip must not create a clearable approval, found: {pending:?}"
        );

        // And the terminal cause is recorded as a causal event for audit.
        let events = store
            .search_causal_events(Some("root-hardcap/researcher.default-abcd1234"), None, 50)
            .expect("causal event search should succeed");
        assert!(
            events
                .iter()
                .any(|e| e.action == "session.turn_hard_cap"
                    && e.enforced_rules.iter().any(|r| r == "Ri-0.12")),
            "expected a session.turn_hard_cap causal event attributed to Ri-0.12"
        );
    }

    /// Issue #854: continuation approvals can extend the soft window, but never
    /// past `max_session_turns_hard`. With soft=1, hard=2: one approval grants a
    /// second window (turn 2), after which the hard cap terminates the session
    /// rather than raising a second approval.
    #[tokio::test]
    async fn test_max_session_turns_hard_cap_bounds_approved_windows() {
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
        cfg.max_session_turns_hard = Some(2);

        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            Some(store.clone()),
        )
        .with_config(Arc::new(cfg))
        .with_session_id("root-bound/researcher.default-abcd1234");

        let mut history = vec![Message::system("System prompt"), Message::user("Turn 1")];
        assert!(matches!(
            runtime
                .execute_with_history(&mut history)
                .await
                .expect("turn 1"),
            TurnOutcome::Completed(_)
        ));

        // Turn 2 hits the soft gate → approval; approve it to grant window 2.
        history.push(Message::user("Turn 2"));
        let request_id = match runtime
            .execute_with_history(&mut history)
            .await
            .expect("turn 2 should suspend at soft gate")
        {
            TurnOutcome::Suspended {
                approval_request_id,
            } => approval_request_id,
            other => panic!("expected Suspended, got {other:?}"),
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

        // Approved window 2 allows one more turn.
        history.push(Message::user("Turn 2 retry"));
        assert!(matches!(
            runtime
                .execute_with_history(&mut history)
                .await
                .expect("turn after approval"),
            TurnOutcome::Completed(_)
        ));

        // Next turn crosses the hard cap: terminate, do NOT raise a 2nd approval.
        history.push(Message::user("Turn 3"));
        let err = runtime
            .execute_with_history(&mut history)
            .await
            .expect_err("turn past hard cap should terminate");
        assert!(err.to_string().contains("max_session_turns_hard=2"));

        // Exactly one continuation approval was ever minted (window 2). The hard
        // cap did not mint a clearable one.
        let pending = store.get_pending_approvals().expect("pending lookup");
        assert!(
            pending.is_empty(),
            "hard-cap trip must not leave a pending approval, found: {pending:?}"
        );
        let approved = store
            .get_approved_approvals_for_session("root-bound/researcher.default-abcd1234")
            .expect("approved lookup");
        assert_eq!(
            approved.len(),
            1,
            "only the single soft-gate window should have been approved"
        );
    }

    /// Issue #854 Option 3: when a delegated child requests a 2nd+ continuation
    /// window, an observational `session.continuation_window_extended` event is
    /// emitted **keyed to the root session** so the operator/planner can see the
    /// child has been running for N windows.
    #[tokio::test]
    async fn test_delegated_continuation_window_emits_root_visible_event() {
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
        // Raise the system ceiling so the per-agent hard override below is not
        // clamped down — we need a generous hard cap to reach a 2nd window.
        cfg.max_session_turns_hard = Some(20);

        let child_session = "root-vis/researcher.default-abcd1234";
        let mut runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            Some(store.clone()),
        )
        .with_config(Arc::new(cfg))
        .with_session_id(child_session);
        // Per-agent hard override lifts this child's ceiling to 20 (the default
        // would be 2× the soft limit = 2, too tight to reach a 2nd window).
        runtime.loop_guard_declaration = Some(autonoetic_types::agent::LoopGuardDeclaration {
            max_session_turns_hard: Some(20),
            ..Default::default()
        });

        let mut history = vec![Message::system("System prompt"), Message::user("Turn 1")];
        runtime.execute_with_history(&mut history).await.expect("turn 1");

        // Drive through the soft gate twice, approving each window. The 2nd
        // window request (approved_windows == 1) is the one that emits the
        // root-visible event.
        for label in ["Turn 2", "Turn 3"] {
            history.push(Message::user(label));
            let request_id = match runtime
                .execute_with_history(&mut history)
                .await
                .expect("soft gate suspension")
            {
                TurnOutcome::Suspended {
                    approval_request_id,
                } => approval_request_id,
                other => panic!("expected Suspended at {label}, got {other:?}"),
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
            history.push(Message::user("retry after approval"));
            runtime
                .execute_with_history(&mut history)
                .await
                .expect("turn after approval");
        }

        // The visibility event is keyed to the ROOT session, not the child.
        let root_events = store
            .search_causal_events(Some("root-vis"), None, 100)
            .expect("root causal event search");
        let window_event = root_events
            .iter()
            .find(|e| e.action == "session.continuation_window_extended")
            .expect("expected a root-visible continuation_window_extended event");
        let payload: serde_json::Value = serde_json::from_str(
            window_event.payload.as_deref().unwrap_or("{}"),
        )
        .expect("payload should be JSON");
        assert_eq!(payload["child_session_id"], child_session);
        assert!(
            payload["requested_window"].as_u64().unwrap_or(0) >= 2,
            "event should record the child requesting a 2nd+ window, got {payload}"
        );
    }

    #[test]
    fn test_native_disclosure_path_extraction() {
        let registry = crate::runtime::tools::default_registry();
        // resolve uses name_or_handle but does not override extract_metadata
        let meta =
            registry.extract_metadata("resolve", "{\"name_or_handle\": \"secrets.txt\"}");
        assert_eq!(meta.path.as_deref(), None);
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
                    reasoning_details: None,
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
                    reasoning_details: None,
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
    fn execute_loop_close_outcome_maps_every_turn_outcome_variant() {
        let completed =
            session_close_outcome_from_turn_outcome(&TurnOutcome::Completed(None));
        let suspended = session_close_outcome_from_turn_outcome(&TurnOutcome::Suspended {
            approval_request_id: "apr-1".to_string(),
        });
        let user_input =
            session_close_outcome_from_turn_outcome(&TurnOutcome::SuspendedUserInput {
                interaction_id: "ui-1".to_string(),
            });
        let escalated = session_close_outcome_from_turn_outcome(&TurnOutcome::Escalated {
            escalation_request_id: "esc-1".to_string(),
        });

        assert_eq!(completed, SessionCloseOutcome::ExecuteLoopComplete);
        assert_eq!(suspended, SessionCloseOutcome::ExecuteLoopSuspended);
        assert_eq!(
            user_input,
            SessionCloseOutcome::ExecuteLoopSuspendedUserInput
        );
        assert_eq!(escalated, SessionCloseOutcome::ExecuteLoopEscalated);
    }

    #[test]
    fn execute_loop_close_outcome_tags_are_closed_and_stable() {
        let reasons = vec![
            SessionCloseOutcome::ExecuteLoopComplete.as_str(),
            SessionCloseOutcome::ExecuteLoopSuspended.as_str(),
            SessionCloseOutcome::ExecuteLoopSuspendedUserInput.as_str(),
            SessionCloseOutcome::ExecuteLoopEscalated.as_str(),
            SessionCloseOutcome::ExecuteLoopError.as_str(),
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
    // -- #968 egress pin×taint inline ask (RFC §5.3) ------------------------

    use autonoetic_types::background::UserInteractionAnswer;
    use autonoetic_types::config::LlmPreset;
    use autonoetic_types::egress::{EgressClass, EgressLabel, Sink};

    fn egress_preset(name: &str, class: EgressClass) -> (String, LlmPreset) {
        (
            name.to_string(),
            LlmPreset {
                provider: Some(if class == EgressClass::Local {
                    "ollama".to_string()
                } else {
                    "anthropic".to_string()
                }),
                model: Some(if class == EgressClass::Local {
                    "llama3".to_string()
                } else {
                    "claude-sonnet".to_string()
                }),
                temperature: Some(0.1),
                fallback_provider: None,
                fallback_model: None,
                chat_only: None,
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                thinking: None,
                tier: None,
                cost: None,
                latency: None,
                routing: None,
                egress_class: Some(class),
            },
        )
    }

    fn pin_ask_executor(
        presets: &[(&str, EgressClass)],
    ) -> (AgentExecutor, Arc<crate::scheduler::gateway_store::GatewayStore>, tempfile::TempDir)
    {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir");
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir");
        let store = Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
                .expect("gateway store"),
        );
        let mut cfg = GatewayConfig::default();
        cfg.agents_dir = temp.path().to_path_buf();
        cfg.llm_presets = presets
            .iter()
            .map(|(name, class)| egress_preset(name, *class))
            .collect();
        let runtime = AgentExecutor::new(
            manifest,
            "System prompt".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            crate::runtime::tools::default_registry(),
            Some(store.clone()),
        )
        .with_config(Arc::new(cfg));
        (runtime, store, temp)
    }

    fn provider_selected_events(
        store: &crate::scheduler::gateway_store::GatewayStore,
    ) -> Vec<serde_json::Value> {
        store
            .search_causal_events(Some("sess-1"), None, 100)
            .expect("search_causal_events")
            .into_iter()
            .filter(|e| e.action == "egress.provider_selected")
            .map(|e| {
                serde_json::from_str(e.payload.as_deref().unwrap_or("{}"))
                    .unwrap_or_else(|_| serde_json::json!({}))
            })
            .collect()
    }

    #[test]
    fn pinned_remote_tainted_batch_files_ask_and_marks_suspension() {
        let (mut runtime, store, _tmp) = pin_ask_executor(&[("remote", EgressClass::Remote), ("local", EgressClass::Local)]);
        let sel = runtime.plan_egress_routing(
            &EgressLabel::local_only(),
            "remote",
            Some(EgressClass::Remote),
            &[],
            "sess-1",
            "turn-000001",
            None,
            true,
        );
        let ask_id = sel.pending_ask.expect("ask should be filed");
        assert!(ask_id.starts_with("ui-"));
        assert!(sel.primary_driver.is_none());
        assert!(sel.refuse_reason.is_none());

        // Ask state rides the executor for the checkpoint.
        let state = runtime.egress_ask_state.as_ref().expect("ask state set");
        assert_eq!(state.interaction_id, ask_id);
        assert_eq!(state.turn_id, "turn-000001");
        assert_eq!(
            state.local_preset.as_ref().map(|c| c.name.as_str()),
            Some("local")
        );

        // The interaction exists with the three-way options and the tag.
        let it = store
            .get_user_interaction(&ask_id)
            .expect("lookup")
            .expect("interaction");
        assert!(crate::runtime::egress_labeler::is_egress_pin_ask(&it));
        let ids: Vec<&str> = it.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["declassify", "run_local", "abort"]);

        // provider_selected carries the filed inline-ask outcome (RFC §9.1).
        let events = provider_selected_events(&store);
        let ev = events.first().expect("provider_selected emitted");
        assert_eq!(ev["inline_ask"]["status"], "filed");
        assert_eq!(ev["inline_ask"]["interaction_id"], ask_id);
        assert_eq!(ev["chosen_preset"], serde_json::Value::Null);
    }

    #[test]
    fn pinned_ask_without_local_preset_offers_only_declassify_and_abort() {
        let (mut runtime, store, _tmp) = pin_ask_executor(&[("remote", EgressClass::Remote)]);
        let sel = runtime.plan_egress_routing(
            &EgressLabel::local_only(),
            "remote",
            Some(EgressClass::Remote),
            &[],
            "sess-1",
            "turn-000001",
            None,
            true,
        );
        let ask_id = sel.pending_ask.expect("ask filed");
        let it = store
            .get_user_interaction(&ask_id)
            .expect("lookup")
            .expect("interaction");
        let ids: Vec<&str> = it.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["declassify", "abort"]);
        assert!(runtime.egress_ask_state.as_ref().unwrap().local_preset.is_none());
    }

    #[test]
    fn unpinned_routing_primary_tainted_batch_reroutes_without_ask() {
        let (mut runtime, store, _tmp) = pin_ask_executor(&[("remote", EgressClass::Remote), ("local", EgressClass::Local)]);
        let sel = runtime.plan_egress_routing(
            &EgressLabel::local_only(),
            "routing",
            Some(EgressClass::Remote),
            &[],
            "sess-1",
            "turn-000001",
            None,
            false,
        );
        assert!(sel.pending_ask.is_none());
        assert!(sel.primary_driver.is_some(), "auto-reroute to the local preset");
        assert_eq!(sel.effective_class, Some(EgressClass::Local));
        assert!(runtime.egress_ask_state.is_none());
        assert!(
            store
                .get_pending_interactions_for_root_session("sess-1")
                .expect("pending")
                .is_empty(),
            "no ask for unpinned primaries"
        );
        let evs = provider_selected_events(&store);
        let ev = evs.first().expect("emitted");
        assert_eq!(ev["rerouted"], true);
        assert!(ev["inline_ask"].is_null());
    }

    #[test]
    fn resumed_ask_run_local_uses_offered_local_preset_for_the_turn() {
        let (mut runtime, store, _tmp) = pin_ask_executor(&[("remote", EgressClass::Remote), ("local", EgressClass::Local)]);
        let batch = EgressLabel::local_only();
        let first = runtime.plan_egress_routing(
            &batch, "remote", Some(EgressClass::Remote), &[], "sess-1", "turn-000001", None, true,
        );
        let ask_id = first.pending_ask.expect("ask filed");
        store
            .answer_user_interaction(&UserInteractionAnswer {
                interaction_id: ask_id.clone(),
                answer_option_id: Some(crate::runtime::egress_labeler::egress_ask_options::RUN_LOCAL.to_string()),
                answer_text: None,
                answered_by: "test-operator".to_string(),
            })
            .expect("answered");

        let sel = runtime.plan_egress_routing(
            &batch, "remote", Some(EgressClass::Remote), &[], "sess-1", "turn-000001", None, true,
        );
        assert!(sel.pending_ask.is_none());
        assert!(sel.refuse_reason.is_none());
        assert!(sel.primary_driver.is_some(), "resumed turn runs the local preset");
        assert_eq!(sel.effective_class, Some(EgressClass::Local));
        assert_eq!(
            sel.effective_model.as_deref(),
            Some("llama3"),
            "cost/tracing attribute the local model"
        );
        // The override persists for the rest of THIS turn.
        assert!(runtime.egress_ask_state.is_some());

        let evs = provider_selected_events(&store);
        let ev = evs.first().expect("emitted");
        assert_eq!(ev["inline_ask"]["status"], "answered");
        assert_eq!(ev["inline_ask"]["outcome"], "run_local");
        assert_eq!(ev["chosen_preset"], "local");
    }

    #[test]
    fn resumed_ask_declassify_materializes_grant_and_keeps_pinned_primary() {
        let (mut runtime, store, _tmp) = pin_ask_executor(&[("remote", EgressClass::Remote), ("local", EgressClass::Local)]);
        let batch = EgressLabel::local_only();
        let first = runtime.plan_egress_routing(
            &batch, "remote", Some(EgressClass::Remote), &[], "sess-1", "turn-000001", None, true,
        );
        let ask_id = first.pending_ask.expect("ask filed");
        store
            .answer_user_interaction(&UserInteractionAnswer {
                interaction_id: ask_id.clone(),
                answer_option_id: Some(crate::runtime::egress_labeler::egress_ask_options::DECLASSIFY.to_string()),
                answer_text: None,
                answered_by: "test-operator".to_string(),
            })
            .expect("answered");
        // The answer-time hook (interaction_answer.rs) materializes the grant.
        let it = store
            .get_user_interaction(&ask_id)
            .expect("lookup")
            .expect("interaction");
        crate::runtime::egress_labeler::apply_egress_ask_declassification(&store, &it)
            .expect("grant materialized");
        assert!(
            crate::runtime::egress_labeler::session_sink_declassified(
                &store,
                "sess-1",
                "sess-1",
                Sink::RemoteModel
            ),
            "session-wide grant live after the declassify answer"
        );

        let sel = runtime.plan_egress_routing(
            &batch, "remote", Some(EgressClass::Remote), &[], "sess-1", "turn-000001", None, true,
        );
        assert!(sel.pending_ask.is_none());
        assert!(sel.refuse_reason.is_none());
        assert!(sel.primary_driver.is_none(), "pinned primary kept");
        assert!(runtime.egress_ask_state.is_none(), "ask state consumed");

        let evs = provider_selected_events(&store);
        let ev = evs.first().expect("emitted");
        assert_eq!(ev["inline_ask"]["status"], "answered");
        assert_eq!(ev["inline_ask"]["outcome"], "declassify");
        assert_eq!(ev["declassified_remote"], true);
        assert_eq!(ev["chosen_preset"], "remote");
    }

    #[test]
    fn resumed_ask_abort_refuses_the_turn() {
        let (mut runtime, store, _tmp) = pin_ask_executor(&[("remote", EgressClass::Remote), ("local", EgressClass::Local)]);
        let batch = EgressLabel::local_only();
        let first = runtime.plan_egress_routing(
            &batch, "remote", Some(EgressClass::Remote), &[], "sess-1", "turn-000001", None, true,
        );
        let ask_id = first.pending_ask.expect("ask filed");
        store
            .answer_user_interaction(&UserInteractionAnswer {
                interaction_id: ask_id.clone(),
                answer_option_id: Some(crate::runtime::egress_labeler::egress_ask_options::ABORT.to_string()),
                answer_text: None,
                answered_by: "test-operator".to_string(),
            })
            .expect("answered");

        let sel = runtime.plan_egress_routing(
            &batch, "remote", Some(EgressClass::Remote), &[], "sess-1", "turn-000001", None, true,
        );
        assert!(sel.pending_ask.is_none());
        let reason = sel.refuse_reason.expect("turn refused");
        assert!(reason.contains("egress_aborted_by_operator"), "reason: {reason}");
        assert!(runtime.egress_ask_state.is_none());

        let evs = provider_selected_events(&store);
        let ev = evs.first().expect("emitted");
        assert_eq!(ev["inline_ask"]["status"], "answered");
        assert_eq!(ev["inline_ask"]["outcome"], "abort");
        assert_eq!(ev["no_eligible_provider"], true);
    }

    #[test]
    fn ask_state_is_turn_scoped_and_stale_state_is_dropped() {
        let (mut runtime, store, _tmp) = pin_ask_executor(&[("remote", EgressClass::Remote), ("local", EgressClass::Local)]);
        let batch = EgressLabel::local_only();
        let first = runtime.plan_egress_routing(
            &batch, "remote", Some(EgressClass::Remote), &[], "sess-1", "turn-000001", None, true,
        );
        let ask_id = first.pending_ask.expect("ask filed");
        store
            .answer_user_interaction(&UserInteractionAnswer {
                interaction_id: ask_id.clone(),
                answer_option_id: Some(crate::runtime::egress_labeler::egress_ask_options::RUN_LOCAL.to_string()),
                answer_text: None,
                answered_by: "test-operator".to_string(),
            })
            .expect("answered");

        // Next turn: the stale state is dropped and the fresh conflict files a
        // new ask (the answered one is no longer pending, so no reuse).
        let sel = runtime.plan_egress_routing(
            &batch, "remote", Some(EgressClass::Remote), &[], "sess-1", "turn-000002", None, true,
        );
        let ask2 = sel.pending_ask.expect("fresh ask for the new turn");
        assert_ne!(ask2, ask_id);
        assert_eq!(
            runtime.egress_ask_state.as_ref().unwrap().turn_id,
            "turn-000002"
        );
    }
}

#[cfg(test)]
mod divergence_robustness_tests {
    use super::is_signal_derived_exit;

    #[test]
    fn signal_derived_exit_codes_are_irrecoverable() {
        assert!(is_signal_derived_exit(
            &serde_json::json!({"ok": false, "exit_code": 130})
        ));
        assert!(is_signal_derived_exit(
            &serde_json::json!({"ok": false, "exit_code": 137})
        ));
    }

    #[test]
    fn non_signal_exits_are_not_irrecoverable() {
        assert!(!is_signal_derived_exit(
            &serde_json::json!({"ok": false, "exit_code": 1})
        ));
        assert!(!is_signal_derived_exit(
            &serde_json::json!({"ok": true, "exit_code": 137})
        ));
        assert!(!is_signal_derived_exit(
            &serde_json::json!({"ok": false})
        ));
    }

    /// Regression: a task-bound agent whose own task is the only active one
    /// must NOT get WaitingForChild. Without the own-task-id exclusion in
    /// `waiting_for_child_yield_reason`, the function sees
    /// `active_task_ids.is_empty() == false` and falsely concludes there are
    /// children to wait for — even though the sole active task is the caller.
    #[test]
    fn waiting_for_child_yield_reason_returns_none_when_only_own_task_active() {
        use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};

        let temp = tempfile::tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agent dir should create");

        let config = autonoetic_types::config::GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..autonoetic_types::config::GatewayConfig::default()
        };
        let gateway_dir = crate::execution::gateway_root_dir(&config);
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
                .expect("store should open"),
        );

        let root_session = "root-own-task";
        let child_session = format!("{root_session}/executor.x");
        let task_id = "task-x";

        let run = crate::scheduler::workflow_store::ensure_workflow_for_root_session(
            &config,
            Some(store.as_ref()),
            root_session,
            Some("planner.default"),
        )
        .expect("workflow should be created");
        let wf_id = run.workflow_id.clone();

        let mut run = crate::scheduler::workflow_store::load_workflow_run(
            &config,
            Some(store.as_ref()),
            &wf_id,
        )
        .expect("workflow should load")
        .expect("workflow should exist");
        run.status = WorkflowRunStatus::Active;
        run.active_task_ids = vec![task_id.to_string()];
        crate::scheduler::workflow_store::save_workflow_run(
            &config,
            Some(store.as_ref()),
            &run,
        )
        .expect("workflow should save");

        let task = TaskRun {
            task_id: task_id.to_string(),
            workflow_id: wf_id.clone(),
            agent_id: "executor.x".to_string(),
            session_id: child_session.clone(),
            parent_session_id: root_session.to_string(),
            status: TaskRunStatus::Running,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: Some("work".to_string()),
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        crate::scheduler::workflow_store::save_task_run(
            &config,
            Some(store.as_ref()),
            &task,
        )
        .expect("task should save");

        // The child agent is the only active task — no OTHER children.
        let reason = super::waiting_for_child_yield_reason(
            &config,
            Some(store.as_ref()),
            &child_session,
        );
        assert!(
            reason.is_none(),
            "waiting_for_child_yield_reason must return None when the only \
             active task belongs to the caller's own session, got: {:?}",
            reason
        );
    }

    /// Regression: `WaitingChildren` workflow status alone must not trigger
    /// `WaitingForChild` — it is set when the current task is enqueued and
    /// cleared only on terminal status, so it is a stale label when the sole
    /// active task IS the caller. `has_other_active` is the real signal.
    #[test]
    fn waiting_for_child_yield_reason_ignores_stale_waiting_children() {
        use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};

        let temp = tempfile::tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agent dir should create");

        let config = autonoetic_types::config::GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..autonoetic_types::config::GatewayConfig::default()
        };
        let gateway_dir = crate::execution::gateway_root_dir(&config);
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
                .expect("store should open"),
        );

        let root_session = "root-wc-only";
        let child_session = format!("{root_session}/executor.y");
        let task_id = "task-y";

        let run = crate::scheduler::workflow_store::ensure_workflow_for_root_session(
            &config,
            Some(store.as_ref()),
            root_session,
            Some("planner.default"),
        )
        .expect("workflow should be created");
        let wf_id = run.workflow_id.clone();

        let mut run = crate::scheduler::workflow_store::load_workflow_run(
            &config,
            Some(store.as_ref()),
            &wf_id,
        )
        .expect("workflow should load")
        .expect("workflow should exist");
        run.status = WorkflowRunStatus::WaitingChildren;
        run.active_task_ids = vec![task_id.to_string()];
        crate::scheduler::workflow_store::save_workflow_run(
            &config,
            Some(store.as_ref()),
            &run,
        )
        .expect("workflow should save");

        let task = TaskRun {
            task_id: task_id.to_string(),
            workflow_id: wf_id.clone(),
            agent_id: "executor.y".to_string(),
            session_id: child_session.clone(),
            parent_session_id: root_session.to_string(),
            status: TaskRunStatus::Running,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: Some("work".to_string()),
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        crate::scheduler::workflow_store::save_task_run(
            &config,
            Some(store.as_ref()),
            &task,
        )
        .expect("task should save");

        // The workflow is WaitingChildren, but the only active task is the
        // caller's own — must NOT yield WaitingForChild.
        let reason = super::waiting_for_child_yield_reason(
            &config,
            Some(store.as_ref()),
            &child_session,
        );
        assert!(
            reason.is_none(),
            "waiting_for_child_yield_reason must return None when WaitingChildren \
             is set but the sole active task belongs to the caller, got: {:?}",
            reason
        );
    }

    /// Sibling-deadlock regression (session-d484ea13): a leaf task that ends
    /// its turn while a SIBLING is still active must NOT yield WaitingForChild
    /// — it spawned nothing, so nothing will ever wake it. The park predicate
    /// is parent-scoped: only non-terminal tasks the session itself spawned
    /// count. Conversely, the spawning parent (root) MUST yield while any of
    /// its children are non-terminal.
    #[test]
    fn waiting_for_child_yield_reason_ignores_active_siblings() {
        use autonoetic_types::workflow::{TaskRun, TaskRunStatus};

        let temp = tempfile::tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agent dir should create");

        let config = autonoetic_types::config::GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..autonoetic_types::config::GatewayConfig::default()
        };
        let gateway_dir = crate::execution::gateway_root_dir(&config);
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
                .expect("store should open"),
        );

        let root_session = "root-sibling-park";
        let leaf_session = format!("{root_session}/static_evaluator.x");
        let sibling_session = format!("{root_session}/auditor.y");

        let run = crate::scheduler::workflow_store::ensure_workflow_for_root_session(
            &config,
            Some(store.as_ref()),
            root_session,
            Some("planner.default"),
        )
        .expect("workflow should be created");
        let wf_id = run.workflow_id.clone();

        let mk = |task_id: &str, session_id: &str| TaskRun {
            task_id: task_id.to_string(),
            workflow_id: wf_id.clone(),
            agent_id: "executor.x".to_string(),
            session_id: session_id.to_string(),
            parent_session_id: root_session.to_string(),
            status: TaskRunStatus::Running,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: Some("work".to_string()),
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        crate::scheduler::workflow_store::save_task_run(
            &config,
            Some(store.as_ref()),
            &mk("task-leaf", &leaf_session),
        )
        .expect("leaf task should save");
        crate::scheduler::workflow_store::save_task_run(
            &config,
            Some(store.as_ref()),
            &mk("task-sibling", &sibling_session),
        )
        .expect("sibling task should save");

        // The leaf spawned nothing: an active sibling must NOT park it.
        let reason = super::waiting_for_child_yield_reason(
            &config,
            Some(store.as_ref()),
            &leaf_session,
        );
        assert!(
            reason.is_none(),
            "leaf session must NOT yield WaitingForChild on an active sibling, got: {:?}",
            reason
        );

        // The root planner spawned both tasks: it MUST yield while they run.
        let reason = super::waiting_for_child_yield_reason(
            &config,
            Some(store.as_ref()),
            root_session,
        );
        assert!(
            matches!(
                reason,
                Some(super::YieldReason::WaitingForChild { .. })
            ),
            "root session must yield WaitingForChild while its spawned children are \
             non-terminal, got: {:?}",
            reason
        );
    }

}