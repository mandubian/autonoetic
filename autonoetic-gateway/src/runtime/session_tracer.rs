//! Session Tracer for Agent Execution.
//!
//! Owns session_id, event sequencing, causal logger access, and shared trace helpers.

use crate::causal_chain::CausalLogger;
use crate::log_redaction::redact_text_for_logs;
use crate::runtime::artifact::Artifact;
use crate::runtime::live_digest::{
    base_session_id, format_tool_action_line, format_tool_digest_result, LiveDigestWriter,
};
use std::sync::{Arc, Mutex};
use autonoetic_types::causal_chain::EntryStatus;
use sha2::{Digest, Sha256};
use std::path::Path;

const EVIDENCE_MODE_ENV: &str = "AUTONOETIC_EVIDENCE_MODE";

/// Max characters for `result_preview` in causal_chain.jsonl tool_invoke entries.
/// Full tool results are stored in the evidence store when evidence mode is Full (see evidence_ref).
const TOOL_RESULT_PREVIEW_MAX_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceMode {
    Off,
    Errors,
    Full,
}

impl EvidenceMode {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "" | "full" => Ok(Self::Full),
            "errors" => Ok(Self::Errors),
            "off" => Ok(Self::Off),
            other => anyhow::bail!(
                "Invalid {}='{}'. Expected one of: full, errors, off",
                EVIDENCE_MODE_ENV,
                other
            ),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            EvidenceMode::Off => "off",
            EvidenceMode::Errors => "errors",
            EvidenceMode::Full => "full",
        }
    }
}

pub struct EvidenceStore {
    mode: EvidenceMode,
    agent_dir: std::path::PathBuf,
    session_id: String,
    base_dir: Option<std::path::PathBuf>,
}

impl EvidenceStore {
    /// Create evidence store from environment variable.
    pub fn from_env(agent_dir: &Path, session_id: &str) -> anyhow::Result<Self> {
        let raw = std::env::var(EVIDENCE_MODE_ENV).unwrap_or_else(|_| "full".to_string());
        let mode = EvidenceMode::parse(&raw)?;
        let base_dir = if mode == EvidenceMode::Full {
            let dir = agent_dir.join("history").join("evidence").join(session_id);
            std::fs::create_dir_all(&dir)?;
            Some(dir)
        } else {
            None
        };
        Ok(Self {
            mode,
            agent_dir: agent_dir.to_path_buf(),
            session_id: session_id.to_string(),
            base_dir,
        })
    }

    /// Create evidence store from config.
    pub fn from_config(
        agent_dir: &Path,
        session_id: &str,
        evidence_mode: &str,
    ) -> anyhow::Result<Self> {
        let mode = EvidenceMode::parse(evidence_mode)?;
        let base_dir = if mode == EvidenceMode::Full {
            let dir = agent_dir.join("history").join("evidence").join(session_id);
            std::fs::create_dir_all(&dir)?;
            Some(dir)
        } else {
            None
        };
        Ok(Self {
            mode,
            agent_dir: agent_dir.to_path_buf(),
            session_id: session_id.to_string(),
            base_dir,
        })
    }

    fn ensure_base_dir(&self) -> anyhow::Result<std::path::PathBuf> {
        if let Some(dir) = &self.base_dir {
            return Ok(dir.clone());
        }
        let dir = self
            .agent_dir
            .join("history")
            .join("evidence")
            .join(&self.session_id);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    pub fn capture_json(
        &self,
        turn_id: Option<&str>,
        category: &str,
        action: &str,
        payload: &serde_json::Value,
    ) -> anyhow::Result<Option<String>> {
        if self.mode != EvidenceMode::Full {
            return Ok(None);
        }
        let base = self.ensure_base_dir()?;
        let file_name = format!(
            "{}-{}-{}-{}-{}.json",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ"),
            sanitize_token(turn_id.unwrap_or("session")),
            sanitize_token(category),
            sanitize_token(action),
            uuid::Uuid::new_v4()
        );
        let path = base.join(file_name);
        std::fs::write(&path, serde_json::to_string_pretty(payload)?)?;
        let rel = path.strip_prefix(&self.agent_dir).unwrap_or(&path);
        Ok(Some(rel.display().to_string()))
    }

    pub fn capture_json_force(
        &self,
        turn_id: Option<&str>,
        category: &str,
        action: &str,
        payload: &serde_json::Value,
    ) -> anyhow::Result<Option<String>> {
        let base = self.ensure_base_dir()?;
        let file_name = format!(
            "{}-{}-{}-{}-{}.json",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ"),
            sanitize_token(turn_id.unwrap_or("session")),
            sanitize_token(category),
            sanitize_token(action),
            uuid::Uuid::new_v4()
        );
        let path = base.join(file_name);
        std::fs::write(&path, serde_json::to_string_pretty(payload)?)?;
        let rel = path.strip_prefix(&self.agent_dir).unwrap_or(&path);
        Ok(Some(rel.display().to_string()))
    }
}

pub struct SessionTracer {
    causal_logger: CausalLogger,
    agent_id: String,
    session_id: String,
    turn_id: Option<String>,
    event_seq: u64,
    evidence_store: EvidenceStore,
    /// Progressive digest written to `.gateway/sessions/{base}/digest.md`.
    live_digest: Option<Arc<Mutex<LiveDigestWriter>>>,
    /// Optional GatewayStore for dual-write to causal_events table.
    gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
}

impl SessionTracer {
    pub fn new(agent_dir: &Path, agent_id: &str, session_id: &str) -> anyhow::Result<Self> {
        let evidence_store = EvidenceStore::from_env(agent_dir, session_id)?;
        Self::new_with_evidence_store(agent_dir, agent_id, session_id, evidence_store)
    }

    pub fn new_with_evidence_mode(
        agent_dir: &Path,
        agent_id: &str,
        session_id: &str,
        evidence_mode: &str,
    ) -> anyhow::Result<Self> {
        let evidence_store = EvidenceStore::from_config(agent_dir, session_id, evidence_mode)?;
        Self::new_with_evidence_store(agent_dir, agent_id, session_id, evidence_store)
    }

    fn new_with_evidence_store(
        agent_dir: &Path,
        agent_id: &str,
        session_id: &str,
        evidence_store: EvidenceStore,
    ) -> anyhow::Result<Self> {
        let causal_logger = init_causal_logger(agent_dir)?;
        Ok(Self {
            causal_logger,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: 0,
            evidence_store,
            live_digest: None,
            gateway_store: None,
        })
    }

    /// Attach a shared live digest writer (opened by [`AgentExecutor`](crate::runtime::lifecycle::AgentExecutor)).
    pub fn with_live_digest(mut self, writer: Arc<Mutex<LiveDigestWriter>>) -> Self {
        self.live_digest = Some(writer);
        self
    }

    pub fn start_digest_turn(&mut self) -> anyhow::Result<()> {
        if let Some(w) = &self.live_digest {
            w.lock().unwrap().start_turn()?;
        }
        self.append_live_digest_event("turn.start", None);
        Ok(())
    }

    pub fn record_digest_llm_round(
        &mut self,
        model: &str,
        stop_reason: &str,
        tool_calls: usize,
        input_tokens: u64,
        output_tokens: u64,
    ) -> anyhow::Result<()> {
        if let Some(w) = &self.live_digest {
            let model_short = model.split('/').last().unwrap_or(model);
            w.lock().unwrap().record_llm_round(
                model_short,
                stop_reason,
                tool_calls,
                input_tokens,
                output_tokens,
            )?;
        }
        self.append_live_digest_event(
            "llm.round",
            Some(serde_json::json!({
                "model": model,
                "stop_reason": stop_reason,
                "tool_calls": tool_calls,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens
            })),
        );
        Ok(())
    }

    pub fn record_digest_llm_retry_note(
        &mut self,
        attempt: usize,
        max_retries: usize,
    ) -> anyhow::Result<()> {
        if let Some(w) = &self.live_digest {
            w.lock()
                .unwrap()
                .record_llm_retry_note(attempt, max_retries)?;
        }
        self.append_live_digest_event(
            "llm.retry",
            Some(serde_json::json!({
                "attempt": attempt,
                "max_retries": max_retries
            })),
        );
        Ok(())
    }

    pub fn end_digest_turn(&mut self) -> anyhow::Result<()> {
        if let Some(w) = &self.live_digest {
            w.lock().unwrap().end_turn()?;
        }
        self.append_live_digest_event("turn.end", None);
        Ok(())
    }

    pub fn with_turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub fn set_turn_id(&mut self, turn_id: impl Into<String>) {
        self.turn_id = Some(turn_id.into());
    }

    pub fn with_gateway_store(
        mut self,
        store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
    ) -> Self {
        self.gateway_store = store;
        self
    }

    fn append_live_digest_event(&self, event_type: &str, payload: Option<serde_json::Value>) {
        let Some(store) = &self.gateway_store else {
            return;
        };
        let row = crate::scheduler::gateway_store::LiveDigestEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            root_session_id: base_session_id(&self.session_id).to_string(),
            source_session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            source_agent_id: Some(self.agent_id.clone()),
            source_node_id: std::env::var("AUTONOETIC_NODE_ID")
                .unwrap_or_else(|_| "gateway".to_string()),
            event_type: event_type.to_string(),
            payload: payload.and_then(|v| serde_json::to_string(&v).ok()),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = store.create_live_digest_event(&row) {
            tracing::debug!(
                target: "live_digest",
                error = %e,
                event_type = %event_type,
                "Failed to persist live digest event"
            );
        }
    }

    fn next_event_seq(&mut self) -> u64 {
        self.event_seq += 1;
        self.event_seq
    }

    pub fn log_event(
        &mut self,
        category: &str,
        action: &str,
        status: EntryStatus,
        payload: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let event_seq = self.next_event_seq();
        let event_id = uuid::Uuid::new_v4().to_string();

        log_causal_event(
            &self.causal_logger,
            &self.agent_id,
            category,
            action,
            status.clone(),
            payload.clone(),
            &self.session_id,
            self.turn_id.as_deref(),
            event_seq,
        )?;

        let payload_str = payload.as_ref().and_then(|v| serde_json::to_string(v).ok());

        if let Some(store) = &self.gateway_store {
            let payload_ref = payload
                .as_ref()
                .and_then(|v| v.get("payload_ref"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let evidence_ref = payload
                .as_ref()
                .and_then(|v| v.get("evidence_ref"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let reason = payload
                .as_ref()
                .and_then(|v| v.get("reason"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let target = payload
                .as_ref()
                .and_then(|v| v.get("target"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if let Err(e) =
                store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
                    event_id: event_id.clone(),
                    agent_id: self.agent_id.clone(),
                    session_id: self.session_id.clone(),
                    turn_id: self.turn_id.clone(),
                    event_seq,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    category: category.to_string(),
                    action: action.to_string(),
                    status: status.to_string(),
                    target,
                    payload: payload_str.clone(),
                    payload_ref,
                    evidence_ref,
                    reason,
                })
            {
                tracing::warn!(
                    target: "session_tracer",
                    error = %e,
                    "Failed to write causal event to DB — continuing with JSONL only"
                );
            }
        }

        Ok(())
    }

    pub fn log_session_start(
        &mut self,
        trigger_type: &str,
        trigger: &str,
        evidence_mode: EvidenceMode,
    ) -> anyhow::Result<()> {
        let mut session_payload = serde_json::json!({
            "trigger_type": trigger_type,
            "trigger_len": trigger.len(),
            "trigger_sha256": sha256_hex(trigger),
            "trigger_preview": redact_text_for_logs(&truncate_for_log(trigger, 256)),
            "evidence_mode": evidence_mode.as_str(),
        });
        let session_evidence = serde_json::json!({
            "trigger": redact_text_for_logs(trigger)
        });
        if let Some(evidence_ref) =
            self.evidence_store
                .capture_json(None, "session", "start", &session_evidence)?
        {
            session_payload["evidence_ref"] = serde_json::json!(evidence_ref);
        }
        self.log_event(
            "session",
            "start",
            EntryStatus::Success,
            Some(session_payload.clone()),
        )?;

        if let Some(w) = &self.live_digest {
            let preview = session_payload
                .get("trigger_preview")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if let Err(e) = w
                .lock()
                .unwrap()
                .start_session(&self.agent_id, preview)
            {
                tracing::warn!(
                    target: "live_digest",
                    error = %e,
                    "Failed to write digest session preamble"
                );
            }
        }
        self.append_live_digest_event(
            "session.start",
            Some(serde_json::json!({
                "trigger_type": trigger_type,
                "trigger_preview": session_payload.get("trigger_preview").cloned()
            })),
        );
        Ok(())
    }

    pub fn log_session_end(&mut self, reason: &str) {
        let _ = self.log_event(
            "session",
            "end",
            EntryStatus::Success,
            Some(serde_json::json!({ "reason": reason })),
        );
    }

    pub fn log_wake(&mut self, history_messages: usize, evidence_mode: EvidenceMode) {
        let _ = self.log_event(
            "lifecycle",
            "wake",
            EntryStatus::Success,
            Some(serde_json::json!({
                "history_messages": history_messages,
                "evidence_mode": evidence_mode.as_str(),
            })),
        );
    }

    pub fn log_llm_completion(
        &mut self,
        model: &str,
        stop_reason: &str,
        text: &str,
        tool_calls: usize,
        input_tokens: u64,
        output_tokens: u64,
        tool_call_details: &[serde_json::Value],
        context_window_tokens: Option<u32>,
        input_context_pct: Option<f32>,
    ) -> anyhow::Result<()> {
        let mut usage = serde_json::json!({
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        });
        if let Some(w) = context_window_tokens {
            usage["context_window_tokens"] = serde_json::json!(w);
        }
        if let Some(p) = input_context_pct {
            usage["input_context_pct"] = serde_json::json!(p);
        }

        let mut llm_payload = serde_json::json!({
            "model": model,
            "stop_reason": stop_reason,
            "text": redact_text_for_logs(&truncate_for_log(text, 256)),
            "text_sha256": sha256_hex(text),
            "tool_calls": tool_calls,
            "usage": usage.clone()
        });
        let llm_evidence = serde_json::json!({
            "model": model,
            "stop_reason": stop_reason,
            "text": redact_text_for_logs(text),
            "tool_calls": tool_call_details,
            "usage": usage
        });
        if let Some(evidence_ref) = self.evidence_store.capture_json(
            self.turn_id.as_deref(),
            "llm",
            "completion",
            &llm_evidence,
        )? {
            llm_payload["evidence_ref"] = serde_json::json!(evidence_ref);
        }
        self.log_event("llm", "completion", EntryStatus::Success, Some(llm_payload))?;
        Ok(())
    }

    pub fn log_tool_requested(&mut self, tool_name: &str, arguments: &str) -> anyhow::Result<()> {
        let redacted_args = redact_text_for_logs(arguments);
        if tool_name != "digest.annotate" {
            if let Some(w) = &self.live_digest {
                let line = format_tool_action_line(tool_name, &redacted_args);
                if let Err(e) = w.lock().unwrap().record_action(&line) {
                    tracing::warn!(target: "live_digest", error = %e, "digest record_action failed");
                }
            }
        }
        self.append_live_digest_event(
            "tool.requested",
            Some(serde_json::json!({
                "tool_name": tool_name,
                "arguments": redacted_args.clone(),
            })),
        );
        let mut requested_payload = serde_json::json!({
            "tool_name": tool_name,
            "arguments": redacted_args,
            "arguments_sha256": sha256_hex(arguments)
        });
        let requested_evidence = serde_json::json!({
            "tool_name": tool_name,
            "arguments": redacted_args
        });
        if let Some(evidence_ref) = self.evidence_store.capture_json(
            self.turn_id.as_deref(),
            "tool_invoke",
            "requested",
            &requested_evidence,
        )? {
            requested_payload["evidence_ref"] = serde_json::json!(evidence_ref);
        }
        self.log_event(
            "tool_invoke",
            "requested",
            EntryStatus::Success,
            Some(requested_payload),
        )?;
        Ok(())
    }

    pub fn log_tool_completed(&mut self, tool_name: &str, result: &str) -> anyhow::Result<()> {
        let mut completed_payload = serde_json::json!({
            "tool_name": tool_name,
            "result_len": result.len(),
            "result_sha256": sha256_hex(result),
            "result_preview": redact_text_for_logs(&truncate_for_log(result, TOOL_RESULT_PREVIEW_MAX_CHARS))
        });
        if let Some(approval_id) = find_approval_request_id_in_result(result) {
            completed_payload["approval_request_id"] = serde_json::json!(approval_id);
        }
        let completed_evidence = serde_json::json!({
            "tool_name": tool_name,
            "result": redact_text_for_logs(result)
        });
        let evidence_ref = if should_force_tool_result_evidence(result) {
            self.evidence_store.capture_json_force(
                self.turn_id.as_deref(),
                "tool_invoke",
                "completed",
                &completed_evidence,
            )?
        } else {
            self.evidence_store.capture_json(
                self.turn_id.as_deref(),
                "tool_invoke",
                "completed",
                &completed_evidence,
            )?
        };
        if let Some(evidence_ref) = evidence_ref {
            completed_payload["evidence_ref"] = serde_json::json!(evidence_ref);
        }
        self.log_event(
            "tool_invoke",
            "completed",
            EntryStatus::Success,
            Some(completed_payload),
        )?;

        if tool_name != "digest.annotate" {
            if let Some(w) = &self.live_digest {
                let mut guard = w.lock().unwrap();
                let formatted = format_tool_digest_result(tool_name, result);
                let ok = serde_json::from_str::<serde_json::Value>(result)
                    .ok()
                    .and_then(|v| v.get("ok").and_then(|x| x.as_bool()))
                    != Some(false);
                let r = if ok {
                    guard.record_result(&formatted)
                } else {
                    guard.record_error(&formatted)
                };
                if let Err(e) = r {
                    tracing::warn!(target: "live_digest", error = %e, "digest record result/error failed");
                }
            }
        }
        self.append_live_digest_event(
            "tool.completed",
            Some(serde_json::json!({
                "tool_name": tool_name,
                "result": crate::log_redaction::redact_text_for_logs(result)
            })),
        );
        Ok(())
    }

    pub fn log_artifact_detected(&mut self, artifact: &Artifact) -> anyhow::Result<()> {
        self.log_event(
            "artifact",
            "detected",
            EntryStatus::Success,
            Some(serde_json::to_value(artifact).unwrap_or(serde_json::json!({
                "type": artifact.artifact_type,
                "name": artifact.name
            }))),
        )
    }

    pub fn log_hibernate(&mut self, stop_reason: &str) {
        let _ = self.log_event(
            "lifecycle",
            "hibernate",
            EntryStatus::Success,
            Some(serde_json::json!({ "stop_reason": stop_reason })),
        );
    }

    pub fn log_stopped(&mut self, stop_reason: &str) {
        let _ = self.log_event(
            "lifecycle",
            "stopped",
            EntryStatus::Error,
            Some(serde_json::json!({ "stop_reason": stop_reason })),
        );
    }

    pub fn log_history_persisted(&mut self, message_count: usize, content_handle: &str) {
        let _ = self.log_event(
            "session",
            "history.persisted",
            EntryStatus::Success,
            Some(serde_json::json!({
                "message_count": message_count,
                "content_handle": content_handle
            })),
        );
    }

    pub fn log_session_forked(
        &mut self,
        source_session_id: &str,
        fork_turn: u64,
        history_handle: &str,
        branch_message: Option<&str>,
    ) {
        let payload = serde_json::json!({
            "source_session_id": source_session_id,
            "fork_turn": fork_turn,
            "history_handle": history_handle,
            "branch_message_sha256": branch_message.map(|m| {
                use sha2::{Sha256, Digest};
                let mut hasher = Sha256::new();
                hasher.update(m.as_bytes());
                format!("{:x}", hasher.finalize())
            })
        });
        let _ = self.log_event("session", "forked", EntryStatus::Success, Some(payload));
    }
}

fn init_causal_logger(agent_dir: &Path) -> anyhow::Result<CausalLogger> {
    let history_dir = agent_dir.join("history");
    std::fs::create_dir_all(&history_dir)?;
    CausalLogger::new(history_dir.join("causal_chain.jsonl"))
}

fn log_causal_event(
    logger: &CausalLogger,
    actor_id: &str,
    category: &str,
    action: &str,
    status: EntryStatus,
    payload: Option<serde_json::Value>,
    session_id: &str,
    turn_id: Option<&str>,
    event_seq: u64,
) -> anyhow::Result<()> {
    logger
        .log(
            actor_id, session_id, turn_id, event_seq, category, action, status, payload,
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to append causal log entry for {}/{} in session {}: {}",
                category,
                action,
                session_id,
                e
            )
        })
}

fn truncate_for_log(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_string();
    }
    let truncated: String = value.chars().take(max_len).collect();
    format!("{}...", truncated)
}

fn sanitize_token(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    format!("{:x}", digest)
}

fn find_approval_request_id_in_result(result: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(result).ok()?;
    let request_id = parsed.get("request_id")?.as_str()?;
    if request_id.starts_with("apr-") {
        Some(request_id.to_string())
    } else {
        None
    }
}

fn should_force_tool_result_evidence(result: &str) -> bool {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result) else {
        return false;
    };

    if parsed
        .get("approval_required")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return true;
    }

    if parsed.get("ok") == Some(&serde_json::Value::Bool(false)) {
        return true;
    }

    if parsed
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .map(|code| code != 0)
        .unwrap_or(false)
    {
        return true;
    }

    parsed.get("error_type").is_some()
}

#[cfg(test)]
impl SessionTracer {
    /// Creates a test tracer that discards all output.
    pub fn test_tracer() -> Self {
        Self {
            causal_logger: CausalLogger::test_logger("/dev/null"),
            agent_id: "test-agent".to_string(),
            session_id: "test-session".to_string(),
            turn_id: Some("test-turn".to_string()),
            event_seq: 0,
            evidence_store: EvidenceStore {
                mode: EvidenceMode::Off,
                agent_dir: std::path::PathBuf::from("/tmp"),
                session_id: "test-session".to_string(),
                base_dir: None,
            },
            live_digest: None,
            gateway_store: None,
        }
    }

    /// Creates a test tracer with gateway store for dual-write testing.
    pub fn test_tracer_with_store(
        agent_dir: &std::path::Path,
        store: std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>,
    ) -> Self {
        Self {
            causal_logger: CausalLogger::test_logger(
                &agent_dir.join("history").join("causal_chain.jsonl"),
            ),
            agent_id: "test-agent".to_string(),
            session_id: "test-session".to_string(),
            turn_id: Some("test-turn".to_string()),
            event_seq: 0,
            evidence_store: EvidenceStore {
                mode: EvidenceMode::Off,
                agent_dir: agent_dir.to_path_buf(),
                session_id: "test-session".to_string(),
                base_dir: None,
            },
            live_digest: None,
            gateway_store: Some(store),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_force_tool_result_evidence_for_failures_and_approvals() {
        assert!(should_force_tool_result_evidence(
            r#"{"ok":false,"error_type":"validation","message":"boom"}"#
        ));
        assert!(should_force_tool_result_evidence(
            r#"{"ok":false,"approval_required":true,"request_id":"apr-12345678"}"#
        ));
        assert!(should_force_tool_result_evidence(
            r#"{"ok":true,"exit_code":1,"stderr":"failed"}"#
        ));
        assert!(!should_force_tool_result_evidence(
            r#"{"ok":true,"exit_code":0,"stdout":"all good"}"#
        ));
    }

    #[test]
    fn test_log_tool_completed_captures_failure_evidence_even_when_off() {
        let temp = tempdir().unwrap();
        let agent_dir = temp.path().join("planner.default");
        fs::create_dir_all(agent_dir.join("history")).unwrap();

        let mut tracer = SessionTracer::new(&agent_dir, "planner.default", "demo-session").unwrap();
        tracer.set_turn_id("turn-000001");

        tracer
            .log_tool_completed(
                "sandbox.exec",
                r#"{"ok":false,"exit_code":1,"stderr":"test failed","stdout":"full output"}"#,
            )
            .unwrap();

        let causal_log =
            fs::read_to_string(agent_dir.join("history").join("causal_chain.jsonl")).unwrap();
        assert!(
            causal_log.contains("evidence_ref"),
            "failed tool results should preserve a full evidence pointer"
        );

        let evidence_dir = agent_dir
            .join("history")
            .join("evidence")
            .join("demo-session");
        let evidence_files: Vec<_> = fs::read_dir(evidence_dir).unwrap().collect();
        assert_eq!(evidence_files.len(), 1);
    }

    #[test]
    fn test_evidence_defaults_to_full_when_env_unset() {
        let temp = tempdir().unwrap();
        let agent_dir = temp.path().join("planner.default");
        fs::create_dir_all(agent_dir.join("history")).unwrap();

        let previous = std::env::var("AUTONOETIC_EVIDENCE_MODE").ok();
        unsafe {
            std::env::remove_var("AUTONOETIC_EVIDENCE_MODE");
        }

        let store = EvidenceStore::from_env(&agent_dir, "demo-session").unwrap();
        assert_eq!(store.mode, EvidenceMode::Full);

        match previous {
            Some(value) => unsafe { std::env::set_var("AUTONOETIC_EVIDENCE_MODE", value) },
            None => unsafe { std::env::remove_var("AUTONOETIC_EVIDENCE_MODE") },
        }
    }

    #[test]
    fn test_dual_write_produces_identical_event_data() {
        let temp = tempdir().unwrap();
        let agents_dir = temp.path().join("agents");
        let agent_dir = agents_dir.join("test-agent");
        let gateway_dir = agents_dir.join(".gateway");
        fs::create_dir_all(agent_dir.join("history")).unwrap();
        fs::create_dir_all(&gateway_dir).unwrap();

        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );

        let mut tracer = SessionTracer::test_tracer_with_store(&agent_dir, store.clone());
        tracer.set_turn_id("turn-000001");

        // Log an event - should write to both JSONL and DB
        let payload = serde_json::json!({
            "tool_name": "sandbox.exec",
            "arguments": "echo hello"
        });
        tracer
            .log_event(
                "tool_invoke",
                "completed",
                EntryStatus::Success,
                Some(payload.clone()),
            )
            .unwrap();

        // Read JSONL
        let jsonl_path = agent_dir.join("history").join("causal_chain.jsonl");
        let jsonl_content = fs::read_to_string(&jsonl_path).unwrap();
        let jsonl_lines: Vec<&str> = jsonl_content.lines().collect();
        assert_eq!(jsonl_lines.len(), 1, "Should have one JSONL entry");

        let jsonl_entry: serde_json::Value = serde_json::from_str(jsonl_lines[0]).unwrap();

        // Verify JSONL has expected fields
        assert_eq!(jsonl_entry["session_id"].as_str().unwrap(), "test-session");
        assert_eq!(jsonl_entry["turn_id"].as_str().unwrap(), "turn-000001");
        assert_eq!(jsonl_entry["category"].as_str().unwrap(), "tool_invoke");
        assert_eq!(jsonl_entry["action"].as_str().unwrap(), "completed");
        assert_eq!(jsonl_entry["status"].as_str().unwrap(), "SUCCESS");

        // Read DB
        let db_events = store
            .search_causal_events(Some("test-session"), None, 100)
            .unwrap();
        assert_eq!(db_events.len(), 1, "Should have one DB entry");

        let db_entry = &db_events[0];
        assert_eq!(db_entry.session_id, "test-session");
        assert_eq!(db_entry.turn_id.as_deref(), Some("turn-000001"));
        assert_eq!(db_entry.category, "tool_invoke");
        assert_eq!(db_entry.action, "completed");
        assert_eq!(db_entry.status, "SUCCESS");
    }

    #[test]
    fn test_dual_write_error_status_preserved() {
        let temp = tempdir().unwrap();
        let agents_dir = temp.path().join("agents");
        let agent_dir = agents_dir.join("test-agent");
        let gateway_dir = agents_dir.join(".gateway");
        fs::create_dir_all(agent_dir.join("history")).unwrap();
        fs::create_dir_all(&gateway_dir).unwrap();

        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );

        let mut tracer = SessionTracer::test_tracer_with_store(&agent_dir, store.clone());
        tracer.set_turn_id("turn-000002");

        // Log an error event
        let payload = serde_json::json!({
            "tool_name": "sandbox.exec",
            "reason": "compilation failed"
        });
        tracer
            .log_event("tool_invoke", "failure", EntryStatus::Error, Some(payload))
            .unwrap();

        // Read DB
        let db_events = store
            .search_causal_events(Some("test-session"), None, 100)
            .unwrap();
        assert_eq!(db_events.len(), 1);
        assert_eq!(db_events[0].status, "ERROR");
        assert_eq!(db_events[0].action, "failure");
    }
}
