//! Session Checkpoint: universal execution snapshots at all yield points.
//!
//! Generalizes the approval-specific `TurnContinuation` into a universal snapshot
//! that can restore an agent session from hibernation, budget exhaustion, max turns,
//! crash recovery, and approval suspension.
//!
//! # Integrity
//!
//! Checkpoint files are HMAC-SHA256 signed using a per-gateway key derived from
//! `GatewayConfig::continuation_key` (or `node_id` as fallback). On load, the
//! signature is verified before the payload is deserialized. Tampered files are
//! rejected with an error.
//!
//! Storage: `.gateway/checkpoints/{session_id}/{turn_id}.checkpoint.json`

use crate::llm::Message;
use crate::runtime::compression::CompressionMetadata;
use crate::runtime::guard::LoopGuard;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::trajectory::FeedbackEvent;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Why execution stopped and a checkpoint was saved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum YieldReason {
    /// Agent paused between turns (EndTurn / StopSequence).
    Hibernation,
    /// Agent finished its task but is declared **resident**
    /// (`agent.resident_idle_ttl_secs`): the session is parked and stays
    /// addressable by `agent_message` rather than terminating. Resumes when a
    /// message arrives; reaped and closed normally once `ttl_secs` elapse
    /// without traffic.
    ///
    /// This is the only yield reason that represents "no work in flight" — every
    /// other suspension is waiting on something specific (a child, an approval,
    /// a human). It exists so that a peer agent has somewhere to *be* between
    /// messages, which is what makes `agent_message` reach anything other than
    /// an orchestrator.
    Idle {
        /// RFC3339 timestamp the session parked at (refreshed on each park).
        since: String,
        /// Seconds of inactivity after `since` before the reaper closes it.
        ttl_secs: u64,
    },
    /// Session budget depleted mid-execution.
    BudgetExhausted,
    /// Approval gate (overlaps TurnContinuation).
    ApprovalRequired { approval_request_id: String },
    /// Explicit question / choice for the human.
    UserInputRequired { interaction_id: String },
    /// Session is suspended waiting for child workflow/task state to change.
    WaitingForChild {
        workflow_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    /// Operator circuit breaker; do not auto-resume.
    EmergencyStop { stop_id: String },
    /// Loop guard limit reached.
    MaxTurnsReached,
    /// Operator/user interrupt.
    ManualStop,
    /// Recoverable error.
    Error(String),
    /// Agent escalated to human operator for guidance.
    HumanEscalation { escalation_request_id: String },
    /// Parent session terminated (crash, emergency stop, exit); child is orphaned.
    ParentTerminated {
        parent_session_id: String,
        reason: String,
    },
}

/// Snapshot of LLM configuration needed for reproducible execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmConfigSnapshot {
    pub provider: String,
    pub model: String,
    pub temperature: f64,
    pub fallback_provider: Option<String>,
    pub fallback_model: Option<String>,
    pub chat_only: bool,
    pub context_window_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_override_preset: Option<String>,
}

impl LlmConfigSnapshot {
    pub fn from_config(config: &autonoetic_types::agent::LlmConfig) -> Self {
        Self {
            provider: config.provider.clone(),
            model: config.model.clone(),
            temperature: config.temperature,
            fallback_provider: config.fallback_provider.clone(),
            fallback_model: config.fallback_model.clone(),
            chat_only: config.chat_only,
            context_window_tokens: config.context_window_tokens,
            preset_name: config.routing_preset.clone(),
            preset_source: None,
            session_override_preset: None,
        }
    }

    pub fn from_inference_profile(
        profile: &crate::runtime::inference_profile::ResolvedInferenceProfile,
    ) -> Self {
        let mut snap = Self::from_config(&profile.llm_config);
        snap.preset_name = profile.preset_name.clone();
        snap.preset_source = Some(profile.snapshot_preset_source());
        snap.session_override_preset = profile.session_override_preset.clone();
        snap
    }
}

/// State for a session suspended mid-tool-batch (approval gate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingToolState {
    /// Tool results already collected before the suspension point.
    pub completed_tool_results: Vec<(String, String, String)>, // (call_id, tool_name, result_json)
    /// The tool call that triggered the suspension.
    pub pending_tool_call: PendingToolCall,
    /// Tool calls that were NOT executed because they came after the suspended one.
    pub remaining_tool_calls: Vec<crate::llm::ToolCall>,
}

/// A tool call that is pending execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: String,
    /// For approval gates, the approval response JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_response: Option<String>,
}

/// Complete execution snapshot for session respawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCheckpoint {
    // --- Execution state (enough to call execute_with_history) ---
    /// Full conversation history up to this point.
    pub history: Vec<Message>,
    /// Current turn number.
    pub turn_counter: u64,
    /// Loop guard state (failure counts, progress tracking).
    pub loop_guard_state: LoopGuard,
    /// Egress label sidecar (RFC data-envelopes §3.4 / §5.6): the
    /// `tool_call_id → EgressLabel` map accumulated over the session. Persisted
    /// so labels survive suspend / resume / fork — a resumed (or forked)
    /// session must withhold from a provider exactly what the live session
    /// would, satisfying the "no envelope label lost across checkpoint /
    /// continuation" acceptance bar (#907). `#[serde(default)]` +
    /// skip-if-empty: unconfigured deployments store nothing, and checkpoints
    /// predating this field deserialize with an empty map.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub egress_labels: std::collections::HashMap<String, autonoetic_types::egress::EgressLabel>,
    /// A filed pin×taint conflict ask (RFC §5.3 / #968) whose answer still
    /// shapes this turn's routing: carried across the suspension so the
    /// resumed turn honors the operator's choice (declassify / run local /
    /// abort) without re-deriving the already-consumed batch taint.
    /// `#[serde(default)]` so checkpoints predating the field deserialize
    /// with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_ask: Option<crate::runtime::egress_labeler::EgressAskState>,
    /// Session runtime state (Normal or Degraded).
    #[serde(default)]
    pub session_state: autonoetic_types::agent::SessionState,
    /// Whether the session had escalated to all tool tiers.
    #[serde(default)]
    pub tool_tier_escalated: bool,
    /// Tool names explicitly discovered via `tool_discover`.
    #[serde(default)]
    pub discovered_tools: std::collections::HashSet<String>,
    #[serde(default)]
    pub blocked_state_event_emitted: bool,
    /// Whether the extended SKILL.md half has been mechanically loaded on the
    /// first tool call (#1015). Persisted so a resumed session keeps the same
    /// loaded state: an already-loaded session must not re-inject the
    /// `gateway_note`, and an un-loaded one must not inline extended before
    /// its first tool call.
    #[serde(default)]
    pub extended_loaded: bool,

    // --- Session identity ---
    pub agent_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub workflow_id: Option<String>,
    pub task_id: Option<String>,

    // --- Reproducibility ---
    /// SHA-256 of runtime.lock content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_lock_hash: Option<String>,
    /// Constitution version that admitted this session (#821), pinned at
    /// session start and updated only when the resume-time drift check
    /// notices the process constitution has changed. `None` when the
    /// constitution runtime was never initialized (e.g. many unit tests).
    /// `#[serde(default)]` so checkpoints predating this field deserialize
    /// with `None` (backward compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constitution_version: Option<String>,
    /// Constitution digest paired with `constitution_version` above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constitution_digest: Option<String>,
    /// LLM configuration at session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_config_snapshot: Option<LlmConfigSnapshot>,
    /// Hash of registered tool set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_registry_version: Option<String>,

    // --- Context ---
    /// Why execution stopped.
    pub yield_reason: YieldReason,
    /// (name, handle) pairs active in session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_store_refs: Vec<(String, String)>,
    /// RFC3339 timestamp.
    pub created_at: String,

    // --- Pending work (for mid-tool-batch suspension) ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_tool_state: Option<PendingToolState>,

    // --- Budget tracking ---
    /// LLM rounds consumed so far.
    #[serde(default)]
    pub llm_rounds_consumed: u64,
    /// Tool invocations consumed so far.
    #[serde(default)]
    pub tool_invocations_consumed: u64,
    /// Tokens consumed so far.
    #[serde(default)]
    pub tokens_consumed: u64,
    /// Estimated cost so far (USD).
    #[serde(default)]
    pub estimated_cost_usd: f64,

    // --- Compression ---
    /// Compression metadata if context compression was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression_metadata: Option<CompressionMetadata>,

    /// Current state capsule for hierarchical summarization (Phase 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capsule_state: Option<crate::runtime::context_governor::capsule::StateCapsule>,

    // --- Approval continuation fields (for TurnContinuation unification) ---
    /// The assistant message containing the tool call(s) that triggered approval.
    /// Re-appended to history on resume before the tool result messages.
    ///
    /// **Boxed** to keep `SessionCheckpoint` small on the stack: it is embedded
    /// by value in the JSON-RPC dispatch/execute futures, whose poll frame is
    /// razor-thin against libtest's 2 MiB test-thread limit (#884/#916). `Message`
    /// is the largest inline field and is set only on the approval-continuation
    /// path, so boxing it (8 bytes vs ~130) buys headroom for other fields (e.g.
    /// the egress label sidecar) without a wire-format change — `Option<Box<T>>`
    /// serializes identically to `Option<T>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_message: Option<Box<Message>>,

    /// The `ScheduledAction` pending approval — stored for TOCTOU verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_action: Option<autonoetic_types::background::ScheduledAction>,

    /// Wall-clock timestamp of suspension (RFC3339). Used by the scheduler
    /// timeout checker to fail tasks that wait too long for approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_at: Option<String>,

    /// Sentinel divergence suppression state (sentinel.suppress).
    /// Without this, suppression is lost on resume and the planner is
    /// re-notified about divergence it already suppressed.
    #[serde(default)]
    pub suppress_until_turn: u64,

    /// Last known trajectory divergence level (e.g. "healthy", "diverging").
    /// Without this, the monitor resets to None on resume and the first
    /// post-resume tick always fires level_changed=true, re-detecting
    /// divergence from the restored LoopGuard state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trajectory_last_level: Option<String>,

    /// Feedback events the gateway issued to the agent before this checkpoint.
    /// Restored into the trajectory monitor so cross-turn `FeedbackIgnored`
    /// detection survives respawn, repair, and approval continuation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback_events: Vec<(u64, FeedbackEvent)>,
}

impl SessionCheckpoint {
    pub fn restore_into(&self, runtime: &mut crate::runtime::lifecycle::AgentExecutor) {
        runtime.guard =
            crate::runtime::guard::LoopGuard::restore(self.loop_guard_state.clone());
        // Restore the egress label sidecar so the resumed session withholds the
        // same labeled content the live session would (RFC data-envelopes §3.4).
        // `#[serde(default)]` on the field means old checkpoints restore empty.
        runtime.egress_labels = self.egress_labels.clone();
        runtime.egress_ask_state = self.egress_ask.clone();
        runtime.session_state = self.session_state;
        runtime.tool_tier_escalated = self.tool_tier_escalated;
        runtime.discovered_tools = self.discovered_tools.clone();
        runtime.extended_loaded = self.extended_loaded;
        runtime.session_started = true;
        runtime.turn_counter = self.turn_counter;
        runtime.blocked_state_event_emitted = self.blocked_state_event_emitted;
        runtime.runtime_lock_hash = self.runtime_lock_hash.clone();
        runtime.constitution_version = self.constitution_version.clone();
        runtime.constitution_digest = self.constitution_digest.clone();
        if let Some(ref cm) = self.compression_metadata {
            runtime.compression_metadata = cm.clone();
        }
        // Restore the prior state capsule so the next governor run evolves it
        // incrementally instead of re-bootstrapping from an empty shell.
        // `#[serde(default)]` on the field means old checkpoints restore `None`.
        runtime.capsule_state = self.capsule_state.clone();
        runtime.suppress_until_turn =
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(self.suppress_until_turn));
        runtime
            .trajectory_monitor
            .restore_last_level(self.trajectory_last_level.as_deref());
        runtime
            .trajectory_monitor
            .restore_feedback(self.feedback_events.clone());
    }

    pub fn initial_user_message(&self) -> String {
        self.history
            .iter()
            .find(|m| matches!(m.role, crate::llm::Role::User))
            .map(|m| m.content.clone())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// HMAC integrity
// ---------------------------------------------------------------------------

/// Resolve the HMAC key for checkpoint signing.  Uses the explicit
/// `continuation_key` config value when set, otherwise derives a key from
/// `node_id`.  **Warning:** the `node_id`-derived default is not a secret and
/// only provides detection of accidental corruption, not protection against a
/// local attacker who can read the config.  Production deployments should set
/// `continuation_key` to a high-entropy secret.
pub fn checkpoint_hmac_key(config: &GatewayConfig) -> String {
    config
        .continuation_key
        .clone()
        .unwrap_or_else(|| format!("autonoetic-checkpoint-{}", config.node_id))
}

/// Produce a deterministic JSON representation.  `serde_json` with sorted keys
/// ensures the same struct always serialises to the same bytes.
pub fn canonical_json<T: serde::Serialize>(value: &T) -> anyhow::Result<String> {
    let mut buf =
        serde_json::Serializer::with_formatter(Vec::new(), serde_json::ser::CompactFormatter);
    serde::Serialize::serialize(value, &mut buf)?;
    let v: serde_json::Value = serde_json::from_slice(&buf.into_inner())?;
    let sorted = serde_json::to_string(&v)?;
    Ok(sorted)
}

/// HMAC-signed envelope wrapping a serialised `SessionCheckpoint` payload.
/// All checkpoints are signed — not just approval ones — to provide a uniform
/// integrity guarantee.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SignedCheckpoint {
    /// Canonical-JSON serialised `SessionCheckpoint`.
    pub payload_json: String,
    /// HMAC-SHA256 hex digest over `payload_json` bytes using the gateway key.
    pub hmac_hex: String,
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

/// Root directory for checkpoint files: `.gateway/checkpoints/`.
pub fn checkpoints_dir(config: &GatewayConfig) -> PathBuf {
    config.agents_dir.join(".gateway").join("checkpoints")
}

/// Canonical turn-id string for a turn number. This is the single source of
/// truth for the on-disk checkpoint filename stem (`turn-000003`), shared by
/// the lifecycle (which writes checkpoints) and any caller that wants to load a
/// checkpoint by turn number (e.g. `trace fork --at-turn N`, `session.fork`).
pub fn turn_id_for(turn: u64) -> String {
    format!("turn-{:06}", turn)
}

/// Parse the numeric turn from a canonical `turn-000003` id.
pub fn turn_number_from_id(turn_id: &str) -> Option<u64> {
    turn_id.strip_prefix("turn-").and_then(|n| n.parse::<u64>().ok())
}

fn checkpoint_path(config: &GatewayConfig, session_id: &str, turn_id: &str) -> PathBuf {
    checkpoints_dir(config)
        .join(sanitize_path_component(session_id))
        .join(format!(
            "{}.checkpoint.json",
            sanitize_path_component(turn_id)
        ))
}

pub fn sanitize_path_component(s: &str) -> String {
    s.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
}

/// Persist a `SessionCheckpoint` for the given session and turn, wrapped in an
/// HMAC-signed envelope.
pub fn save_checkpoint(
    config: &GatewayConfig,
    checkpoint: &SessionCheckpoint,
) -> anyhow::Result<()> {
    let dir = checkpoints_dir(config).join(sanitize_path_component(&checkpoint.session_id));
    std::fs::create_dir_all(&dir)?;
    let path = checkpoint_path(config, &checkpoint.session_id, &checkpoint.turn_id);

    let payload_json = canonical_json(checkpoint)?;
    let key = checkpoint_hmac_key(config);
    let hmac_hex = crate::server::ofp::hmac_sign(&key, payload_json.as_bytes());

    let envelope = SignedCheckpoint {
        payload_json,
        hmac_hex,
    };
    let json = serde_json::to_string_pretty(&envelope)?;
    std::fs::write(&path, json)?;
    tracing::debug!(
        target: "checkpoint",
        session_id = %checkpoint.session_id,
        turn_id = %checkpoint.turn_id,
        yield_reason = ?checkpoint.yield_reason,
        path = %path.display(),
        "Saved signed session checkpoint"
    );
    Ok(())
}

/// Error returned when a checkpoint file fails HMAC integrity verification.
/// Used to distinguish tamper-detection errors from ordinary I/O or parse
/// failures.
#[derive(Debug)]
pub struct CheckpointIntegrityError {
    pub session_id: String,
    pub turn_id: String,
    pub message: String,
}

impl std::fmt::Display for CheckpointIntegrityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "checkpoint integrity violation for session '{}' turn '{}': {}",
            self.session_id, self.turn_id, self.message
        )
    }
}

impl std::error::Error for CheckpointIntegrityError {}

/// Returns `true` if the error is a `CheckpointIntegrityError` (HMAC
/// mismatch / tamper detection).
pub fn is_integrity_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CheckpointIntegrityError>().is_some()
}

/// Load a specific checkpoint by session and turn ID, verifying HMAC integrity.
pub fn load_checkpoint(
    config: &GatewayConfig,
    session_id: &str,
    turn_id: &str,
) -> anyhow::Result<Option<SessionCheckpoint>> {
    let path = checkpoint_path(config, session_id, turn_id);
    if !path.exists() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(&path)?;

    // Try signed envelope format first.
    if let Ok(envelope) = serde_json::from_str::<SignedCheckpoint>(&json) {
        let key = checkpoint_hmac_key(config);
        if !crate::server::ofp::hmac_verify(
            &key,
            envelope.payload_json.as_bytes(),
            &envelope.hmac_hex,
        ) {
            return Err(CheckpointIntegrityError {
                session_id: session_id.to_string(),
                turn_id: turn_id.to_string(),
                message: "HMAC mismatch".to_string(),
            }
            .into());
        }
        let checkpoint: SessionCheckpoint =
            serde_json::from_str(&envelope.payload_json)?;
        return Ok(Some(checkpoint));
    }

    // Legacy unsigned format — try direct deserialization.
    // This path exists for cleanup; new checkpoints are always signed.
    let checkpoint: SessionCheckpoint = serde_json::from_str(&json)?;
    Ok(Some(checkpoint))
}

/// Verify HMAC and deserialize a checkpoint from raw JSON, handling both
/// the signed envelope format and the legacy unsigned format.
///
/// Tampered checkpoints are rejected; `None` is returned for files that
/// fail both signed and legacy deserialization (e.g. corrupt/truncated).
fn verify_and_deserialize_checkpoint(
    config: &GatewayConfig,
    json: &str,
) -> Option<SessionCheckpoint> {
    // Signed envelope format — verify HMAC before trusting payload.
    if let Ok(envelope) = serde_json::from_str::<SignedCheckpoint>(json) {
        let key = checkpoint_hmac_key(config);
        if !crate::server::ofp::hmac_verify(
            &key,
            envelope.payload_json.as_bytes(),
            &envelope.hmac_hex,
        ) {
            tracing::warn!(
                target: "checkpoint",
                "HMAC verification failed during scan — skipping tampered checkpoint"
            );
            return None;
        }
        return serde_json::from_str(&envelope.payload_json).ok();
    }
    // Legacy unsigned format
    serde_json::from_str::<SessionCheckpoint>(json).ok()
}

/// Load the latest checkpoint for a session (highest turn number).
pub fn load_latest_checkpoint(
    config: &GatewayConfig,
    session_id: &str,
) -> anyhow::Result<Option<SessionCheckpoint>> {
    let dir = checkpoints_dir(config).join(sanitize_path_component(session_id));
    if !dir.is_dir() {
        return Ok(None);
    }

    let mut latest: Option<(u64, SessionCheckpoint)> = None;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".checkpoint.json") {
            continue;
        }
        let json = std::fs::read_to_string(entry.path())?;
        if let Some(checkpoint) = verify_and_deserialize_checkpoint(config, &json) {
            let turn = checkpoint.turn_counter;
            match &latest {
                None => latest = Some((turn, checkpoint)),
                Some((prev_turn, _)) if turn > *prev_turn => {
                    latest = Some((turn, checkpoint));
                }
                _ => {}
            }
        }
    }
    Ok(latest.map(|(_, c)| c))
}

/// Strict variant of [`load_latest_checkpoint`] used on the resume path.
///
/// Unlike the tolerant `load_latest_checkpoint` (which silently skips a
/// tampered checkpoint file and falls back to a fresh start), this surfaces a
/// [`CheckpointIntegrityError`] when the **highest-turn** checkpoint file on
/// disk has a broken HMAC signature. Surfacing the tamper at resume is what
/// lets the gateway record a durable audit trail and cancel the bound approval
/// (#606) instead of silently abandoning the suspended turn.
///
/// Returns `Ok(None)` only when no checkpoint files exist for the session.
pub fn load_latest_checkpoint_strict(
    config: &GatewayConfig,
    session_id: &str,
) -> anyhow::Result<Option<SessionCheckpoint>> {
    let dir = checkpoints_dir(config).join(sanitize_path_component(session_id));
    if !dir.is_dir() {
        return Ok(None);
    }

    // Order by turn parsed from the *filename* (untrusted payloads cannot be
    // used to order once tampering is in play) so the active/latest checkpoint
    // is checked first.
    let mut entries: Vec<(u64, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_suffix(".checkpoint.json") else {
            continue;
        };
        let turn = turn_number_from_id(stem).unwrap_or(0);
        entries.push((turn, entry.path()));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));

    for (turn, path) in &entries {
        let json = match std::fs::read_to_string(path) {
            Ok(j) => j,
            Err(_) => continue,
        };
        // Signed envelope: detect tamper before trusting the payload.
        if let Ok(envelope) = serde_json::from_str::<SignedCheckpoint>(&json) {
            let key = checkpoint_hmac_key(config);
            if !crate::server::ofp::hmac_verify(
                &key,
                envelope.payload_json.as_bytes(),
                &envelope.hmac_hex,
            ) {
                return Err(CheckpointIntegrityError {
                    session_id: session_id.to_string(),
                    turn_id: turn_id_for(*turn),
                    message: "HMAC mismatch on latest checkpoint".to_string(),
                }
                .into());
            }
            if let Ok(checkpoint) =
                serde_json::from_str::<SessionCheckpoint>(&envelope.payload_json)
            {
                return Ok(Some(checkpoint));
            }
            continue;
        }
        // Legacy unsigned format.
        if let Ok(checkpoint) = serde_json::from_str::<SessionCheckpoint>(&json) {
            return Ok(Some(checkpoint));
        }
    }
    Ok(None)
}

/// Mark the latest checkpoint as being inside a response-validation repair
/// cycle. The next respawn will restore the LoopGuard in repair mode so the
/// repair iterations do not count against `max_loops_without_progress`.
pub fn enter_repair_mode_on_latest_checkpoint(
    config: &GatewayConfig,
    session_id: &str,
    max_repair_loops: u32,
) -> anyhow::Result<()> {
    let Some(mut cp) = load_latest_checkpoint(config, session_id)? else {
        return Ok(());
    };
    cp.loop_guard_state.enter_repair_mode(max_repair_loops);
    save_checkpoint(config, &cp)
}

/// Reset the latest checkpoint's LoopGuard after a successful repair:
/// `current_loops` is cleared and repair mode is exited.
pub fn reset_after_successful_repair_on_latest_checkpoint(
    config: &GatewayConfig,
    session_id: &str,
) -> anyhow::Result<()> {
    let Some(mut cp) = load_latest_checkpoint(config, session_id)? else {
        return Ok(());
    };
    cp.loop_guard_state.reset_after_successful_repair();
    save_checkpoint(config, &cp)
}

/// Exit repair mode on the latest checkpoint without clearing `current_loops`,
/// used when repair fails or is exhausted.
pub fn exit_repair_mode_on_latest_checkpoint(
    config: &GatewayConfig,
    session_id: &str,
) -> anyhow::Result<()> {
    let Some(mut cp) = load_latest_checkpoint(config, session_id)? else {
        return Ok(());
    };
    cp.loop_guard_state.exit_repair_mode();
    save_checkpoint(config, &cp)
}

/// Append feedback events to the latest checkpoint for a session.
///
/// Used when feedback is issued outside the agent executor (e.g. response
/// validation violations) so that a later resume or retry can still detect
/// ignored feedback. The feedback is recorded against the checkpoint's own
/// turn counter. Returns `Ok(())` if there is no checkpoint to update.
pub fn append_feedback_to_latest_checkpoint(
    config: &GatewayConfig,
    session_id: &str,
    events: &[FeedbackEvent],
) -> anyhow::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let Some(mut cp) = load_latest_checkpoint(config, session_id)? else {
        return Ok(());
    };
    let turn_counter = cp.turn_counter;
    for event in events {
        cp.feedback_events.push((turn_counter, event.clone()));
    }
    save_checkpoint(config, &cp)
}

/// Delete all checkpoint files for a session.
pub fn cleanup_session_checkpoints(
    config: &GatewayConfig,
    session_id: &str,
) -> anyhow::Result<()> {
    let dir = checkpoints_dir(config).join(sanitize_path_component(session_id));
    if !dir.is_dir() {
        return Ok(());
    }
    let mut count = 0usize;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().ends_with(".checkpoint.json") {
            std::fs::remove_file(entry.path())?;
            count += 1;
        }
    }
    if count > 0 {
        tracing::debug!(
            target: "checkpoint",
            session_id = %session_id,
            count = count,
            "Cleaned up session checkpoints after completion"
        );
    }
    Ok(())
}

/// Delete a specific checkpoint file.
pub fn delete_checkpoint(
    config: &GatewayConfig,
    session_id: &str,
    turn_id: &str,
) -> anyhow::Result<()> {
    let path = checkpoint_path(config, session_id, turn_id);
    if path.exists() {
        std::fs::remove_file(&path)?;
        tracing::debug!(
            target: "checkpoint",
            session_id = %session_id,
            turn_id = %turn_id,
            "Deleted session checkpoint"
        );
    }
    Ok(())
}

/// Delete the checkpoint file(s) for a session whose `yield_reason` binds them
/// to `approval_id`. Called when an approval is rejected or cancelled so the
/// suspended turn's signed checkpoint file does not leak on disk (#607).
/// Returns the number of files deleted.
pub fn delete_approval_bound_checkpoint(
    config: &GatewayConfig,
    session_id: &str,
    approval_id: &str,
) -> anyhow::Result<usize> {
    let dir = checkpoints_dir(config).join(sanitize_path_component(session_id));
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut deleted = 0usize;
    for entry in std::fs::read_dir(&dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(target: "checkpoint", session_id = %session_id, error = %e, "skipping unreadable dir entry during reap");
                continue;
            }
        };
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with(".checkpoint.json") {
            continue;
        }
        let path = entry.path();
        let json = match std::fs::read_to_string(&path) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(target: "checkpoint", path = %path.display(), error = %e, "skipping unreadable checkpoint during reap");
                continue;
            }
        };
        let bound = verify_and_deserialize_checkpoint(config, &json)
            .map(|cp| {
                matches!(
                    cp.yield_reason,
                    YieldReason::ApprovalRequired { ref approval_request_id }
                        if approval_request_id == approval_id
                )
            })
            .unwrap_or(false);
        if bound && std::fs::remove_file(&path).is_ok() {
            deleted += 1;
        }
    }
    if deleted > 0 {
        tracing::debug!(
            target: "checkpoint",
            session_id = %session_id,
            approval_request_id = %approval_id,
            count = deleted,
            "Reaped orphan checkpoint(s) after reject/cancel"
        );
    }
    Ok(deleted)
}

/// Startup reaper: scan every session checkpoint directory and delete
/// checkpoint files whose bound approval is in a terminal state (rejected /
/// cancelled) or whose approval row no longer exists. Clears orphans left
/// behind by a crash or restart during reject/cancel (#607). Tampered or
/// unrecognizable files are left untouched (those are surfaced separately by
/// the integrity path). Returns the number of files reaped.
pub fn reap_orphan_checkpoints(
    config: &GatewayConfig,
    store: &crate::scheduler::gateway_store::GatewayStore,
) -> anyhow::Result<usize> {
    use autonoetic_types::background::ApprovalStatus;

    let root = checkpoints_dir(config);
    if !root.is_dir() {
        return Ok(0);
    }
    let mut reaped = 0usize;
    for session_entry in std::fs::read_dir(&root)? {
        let session_entry = session_entry?;
        if !session_entry.file_type()?.is_dir() {
            continue;
        }
        let dir = session_entry.path();
        for cp_entry in std::fs::read_dir(&dir)? {
            let cp_entry = cp_entry?;
            let name = cp_entry.file_name();
            if !name.to_string_lossy().ends_with(".checkpoint.json") {
                continue;
            }
            let path = cp_entry.path();
            let json = match std::fs::read_to_string(&path) {
                Ok(j) => j,
                Err(_) => continue,
            };
            let Some(cp) = verify_and_deserialize_checkpoint(config, &json) else {
                continue;
            };
            let YieldReason::ApprovalRequired {
                approval_request_id,
            } = cp.yield_reason
            else {
                continue;
            };
            let orphan = match store.get_approval(&approval_request_id) {
                Ok(Some(req)) => req
                    .status
                    .as_ref()
                    .map(|s| matches!(s, ApprovalStatus::Rejected | ApprovalStatus::Cancelled))
                    .unwrap_or(false),
                Ok(None) => true,
                Err(_) => false,
            };
            if orphan && std::fs::remove_file(&path).is_ok() {
                reaped += 1;
            }
        }
    }
    if reaped > 0 {
        tracing::info!(
            target: "checkpoint",
            count = reaped,
            "Startup reaper removed orphan checkpoint files"
        );
    }
    Ok(reaped)
}

/// Prune old checkpoints for a session, keeping the last N.
pub fn prune_checkpoints(
    config: &GatewayConfig,
    session_id: &str,
    keep_last: usize,
) -> anyhow::Result<()> {
    let dir = checkpoints_dir(config).join(sanitize_path_component(session_id));
    if !dir.is_dir() {
        return Ok(());
    }

    let mut checkpoints: Vec<(u64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".checkpoint.json") {
            continue;
        }
        let json = std::fs::read_to_string(&path)?;
        if let Some(checkpoint) = verify_and_deserialize_checkpoint(config, &json) {
            checkpoints.push((checkpoint.turn_counter, path));
        }
    }

    checkpoints.sort_by_key(|(turn, _)| std::cmp::Reverse(*turn));

    for (_, path) in checkpoints.into_iter().skip(keep_last) {
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!(
                target: "checkpoint",
                path = %path.display(),
                error = %e,
                "Failed to prune checkpoint"
            );
        }
    }

    Ok(())
}

/// List all checkpoint turn IDs for a session (sorted by turn number).
pub fn list_checkpoints(config: &GatewayConfig, session_id: &str) -> anyhow::Result<Vec<String>> {
    let dir = checkpoints_dir(config).join(sanitize_path_component(session_id));
    if !dir.is_dir() {
        return Ok(vec![]);
    }

    let mut checkpoints: Vec<(u64, String)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".checkpoint.json") {
            continue;
        }
        let json = std::fs::read_to_string(entry.path())?;
        if let Some(checkpoint) = verify_and_deserialize_checkpoint(config, &json) {
            checkpoints.push((checkpoint.turn_counter, checkpoint.turn_id));
        }
    }

    checkpoints.sort_by_key(|(turn, _)| *turn);
    Ok(checkpoints.into_iter().map(|(_, id)| id).collect())
}

/// Compute SHA-256 hash of runtime.lock content (if it exists).
pub fn compute_runtime_lock_hash(agent_dir: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let lock_path = agent_dir.join("runtime.lock");
    let content = std::fs::read(&lock_path).ok()?;
    let hash = Sha256::digest(&content);
    Some(format!("{:x}", hash))
}

// ---------------------------------------------------------------------------
// Session fork from checkpoint
// ---------------------------------------------------------------------------

/// Fork a session from a checkpoint.
///
/// Replaces the old `SessionSnapshot`-based fork. The checkpoint already contains
/// full conversation history, so forking reads from the checkpoint file.
#[derive(Debug)]
pub struct SessionFork {
    /// New session ID.
    pub new_session_id: String,
    /// Source session ID.
    pub source_session_id: String,
    /// Fork turn number.
    pub fork_turn: usize,
    /// Content handle of the copied history.
    pub history_handle: String,
    /// Initial history for the forked session (including branch message if any).
    pub initial_history: Vec<Message>,
    /// Agent the source checkpoint was running — the truthful attribution
    /// fallback when the caller doesn't name an acting agent explicitly.
    pub agent_id: String,
}

impl SessionFork {
    /// Creates a new session by forking from the latest checkpoint of a source session.
    pub fn fork(
        config: &GatewayConfig,
        source_session_id: &str,
        new_session_id: Option<&str>,
        branch_message: Option<&str>,
    ) -> anyhow::Result<Self> {
        let checkpoint = load_latest_checkpoint(config, source_session_id)?.ok_or_else(|| {
            anyhow::anyhow!(
                "No checkpoint found for session '{}'. Cannot fork without a checkpoint.",
                source_session_id
            )
        })?;
        Self::fork_from_checkpoint(config, &checkpoint, new_session_id, branch_message)
    }

    /// Creates a new session by forking from a specific checkpoint.
    pub fn fork_from_checkpoint(
        config: &GatewayConfig,
        checkpoint: &SessionCheckpoint,
        new_session_id: Option<&str>,
        branch_message: Option<&str>,
    ) -> anyhow::Result<Self> {
        let gw_dir = config.agents_dir.join(".gateway");
        let store = crate::runtime::content_store::ContentStore::new(&gw_dir)?;

        let new_session_id = new_session_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| autonoetic_types::id_format::short_random_id("fork-"));

        // Build history from checkpoint
        let mut history = checkpoint.history.clone();

        // Add branch message if provided
        if let Some(msg_text) = branch_message {
            history.push(crate::llm::Message::user(msg_text));
        }

        // Copy history to new session (consumed by chat/trace history display).
        let history_json = serde_json::to_string(&history)?;
        let history_handle = store.write(history_json.as_bytes())?;
        store.register_name(&new_session_id, "session_history", &history_handle)?;

        // Write a checkpoint under the new session id so the fork is actually
        // *runnable*. The execution engine seeds a session's LLM context from
        // its latest checkpoint (not from the `session_history` content name —
        // that only feeds the UI), so without this the agent would resume the
        // branch from a blank history and the fork point would be lost.
        //
        // We mark it `Hibernation` (a normal, auto-resumable yield point) and
        // strip any pending-tool / approval state inherited from the source
        // checkpoint, so the next message to the forked session resumes cleanly
        // with the full branch-point context (plus the branch message, if any).
        let forked_checkpoint = SessionCheckpoint {
            // NOTE: do *not* set `egress_labels` here — it must be inherited
            // from `..checkpoint.clone()` below so the fork carries the source
            // session's label sidecar (RFC data-envelopes §3.4). Overriding it
            // to `Default::default()` would silently let the forked session
            // ship previously-withheld content to a remote provider.
            history: history.clone(),
            session_id: new_session_id.clone(),
            yield_reason: YieldReason::Hibernation,
            pending_tool_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
            ..checkpoint.clone()
        };
        save_checkpoint(config, &forked_checkpoint)?;

        Ok(SessionFork {
            new_session_id,
            source_session_id: checkpoint.session_id.clone(),
            fork_turn: checkpoint.turn_counter as usize,
            history_handle,
            initial_history: history,
            agent_id: checkpoint.agent_id.clone(),
        })
    }

    /// Returns the initial history for the forked session.
    pub fn initial_history(&self) -> &[Message] {
        &self.initial_history
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(temp: &tempfile::TempDir) -> GatewayConfig {
        GatewayConfig {
            agents_dir: temp.path().to_path_buf(),
            ..Default::default()
        }
    }

    #[test]
    fn test_save_and_load_checkpoint() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let config = test_config(&temp);

        let checkpoint = SessionCheckpoint {
            egress_labels: Default::default(),
            egress_ask: None,
            history: vec![Message::user("hello")],
            turn_counter: 1,
            loop_guard_state: LoopGuard {
                max_loops_without_progress: 10,
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
            blocked_state_event_emitted: false,
            extended_loaded: false,
            agent_id: "test-agent".to_string(),
            session_id: "session-123".to_string(),
            turn_id: "turn-001".to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::Hibernation,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 1,
            tool_invocations_consumed: 0,
            tokens_consumed: 100,
            estimated_cost_usd: 0.001,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };

        save_checkpoint(&config, &checkpoint).expect("should save");
        let loaded = load_checkpoint(&config, &checkpoint.session_id, &checkpoint.turn_id)
            .expect("should load");
        let loaded = loaded.expect("should have checkpoint");

        assert_eq!(loaded.session_id, checkpoint.session_id);
        assert_eq!(loaded.turn_counter, checkpoint.turn_counter);
        assert_eq!(loaded.history.len(), 1);
        assert_eq!(loaded.yield_reason, YieldReason::Hibernation);
    }

    /// Minimal checkpoint for the focused field-preservation tests below, so
    /// they don't each repeat the full literal. (The module still has other
    /// standalone `SessionCheckpoint` literals; consolidating them all behind a
    /// shared builder is tracked in #923 — this helper is just the start.)
    fn sample_checkpoint() -> SessionCheckpoint {
        SessionCheckpoint {
            egress_labels: Default::default(),
            egress_ask: None,
            history: vec![Message::user("hello")],
            turn_counter: 1,
            loop_guard_state: LoopGuard::default(),
            session_state: autonoetic_types::agent::SessionState::Normal,
            tool_tier_escalated: false,
            discovered_tools: Default::default(),
            blocked_state_event_emitted: false,
            extended_loaded: false,
            agent_id: "test-agent".to_string(),
            session_id: "session-egress".to_string(),
            turn_id: "turn-001".to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::Hibernation,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 1,
            tool_invocations_consumed: 0,
            tokens_consumed: 100,
            estimated_cost_usd: 0.001,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        }
    }

    #[test]
    fn checkpoint_preserves_egress_labels_across_save_load() {
        // RFC data-envelopes §3.4 / #907 acceptance bar: the egress label
        // sidecar must survive suspend/resume. A resumed session has to
        // withhold from a provider exactly what the live session would; without
        // persistence the map is dropped on save and the resumed session
        // silently ships previously-withheld content to a remote model.
        use autonoetic_types::egress::{EgressLabel, Sink};
        let temp = tempfile::tempdir().expect("tempdir should create");
        let config = test_config(&temp);

        let mut egress_labels = std::collections::HashMap::new();
        egress_labels.insert("tc_email_read_1".to_string(), EgressLabel::local_only());
        egress_labels.insert("tc_mailbox_2".to_string(), EgressLabel::no_remote_model());

        let checkpoint = SessionCheckpoint {
            egress_labels: egress_labels.clone(),
            egress_ask: None,
            ..sample_checkpoint()
        };

        save_checkpoint(&config, &checkpoint).expect("should save");
        let loaded = load_checkpoint(&config, &checkpoint.session_id, &checkpoint.turn_id)
            .expect("should load")
            .expect("should have checkpoint");

        assert_eq!(
            loaded.egress_labels, egress_labels,
            "egress label sidecar must survive save/load intact"
        );
        // The restored local_only label must still exclude the remote model —
        // the whole point is that a resumed turn keeps withholding.
        let restored = loaded
            .egress_labels
            .get("tc_email_read_1")
            .expect("labeled tool result must survive");
        assert!(restored.allows(Sink::LocalModel));
        assert!(!restored.allows(Sink::RemoteModel));
    }

    #[test]
    fn checkpoint_without_egress_labels_deserializes_empty() {
        // Backward compat: `#[serde(default, skip_serializing_if = empty)]`
        // means a clean checkpoint omits the key entirely, and a checkpoint
        // written before this field existed deserializes with an empty map
        // (not a hard error). Verify both the omission and the default.
        let cp = sample_checkpoint();
        assert!(cp.egress_labels.is_empty());
        // Assert key absence on the parsed object, not via substring search —
        // "egress_labels" could otherwise appear inside serialized message text.
        let value: serde_json::Value = serde_json::to_value(&cp).expect("serialize");
        assert!(
            value.get("egress_labels").is_none(),
            "empty sidecar must be omitted from the wire form (skip_serializing_if)"
        );
        let back: SessionCheckpoint = serde_json::from_value(value).expect("deserialize");
        assert!(
            back.egress_labels.is_empty(),
            "a checkpoint lacking the field must default to an empty sidecar"
        );
    }

    #[test]
    fn fork_from_checkpoint_inherits_egress_labels() {
        // Regression guard: `fork_from_checkpoint` must inherit the source
        // session's label sidecar (via `..checkpoint.clone()`), not reset it.
        // A fork that drops labels would let the branched session ship
        // previously-withheld content to a remote provider (RFC §3.4).
        use autonoetic_types::egress::{EgressLabel, Sink};
        let temp = tempfile::tempdir().expect("tempdir should create");
        let config = test_config(&temp);

        let mut egress_labels = std::collections::HashMap::new();
        egress_labels.insert("tc_email_read_1".to_string(), EgressLabel::local_only());
        let source = SessionCheckpoint {
            egress_labels: egress_labels.clone(),
            egress_ask: None,
            ..sample_checkpoint()
        };

        let fork = SessionFork::fork_from_checkpoint(&config, &source, Some("forked-sess"), None)
            .expect("fork should succeed");

        let forked_cp = load_checkpoint(&config, &fork.new_session_id, &source.turn_id)
            .expect("should load forked checkpoint")
            .expect("forked checkpoint must exist");
        assert_eq!(
            forked_cp.egress_labels, egress_labels,
            "fork must inherit the source session's egress label sidecar"
        );
        assert!(!forked_cp
            .egress_labels
            .get("tc_email_read_1")
            .expect("labeled result must survive the fork")
            .allows(Sink::RemoteModel));
    }

    #[test]
    fn test_checkpoint_round_trips_capsule_state() {
        // The capsule must survive save/load so that on resume the governor
        // evolves it incrementally instead of re-bootstrapping from an empty
        // shell (regression guard for the prior-capsule-reuse wiring).
        let temp = tempfile::tempdir().expect("tempdir should create");
        let config = test_config(&temp);

        use crate::runtime::context_governor::capsule::{CapsuleDecision, StateCapsule};
        let capsule = StateCapsule {
            version: 4,
            session_id: "session-123".to_string(),
            last_update_turn: 9,
            objective_and_criteria: "Accumulated objective".to_string(),
            decisions_and_rationale: vec![CapsuleDecision {
                turn: 2,
                summary: "decided".into(),
                rationale: "because".into(),
                referenced_ids: vec![],
            }],
            stable_identifiers: vec![],
            open_tasks: vec![],
            prior_decisions_summary: None,
            previous_version_handle: Some("sha-prev".into()),
            source_history_handle: Some("sha-self".into()),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        };

        let checkpoint = SessionCheckpoint {
            egress_labels: Default::default(),
            egress_ask: None,
            history: vec![Message::user("hello")],
            turn_counter: 1,
            loop_guard_state: LoopGuard::default(),
            session_state: autonoetic_types::agent::SessionState::Normal,
            tool_tier_escalated: false,
            discovered_tools: Default::default(),
            blocked_state_event_emitted: false,
            extended_loaded: false,
            agent_id: "test-agent".to_string(),
            session_id: "session-123".to_string(),
            turn_id: "turn-001".to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::Hibernation,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 1,
            tool_invocations_consumed: 0,
            tokens_consumed: 100,
            estimated_cost_usd: 0.001,
            compression_metadata: None,
            capsule_state: Some(capsule),
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };

        save_checkpoint(&config, &checkpoint).expect("should save");
        let loaded = load_checkpoint(&config, &checkpoint.session_id, &checkpoint.turn_id)
            .expect("should load")
            .expect("should have checkpoint");

        let loaded_capsule = loaded
            .capsule_state
            .as_ref()
            .expect("capsule_state should round-trip");
        assert_eq!(loaded_capsule.version, 4);
        assert_eq!(loaded_capsule.objective_and_criteria, "Accumulated objective");
        assert_eq!(loaded_capsule.previous_version_handle.as_deref(), Some("sha-prev"));
        assert_eq!(loaded_capsule.source_history_handle.as_deref(), Some("sha-self"));
    }

    #[test]
    fn test_checkpoint_round_trips_extended_loaded_flag() {
        // #1015: the extended-instructions loaded flag must survive save/load.
        // A session that already mechanically loaded extended on its first
        // tool call must not re-inject the gateway_note after a resume, and an
        // un-loaded one must not inline extended before its first tool call.
        let temp = tempfile::tempdir().expect("tempdir should create");
        let config = test_config(&temp);

        for expected in [false, true] {
            let checkpoint = SessionCheckpoint {
                egress_labels: Default::default(),
                egress_ask: None,
                history: vec![Message::user("hello")],
                turn_counter: 1,
                loop_guard_state: LoopGuard::default(),
                session_state: autonoetic_types::agent::SessionState::Normal,
                tool_tier_escalated: false,
                discovered_tools: Default::default(),
                blocked_state_event_emitted: false,
                extended_loaded: expected,
                agent_id: "test-agent".to_string(),
                session_id: format!("session-ext-{expected}"),
                turn_id: "turn-001".to_string(),
                workflow_id: None,
                task_id: None,
                runtime_lock_hash: None,
                constitution_version: None,
                constitution_digest: None,
                llm_config_snapshot: None,
                tool_registry_version: None,
                yield_reason: YieldReason::Hibernation,
                content_store_refs: vec![],
                created_at: "2024-01-01T00:00:00Z".to_string(),
                pending_tool_state: None,
                llm_rounds_consumed: 1,
                tool_invocations_consumed: 0,
                tokens_consumed: 100,
                estimated_cost_usd: 0.001,
                compression_metadata: None,
                capsule_state: None,
                assistant_message: None,
                pending_action: None,
                suspended_at: None,
                suppress_until_turn: 0,
                trajectory_last_level: None,
                feedback_events: vec![],
            };
            save_checkpoint(&config, &checkpoint).expect("should save");
            let loaded = load_checkpoint(&config, &checkpoint.session_id, &checkpoint.turn_id)
                .expect("should load")
                .expect("should have checkpoint");
            assert_eq!(loaded.extended_loaded, expected);
        }

        // Legacy checkpoints (predating the field) deserialize as unloaded.
        let legacy = SessionCheckpoint {
            egress_labels: Default::default(),
            egress_ask: None,
            history: vec![Message::user("hello")],
            turn_counter: 1,
            loop_guard_state: LoopGuard::default(),
            session_state: autonoetic_types::agent::SessionState::Normal,
            tool_tier_escalated: false,
            discovered_tools: Default::default(),
            blocked_state_event_emitted: false,
            extended_loaded: true,
            agent_id: "test-agent".to_string(),
            session_id: "session-legacy".to_string(),
            turn_id: "turn-001".to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::Hibernation,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 1,
            tool_invocations_consumed: 0,
            tokens_consumed: 100,
            estimated_cost_usd: 0.001,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };
        // Serialize with the field present, then strip it to simulate an old
        // checkpoint file (serde default kicks in on deserialize).
        let json = serde_json::to_string(&legacy).expect("serialize");
        let stripped: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let obj = stripped.as_object().expect("object");
        let without_field = {
            let mut o = obj.clone();
            o.remove("extended_loaded");
            serde_json::Value::Object(o)
        };
        let deserialized: SessionCheckpoint =
            serde_json::from_value(without_field).expect("legacy checkpoint deserializes");
        assert!(!deserialized.extended_loaded);
    }

    #[test]
    fn test_load_latest_checkpoint() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let config = test_config(&temp);
        let session_id = "session-456";

        let c1 = SessionCheckpoint {
            egress_labels: Default::default(),
            egress_ask: None,
            history: vec![],
            turn_counter: 1,
            loop_guard_state: LoopGuard {
                max_loops_without_progress: 10,
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
            yield_reason: YieldReason::Hibernation,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 1,
            tool_invocations_consumed: 0,
            tokens_consumed: 100,
            estimated_cost_usd: 0.001,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };

        let mut c2 = c1.clone();
        c2.turn_counter = 2;
        c2.turn_id = "turn-002".to_string();

        let mut c3 = c1.clone();
        c3.turn_counter = 3;
        c3.turn_id = "turn-003".to_string();

        save_checkpoint(&config, &c1).unwrap();
        save_checkpoint(&config, &c2).unwrap();
        save_checkpoint(&config, &c3).unwrap();

        let latest = load_latest_checkpoint(&config, session_id)
            .expect("should load")
            .expect("should have checkpoint");
        assert_eq!(latest.turn_counter, 3);
    }

    #[test]
    fn test_prune_checkpoints() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let config = test_config(&temp);
        let session_id = "session-789";

        for i in 1..=5 {
            let checkpoint = SessionCheckpoint {
                egress_labels: Default::default(),
                egress_ask: None,
                history: vec![],
                turn_counter: i,
                loop_guard_state: LoopGuard {
                    max_loops_without_progress: 10,
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
                blocked_state_event_emitted: false,
                extended_loaded: false,
                agent_id: "test-agent".to_string(),
                session_id: session_id.to_string(),
                turn_id: format!("turn-{:03}", i),
                workflow_id: None,
                task_id: None,
                runtime_lock_hash: None,
                constitution_version: None,
                constitution_digest: None,
                llm_config_snapshot: None,
                tool_registry_version: None,
                yield_reason: YieldReason::Hibernation,
                content_store_refs: vec![],
                created_at: "2024-01-01T00:00:00Z".to_string(),
                pending_tool_state: None,
                llm_rounds_consumed: i,
                tool_invocations_consumed: 0,
                tokens_consumed: 100,
                estimated_cost_usd: 0.001,
                compression_metadata: None,
                capsule_state: None,
                assistant_message: None,
                pending_action: None,
                suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
            };
            save_checkpoint(&config, &checkpoint).unwrap();
        }

        prune_checkpoints(&config, session_id, 3).unwrap();

        let remaining = list_checkpoints(&config, session_id).unwrap();
        assert_eq!(remaining.len(), 3);
        assert!(remaining.contains(&"turn-003".to_string()));
        assert!(remaining.contains(&"turn-004".to_string()));
        assert!(remaining.contains(&"turn-005".to_string()));
    }

    #[test]
    fn test_yield_reason_serialization() {
        let reasons = vec![
            YieldReason::Hibernation,
            YieldReason::BudgetExhausted,
            YieldReason::ApprovalRequired {
                approval_request_id: "apr-123".to_string(),
            },
            YieldReason::UserInputRequired {
                interaction_id: "ui-456".to_string(),
            },
            YieldReason::WaitingForChild {
                workflow_id: "wf-789".to_string(),
                task_id: Some("task-123".to_string()),
            },
            YieldReason::EmergencyStop {
                stop_id: "estop-789".to_string(),
            },
            YieldReason::MaxTurnsReached,
            YieldReason::ManualStop,
            YieldReason::Error("something went wrong".to_string()),
            YieldReason::HumanEscalation {
                escalation_request_id: "esc-001".to_string(),
            },
            YieldReason::ParentTerminated {
                parent_session_id: "root/parent-abc".to_string(),
                reason: "emergency_stop".to_string(),
            },
        ];

        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let decoded: YieldReason = serde_json::from_str(&json).unwrap();
            assert_eq!(reason, decoded);
        }
    }

    #[test]
    fn test_checkpoint_hmac_tamper_rejection() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let config = test_config(&temp);
        let session_id = "session-tamper";
        let turn_id = "turn-001";

        // Build and save a checkpoint.
        let checkpoint = SessionCheckpoint {
            egress_labels: Default::default(),
            egress_ask: None,
            history: vec![Message::user("hello")],
            turn_counter: 1,
            loop_guard_state: LoopGuard::default(),
            session_state: autonoetic_types::agent::SessionState::Normal,
            tool_tier_escalated: false,
            discovered_tools: Default::default(),
            blocked_state_event_emitted: false,
            extended_loaded: false,
            agent_id: "test-agent".to_string(),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::Hibernation,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 1,
            tool_invocations_consumed: 0,
            tokens_consumed: 100,
            estimated_cost_usd: 0.001,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };
        save_checkpoint(&config, &checkpoint).expect("should save");

        // --- load_checkpoint rejects tampered payload ---
        let path = checkpoint_path(&config, session_id, turn_id);
        let original = std::fs::read_to_string(&path).unwrap();
        let mut envelope: serde_json::Value =
            serde_json::from_str(&original).expect("saved as signed envelope");
        // Tamper: replace hmac_hex with garbage so verification fails.
        envelope["hmac_hex"] = serde_json::json!("deadbeef00".repeat(8));
        std::fs::write(&path, serde_json::to_string(&envelope).unwrap())
            .expect("write tampered file");

        let result = load_checkpoint(&config, session_id, turn_id);
        assert!(
            result.is_err(),
            "tampered checkpoint should be rejected by load_checkpoint"
        );
        assert!(
            is_integrity_error(&result.unwrap_err()),
            "error should be CheckpointIntegrityError"
        );

        // --- load_latest_checkpoint skips tampered file ---
        let latest = load_latest_checkpoint(&config, session_id).unwrap();
        assert!(
            latest.is_none(),
            "tampered checkpoint should be skipped by load_latest_checkpoint"
        );

        // --- load_latest_checkpoint_strict surfaces the tamper (#606) ---
        let strict_err = load_latest_checkpoint_strict(&config, session_id)
            .expect_err("strict loader should surface a tampered latest checkpoint");
        assert!(
            is_integrity_error(&strict_err),
            "strict loader error should be a CheckpointIntegrityError: {:?}",
            strict_err
        );
    }

    #[test]
    fn load_latest_checkpoint_strict_loads_valid_latest() {
        let temp = tempfile::tempdir().unwrap();
        let config = test_config(&temp);
        let session_id = "session-strict-valid";

        // Save two valid checkpoints; the strict loader must return the latest.
        for turn in [1u64, 2] {
            let checkpoint = SessionCheckpoint {
                egress_labels: Default::default(),
                egress_ask: None,
                history: vec![Message::user("hello")],
                turn_counter: turn,
                loop_guard_state: LoopGuard::default(),
                session_state: autonoetic_types::agent::SessionState::Normal,
                tool_tier_escalated: false,
                discovered_tools: Default::default(),
                blocked_state_event_emitted: false,
                extended_loaded: false,
                agent_id: "test-agent".to_string(),
                session_id: session_id.to_string(),
                turn_id: turn_id_for(turn),
                workflow_id: None,
                task_id: None,
                runtime_lock_hash: None,
                constitution_version: None,
                constitution_digest: None,
                llm_config_snapshot: None,
                tool_registry_version: None,
                yield_reason: YieldReason::Hibernation,
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
            save_checkpoint(&config, &checkpoint).unwrap();
        }

        let latest = load_latest_checkpoint_strict(&config, session_id)
            .expect("valid checkpoints should load")
            .expect("a checkpoint should exist");
        assert_eq!(latest.turn_counter, 2, "strict loader returns the latest turn");

        // No checkpoints at all -> Ok(None).
        assert!(
            load_latest_checkpoint_strict(&config, "no-such-session")
                .unwrap()
                .is_none(),
            "strict loader returns Ok(None) when no checkpoint files exist"
        );
    }

    #[test]
    fn append_feedback_to_latest_checkpoint_round_trips() {
        use autonoetic_types::trajectory::FeedbackEvent;

        let temp = tempfile::tempdir().unwrap();
        let config = test_config(&temp);
        let session_id = "session-feedback";
        let turn_id = "turn-002";

        let checkpoint = SessionCheckpoint {
            egress_labels: Default::default(),
            egress_ask: None,
            history: vec![Message::user("hello")],
            turn_counter: 2,
            loop_guard_state: LoopGuard::default(),
            session_state: autonoetic_types::agent::SessionState::Normal,
            tool_tier_escalated: false,
            discovered_tools: Default::default(),
            blocked_state_event_emitted: false,
            extended_loaded: false,
            agent_id: "test-agent".to_string(),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::Hibernation,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 1,
            tool_invocations_consumed: 0,
            tokens_consumed: 100,
            estimated_cost_usd: 0.001,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };
        save_checkpoint(&config, &checkpoint).expect("should save");

        let events = vec![
            FeedbackEvent::Validation {
                rule: "required_artifacts".into(),
                field_path: None,
            },
            FeedbackEvent::Validation {
                rule: "output_schema".into(),
                field_path: None,
            },
        ];
        append_feedback_to_latest_checkpoint(&config, session_id, &events)
            .expect("should append feedback");

        let latest = load_latest_checkpoint(&config, session_id)
            .expect("should load")
            .expect("checkpoint should exist");
        assert_eq!(latest.feedback_events.len(), 2);
        assert_eq!(latest.feedback_events[0].0, 2);
        assert_eq!(
            latest.feedback_events[0].1,
            FeedbackEvent::Validation {
                rule: "required_artifacts".into(),
                field_path: None,
            }
        );
    }

    #[test]
    fn repair_mode_checkpoint_helpers_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let config = test_config(&temp);
        let session_id = "session-repair";
        let turn_id = "turn-003";

        let mut guard = LoopGuard::new(5);
        guard.current_loops = 3;
        let checkpoint = SessionCheckpoint {
            egress_labels: Default::default(),
            egress_ask: None,
            history: vec![Message::user("hello")],
            turn_counter: 3,
            loop_guard_state: guard,
            session_state: autonoetic_types::agent::SessionState::Normal,
            tool_tier_escalated: false,
            discovered_tools: Default::default(),
            blocked_state_event_emitted: false,
            extended_loaded: false,
            agent_id: "test-agent".to_string(),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::Hibernation,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 1,
            tool_invocations_consumed: 0,
            tokens_consumed: 100,
            estimated_cost_usd: 0.001,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };
        save_checkpoint(&config, &checkpoint).expect("should save");

        enter_repair_mode_on_latest_checkpoint(&config, session_id, 10).expect("enter repair mode");
        let latest = load_latest_checkpoint(&config, session_id)
            .expect("should load")
            .expect("checkpoint should exist");
        assert!(latest.loop_guard_state.repair_mode);
        assert_eq!(latest.loop_guard_state.repair_loops, 0);
        assert_eq!(latest.loop_guard_state.max_repair_loops, 10);
        assert_eq!(latest.loop_guard_state.current_loops, 3, "outer loops preserved");

        reset_after_successful_repair_on_latest_checkpoint(&config, session_id)
            .expect("reset after repair");
        let latest = load_latest_checkpoint(&config, session_id)
            .expect("should load")
            .expect("checkpoint should exist");
        assert!(!latest.loop_guard_state.repair_mode);
        assert_eq!(latest.loop_guard_state.current_loops, 0, "outer loops cleared on success");
    }

    /// #821 backward compat: a checkpoint JSON blob written before
    /// `constitution_version`/`constitution_digest` existed (simulated here
    /// by stripping the keys post-serialization) must still deserialize,
    /// with the new fields defaulting to `None` instead of failing.
    #[test]
    fn checkpoint_without_constitution_pin_fields_deserializes_as_none() {
        let checkpoint = SessionCheckpoint {
            egress_labels: Default::default(),
            egress_ask: None,
            history: vec![Message::user("hello")],
            turn_counter: 1,
            loop_guard_state: LoopGuard::default(),
            session_state: autonoetic_types::agent::SessionState::Normal,
            tool_tier_escalated: false,
            discovered_tools: Default::default(),
            blocked_state_event_emitted: false,
            extended_loaded: false,
            agent_id: "test-agent".to_string(),
            session_id: "session-legacy".to_string(),
            turn_id: "turn-001".to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: Some("2026.06.05".to_string()),
            constitution_digest: Some("deadbeef".to_string()),
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::Hibernation,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 1,
            tool_invocations_consumed: 0,
            tokens_consumed: 100,
            estimated_cost_usd: 0.001,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };

        let mut json = serde_json::to_value(&checkpoint).expect("serialize");
        // Simulate a pre-#821 checkpoint on disk: strip the new fields entirely.
        let obj = json.as_object_mut().expect("checkpoint serializes to an object");
        obj.remove("constitution_version");
        obj.remove("constitution_digest");

        let restored: SessionCheckpoint = serde_json::from_value(json)
            .expect("legacy checkpoint JSON (missing constitution pin fields) should still deserialize");
        assert_eq!(restored.constitution_version, None);
        assert_eq!(restored.constitution_digest, None);
        // Sanity: everything else round-tripped normally.
        assert_eq!(restored.session_id, "session-legacy");
    }
}
