//! Turn Continuation: checkpoint-and-resume for approval-suspended agent turns.
//!
//! When a tool call requires operator approval, the turn is **checkpointed** to disk
//! and the tokio task is released. When the approval is resolved the scheduler
//! re-queues the task; `spawn_task_execution` loads the continuation, executes the
//! approved action directly, reconstructs the conversation history, and resumes
//! `execute_with_history` — the LLM sees the real tool result and continues normally.
//!
//! This replaces the previous "kill turn + notify agent + agent retries with
//! approval_ref" pattern, eliminating the source of context loss and the complex
//! resume message / checkpoint dance.

use crate::llm::{Message, ToolCall};
use crate::runtime::guard::LoopGuardState;
use autonoetic_types::background::{ApprovalDecision, ScheduledAction};
use autonoetic_types::config::GatewayConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

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
    /// Tool name (e.g. `"sandbox.exec"`, `"agent.install"`).
    pub tool_name: String,
    /// JSON-encoded arguments string as produced by the model.
    pub arguments: String,
    /// The raw `approval_required` JSON returned by the tool handler.
    pub approval_response: String,
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

/// Root directory for continuation files: `.gateway/continuations/`.
pub fn continuations_dir(config: &GatewayConfig) -> PathBuf {
    config.agents_dir.join(".gateway").join("continuations")
}

fn continuation_path(config: &GatewayConfig, task_id: &str) -> PathBuf {
    continuations_dir(config).join(format!("{}.json", task_id))
}

/// Persist a `TurnContinuation` for the given task.
pub fn save_continuation(
    config: &GatewayConfig,
    task_id: &str,
    cont: &TurnContinuation,
) -> anyhow::Result<()> {
    let dir = continuations_dir(config);
    std::fs::create_dir_all(&dir)?;
    let path = continuation_path(config, task_id);
    let json = serde_json::to_string_pretty(cont)?;
    std::fs::write(&path, json)?;
    tracing::debug!(
        target: "continuation",
        task_id = %task_id,
        path = %path.display(),
        "Saved turn continuation"
    );
    Ok(())
}

/// Load a previously saved `TurnContinuation`, returning `None` if not found.
pub fn load_continuation(
    config: &GatewayConfig,
    task_id: &str,
) -> anyhow::Result<Option<TurnContinuation>> {
    let path = continuation_path(config, task_id);
    if !path.exists() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(&path)?;
    let cont: TurnContinuation = serde_json::from_str(&json)?;
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
/// For `AgentInstall` this calls the `agent.install` tool handler with the
/// stored full payload and `promotion_gate.install_approval_ref` set.
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
            // Build args JSON with approval_ref set so the handler skips
            // remote-access detection and proceeds directly to execution.
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
                "sandbox.exec",
                manifest,
                &policy,
                agent_dir,
                gateway_dir,
                &args.to_string(),
                session_id,
                None,
                Some(config),
                gateway_store,
            )
        }

        ScheduledAction::AgentInstall {
            agent_id, payload, ..
        } => {
            // Re-invoke agent.install with the stored payload and the
            // approval_ref pre-set so the handler skips the approval gate.
            let stored_payload = payload.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "AgentInstall approval for '{}' has no stored payload",
                    agent_id
                )
            })?;

            // Inject install_approval_ref into the promotion_gate field.
            let mut args = stored_payload;
            let gate = args
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("AgentInstall payload is not a JSON object"))?
                .entry("promotion_gate")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(gate_obj) = gate.as_object_mut() {
                gate_obj.insert(
                    "install_approval_ref".to_string(),
                    serde_json::Value::String(decision.request_id.clone()),
                );
            }

            tracing::info!(
                target: "continuation",
                request_id = %decision.request_id,
                agent_id = %agent_id,
                "Executing approved agent.install action"
            );
            registry.execute(
                "agent.install",
                manifest,
                &policy,
                agent_dir,
                gateway_dir,
                &args.to_string(),
                session_id,
                None,
                Some(config),
                gateway_store,
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
