//! Structured live/session reporting alongside the append-only `digest.md`.
//!
//! This module keeps a compact JSON state file and renders two operator-facing views:
//! - `session_overview.md` — live, agent-centric overview rewritten on each update
//! - `session_report.{md,json}` — final structured report written on session close

use crate::log_redaction::redact_text_for_logs;
use crate::runtime::live_digest::{base_session_id, session_depth};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

static SESSION_REPORT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

const MAX_EVENTS: usize = 400;
const MAX_RECENT_EVENTS_PER_AGENT: usize = 12;
const LARGE_PAYLOAD_THRESHOLD: usize = 500;
const OUTPUT_PREVIEW_DISPLAY_CHARS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionReportState {
    version: u32,
    root_session_id: String,
    status: String,
    started_at: Option<String>,
    ended_at: Option<String>,
    generated_at: String,
    agents: BTreeMap<String, AgentReport>,
    timeline: Vec<ReportEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentReport {
    session_id: String,
    agent_id: String,
    parent_session_id: Option<String>,
    depth: usize,
    started_at: Option<String>,
    ended_at: Option<String>,
    status: String,
    close_reason: Option<String>,
    input_preview: Option<String>,
    output_preview: Option<String>,
    turn_count: u32,
    tool_count: u32,
    error_count: u32,
    approval_count: u32,
    last_event_at: Option<String>,
    last_event_kind: Option<String>,
    last_event_summary: Option<String>,
    approvals: Vec<ApprovalItem>,
    errors: Vec<ErrorItem>,
    artifacts: Vec<ArtifactItem>,
    delegations: Vec<DelegationItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    links: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApprovalItem {
    event_id: Option<String>,
    request_id: String,
    status: String,
    kind: String,
    summary: String,
    reason: Option<String>,
    decision: Option<String>,
    created_at: String,
    resolved_at: Option<String>,
    resolution_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    links: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ErrorItem {
    event_id: Option<String>,
    created_at: String,
    tool_name: Option<String>,
    summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    links: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactItem {
    created_at: String,
    tool_name: String,
    artifact_id: String,
    summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DelegationItem {
    created_at: String,
    target_agent: String,
    task_preview: String,
    output_preview: Option<String>,
    status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReportEvent {
    event_id: Option<String>,
    created_at: String,
    session_id: String,
    agent_id: String,
    turn_id: Option<String>,
    kind: String,
    summary: String,
    important: bool,
    details: Option<Value>,
    payload_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    links: Option<Value>,
}

pub struct SessionReportWriter {
    state_path: PathBuf,
    live_md_path: PathBuf,
    live_html_path: PathBuf,
    final_md_path: PathBuf,
    final_json_path: PathBuf,
    final_html_path: PathBuf,
    report_data_dir: PathBuf,
    session_id: String,
    agent_id: String,
    depth: usize,
    payload_counter: u32,
    content_store: Option<crate::runtime::content_store::ContentStore>,
}

impl SessionReportState {
    fn new(root_session_id: &str) -> Self {
        Self {
            version: 1,
            root_session_id: root_session_id.to_string(),
            status: "running".to_string(),
            started_at: None,
            ended_at: None,
            generated_at: chrono::Utc::now().to_rfc3339(),
            agents: BTreeMap::new(),
            timeline: Vec::new(),
        }
    }
}

impl AgentReport {
    fn new(session_id: &str, agent_id: &str, depth: usize) -> Self {
        Self {
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            parent_session_id: session_id
                .rsplit_once('/')
                .map(|(parent, _)| parent.to_string()),
            depth,
            started_at: None,
            ended_at: None,
            status: "running".to_string(),
            close_reason: None,
            input_preview: None,
            output_preview: None,
            turn_count: 0,
            tool_count: 0,
            error_count: 0,
            approval_count: 0,
            last_event_at: None,
            last_event_kind: None,
            last_event_summary: None,
            approvals: Vec::new(),
            errors: Vec::new(),
            artifacts: Vec::new(),
            delegations: Vec::new(),
            links: None,
        }
    }
}

impl SessionReportWriter {
    pub fn open(gateway_dir: &Path, session_id: &str, agent_id: &str) -> anyhow::Result<Self> {
        let base = base_session_id(session_id);
        let dir = gateway_dir.join("sessions").join(base);
        std::fs::create_dir_all(&dir)?;
        let report_data_dir = dir.join("report-data");
        std::fs::create_dir_all(&report_data_dir)?;
        let payload_counter = std::fs::read_dir(&report_data_dir)
            .map(|entries| entries.filter_map(|e| e.ok()).count())
            .unwrap_or(0) as u32;
        let content_store = crate::runtime::content_store::ContentStore::new(gateway_dir).ok();
        Ok(Self {
            state_path: dir.join("session_report.live.json"),
            live_md_path: dir.join("session_overview.md"),
            live_html_path: dir.join("session_overview.html"),
            final_md_path: dir.join("session_report.md"),
            final_json_path: dir.join("session_report.json"),
            final_html_path: dir.join("session_report.html"),
            report_data_dir: dir.join("report-data"),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            depth: session_depth(session_id),
            payload_counter,
            content_store,
        })
    }

    pub fn start_session(&mut self, task_preview: &str) -> anyhow::Result<()> {
        self.update_state(|state| {
            let now = chrono::Utc::now().to_rfc3339();
            state.status = "running".to_string();
            state.ended_at = None;
            if state.started_at.is_none() {
                state.started_at = Some(now.clone());
            }
            let agent = ensure_agent(state, &self.session_id, &self.agent_id, self.depth);
            if agent.started_at.is_none() {
                agent.started_at = Some(now.clone());
            }
            agent.ended_at = None;
            agent.status = "running".to_string();
            agent.close_reason = None;
            if !task_preview.trim().is_empty() {
                agent.input_preview = Some(truncate_chars(
                    &redact_text_for_logs(task_preview.trim()),
                    600,
                ));
            }
            push_event(
                state,
                ReportEvent {
                    event_id: None,
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: None,
                    kind: "SESSION".to_string(),
                    summary: "session started".to_string(),
                    important: true,
                    details: None,
                    payload_ref: None,
                    links: None,
                },
            );
        })
    }

    pub fn start_turn(&mut self, turn_id: Option<&str>) -> anyhow::Result<()> {
        self.update_state(|state| {
            let now = chrono::Utc::now().to_rfc3339();
            let turn_count = {
                let agent = ensure_agent(state, &self.session_id, &self.agent_id, self.depth);
                agent.turn_count = agent.turn_count.saturating_add(1);
                agent.status = "running".to_string();
                touch_agent(agent, "TURN", "turn started", &now);
                agent.turn_count
            };
            push_event(
                state,
                ReportEvent {
                    event_id: None,
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: "TURN".to_string(),
                    summary: format!("turn {}", turn_count),
                    important: false,
                    details: None,
                    payload_ref: None,
                    links: None,
                },
            );
        })
    }

    pub fn record_annotation(
        &mut self,
        kind: &str,
        content: &str,
        turn_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let content = truncate_chars(&redact_text_for_logs(content.trim()), 600);
        self.update_state(|state| {
            let now = chrono::Utc::now().to_rfc3339();
            let agent = ensure_agent(state, &self.session_id, &self.agent_id, self.depth);
            let summary = format!("{}: {}", kind, content);
            touch_agent(agent, "NOTE", &summary, &now);
            push_event(
                state,
                ReportEvent {
                    event_id: None,
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: "NOTE".to_string(),
                    summary,
                    important: false,
                    details: None,
                    payload_ref: None,
                    links: None,
                },
            );
        })
    }

    /// Records a failure that is not tied to a tool result JSON (e.g. LLM transport/API error).
    pub fn record_execution_failure(
        &mut self,
        component: &str,
        summary: &str,
        turn_id: Option<&str>,
        details: Option<Value>,
        event_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let session_id = self.session_id.clone();
        let agent_id = self.agent_id.clone();
        let depth = self.depth;
        let event_id_owned = event_id.map(String::from);
        self.update_state(|state| {
            let now = chrono::Utc::now().to_rfc3339();
            let redacted = redact_text_for_logs(summary);
            let short = truncate_chars(&redacted, 400);
            let agent = ensure_agent(state, &session_id, &agent_id, depth);
            agent.error_count = agent.error_count.saturating_add(1);
            agent.errors.push(ErrorItem {
                event_id: event_id_owned.clone(),
                created_at: now.clone(),
                tool_name: Some(component.to_string()),
                summary: short.clone(),
                links: None,
            });
            touch_agent(agent, "ERROR", &short, &now);
            push_event(
                state,
                ReportEvent {
                    event_id: event_id_owned,
                    created_at: now,
                    session_id: session_id.clone(),
                    agent_id: agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: "ERROR".to_string(),
                    summary: short,
                    important: true,
                    details,
                    payload_ref: None,
                    links: None,
                },
            );
        })
    }

    pub fn record_tool_requested(
        &mut self,
        tool_name: &str,
        arguments_redacted: &str,
        turn_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.update_state(|state| {
            let now = chrono::Utc::now().to_rfc3339();
            let agent = ensure_agent(state, &self.session_id, &self.agent_id, self.depth);
            agent.tool_count = agent.tool_count.saturating_add(1);
            let summary = summarize_tool_request(tool_name, arguments_redacted);
            let kind = if is_poll_tool(tool_name) {
                "POLL"
            } else {
                "ACTION"
            };
            touch_agent(agent, kind, &summary, &now);
            push_event(
                state,
                ReportEvent {
                    event_id: None,
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: kind.to_string(),
                    summary,
                    important: !is_poll_tool(tool_name),
                    details: parse_truncated_json(arguments_redacted, 200),
                    payload_ref: None,
                    links: None,
                },
            );
        })
    }

    pub fn record_tool_completed(
        &mut self,
        tool_name: &str,
        result_json: &str,
        approval_ref: Option<&str>,
        turn_id: Option<&str>,
        event_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let save_payload = result_json.len() > LARGE_PAYLOAD_THRESHOLD;
        let session_id = self.session_id.clone();
        let agent_id = self.agent_id.clone();
        let depth = self.depth;
        let mut counter = self.payload_counter;
        let mut payload_filename: Option<String> = None;
        let event_id_owned = event_id.map(String::from);
        self.update_state(|state| {
            let now = chrono::Utc::now().to_rfc3339();
            let parsed = serde_json::from_str::<Value>(result_json).ok();
            let is_approval = parsed
                .as_ref()
                .and_then(|v| v.get("approval_required").and_then(|x| x.as_bool()))
                == Some(true);
            let mut ok = parsed
                .as_ref()
                .and_then(|v| v.get("ok").and_then(|x| x.as_bool()))
                != Some(false);
            if tool_name == "workflow_wait" {
                if parsed
                    .as_ref()
                    .and_then(|v| v.get("any_failed").and_then(|x| x.as_bool()))
                    == Some(true)
                {
                    ok = false;
                }
            }

            let (kind, important, summary, details, payload_ref) = if is_approval {
                (
                    "APPROVAL",
                    true,
                    summarize_approval(parsed.as_ref()),
                    parsed.as_ref().map(|v| truncate_json(v, 200)),
                    None,
                )
            } else if approval_ref.is_some() {
                (
                    "APPROVAL",
                    true,
                    format!(
                        "approval `{}` resolved: {}",
                        approval_ref.unwrap_or("unknown"),
                        summarize_tool_result(tool_name, parsed.as_ref(), result_json)
                    ),
                    parsed.as_ref().map(|v| truncate_json(v, 200)),
                    None,
                )
            } else if !ok {
                (
                    "ERROR",
                    true,
                    summarize_tool_error(tool_name, parsed.as_ref(), result_json),
                    parsed.as_ref().map(|v| truncate_json(v, 200)),
                    None,
                )
            } else if is_poll_tool(tool_name) {
                let summary = summarize_tool_result(tool_name, parsed.as_ref(), result_json);
                let important = poll_result_is_important(tool_name, parsed.as_ref());
                (
                    "POLL",
                    important,
                    summary,
                    parsed.as_ref().map(|v| truncate_json(v, 200)),
                    None,
                )
            } else {
                let summary = summarize_tool_result(tool_name, parsed.as_ref(), result_json);
                let payload_ref = if save_payload {
                    counter += 1;
                    if let Some(ref cs) = self.content_store {
                        let payload = serde_json::json!({
                            "tool": tool_name,
                            "turn_id": turn_id,
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                            "raw_result": result_json,
                        });
                        match serde_json::to_string_pretty(&payload) {
                            Ok(body) => match cs.write(body.as_bytes()) {
                                Ok(handle) => Some(handle),
                                Err(_) => None,
                            },
                            Err(_) => None,
                        }
                    } else {
                        let filename = format!(
                            "event-{}-{}-{}.json",
                            counter,
                            tool_name.replace('.', "_"),
                            turn_id.unwrap_or("none"),
                        );
                        payload_filename = Some(filename.clone());
                        Some(filename)
                    }
                } else {
                    None
                };
                (
                    "RESULT",
                    true,
                    summary,
                    parsed.as_ref().map(|v| truncate_json(v, 200)),
                    payload_ref,
                )
            };

            let agent = ensure_agent(state, &session_id, &agent_id, depth);
            touch_agent(agent, kind, &summary, &now);

            if is_approval {
                agent.status = "awaiting_approval".to_string();
                agent.approval_count = agent.approval_count.saturating_add(1);
                upsert_approval(agent, parsed.as_ref(), &now);
            } else if let Some(request_id) = approval_ref {
                resolve_approval(agent, request_id, "approved", &summary, &now);
                if agent.status == "awaiting_approval" {
                    agent.status = "running".to_string();
                }
            } else if !ok {
                agent.error_count = agent.error_count.saturating_add(1);
                agent.errors.push(ErrorItem {
                    event_id: event_id_owned.clone(),
                    created_at: now.clone(),
                    tool_name: Some(tool_name.to_string()),
                    summary: summary.to_string(),
                    links: None,
                });
            } else {
                maybe_record_output(agent, tool_name, parsed.as_ref(), &summary, &now);
            }

            push_event(
                state,
                ReportEvent {
                    event_id: event_id_owned,
                    created_at: now,
                    session_id: session_id.clone(),
                    agent_id: agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: kind.to_string(),
                    summary: summary.to_string(),
                    important,
                    details,
                    payload_ref,
                    links: None,
                },
            );
        })?;
        self.payload_counter = counter;
        if let Some(filename) = payload_filename {
            if self.content_store.is_none() {
                let payload = serde_json::json!({
                    "tool": tool_name,
                    "turn_id": turn_id,
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "raw_result": result_json,
                });
                if let Ok(body) = serde_json::to_string_pretty(&payload) {
                    let _ = std::fs::write(self.report_data_dir.join(&filename), body);
                }
            }
        }
        Ok(())
    }

    pub fn record_approval_resolved(
        &mut self,
        request_id: &str,
        decision: &str,
        summary: &str,
    ) -> anyhow::Result<()> {
        self.update_state(|state| {
            let now = chrono::Utc::now().to_rfc3339();
            if let Some(agent) = state.agents.get_mut(&self.session_id) {
                resolve_approval(agent, request_id, decision, summary, &now);
                if agent.status == "awaiting_approval" && decision == "approved" {
                    agent.status = "running".to_string();
                }
            }
            push_event(
                state,
                ReportEvent {
                    event_id: None,
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: None,
                    kind: "APPROVAL".to_string(),
                    summary: format!(
                        "approval `{}` {}: {}",
                        request_id,
                        decision,
                        truncate_chars(summary, 140)
                    ),
                    important: true,
                    details: None,
                    payload_ref: None,
                    links: None,
                },
            );
        })
    }

    pub fn record_delegation_start(
        &mut self,
        target_agent: &str,
        task_preview: &str,
        turn_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let preview = truncate_chars(&redact_text_for_logs(task_preview), 400);
        self.update_state(|state| {
            let now = chrono::Utc::now().to_rfc3339();
            let agent = ensure_agent(state, &self.session_id, &self.agent_id, self.depth);
            let summary = format!("delegated to `{}`: {}", target_agent, preview);
            touch_agent(agent, "DELEGATE", &summary, &now);
            agent.delegations.push(DelegationItem {
                created_at: now.clone(),
                target_agent: target_agent.to_string(),
                task_preview: preview.clone(),
                output_preview: None,
                status: "started".to_string(),
            });
            push_event(
                state,
                ReportEvent {
                    event_id: None,
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: "DELEGATE".to_string(),
                    summary,
                    important: true,
                    details: None,
                    payload_ref: None,
                    links: None,
                },
            );
        })
    }

    pub fn finish_session(
        &mut self,
        reason: &str,
        latest_assistant_output: Option<&str>,
    ) -> anyhow::Result<()> {
        self.update_state_and_finalize(|state| {
            let now = chrono::Utc::now().to_rfc3339();
            let status = status_from_close_reason(reason);
            state.status = status.to_string();
            state.ended_at = Some(now.clone());
            let agent = ensure_agent(state, &self.session_id, &self.agent_id, self.depth);
            agent.status = status.to_string();
            agent.ended_at = Some(now.clone());
            agent.close_reason = Some(reason.to_string());
            if let Some(text) = latest_assistant_output
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                agent.output_preview = Some(truncate_chars(&redact_text_for_logs(text), 800));
            }
            touch_agent(
                agent,
                "FINAL",
                &format!("session closed: {}", truncate_chars(reason, 200)),
                &now,
            );
            push_event(
                state,
                ReportEvent {
                    event_id: None,
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: None,
                    kind: "FINAL".to_string(),
                    summary: format!("session closed: {}", truncate_chars(reason, 200)),
                    important: true,
                    details: latest_assistant_output
                        .map(|s| Value::String(truncate_chars(&redact_text_for_logs(s), 800))),
                    payload_ref: None,
                    links: None,
                },
            );
        })
    }

    fn update_state<F>(&self, f: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut SessionReportState),
    {
        let _guard = SESSION_REPORT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut state = self.load_state()?;
        f(&mut state);
        state.generated_at = chrono::Utc::now().to_rfc3339();
        let live_md = render_live_markdown(&state);
        let live_html = render_live_html(&state);
        write_json_atomic(&self.state_path, &state)?;
        write_string_atomic(&self.live_md_path, &live_md)?;
        write_string_atomic(&self.live_html_path, &live_html)?;
        Ok(())
    }

    fn update_state_and_finalize<F>(&self, f: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut SessionReportState),
    {
        let _guard = SESSION_REPORT_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut state = self.load_state()?;
        f(&mut state);
        state.generated_at = chrono::Utc::now().to_rfc3339();
        attach_links(&mut state);
        let live_md = render_live_markdown(&state);
        let live_html = render_live_html(&state);
        let final_md = render_final_markdown(&state);
        let final_html = render_html_report(&state);
        write_json_atomic(&self.state_path, &state)?;
        write_string_atomic(&self.live_md_path, &live_md)?;
        write_string_atomic(&self.live_html_path, &live_html)?;
        write_json_atomic(&self.final_json_path, &state)?;
        write_string_atomic(&self.final_md_path, &final_md)?;
        write_string_atomic(&self.final_html_path, &final_html)?;
        Ok(())
    }

    fn load_state(&self) -> anyhow::Result<SessionReportState> {
        if !self.state_path.exists() {
            return Ok(SessionReportState::new(base_session_id(&self.session_id)));
        }
        match std::fs::read_to_string(&self.state_path)
            .ok()
            .and_then(|s| serde_json::from_str::<SessionReportState>(&s).ok())
        {
            Some(state) => Ok(state),
            None => Ok(SessionReportState::new(base_session_id(&self.session_id))),
        }
    }
}

fn ensure_agent<'a>(
    state: &'a mut SessionReportState,
    session_id: &str,
    agent_id: &str,
    depth: usize,
) -> &'a mut AgentReport {
    state
        .agents
        .entry(session_id.to_string())
        .or_insert_with(|| AgentReport::new(session_id, agent_id, depth))
}

fn push_event(state: &mut SessionReportState, event: ReportEvent) {
    state.timeline.push(event);
    if state.timeline.len() > MAX_EVENTS {
        let drop_n = state.timeline.len().saturating_sub(MAX_EVENTS);
        state.timeline.drain(0..drop_n);
    }
}

fn obs_uri(root: &str, tail: &str) -> String {
    format!("autonoetic://observability/roots/{}/{}", root, tail)
}

fn encoded_session(sid: &str) -> String {
    sid.replace('/', "%2F")
}

fn attach_links(state: &mut SessionReportState) {
    let root = &state.root_session_id;

    for agent in state.agents.values_mut() {
        let enc = encoded_session(&agent.session_id);
        agent.links = Some(serde_json::json!({
            "self": obs_uri(root, &format!("report/agents/{}", enc)),
            "session": obs_uri(root, &format!("sessions/{}", enc)),
            "causal": obs_uri(root, &format!("sessions/{}/causal", enc)),
            "traces": obs_uri(root, &format!("sessions/{}/traces", enc)),
        }));

        for err in &mut agent.errors {
            let mut links = serde_json::json!({
                "session": obs_uri(root, &format!("sessions/{}", enc)),
            });
            if let Some(ref eid) = err.event_id {
                links["causal"] =
                    serde_json::json!(obs_uri(root, &format!("sessions/{}/causal/{}", enc, eid)));
                links["self"] = serde_json::json!(obs_uri(root, &format!("report/errors/{}", eid)));
            }
            err.links = Some(links);
        }

        for appr in &mut agent.approvals {
            let mut links = serde_json::json!({
                "self": obs_uri(root, &format!("report/approvals/{}", appr.request_id)),
                "session": obs_uri(root, &format!("sessions/{}", enc)),
            });
            if let Some(ref eid) = appr.event_id {
                links["causal"] =
                    serde_json::json!(obs_uri(root, &format!("sessions/{}/causal/{}", enc, eid)));
            }
            appr.links = Some(links);
        }
    }

    for ev in &mut state.timeline {
        if ev.event_id.is_none() {
            continue;
        }
        let enc = encoded_session(&ev.session_id);
        let eid = ev.event_id.as_ref().unwrap();
        let mut links = serde_json::json!({
            "self": obs_uri(root, &format!("report/timeline/{}", eid)),
            "session": obs_uri(root, &format!("sessions/{}", enc)),
            "causal": obs_uri(root, &format!("sessions/{}/causal/{}", enc, eid)),
        });
        if ev.kind == "TOOL_COMPLETE" || ev.kind == "TOOL_ERROR" {
            links["trace_collection"] =
                serde_json::json!(obs_uri(root, &format!("sessions/{}/traces", enc)));
        }
        ev.links = Some(links);
    }
}

fn touch_agent(agent: &mut AgentReport, kind: &str, summary: &str, timestamp: &str) {
    agent.last_event_at = Some(timestamp.to_string());
    agent.last_event_kind = Some(kind.to_string());
    agent.last_event_summary = Some(truncate_chars(summary, 500));
}

fn summarize_tool_request(tool_name: &str, arguments_redacted: &str) -> String {
    let parsed = serde_json::from_str::<Value>(arguments_redacted).ok();
    match tool_name {
        "sandbox_exec" => format!(
            "run {}",
            extract_field_str(parsed.as_ref(), &["command"]).unwrap_or("command")
        ),
        "web_search" => format!(
            "search {}",
            extract_field_str(parsed.as_ref(), &["query", "q"]).unwrap_or("query")
        ),
        "web_fetch" => format!(
            "fetch {}",
            extract_field_str(parsed.as_ref(), &["url"]).unwrap_or("url")
        ),
        "workflow_wait" => {
            let tasks = parsed
                .as_ref()
                .and_then(|v| v.get("task_ids").and_then(|x| x.as_array()))
                .map(|a| a.len())
                .unwrap_or(0);
            let timeout = parsed
                .as_ref()
                .and_then(|v| v.get("timeout_secs").and_then(|x| x.as_u64()))
                .unwrap_or(0);
            format!("wait on {} task(s) for {}s", tasks, timeout)
        }
        "workflow_state" => "refresh workflow state".to_string(),
        "content_write" => format!(
            "write {}",
            extract_field_str(parsed.as_ref(), &["name"]).unwrap_or("content")
        ),
        _ => format!(
            "{} {}",
            tool_name,
            truncate_chars(&redact_text_for_logs(arguments_redacted), 160)
        ),
    }
}

fn summarize_tool_result(tool_name: &str, parsed: Option<&Value>, raw: &str) -> String {
    match tool_name {
        "workflow_wait" => summarize_workflow_wait(parsed),
        "workflow_state" => summarize_workflow_state(parsed),
        "artifact_build" | "artifact_inspect" => {
            let id = extract_field_str(parsed, &["artifact_id", "id"]).unwrap_or("artifact");
            let files = parsed
                .and_then(|v| v.get("files").and_then(|x| x.as_array()))
                .map(|a| a.len())
                .unwrap_or(0);
            if files > 0 {
                format!("{} `{}` ({} file(s))", tool_name, id, files)
            } else {
                format!("{} `{}`", tool_name, id)
            }
        }
        "content_write" => format!(
            "wrote {}",
            extract_field_str(parsed, &["name", "sandbox_path", "handle"]).unwrap_or("content")
        ),
        "content_read" => {
            if let Some(len) = parsed
                .and_then(|v| v.get("content"))
                .and_then(|x| x.as_str())
                .map(str::len)
            {
                format!("read {} bytes", len)
            } else {
                "read content".to_string()
            }
        }
        "web_search" => {
            let query = extract_field_str(parsed, &["query"]).unwrap_or("query");
            let count = parsed
                .and_then(|v| v.get("result_count").and_then(|x| x.as_u64()))
                .unwrap_or(0);
            format!(
                "search `{}` -> {} result(s)",
                truncate_chars(query, 80),
                count
            )
        }
        "web_fetch" => {
            let url = extract_field_str(parsed, &["url"]).unwrap_or("url");
            let status = parsed
                .and_then(|v| v.get("status_code").and_then(|x| x.as_u64()))
                .unwrap_or(0);
            let truncated = parsed
                .and_then(|v| v.get("truncated").and_then(|x| x.as_bool()))
                .unwrap_or(false);
            if truncated {
                format!(
                    "fetch `{}` -> {} (truncated)",
                    truncate_chars(url, 80),
                    status
                )
            } else {
                format!("fetch `{}` -> {}", truncate_chars(url, 80), status)
            }
        }
        "sandbox_exec" => {
            let exit = parsed
                .and_then(|v| v.get("exit_code").and_then(|x| x.as_i64()))
                .unwrap_or(-1);
            let stdout = parsed
                .and_then(|v| v.get("stdout").and_then(|x| x.as_str()))
                .map(|s| truncate_chars(s.trim(), 100))
                .filter(|s| !s.is_empty());
            if let Some(stdout) = stdout {
                format!(
                    "command exit={} stdout=`{}`",
                    exit,
                    redact_text_for_logs(&stdout)
                )
            } else {
                format!("command exit={}", exit)
            }
        }
        _ => {
            if let Some(v) = parsed {
                if let Some(msg) = v.get("message").and_then(|x| x.as_str()) {
                    return truncate_chars(&redact_text_for_logs(msg), 160);
                }
                if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                    return format!("{} `{}`", tool_name, truncate_chars(id, 80));
                }
            }
            truncate_chars(&redact_text_for_logs(raw), 160)
        }
    }
}

fn summarize_workflow_wait(parsed: Option<&Value>) -> String {
    let Some(v) = parsed else {
        return "workflow wait updated".to_string();
    };
    let waited = v
        .get("waited_secs")
        .and_then(|x| x.as_u64())
        .unwrap_or_default();
    let any_failed = v
        .get("any_failed")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let join_satisfied = v
        .get("join_satisfied")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let tasks = v
        .get("tasks")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if join_satisfied {
        return format!("workflow wait satisfied after {}s", waited);
    }
    if any_failed {
        let failed = v
            .get("failed_task_count")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        return format!("workflow wait: {} failed task(s)", failed);
    }
    let blockers: Vec<String> = tasks
        .iter()
        .filter_map(|task| {
            let status = task.get("status").and_then(|x| x.as_str()).unwrap_or("");
            if status == "AwaitingApproval"
                || status == "Running"
                || status == "Runnable"
                || status == "Paused"
            {
                let agent = task
                    .get("agent_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("agent");
                let summary = task
                    .get("result_summary")
                    .and_then(|x| x.as_str())
                    .unwrap_or(status);
                Some(format!("{} {}", agent, truncate_chars(summary, 80)))
            } else {
                None
            }
        })
        .collect();
    if !blockers.is_empty() {
        format!("workflow wait after {}s: {}", waited, blockers.join("; "))
    } else {
        format!("workflow wait after {}s", waited)
    }
}

fn summarize_workflow_state(parsed: Option<&Value>) -> String {
    let Some(v) = parsed else {
        return "workflow state refreshed".to_string();
    };
    let completed = v
        .get("completed_tasks")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let active = v
        .get("active_tasks")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let pending = v
        .get("pending_approvals")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let failed = v
        .get("failed_task_count")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    format!(
        "workflow state: {} completed, {} active, {} pending approval, {} failed",
        completed, active, pending, failed
    )
}

fn summarize_tool_error(tool_name: &str, parsed: Option<&Value>, raw: &str) -> String {
    if tool_name == "workflow_wait" {
        return summarize_workflow_wait(parsed);
    }
    if let Some(v) = parsed {
        if let Some(stderr) = v.get("stderr").and_then(|x| x.as_str()) {
            let s = truncate_chars(stderr.trim(), 160);
            if !s.is_empty() {
                return format!("{} failed: {}", tool_name, redact_text_for_logs(&s));
            }
        }
        if let Some(msg) = v.get("message").and_then(|x| x.as_str()) {
            return format!(
                "{} failed: {}",
                tool_name,
                truncate_chars(&redact_text_for_logs(msg), 160)
            );
        }
        if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
            return format!(
                "{} failed: {}",
                tool_name,
                truncate_chars(&redact_text_for_logs(err), 160)
            );
        }
    }
    format!(
        "{} failed: {}",
        tool_name,
        truncate_chars(&redact_text_for_logs(raw), 160)
    )
}

fn summarize_approval(parsed: Option<&Value>) -> String {
    let Some(v) = parsed else {
        return "approval required".to_string();
    };
    let request_id = v
        .get("request_id")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown");
    let summary = v
        .get("approval")
        .and_then(|a| a.get("summary").and_then(|s| s.as_str()))
        .unwrap_or("approval required");
    format!(
        "approval `{}` pending: {}",
        request_id,
        truncate_chars(&redact_text_for_logs(summary), 140)
    )
}

fn upsert_approval(agent: &mut AgentReport, parsed: Option<&Value>, now: &str) {
    let Some(v) = parsed else {
        return;
    };
    let request_id = v
        .get("request_id")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown");
    let summary = v
        .get("approval")
        .and_then(|a| a.get("summary").and_then(|s| s.as_str()))
        .unwrap_or("approval required");
    let kind = v
        .get("approval")
        .and_then(|a| a.get("kind").and_then(|k| k.as_str()))
        .unwrap_or("unknown");
    let reason = v
        .get("approval")
        .and_then(|a| a.get("reason").and_then(|r| r.as_str()))
        .map(|s| truncate_chars(&redact_text_for_logs(s), 200));
    if let Some(existing) = agent
        .approvals
        .iter_mut()
        .find(|approval| approval.request_id == request_id)
    {
        existing.status = "pending".to_string();
        existing.summary = truncate_chars(&redact_text_for_logs(summary), 140);
        existing.reason = reason;
        return;
    }
    agent.approvals.push(ApprovalItem {
        event_id: None,
        request_id: request_id.to_string(),
        status: "pending".to_string(),
        kind: kind.to_string(),
        summary: truncate_chars(&redact_text_for_logs(summary), 140),
        reason,
        decision: None,
        created_at: now.to_string(),
        resolved_at: None,
        resolution_summary: None,
        links: None,
    });
}

fn resolve_approval(
    agent: &mut AgentReport,
    request_id: &str,
    decision: &str,
    summary: &str,
    now: &str,
) {
    if let Some(existing) = agent
        .approvals
        .iter_mut()
        .find(|approval| approval.request_id == request_id)
    {
        existing.status = "resolved".to_string();
        existing.decision = Some(decision.to_string());
        existing.resolved_at = Some(now.to_string());
        existing.resolution_summary = Some(truncate_chars(summary, 200));
    }
}

fn maybe_record_output(
    agent: &mut AgentReport,
    tool_name: &str,
    parsed: Option<&Value>,
    summary: &str,
    now: &str,
) {
    match tool_name {
        "artifact_build" | "artifact_inspect" => {
            if let Some(artifact_id) = extract_field_str(parsed, &["artifact_id", "id"]) {
                if !agent.artifacts.iter().any(|a| a.artifact_id == artifact_id) {
                    agent.artifacts.push(ArtifactItem {
                        created_at: now.to_string(),
                        tool_name: tool_name.to_string(),
                        artifact_id: artifact_id.to_string(),
                        summary: truncate_chars(summary, 160),
                    });
                }
                agent.output_preview = Some(format!("artifact `{}`", artifact_id));
            }
        }
        "content_write" => {
            agent.output_preview = Some(truncate_chars(summary, 160));
        }
        "knowledge_store" => {
            if let Some(id) = extract_field_str(parsed, &["id"]) {
                agent.output_preview = Some(format!("stored knowledge `{}`", id));
            }
        }
        "sandbox_exec" | "web_search" | "web_fetch" => {
            agent.output_preview = Some(truncate_chars(summary, 180));
        }
        _ => {}
    }
}

fn is_poll_tool(tool_name: &str) -> bool {
    matches!(tool_name, "workflow_wait" | "workflow_state")
}

fn poll_result_is_important(tool_name: &str, parsed: Option<&Value>) -> bool {
    match tool_name {
        "workflow_wait" => {
            let Some(v) = parsed else {
                return false;
            };
            v.get("any_failed").and_then(|x| x.as_bool()) == Some(true)
                || v.get("join_satisfied").and_then(|x| x.as_bool()) == Some(true)
                || v.get("tasks")
                    .and_then(|x| x.as_array())
                    .map(|tasks| {
                        tasks.iter().any(|task| {
                            matches!(
                                task.get("status").and_then(|x| x.as_str()),
                                Some("AwaitingApproval" | "Failed" | "Cancelled" | "Aborted")
                            )
                        })
                    })
                    .unwrap_or(false)
        }
        "workflow_state" => {
            let Some(v) = parsed else {
                return false;
            };
            v.get("failed_task_count")
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
                > 0
                || v.get("pending_approvals")
                    .and_then(|x| x.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
        }
        _ => false,
    }
}

fn render_live_markdown(state: &SessionReportState) -> String {
    let mut out = String::new();
    let open_blockers = collect_open_blockers(state, false);
    let turn_nums = timeline_turn_numbers(state);
    let recent_events: Vec<(u32, &ReportEvent)> = state
        .timeline
        .iter()
        .enumerate()
        .filter(|(_, e)| e.important)
        .map(|(i, e)| (turn_nums[i], e))
        .rev()
        .take(16)
        .collect();

    let _ = writeln!(out, "# Session overview: `{}`", state.root_session_id);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "_Auto-updated structured view. Narrative log remains in `digest.md`._"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "| Field | Value |");
    let _ = writeln!(out, "|---|---|");
    let total_turns: u32 = state.agents.values().map(|a| a.turn_count).sum();
    let _ = writeln!(out, "| Status | `{}` |", state.status);
    let _ = writeln!(
        out,
        "| Started | {} |",
        state.started_at.as_deref().unwrap_or("—")
    );
    let _ = writeln!(out, "| Total turns | {} |", total_turns);
    let _ = writeln!(
        out,
        "| Duration | {} |",
        format_duration(state.started_at.as_deref(), Some(&state.generated_at))
    );
    let _ = writeln!(out, "| Last updated | {} |", state.generated_at);
    let _ = writeln!(out);
    {
        let agents_with_errors = state.agents.values().filter(|a| a.error_count > 0).count();
        let awaiting = state
            .agents
            .values()
            .filter(|a| a.status == "awaiting_approval")
            .count();
        let failed = state
            .agents
            .values()
            .filter(|a| a.status == "failed")
            .count();
        let abnormal_exits = state
            .agents
            .values()
            .filter(|a| a.close_reason.as_deref().map_or(false, is_abnormal_close))
            .count();
        let health_line = if failed > 0 || agents_with_errors > 0 || abnormal_exits > 0 {
            format!(
                "> **[ISSUES]** {} agent(s) with errors | {} failed | {} awaiting approval | {} abnormal exit(s)",
                agents_with_errors, failed, awaiting, abnormal_exits
            )
        } else if awaiting > 0 {
            format!("> **[WAITING]** {} agent(s) awaiting approval", awaiting)
        } else {
            "> **[OK]** No issues detected".to_string()
        };
        let _ = writeln!(out, "{}", health_line);
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Active Agents");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| Agent | Session | Parent | Status | Turns | Err% | Started | Last Event | Output | Errors |"
    );
    let _ = writeln!(out, "|---|---|---|---|---:|---:|---|---|---|---:|");
    for (depth, agent) in agents_by_tree(state, None) {
        let indent = "  ".repeat(depth);
        let err_pct = if agent.tool_count == 0 {
            "—".to_string()
        } else {
            format!("{}%", agent.error_count * 100 / agent.tool_count)
        };
        let _ = writeln!(
            out,
            "| `{}{}` | `{}` | `{}` | `{}` | {} | {} | {} | {} | {} | {} |",
            indent,
            agent.agent_id,
            short_session_id(&agent.session_id),
            agent
                .parent_session_id
                .as_deref()
                .map(short_session_id)
                .unwrap_or("—"),
            agent.status,
            agent.turn_count,
            err_pct,
            format_timestamp(agent.started_at.as_deref()),
            truncate_chars(
                agent.last_event_summary.as_deref().unwrap_or("—"),
                OUTPUT_PREVIEW_DISPLAY_CHARS
            ),
            truncate_chars(
                agent.output_preview.as_deref().unwrap_or("—"),
                OUTPUT_PREVIEW_DISPLAY_CHARS
            ),
            agent.error_count
        );
    }
    let _ = writeln!(out);
    let error_blockers: Vec<_> = open_blockers.iter().filter(|b| b.2 == "error").collect();
    let _ = writeln!(out, "## Open Errors");
    let _ = writeln!(out);
    if error_blockers.is_empty() {
        let _ = writeln!(out, "_(none)_");
    } else {
        let _ = writeln!(out, "| Time | Agent | Summary |");
        let _ = writeln!(out, "|---|---|---|");
        for blocker in &error_blockers {
            let _ = writeln!(
                out,
                "| {} | `{}` | {} |",
                format_timestamp(Some(&blocker.0)),
                blocker.1,
                truncate_chars(&blocker.3, OUTPUT_PREVIEW_DISPLAY_CHARS)
            );
        }
    }
    let _ = writeln!(out);

    let pending_approvals: Vec<_> = collect_all_approvals(state)
        .into_iter()
        .filter(|a| a.status == "pending")
        .collect();
    let _ = writeln!(out, "## Open Approvals");
    let _ = writeln!(out);
    if pending_approvals.is_empty() {
        let _ = writeln!(out, "_(none)_");
    } else {
        let _ = writeln!(
            out,
            "| Time | Agent | Request ID | Kind | Summary | Reason |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|");
        for a in &pending_approvals {
            let _ = writeln!(
                out,
                "| {} | `{}` | `{}` | {} | {} | {} |",
                format_timestamp(Some(&a.created_at)),
                a.agent_id,
                a.request_id,
                a.kind,
                truncate_chars(&a.summary, OUTPUT_PREVIEW_DISPLAY_CHARS),
                a.reason.as_deref().unwrap_or("—"),
            );
        }
    }
    let _ = writeln!(out);
    {
        let abnormal: Vec<_> = state
            .agents
            .values()
            .filter(|a| a.close_reason.as_deref().map_or(false, is_abnormal_close))
            .collect();
        let _ = writeln!(out, "## Abnormal Exits");
        let _ = writeln!(out);
        if abnormal.is_empty() {
            let _ = writeln!(out, "_(none)_");
        } else {
            let _ = writeln!(out, "| Agent | Session | Status | Close Reason |");
            let _ = writeln!(out, "|---|---|---|---|");
            for agent in &abnormal {
                let _ = writeln!(
                    out,
                    "| `{}` | `{}` | `{}` | {} |",
                    agent.agent_id,
                    short_session_id(&agent.session_id),
                    agent.status,
                    truncate_chars(agent.close_reason.as_deref().unwrap_or("—"), 200)
                );
            }
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Recent Important Events");
    let _ = writeln!(out);
    if recent_events.is_empty() {
        let _ = writeln!(out, "_(none yet)_");
    } else {
        let _ = writeln!(out, "| Time | T# | Session | Agent | Kind | Summary |");
        let _ = writeln!(out, "|---|---|---|---|---|---|");
        for (turn_num, event) in recent_events.into_iter().rev() {
            let _ = writeln!(
                out,
                "| {} | T{} | `{}` | `{}` | {} | {} |",
                format_timestamp(Some(&event.created_at)),
                turn_num,
                short_session_id(&event.session_id),
                event.agent_id,
                event.kind,
                truncate_chars(&event.summary, OUTPUT_PREVIEW_DISPLAY_CHARS)
            );
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Agent Details");
    let _ = writeln!(out);
    for (_depth, agent) in agents_by_tree(state, None) {
        let turn_range_str = match agent_turn_range(state, &agent.session_id, &turn_nums) {
            Some((f, l)) if f == l => format!(" (T{})", f),
            Some((f, l)) => format!(" (T{}–T{})", f, l),
            None => String::new(),
        };
        let _ = writeln!(
            out,
            "### {} `{}`{}",
            agent.agent_id,
            short_session_id(&agent.session_id),
            turn_range_str
        );
        let _ = writeln!(out);
        {
            let streak = consecutive_error_streak(state, &agent.session_id);
            let avg_turn = avg_turn_secs(state, &agent.session_id)
                .map(|s| format!("{}s", s))
                .unwrap_or_else(|| "—".to_string());
            let err_rate = if agent.tool_count == 0 {
                "—".to_string()
            } else {
                format!("{}%", agent.error_count * 100 / agent.tool_count)
            };
            let _ = writeln!(
                out,
                "- Status: `{}` | Started: {} | Ended: {} | Turns: {} | Tools: {} | Errors: {} ({}){} | Avg turn: {}",
                agent.status,
                agent.started_at.as_deref().unwrap_or("—"),
                agent.ended_at.as_deref().unwrap_or("—"),
                agent.turn_count,
                agent.tool_count,
                agent.error_count,
                err_rate,
                if streak > 1 { format!(" ⚠ streak:{}", streak) } else { String::new() },
                avg_turn
            );
        }
        let _ = writeln!(
            out,
            "- Input: {}",
            agent.input_preview.as_deref().unwrap_or("—")
        );
        let _ = writeln!(
            out,
            "- Output: {}",
            agent.output_preview.as_deref().unwrap_or("—")
        );
        if let Some(reason) = &agent.close_reason {
            let _ = writeln!(out, "- Close reason: {}", truncate_chars(reason, 180));
        }
        if !agent.errors.is_empty() {
            let _ = writeln!(out, "- Errors:");
            for error in &agent.errors {
                let _ = writeln!(
                    out,
                    "  - [{}] `{}` {}",
                    format_timestamp(Some(&error.created_at)),
                    error.tool_name.as_deref().unwrap_or("—"),
                    truncate_chars(&error.summary, 180)
                );
                // Pre-error context: last 2 events before this error
                for ctx in events_before(state, &agent.session_id, &error.created_at, 2) {
                    let _ = writeln!(
                        out,
                        "    - _before:_ [{}] {} {}",
                        format_timestamp(Some(&ctx.created_at)),
                        ctx.kind,
                        truncate_chars(&ctx.summary, 120)
                    );
                }
            }
        }
        if !agent.approvals.is_empty() {
            let _ = writeln!(out, "- Approvals:");
            for approval in &agent.approvals {
                let status_str = match approval.status.as_str() {
                    "pending" => "PENDING".to_string(),
                    _ => format!(
                        "{}: {}",
                        approval.status.to_uppercase(),
                        approval.decision.as_deref().unwrap_or("unknown")
                    ),
                };
                let wait = if approval.status == "pending" {
                    format!(
                        " (waiting {})",
                        format_duration(Some(&approval.created_at), Some(&state.generated_at))
                    )
                } else if let Some(ref resolved) = approval.resolved_at {
                    format!(
                        " (waited {})",
                        format_duration(Some(&approval.created_at), Some(resolved))
                    )
                } else {
                    String::new()
                };
                let _ = writeln!(
                    out,
                    "  - `{}` {}{} — {}",
                    approval.request_id,
                    status_str,
                    wait,
                    truncate_chars(&approval.summary, 200)
                );
            }
        }
        if !agent.artifacts.is_empty() {
            let _ = writeln!(out, "- Artifacts:");
            for artifact in agent.artifacts.iter().rev().take(4).rev() {
                let _ = writeln!(
                    out,
                    "  - `{}` {}",
                    artifact.artifact_id,
                    truncate_chars(&artifact.summary, 200)
                );
            }
        }
        if !agent.delegations.is_empty() {
            let _ = writeln!(out, "- Delegations:");
            let mut per_target: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for d in &agent.delegations {
                let idx = *per_target
                    .entry(d.target_agent.as_str())
                    .and_modify(|c| *c += 1)
                    .or_insert(0);
                let child = find_child_agent(state, &agent.session_id, &d.target_agent, idx);
                let child_status = child.map(|a| a.status.as_str()).unwrap_or("unknown");
                let child_errors = child.map(|a| a.error_count).unwrap_or(0);
                let child_close = child.and_then(|a| a.close_reason.as_deref()).unwrap_or("—");
                let child_output = child
                    .and_then(|a| a.output_preview.as_deref())
                    .unwrap_or(d.output_preview.as_deref().unwrap_or("—"));
                let abnormal = child
                    .and_then(|a| a.close_reason.as_deref())
                    .map_or(false, is_abnormal_close);
                let _ = writeln!(
                    out,
                    "  - `{}` `{}` errs:{}{} → {}",
                    d.target_agent,
                    child_status,
                    child_errors,
                    if abnormal {
                        format!(" ⚠ {}", truncate_chars(child_close, 60))
                    } else {
                        String::new()
                    },
                    truncate_chars(child_output, 120)
                );
            }
        }
        let tool_events: Vec<&ReportEvent> = state
            .timeline
            .iter()
            .filter(|e| {
                e.session_id == agent.session_id && (e.kind == "ACTION" || e.kind == "RESULT" || e.kind == "ERROR")
            })
            .rev()
            .take(MAX_RECENT_EVENTS_PER_AGENT)
            .collect();
        if !tool_events.is_empty() {
            let _ = writeln!(out, "- Tool calls:");
            for event in tool_events.into_iter().rev() {
                let _ = writeln!(
                    out,
                    "  - [{}] {} {}",
                    format_timestamp(Some(&event.created_at)),
                    event.kind,
                    truncate_chars(&event.summary, 200)
                );
            }
        }
        let recent: Vec<&ReportEvent> = state
            .timeline
            .iter()
            .filter(|event| event.session_id == agent.session_id)
            .rev()
            .take(MAX_RECENT_EVENTS_PER_AGENT)
            .collect();
        if !recent.is_empty() {
            let _ = writeln!(out, "- Recent events:");
            for event in recent.into_iter().rev() {
                let _ = writeln!(
                    out,
                    "  - [{}] {} {}",
                    format_timestamp(Some(&event.created_at)),
                    event.kind,
                    truncate_chars(&event.summary, 200)
                );
            }
        }
        let _ = writeln!(out);
    }
    out
}

fn render_live_html(state: &SessionReportState) -> String {
    let mut out = String::new();
    let open_blockers = collect_open_blockers(state, false);
    let turn_nums = timeline_turn_numbers(state);
    let recent_events: Vec<(u32, &ReportEvent)> = state
        .timeline
        .iter()
        .enumerate()
        .filter(|(_, e)| e.important)
        .map(|(i, e)| (turn_nums[i], e))
        .rev()
        .take(16)
        .collect();

    out.push_str("<!DOCTYPE html>\n<html><head>\n");
    out.push_str("<meta charset=\"UTF-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n");
    out.push_str(&format!(
        "<title>Session Overview: {}</title>\n",
        escape_html(&state.root_session_id)
    ));
    out.push_str(r#"<style>
:root {
  --bg: #0d1117; --surface: #161b22; --border: #30363d;
  --text: #e6edf3; --text-dim: #8b949e; --accent: #58a6ff;
  --green: #3fb950; --red: #f85149; --yellow: #d29928; --orange: #db6d28;
  --blue: #58a6ff; --purple: #bc8cff;
}
* { box-sizing: border-box; margin: 0; padding: 0; }
body {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
  background: var(--bg); color: var(--text); line-height: 1.6;
  padding: 2rem; max-width: 1400px; margin: 0 auto;
}
h1 { font-size: 1.5rem; margin-bottom: 0.5rem; border-bottom: 1px solid var(--border); padding-bottom: 0.5rem; }
h2 { font-size: 1.2rem; margin: 1.25rem 0 0.5rem; color: var(--accent); }
table { width: 100%; border-collapse: collapse; margin: 0.75rem 0; font-size: 0.85rem; }
th, td { border: 1px solid var(--border); padding: 0.4rem 0.6rem; text-align: left; }
th { background: var(--surface); font-weight: 600; white-space: nowrap; }
tr:hover td { background: #1c2128; }
code { background: #21262d; padding: 0.1rem 0.3rem; border-radius: 3px; font-size: 0.85em; }
.badge {
  display: inline-block; padding: 0.1rem 0.5rem; border-radius: 99px;
  font-size: 0.7rem; font-weight: 600;
}
.badge-running { background: #1f6feb33; color: var(--blue); }
.badge-completed { background: #3fb95033; color: var(--green); }
.badge-failed { background: #f8514933; color: var(--red); }
.badge-suspended { background: #d2992833; color: var(--yellow); }
.badge-awaiting_approval { background: #db6d2833; color: var(--orange); }
.section-note { color: var(--text-dim); font-style: italic; font-size: 0.85rem; }
.health-banner { padding: 0.5rem 1rem; border-radius: 4px; margin: 0.75rem 0; font-weight: 600; font-size: 0.9rem; }
.health-ok { background: #3fb95022; color: var(--green); border-left: 3px solid var(--green); }
.health-warn { background: #d2992822; color: var(--yellow); border-left: 3px solid var(--yellow); }
.health-error { background: #f8514922; color: var(--red); border-left: 3px solid var(--red); }
</style>
</head><body>\n"#);

    out.push_str(&format!(
        "<h1>Session Overview: <code>{}</code></h1>\n",
        escape_html(&state.root_session_id)
    ));
    out.push_str("<p><em>Auto-updated structured view. Narrative log remains in <code>digest.md</code>.</em></p>\n");

    let total_turns: u32 = state.agents.values().map(|a| a.turn_count).sum();
    out.push_str("<h2>Status</h2>\n");
    out.push_str("<table><tbody>\n");
    out.push_str(&format!(
        "<tr><th>Status</th><td><code>{}</code></td></tr>\n",
        state.status
    ));
    out.push_str(&format!(
        "<tr><th>Started</th><td>{}</td></tr>\n",
        state.started_at.as_deref().unwrap_or("—")
    ));
    out.push_str(&format!(
        "<tr><th>Total turns</th><td>{}</td></tr>\n",
        total_turns
    ));
    out.push_str(&format!(
        "<tr><th>Duration</th><td>{}</td></tr>\n",
        format_duration(state.started_at.as_deref(), Some(&state.generated_at))
    ));
    out.push_str(&format!(
        "<tr><th>Last updated</th><td>{}</td></tr>\n",
        state.generated_at
    ));
    out.push_str("</tbody></table>\n");
    {
        let agents_with_errors = state.agents.values().filter(|a| a.error_count > 0).count();
        let awaiting = state
            .agents
            .values()
            .filter(|a| a.status == "awaiting_approval")
            .count();
        let failed = state
            .agents
            .values()
            .filter(|a| a.status == "failed")
            .count();
        let abnormal_exits = state
            .agents
            .values()
            .filter(|a| a.close_reason.as_deref().map_or(false, is_abnormal_close))
            .count();
        let (class, msg) = if failed > 0 || agents_with_errors > 0 || abnormal_exits > 0 {
            ("health-error", format!(
                "[ISSUES] {} agent(s) with errors | {} failed | {} awaiting approval | {} abnormal exit(s)",
                agents_with_errors, failed, awaiting, abnormal_exits
            ))
        } else if awaiting > 0 {
            (
                "health-warn",
                format!("[WAITING] {} agent(s) awaiting approval", awaiting),
            )
        } else {
            ("health-ok", "[OK] No issues detected".to_string())
        };
        out.push_str(&format!(
            "<div class=\"health-banner {}\">{}</div>\n",
            class,
            escape_html(&msg)
        ));
    }

    out.push_str("<h2>Active Agents</h2>\n");
    out.push_str("<table><thead><tr><th style=\"width:14%\">Agent</th><th style=\"width:10%\">Session</th><th style=\"width:8%\">Parent</th><th style=\"width:8%\">Status</th><th style=\"width:5%\">Turns</th><th style=\"width:5%\">Err%</th><th style=\"width:10%\">Started</th><th style=\"width:18%\">Last Event</th><th style=\"width:14%\">Output</th><th style=\"width:4%\">Errors</th></tr></thead><tbody>\n");
    for (depth, agent) in agents_by_tree(state, None) {
        let indent = "&nbsp;&nbsp;".repeat(depth);
        let err_pct = if agent.tool_count == 0 {
            "—".to_string()
        } else {
            format!("{}%", agent.error_count * 100 / agent.tool_count)
        };
        out.push_str(&format!("<tr><td><code>{}{}</code></td><td><code>{}</code></td><td>{}</td><td><span class=\"badge badge-{}\">{}</span></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            indent, escape_html(&agent.agent_id),
            escape_html(short_session_id(&agent.session_id)),
            agent.parent_session_id.as_deref().map(|s| format!("<code>{}</code>", escape_html(short_session_id(s)))).unwrap_or_else(|| "—".to_string()),
            status_to_badge_class(&agent.status), agent.status,
            agent.turn_count,
            escape_html(&err_pct),
            format_timestamp(agent.started_at.as_deref()),
            truncate_html(agent.last_event_summary.as_deref().unwrap_or("—"), OUTPUT_PREVIEW_DISPLAY_CHARS),
            truncate_html(agent.output_preview.as_deref().unwrap_or("—"), OUTPUT_PREVIEW_DISPLAY_CHARS),
            agent.error_count,
        ));
    }
    out.push_str("</tbody></table>\n");

    let error_blockers: Vec<_> = open_blockers.iter().filter(|b| b.2 == "error").collect();
    out.push_str("<h2>Open Errors</h2>\n");
    if error_blockers.is_empty() {
        out.push_str("<p class=\"section-note\">none</p>\n");
    } else {
        out.push_str("<table><thead><tr><th style=\"width:15%\">Time</th><th style=\"width:15%\">Agent</th><th>Summary</th></tr></thead><tbody>\n");
        for blocker in &error_blockers {
            out.push_str(&format!(
                "<tr><td>{}</td><td><code>{}</code></td><td>{}</td></tr>\n",
                format_timestamp(Some(&blocker.0)),
                escape_html(&blocker.1),
                truncate_html(&blocker.3, OUTPUT_PREVIEW_DISPLAY_CHARS),
            ));
        }
        out.push_str("</tbody></table>\n");
    }

    let pending_approvals: Vec<_> = collect_all_approvals(state)
        .into_iter()
        .filter(|a| a.status == "pending")
        .collect();
    out.push_str("<h2>Open Approvals</h2>\n");
    if pending_approvals.is_empty() {
        out.push_str("<p class=\"section-note\">none</p>\n");
    } else {
        out.push_str("<table><thead><tr><th style=\"width:15%\">Time</th><th style=\"width:12%\">Agent</th><th style=\"width:12%\">Request ID</th><th style=\"width:8%\">Kind</th><th>Summary</th><th>Reason</th></tr></thead><tbody>\n");
        for a in &pending_approvals {
            out.push_str(&format!("<tr><td>{}</td><td><code>{}</code></td><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                format_timestamp(Some(&a.created_at)),
                escape_html(&a.agent_id),
                escape_html(&a.request_id),
                escape_html(&a.kind),
                truncate_html(&a.summary, OUTPUT_PREVIEW_DISPLAY_CHARS),
                a.reason.as_deref().unwrap_or("—"),
            ));
        }
        out.push_str("</tbody></table>\n");
    }

    {
        let abnormal: Vec<_> = state
            .agents
            .values()
            .filter(|a| a.close_reason.as_deref().map_or(false, is_abnormal_close))
            .collect();
        out.push_str("<h2>Abnormal Exits</h2>\n");
        if abnormal.is_empty() {
            out.push_str("<p class=\"section-note\">none</p>\n");
        } else {
            out.push_str("<table><thead><tr><th>Agent</th><th>Session</th><th>Status</th><th>Close Reason</th></tr></thead><tbody>\n");
            for agent in &abnormal {
                out.push_str(&format!("<tr><td><code>{}</code></td><td><code>{}</code></td><td><span class=\"badge badge-{}\">{}</span></td><td>{}</td></tr>\n",
                    escape_html(&agent.agent_id),
                    escape_html(short_session_id(&agent.session_id)),
                    status_to_badge_class(&agent.status), agent.status,
                    truncate_html(agent.close_reason.as_deref().unwrap_or("—"), 200),
                ));
            }
            out.push_str("</tbody></table>\n");
        }
    }

    out.push_str("<h2>Recent Important Events</h2>\n");
    if recent_events.is_empty() {
        out.push_str("<p class=\"section-note\">none yet</p>\n");
    } else {
        out.push_str("<table><thead><tr><th style=\"width:13%\">Time</th><th style=\"width:5%\">T#</th><th style=\"width:10%\">Session</th><th style=\"width:12%\">Agent</th><th style=\"width:10%\">Kind</th><th>Summary</th></tr></thead><tbody>\n");
        for (turn_num, event) in recent_events.into_iter().rev() {
            out.push_str(&format!(
                "<tr><td>{}</td><td>T{}</td><td><code>{}</code></td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>\n",
                format_timestamp(Some(&event.created_at)),
                turn_num,
                escape_html(short_session_id(&event.session_id)),
                escape_html(&event.agent_id),
                escape_html(&event.kind),
                truncate_html(&event.summary, OUTPUT_PREVIEW_DISPLAY_CHARS),
            ));
        }
        out.push_str("</tbody></table>\n");
    }

    out.push_str("<h2>Agent Details</h2>\n");
    for (_depth, agent) in agents_by_tree(state, None) {
        let turn_range_str = match agent_turn_range(state, &agent.session_id, &turn_nums) {
            Some((f, l)) if f == l => format!(" (T{})", f),
            Some((f, l)) => format!(" (T{}–T{})", f, l),
            None => String::new(),
        };
        out.push_str(&format!(
            "<h3><code>{}</code> <code>{}</code>{}</h3>\n",
            escape_html(&agent.agent_id),
            escape_html(short_session_id(&agent.session_id)),
            turn_range_str
        ));
        {
            let streak = consecutive_error_streak(state, &agent.session_id);
            let avg_turn = avg_turn_secs(state, &agent.session_id)
                .map(|s| format!("{}s", s))
                .unwrap_or_else(|| "—".to_string());
            let err_rate = if agent.tool_count == 0 {
                "—".to_string()
            } else {
                format!("{}%", agent.error_count * 100 / agent.tool_count)
            };
            out.push_str("<table><tbody>\n");
            out.push_str(&format!("<tr><th>Status</th><td><span class=\"badge badge-{}\">{}</span></td><th>Started</th><td>{}</td><th>Turns</th><td>{}</td></tr>\n",
                status_to_badge_class(&agent.status), agent.status,
                format_timestamp(agent.started_at.as_deref()), agent.turn_count));
            out.push_str(&format!(
                "<tr><th>Tools</th><td>{}</td><th>Errors</th><td>{} ({}){}</td><th>Avg turn</th><td>{}</td></tr>\n",
                agent.tool_count,
                agent.error_count,
                escape_html(&err_rate),
                if streak > 1 { format!(" <strong style=\"color:var(--red)\">⚠ streak:{}</strong>", streak) } else { String::new() },
                avg_turn
            ));
            out.push_str(&format!(
                "<tr><th>Input</th><td colspan=\"5\">{}</td></tr>\n",
                escape_html(agent.input_preview.as_deref().unwrap_or("—"))
            ));
            out.push_str(&format!(
                "<tr><th>Output</th><td colspan=\"5\">{}</td></tr>\n",
                escape_html(agent.output_preview.as_deref().unwrap_or("—"))
            ));
            out.push_str("</tbody></table>\n");
        }
        if !agent.errors.is_empty() {
            let _ = writeln!(
                out,
                "<details><summary>Errors with context ({})</summary>",
                agent.errors.len()
            );
            out.push_str("<table><thead><tr><th>Time</th><th>Tool</th><th>Summary</th><th>Context before</th></tr></thead><tbody>\n");
            for e in &agent.errors {
                let ctx_items: Vec<String> =
                    events_before(state, &agent.session_id, &e.created_at, 2)
                        .iter()
                        .map(|ctx| {
                            format!(
                                "[{}] {} {}",
                                format_timestamp(Some(&ctx.created_at)),
                                escape_html(&ctx.kind),
                                truncate_html(&ctx.summary, 80)
                            )
                        })
                        .collect();
                out.push_str(&format!(
                    "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td style=\"font-size:0.8rem;color:var(--text-dim)\">{}</td></tr>\n",
                    format_timestamp(Some(&e.created_at)),
                    escape_html(e.tool_name.as_deref().unwrap_or("—")),
                    truncate_html(&e.summary, 120),
                    ctx_items.join("<br>"),
                ));
            }
            out.push_str("</tbody></table></details>\n");
        }
        if !agent.approvals.is_empty() {
            let _ = writeln!(
                out,
                "<details><summary>Approvals ({})</summary>",
                agent.approvals.len()
            );
            out.push_str("<table><thead><tr><th>Request ID</th><th>Kind</th><th>Status</th><th>Wait</th><th>Summary</th></tr></thead><tbody>\n");
            for a in &agent.approvals {
                let status_cls = if a.status == "pending" {
                    "badge-awaiting_approval"
                } else {
                    "badge-completed"
                };
                let wait = if a.status == "pending" {
                    format_duration(Some(&a.created_at), Some(&state.generated_at))
                } else if let Some(ref resolved) = a.resolved_at {
                    format!(
                        "waited {}",
                        format_duration(Some(&a.created_at), Some(resolved))
                    )
                } else {
                    "—".to_string()
                };
                out.push_str(&format!("<tr><td><code>{}</code></td><td>{}</td><td><span class=\"badge {}\">{}</span></td><td>{}</td><td>{}</td></tr>\n",
                    escape_html(&a.request_id), escape_html(&a.kind),
                    status_cls, a.status,
                    escape_html(&wait),
                    truncate_html(&a.summary, 80),
                ));
            }
            out.push_str("</tbody></table></details>\n");
        }
        if !agent.delegations.is_empty() {
            let _ = writeln!(
                out,
                "<details><summary>Delegations ({})</summary>",
                agent.delegations.len()
            );
            out.push_str("<table><thead><tr><th>Target</th><th>Status</th><th>Errors</th><th>Close</th><th>Output</th></tr></thead><tbody>\n");
            let mut per_target: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for d in &agent.delegations {
                let idx = *per_target
                    .entry(d.target_agent.as_str())
                    .and_modify(|c| *c += 1)
                    .or_insert(0);
                let child = find_child_agent(state, &agent.session_id, &d.target_agent, idx);
                let child_status = child.map(|a| a.status.as_str()).unwrap_or("unknown");
                let child_errors = child.map(|a| a.error_count).unwrap_or(0);
                let child_close = child.and_then(|a| a.close_reason.as_deref()).unwrap_or("—");
                let child_output = child
                    .and_then(|a| a.output_preview.as_deref())
                    .unwrap_or(d.output_preview.as_deref().unwrap_or("—"));
                let abnormal = child
                    .and_then(|a| a.close_reason.as_deref())
                    .map_or(false, is_abnormal_close);
                out.push_str(&format!("<tr><td><code>{}</code></td><td><span class=\"badge badge-{}\">{}</span></td><td>{}</td><td style=\"color:{}\">{}</td><td>{}</td></tr>\n",
                    escape_html(&d.target_agent),
                    status_to_badge_class(child_status), child_status,
                    child_errors,
                    if abnormal { "var(--red)" } else { "inherit" },
                    truncate_html(child_close, 80),
                    truncate_html(child_output, 80),
                ));
            }
            out.push_str("</tbody></table></details>\n");
        }
        let tool_events: Vec<_> = state
            .timeline
            .iter()
            .filter(|e| e.session_id == agent.session_id && (e.kind == "ACTION" || e.kind == "RESULT" || e.kind == "ERROR"))
            .rev()
            .take(MAX_RECENT_EVENTS_PER_AGENT)
            .collect();
        if !tool_events.is_empty() {
            let _ = writeln!(
                out,
                "<details><summary>Tool Calls ({})</summary>",
                tool_events.len()
            );
            out.push_str("<table><thead><tr><th style=\"width:13%\">Time</th><th style=\"width:10%\">Kind</th><th>Summary</th></tr></thead><tbody>\n");
            for event in tool_events.into_iter().rev() {
                out.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                    format_timestamp(Some(&event.created_at)),
                    escape_html(&event.kind),
                    truncate_html(&event.summary, 150),
                ));
            }
            out.push_str("</tbody></table></details>\n");
        }
        let recent: Vec<_> = state
            .timeline
            .iter()
            .filter(|e| e.session_id == agent.session_id)
            .rev()
            .take(MAX_RECENT_EVENTS_PER_AGENT)
            .collect();
        if !recent.is_empty() {
            let _ = writeln!(
                out,
                "<details><summary>Recent Events ({})</summary>",
                recent.len()
            );
            for event in recent.into_iter().rev() {
                out.push_str(&format!("<div class=\"timeline-item\"><span class=\"timeline-time\">{}</span><span class=\"timeline-kind kind-{}\">{}</span><span class=\"timeline-summary\">{}</span></div>\n",
                    format_timestamp(Some(&event.created_at)),
                    event.kind.to_lowercase(),
                    event.kind,
                    truncate_html(&event.summary, 120),
                ));
            }
            out.push_str("</details>\n");
        }
        out.push_str("<br>\n");
    }

    out.push_str(&format!("<p style=\"color:var(--text-dim);font-size:0.75rem;margin-top:1.5rem;\">Generated at {} &bull; Live Overview</p>\n",
        state.generated_at));
    out.push_str("</body></html>");
    out
}

fn render_final_markdown(state: &SessionReportState) -> String {
    let mut out = String::new();
    let agents = sorted_agents(state);
    let _blockers = collect_open_blockers(state, true);
    let total_errors: u32 = state.agents.values().map(|a| a.error_count).sum();
    let total_approvals: u32 = state.agents.values().map(|a| a.approval_count).sum();

    let _ = writeln!(out, "# Session report: `{}`", state.root_session_id);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Overview");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Field | Value |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| Status | `{}` |", state.status);
    let _ = writeln!(
        out,
        "| Started | {} |",
        state.started_at.as_deref().unwrap_or("—")
    );
    let _ = writeln!(
        out,
        "| Ended | {} |",
        state.ended_at.as_deref().unwrap_or("—")
    );
    let _ = writeln!(
        out,
        "| Duration | {} |",
        format_duration(state.started_at.as_deref(), state.ended_at.as_deref())
    );
    let _ = writeln!(out, "| Agent sessions | {} |", agents.len());
    let _ = writeln!(out, "| Errors | {} |", total_errors);
    let _ = writeln!(out, "| Approvals | {} |", total_approvals);
    let _ = writeln!(out);
    let _ = writeln!(out, "## Agent Summary");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| Agent | Session | Parent | Started | Ended | Duration | Turns | Status | Input | Output | Errors |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---:|---|---|---|---:|");
    for agent in &agents {
        let _ = writeln!(
            out,
            "| `{}` | `{}` | `{}` | {} | {} | {} | {} | `{}` | {} | {} | {} |",
            agent.agent_id,
            short_session_id(&agent.session_id),
            agent
                .parent_session_id
                .as_deref()
                .map(short_session_id)
                .unwrap_or("—"),
            format_timestamp(agent.started_at.as_deref()),
            format_timestamp(agent.ended_at.as_deref()),
            format_duration(agent.started_at.as_deref(), agent.ended_at.as_deref()),
            agent.turn_count,
            agent.status,
            truncate_chars(
                agent.input_preview.as_deref().unwrap_or("—"),
                OUTPUT_PREVIEW_DISPLAY_CHARS
            ),
            truncate_chars(
                agent.output_preview.as_deref().unwrap_or("—"),
                OUTPUT_PREVIEW_DISPLAY_CHARS
            ),
            agent.error_count
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Sub-Agent Ledger");
    let _ = writeln!(out);
    let has_delegations = state.agents.values().any(|a| !a.delegations.is_empty());
    if has_delegations {
        let _ = writeln!(
            out,
            "| Parent Agent | Target Agent | Session | Task | Status | Errors | Close | Output |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|---|---|");
        for agent in &agents {
            let mut per_target_counter: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for d in &agent.delegations {
                let idx = *per_target_counter
                    .entry(&d.target_agent)
                    .and_modify(|c| {
                        *c += 1;
                    })
                    .or_insert(0);
                let child = find_child_agent(state, &agent.session_id, &d.target_agent, idx);
                let child_status = child.map(|a| a.status.as_str()).unwrap_or("unknown");
                let child_session = child
                    .map(|a| short_session_id(&a.session_id))
                    .unwrap_or("—");
                let child_output = child
                    .and_then(|a| a.output_preview.as_deref())
                    .unwrap_or(d.output_preview.as_deref().unwrap_or("—"));
                let child_errors = child
                    .map(|a| a.error_count.to_string())
                    .unwrap_or_else(|| "—".to_string());
                let child_close = child
                    .and_then(|a| a.close_reason.as_deref())
                    .map(|r| truncate_chars(r, 80))
                    .unwrap_or_else(|| "—".to_string());
                let _ = writeln!(
                    out,
                    "| `{}` | `{}` | `{}` | {} | `{}` | {} | {} | {} |",
                    escape_html(&agent.agent_id),
                    escape_html(&d.target_agent),
                    child_session,
                    truncate_chars(&d.task_preview, OUTPUT_PREVIEW_DISPLAY_CHARS),
                    child_status,
                    child_errors,
                    child_close,
                    truncate_chars(child_output, OUTPUT_PREVIEW_DISPLAY_CHARS),
                );
            }
        }
    } else {
        let _ = writeln!(out, "_No sub-agent delegations recorded._");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Errors");
    let _ = writeln!(out);
    let errors: Vec<_> = state
        .agents
        .values()
        .flat_map(|a| a.errors.iter().map(move |e| (a.agent_id.as_str(), e)))
        .collect();
    if errors.is_empty() {
        let _ = writeln!(out, "_(none)_");
    } else {
        let _ = writeln!(out, "| Time | Agent | Tool | Summary |");
        let _ = writeln!(out, "|---|---|---|---|");
        for (agent_id, error) in &errors {
            let _ = writeln!(
                out,
                "| {} | `{}` | {} | {} |",
                format_timestamp(Some(&error.created_at)),
                agent_id,
                error.tool_name.as_deref().unwrap_or("—"),
                truncate_chars(&error.summary, 200),
            );
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Approvals");
    let _ = writeln!(out);
    let approvals = collect_all_approvals(state);
    if approvals.is_empty() {
        let _ = writeln!(out, "_(none)_");
    } else {
        let _ = writeln!(out, "| Time | Agent | Request ID | Kind | Status | Decision | Summary | Reason | Resolved |");
        let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|");
        for a in &approvals {
            let _ = writeln!(
                out,
                "| {} | `{}` | `{}` | {} | `{}` | {} | {} | {} | {} |",
                format_timestamp(Some(&a.created_at)),
                a.agent_id,
                a.request_id,
                a.kind,
                a.status,
                a.decision.as_deref().unwrap_or("—"),
                truncate_chars(&a.summary, OUTPUT_PREVIEW_DISPLAY_CHARS),
                a.reason.as_deref().unwrap_or("—"),
                a.resolved_at
                    .as_deref()
                    .map(|t| format_timestamp(Some(t)))
                    .unwrap_or_else(|| "—".to_string()),
            );
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Outputs");
    let _ = writeln!(out);
    let artifact_rows: Vec<(&AgentReport, &ArtifactItem)> = state
        .agents
        .values()
        .flat_map(|agent| {
            agent
                .artifacts
                .iter()
                .map(move |artifact| (agent, artifact))
        })
        .collect();
    if artifact_rows.is_empty() {
        let _ = writeln!(out, "_(no artifacts captured)_");
    } else {
        let _ = writeln!(out, "| Agent | Artifact | Tool | Summary |");
        let _ = writeln!(out, "|---|---|---|---|");
        for (agent, artifact) in artifact_rows {
            let _ = writeln!(
                out,
                "| `{}` | `{}` | `{}` | {} |",
                agent.agent_id,
                artifact.artifact_id,
                artifact.tool_name,
                truncate_chars(&artifact.summary, OUTPUT_PREVIEW_DISPLAY_CHARS)
            );
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Recent Important Events");
    let _ = writeln!(out);
    let turn_nums_final = timeline_turn_numbers(state);
    let _ = writeln!(out, "| Time | T# | Session | Agent | Kind | Summary |");
    let _ = writeln!(out, "|---|---|---|---|---|---|");
    for (turn_num, event) in state
        .timeline
        .iter()
        .enumerate()
        .filter(|(_, e)| e.important)
        .map(|(i, e)| (turn_nums_final[i], e))
        .rev()
        .take(24)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let _ = writeln!(
            out,
            "| {} | T{} | `{}` | `{}` | {} | {} |",
            format_timestamp(Some(&event.created_at)),
            turn_num,
            short_session_id(&event.session_id),
            event.agent_id,
            event.kind,
            truncate_chars(&event.summary, OUTPUT_PREVIEW_DISPLAY_CHARS)
        );
    }
    out
}

fn render_html_report(state: &SessionReportState) -> String {
    let agents = sorted_agents(state);
    let _blockers = collect_open_blockers(state, true);
    let total_errors: u32 = state.agents.values().map(|a| a.error_count).sum();
    let total_approvals: u32 = state.agents.values().map(|a| a.approval_count).sum();

    let mut out = String::new();
    out.push_str("<!DOCTYPE html>\n<html><head>\n");
    out.push_str("<meta charset=\"UTF-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n");
    out.push_str(&format!(
        "<title>Session Report: {}</title>\n",
        escape_html(&state.root_session_id)
    ));
    out.push_str(r#"<style>
:root {
  --bg: #0d1117; --surface: #161b22; --border: #30363d;
  --text: #e6edf3; --text-dim: #8b949e; --accent: #58a6ff;
  --green: #3fb950; --red: #f85149; --yellow: #d29928; --orange: #db6d28;
  --blue: #58a6ff; --purple: #bc8cff;
}
* { box-sizing: border-box; margin: 0; padding: 0; }
body {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
  background: var(--bg); color: var(--text); line-height: 1.6;
  padding: 2rem; max-width: 1200px; margin: 0 auto;
}
h1 { font-size: 1.8rem; margin-bottom: 0.5rem; border-bottom: 1px solid var(--border); padding-bottom: 0.5rem; }
h2 { font-size: 1.3rem; margin: 1.5rem 0 0.75rem; color: var(--accent); }
h3 { font-size: 1.05rem; margin: 1rem 0 0.5rem; color: var(--text-dim); }
p { margin-bottom: 0.5rem; }
table { width: 100%; border-collapse: collapse; margin: 1rem 0; font-size: 0.9rem; }
th, td { border: 1px solid var(--border); padding: 0.5rem 0.75rem; text-align: left; }
th { background: var(--surface); font-weight: 600; white-space: nowrap; }
tr:hover td { background: #1c2128; }
code { background: #21262d; padding: 0.1rem 0.3rem; border-radius: 3px; font-size: 0.85em; }
.badge {
  display: inline-block; padding: 0.1rem 0.5rem; border-radius: 99px;
  font-size: 0.75rem; font-weight: 600;
}
.badge-running { background: #1f6feb33; color: var(--blue); }
.badge-completed { background: #3fb95033; color: var(--green); }
.badge-failed { background: #f8514933; color: var(--red); }
.badge-suspended { background: #d2992833; color: var(--yellow); }
.badge-awaiting_approval { background: #db6d2833; color: var(--orange); }
.stat-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(140px, 1fr)); gap: 0.75rem; margin: 1rem 0; }
.stat-card {
  background: var(--surface); border: 1px solid var(--border);
  border-radius: 6px; padding: 0.75rem 1rem; text-align: center;
}
.stat-card .stat-value { font-size: 1.5rem; font-weight: 700; }
.stat-card .stat-label { font-size: 0.75rem; color: var(--text-dim); text-transform: uppercase; }
.tree-node { margin-left: 1.5rem; border-left: 2px solid var(--border); padding-left: 0.75rem; }
.kind-ACTION { color: var(--blue); }
.kind-RESULT { color: var(--green); }
.kind-ERROR { color: var(--red); }
.kind-APPROVAL { color: var(--orange); }
.kind-POLL { color: var(--text-dim); }
.kind-DELEGATE { color: var(--purple); }
.kind-NOTE { color: var(--text-dim); }
.kind-SESSION, .kind-FINAL { color: var(--accent); }
details { margin: 0.5rem 0; }
summary { cursor: pointer; color: var(--text-dim); font-size: 0.9rem; }
summary:hover { color: var(--text); }
pre { background: var(--surface); border: 1px solid var(--border); border-radius: 4px; padding: 0.75rem; overflow-x: auto; font-size: 0.8rem; }
.agent-card {
  background: var(--surface); border: 1px solid var(--border);
  border-radius: 8px; padding: 1rem; margin: 1rem 0;
}
.agent-header { display: flex; align-items: center; gap: 0.75rem; margin-bottom: 0.75rem; }
.agent-header h3 { margin: 0; flex: 1; }
.meta-row { display: flex; gap: 1rem; flex-wrap: wrap; font-size: 0.85rem; color: var(--text-dim); margin-bottom: 0.5rem; }
.meta-row strong { color: var(--text); }
.preview { background: #21262d; border-left: 3px solid var(--border); padding: 0.5rem 0.75rem; margin: 0.5rem 0; font-size: 0.85rem; white-space: pre-wrap; word-break: break-word; }
.timeline-item { display: flex; gap: 0.75rem; padding: 0.4rem 0; font-size: 0.85rem; border-bottom: 1px solid var(--border); }
.timeline-time { color: var(--text-dim); white-space: nowrap; min-width: 70px; }
.timeline-kind { font-weight: 600; min-width: 80px; }
.timeline-summary { flex: 1; }
.section-note { color: var(--text-dim); font-style: italic; font-size: 0.9rem; }
</style>
</head><body>\n"#);

    out.push_str(&format!(
        "<h1>Session Report: <code>{}</code></h1>\n",
        escape_html(&state.root_session_id)
    ));

    out.push_str("<h2>Overview</h2>\n");
    out.push_str("<div class=\"stat-grid\">\n");
    out.push_str(&format!("<div class=\"stat-card\"><div class=\"stat-value\">{}</div><div class=\"stat-label\">Status</div></div>\n",
        match state.status.as_str() {
            "completed" => r#"<span class="badge badge-completed">Completed</span>"#,
            "failed" => r#"<span class="badge badge-failed">Failed</span>"#,
            "suspended" => r#"<span class="badge badge-suspended">Suspended</span>"#,
            "running" => r#"<span class="badge badge-running">Running</span>"#,
            s => s,
        }));
    out.push_str(&format!("<div class=\"stat-card\"><div class=\"stat-value\">{}</div><div class=\"stat-label\">Agents</div></div>\n", agents.len()));
    out.push_str(&format!("<div class=\"stat-card\"><div class=\"stat-value\" style=\"color:{}\">{}</div><div class=\"stat-label\">Errors</div></div>\n",
        if total_errors > 0 { "var(--red)" } else { "var(--text)" }, total_errors));
    out.push_str(&format!("<div class=\"stat-card\"><div class=\"stat-value\">{}</div><div class=\"stat-label\">Approvals</div></div>\n", total_approvals));
    out.push_str(&format!("<div class=\"stat-card\"><div class=\"stat-value\">{}</div><div class=\"stat-label\">Events</div></div>\n", state.timeline.len()));
    out.push_str("</div>\n");
    out.push_str(&format!("<p><strong>Started:</strong> {} &nbsp;|&nbsp; <strong>Ended:</strong> {} &nbsp;|&nbsp; <strong>Duration:</strong> {}</p>\n",
        state.started_at.as_deref().unwrap_or("—"),
        state.ended_at.as_deref().unwrap_or("—"),
        format_duration(state.started_at.as_deref(), state.ended_at.as_deref())));

    out.push_str("<h2>Agent Hierarchy</h2>\n");
    render_agent_tree_html(&mut out, state, None, 0);

    out.push_str("<h2>Sub-Agent Ledger</h2>\n");
    let has_delegations = state.agents.values().any(|a| !a.delegations.is_empty());
    if has_delegations {
        out.push_str("<table><thead><tr><th>Parent Agent</th><th>Target Agent</th><th>Session</th><th>Task</th><th>Status</th><th>Errors</th><th>Close</th><th>Output</th></tr></thead><tbody>\n");
        for agent in &agents {
            let mut per_target_counter: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for d in &agent.delegations {
                let idx = *per_target_counter
                    .entry(&d.target_agent)
                    .and_modify(|c| {
                        *c += 1;
                    })
                    .or_insert(0);
                let child = find_child_agent(state, &agent.session_id, &d.target_agent, idx);
                let child_status = child.map(|a| a.status.as_str()).unwrap_or("unknown");
                let child_session = child
                    .map(|a| short_session_id(&a.session_id))
                    .unwrap_or("—");
                let child_output = child
                    .and_then(|a| a.output_preview.as_deref())
                    .unwrap_or(d.output_preview.as_deref().unwrap_or("—"));
                let child_errors = child
                    .map(|a| a.error_count.to_string())
                    .unwrap_or_else(|| "—".to_string());
                let child_close = child.and_then(|a| a.close_reason.as_deref()).unwrap_or("—");
                out.push_str(&format!("<tr><td><code>{}</code></td><td><code>{}</code></td><td><code>{}</code></td><td>{}</td><td><span class=\"badge badge-{}\">{}</span></td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                    escape_html(&agent.agent_id),
                    escape_html(&d.target_agent),
                    escape_html(child_session),
                    truncate_html(&d.task_preview, 60),
                    status_to_badge_class(child_status),
                    child_status,
                    escape_html(&child_errors),
                    truncate_html(child_close, 80),
                    truncate_html(child_output, 60),
                ));
            }
        }
        out.push_str("</tbody></table>\n");
    } else {
        out.push_str("<p class=\"section-note\">No sub-agent delegations recorded.</p>\n");
    }

    out.push_str("<h2>Agent Summary</h2>\n");
    out.push_str("<table><thead><tr><th>Agent</th><th>Session</th><th>Parent</th><th>Status</th><th>Turns</th><th>Tools</th><th>Errors</th><th>Duration</th></tr></thead><tbody>\n");
    for agent in &agents {
        out.push_str(&format!("<tr><td><code>{}</code></td><td><code>{}</code></td><td>{}</td><td><span class=\"badge badge-{}\">{}</span></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            escape_html(&agent.agent_id),
            escape_html(short_session_id(&agent.session_id)),
            agent.parent_session_id.as_deref().map(|s| format!("<code>{}</code>", escape_html(short_session_id(s)))).unwrap_or_else(|| "—".to_string()),
            status_to_badge_class(&agent.status),
            agent.status,
            agent.turn_count,
            agent.tool_count,
            agent.error_count,
            format_duration(agent.started_at.as_deref(), agent.ended_at.as_deref()),
        ));
    }
    out.push_str("</tbody></table>\n");

    let html_errors: Vec<_> = state
        .agents
        .values()
        .flat_map(|a| a.errors.iter().map(move |e| (a.agent_id.as_str(), e)))
        .collect();
    if !html_errors.is_empty() {
        out.push_str("<h2>Errors</h2>\n");
        out.push_str("<table><thead><tr><th>Time</th><th>Agent</th><th>Tool</th><th>Summary</th></tr></thead><tbody>\n");
        for (agent_id, error) in &html_errors {
            out.push_str(&format!(
                "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>\n",
                format_timestamp(Some(&error.created_at)),
                escape_html(agent_id),
                escape_html(error.tool_name.as_deref().unwrap_or("—")),
                truncate_html(&error.summary, 120),
            ));
        }
        out.push_str("</tbody></table>\n");
    }

    let html_approvals = collect_all_approvals(state);
    if !html_approvals.is_empty() {
        out.push_str("<h2>Approvals</h2>\n");
        out.push_str("<table><thead><tr><th>Time</th><th>Agent</th><th>Request ID</th><th>Kind</th><th>Status</th><th>Decision</th><th>Summary</th><th>Resolved</th></tr></thead><tbody>\n");
        for a in &html_approvals {
            let status_badge_class = if a.status == "pending" {
                "badge-awaiting_approval"
            } else {
                "badge-completed"
            };
            let decision_html = a
                .decision
                .as_deref()
                .map(|d| {
                    let cls = if d == "approved" {
                        "badge-completed"
                    } else {
                        "badge-failed"
                    };
                    format!("<span class=\"badge {}\">{}</span>", cls, escape_html(d))
                })
                .unwrap_or_else(|| "—".to_string());
            out.push_str(&format!(
                "<tr><td>{}</td><td><code>{}</code></td><td><code>{}</code></td><td>{}</td><td><span class=\"badge {}\">{}</span></td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                format_timestamp(Some(&a.created_at)),
                escape_html(&a.agent_id),
                escape_html(&a.request_id),
                escape_html(&a.kind),
                status_badge_class,
                a.status,
                decision_html,
                truncate_html(&a.summary, 80),
                a.resolved_at.as_deref().map(|t| format_timestamp(Some(t))).unwrap_or_else(|| "—".to_string()),
            ));
        }
        out.push_str("</tbody></table>\n");
    }

    let artifact_rows: Vec<(&AgentReport, &ArtifactItem)> = state
        .agents
        .values()
        .flat_map(|agent| {
            agent
                .artifacts
                .iter()
                .map(move |artifact| (agent, artifact))
        })
        .collect();
    if !artifact_rows.is_empty() {
        out.push_str("<h2>Artifacts</h2>\n");
        out.push_str("<table><thead><tr><th>Agent</th><th>Artifact</th><th>Tool</th><th>Summary</th></tr></thead><tbody>\n");
        for (agent, artifact) in &artifact_rows {
            out.push_str(&format!("<tr><td><code>{}</code></td><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>\n",
                escape_html(&agent.agent_id),
                escape_html(&artifact.artifact_id),
                escape_html(&artifact.tool_name),
                truncate_html(&artifact.summary, 100),
            ));
        }
        out.push_str("</tbody></table>\n");
    }

    out.push_str("<h2>Timeline</h2>\n");
    let important_events: Vec<_> = state.timeline.iter().filter(|e| e.important).collect();
    let all_events: Vec<_> = state.timeline.iter().collect();
    out.push_str(&format!(
        "<p>Showing {} important events out of {} total.</p>\n",
        important_events.len(),
        all_events.len()
    ));

    for event in all_events {
        let badge_class = format!("badge-{}", escape_html(&event.kind.to_lowercase()));
        out.push_str(&format!("<div class=\"timeline-item\"><span class=\"timeline-time\">{}</span><span class=\"timeline-kind kind-{}\"><span class=\"badge {}\">{}</span></span><span class=\"timeline-summary\"><code>{}</code> {}</span>",
            format_timestamp(Some(&event.created_at)),
            escape_html(&event.kind.to_lowercase()),
            badge_class,
            escape_html(&event.kind),
            escape_html(&event.agent_id),
            truncate_html(&event.summary, 150),
        ));
        if let Some(ref pref) = event.payload_ref {
            out.push_str(&format!(" <a href=\"report-data/{}\" style=\"font-size:0.75rem;color:var(--accent);\">[payload]</a>", escape_html(pref)));
        }
        out.push_str("</div>\n");
    }

    out.push_str("<h2>Agent Details</h2>\n");
    for agent in &agents {
        out.push_str(&format!("<div class=\"agent-card\">\n"));
        out.push_str(&format!("<div class=\"agent-header\"><h3><code>{}</code></h3><span class=\"badge badge-{}\">{}</span></div>\n",
            escape_html(&agent.agent_id), status_to_badge_class(&agent.status), agent.status));
        out.push_str("<div class=\"meta-row\">\n");
        out.push_str(&format!(
            "<span><strong>Session:</strong> <code>{}</code></span>\n",
            escape_html(short_session_id(&agent.session_id))
        ));
        out.push_str(&format!(
            "<span><strong>Turns:</strong> {}</span>\n",
            agent.turn_count
        ));
        out.push_str(&format!(
            "<span><strong>Tools:</strong> {}</span>\n",
            agent.tool_count
        ));
        out.push_str(&format!(
            "<span><strong>Errors:</strong> {}</span>\n",
            agent.error_count
        ));
        out.push_str(&format!(
            "<span><strong>Approvals:</strong> {}</span>\n",
            agent.approval_count
        ));
        out.push_str("</div>\n");
        if let Some(ref input) = agent.input_preview {
            let _ = writeln!(
                out,
                "<p><strong>Input:</strong></p><div class=\"preview\">{}</div>",
                escape_html(input)
            );
        }
        if let Some(ref output) = agent.output_preview {
            let _ = writeln!(
                out,
                "<p><strong>Output:</strong></p><div class=\"preview\">{}</div>",
                escape_html(output)
            );
        }
        if !agent.approvals.is_empty() {
            let _ = writeln!(
                out,
                "<details><summary>Approvals ({})</summary>",
                agent.approvals.len()
            );
            out.push_str("<table><thead><tr><th>Request ID</th><th>Kind</th><th>Status</th><th>Decision</th><th>Summary</th><th>Resolved</th></tr></thead><tbody>\n");
            for a in &agent.approvals {
                let status_cls = if a.status == "pending" {
                    "badge-awaiting_approval"
                } else {
                    "badge-completed"
                };
                let decision_html = a
                    .decision
                    .as_deref()
                    .map(|d| {
                        let cls = if d == "approved" {
                            "badge-completed"
                        } else {
                            "badge-failed"
                        };
                        format!("<span class=\"badge {}\">{}</span>", cls, escape_html(d))
                    })
                    .unwrap_or_else(|| "—".to_string());
                out.push_str(&format!("<tr><td><code>{}</code></td><td>{}</td><td><span class=\"badge {}\">{}</span></td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                    escape_html(&a.request_id), escape_html(&a.kind),
                    status_cls, a.status, decision_html,
                    truncate_html(&a.summary, 80),
                    a.resolved_at.as_deref().map(|t| format_timestamp(Some(t))).unwrap_or_else(|| "—".to_string()),
                ));
            }
            out.push_str("</tbody></table></details>\n");
        }
        if !agent.errors.is_empty() {
            let _ = writeln!(
                out,
                "<details><summary>Errors ({})</summary>",
                agent.errors.len()
            );
            out.push_str("<table><thead><tr><th>Time</th><th>Tool</th><th>Summary</th></tr></thead><tbody>\n");
            for e in &agent.errors {
                out.push_str(&format!(
                    "<tr><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                    format_timestamp(Some(&e.created_at)),
                    escape_html(e.tool_name.as_deref().unwrap_or("—")),
                    truncate_html(&e.summary, 100),
                ));
            }
            out.push_str("</tbody></table></details>\n");
        }
        if !agent.artifacts.is_empty() {
            let _ = writeln!(
                out,
                "<details><summary>Artifacts ({})</summary>",
                agent.artifacts.len()
            );
            for a in &agent.artifacts {
                out.push_str(&format!(
                    "<p><code>{}</code> via <code>{}</code>: {}</p>\n",
                    escape_html(&a.artifact_id),
                    escape_html(&a.tool_name),
                    truncate_html(&a.summary, 120)
                ));
            }
            out.push_str("</details>\n");
        }
        let tool_events: Vec<_> = state
            .timeline
            .iter()
            .filter(|e| e.session_id == agent.session_id && (e.kind == "ACTION" || e.kind == "RESULT" || e.kind == "ERROR"))
            .rev()
            .take(MAX_RECENT_EVENTS_PER_AGENT)
            .collect();
        if !tool_events.is_empty() {
            let _ = writeln!(
                out,
                "<details><summary>Tool Calls ({})</summary>",
                tool_events.len()
            );
            out.push_str("<table><thead><tr><th style=\"width:13%\">Time</th><th style=\"width:10%\">Kind</th><th>Summary</th></tr></thead><tbody>\n");
            for event in tool_events.into_iter().rev() {
                out.push_str(&format!("<tr><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                    format_timestamp(Some(&event.created_at)),
                    escape_html(&event.kind),
                    truncate_html(&event.summary, 150),
                ));
            }
            out.push_str("</tbody></table></details>\n");
        }
        let recent: Vec<_> = state
            .timeline
            .iter()
            .filter(|e| e.session_id == agent.session_id)
            .rev()
            .take(MAX_RECENT_EVENTS_PER_AGENT)
            .collect();
        if !recent.is_empty() {
            let _ = writeln!(
                out,
                "<details><summary>Recent Events ({})</summary>",
                recent.len()
            );
            for event in recent.into_iter().rev() {
                out.push_str(&format!("<div class=\"timeline-item\"><span class=\"timeline-time\">{}</span><span class=\"timeline-kind kind-{}\">{}</span><span class=\"timeline-summary\">{}</span></div>\n",
                    format_timestamp(Some(&event.created_at)),
                    event.kind.to_lowercase(),
                    event.kind,
                    truncate_html(&event.summary, 120),
                ));
            }
            out.push_str("</details>\n");
        }
        out.push_str("</div>\n");
    }

    out.push_str(&format!("\n<p style=\"color:var(--text-dim);font-size:0.8rem;margin-top:2rem;\">Generated at {} &bull; Session Report v{} &bull; <a href=\"session_report.json\" style=\"color:var(--accent);\">JSON</a> &bull; <a href=\"session_overview.md\" style=\"color:var(--accent);\">Overview</a></p>\n",
        state.generated_at, state.version));

    out.push_str("</body></html>");
    out
}

fn render_agent_tree_html(
    out: &mut String,
    state: &SessionReportState,
    parent: Option<&str>,
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    let mut agents: Vec<_> = state.agents.values().collect();
    agents.sort_by(|a, b| a.started_at.cmp(&b.started_at));

    for agent in &agents {
        let agent_parent = agent.parent_session_id.as_deref();
        if agent_parent != parent {
            continue;
        }
        let badge = format!(
            "<span class=\"badge badge-{}\">{}</span>",
            status_to_badge_class(&agent.status),
            agent.status
        );
        out.push_str(&format!("{}<div style=\"margin:0.25rem 0;\"><code>{}</code> ({}) — {} turns, {} tools, {} errors {}\n",
            indent, escape_html(&agent.agent_id), escape_html(short_session_id(&agent.session_id)),
            agent.turn_count, agent.tool_count, agent.error_count, badge));

        if agent_parent == parent {
            render_agent_tree_html(out, state, Some(&agent.session_id), depth + 1);
        }
        out.push_str("</div>\n");
    }
}

fn status_to_badge_class(status: &str) -> &str {
    match status {
        "completed" => "badge-completed",
        "failed" => "badge-failed",
        "suspended" => "badge-suspended",
        "running" => "badge-running",
        "awaiting_approval" => "badge-awaiting_approval",
        _ => "badge-running",
    }
}

fn truncate_html(s: &str, max: usize) -> String {
    if s.len() <= max {
        return escape_html(s);
    }
    let mut chars = s.chars();
    let chunk: String = chars.by_ref().take(max).collect();
    let escaped = escape_html(&chunk);
    if chars.next().is_some() {
        format!("{}…", escaped)
    } else {
        escaped
    }
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn sorted_agents(state: &SessionReportState) -> Vec<&AgentReport> {
    let mut agents: Vec<&AgentReport> = state.agents.values().collect();
    agents.sort_by(|a, b| {
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then_with(|| a.started_at.cmp(&b.started_at))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    agents
}

fn find_child_agent<'a>(
    state: &'a SessionReportState,
    parent_session_id: &str,
    target_agent: &str,
    delegation_index: usize,
) -> Option<&'a AgentReport> {
    let mut candidates: Vec<&'a AgentReport> = state
        .agents
        .values()
        .filter(|agent| {
            agent.parent_session_id.as_deref() == Some(parent_session_id)
                && agent.agent_id == target_agent
        })
        .collect();
    candidates.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    candidates.get(delegation_index).copied()
}

fn agents_by_tree<'a>(
    state: &'a SessionReportState,
    parent: Option<&str>,
) -> Vec<(usize, &'a AgentReport)> {
    let mut agents: Vec<&AgentReport> = state.agents.values().collect();
    agents.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    let mut result = Vec::new();
    for agent in agents {
        let agent_parent = agent.parent_session_id.as_deref();
        if agent_parent != parent {
            continue;
        }
        result.push((agent.depth, agent));
        result.extend(agents_by_tree(state, Some(&agent.session_id)));
    }
    result
}

/// Returns a parallel vec mapping each timeline entry to a global turn number (1-based).
/// The counter increments on every TURN event so all subsequent events carry the same turn number.
fn timeline_turn_numbers(state: &SessionReportState) -> Vec<u32> {
    let mut result = Vec::with_capacity(state.timeline.len());
    let mut n = 0u32;
    for e in &state.timeline {
        if e.kind == "TURN" {
            n += 1;
        }
        result.push(n);
    }
    result
}

/// Returns the (first_global_turn, last_global_turn) range for an agent, or None.
fn agent_turn_range(
    state: &SessionReportState,
    session_id: &str,
    turn_nums: &[u32],
) -> Option<(u32, u32)> {
    let mut first = None;
    let mut last = None;
    for (i, e) in state.timeline.iter().enumerate() {
        if e.kind == "TURN" && e.session_id == session_id {
            let t = turn_nums[i];
            if first.is_none() {
                first = Some(t);
            }
            last = Some(t);
        }
    }
    match (first, last) {
        (Some(f), Some(l)) => Some((f, l)),
        _ => None,
    }
}

/// Is a close reason not a clean finish?
fn is_abnormal_close(reason: &str) -> bool {
    let r = reason.to_lowercase();
    !r.contains("task_completed")
        && !r.contains("completed")
        && !r.contains("budget_reached")
        && !r.contains("max_turns")
}

/// Maximum run of consecutive ERROR events without an intervening successful RESULT.
fn consecutive_error_streak(state: &SessionReportState, session_id: &str) -> u32 {
    let mut max_streak = 0u32;
    let mut cur = 0u32;
    for e in &state.timeline {
        if e.session_id != session_id {
            continue;
        }
        match e.kind.as_str() {
            "ERROR" => {
                cur += 1;
                max_streak = max_streak.max(cur);
            }
            "RESULT" => {
                cur = 0;
            }
            _ => {}
        }
    }
    max_streak
}

/// Last `n` timeline events for an agent that occurred strictly before `before_ts`.
fn events_before<'a>(
    state: &'a SessionReportState,
    session_id: &str,
    before_ts: &str,
    n: usize,
) -> Vec<&'a ReportEvent> {
    state
        .timeline
        .iter()
        .filter(|e| e.session_id == session_id && e.created_at.as_str() < before_ts)
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Average inter-turn gap in seconds for an agent (None if fewer than 2 turns).
fn avg_turn_secs(state: &SessionReportState, session_id: &str) -> Option<u64> {
    let turns: Vec<&ReportEvent> = state
        .timeline
        .iter()
        .filter(|e| e.session_id == session_id && e.kind == "TURN")
        .collect();
    if turns.len() < 2 {
        return None;
    }
    let mut total = 0i64;
    let mut count = 0u32;
    for pair in turns.windows(2) {
        if let (Ok(t0), Ok(t1)) = (
            chrono::DateTime::parse_from_rfc3339(&pair[0].created_at),
            chrono::DateTime::parse_from_rfc3339(&pair[1].created_at),
        ) {
            let diff = (t1 - t0).num_seconds();
            if diff >= 0 {
                total += diff;
                count += 1;
            }
        }
    }
    if count == 0 {
        None
    } else {
        Some((total / count as i64) as u64)
    }
}

fn status_rank(status: &str) -> u8 {
    match status {
        "awaiting_approval" => 0,
        "failed" => 1,
        "running" => 2,
        "suspended" => 3,
        "completed" => 4,
        _ => 9,
    }
}

fn collect_open_blockers(
    state: &SessionReportState,
    include_resolved: bool,
) -> Vec<(String, String, String, String, Option<String>)> {
    let mut items = Vec::new();
    for agent in state.agents.values() {
        for error in &agent.errors {
            items.push((
                error.created_at.clone(),
                agent.agent_id.clone(),
                "error".to_string(),
                error.summary.clone(),
                None,
            ));
        }
        for approval in &agent.approvals {
            if include_resolved || approval.status == "pending" {
                items.push((
                    approval.created_at.clone(),
                    agent.agent_id.clone(),
                    "approval".to_string(),
                    approval.summary.clone(),
                    approval.resolution_summary.clone(),
                ));
            }
        }
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
}

struct ApprovalRow {
    created_at: String,
    agent_id: String,
    request_id: String,
    kind: String,
    status: String,
    decision: Option<String>,
    summary: String,
    reason: Option<String>,
    resolved_at: Option<String>,
    resolution_summary: Option<String>,
}

fn collect_all_approvals(state: &SessionReportState) -> Vec<ApprovalRow> {
    let mut rows = Vec::new();
    for agent in state.agents.values() {
        for a in &agent.approvals {
            rows.push(ApprovalRow {
                created_at: a.created_at.clone(),
                agent_id: agent.agent_id.clone(),
                request_id: a.request_id.clone(),
                kind: a.kind.clone(),
                status: a.status.clone(),
                decision: a.decision.clone(),
                summary: a.summary.clone(),
                reason: a.reason.clone(),
                resolved_at: a.resolved_at.clone(),
                resolution_summary: a.resolution_summary.clone(),
            });
        }
    }
    rows.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    rows
}

fn status_from_close_reason(reason: &str) -> &'static str {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("suspend") {
        "suspended"
    } else if lower.contains("fail") || lower.contains("error") {
        "failed"
    } else {
        "completed"
    }
}

fn extract_field_str<'a>(parsed: Option<&'a Value>, keys: &[&str]) -> Option<&'a str> {
    let value = parsed?;
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|candidate| candidate.as_str()))
}

fn truncate_json(value: &Value, max_len: usize) -> Value {
    let mut clone = value.clone();
    truncate_json_strings(&mut clone, max_len);
    clone
}

fn truncate_json_strings(value: &mut Value, max_len: usize) {
    match value {
        Value::String(s) => {
            if s.len() > max_len {
                let truncated: String = s.chars().take(max_len).collect();
                *s = format!("{}…", truncated);
            }
        }
        Value::Array(items) => {
            for item in items {
                truncate_json_strings(item, max_len);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                truncate_json_strings(value, max_len);
            }
        }
        _ => {}
    }
}

fn parse_truncated_json(input: &str, max_len: usize) -> Option<Value> {
    let value = serde_json::from_str::<Value>(input).ok()?;
    Some(truncate_json(&value, max_len))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let body = serde_json::to_string_pretty(value)?;
    write_string_atomic(path, &body)
}

fn write_string_atomic(path: &Path, body: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("write")
    ));
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn truncate_chars(s: &str, max: usize) -> String {
    let sanitized = sanitize_md_cell(s);
    let mut iter = sanitized.chars();
    let chunk: String = iter.by_ref().take(max).collect();
    if iter.next().is_some() {
        format!("{}…", chunk)
    } else {
        chunk
    }
}

fn sanitize_md_cell(s: &str) -> String {
    let mut out = String::new();
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        let stripped = trimmed.replace("**", "").replace('|', "\\|");
        out.push_str(stripped.trim());
    }
    out
}

fn format_timestamp(timestamp: Option<&str>) -> String {
    timestamp
        .and_then(|ts| ts.split('T').nth(1))
        .and_then(|rest| rest.split('.').next())
        .map(String::from)
        .unwrap_or_else(|| "—".to_string())
}

fn short_session_id(session_id: &str) -> &str {
    session_id.rsplit('/').next().unwrap_or(session_id)
}

fn format_duration(started_at: Option<&str>, ended_at: Option<&str>) -> String {
    let Some(start) = started_at else {
        return "—".to_string();
    };
    let Some(end) = ended_at else {
        return "—".to_string();
    };
    let Ok(start) = chrono::DateTime::parse_from_rfc3339(start) else {
        return "—".to_string();
    };
    let Ok(end) = chrono::DateTime::parse_from_rfc3339(end) else {
        return "—".to_string();
    };
    let secs = (end - start).num_seconds().max(0);
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m{:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn session_report_writes_live_and_final_views() {
        let tmp = tempdir().unwrap();
        let gateway_dir = tmp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();

        let mut writer = SessionReportWriter::open(
            &gateway_dir,
            "root/evaluator.default-abcd",
            "evaluator.default",
        )
        .unwrap();
        writer.start_session("Evaluate artifact art_123").unwrap();
        writer.start_turn(Some("turn-1")).unwrap();
        writer
            .record_tool_requested(
                "sandbox_exec",
                r#"{"command":"python3 /tmp/test_weather.py"}"#,
                Some("turn-1"),
            )
            .unwrap();
        writer
            .record_tool_completed(
                "sandbox_exec",
                r#"{"approval_required":true,"request_id":"apr-1","approval":{"kind":"sandbox_exec","summary":"remote access detected","reason":"api.open-meteo.com"},"ok":false}"#,
                None,
                Some("turn-1"),
                None,
            )
            .unwrap();
        writer
            .finish_session("session suspended awaiting approval", None)
            .unwrap();

        let session_dir = gateway_dir.join("sessions").join("root");
        let live = std::fs::read_to_string(session_dir.join("session_overview.md")).unwrap();
        let final_md = std::fs::read_to_string(session_dir.join("session_report.md")).unwrap();
        let final_json = std::fs::read_to_string(session_dir.join("session_report.json")).unwrap();

        assert!(live.contains("Active Agents"));
        assert!(live.contains("suspended"));
        assert!(live.contains("apr-1"));
        assert!(live.contains("Open Approvals"));
        assert!(final_md.contains("Agent Summary"));
        assert!(final_md.contains("## Errors"));
        assert!(final_md.contains("## Approvals"));
        assert!(final_json.contains("\"request_id\": \"apr-1\""));
    }
}
