//! Turn Continuation: checkpoint-and-resume for approval-suspended agent turns.
//!
//! When a tool call requires operator approval, the turn is **checkpointed** to disk
//! and the tokio task is released. When the approval is resolved the scheduler
//! re-queues the task; `spawn_task_execution` loads the continuation, executes the
//! approved action directly, reconstructs the conversation history, and resumes
//! `execute_with_history` — the LLM sees the real tool result and continues normally.
//!
//! # Integrity
//!
//! Continuation files are HMAC-SHA256 signed using a per-gateway key derived from
//! `GatewayConfig::continuation_key` (or `node_id` as fallback). On load, the
//! signature is verified before the payload is deserialized. Tampered files are
//! rejected, a causal event is emitted, and the bound approval is cancelled.

use crate::llm::{Message, ToolCall};
use crate::runtime::guard::LoopGuardState;
use crate::server::ofp;
use autonoetic_types::background::{ApprovalDecision, ScheduledAction};
use autonoetic_types::config::GatewayConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Error returned when a continuation file fails HMAC integrity verification.
/// Used to distinguish tamper-detection errors from ordinary I/O or parse
/// failures, so callers can emit the appropriate causal event and cancel the
/// bound approval.
#[derive(Debug)]
pub struct ContinuationIntegrityError {
    pub task_id: String,
    pub message: String,
}

impl std::fmt::Display for ContinuationIntegrityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "continuation integrity violation for task '{}': {}",
            self.task_id, self.message
        )
    }
}

impl std::error::Error for ContinuationIntegrityError {}

/// Returns `true` if the error is a `ContinuationIntegrityError` (HMAC
/// mismatch / tamper detection).  Used by callers to decide whether to cancel
/// approvals and emit a tamper causal event.
pub fn is_integrity_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ContinuationIntegrityError>().is_some()
}

/// HMAC-signed envelope wrapping a serialised `TurnContinuation` payload.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SignedContinuation {
    /// Canonical-JSON serialised `TurnContinuation`.
    pub payload_json: String,
    /// HMAC-SHA256 hex digest over `payload_json` bytes using the gateway key.
    pub hmac_hex: String,
}

/// Serializable snapshot of an agent turn that has been suspended at an
/// approval boundary.  Saved to disk; loaded on resume to seamlessly continue
/// the turn with the real tool result injected into history.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TurnContinuation {
    /// Full conversation history at the point of suspension (system + user +
    /// all prior assistant / tool exchanges **before** the suspended batch).
    pub history: Vec<Message>,

    /// The assistant message that contained the tool call(s) triggering approval.
    /// Re-appended to history on resume before the tool result messages.
    pub assistant_message: Message,

    /// Tool results already collected **before** the approval-requiring call
    /// within the same tool-use batch.  Injected as `tool_result` messages
    /// before the approval result on resume.
    pub completed_tool_results: Vec<(String, String, String)>, // (call_id, tool_name, result_json)

    /// The tool call that triggered the approval gate.
    pub pending_tool_call: PendingApprovalToolCall,

    /// Tool calls that were NOT executed because they came after the
    /// approval-requiring one.  Re-executed on resume after the approval
    /// result is injected.
    pub remaining_tool_calls: Vec<ToolCall>,

    /// Approval request ID stored in `GatewayStore`.
    pub approval_request_id: String,

    /// The `ScheduledAction` that is pending approval.  Stored so the gateway
    /// can verify structural equality against the approval row at resume,
    /// preventing TOCTOU substitution attacks.
    #[serde(default)]
    pub pending_action: Option<ScheduledAction>,

    /// Workflow / task context — populated by `spawn_task_execution`.
    pub workflow_id: Option<String>,
    pub task_id: Option<String>,

    /// Session and turn identifiers for correlation and tracing.
    pub session_id: String,
    pub turn_id: String,

    /// Wall-clock timestamp of suspension (RFC3339).  Used by the scheduler
    /// timeout checker to fail tasks that wait too long for approval.
    pub suspended_at: String,

    /// Loop guard state at suspension so the guard can be restored on resume
    /// without counting suspension time as wasted iterations.
    pub loop_guard_state: LoopGuardState,
}

/// The specific tool call that triggered the approval gate.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingApprovalToolCall {
    /// The LLM-assigned call ID for this invocation.
    pub call_id: String,
    /// Tool name (e.g. `"sandbox_exec"`, `"agent.install"`).
    pub tool_name: String,
    /// JSON-encoded arguments string as produced by the model.
    pub arguments: String,
    /// The raw `approval_required` JSON returned by the tool handler.
    pub approval_response: String,
}

// ---------------------------------------------------------------------------
// Key derivation
// ---------------------------------------------------------------------------

/// Resolve the HMAC key for continuation signing.  Uses the explicit
/// `continuation_key` config value when set, otherwise derives a key from
/// `node_id`.  **Warning:** the `node_id`-derived default is not a secret and
/// only provides detection of accidental corruption, not protection against a
/// local attacker who can read the config.  Production deployments should set
/// `continuation_key` to a high-entropy secret.
pub fn continuation_hmac_key(config: &GatewayConfig) -> String {
    config
        .continuation_key
        .clone()
        .unwrap_or_else(|| format!("autonoetic-continuation-{}", config.node_id))
}

// ---------------------------------------------------------------------------
// Canonical JSON helper
// ---------------------------------------------------------------------------

/// Produce a deterministic JSON representation.  `serde_json` with sorted keys
/// ensures the same struct always serialises to the same bytes.
fn canonical_json<T: serde::Serialize>(value: &T) -> anyhow::Result<String> {
    let mut buf = serde_json::Serializer::with_formatter(
        Vec::new(),
        serde_json::ser::CompactFormatter,
    );
    serde::Serialize::serialize(value, &mut buf)?;
    // Re-parse and re-serialize with sorted keys for determinism
    let v: serde_json::Value = serde_json::from_slice(&buf.into_inner())?;
    let sorted = serde_json::to_string(&v)?;
    Ok(sorted)
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

/// Root directory for continuation files: `.gateway/continuations/`.
pub fn continuations_dir(config: &GatewayConfig) -> PathBuf {
    config.agents_dir.join(".gateway").join("continuations")
}

pub fn continuation_path(config: &GatewayConfig, task_id: &str) -> PathBuf {
    continuations_dir(config).join(format!("{}.json", task_id))
}

/// Persist a `TurnContinuation` for the given task, wrapped in a signed
/// envelope.
pub fn save_continuation(
    config: &GatewayConfig,
    task_id: &str,
    cont: &TurnContinuation,
) -> anyhow::Result<()> {
    let dir = continuations_dir(config);
    std::fs::create_dir_all(&dir)?;
    let path = continuation_path(config, task_id);

    let payload_json = canonical_json(cont)?;
    let key = continuation_hmac_key(config);
    let hmac_hex = ofp::hmac_sign(&key, payload_json.as_bytes());

    let envelope = SignedContinuation {
        payload_json,
        hmac_hex,
    };
    let json = serde_json::to_string_pretty(&envelope)?;
    std::fs::write(&path, json)?;

    tracing::debug!(
        target: "continuation",
        task_id = %task_id,
        path = %path.display(),
        "Saved signed turn continuation"
    );
    Ok(())
}

/// Load a previously saved `TurnContinuation`, verifying HMAC integrity.
///
/// Returns `Ok(None)` if the file does not exist.
/// Returns `Err` on HMAC verification failure (tampering detected) or
/// deserialization errors.
pub fn load_continuation(
    config: &GatewayConfig,
    task_id: &str,
) -> anyhow::Result<Option<TurnContinuation>> {
    let path = continuation_path(config, task_id);
    if !path.exists() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(&path)?;

    // Try signed envelope format first.
    if let Ok(envelope) = serde_json::from_str::<SignedContinuation>(&json) {
        let key = continuation_hmac_key(config);
        if !ofp::hmac_verify(&key, envelope.payload_json.as_bytes(), &envelope.hmac_hex) {
            tracing::error!(
                target: "continuation",
                task_id = %task_id,
                "HMAC verification failed — continuation file may have been tampered with"
            );
            return Err(ContinuationIntegrityError {
                task_id: task_id.to_string(),
                message: "HMAC mismatch".to_string(),
            }.into());
        }
        let cont: TurnContinuation = serde_json::from_str(&envelope.payload_json)?;
        return Ok(Some(cont));
    }

    // Legacy unsigned format — still accepted for existing continuations
    // written before the signing feature was deployed.
    let cont: TurnContinuation = serde_json::from_str(&json)?;
    tracing::warn!(
        target: "continuation",
        task_id = %task_id,
        "Loaded unsigned (legacy) continuation — consider re-saving to sign"
    );
    Ok(Some(cont))
}

/// Delete the continuation file for a task (called on resume or cancellation).
pub fn delete_continuation(config: &GatewayConfig, task_id: &str) -> anyhow::Result<()> {
    let path = continuation_path(config, task_id);
    if path.exists() {
        std::fs::remove_file(&path)?;
        tracing::debug!(
            target: "continuation",
            task_id = %task_id,
            "Deleted turn continuation"
        );
    }
    Ok(())
}

/// Return the task IDs of all suspended continuations currently on disk.
pub fn list_suspended_task_ids(config: &GatewayConfig) -> anyhow::Result<Vec<String>> {
    let dir = continuations_dir(config);
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".json") {
            let task_id = name.trim_end_matches(".json").to_string();
            ids.push(task_id);
        }
    }
    Ok(ids)
}

/// Remove orphaned continuation files whose bound approval is in a terminal
/// state (approved/rejected/cancelled) or whose approval row no longer exists.
/// Returns the number of reaped files.
pub fn reap_orphaned_continuations(
    config: &GatewayConfig,
    store: &crate::scheduler::gateway_store::GatewayStore,
) -> anyhow::Result<u32> {
    let suspended = list_suspended_task_ids(config)?;
    if suspended.is_empty() {
        return Ok(0);
    }

    let mut reaped = 0u32;
    for task_id in &suspended {
        let status = store.get_approval_status_by_task_id(task_id)?;
        let is_orphan = match &status {
            None => true,
            Some(s) => s != "pending",
        };
        if is_orphan {
            tracing::info!(
                target: "continuation",
                task_id = %task_id,
                approval_status = ?status,
                "Reaping orphaned continuation file"
            );
            delete_continuation(config, task_id)?;
            reaped += 1;
        }
    }
    Ok(reaped)
}

// ---------------------------------------------------------------------------
// Approved-action execution
// ---------------------------------------------------------------------------

/// Execute the action that was approved, returning the tool result JSON the
/// agent would have received had there been no approval gate.
///
/// For `SandboxExec` this calls the `sandbox.exec` tool handler with
/// `approval_ref` already validated — the handler skips remote-access
/// detection and runs the sandbox directly.
///
/// `AgentInstall` approvals are legacy-only and are not executable in current runtime.
///
/// Any future `ScheduledAction` variant just needs a match arm here.
pub fn execute_approved_action(
    decision: &ApprovalDecision,
    manifest: &autonoetic_types::agent::AgentManifest,
    agent_dir: &Path,
    gateway_dir: Option<&Path>,
    session_id: Option<&str>,
    config: &GatewayConfig,
    gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
) -> anyhow::Result<String> {
    let registry = crate::runtime::tools::default_registry();
    let policy = crate::policy::PolicyEngine::new(manifest.clone());

    match &decision.action {
        ScheduledAction::SandboxExec {
            command,
            dependencies,
            ..
        } => {
            let deps_json = match dependencies {
                Some(d) => serde_json::json!({
                    "runtime": d.runtime,
                    "packages": d.packages,
                }),
                None => serde_json::Value::Null,
            };
            let args = if deps_json.is_null() {
                serde_json::json!({
                    "command": command,
                    "approval_ref": decision.request_id,
                })
            } else {
                serde_json::json!({
                    "command": command,
                    "dependencies": deps_json,
                    "approval_ref": decision.request_id,
                })
            };
            tracing::info!(
                target: "continuation",
                request_id = %decision.request_id,
                command = %command,
                "Executing approved sandbox.exec action"
            );
            registry.execute(
                "sandbox_exec",
                manifest,
                &policy,
                agent_dir,
                gateway_dir,
                &args.to_string(),
                session_id,
                None,
                Some(config),
                gateway_store,
                None,
            )
        }

        ScheduledAction::AgentInstall { agent_id, .. } => anyhow::bail!(
            "Legacy approval action 'AgentInstall' for '{}' cannot be executed: agent.install has been removed. Use revision create + promote workflows.",
            agent_id
        ),

        ScheduledAction::SessionContinue {
            session_id,
            turn_counter,
            max_turns,
            ..
        } => Ok(serde_json::json!({
            "ok": true,
            "approval_required": false,
            "continued": true,
            "session_id": session_id,
            "turn_counter": turn_counter,
            "max_turns": max_turns,
            "request_id": decision.request_id,
        })
        .to_string()),

        ScheduledAction::SessionEscalate { .. } => anyhow::bail!(
            "SessionEscalate approval is handled by injecting operator guidance into the conversation; no action execution needed"
        ),

        ScheduledAction::CredentialRequest {
            credential_id,
            url,
            method,
            headers,
            body,
            inject_secret_as,
            ..
        } => {
            let mut args = serde_json::Map::new();
            args.insert("credential_id".to_string(), serde_json::json!(credential_id));
            args.insert("url".to_string(), serde_json::json!(url));
            args.insert("approval_ref".to_string(), serde_json::json!(decision.request_id));
            if let Some(m) = method {
                args.insert("method".to_string(), serde_json::json!(m));
            }
            if let Some(h) = headers {
                args.insert("headers".to_string(), serde_json::json!(h));
            }
            if let Some(b) = body {
                args.insert("body".to_string(), b.clone());
            }
            if let Some(i) = inject_secret_as {
                args.insert("inject_secret_as".to_string(), serde_json::json!(i));
            }
            tracing::info!(
                target: "continuation",
                request_id = %decision.request_id,
                url = %url,
                "Executing approved credential.request action"
            );
            registry.execute(
                "credential_request",
                manifest,
                &policy,
                agent_dir,
                gateway_dir,
                &serde_json::Value::Object(args).to_string(),
                session_id,
                None,
                Some(config),
                gateway_store,
                None,
            )
        }

        other => {
            anyhow::bail!(
                "execute_approved_action: unsupported ScheduledAction variant {:?}",
                std::mem::discriminant(other)
            )
        }
    }
}

// ---------------------------------------------------------------------------
// History reconstruction
// ---------------------------------------------------------------------------

/// Reconstruct the conversation history from a `TurnContinuation` plus the
/// real tool result obtained after approval.  The returned history is ready
/// to be fed back into `execute_with_history` so the LLM can continue where
/// it left off.
pub fn reconstruct_history(
    cont: &TurnContinuation,
    approved_result: String,
    remaining_results: Vec<(String, String, String)>,
) -> Vec<Message> {
    let mut history = cont.history.clone();

    // Assistant message that contained all the tool calls of the batch
    history.push(cont.assistant_message.clone());

    // Tool results that completed before the approval-requiring call
    for (call_id, tool_name, result) in &cont.completed_tool_results {
        history.push(Message::tool_result(
            call_id.clone(),
            tool_name.clone(),
            result.clone(),
        ));
    }

    // The real result for the previously blocked tool call
    history.push(Message::tool_result(
        cont.pending_tool_call.call_id.clone(),
        cont.pending_tool_call.tool_name.clone(),
        approved_result,
    ));

    // Results from remaining tool calls (executed after approval)
    for (call_id, tool_name, result) in remaining_results {
        history.push(Message::tool_result(call_id, tool_name, result));
    }

    history
}
