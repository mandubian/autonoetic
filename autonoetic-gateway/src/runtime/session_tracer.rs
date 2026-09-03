//! Session Tracer for Agent Execution.
//!
//! Owns session_id, event sequencing, causal logger access, and shared trace helpers.

use crate::causal_chain::CausalLogger;
use crate::log_redaction::redact_text_for_logs;
use crate::runtime::artifact::Artifact;
use crate::runtime::live_digest::{
    base_session_id, format_tool_action_line, format_tool_digest_result, LiveDigestWriter,
};
use crate::runtime::session_report::SessionReportWriter;
use autonoetic_types::causal_chain::EntryStatus;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex};

const EVIDENCE_MODE_ENV: &str = "AUTONOETIC_EVIDENCE_MODE";

/// Max characters for `result_preview` in causal_chain.jsonl tool_invoke entries.
/// Full tool results are stored in the evidence store when evidence mode is Full (see evidence_ref).
const TOOL_RESULT_PREVIEW_MAX_CHARS: usize = 256;

/// Max characters for `agent.message` / `agent.reasoning` on the canonical
/// timeline (after redaction). Aligns with the room list body ceiling; the TUI
/// still repairs JSON truncated at the tail. Full text remains in the evidence store.
pub(crate) const TIMELINE_AGENT_NARRATIVE_MAX_CHARS: usize = 8_000;

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
    /// Structured live/final report written beside `digest.md`.
    live_report: Option<Arc<Mutex<SessionReportWriter>>>,
    /// Optional GatewayStore for dual-write to causal_events table.
    gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
    /// Turn-scoped ring of recent action labels (most recent last), so a failure
    /// can carry the chain that led to it instead of a context-free error (#367).
    recent_actions: std::collections::VecDeque<String>,
}

/// How many recent actions to keep for the error→action-chain link.
const RECENT_ACTIONS_CAP: usize = 6;

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
            live_report: None,
            gateway_store: None,
            recent_actions: std::collections::VecDeque::new(),
        })
    }

    /// Attach a shared live digest writer (opened by [`AgentExecutor`](crate::runtime::lifecycle::AgentExecutor)).
    pub fn with_live_digest(mut self, writer: Arc<Mutex<LiveDigestWriter>>) -> Self {
        self.live_digest = Some(writer);
        self
    }

    pub fn with_session_report(mut self, writer: Arc<Mutex<SessionReportWriter>>) -> Self {
        self.live_report = Some(writer);
        self
    }

    pub fn start_digest_turn(&mut self) -> anyhow::Result<()> {
        if let Some(w) = &self.live_digest {
            w.lock().unwrap().start_turn()?;
        }
        if let Some(w) = &self.live_report {
            w.lock().unwrap().start_turn(self.turn_id.as_deref())?;
        }
        // The action chain is turn-scoped — a failure links to what happened in
        // this turn, not stale actions from earlier ones (#367).
        self.recent_actions.clear();
        self.append_live_digest_event("turn.start", None);
        Ok(())
    }

    /// Record a short action label in the turn-scoped ring (capped), so a later
    /// failure can carry the chain that led to it.
    fn note_action(&mut self, label: impl Into<String>) {
        self.recent_actions.push_back(label.into());
        while self.recent_actions.len() > RECENT_ACTIONS_CAP {
            self.recent_actions.pop_front();
        }
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

    /// Surface a runtime-lock drift on the canonical timeline (#367) — an
    /// integrity event that was previously causal-only, so the room never showed
    /// it. The runtime's own mechanical ruling, so it speaks from the `Runtime`
    /// seat. A **rejected** drift (the session is about to fail) is `Error`; an
    /// **override** (`allow_runtime_lock_drift`, running in a drifted environment
    /// anyway) is `Attention` — a silently-weakened reproducibility guarantee
    /// the operator should still see. `payload` mirrors the causal record.
    pub fn record_runtime_lock_drift(&self, payload: serde_json::Value, allow: bool) {
        let Some(store) = &self.gateway_store else {
            return;
        };
        let principal = autonoetic_types::principal::Principal {
            kind: autonoetic_types::principal::PrincipalKind::Script,
            id: "gateway".to_string(),
        };
        let altitude = if allow {
            autonoetic_types::session_timeline::Altitude::Attention
        } else {
            autonoetic_types::session_timeline::Altitude::Error
        };
        let event = crate::runtime::session_timeline::build_timeline_event(
            base_session_id(&self.session_id).to_string(),
            self.session_id.clone(),
            self.turn_id.clone(),
            &principal,
            &autonoetic_types::session_timeline::SessionRole::Runtime,
            "runtime.lock_drift",
            Some(altitude),
            Some(payload),
            autonoetic_types::session_timeline::TimelineRefs::default(),
        );
        if let Err(e) = store.create_live_digest_event(&event) {
            tracing::debug!(
                target: "session_timeline",
                error = %e,
                "runtime.lock_drift timeline emit failed"
            );
        }
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
        self.append_live_digest_event_at(
            event_type,
            payload,
            None,
            autonoetic_types::session_timeline::TimelineRefs::default(),
        )
    }

    /// Like [`append_live_digest_event`] but lets the caller *raise* the
    /// altitude for this one event (e.g. a `tool.completed` whose result is
    /// `ok:false` is bumped to `Attention` so failures aren't hidden at
    /// `Detail`). The override only ever raises: `max(override, derived)` is
    /// used, so a caller cannot accidentally lower an event below its policy
    /// floor.
    fn append_live_digest_event_at(
        &self,
        event_type: &str,
        payload: Option<serde_json::Value>,
        altitude_override: Option<autonoetic_types::session_timeline::Altitude>,
        refs: autonoetic_types::session_timeline::TimelineRefs,
    ) {
        let Some(store) = &self.gateway_store else {
            return;
        };
        // Session Room attribution (#363 P1): seat derived from the agent id,
        // principal = this autonoetic agent, altitude = max(base, role_floor).
        let role = crate::runtime::session_timeline::derive_role(&self.agent_id);
        let mut altitude = crate::runtime::session_timeline::altitude_for(event_type, &role);
        if let Some(override_alt) = altitude_override {
            altitude = altitude.max(override_alt);
        }
        let principal = autonoetic_types::principal::Principal::agent(self.agent_id.clone());
        let row = crate::scheduler::gateway_store::LiveDigestEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            root_session_id: base_session_id(&self.session_id).to_string(),
            source_session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            source_agent_id: Some(self.agent_id.clone()),
            source_node_id: crate::execution::gateway_actor_id(),
            event_type: event_type.to_string(),
            payload: payload.and_then(|v| serde_json::to_string(&v).ok()),
            created_at: chrono::Utc::now().to_rfc3339(),
            principal_kind: Some(principal.kind_to_storage()),
            principal_id: Some(principal.id.clone()),
            role: Some(role.to_storage()),
            altitude: Some(altitude.as_str().to_string()),
            // First-class cross-references (#391) so the Room TUI can drill from a
            // timeline line into depth. Empty refs stay `None` (unchanged from the
            // previous hardcoded behavior for events that carry none).
            refs_json: if refs.is_empty() {
                None
            } else {
                serde_json::to_string(&refs).ok()
            },
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
    ) -> anyhow::Result<String> {
        let event_seq = self.next_event_seq();
        let event_id = uuid::Uuid::new_v4().to_string();

        // Attribution and target are extracted once so the JSONL witness and
        // the DB row carry the same values (#1278) — the witness binds both
        // into its entry hash, so they must be written, not derived later.
        let target = payload
            .as_ref()
            .and_then(|v| v.get("target"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let enforced_rules = enforced_rules_from_payload(payload.as_ref());

        log_causal_event(
            &self.causal_logger,
            &self.agent_id,
            category,
            action,
            status.clone(),
            target.as_deref(),
            &enforced_rules,
            payload
                .as_ref()
                .map(|v| crate::log_redaction::RedactedPayload::from_redacted(v.clone())),
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
                    enforced_rules,
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

        Ok(event_id)
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
            if let Err(e) = w.lock().unwrap().start_session(&self.agent_id, preview) {
                tracing::warn!(
                    target: "live_digest",
                    error = %e,
                    "Failed to write digest session preamble"
                );
            }
        }
        if let Some(w) = &self.live_report {
            let preview = session_payload
                .get("trigger_preview")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if let Err(e) = w.lock().unwrap().start_session(preview) {
                tracing::warn!(
                    target: "session_report",
                    error = %e,
                    "Failed to write session report start"
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

    /// Record the **filtered wire view** for one LLM request (RFC §9.2).
    ///
    /// The §9.2 acceptance bar is that "what left the machine at turn N?" has a
    /// literal answer. The chokepoint (`EgressChokepointDriver`) already
    /// produces a `FilterReport` describing what it withheld; this method
    /// records that summary on the session-local tracer (JSONL +
    /// `causal_chain.jsonl`) so the answer is available alongside the response
    /// log, not only in `gateway.db`. Content-free metadata only (sink, counts,
    /// withheld tool_call_ids + the indication text, which is itself metadata).
    ///
    /// `emit_chokepoint_events` (called by lifecycle.rs next to this) writes
    /// the per-entry `egress.envelope_withheld` / `egress.request_filtered` /
    /// `egress.assertion_violation` events to `gateway.db`; this method writes
    /// the consolidated per-request summary to the tracer artifact. No-op-safe:
    /// call unconditionally; the caller already guards on
    /// `!egress_labels.is_empty()`.
    pub fn log_egress_request_filtered(
        &mut self,
        model: &str,
        report: &crate::llm::egress_chokepoint::FilterReport,
    ) -> anyhow::Result<()> {
        let payload = serde_json::json!({
            "model": model,
            "target_sink": report.sink,
            "withheld_count": report.withheld.len(),
            "included_count": report.included,
            "violation_count": report.violations.len(),
            // Per-withheld metadata: tool_call_id + indication (content-free).
            "withheld": report.withheld.iter().map(|w| serde_json::json!({
                "tool_call_id": w.tool_call_id,
                "indication": w.indication,
            })).collect::<Vec<_>>(),
        });
        self.log_event(
            "egress",
            // Full dotted action — matches the slice-1 `egress.*` convention
            // (egress.request_filtered / egress.envelope_withheld / …) so the
            // audit CLI's renderer and any `egress.*` tooling match it.
            "egress.request_forwarded",
            EntryStatus::Success,
            Some(payload),
        )?;
        Ok(())
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
        reasoning_content: Option<&str>,
        cached_tokens: u64,
        reasoning_tokens: u64,
    ) -> anyhow::Result<()> {
        let mut usage = serde_json::json!({
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cached_tokens": cached_tokens,
            "reasoning_tokens": reasoning_tokens,
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
        if let Some(rc) = &reasoning_content {
            llm_payload["reasoning_sha256"] = serde_json::json!(sha256_hex(rc));
        }
        let mut llm_evidence = serde_json::json!({
            "model": model,
            "stop_reason": stop_reason,
            "text": redact_text_for_logs(text),
            "tool_calls": tool_call_details,
            "usage": usage
        });
        if let Some(rc) = &reasoning_content {
            llm_evidence["reasoning_content"] = serde_json::json!(redact_text_for_logs(rc));
        }
        if let Some(evidence_ref) = self.evidence_store.capture_json(
            self.turn_id.as_deref(),
            "llm",
            "completion",
            &llm_evidence,
        )? {
            llm_payload["evidence_ref"] = serde_json::json!(evidence_ref);
        }
        if let Some(rc) = &reasoning_content {
            let reasoning_evidence = serde_json::json!({
                "reasoning_content": redact_text_for_logs(rc),
                "reasoning_sha256": sha256_hex(rc),
            });
            if let Some(ref_) = self.evidence_store.capture_json_force(
                self.turn_id.as_deref(),
                "llm",
                "reasoning",
                &reasoning_evidence,
            )? {
                llm_payload["reasoning_evidence_ref"] = serde_json::json!(ref_);
            }
        }
        self.log_event("llm", "completion", EntryStatus::Success, Some(llm_payload))?;

        // P4 (#367) — the agent's *narrative* onto the canonical timeline so the
        // room reads as a conversation (intent → actions → result), not just
        // mechanical `llm.round` markers + tool calls. The full text/reasoning
        // already live in the evidence store; here we surface a readable, capped,
        // redacted copy. `agent.message` is the agent speaking (Normal, shown at
        // the default floor, symmetric to `operator.message`); `agent.reasoning`
        // is the hidable "why" (Detail). Empty text (a pure tool-call round) is
        // skipped so we don't emit blank lines.
        // Redact the *full* text first (so JSON-aware key redaction sees valid
        // JSON), then hard-cap for the timeline row.
        let message = text.trim();
        if !message.is_empty() {
            self.append_live_digest_event(
                "agent.message",
                Some(serde_json::json!({
                    "message": cap_chars(
                        &autonoetic_types::redaction::redact_embedded_secrets(message),
                        TIMELINE_AGENT_NARRATIVE_MAX_CHARS,
                    ),
                })),
            );
        }
        if let Some(rc) = reasoning_content {
            let rc = rc.trim();
            if !rc.is_empty() {
                self.append_live_digest_event(
                    "agent.reasoning",
                    Some(serde_json::json!({
                        "reasoning": cap_chars(
                            &autonoetic_types::redaction::redact_embedded_secrets(rc),
                            TIMELINE_AGENT_NARRATIVE_MAX_CHARS,
                        ),
                    })),
                );
            }
        }
        Ok(())
    }

    /// Logged when the LLM driver returns an error before any completion is produced (causal chain + session report).
    pub fn log_llm_request_failed(&mut self, model: &str, err: &anyhow::Error) -> anyhow::Result<()> {
        let msg = redact_text_for_logs(&err.to_string());
        let payload = serde_json::json!({
            "error": msg,
            "model": model,
        });
        let event_id = self.log_event(
            "llm",
            "request_failed",
            EntryStatus::Error,
            Some(payload.clone()),
        )?;
        if let Some(w) = &self.live_report {
            if let Err(e) = w.lock().unwrap().record_execution_failure(
                "llm.complete",
                &msg,
                self.turn_id.as_deref(),
                Some(payload),
                Some(&event_id),
            ) {
                tracing::warn!(
                    target: "session_report",
                    error = %e,
                    "session report record_execution_failure (llm) failed"
                );
            }
        }
        // Link the preceding action chain so a failure isn't a context-free ✗
        // (#367) — the room can show "after: a → b → c".
        self.append_live_digest_event(
            "llm.request_failed",
            Some(serde_json::json!({
                "error": msg,
                "model": model,
                "preceding": Vec::from_iter(self.recent_actions.iter().cloned()),
            })),
        );
        Ok(())
    }

    /// Logged when the LLM driver returns Ok but with zero output tokens and empty text
    /// (provider silently returned nothing). Records a causal event, session report entry,
    /// and live digest event so the operator can see what happened.
    pub fn log_llm_empty_response(
        &mut self,
        model: &str,
        stop_reason: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> anyhow::Result<()> {
        let payload = serde_json::json!({
            "model": model,
            "stop_reason": stop_reason,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "error": format!("LLM returned empty response (0 output tokens, stop_reason={})", stop_reason),
        });
        let event_id = self.log_event(
            "llm",
            "empty_response",
            autonoetic_types::causal_chain::EntryStatus::Error,
            Some(payload.clone()),
        )?;
        if let Some(w) = &self.live_report {
            if let Err(e) = w.lock().unwrap().record_execution_failure(
                "llm.empty_response",
                &format!("model={} stop_reason={} input_tokens={} output_tokens=0", model, stop_reason, input_tokens),
                self.turn_id.as_deref(),
                Some(payload),
                Some(&event_id),
            ) {
                tracing::warn!(
                    target: "session_report",
                    error = %e,
                    "session report record_execution_failure (llm empty) failed"
                );
            }
        }
        self.append_live_digest_event(
            "llm.empty_response",
            Some(serde_json::json!({
                "model": model,
                "stop_reason": stop_reason,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "preceding": Vec::from_iter(self.recent_actions.iter().cloned()),
            })),
        );
        Ok(())
    }

    pub fn log_tool_requested(
        &mut self,
        tool_name: &str,
        arguments: &str,
        intent: Option<&str>,
        call_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let redacted_args = redact_text_for_logs(arguments);
        if tool_name != "digest_annotate" {
            if let Some(w) = &self.live_digest {
                let mut guard = w.lock().unwrap();
                if tool_name == "agent_spawn" {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                        let target = args["agent_id"].as_str().unwrap_or("unknown");
                        let msg = args["message"].as_str().unwrap_or("");
                        let _ = guard.record_delegation_start(target, msg);
                    }
                } else {
                    let line = format_tool_action_line(tool_name, &redacted_args);
                    if let Err(e) = guard.record_action(&line) {
                        tracing::warn!(target: "live_digest", error = %e, "digest record_action failed");
                    }
                }
            }
        }
        if tool_name != "digest_annotate" {
            if let Some(w) = &self.live_report {
                let mut guard = w.lock().unwrap();
                if tool_name == "agent_spawn" {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                        let target = args["agent_id"].as_str().unwrap_or("unknown");
                        let msg = args["message"].as_str().unwrap_or("");
                        let _ = guard.record_delegation_start(target, msg, self.turn_id.as_deref());
                    }
                } else if let Err(e) =
                    guard.record_tool_requested(tool_name, &redacted_args, self.turn_id.as_deref())
                {
                    tracing::warn!(
                        target: "session_report",
                        error = %e,
                        "session report record_tool_requested failed"
                    );
                }
            }
        }
        // `digest_annotate` is internal bookkeeping, excluded from the human-facing
        // digest/report above; keep it out of the failure action-chain too so a
        // failure doesn't render `after: digest_annotate → …`.
        if tool_name != "digest_annotate" {
            self.note_action(tool_name);
        }
        self.append_live_digest_event(
            "tool.requested",
            Some(serde_json::json!({
                "tool_name": tool_name,
                "arguments": redacted_args.clone(),
                "intent": intent,
                // Correlation key: the LLM's tool_call_id. Lets the room pair this
                // request with its (possibly async, far-later) `tool.completed`.
                "call_id": call_id,
            })),
        );
        let mut requested_payload = serde_json::json!({
            "tool_name": tool_name,
            "arguments": redacted_args,
            "arguments_sha256": sha256_hex(arguments)
        });
        if let Some(intent) = intent {
            requested_payload["intent"] = serde_json::json!(intent);
        }
        if let Some(call_id) = call_id {
            requested_payload["call_id"] = serde_json::json!(call_id);
        }
        let requested_evidence = serde_json::json!({
            "tool_name": tool_name,
            "arguments": redacted_args,
            "intent": intent,
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

    pub fn log_tool_completed(&mut self, tool_name: &str, result: &str) -> anyhow::Result<String> {
        self.log_tool_completed_with_approval(tool_name, result, None, None, None)
    }

    pub fn log_tool_completed_with_approval(
        &mut self,
        tool_name: &str,
        result: &str,
        arguments: Option<&str>,
        approval_ref: Option<&str>,
        call_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let parsed_result = serde_json::from_str::<serde_json::Value>(result).ok();
        let mut completed_payload = serde_json::json!({
            "tool_name": tool_name,
            "result_len": result.len(),
            "result_sha256": sha256_hex(result),
            "result_preview": redact_text_for_logs(&truncate_for_log(result, TOOL_RESULT_PREVIEW_MAX_CHARS))
        });
        let args_preview = arguments.and_then(|a| extract_tool_args_preview(tool_name, a))
            .or_else(|| {
                if tool_name == "artifact_build" {
                    parsed_result.as_ref()
                        .and_then(|r| r.get("artifact_ref"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            });
        if let Some(ref preview) = args_preview {
            completed_payload["args_preview"] = serde_json::json!(preview);
        }
        if let Some(call_id) = call_id {
            completed_payload["call_id"] = serde_json::json!(call_id);
        }
        if let Some(enforced_rules) = parsed_result
            .as_ref()
            .and_then(|v| v.get("enforced_rules"))
            .and_then(|v| v.as_array())
        {
            let rule_ids: Vec<String> = enforced_rules
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect();
            if !rule_ids.is_empty() {
                completed_payload["enforced_rules"] = serde_json::json!(rule_ids);
            }
        }
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
        let event_id = self.log_event(
            "tool_invoke",
            "completed",
            EntryStatus::Success,
            Some(completed_payload),
        )?;

        if tool_name != "digest_annotate" {
            if let Some(w) = &self.live_digest {
                let mut guard = w.lock().unwrap();
                let formatted = format_tool_digest_result(tool_name, result);
                let parsed = parsed_result.clone();
                let ok = parsed
                    .as_ref()
                    .and_then(|v| v.get("ok").and_then(|x| x.as_bool()))
                    != Some(false);
                let is_approval_suspension = parsed
                    .as_ref()
                    .and_then(|v| v.get("approval_required").and_then(|x| x.as_bool()))
                    == Some(true);
                let r = if is_approval_suspension {
                    let request_id = parsed
                        .as_ref()
                        .and_then(|v| v.get("request_id").and_then(|x| x.as_str()))
                        .unwrap_or("unknown");
                    let kind = parsed
                        .as_ref()
                        .and_then(|v| {
                            v.get("approval")
                                .and_then(|a| a.get("kind").and_then(|k| k.as_str()))
                        })
                        .unwrap_or("unknown");
                    let summary = parsed
                        .as_ref()
                        .and_then(|v| {
                            v.get("approval")
                                .and_then(|a| a.get("summary").and_then(|s| s.as_str()))
                        })
                        .unwrap_or("Approval required");
                    let reason = parsed
                        .as_ref()
                        .and_then(|v| {
                            v.get("approval")
                                .and_then(|a| a.get("reason").and_then(|r| r.as_str()))
                        })
                        .unwrap_or("Operator approval required");
                    guard.record_approval_pending(request_id, kind, summary, reason)
                } else if let Some(apr_ref) = approval_ref {
                    let decision = parsed
                        .as_ref()
                        .and_then(|v| v.get("decision").and_then(|d| d.as_str()))
                        .unwrap_or("approved");
                    let approved = decision != "denied" && decision != "rejected";
                    guard.record_approval_resolved(apr_ref, approved, &formatted)
                } else if ok {
                    guard.record_result(&formatted)
                } else {
                    guard.record_error(&formatted)
                };
                if let Err(e) = r {
                    tracing::warn!(target: "live_digest", error = %e, "digest record result/error failed");
                }
            }
        }
        {
            if let Some(w) = &self.live_report {
                if let Err(e) = w.lock().unwrap().record_tool_completed(
                    tool_name,
                    result,
                    approval_ref,
                    self.turn_id.as_deref(),
                    Some(&event_id),
                ) {
                    tracing::warn!(
                        target: "session_report",
                        error = %e,
                        "session report record_tool_completed failed"
                    );
                }
            }
        }
        // Room TUI reads the canonical timeline — carry the same args_preview the
        // causal row stores so list rows can show artifact_ref / name / agent_id.
        let mut timeline_payload = serde_json::json!({
            "tool_name": tool_name,
            "result": crate::log_redaction::redact_text_for_logs(result),
        });
        if let Some(preview) = args_preview {
            timeline_payload["args_preview"] = serde_json::json!(preview);
        }
        if let Some(call_id) = call_id {
            timeline_payload["call_id"] = serde_json::json!(call_id);
        }
        // `tool.completed` is Detail by default (success = plumbing). A result
        // with `ok:false` is a failure the operator should see without dialing
        // the floor down, so bump it to Attention. The override only raises.
        let failed = parsed_result
            .as_ref()
            .and_then(|r| r.get("ok"))
            .and_then(|v| v.as_bool())
            .map(|ok| !ok)
            .unwrap_or(false);
        let alt_override = if failed {
            Some(autonoetic_types::session_timeline::Altitude::Attention)
        } else {
            None
        };
        // Lift drill-down handles out of the result into first-class refs so the
        // Room TUI's live content pane can find the artifact a tool built. Without
        // this the "artifact was built" row rendered (from the payload) but the
        // content popup, which keys off `refs.artifact_id`, never saw it — the
        // artifact build result exposes the session-visible `artifact_ref` (some
        // tools use an `artifact_id` key), and the pane passes that ref straight
        // to `artifact.list_files`.
        let refs = {
            let mut r = autonoetic_types::session_timeline::TimelineRefs::default();
            if let Some(res) = parsed_result.as_ref() {
                if let Some(aid) = res
                    .get("artifact_ref")
                    .and_then(|v| v.as_str())
                    .or_else(|| res.get("artifact_id").and_then(|v| v.as_str()))
                {
                    r.artifact_id = Some(aid.to_string());
                }
                if let Some(tid) = res.get("execution_trace_id").and_then(|v| v.as_str()) {
                    r.execution_trace_id = Some(tid.to_string());
                }
            }
            r
        };
        self.append_live_digest_event_at(
            "tool.completed",
            Some(timeline_payload),
            alt_override,
            refs,
        );
        Ok(event_id)
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
        )?;
        Ok(())
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
    target: Option<&str>,
    enforced_rules: &[String],
    payload: Option<crate::log_redaction::RedactedPayload>,
    session_id: &str,
    turn_id: Option<&str>,
    event_seq: u64,
) -> anyhow::Result<()> {
    logger
        .log(
            actor_id,
            session_id,
            turn_id,
            event_seq,
            category,
            action,
            status,
            target,
            enforced_rules,
            payload,
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

/// Extract the witnessed rule set from an event payload — the same shape the
/// DB write has always accepted (`payload.enforced_rules`), now also bound
/// into the JSONL witness hash (I-6). Falls back to the baseline attribution
/// rule when the payload names nothing.
fn enforced_rules_from_payload(payload: Option<&serde_json::Value>) -> Vec<String> {
    payload
        .and_then(|v| v.get("enforced_rules"))
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect::<Vec<String>>()
        })
        .filter(|items| !items.is_empty())
        .unwrap_or_else(autonoetic_types::causal_chain::default_enforced_rules)
}

fn truncate_for_log(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_string();
    }
    let truncated: String = value.chars().take(max_len).collect();
    format!("{}...", truncated)
}

/// Hard cap to `max` chars, ellipsis included (so the result never exceeds
/// `max` — unlike `truncate_for_log`, which appends `...` after the limit).
/// Used for timeline rows after redaction.
fn cap_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let truncated: String = value.chars().take(max - 1).collect();
    format!("{truncated}…")
}

/// Extract a short preview of the key argument for known tools.
/// Returned string is the argument value, which is small (artifact ref, file name,
/// agent id) and does not need truncation.
fn extract_tool_args_preview(tool_name: &str, arguments: &str) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let preview: String = match tool_name {
        "artifact_inspect" => args.get("artifact_ref").and_then(|v| v.as_str())?.to_string(),
        "content_write" | "content_patch" => args.get("name").and_then(|v| v.as_str())?.to_string(),
        "agent_spawn" => args.get("agent_id").and_then(|v| v.as_str())?.to_string(),
        "sandbox_exec" => args.get("command").and_then(|v| v.as_str())?.to_string(),
        // Surface what is actually executed AND where: `<entrypoint> <args> · <artifact_ref>`.
        // The entrypoint+args lead so they survive the length cap; the artifact ref
        // (which tells the operator *which* bundle ran) trails.
        "artifact_exec" => {
            let entrypoint = args.get("entrypoint").and_then(|v| v.as_str())?;
            let call_args = args
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let cmd = if call_args.trim().is_empty() {
                entrypoint.to_string()
            } else {
                format!("{entrypoint} {call_args}")
            };
            match args.get("artifact_ref").and_then(|v| v.as_str()) {
                Some(art) if !art.is_empty() => format!("{cmd} · {art}"),
                _ => cmd,
            }
        }
        _ => return None,
    };
    // Char-based cap (byte slicing could split a multi-byte boundary and panic).
    Some(if preview.chars().count() > 80 {
        let truncated: String = preview.chars().take(79).collect();
        format!("{truncated}…")
    } else {
        preview
    })
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
            live_report: None,
            gateway_store: None,
            recent_actions: std::collections::VecDeque::new(),
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
            live_report: None,
            gateway_store: Some(store),
            recent_actions: std::collections::VecDeque::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn recent_actions_ring_caps_and_clears_on_turn_start() {
        let mut t = SessionTracer::test_tracer();
        for i in 0..(RECENT_ACTIONS_CAP + 3) {
            t.note_action(format!("a{i}"));
        }
        // Capped to the most recent N; oldest dropped, newest kept.
        assert_eq!(t.recent_actions.len(), RECENT_ACTIONS_CAP);
        assert_eq!(
            t.recent_actions.back().unwrap(),
            &format!("a{}", RECENT_ACTIONS_CAP + 2)
        );
        assert_eq!(t.recent_actions.front().unwrap(), "a3");
        // A new turn starts a fresh chain.
        t.start_digest_turn().unwrap();
        assert!(t.recent_actions.is_empty());
    }

    #[test]
    fn digest_annotate_is_excluded_from_action_chain() {
        let mut t = SessionTracer::test_tracer();
        // Internal bookkeeping must not leak into the operator-facing chain.
        t.log_tool_requested("digest_annotate", "{}", None, None).unwrap();
        t.log_tool_requested("read_file", "{}", None, None).unwrap();
        assert_eq!(
            Vec::from_iter(t.recent_actions.iter().cloned()),
            vec!["read_file".to_string()]
        );
    }

    #[test]
    fn cap_chars_is_a_hard_cap_including_ellipsis() {
        let s = "abcdefghij"; // 10 chars
        assert_eq!(cap_chars(s, 10), "abcdefghij"); // exactly fits, untouched
        let out = cap_chars(s, 5);
        assert_eq!(out.chars().count(), 5, "must not exceed max incl. ellipsis");
        assert!(out.ends_with('…'));
        assert_eq!(cap_chars(s, 0), "");
    }

    #[test]
    fn timeline_agent_narrative_cap_is_eight_thousand_chars() {
        assert_eq!(TIMELINE_AGENT_NARRATIVE_MAX_CHARS, 8_000);
    }

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
    fn tool_completed_timeline_includes_args_preview() {
        let temp = tempdir().unwrap();
        let agents_dir = temp.path().join("agents");
        let agent_dir = agents_dir.join("planner.default");
        let gateway_dir = agents_dir.join(".gateway");
        fs::create_dir_all(agent_dir.join("history")).unwrap();
        fs::create_dir_all(&gateway_dir).unwrap();
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );
        let mut tracer = SessionTracer::test_tracer_with_store(&agent_dir, store.clone());
        tracer.set_turn_id("turn-000001");

        tracer
            .log_tool_completed_with_approval(
                "agent_spawn",
                r#"{"accepted":true,"agent_id":"coder.default"}"#,
                Some(r#"{"agent_id":"coder.default","message":"implement feature"}"#),
                None,
                Some("call_abc123"),
            )
            .unwrap();

        let result = store
            .list_session_timeline("test-session", None, 10, None, None)
            .unwrap();
        let completed = result
            .entries
            .iter()
            .find(|e| e.event_type == "tool.completed")
            .expect("tool.completed on timeline");
        let payload: serde_json::Value =
            serde_json::from_str(completed.payload.as_deref().unwrap()).unwrap();
        assert_eq!(
            payload.get("args_preview").and_then(|v| v.as_str()),
            Some("coder.default")
        );
        // The correlation key rides along so the room can pair this completion
        // with its request (possibly async, far earlier on the timeline).
        assert_eq!(
            payload.get("call_id").and_then(|v| v.as_str()),
            Some("call_abc123")
        );
    }

    #[test]
    fn tool_completed_failure_is_bumped_to_attention() {
        // tool.completed is Detail for success, but a result with ok:false is a
        // failure the operator should see without dialing the floor down — so
        // the emit site bumps it to Attention. Verify both halves.
        use autonoetic_types::session_timeline::Altitude;
        let temp = tempdir().unwrap();
        let agents_dir = temp.path().join("agents");
        let agent_dir = agents_dir.join("planner.default");
        let gateway_dir = agents_dir.join(".gateway");
        fs::create_dir_all(agent_dir.join("history")).unwrap();
        fs::create_dir_all(&gateway_dir).unwrap();
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );
        let mut tracer = SessionTracer::test_tracer_with_store(&agent_dir, store.clone());
        tracer.set_turn_id("turn-000001");

        // Success — Detail (folds as routine plumbing).
        tracer
            .log_tool_completed("resolve", r#"{"ok":true,"value":"answer"}"#)
            .unwrap();
        // Failure — bumped to Attention.
        tracer
            .log_tool_completed("sandbox_exec", r#"{"ok":false,"exit_code":1,"stderr":"boom"}"#)
            .unwrap();

        let result = store
            .list_session_timeline("test-session", None, 10, None, None)
            .unwrap();
        let mut seen_success = false;
        let mut seen_failure = false;
        for e in &result.entries {
            if e.event_type != "tool.completed" {
                continue;
            }
            let payload: serde_json::Value =
                serde_json::from_str(e.payload.as_deref().unwrap()).unwrap();
            let ok = payload.get("result")
                .and_then(|r| serde_json::from_str::<serde_json::Value>(r.as_str().unwrap_or("")).ok())
                .and_then(|r| r.get("ok").and_then(|v| v.as_bool()))
                .unwrap_or(true);
            if ok {
                assert_eq!(e.altitude, Altitude::Detail, "success should stay Detail");
                seen_success = true;
            } else {
                assert_eq!(e.altitude, Altitude::Attention, "failure should bump to Attention");
                seen_failure = true;
            }
        }
        assert!(seen_success, "expected a successful tool.completed");
        assert!(seen_failure, "expected a failed tool.completed");
    }

    /// Test harness: a store-backed tracer with a turn id set.
    fn store_tracer() -> (tempfile::TempDir, std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>, SessionTracer) {
        let temp = tempdir().unwrap();
        let agent_dir = temp.path().join("agents").join("planner.default");
        let gateway_dir = temp.path().join("agents").join(".gateway");
        fs::create_dir_all(agent_dir.join("history")).unwrap();
        fs::create_dir_all(&gateway_dir).unwrap();
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );
        let mut tracer = SessionTracer::test_tracer_with_store(&agent_dir, store.clone());
        tracer.set_turn_id("turn-000001");
        (temp, store, tracer)
    }

    fn completed_entry(
        store: &crate::scheduler::gateway_store::GatewayStore,
    ) -> autonoetic_types::session_timeline::SessionTimelineEntry {
        store
            .list_session_timeline("test-session", None, 10, None, None)
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.event_type == "tool.completed")
            .expect("tool.completed on timeline")
    }

    /// Regression (Room TUI live content popup): a built artifact rendered as
    /// "artifact was built" in the timeline but never appeared in the popup,
    /// because tool.completed was emitted with no refs — and the popup keys its
    /// Artifacts section off `refs.artifact_id`. The artifact_build result
    /// exposes the session-visible `artifact_ref`; it must land in refs.
    #[test]
    fn tool_completed_lifts_artifact_ref_into_refs() {
        let (_temp, store, mut tracer) = store_tracer();
        tracer
            .log_tool_completed(
                "artifact_build",
                r#"{"ok":true,"artifact_ref":"ar.session.abc123","artifact_canonical_digest":"sha256:deadbeef"}"#,
            )
            .unwrap();
        let completed = completed_entry(&store);
        assert_eq!(
            completed.refs.artifact_id.as_deref(),
            Some("ar.session.abc123"),
            "artifact_build's artifact_ref must be lifted into refs.artifact_id"
        );
    }

    /// Fallback: a result using an `artifact_id` key (rather than `artifact_ref`)
    /// still populates refs.artifact_id, and `execution_trace_id` is lifted too.
    #[test]
    fn tool_completed_lifts_artifact_id_key_and_trace_id() {
        let (_temp, store, mut tracer) = store_tracer();
        tracer
            .log_tool_completed(
                "artifact_exec",
                r#"{"ok":true,"artifact_id":"art_abcd1234","execution_trace_id":"etr-1"}"#,
            )
            .unwrap();
        let completed = completed_entry(&store);
        assert_eq!(completed.refs.artifact_id.as_deref(), Some("art_abcd1234"));
        assert_eq!(completed.refs.execution_trace_id.as_deref(), Some("etr-1"));
    }

    /// A tool whose result carries no drill-down handle keeps empty refs — no
    /// spurious refs_json, matching the prior behavior for such events.
    #[test]
    fn tool_completed_without_handles_has_empty_refs() {
        let (_temp, store, mut tracer) = store_tracer();
        tracer
            .log_tool_completed("resolve", r#"{"ok":true,"value":"answer"}"#)
            .unwrap();
        assert!(
            completed_entry(&store).refs.is_empty(),
            "no drill-down handle => empty refs"
        );
    }

    #[test]
    fn extract_tool_args_preview_for_known_tools() {
        assert_eq!(
            extract_tool_args_preview(
                "content_write",
                r#"{"name":"skills/weather/SKILL.md","content":"..."}"#
            )
            .as_deref(),
            Some("skills/weather/SKILL.md")
        );
        assert_eq!(
            extract_tool_args_preview(
                "agent_spawn",
                r#"{"agent_id":"researcher.default","message":"find APIs"}"#
            )
            .as_deref(),
            Some("researcher.default")
        );
        assert_eq!(
            extract_tool_args_preview("spawn", r#"{"agent_id":"coder.default"}"#),
            None
        );
        assert_eq!(
            extract_tool_args_preview("sandbox_exec", r#"{"command":"pytest -k foo"}"#)
                .as_deref(),
            Some("pytest -k foo"),
            "sandbox_exec should preview its command, not fall back to 'tool sandbox_exec'"
        );
        assert_eq!(
            extract_tool_args_preview(
                "artifact_exec",
                r#"{"artifact_ref":"ar.abc123","entrypoint":"main.py","args":["--fast","x"]}"#
            )
            .as_deref(),
            Some("main.py --fast x · ar.abc123"),
            "artifact_exec should preview entrypoint + args + which artifact ran"
        );
        assert_eq!(
            extract_tool_args_preview(
                "artifact_exec",
                r#"{"artifact_ref":"ar.abc123","entrypoint":"main.py"}"#
            )
            .as_deref(),
            Some("main.py · ar.abc123"),
            "artifact_exec without args still names the artifact"
        );
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
                "sandbox_exec",
                r#"{"ok":false,"exit_code":1,"stderr":"test failed","stdout":"full output"}"#,
            )
            .unwrap();

        let causal_path = agent_dir.join("history").join("causal_chain.jsonl");
        let causal_log = fs::read_to_string(&causal_path).unwrap();
        assert!(
            !causal_log.contains("test failed"),
            "lean witness must not embed payload text"
        );

        // The evidence pointer lives in the content-addressed payload and
        // must resolve (and hash-verify) from the entry's payload_ref.
        let entries = crate::causal_chain::CausalLogger::read_entries(&causal_path).unwrap();
        let completed = entries
            .iter()
            .rev()
            .find(|e| e.category == "tool_invoke" && e.action == "completed")
            .expect("completed entry should exist");
        let resolved = crate::causal_chain::resolve_entry_payload(&causal_path, completed)
            .expect("payload should resolve and verify")
            .expect("completed entry should reference a payload");
        assert!(
            resolved.get("evidence_ref").is_some(),
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
    fn record_runtime_lock_drift_surfaces_on_timeline() {
        use autonoetic_types::session_timeline::{Altitude, SessionRole};
        let temp = tempdir().unwrap();
        let agents_dir = temp.path().join("agents");
        let agent_dir = agents_dir.join("test-agent");
        let gateway_dir = agents_dir.join(".gateway");
        fs::create_dir_all(agent_dir.join("history")).unwrap();
        fs::create_dir_all(&gateway_dir).unwrap();
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );
        let tracer = SessionTracer::test_tracer_with_store(&agent_dir, store.clone());

        tracer.record_runtime_lock_drift(
            serde_json::json!({ "drift_field": "binary_sha256", "override": false }),
            false,
        );
        tracer.record_runtime_lock_drift(
            serde_json::json!({ "drift_field": "build_sha256", "override": true }),
            true,
        );

        let result = store
            .list_session_timeline("test-session", None, 50, Some(Altitude::Detail), None)
            .unwrap();
        let drifts: Vec<_> = result
            .entries
            .iter()
            .filter(|e| e.event_type == "runtime.lock_drift")
            .collect();
        assert_eq!(drifts.len(), 2, "both drifts must reach the timeline");
        // Attributed to the Runtime seat (the executor's mechanical ruling).
        assert!(drifts.iter().all(|e| matches!(e.role, SessionRole::Runtime)));
        // Rejected ⇒ Error; override ⇒ Attention.
        let rejected = drifts
            .iter()
            .find(|e| {
                e.payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                    .and_then(|v| v.get("override").and_then(|o| o.as_bool()))
                    == Some(false)
            })
            .unwrap();
        assert_eq!(rejected.altitude, Altitude::Error);
        let overridden = drifts
            .iter()
            .find(|e| {
                e.payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                    .and_then(|v| v.get("override").and_then(|o| o.as_bool()))
                    == Some(true)
            })
            .unwrap();
        assert_eq!(overridden.altitude, Altitude::Attention);
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
            "tool_name": "sandbox_exec",
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
            "tool_name": "sandbox_exec",
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
