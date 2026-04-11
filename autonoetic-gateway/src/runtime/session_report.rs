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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApprovalItem {
    request_id: String,
    status: String,
    kind: String,
    summary: String,
    reason: Option<String>,
    created_at: String,
    resolved_at: Option<String>,
    resolution_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ErrorItem {
    created_at: String,
    tool_name: Option<String>,
    summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactItem {
    created_at: String,
    tool_name: String,
    artifact_id: String,
    summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReportEvent {
    created_at: String,
    session_id: String,
    agent_id: String,
    turn_id: Option<String>,
    kind: String,
    summary: String,
    important: bool,
    details: Option<Value>,
}

pub struct SessionReportWriter {
    state_path: PathBuf,
    live_md_path: PathBuf,
    final_md_path: PathBuf,
    final_json_path: PathBuf,
    session_id: String,
    agent_id: String,
    depth: usize,
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
        }
    }
}

impl SessionReportWriter {
    pub fn open(gateway_dir: &Path, session_id: &str, agent_id: &str) -> anyhow::Result<Self> {
        let base = base_session_id(session_id);
        let dir = gateway_dir.join("sessions").join(base);
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            state_path: dir.join("session_report.live.json"),
            live_md_path: dir.join("session_overview.md"),
            final_md_path: dir.join("session_report.md"),
            final_json_path: dir.join("session_report.json"),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            depth: session_depth(session_id),
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
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: None,
                    kind: "SESSION".to_string(),
                    summary: "session started".to_string(),
                    important: true,
                    details: None,
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
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: "TURN".to_string(),
                    summary: format!("turn {}", turn_count),
                    important: false,
                    details: None,
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
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: "NOTE".to_string(),
                    summary,
                    important: false,
                    details: None,
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
            let kind = if is_poll_tool(tool_name) { "POLL" } else { "ACTION" };
            touch_agent(agent, kind, &summary, &now);
            push_event(
                state,
                ReportEvent {
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: kind.to_string(),
                    summary,
                    important: !is_poll_tool(tool_name),
                    details: parse_truncated_json(arguments_redacted, 200),
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
    ) -> anyhow::Result<()> {
        self.update_state(|state| {
            let now = chrono::Utc::now().to_rfc3339();
            let parsed = serde_json::from_str::<Value>(result_json).ok();
            let is_approval = parsed
                .as_ref()
                .and_then(|v| v.get("approval_required").and_then(|x| x.as_bool()))
                == Some(true);
            let ok = parsed
                .as_ref()
                .and_then(|v| v.get("ok").and_then(|x| x.as_bool()))
                != Some(false);

            let (kind, important, summary, details) = if is_approval {
                (
                    "APPROVAL",
                    true,
                    summarize_approval(parsed.as_ref()),
                    parsed.as_ref().map(|v| truncate_json(v, 200)),
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
                )
            } else if !ok {
                (
                    "ERROR",
                    true,
                    summarize_tool_error(tool_name, parsed.as_ref(), result_json),
                    parsed.as_ref().map(|v| truncate_json(v, 200)),
                )
            } else if is_poll_tool(tool_name) {
                let summary = summarize_tool_result(tool_name, parsed.as_ref(), result_json);
                let important = poll_result_is_important(tool_name, parsed.as_ref());
                (
                    "POLL",
                    important,
                    summary,
                    parsed.as_ref().map(|v| truncate_json(v, 200)),
                )
            } else {
                (
                    "RESULT",
                    true,
                    summarize_tool_result(tool_name, parsed.as_ref(), result_json),
                    parsed.as_ref().map(|v| truncate_json(v, 200)),
                )
            };

            let agent = ensure_agent(state, &self.session_id, &self.agent_id, self.depth);
            touch_agent(agent, kind, &summary, &now);

            if is_approval {
                agent.status = "awaiting_approval".to_string();
                agent.approval_count = agent.approval_count.saturating_add(1);
                upsert_approval(agent, parsed.as_ref(), &now);
            } else if let Some(request_id) = approval_ref {
                resolve_approval(agent, request_id, &summary, &now);
                if agent.status == "awaiting_approval" {
                    agent.status = "running".to_string();
                }
            } else if !ok {
                agent.error_count = agent.error_count.saturating_add(1);
                agent.errors.push(ErrorItem {
                    created_at: now.clone(),
                    tool_name: Some(tool_name.to_string()),
                    summary: summary.clone(),
                });
            } else {
                maybe_record_output(agent, tool_name, parsed.as_ref(), &summary, &now);
            }

            push_event(
                state,
                ReportEvent {
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: kind.to_string(),
                    summary,
                    important,
                    details,
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
            push_event(
                state,
                ReportEvent {
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: turn_id.map(String::from),
                    kind: "DELEGATE".to_string(),
                    summary,
                    important: true,
                    details: None,
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
            if let Some(text) = latest_assistant_output.map(str::trim).filter(|s| !s.is_empty()) {
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
                    created_at: now,
                    session_id: self.session_id.clone(),
                    agent_id: self.agent_id.clone(),
                    turn_id: None,
                    kind: "FINAL".to_string(),
                    summary: format!("session closed: {}", truncate_chars(reason, 200)),
                    important: true,
                    details: latest_assistant_output
                        .map(|s| Value::String(truncate_chars(&redact_text_for_logs(s), 800))),
                },
            );
        })
    }

    fn update_state<F>(&self, f: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut SessionReportState),
    {
        let _guard = SESSION_REPORT_LOCK.lock().unwrap();
        let mut state = self.load_state()?;
        f(&mut state);
        state.generated_at = chrono::Utc::now().to_rfc3339();
        let live_md = render_live_markdown(&state);
        write_json_atomic(&self.state_path, &state)?;
        write_string_atomic(&self.live_md_path, &live_md)?;
        Ok(())
    }

    fn update_state_and_finalize<F>(&self, f: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut SessionReportState),
    {
        let _guard = SESSION_REPORT_LOCK.lock().unwrap();
        let mut state = self.load_state()?;
        f(&mut state);
        state.generated_at = chrono::Utc::now().to_rfc3339();
        let live_md = render_live_markdown(&state);
        let final_md = render_final_markdown(&state);
        write_json_atomic(&self.state_path, &state)?;
        write_string_atomic(&self.live_md_path, &live_md)?;
        write_json_atomic(&self.final_json_path, &state)?;
        write_string_atomic(&self.final_md_path, &final_md)?;
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

fn touch_agent(agent: &mut AgentReport, kind: &str, summary: &str, timestamp: &str) {
    agent.last_event_at = Some(timestamp.to_string());
    agent.last_event_kind = Some(kind.to_string());
    agent.last_event_summary = Some(truncate_chars(summary, 500));
}

fn summarize_tool_request(tool_name: &str, arguments_redacted: &str) -> String {
    let parsed = serde_json::from_str::<Value>(arguments_redacted).ok();
    match tool_name {
        "sandbox.exec" => format!(
            "run {}",
            extract_field_str(parsed.as_ref(), &["command"]).unwrap_or("command")
        ),
        "web.search" => format!(
            "search {}",
            extract_field_str(parsed.as_ref(), &["query", "q"]).unwrap_or("query")
        ),
        "web.fetch" => format!(
            "fetch {}",
            extract_field_str(parsed.as_ref(), &["url"]).unwrap_or("url")
        ),
        "workflow.wait" => {
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
        "workflow.state" => "refresh workflow state".to_string(),
        "content.write" => format!(
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
        "workflow.wait" => summarize_workflow_wait(parsed),
        "workflow.state" => summarize_workflow_state(parsed),
        "artifact.build" | "artifact.inspect" => {
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
        "content.write" => format!(
            "wrote {}",
            extract_field_str(parsed, &["name", "sandbox_path", "handle"]).unwrap_or("content")
        ),
        "content.read" => {
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
        "web.search" => {
            let query = extract_field_str(parsed, &["query"]).unwrap_or("query");
            let count = parsed
                .and_then(|v| v.get("result_count").and_then(|x| x.as_u64()))
                .unwrap_or(0);
            format!("search `{}` -> {} result(s)", truncate_chars(query, 80), count)
        }
        "web.fetch" => {
            let url = extract_field_str(parsed, &["url"]).unwrap_or("url");
            let status = parsed
                .and_then(|v| v.get("status_code").and_then(|x| x.as_u64()))
                .unwrap_or(0);
            let truncated = parsed
                .and_then(|v| v.get("truncated").and_then(|x| x.as_bool()))
                .unwrap_or(false);
            if truncated {
                format!("fetch `{}` -> {} (truncated)", truncate_chars(url, 80), status)
            } else {
                format!("fetch `{}` -> {}", truncate_chars(url, 80), status)
            }
        }
        "sandbox.exec" => {
            let exit = parsed
                .and_then(|v| v.get("exit_code").and_then(|x| x.as_i64()))
                .unwrap_or(-1);
            let stdout = parsed
                .and_then(|v| v.get("stdout").and_then(|x| x.as_str()))
                .map(|s| truncate_chars(s.trim(), 100))
                .filter(|s| !s.is_empty());
            if let Some(stdout) = stdout {
                format!("command exit={} stdout=`{}`", exit, redact_text_for_logs(&stdout))
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
            if status == "AwaitingApproval" || status == "Running" || status == "Runnable" {
                let agent = task.get("agent_id").and_then(|x| x.as_str()).unwrap_or("agent");
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
        request_id: request_id.to_string(),
        status: "pending".to_string(),
        kind: kind.to_string(),
        summary: truncate_chars(&redact_text_for_logs(summary), 140),
        reason,
        created_at: now.to_string(),
        resolved_at: None,
        resolution_summary: None,
    });
}

fn resolve_approval(agent: &mut AgentReport, request_id: &str, summary: &str, now: &str) {
    if let Some(existing) = agent
        .approvals
        .iter_mut()
        .find(|approval| approval.request_id == request_id)
    {
        existing.status = "resolved".to_string();
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
        "artifact.build" | "artifact.inspect" => {
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
        "content.write" => {
            agent.output_preview = Some(truncate_chars(summary, 160));
        }
        "knowledge.store" => {
            if let Some(id) = extract_field_str(parsed, &["id"]) {
                agent.output_preview = Some(format!("stored knowledge `{}`", id));
            }
        }
        "sandbox.exec" | "web.search" | "web.fetch" => {
            agent.output_preview = Some(truncate_chars(summary, 180));
        }
        _ => {}
    }
}

fn is_poll_tool(tool_name: &str) -> bool {
    matches!(tool_name, "workflow.wait" | "workflow.state")
}

fn poll_result_is_important(tool_name: &str, parsed: Option<&Value>) -> bool {
    match tool_name {
        "workflow.wait" => {
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
        "workflow.state" => {
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
    let agents = sorted_agents(state);
    let open_blockers = collect_open_blockers(state, false);
    let recent_events: Vec<&ReportEvent> = state
        .timeline
        .iter()
        .filter(|e| e.important)
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
    let _ = writeln!(out, "| Status | `{}` |", state.status);
    let _ = writeln!(
        out,
        "| Started | {} |",
        state.started_at.as_deref().unwrap_or("—")
    );
    let _ = writeln!(
        out,
        "| Last updated | {} |",
        state.generated_at
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "## Active Agents");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| Agent | Session | Parent | Status | Started | Last Event | Output | Errors |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---:|");
    for agent in agents {
        let _ = writeln!(
            out,
            "| `{}` | `{}` | `{}` | `{}` | {} | {} | {} | {} |",
            agent.agent_id,
            short_session_id(&agent.session_id),
            agent.parent_session_id
                .as_deref()
                .map(short_session_id)
                .unwrap_or("—"),
            agent.status,
            format_timestamp(agent.started_at.as_deref()),
            truncate_chars(agent.last_event_summary.as_deref().unwrap_or("—"), 70),
            truncate_chars(agent.output_preview.as_deref().unwrap_or("—"), 50),
            agent.error_count
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Open Blockers");
    let _ = writeln!(out);
    if open_blockers.is_empty() {
        let _ = writeln!(out, "_(none)_");
    } else {
        let _ = writeln!(out, "| Time | Agent | Type | Summary |");
        let _ = writeln!(out, "|---|---|---|---|");
        for blocker in open_blockers {
            let _ = writeln!(
                out,
                "| {} | `{}` | {} | {} |",
                format_timestamp(Some(&blocker.0)),
                blocker.1,
                blocker.2,
                truncate_chars(&blocker.3, 110)
            );
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Recent Important Events");
    let _ = writeln!(out);
    if recent_events.is_empty() {
        let _ = writeln!(out, "_(none yet)_");
    } else {
        let _ = writeln!(out, "| Time | Agent | Kind | Summary |");
        let _ = writeln!(out, "|---|---|---|---|");
        for event in recent_events.into_iter().rev() {
            let _ = writeln!(
                out,
                "| {} | `{}` | {} | {} |",
                format_timestamp(Some(&event.created_at)),
                event.agent_id,
                event.kind,
                truncate_chars(&event.summary, 120)
            );
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Agent Details");
    let _ = writeln!(out);
    for agent in sorted_agents(state) {
        let _ = writeln!(
            out,
            "### {} `{}`",
            agent.agent_id,
            short_session_id(&agent.session_id)
        );
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "- Status: `{}` | Started: {} | Ended: {} | Turns: {} | Tools: {} | Errors: {}",
            agent.status,
            agent.started_at.as_deref().unwrap_or("—"),
            agent.ended_at.as_deref().unwrap_or("—"),
            agent.turn_count,
            agent.tool_count,
            agent.error_count
        );
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
        let unresolved: Vec<&ApprovalItem> = agent
            .approvals
            .iter()
            .filter(|approval| approval.status == "pending")
            .collect();
        if !unresolved.is_empty() {
            let _ = writeln!(out, "- Pending approvals:");
            for approval in unresolved {
                let _ = writeln!(
                    out,
                    "  - `{}` {}",
                    approval.request_id,
                    truncate_chars(&approval.summary, 120)
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
                    truncate_chars(&artifact.summary, 120)
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
                    truncate_chars(&event.summary, 120)
                );
            }
        }
        let _ = writeln!(out);
    }
    out
}

fn render_final_markdown(state: &SessionReportState) -> String {
    let mut out = String::new();
    let agents = sorted_agents(state);
    let blockers = collect_open_blockers(state, true);
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
        "| Agent | Session | Parent | Started | Ended | Duration | Status | Input | Output | Errors |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|---:|");
    for agent in agents {
        let _ = writeln!(
            out,
            "| `{}` | `{}` | `{}` | {} | {} | {} | `{}` | {} | {} | {} |",
            agent.agent_id,
            short_session_id(&agent.session_id),
            agent.parent_session_id
                .as_deref()
                .map(short_session_id)
                .unwrap_or("—"),
            format_timestamp(agent.started_at.as_deref()),
            format_timestamp(agent.ended_at.as_deref()),
            format_duration(agent.started_at.as_deref(), agent.ended_at.as_deref()),
            agent.status,
            truncate_chars(agent.input_preview.as_deref().unwrap_or("—"), 48),
            truncate_chars(agent.output_preview.as_deref().unwrap_or("—"), 48),
            agent.error_count
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Errors And Approvals");
    let _ = writeln!(out);
    if blockers.is_empty() {
        let _ = writeln!(out, "_(none)_");
    } else {
        let _ = writeln!(out, "| Time | Agent | Type | Summary | Resolution |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        for blocker in blockers {
            let _ = writeln!(
                out,
                "| {} | `{}` | {} | {} | {} |",
                format_timestamp(Some(&blocker.0)),
                blocker.1,
                blocker.2,
                truncate_chars(&blocker.3, 100),
                blocker.4.unwrap_or_else(|| "—".to_string())
            );
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Outputs");
    let _ = writeln!(out);
    let artifact_rows: Vec<(&AgentReport, &ArtifactItem)> = state
        .agents
        .values()
        .flat_map(|agent| agent.artifacts.iter().map(move |artifact| (agent, artifact)))
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
                truncate_chars(&artifact.summary, 120)
            );
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Recent Important Events");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Time | Agent | Kind | Summary |");
    let _ = writeln!(out, "|---|---|---|---|");
    for event in state
        .timeline
        .iter()
        .filter(|event| event.important)
        .rev()
        .take(24)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | {} |",
            format_timestamp(Some(&event.created_at)),
            event.agent_id,
            event.kind,
            truncate_chars(&event.summary, 120)
        );
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
                *s = format!("{}…", &s[..max_len]);
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
    let mut iter = s.chars();
    let chunk: String = iter.by_ref().take(max).collect();
    if iter.next().is_some() {
        format!("{}…", chunk)
    } else {
        chunk
    }
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
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
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

        let mut writer =
            SessionReportWriter::open(&gateway_dir, "root/evaluator.default-abcd", "evaluator.default")
                .unwrap();
        writer.start_session("Evaluate artifact art_123").unwrap();
        writer.start_turn(Some("turn-1")).unwrap();
        writer
            .record_tool_requested(
                "sandbox.exec",
                r#"{"command":"python3 /tmp/test_weather.py"}"#,
                Some("turn-1"),
            )
            .unwrap();
        writer
            .record_tool_completed(
                "sandbox.exec",
                r#"{"approval_required":true,"request_id":"apr-1","approval":{"kind":"sandbox_exec","summary":"remote access detected","reason":"api.open-meteo.com"},"ok":false}"#,
                None,
                Some("turn-1"),
            )
            .unwrap();
        writer.finish_session("session suspended awaiting approval", None).unwrap();

        let session_dir = gateway_dir.join("sessions").join("root");
        let live = std::fs::read_to_string(session_dir.join("session_overview.md")).unwrap();
        let final_md = std::fs::read_to_string(session_dir.join("session_report.md")).unwrap();
        let final_json =
            std::fs::read_to_string(session_dir.join("session_report.json")).unwrap();

        assert!(live.contains("Active Agents"));
        assert!(live.contains("suspended"));
        assert!(live.contains("apr-1"));
        assert!(final_md.contains("Agent Summary"));
        assert!(final_md.contains("Errors And Approvals"));
        assert!(final_json.contains("\"request_id\": \"apr-1\""));
    }
}
