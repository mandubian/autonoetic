//! Session Checkpoint: universal execution snapshots at all yield points.
//!
//! Generalizes the approval-specific `TurnContinuation` into a universal snapshot
//! that can restore an agent session from hibernation, budget exhaustion, max turns,
//! crash recovery, and approval suspension.
//!
//! Storage: `.gateway/checkpoints/{session_id}/{turn_id}.checkpoint.json`

use crate::llm::Message;
use crate::runtime::guard::LoopGuardState;
use autonoetic_types::config::GatewayConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Why execution stopped and a checkpoint was saved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum YieldReason {
    /// Agent paused between turns (EndTurn / StopSequence).
    Hibernation,
    /// Session budget depleted mid-execution.
    BudgetExhausted,
    /// Approval gate (overlaps TurnContinuation).
    ApprovalRequired { approval_request_id: String },
    /// Explicit question / choice for the human.
    UserInputRequired { interaction_id: String },
    /// Operator circuit breaker; do not auto-resume.
    EmergencyStop { stop_id: String },
    /// Loop guard limit reached.
    MaxTurnsReached,
    /// Operator/user interrupt.
    ManualStop,
    /// Recoverable error.
    Error(String),
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
        }
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
    pub loop_guard_state: LoopGuardState,

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
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

/// Root directory for checkpoint files: `.gateway/checkpoints/`.
pub fn checkpoints_dir(config: &GatewayConfig) -> PathBuf {
    config.agents_dir.join(".gateway").join("checkpoints")
}

fn checkpoint_path(config: &GatewayConfig, session_id: &str, turn_id: &str) -> PathBuf {
    checkpoints_dir(config)
        .join(sanitize_path_component(session_id))
        .join(format!(
            "{}.checkpoint.json",
            sanitize_path_component(turn_id)
        ))
}

fn sanitize_path_component(s: &str) -> String {
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

/// Persist a `SessionCheckpoint` for the given session and turn.
pub fn save_checkpoint(
    config: &GatewayConfig,
    checkpoint: &SessionCheckpoint,
) -> anyhow::Result<()> {
    let dir = checkpoints_dir(config).join(sanitize_path_component(&checkpoint.session_id));
    std::fs::create_dir_all(&dir)?;
    let path = checkpoint_path(config, &checkpoint.session_id, &checkpoint.turn_id);
    let json = serde_json::to_string_pretty(checkpoint)?;
    std::fs::write(&path, json)?;
    tracing::debug!(
        target: "checkpoint",
        session_id = %checkpoint.session_id,
        turn_id = %checkpoint.turn_id,
        yield_reason = ?checkpoint.yield_reason,
        path = %path.display(),
        "Saved session checkpoint"
    );
    Ok(())
}

/// Load a specific checkpoint by session and turn ID.
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
    let checkpoint: SessionCheckpoint = serde_json::from_str(&json)?;
    Ok(Some(checkpoint))
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
        if let Ok(checkpoint) = serde_json::from_str::<SessionCheckpoint>(&json) {
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
        if let Ok(checkpoint) = serde_json::from_str::<SessionCheckpoint>(&json) {
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
        if let Ok(checkpoint) = serde_json::from_str::<SessionCheckpoint>(&json) {
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
            .unwrap_or_else(|| format!("fork-{}", &uuid::Uuid::new_v4().to_string()[..8]));

        // Build history from checkpoint
        let mut history = checkpoint.history.clone();

        // Add branch message if provided
        if let Some(msg_text) = branch_message {
            history.push(crate::llm::Message::user(msg_text));
        }

        // Copy history to new session
        let history_json = serde_json::to_string(&history)?;
        let history_handle = store.write(history_json.as_bytes())?;
        store.register_name(&new_session_id, "session_history", &history_handle)?;

        Ok(SessionFork {
            new_session_id,
            source_session_id: checkpoint.session_id.clone(),
            fork_turn: checkpoint.turn_counter as usize,
            history_handle,
            initial_history: history,
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
            history: vec![Message::user("hello")],
            turn_counter: 1,
            loop_guard_state: LoopGuardState {
                max_loops_without_progress: 10,
                current_loops: 0,
                last_failure_hash: None,
                consecutive_failures: 0,
            },
            agent_id: "test-agent".to_string(),
            session_id: "session-123".to_string(),
            turn_id: "turn-001".to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
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

    #[test]
    fn test_load_latest_checkpoint() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let config = test_config(&temp);
        let session_id = "session-456";

        let c1 = SessionCheckpoint {
            history: vec![],
            turn_counter: 1,
            loop_guard_state: LoopGuardState {
                max_loops_without_progress: 10,
                current_loops: 0,
                last_failure_hash: None,
                consecutive_failures: 0,
            },
            agent_id: "test-agent".to_string(),
            session_id: session_id.to_string(),
            turn_id: "turn-001".to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
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
                history: vec![],
                turn_counter: i,
                loop_guard_state: LoopGuardState {
                    max_loops_without_progress: 10,
                    current_loops: 0,
                    last_failure_hash: None,
                    consecutive_failures: 0,
                },
                agent_id: "test-agent".to_string(),
                session_id: session_id.to_string(),
                turn_id: format!("turn-{:03}", i),
                workflow_id: None,
                task_id: None,
                runtime_lock_hash: None,
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
            YieldReason::EmergencyStop {
                stop_id: "estop-789".to_string(),
            },
            YieldReason::MaxTurnsReached,
            YieldReason::ManualStop,
            YieldReason::Error("something went wrong".to_string()),
        ];

        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let decoded: YieldReason = serde_json::from_str(&json).unwrap();
            assert_eq!(reason, decoded);
        }
    }
}
