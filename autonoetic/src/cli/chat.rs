//! TUI Chat interface using ratatui + crossterm.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use tokio::net::TcpStream;

use crossterm::{
    cursor::Show,
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame, Terminal,
};
use unicode_width::UnicodeWidthStr;

use super::common::{
    default_terminal_channel_id, default_terminal_sender_id, terminal_channel_envelope,
};
use autonoetic_gateway::router::{
    JsonRpcRequest as GatewayJsonRpcRequest, JsonRpcResponse as GatewayJsonRpcResponse,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction, UserInteraction};
use autonoetic_types::config::GatewayConfig;

// ============================================================================
// Constants
// ============================================================================

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Soft cap per field when printing approvals in the TUI (avoids runaway memory on pathological payloads).
const CHAT_APPROVAL_FIELD_MAX_CHARS: usize = 16_384;

/// Status bar preview of `latest_signal` (first line); main transcript shows full messages.
const STATUS_BAR_EVENT_PREVIEW_CHARS: usize = 160;
const RECONNECT_NOTICE_BASE_ATTEMPTS: u32 = 3;
const RIGHT_PANE_WIDTH: u16 = 44;
const MIN_MAIN_MESSAGES_WIDTH: u16 = 60;

/// If `handle_chat` enables raw mode / alternate screen then exits early (missing env, I/O error,
/// `run_loop` failure), the tty stays raw with echo off unless we restore it—keyboard input looks
/// "invisible" until `reset`.
struct ChatTerminalRestore;

impl Drop for ChatTerminalRestore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = execute!(out, LeaveAlternateScreen, DisableMouseCapture);
        let _ = execute!(out, Show);
    }
}

// ============================================================================
// App State
// ============================================================================

#[derive(Debug, Clone)]
enum MessageRole {
    User,
    Assistant,
    System,
    Signal,
    SignalLow,
    AgentOutput,
}

#[derive(Debug, Clone)]
struct ChatMessage {
    role: MessageRole,
    content: String,
}

struct PendingRequest {
    id: u64,
    sent_at: Instant,
}

#[derive(Debug, Clone)]
struct SignalResumeRef {
    signal_session_id: String,
    request_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WorkflowOverview {
    workflow_id: Option<String>,
    status: String,
    running: usize,
    queued: usize,
    awaiting: usize,
    done: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SessionOverview {
    root_session_id: String,
    workflow: WorkflowOverview,
    pending_user_interactions: usize,
    latest_signal: Option<String>,
}

impl SessionOverview {
    fn status_line(&self) -> String {
        let workflow = if let Some(workflow_id) = &self.workflow.workflow_id {
            format!(
                "wf:{} {} | run:{} queue:{} wait:{} done:{}",
                workflow_id,
                self.workflow.status,
                self.workflow.running,
                self.workflow.queued,
                self.workflow.awaiting,
                self.workflow.done
            )
        } else {
            let root = if self.root_session_id.len() > 16 {
                format!("{}...", &self.root_session_id[..16])
            } else {
                self.root_session_id.clone()
            };
            format!("workflow: n/a (session: {})", root)
        };

        let ask = if self.pending_user_interactions > 0 {
            format!(" | ask:{}", self.pending_user_interactions)
        } else {
            String::new()
        };

        let latest_signal = self
            .latest_signal
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                let compact = value.lines().next().unwrap_or(value).trim();
                let shortened: String = compact
                    .chars()
                    .take(STATUS_BAR_EVENT_PREVIEW_CHARS)
                    .collect();
                if compact.chars().count() > STATUS_BAR_EVENT_PREVIEW_CHARS {
                    format!(" | event:{}…", shortened)
                } else {
                    format!(" | event:{}", shortened)
                }
            })
            .unwrap_or_default();

        format!("{}{}{}", workflow, ask, latest_signal)
    }
}

#[derive(Debug, Clone)]
struct SessionPollSnapshot {
    overview: SessionOverview,
    pending_interactions: Vec<UserInteraction>,
}

#[derive(Debug, Clone, Default)]
struct LiveTaskSummary {
    task_id: String,
    agent_id: String,
    status: String,
}

struct App {
    messages: Vec<ChatMessage>,
    input: String,
    cursor_pos: usize,
    pending: Vec<PendingRequest>,
    next_id: u64,
    spinner_frame: usize,
    scroll_offset: usize,
    last_max_scroll_offset: usize,
    follow_output: bool,
    session_paused: bool,
    esc_cancel_armed_until: Option<Instant>,
    session_id: String,
    target_hint: String,
    // Mouse selection - stored as CONTENT positions (row, col), not screen positions
    selecting: bool,
    sel_start: Option<(usize, usize)>, // (content_row, content_col)
    sel_end: Option<(usize, usize)>,   // (content_row, content_col)
    signal_resume_by_internal_id: HashMap<u64, SignalResumeRef>,
    signal_resume_inflight: HashSet<String>,
    seen_workflow_event_ids: HashSet<String>,
    bootstrapped_workflow_ids: HashSet<String>,
    current_workflow_id: Option<String>,
    session_overview: SessionOverview,
    live_tasks: Vec<LiveTaskSummary>,
    /// `user.ask` cards we already showed for this TUI session (avoid duplicate polls).
    seen_user_interaction_prompts: HashSet<String>,
    // Persistent clipboard — must stay alive so arboard's background ownership
    // thread keeps running and clipboard managers have time to capture the content.
    clipboard: Option<arboard::Clipboard>,
    /// Inline approvals: pending approval request IDs from workflow events and gateway store sync.
    /// Populated when `chat.inline_approvals` is enabled in config.
    pending_approval_ids: Vec<String>,
    /// Whether inline approvals are enabled (from `config.chat.inline_approvals`).
    inline_approvals_enabled: bool,
    /// Store-derived approval IDs we already announced (avoid repeating every poll).
    announced_store_approval_ids: HashSet<String>,
}

impl App {
    fn new(session_id: String, target_hint: String) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            pending: Vec::new(),
            next_id: 1,
            spinner_frame: 0,
            scroll_offset: 0,
            last_max_scroll_offset: 0,
            follow_output: true,
            session_paused: false,
            esc_cancel_armed_until: None,
            session_id,
            target_hint,
            selecting: false,
            sel_start: None,
            sel_end: None,
            signal_resume_by_internal_id: HashMap::new(),
            signal_resume_inflight: HashSet::new(),
            seen_workflow_event_ids: HashSet::new(),
            bootstrapped_workflow_ids: HashSet::new(),
            current_workflow_id: None,
            session_overview: SessionOverview::default(),
            live_tasks: Vec::new(),
            seen_user_interaction_prompts: HashSet::new(),
            // Safe clipboard initialization - arboard can panic on headless/SSH systems
            clipboard: std::panic::catch_unwind(|| arboard::Clipboard::new().ok()).unwrap_or(None),
            pending_approval_ids: Vec::new(),
            inline_approvals_enabled: false,
            announced_store_approval_ids: HashSet::new(),
        }
    }

    fn add_message(&mut self, role: MessageRole, content: String) {
        self.messages.push(ChatMessage { role, content });
        if self.follow_output {
            self.scroll_offset = self.last_max_scroll_offset;
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn add_pending(&mut self, id: u64) {
        self.pending.push(PendingRequest {
            id,
            sent_at: Instant::now(),
        });
    }

    fn remove_pending(&mut self, id: u64) {
        self.pending.retain(|r| r.id != id);
    }

    fn oldest_secs(&self) -> u64 {
        self.pending
            .iter()
            .map(|r| r.sent_at.elapsed().as_secs())
            .max()
            .unwrap_or(0)
    }

    fn tick_spinner(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
    }

    fn spinner(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_frame]
    }

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    fn delete_char(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.input[..self.cursor_pos].chars().last().unwrap();
            let len = prev.len_utf8();
            self.cursor_pos -= len;
            self.input.remove(self.cursor_pos);
        }
    }

    fn cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.input[..self.cursor_pos].chars().last().unwrap();
            self.cursor_pos -= prev.len_utf8();
        }
    }

    fn cursor_right(&mut self) {
        if self.cursor_pos < self.input.len() {
            let next = self.input[self.cursor_pos..].chars().next().unwrap();
            self.cursor_pos += next.len_utf8();
        }
    }

    fn content_line_count(&self, content_width: u16) -> usize {
        let content_width = content_width.max(1) as usize;
        let mut count = 0usize;
        for msg in &self.messages {
            let icon = match msg.role {
                MessageRole::User => "> ",
                MessageRole::Assistant => "🤖 ",
                MessageRole::System => "ℹ ",
                MessageRole::Signal => "🔔 ",
                MessageRole::SignalLow => "  ",
                MessageRole::AgentOutput => "📝 ",
            };

            for (i, text_line) in msg.content.lines().enumerate() {
                let prefix = if i == 0 { icon } else { "  " };
                count = count.saturating_add(wrapped_visual_line_count(
                    prefix,
                    text_line,
                    content_width,
                ));
            }
            count = count.saturating_add(1);
        }
        if !self.pending.is_empty() {
            let pending_text = format!(
                "{} Working... ({} pending, {}s)",
                self.spinner(),
                self.pending.len(),
                self.oldest_secs()
            );
            count = count.saturating_add(wrapped_visual_line_count("", &pending_text, content_width));
        }
        count
    }

    fn scroll_messages_up(&mut self, lines: usize) {
        if self.follow_output {
            self.scroll_offset = self.last_max_scroll_offset;
            self.follow_output = false;
        }
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    fn scroll_messages_down(&mut self, lines: usize) {
        let next = self.scroll_offset.saturating_add(lines);
        if next >= self.last_max_scroll_offset {
            self.scroll_offset = self.last_max_scroll_offset;
            self.follow_output = true;
        } else {
            self.scroll_offset = next;
        }
    }

    fn effective_scroll_offset(&self) -> usize {
        if self.follow_output {
            self.last_max_scroll_offset
        } else {
            self.scroll_offset.min(self.last_max_scroll_offset)
        }
    }

    fn cancel_armed(&self) -> bool {
        self.esc_cancel_armed_until
            .map(|deadline| Instant::now() <= deadline)
            .unwrap_or(false)
    }

    fn arm_cancel_window(&mut self) {
        self.esc_cancel_armed_until = Some(Instant::now() + Duration::from_secs(2));
    }

    fn disarm_cancel_window(&mut self) {
        self.esc_cancel_armed_until = None;
    }
}

fn hydrate_session_history(
    app: &mut App,
    config: &autonoetic_types::config::GatewayConfig,
    session_id: &str,
) -> anyhow::Result<usize> {
    let gateway_dir = config.agents_dir.join(".gateway");
    let store = autonoetic_gateway::runtime::content_store::ContentStore::new(&gateway_dir)?;
    let handle = match store.resolve_name_with_root(session_id, "session_history") {
        Ok(handle) => handle,
        Err(_) => return Ok(0),
    };

    let history_json = store.read_string(&handle)?;
    let history: Vec<autonoetic_gateway::llm::Message> = serde_json::from_str(&history_json)
        .map_err(|e| {
            anyhow::anyhow!("Invalid session_history payload for {}: {}", session_id, e)
        })?;

    let mut restored = 0usize;
    for msg in history {
        match msg.role {
            autonoetic_gateway::llm::Role::User => {
                if !msg.content.trim().is_empty() {
                    app.add_message(MessageRole::User, msg.content);
                    restored += 1;
                }
            }
            autonoetic_gateway::llm::Role::Assistant => {
                if !msg.content.trim().is_empty() {
                    app.add_message(MessageRole::Assistant, msg.content);
                    restored += 1;
                }
            }
            autonoetic_gateway::llm::Role::System => {
                if !msg.content.trim().is_empty() {
                    app.add_message(MessageRole::System, msg.content);
                    restored += 1;
                }
            }
            autonoetic_gateway::llm::Role::Tool => {}
        }
    }

    Ok(restored)
}

/// Pending `user.ask` rows for this terminal session: exact session plus any under the same root
/// (planner chat can surface child-session questions).
fn list_pending_user_interactions_for_terminal_session(
    store: &GatewayStore,
    session_id: &str,
) -> anyhow::Result<Vec<UserInteraction>> {
    let root = autonoetic_gateway::runtime::content_store::root_session_id(session_id);
    let mut by_id: HashMap<String, UserInteraction> = HashMap::new();
    for i in store.get_pending_interactions_for_session(session_id)? {
        by_id.insert(i.interaction_id.clone(), i);
    }
    for i in store.get_pending_interactions_for_root_session(&root)? {
        by_id.entry(i.interaction_id.clone()).or_insert(i);
    }
    let mut v: Vec<_> = by_id.into_values().collect();
    v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(v)
}

fn poll_session_snapshot(
    config: &autonoetic_types::config::GatewayConfig,
    store: Option<&GatewayStore>,
    session_id: &str,
    previous_latest_signal: Option<String>,
) -> anyhow::Result<SessionPollSnapshot> {
    let root_session_id = autonoetic_gateway::runtime::content_store::root_session_id(session_id);
    let pending_interactions = match store {
        Some(store) => list_pending_user_interactions_for_terminal_session(store, session_id)?,
        None => Vec::new(),
    };

    let workflow_id = autonoetic_gateway::scheduler::resolve_workflow_id_for_root_session(
        config,
        &root_session_id,
    )?;

    let workflow = if let Some(workflow_id) = workflow_id {
        let status = autonoetic_gateway::scheduler::load_workflow_run(config, None, &workflow_id)
            .ok()
            .flatten()
            .map(|run| format!("{:?}", run.status).to_lowercase())
            .unwrap_or_else(|| "unknown".to_string());

        let mut running = 0usize;
        let mut queued = 0usize;
        let mut awaiting = 0usize;
        let mut done = 0usize;

        if let Ok(tasks) =
            autonoetic_gateway::scheduler::list_task_runs_for_workflow(config, None, &workflow_id)
        {
            for task in tasks {
                match task.status {
                    autonoetic_types::workflow::TaskRunStatus::Pending => queued += 1,
                    autonoetic_types::workflow::TaskRunStatus::Runnable
                    | autonoetic_types::workflow::TaskRunStatus::Running => running += 1,
                    autonoetic_types::workflow::TaskRunStatus::AwaitingApproval => awaiting += 1,
                    autonoetic_types::workflow::TaskRunStatus::Succeeded
                    | autonoetic_types::workflow::TaskRunStatus::Failed
                    | autonoetic_types::workflow::TaskRunStatus::Cancelled
                    | autonoetic_types::workflow::TaskRunStatus::Aborted => done += 1,
                    autonoetic_types::workflow::TaskRunStatus::Paused => {}
                    autonoetic_types::workflow::TaskRunStatus::Aborting => running += 1,
                }
            }
        }

        WorkflowOverview {
            workflow_id: Some(workflow_id),
            status,
            running,
            queued,
            awaiting,
            done,
        }
    } else {
        WorkflowOverview::default()
    };

    Ok(SessionPollSnapshot {
        overview: SessionOverview {
            root_session_id: root_session_id.to_string(),
            workflow,
            pending_user_interactions: pending_interactions.len(),
            latest_signal: previous_latest_signal,
        },
        pending_interactions,
    })
}

/// Multi-line card for the TUI (Signal role), mirroring structured approval cards.
fn format_user_interaction_prompt(interaction: &UserInteraction) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "User input required — {}",
        interaction.interaction_id
    ));
    lines.push(format!("kind: {}", interaction.kind.as_str()));
    lines.push(format!("question: {}", interaction.question));
    if let Some(ctx) = &interaction.context {
        if !ctx.trim().is_empty() {
            lines.push(String::new());
            lines.push("context:".to_string());
            for ln in ctx.lines() {
                lines.push(format!("  {}", ln));
            }
        }
    }
    if !interaction.options.is_empty() {
        lines.push(String::new());
        lines.push("Options (use --option with the id):".to_string());
        for (n, o) in interaction.options.iter().enumerate() {
            lines.push(format!("  {}. [{}] {} → {}", n + 1, o.id, o.label, o.value));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "freeform: {}",
        if interaction.allow_freeform {
            "allowed (see --text)"
        } else {
            "not allowed — choose an option id"
        }
    ));
    lines.push(String::new());
    lines.push("Answer (CLI):".to_string());
    if !interaction.options.is_empty() {
        lines.push(format!(
            "  autonoetic gateway interactions answer --interaction-id {} --option <id>",
            interaction.interaction_id
        ));
    }
    if interaction.allow_freeform {
        lines.push(format!(
            "  autonoetic gateway interactions answer --interaction-id {} --text \"…\"",
            interaction.interaction_id
        ));
    }
    lines.join("\n")
}

/// Append structured cards for new pending interactions. Returns how many were added.
fn append_new_pending_user_interaction_prompts(
    app: &mut App,
    pending: &[UserInteraction],
) -> usize {
    let mut added = 0usize;
    for interaction in pending {
        if app
            .seen_user_interaction_prompts
            .contains(&interaction.interaction_id)
        {
            continue;
        }
        app.seen_user_interaction_prompts
            .insert(interaction.interaction_id.clone());
        let card = format_user_interaction_prompt(&interaction);
        app.session_overview.latest_signal =
            Some(format!("user.ask {}", interaction.interaction_id));
        app.add_message(MessageRole::Signal, card);
        added += 1;
    }
    added
}

fn signal_resume_key(signal_session_id: &str, request_id: &str) -> String {
    format!("{}::{}", signal_session_id, request_id)
}

fn format_workflow_event_card(
    event: &autonoetic_types::workflow::WorkflowEventRecord,
) -> Option<(String, MessageRole)> {
    let ts_short: String = event.occurred_at.chars().take(19).collect();
    let task = event.task_id.as_deref().unwrap_or("-");
    let status = event
        .payload
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let approval = event
        .payload
        .get("approval")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let agent_id = event
        .payload
        .get("agent_id")
        .and_then(|v| v.as_str())
        .or_else(|| event.agent_id.as_deref())
        .unwrap_or("");
    let agent_suffix = if agent_id.is_empty() {
        String::new()
    } else {
        format!(" → {}", agent_id)
    };

    let result = match event.event_type.as_str() {
        "workflow.started" => Some((format!("📋 [{}] Workflow started", ts_short), MessageRole::Signal)),
        "task.spawned" => {
            let target = event
                .payload
                .get("target_agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or(agent_id);
            Some((
                format!("🚀 [{}] Task spawned: {} → {}", ts_short, task, target),
                MessageRole::Signal,
            ))
        }
        "task.queued" => Some((
            format!("📥 [{}] Task queued: {}{}", ts_short, task, agent_suffix),
            MessageRole::Signal,
        )),
        "task.awaiting_approval" => {
            let kind = if approval.contains("sandbox") {
                "sandbox_exec".to_string()
            } else if approval.contains("agent_install") {
                "legacy install approval".to_string()
            } else {
                "tool execution".to_string()
            };
            let apr_id = event
                .payload
                .get("approval_request_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let reason = event
                .payload
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let apr_suffix = if apr_id.is_empty() {
                String::new()
            } else {
                let reason_part = if reason.is_empty() {
                    String::new()
                } else {
                    let mut block = String::from("\n   Reason:");
                    for ln in reason.lines() {
                        block.push_str(&format!("\n   {}", ln));
                    }
                    block
                };
                format!(
                    "{}\n   → Approve: autonoetic gateway approvals approve {}\n   → After approval, execution resumes automatically — no retry needed.",
                    reason_part, apr_id
                )
            };
            Some((
                format!("⏸ [{}] Suspended for approval: {} ({}){}", ts_short, task, kind, apr_suffix),
                MessageRole::Signal,
            ))
        }
        "task.approved" => Some((
            format!("✅ [{}] Approval granted — resuming: {}", ts_short, task),
            MessageRole::Signal,
        )),
        "task.rejected" => Some((
            format!("❌ [{}] Approval rejected: {}", ts_short, task),
            MessageRole::Signal,
        )),
        "task.approval_timeout" => {
            let reason = event
                .payload
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("Approval timed out");
            let timeout_secs = event
                .payload
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some((
                format!("⏰ [{}] Approval timed out: {} (after {}s)", ts_short, reason, timeout_secs),
                MessageRole::Signal,
            ))
        }
        "workflow.failure_threshold_reached" => {
            let count = event
                .payload
                .get("failed_task_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some((
                format!("🆘 [{}] Failure threshold reached: {} tasks failed", ts_short, count),
                MessageRole::Signal,
            ))
        }
        "workflow.escalated" => {
            let target = event
                .payload
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let urgency = event
                .payload
                .get("urgency")
                .and_then(|v| v.as_str())
                .unwrap_or("medium");
            let reason = event
                .payload
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let context = event
                .payload
                .get("context")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut line = format!(
                "🆘 [{}] Escalated to {} (urgency: {})",
                ts_short, target, urgency
            );
            if !reason.is_empty() {
                line.push_str(&format!("\n   Reason: {}", reason));
            }
            if !context.is_empty() {
                line.push_str(&format!("\n   Context: {}", context));
            }
            Some((line, MessageRole::Signal))
        }
        "task.started" => Some((
            format!("▶ [{}] Task started: {}{}", ts_short, task, agent_suffix),
            MessageRole::Signal,
        )),
        "task.completed" => {
            let result_summary = event
                .payload
                .get("result_summary")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if event.workflow_id.starts_with("sched-") && !result_summary.is_empty() {
                Some((
                    format!("🔔 [{}] {}: {}", ts_short, agent_id, result_summary),
                    MessageRole::AgentOutput,
                ))
            } else {
                Some((
                    format!("✅ [{}] Task completed: {}{}", ts_short, task, agent_suffix),
                    MessageRole::Signal,
                ))
            }
        }
        "task.failed" => Some((
            format!("❌ [{}] Task failed: {}{}", ts_short, task, agent_suffix),
            MessageRole::Signal,
        )),
        "task.cancelled" => Some((
            format!("🚫 [{}] Task cancelled: {}{}", ts_short, task, agent_suffix),
            MessageRole::Signal,
        )),
        "task.paused" => Some((
            format!("⏸ [{}] Task paused: {}{}", ts_short, task, agent_suffix),
            MessageRole::Signal,
        )),
        "workflow.join.satisfied" => Some((
            format!("✅ [{}] Workflow join satisfied", ts_short),
            MessageRole::SignalLow,
        )),
        "workflow.checkpoint.saved" => Some((
            format!("💾 [{}] Workflow checkpoint saved", ts_short),
            MessageRole::SignalLow,
        )),
        "task.checkpoint.saved" => Some((
            format!("💾 [{}] Task checkpoint saved: {}{}", ts_short, task, agent_suffix),
            MessageRole::SignalLow,
        )),
        "scheduled_job.triggered" => Some((
            format!("⏱ [{}] Scheduled job triggered: {}", ts_short, agent_id),
            MessageRole::AgentOutput,
        )),
        "task.updated" if status == "runnable" => Some((
            format!("🔁 [{}] Resumed after approval: {}{}", ts_short, task, agent_suffix),
            MessageRole::Signal,
        )),
        "task.updated" => Some((
            format!("🔄 [{}] Task updated: {}{} ({})", ts_short, task, agent_suffix, status),
            MessageRole::SignalLow,
        )),
        other => {
            Some((
                format!("⚡ [{}] {} (task: {})", ts_short, other, task),
                MessageRole::Signal,
            ))
        }
    };

    result
}

fn push_workflow_event_message(app: &mut App, role: MessageRole, card: String) {
    match role {
        MessageRole::SignalLow => {
            // Keep one rolling low-signal line instead of growing the transcript.
            if let Some(last_idx) = app
                .messages
                .iter()
                .rposition(|m| matches!(m.role, MessageRole::SignalLow))
            {
                app.messages[last_idx].content = card;
            } else {
                app.add_message(role, card);
            }
        }
        _ => {
            // Drop stale low-signal noise when meaningful events arrive.
            if let Some(last_idx) = app
                .messages
                .iter()
                .rposition(|m| matches!(m.role, MessageRole::SignalLow))
            {
                app.messages.remove(last_idx);
            }
            app.add_message(role, card);
        }
    }
}

// ============================================================================
// Approval request id extraction (apr-* and UUID fallback)
// ============================================================================

fn extract_approval_request_id(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    if !lower.contains("approval") && !lower.contains("approve") {
        return None;
    }
    let prefixes = ["request_id:", "request id:", "request_id :", "request id :"];
    for prefix in &prefixes {
        if let Some(start) = lower.find(prefix) {
            let after = &text[start + prefix.len()..].trim();
            if let Some(request_id) = extract_request_id(after) {
                return Some(request_id);
            }
        }
    }
    extract_request_id(text)
}

fn extract_request_id(text: &str) -> Option<String> {
    extract_short_approval_id(text).or_else(|| extract_uuid(text))
}

fn extract_short_approval_id(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i + 4 <= chars.len() {
        let is_prefix = chars[i].eq_ignore_ascii_case(&'a')
            && chars[i + 1].eq_ignore_ascii_case(&'p')
            && chars[i + 2].eq_ignore_ascii_case(&'r')
            && chars[i + 3] == '-';
        if !is_prefix {
            i += 1;
            continue;
        }

        let mut j = i + 4;
        while j < chars.len() && chars[j].is_ascii_hexdigit() {
            j += 1;
        }

        // Current approval IDs are short ids like apr-1234abcd.
        if j >= i + 12 {
            let before_ok = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
            let after_ok = j == chars.len() || !chars[j].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return Some(chars[i..j].iter().collect());
            }
        }

        i += 1;
    }
    None
}

fn extract_uuid(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if i + 8 <= chars.len() && chars[i..i + 8].iter().all(|c| c.is_ascii_hexdigit()) {
            let mut pos = i + 8;
            let segs = [4, 4, 12];
            let mut ok = true;
            for &len in &segs {
                if pos + 1 + len > chars.len() || chars[pos] != '-' {
                    ok = false;
                    break;
                }
                pos += 1;
                if !chars[pos..pos + len].iter().all(|c| c.is_ascii_hexdigit()) {
                    ok = false;
                    break;
                }
                pos += len;
            }
            if ok {
                return Some(chars[i..pos].iter().collect());
            }
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone)]
struct StructuredApprovalView {
    request_id: Option<String>,
    card: String,
}

fn json_array_to_csv(value: Option<&serde_json::Value>) -> Option<String> {
    let Some(serde_json::Value::Array(values)) = value else {
        return None;
    };
    let parts: Vec<String> = values
        .iter()
        .filter_map(|v| v.as_str().map(ToOwned::to_owned))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn extract_structured_approval(text: &str) -> Option<StructuredApprovalView> {
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let approval = parsed.get("approval")?;
    let kind = approval
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let summary = approval
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("Approval required");
    let reason = approval
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("Operator approval required");
    let retry_field = approval
        .get("retry_field")
        .and_then(|v| v.as_str())
        .unwrap_or("approval_ref");
    let request_id = parsed
        .get("request_id")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned);

    let subject = approval.get("subject").cloned().unwrap_or_default();
    let mut details = Vec::new();
    match kind {
        "sandbox_exec" => {
            if let Some(command) = subject.get("command").and_then(|v| v.as_str()) {
                details.push(format!("command: {}", command));
            }
            if let Some(hosts) = json_array_to_csv(subject.get("hosts")) {
                details.push(format!("hosts: {}", hosts));
            }
            if let Some(deps) = subject.get("dependencies") {
                let runtime = deps.get("runtime").and_then(|v| v.as_str()).unwrap_or("-");
                let packages = json_array_to_csv(deps.get("packages")).unwrap_or_default();
                if !packages.is_empty() {
                    details.push(format!("deps: {} ({})", runtime, packages));
                } else {
                    details.push(format!("deps: {}", runtime));
                }
            }
        }
        "agent_install" => {
            if let Some(agent_id) = subject.get("agent_id").and_then(|v| v.as_str()) {
                details.push(format!("agent: {}", agent_id));
            }
            if let Some(artifact_id) = subject.get("artifact_id").and_then(|v| v.as_str()) {
                details.push(format!("artifact: {}", artifact_id));
            }
            if let Some(risk_factors) = json_array_to_csv(subject.get("risk_factors")) {
                details.push(format!("risk: {}", risk_factors));
            }
            if let Some(capabilities) = json_array_to_csv(subject.get("capabilities")) {
                details.push(format!("capabilities: {}", capabilities));
            }
        }
        _ => {}
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "Approval required{}",
        request_id
            .as_ref()
            .map(|id| format!(": {}", id))
            .unwrap_or_default()
    ));
    lines.push(format!("kind: {}", kind));
    lines.push(format!("summary: {}", summary));
    lines.push(format!("reason: {}", reason));
    if !details.is_empty() {
        lines.push("subject:".to_string());
        for d in details {
            lines.push(format!("  {}", d));
        }
    }
    lines.push(format!("retry field: {}", retry_field));

    Some(StructuredApprovalView {
        request_id,
        card: lines.join("\n"),
    })
}

// ============================================================================
// Drawing
// ============================================================================

struct ChatLayout {
    status: Rect,
    separator: Rect,
    messages: Rect,
    right_pane: Option<Rect>,
    input: Rect,
}

fn compute_chat_layout(area: Rect) -> ChatLayout {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Status
            Constraint::Length(1), // Separator
            Constraint::Min(5),    // Body
            Constraint::Length(3), // Input
        ])
        .split(area);

    let body = rows[2];
    let right_pane_enabled = body.width > RIGHT_PANE_WIDTH + MIN_MAIN_MESSAGES_WIDTH;
    if right_pane_enabled {
        let body_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(MIN_MAIN_MESSAGES_WIDTH),
                Constraint::Length(RIGHT_PANE_WIDTH),
            ])
            .split(body);
        ChatLayout {
            status: rows[0],
            separator: rows[1],
            messages: body_cols[0],
            right_pane: Some(body_cols[1]),
            input: rows[3],
        }
    } else {
        ChatLayout {
            status: rows[0],
            separator: rows[1],
            messages: body,
            right_pane: None,
            input: rows[3],
        }
    }
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    if area.height < 8 || area.width < 20 {
        let p = Paragraph::new("Terminal too small — resize to continue")
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(p, area);
        return;
    }

    let layout = compute_chat_layout(area);

    draw_status(f, app, layout.status);

    let sep = Paragraph::new(Line::from(Span::styled(
        "─".repeat(layout.separator.width as usize),
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(sep, layout.separator);

    draw_messages(f, app, layout.messages);

    if let Some(right) = layout.right_pane {
        draw_right_pane(f, app, right);
    }

    draw_input(f, app, layout.input);

    let before_cursor_display_width = app.input[..app.cursor_pos].chars().count() as u16;
    let cursor_x = (layout.input.x + 2 + before_cursor_display_width)
        .min(layout.input.x + layout.input.width.saturating_sub(1));
    let cursor_y = layout.input.y + 1;
    f.set_cursor_position((cursor_x, cursor_y));
}

fn draw_right_pane(f: &mut Frame, app: &App, area: Rect) {
    let wf = &app.session_overview.workflow;
    let wf_id = wf
        .workflow_id
        .as_deref()
        .map(|id| {
            if id.len() > 18 {
                format!("{}…", &id[..18])
            } else {
                id.to_string()
            }
        })
        .unwrap_or_else(|| "n/a".to_string());

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled("Live Ops", Style::default().add_modifier(Modifier::BOLD))),
        Line::raw(format!("workflow: {}", wf_id)),
        Line::raw(format!("status: {}", if wf.status.is_empty() { "n/a" } else { &wf.status })),
        Line::raw(format!("run:{}  queue:{}  wait:{}  done:{}", wf.running, wf.queued, wf.awaiting, wf.done)),
        Line::raw(""),
        Line::raw(format!("pending RPC: {}", app.pending.len())),
        Line::raw(format!("approvals: {}", app.pending_approval_ids.len())),
        Line::raw(format!("questions: {}", app.session_overview.pending_user_interactions)),
        Line::raw(format!("follow: {}", if app.follow_output { "on" } else { "off" })),
        Line::raw(format!("paused: {}", if app.session_paused { "yes" } else { "no" })),
        Line::raw(""),
        Line::from(Span::styled("Active Agents", Style::default().add_modifier(Modifier::BOLD))),
    ];

    let mut active_count = 0usize;
    for task in app.live_tasks.iter().filter(|t| {
        matches!(
            t.status.as_str(),
            "running" | "runnable" | "awaiting_approval" | "pending"
        )
    }).take(8)
    {
        active_count += 1;
        let agent = if task.agent_id.len() > 20 {
            format!("{}…", &task.agent_id[..20])
        } else {
            task.agent_id.clone()
        };
        let task_short = if task.task_id.len() > 10 {
            format!("{}…", &task.task_id[..10])
        } else {
            task.task_id.clone()
        };
        let status_symbol = match task.status.as_str() {
            "running" => "▶",
            "awaiting_approval" => "⏸",
            "runnable" => "↻",
            "pending" => "…",
            _ => "·",
        };
        lines.push(Line::raw(format!("{} {} [{}]", status_symbol, agent, task_short)));
    }
    if active_count == 0 {
        lines.push(Line::raw("(no active tasks)"));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("Approvals", Style::default().add_modifier(Modifier::BOLD))));
    if app.pending_approval_ids.is_empty() {
        lines.push(Line::raw("none"));
    } else {
        for apr in app.pending_approval_ids.iter().rev().take(5) {
            lines.push(Line::raw(format!("- {}", apr)));
        }
    }

    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title("Workflow")
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
    f.render_widget(paragraph, area);
}

fn draw_messages(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let content_width = area.width.saturating_sub(1).max(1) as usize;
    let effective_scroll = app.effective_scroll_offset();
    // ratatui Paragraph::scroll uses u16. Keep a moving visual window so
    // very long transcripts still follow live output instead of wrapping.
    let visual_window_start = effective_scroll.saturating_sub(u16::MAX as usize);
    let mut render_base_visual_row: Option<usize> = None;
    let mut visual_row: usize = 0;
    // `row` is the absolute content-line index (0 = very first line of all messages).
    let mut row: usize = 0;

    // Selection bounds are stored as CONTENT coordinates (content_row, content_col).
    let (content_sel_top, content_sel_bot, sel_col_start_override, sel_col_end_override) =
        match (app.sel_start, app.sel_end) {
            (Some((r1, c1)), Some((r2, c2))) => {
                let lo_row = r1.min(r2);
                let hi_row = r1.max(r2);
                let lo_col = c1.min(c2);
                let hi_col = c1.max(c2);
                (lo_row, hi_row, lo_col, hi_col)
            }
            _ => (usize::MAX, usize::MAX, 0, 0),
        };

    for msg in &app.messages {
        let (icon, style) = match msg.role {
            MessageRole::User => ("> ", Style::default().fg(Color::Green)),
            MessageRole::Assistant => ("🤖 ", Style::default().fg(Color::Blue)),
            MessageRole::System => ("ℹ ", Style::default().fg(Color::Yellow)),
            MessageRole::Signal => ("🔔 ", Style::default().fg(Color::Cyan)),
            MessageRole::SignalLow => ("  ", Style::default().fg(Color::DarkGray)),
            MessageRole::AgentOutput => ("📝 ", Style::default().fg(Color::Magenta)),
        };

        for (i, text_line) in msg.content.lines().enumerate() {
            let prefix = if i == 0 { icon } else { "  " };
            let visual_line_count = wrapped_visual_line_count(prefix, text_line, content_width);
            let visual_line_end = visual_row.saturating_add(visual_line_count);
            let include_line = visual_line_end > visual_window_start;

            // Compare content row against selection bounds.
            let is_selected =
                row >= content_sel_top && row <= content_sel_bot && content_sel_top != usize::MAX;

            if include_line && is_selected {
                // For selected lines, render with highlight.
                // Column bounds only apply at the first and last selected lines.
                let sel_col_start = if row == content_sel_top {
                    sel_col_start_override
                } else {
                    0
                };
                let sel_col_end = if row == content_sel_bot {
                    sel_col_end_override
                } else {
                    text_line.len()
                };

                // Normalize selection order (handle backwards selection)
                let (sel_start, sel_end) = if sel_col_start <= sel_col_end {
                    (sel_col_start, sel_col_end)
                } else {
                    (sel_col_end, sel_col_start)
                };

                let mut spans: Vec<Span> = Vec::new();
                spans.push(Span::raw(prefix));

                let sel_start_clamped = sel_start.min(text_line.len());
                let sel_end_clamped = sel_end.min(text_line.len());

                let sel_start_clamped = if text_line.is_char_boundary(sel_start_clamped) {
                    sel_start_clamped
                } else {
                    (0..sel_start_clamped)
                        .rfind(|&i| text_line.is_char_boundary(i))
                        .unwrap_or(0)
                };
                let sel_end_clamped = if text_line.is_char_boundary(sel_end_clamped) {
                    sel_end_clamped
                } else {
                    (0..sel_end_clamped)
                        .rfind(|&i| text_line.is_char_boundary(i))
                        .unwrap_or(text_line.len())
                };

                let before_sel = &text_line[..sel_start_clamped];
                let in_sel = &text_line[sel_start_clamped..sel_end_clamped];
                let after_sel = &text_line[sel_end_clamped..];

                if !before_sel.is_empty() {
                    spans.push(Span::styled(before_sel.to_string(), style));
                }
                if !in_sel.is_empty() {
                    spans.push(Span::styled(in_sel.to_string(), style.bg(Color::DarkGray)));
                }
                if !after_sel.is_empty() {
                    spans.push(Span::styled(after_sel.to_string(), style));
                }

                lines.push(Line::from(spans));
            } else if include_line {
                lines.push(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(text_line.to_string(), style),
                ]));
            }

            if include_line && render_base_visual_row.is_none() {
                render_base_visual_row = Some(visual_row);
            }

            visual_row = visual_line_end;
            row = row.saturating_add(1);
        }
        let include_blank = visual_row.saturating_add(1) > visual_window_start;
        if include_blank {
            if render_base_visual_row.is_none() {
                render_base_visual_row = Some(visual_row);
            }
            lines.push(Line::raw(""));
        }
        visual_row = visual_row.saturating_add(1);
        row = row.saturating_add(1);
    }

    // Pending indicator
    if !app.pending.is_empty() {
        let pending_text = format!(
            "{} Working... ({} pending, {}s)",
            app.spinner(),
            app.pending.len(),
            app.oldest_secs()
        );
        let include_pending = visual_row
            .saturating_add(wrapped_visual_line_count("", &pending_text, content_width))
            > visual_window_start;
        if include_pending {
            if render_base_visual_row.is_none() {
                render_base_visual_row = Some(visual_row);
            }
            lines.push(Line::from(vec![Span::styled(
                pending_text,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::ITALIC),
            )]));
        }
    }

    let render_base = render_base_visual_row.unwrap_or(0);
    let relative_scroll = effective_scroll.saturating_sub(render_base).min(u16::MAX as usize);

    let p = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((relative_scroll as u16, 0))
        .block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
    f.render_widget(p, area);
}

fn wrapped_visual_line_count(prefix: &str, text: &str, content_width: usize) -> usize {
    if content_width == 0 {
        return 0;
    }

    let prefix_width = UnicodeWidthStr::width(prefix).min(content_width.saturating_sub(1));
    let available_width = content_width.saturating_sub(prefix_width).max(1);
    let text_width = UnicodeWidthStr::width(text);

    if text_width == 0 {
        1
    } else {
        text_width.div_ceil(available_width)
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let workflow = app.session_overview.status_line();
    let approve_hint = if app.inline_approvals_enabled && !app.pending_approval_ids.is_empty() {
        " | Approve: Ctrl+A"
    } else {
        ""
    };
    let pause_hint = if app.session_paused {
        "Paused: ON"
    } else {
        "Paused: OFF"
    };
    let esc_hint = if app.cancel_armed() {
        "Esc: armed (press again to cancel)"
    } else {
        "Esc: pause"
    };
    let follow_hint = if app.follow_output {
        "Follow: ON"
    } else {
        "Follow: OFF (Ctrl+F to jump live)"
    };
    let text = if !app.pending.is_empty() {
        format!(
            "{} {} pending | {} | {} | {} | {} | Enter: send | Scroll: Shift+↑↓ | Quit: Ctrl+C{}",
            app.spinner(),
            app.pending.len(),
            workflow,
            pause_hint,
            esc_hint,
            follow_hint,
            approve_hint,
        )
    } else {
        format!(
            "Session: {} | Target: {} | {} | {} | {} | {} | Enter: send | Scroll: Shift+↑↓ | Quit: Ctrl+C{}",
            &app.session_id[..20.min(app.session_id.len())],
            app.target_hint,
            workflow,
            pause_hint,
            esc_hint,
            follow_hint,
            approve_hint,
        )
    };

    let p = Paragraph::new(Span::styled(text, Style::default().fg(Color::White)));
    f.render_widget(p, area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled("> ", Style::default().fg(Color::Green))];

    if app.input.is_empty() {
        spans.push(Span::styled(" ", Style::default().bg(Color::White)));
    } else {
        let before = &app.input[..app.cursor_pos];
        let after = &app.input[app.cursor_pos..];

        if !before.is_empty() {
            spans.push(Span::raw(before.to_string()));
        }
        spans.push(Span::styled(" ", Style::default().bg(Color::White)));
        if !after.is_empty() {
            spans.push(Span::raw(after.to_string()));
        }
    }

    let p = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(p, area);
}

/// Wait until `deadline` while checking for Ctrl+C via both the shutdown
/// signal and crossterm key events. In raw mode, Ctrl+C does not generate
/// SIGINT, so `tokio::signal::ctrl_c()` never fires — we must poll the
/// terminal for the key event instead.
///
/// Returns `true` if the user requested quit (Ctrl+C), `false` if the
/// deadline elapsed normally.
async fn wait_with_cancel(
    deadline: tokio::time::Instant,
    shutdown: &Arc<tokio::sync::Notify>,
) -> bool {
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                // Drain crossterm events to catch Ctrl+C in raw mode
                while event::poll(Duration::ZERO).unwrap_or(false) {
                    if let Ok(Event::Key(key)) = event::read() {
                        if key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            return true;
                        }
                    }
                }
            }
            _ = shutdown.notified() => {
                return true;
            }
        }
    }
    false
}

// ============================================================================
// Main Entry Point
// ============================================================================

pub async fn handle_chat(config_path: &Path, args: &super::common::ChatArgs) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let target_hint = args.agent_id.as_deref().unwrap_or("planner.default");
    let session_id = args
        .session_id
        .clone()
        .unwrap_or_else(|| format!("session-{}", &uuid::Uuid::new_v4().to_string()[..8]));
    let sender_id = args
        .sender_id
        .clone()
        .unwrap_or_else(default_terminal_sender_id);
    let channel_id = args
        .channel_id
        .clone()
        .unwrap_or_else(|| default_terminal_channel_id(&sender_id, target_hint));
    let gateway_addr = format!("127.0.0.1:{}", config.port);

    // Connect handling is mostly inside the loop.
    let envelope = terminal_channel_envelope(&channel_id, &sender_id, &session_id);
    let config = Arc::new(config);

    let jsonrpc_auth_token = std::env::var("AUTONOETIC_SHARED_SECRET").map_err(|_| {
        anyhow::anyhow!(
            "Missing required environment variable AUTONOETIC_SHARED_SECRET for chat JSON-RPC ingress authentication"
        )
    })?;

    // Setup terminal (only after prerequisites—early `?` must not leave raw mode / alt screen on)
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let _terminal_restore = ChatTerminalRestore;

    let mut app = App::new(session_id.clone(), target_hint.to_string());
    app.inline_approvals_enabled = config.chat.inline_approvals;
    if let Ok(restored) = hydrate_session_history(&mut app, config.as_ref(), &session_id) {
        if restored > 0 {
            app.add_message(
                MessageRole::System,
                format!(
                    "Restored {} message(s) from previous session history",
                    restored
                ),
            );
        }
    }

    // Show compact session info
    let root_session = autonoetic_gateway::runtime::content_store::root_session_id(&session_id);
    let wf_hint =
        autonoetic_gateway::scheduler::resolve_workflow_id_for_root_session(&config, root_session)
            .ok()
            .flatten()
            .map(|wf_id| format!(" · wf:{}", &wf_id[..8.min(wf_id.len())]))
            .unwrap_or_default();
    app.add_message(
        MessageRole::System,
        format!("{}{}", &session_id[..20.min(session_id.len())], wf_hint),
    );

    // Channel for sending messages from TUI to gateway
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, String)>();

    // Shared shutdown flag — set by Ctrl+C handler, checked by all loops
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());

    // Map gateway request IDs to internal IDs
    let mut pending_map: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    // Signal check interval
    let mut signal_interval = tokio::time::interval(Duration::from_secs(1));
    signal_interval.tick().await;

    // Open gateway store for approvals and signals (same path as gateway daemon)
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(config.as_ref());
    let gateway_store: Option<std::sync::Arc<autonoetic_gateway::scheduler::gateway_store::GatewayStore>> =
        match autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir) {
            Ok(store) => Some(std::sync::Arc::new(store)),
            Err(e) => {
                tracing::debug!(target: "chat", error = %e, "Gateway store unavailable, continuing without workflow events");
                None
            }
        };

    let execution_for_interactions = gateway_store.as_ref().map(|store| {
        std::sync::Arc::new(autonoetic_gateway::execution::GatewayExecutionService::new(
            config.as_ref().clone(),
            Some(store.clone()),
        ))
    });

    let gateway_log_dir = gateway_dir.join("logs");
    let mut reconnect_attempts: u32 = 0;

    if let Some(ref store) = gateway_store {
        if let Ok(snapshot) = poll_session_snapshot(
            config.as_ref(),
            Some(store),
            &session_id,
            app.session_overview.latest_signal.clone(),
        ) {
            app.session_overview = snapshot.overview.clone();
            let _ = append_new_pending_user_interaction_prompts(
                &mut app,
                &snapshot.pending_interactions,
            );
        }
        let _ = merge_gateway_store_pending_approvals(
            &mut app,
            config.as_ref(),
            store.as_ref(),
            &session_id,
        );
    }

    // Main loop
    // Spawn a single Ctrl+C listener that sets the shutdown flag.
    // This ensures Ctrl+C is handled exactly once, regardless of how many
    // tokio::select! branches are waiting on it.
    let shutdown_listener = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_listener.notify_one();
    });

    loop {
        // Connect
        let stream = match TcpStream::connect(&gateway_addr).await {
            Ok(s) => {
                reconnect_attempts = 0;
                s
            }
            Err(e) => {
                reconnect_attempts = reconnect_attempts.saturating_add(1);
                tracing::debug!(target: "chat", error = %e, "Gateway connection failed, reconnecting");
                if !app.pending.is_empty()
                    && (reconnect_attempts == RECONNECT_NOTICE_BASE_ATTEMPTS
                        || reconnect_attempts % 10 == 0)
                {
                    app.add_message(
                        MessageRole::System,
                        format!(
                            "Gateway is unreachable (attempt {}). {} pending request(s) may be stalled if the gateway crashed. Check logs under {} and restart with: autonoetic gateway start",
                            reconnect_attempts,
                            app.pending.len(),
                            gateway_log_dir.display(),
                        ),
                    );
                }
                terminal.draw(|f| draw(f, &app))?;

                let reconnect_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                if wait_with_cancel(reconnect_deadline, &shutdown).await {
                    break;
                }
                continue;
            }
        };
        // Set TCP keepalive to detect dead gateway quickly
        let _ = stream.set_nodelay(true);
        let (read_half, write_half) = stream.into_split();
        let mut gateway_lines = BufReader::new(read_half).lines();

        let disconnected = run_loop(
            &mut terminal,
            &mut app,
            write_half,
            &mut gateway_lines,
            &config,
            gateway_store
                .as_ref()
                .map(|s| s.as_ref()),
            execution_for_interactions.as_ref(),
            &session_id,
            &envelope,
            &tx,
            &mut rx,
            &mut pending_map,
            &mut signal_interval,
            &shutdown,
            &jsonrpc_auth_token,
        )
        .await?;

        if !disconnected {
            break; // User quit explicitly
        }

        reconnect_attempts = reconnect_attempts.saturating_add(1);

        app.add_message(
            MessageRole::System,
            "Gateway disconnected, reconnecting in 3s...".to_string(),
        );
        if !app.pending.is_empty()
            && (reconnect_attempts == RECONNECT_NOTICE_BASE_ATTEMPTS
                || reconnect_attempts % 10 == 0)
        {
            app.add_message(
                MessageRole::System,
                format!(
                    "Connection dropped with {} pending request(s). If this repeats, inspect gateway logs under {} for panic traces.",
                    app.pending.len(),
                    gateway_log_dir.display(),
                ),
            );
        }
        terminal.draw(|f| draw(f, &app))?;

        // Poll with short intervals so Ctrl+C is responsive during reconnect wait
        let reconnect_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        if wait_with_cancel(reconnect_deadline, &shutdown).await {
            break;
        }
    }

    // `_terminal_restore` Drop: raw mode off, leave alt screen, mouse capture off, show cursor
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    mut write_half: tokio::net::tcp::OwnedWriteHalf,
    gateway_lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>>,
    config: &Arc<autonoetic_types::config::GatewayConfig>,
    gateway_store: Option<&autonoetic_gateway::scheduler::gateway_store::GatewayStore>,
    execution_for_interactions: Option<&std::sync::Arc<autonoetic_gateway::execution::GatewayExecutionService>>,
    session_id: &str,
    envelope: &serde_json::Value,
    tx: &tokio::sync::mpsc::UnboundedSender<(u64, String)>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, String)>,
    pending_map: &mut std::collections::HashMap<String, u64>,
    signal_interval: &mut tokio::time::Interval,
    shutdown: &std::sync::Arc<tokio::sync::Notify>,
    jsonrpc_auth_token: &str,
) -> anyhow::Result<bool> {
    let mut needs_redraw = true;
    let mut last_spinner_tick = Instant::now();

    loop {
        // Tick spinner every 100ms (only when needed for redraw)
        if last_spinner_tick.elapsed() > Duration::from_millis(100) {
            app.tick_spinner();
            last_spinner_tick = Instant::now();
            needs_redraw = true;
        }

        // Only draw when something changed
        if needs_redraw {
            let size = terminal.size()?;
            let area = Rect::new(0, 0, size.width, size.height);
            let layout = compute_chat_layout(area);
            let messages_height = layout.messages.height as usize;
            let messages_content_width = layout.messages.width.saturating_sub(1);
            app.last_max_scroll_offset = app
                .content_line_count(messages_content_width)
                .saturating_sub(messages_height);
            if app.follow_output {
                app.scroll_offset = app.last_max_scroll_offset;
            } else {
                app.scroll_offset = app.scroll_offset.min(app.last_max_scroll_offset);
            }
            terminal.draw(|f| draw(f, app))?;
            needs_redraw = false;
        }

        // Use tokio::select to handle async events
        tokio::select! {
            biased;

            // Shutdown notification from Ctrl+C handler
            _ = shutdown.notified() => {
                return Ok(false); // Clean quit
            }

            // Signal check always gets priority to avoid starvation
            _ = signal_interval.tick() => {
                if check_signals(app, config, gateway_store, session_id, tx).await {
                    needs_redraw = true;
                }
            }

            // Gateway response
            result = gateway_lines.next_line() => {
                match result {
                    Ok(Some(line)) => {
                        if let Ok(resp) = serde_json::from_str::<GatewayJsonRpcResponse>(&line) {
                            if let Some(internal_id) = pending_map.remove(&resp.id) {
                                app.remove_pending(internal_id);
                                let signal_resume_ref =
                                    app.signal_resume_by_internal_id.remove(&internal_id);
                                if let Some(resume_ref) = &signal_resume_ref {
                                    app.signal_resume_inflight.remove(&signal_resume_key(
                                        &resume_ref.signal_session_id,
                                        &resume_ref.request_id,
                                    ));
                                }

                                if let Some(error) = resp.error {
                                    app.add_message(MessageRole::System, format!("Error: {}", error.message));
                                } else {
                                    let result_json = resp.result.as_ref();
                                    let reply = result_json
                                        .and_then(|v| v.get("assistant_reply").and_then(|r| r.as_str().map(ToOwned::to_owned)))
                                        .unwrap_or_else(|| "[No response]".to_string());

                                    // Try to extract user interactions directly from response (zero latency),
                                    // then fall back to store polling.
                                    let new_user_prompts_from_response = result_json
                                        .and_then(|v| v.get("pending_user_interactions"))
                                        .and_then(|v| serde_json::from_value::<Vec<UserInteraction>>(v.clone()).ok())
                                        .map(|interactions| append_new_pending_user_interaction_prompts(app, &interactions));

                                    let new_user_prompts = if new_user_prompts_from_response.is_some() {
                                        new_user_prompts_from_response.unwrap()
                                    } else if let Some(store) = gateway_store {
                                        match poll_session_snapshot(config, Some(store), session_id, app.session_overview.latest_signal.clone()) {
                                            Ok(snapshot) => {
                                                app.session_overview.root_session_id = snapshot.overview.root_session_id.clone();
                                                app.session_overview.workflow = snapshot.overview.workflow.clone();
                                                app.session_overview.pending_user_interactions = snapshot.overview.pending_user_interactions;
                                                append_new_pending_user_interaction_prompts(
                                                    app,
                                                    &snapshot.pending_interactions,
                                                )
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    target: "chat",
                                                    error = %e,
                                                    "pending user interaction poll failed"
                                                );
                                                0
                                            }
                                        }
                                    } else {
                                        0
                                    };
                                    let reply_is_placeholder =
                                        reply.trim().is_empty() || reply == "[No response]";

                                    if let Some(structured) = extract_structured_approval(&reply) {
                                        app.session_overview.latest_signal = Some(
                                            structured
                                                .request_id
                                                .as_deref()
                                                .map(|id| format!("approval {}", id))
                                                .unwrap_or_else(|| {
                                                    structured
                                                        .card
                                                        .lines()
                                                        .next()
                                                        .unwrap_or("approval required")
                                                        .to_string()
                                                }),
                                        );
                                        app.add_message(MessageRole::Signal, structured.card);
                                    } else if let Some(req_id) = extract_approval_request_id(&reply) {
                                        app.session_overview.latest_signal =
                                            Some(format!("approval {}", req_id));
                                        app.add_message(
                                            MessageRole::Signal,
                                            format!("Approval required: {}", req_id),
                                        );
                                    }

                                    if !(new_user_prompts > 0 && reply_is_placeholder) {
                                        app.add_message(MessageRole::Assistant, reply);
                                    }

                                }
                                needs_redraw = true;
                            }
                        }
                    }
                    Ok(None) => {
                        return Ok(true); // Disconnected
                    }
                    Err(_e) => {
                        return Ok(true); // Disconnected
                    }
                }
            }

            // User message to send
            msg = rx.recv() => {
                if let Some((id, message)) = msg {
                    // Pending user.ask: gateway-owned answer + resume (workflow Runnable or session checkpoint).
                    let mut skip_chat_ingest = false;
                    if let (Some(store), Some(exec)) = (gateway_store, execution_for_interactions) {
                        if let Ok(pending) =
                            list_pending_user_interactions_for_terminal_session(store, session_id)
                        {
                            if let Some(interaction) = pending.into_iter().next() {
                                use autonoetic_gateway::interaction_answer::{
                                    answer_and_orchestrate_resume, InteractionAnswerParams,
                                };
                                match answer_and_orchestrate_resume(
                                    exec,
                                    InteractionAnswerParams {
                                        interaction_id: interaction.interaction_id.clone(),
                                        answer_text: Some(message.clone()),
                                        answer_option_id: None,
                                        answered_by: Some("chat-tui".to_string()),
                                        follow_up_message: Some(message.clone()),
                                    },
                                )
                                .await
                                {
                                    Ok(out) => {
                                        app.add_message(
                                            MessageRole::System,
                                            format!(
                                                "Answered interaction {} (gateway resume: resumed={}, wf_unblocked={})",
                                                interaction.interaction_id, out.resumed, out.workflow_task_unblocked
                                            ),
                                        );
                                        skip_chat_ingest = true;
                                    }
                                    Err(e) => {
                                        app.add_message(
                                            MessageRole::System,
                                            format!("interaction answer orchestration failed: {}", e),
                                        );
                                        skip_chat_ingest = true;
                                    }
                                }
                            }
                        }
                    }

                    if skip_chat_ingest {
                        // We answered the interaction in-process and intentionally skipped
                        // sending `event.ingest`, so clear the in-flight request marker
                        // created when the user pressed Enter.
                        app.remove_pending(id);
                        needs_redraw = true;
                        continue;
                    }

                    let req_id = format!("tui-{}", id);
                    pending_map.insert(req_id.clone(), id);

                    let params = serde_json::json!({
                        "event_type": "chat",
                        "message": message,
                        "session_id": session_id,
                        "target_agent_id": app.target_hint.clone(),
                        "metadata": envelope,
                    });

                    let request = GatewayJsonRpcRequest {
                        jsonrpc: "2.0".to_string(),
                        id: req_id,
                        method: "event.ingest".to_string(),
                        params,
                        auth_token: Some(jsonrpc_auth_token.to_string()),
                    };

                    let encoded = serde_json::to_string(&request)?;
                    write_half.write_all(encoded.as_bytes()).await?;
                    write_half.write_all(b"\n").await?;
                    write_half.flush().await?;
                    needs_redraw = true;
                }
            }

            // TUI input - poll with short timeout for responsive UI
            _ = tokio::time::sleep(Duration::from_millis(16)) => {  // ~60fps
                // Drain all pending crossterm events
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) => {
                            match handle_key(key, app, tx)? {
                                HandleKeyAction::Quit => return Ok(false),
                                HandleKeyAction::PauseSession => {
                                    app.session_paused = true;
                                    let root_session_id = if app.session_overview.root_session_id.is_empty() {
                                        autonoetic_gateway::runtime::content_store::root_session_id(session_id)
                                            .to_string()
                                    } else {
                                        app.session_overview.root_session_id.clone()
                                    };

                                    let mut paused_tasks = 0usize;
                                    if let Some(store) = gateway_store {
                                        if let Ok(Some(workflow_id)) = autonoetic_gateway::scheduler::resolve_workflow_id_for_root_session(
                                            config,
                                            &root_session_id,
                                        ) {
                                            if let Ok(tasks) = autonoetic_gateway::scheduler::list_task_runs_for_workflow(
                                                config,
                                                Some(store),
                                                &workflow_id,
                                            ) {
                                                for task in tasks {
                                                    if matches!(
                                                        task.status,
                                                        autonoetic_types::workflow::TaskRunStatus::Pending
                                                            | autonoetic_types::workflow::TaskRunStatus::Runnable
                                                            | autonoetic_types::workflow::TaskRunStatus::Running
                                                            | autonoetic_types::workflow::TaskRunStatus::AwaitingApproval
                                                    ) {
                                                        if autonoetic_gateway::scheduler::workflow_store::update_task_run_status(
                                                            config,
                                                            Some(store),
                                                            &workflow_id,
                                                            &task.task_id,
                                                            autonoetic_types::workflow::TaskRunStatus::Paused,
                                                            Some("paused by operator via chat TUI (Esc)".to_string()),
                                                            None,
                                                            None,
                                                        )
                                                        .is_ok()
                                                        {
                                                            paused_tasks = paused_tasks.saturating_add(1);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    app.add_message(
                                        MessageRole::System,
                                        format!(
                                            "Pause requested via Esc. Paused {} workflow task(s). Press Esc again within 2s to cancel the root session.",
                                            paused_tasks
                                        ),
                                    );
                                }
                                HandleKeyAction::CancelSession => {
                                    let root_session_id = if app.session_overview.root_session_id.is_empty() {
                                        autonoetic_gateway::runtime::content_store::root_session_id(session_id)
                                            .to_string()
                                    } else {
                                        app.session_overview.root_session_id.clone()
                                    };

                                    if let Some(exec) = execution_for_interactions {
                                        match exec
                                            .emergency_stop_root_session(
                                                &root_session_id,
                                                "Cancelled from chat TUI (double Esc)",
                                                "operator",
                                                "chat-tui",
                                                "chat_escape_double",
                                                None,
                                            )
                                            .await
                                        {
                                            Ok(_) => {
                                                app.session_paused = true;
                                                app.add_message(
                                                    MessageRole::System,
                                                    format!(
                                                        "Cancelled root session via emergency stop: {}",
                                                        root_session_id
                                                    ),
                                                );
                                            }
                                            Err(e) => {
                                                app.add_message(
                                                    MessageRole::System,
                                                    format!(
                                                        "Failed to cancel root session {}: {}",
                                                        root_session_id, e
                                                    ),
                                                );
                                            }
                                        }
                                    } else {
                                        app.add_message(
                                            MessageRole::System,
                                            "Execution service unavailable: cannot cancel root session from chat TUI.".to_string(),
                                        );
                                    }
                                }
                                HandleKeyAction::ApproveInline(apr_id) => {
                                    // Handle inline approval
                                    if let Some(store) = gateway_store {
                                        let approver_level =
                                            autonoetic_types::background::ApprovalLevel::Operator;
                                        match autonoetic_gateway::scheduler::approve_request(
                                            config,
                                            Some(store),
                                            &apr_id,
                                            "chat-tui",
                                            None,
                                            None,
                                            Some(&approver_level),
                                            None,
                                        ) {
                                            Ok(_decision) => {
                                                app.pending_approval_ids
                                                    .retain(|id| id != &apr_id);
                                                app.announced_store_approval_ids
                                                    .remove(&apr_id);
                                                app.add_message(
                                                    MessageRole::System,
                                                    format!("Approved: {}", apr_id),
                                                );
                                            }
                                            Err(e) => {
                                                app.add_message(
                                                    MessageRole::System,
                                                    format!("Failed to approve: {}", e),
                                                );
                                            }
                                        }
                                    } else {
                                        app.add_message(
                                            MessageRole::System,
                                            "Gateway store not available for inline approval.".to_string(),
                                        );
                                    }
                                }
                                HandleKeyAction::Continue => {}
                            }
                            needs_redraw = true;
                        }
                        Event::Mouse(mouse) => {
                            let redraw = handle_mouse(mouse, app);
                            needs_redraw = needs_redraw || redraw;
                        }
                        Event::Resize(_, _) => {
                            needs_redraw = true;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    // Loop only exits via returns
}

fn handle_mouse(mouse: crossterm::event::MouseEvent, app: &mut App) -> bool {
    // Shift+mouse events pass through to the terminal for native selection/copy.
    // This lets users hold Shift to use the terminal emulator's built-in
    // click-drag-to-select and middle-click-to-paste without interference
    // from the application's mouse capture.
    if mouse.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) {
        return false;
    }
    match mouse.kind {
        crossterm::event::MouseEventKind::ScrollUp => {
            app.scroll_messages_up(3);
            true
        }
        crossterm::event::MouseEventKind::ScrollDown => {
            app.scroll_messages_down(3);
            true
        }
        crossterm::event::MouseEventKind::Down(btn) => {
            if btn == crossterm::event::MouseButton::Left {
                // Only start selection if clicking in messages area (row >= 2)
                if mouse.row >= 2 {
                    // Convert screen coordinates to content coordinates
                    // Layout: status (1 row) + separator (1 row) = messages start at row 2
                    // Messages widget has left border (1 col) + prefix (2 cols) = text at col 3
                    let content_row = (mouse.row as usize - 2) + app.effective_scroll_offset();
                    let content_col = (mouse.column as usize).saturating_sub(3);
                    app.selecting = true;
                    app.sel_start = Some((content_row, content_col));
                    app.sel_end = Some((content_row, content_col));
                    true
                } else {
                    // Clicked on status or separator - clear any existing selection
                    if app.sel_start.is_some() || app.sel_end.is_some() {
                        app.sel_start = None;
                        app.sel_end = None;
                        true
                    } else {
                        false
                    }
                }
            } else {
                false
            }
        }
        crossterm::event::MouseEventKind::Up(btn) => {
            if btn == crossterm::event::MouseButton::Left && app.selecting {
                // Only complete selection if mouse is in messages area
                if mouse.row >= 2 {
                    let content_row = (mouse.row as usize - 2) + app.effective_scroll_offset();
                    let content_col = (mouse.column as usize).saturating_sub(3);
                    app.sel_end = Some((content_row, content_col));
                    app.selecting = false;
                    copy_selection_to_clipboard(app);
                } else {
                    // Mouse released outside messages area - cancel selection
                    app.selecting = false;
                    app.sel_start = None;
                    app.sel_end = None;
                }
                true
            } else {
                false
            }
        }
        crossterm::event::MouseEventKind::Drag(btn) => {
            if btn == crossterm::event::MouseButton::Left && app.selecting {
                // Only update if in messages area
                if mouse.row >= 2 {
                    let content_row = (mouse.row as usize - 2) + app.effective_scroll_offset();
                    let content_col = (mouse.column as usize).saturating_sub(3);
                    app.sel_end = Some((content_row, content_col));
                }
                true // Need redraw to show selection highlight
            } else {
                false
            }
        }
        _ => false,
    }
}

enum HandleKeyAction {
    Continue,
    Quit,
    ApproveInline(String),
    PauseSession,
    CancelSession,
}

fn handle_key(
    key: crossterm::event::KeyEvent,
    app: &mut App,
    tx: &tokio::sync::mpsc::UnboundedSender<(u64, String)>,
) -> anyhow::Result<HandleKeyAction> {
    match key.code {
        // Quit
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Ok(HandleKeyAction::Quit)
        }

        // Send
        KeyCode::Enter => {
            if app.session_paused {
                app.add_message(
                    MessageRole::System,
                    "Session is paused. Press Esc again within 2s to cancel, or Ctrl+R to resume input.".to_string(),
                );
            } else if !app.input.is_empty() {
                let msg = std::mem::take(&mut app.input);
                app.cursor_pos = 0;
                let id = app.next_id();
                app.add_pending(id);
                app.add_message(MessageRole::User, msg.clone());
                let _ = tx.send((id, msg));
            }
        }

        // Escape safety: first hit pauses, second hit within 2s cancels root session.
        KeyCode::Esc => {
            if app.cancel_armed() {
                app.disarm_cancel_window();
                return Ok(HandleKeyAction::CancelSession);
            }
            app.arm_cancel_window();
            return Ok(HandleKeyAction::PauseSession);
        }

        // Cursor
        KeyCode::Left => app.cursor_left(),
        KeyCode::Right => app.cursor_right(),
        KeyCode::Home => app.cursor_pos = 0,
        KeyCode::End => app.cursor_pos = app.input.len(),

        // Delete
        KeyCode::Backspace => app.delete_char(),
        KeyCode::Delete => {
            if app.cursor_pos < app.input.len() {
                app.input.remove(app.cursor_pos);
            }
        }

        // Type
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.insert_char(c);
        }

        // Scroll (Shift or Ctrl)
        KeyCode::Up
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.scroll_messages_up(3);
        }
        KeyCode::Down
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.scroll_messages_down(3);
        }

        // Inline approval: Ctrl+A approves the latest pending approval
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !app.inline_approvals_enabled {
                app.add_message(
                    MessageRole::System,
                    "Inline approvals not enabled. Set chat.inline_approvals: true in gateway config.".to_string(),
                );
            } else if app.pending_approval_ids.is_empty() {
                app.add_message(
                    MessageRole::System,
                    "No pending approvals to approve.".to_string(),
                );
            } else {
                // Pop the latest pending approval ID
                let apr_id = app.pending_approval_ids.pop().unwrap();
                return Ok(HandleKeyAction::ApproveInline(apr_id));
            }
        }

        // Resume local input after an Esc pause.
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.session_paused = false;
            app.disarm_cancel_window();
            app.add_message(
                MessageRole::System,
                "Session input resumed (Ctrl+R).".to_string(),
            );
        }

        // Jump to bottom and re-enable live following.
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.follow_output = true;
            app.scroll_offset = app.last_max_scroll_offset;
        }

        _ => {}
    }

    Ok(HandleKeyAction::Continue)
}

fn clamp_chat_field(s: &str) -> String {
    let count = s.chars().count();
    if count <= CHAT_APPROVAL_FIELD_MAX_CHARS {
        s.to_string()
    } else {
        let take = CHAT_APPROVAL_FIELD_MAX_CHARS.saturating_sub(1);
        format!(
            "{}…",
            s.chars().take(take).collect::<String>()
        )
    }
}

fn approval_level_as_str(level: &ApprovalLevel) -> String {
    match level {
        ApprovalLevel::Operator => "operator".to_string(),
        ApprovalLevel::Admin => "admin".to_string(),
        ApprovalLevel::Agent(a) => format!("agent:{a}"),
    }
}

/// Full action detail lines for store-backed approvals (scrollable in the transcript).
fn format_scheduled_action_detail_lines(action: &ScheduledAction) -> Vec<String> {
    match action {
        ScheduledAction::SessionContinue {
            session_id,
            root_session_id,
            requested_by_agent_id,
            turn_counter,
            max_turns,
            payload,
        } => {
            let mut v = vec![
                "type: session_continue".to_string(),
                format!("  session: {}", clamp_chat_field(session_id)),
                format!("  root: {}", clamp_chat_field(root_session_id)),
                format!("  requested_by: {}", clamp_chat_field(requested_by_agent_id)),
                format!("  turns: {turn_counter} (max {max_turns})"),
            ];
            if let Some(p) = payload {
                if let Ok(s) = serde_json::to_string_pretty(p) {
                    v.push("  payload:".to_string());
                    for ln in s.lines() {
                        v.push(format!("    {}", clamp_chat_field(ln)));
                    }
                }
            }
            v
        }
        ScheduledAction::CredentialRequest {
            credential_id,
            url,
            method,
            headers,
            body,
            inject_secret_as,
            ..
        } => {
            let mut v = vec![
                "type: credential_request".to_string(),
                format!("  credential_id: {}", clamp_chat_field(credential_id)),
            ];
            if let Some(m) = method {
                v.push(format!("  method: {m}"));
            }
            v.push(format!("  url: {}", clamp_chat_field(url)));
            if let Some(h) = headers {
                if let Ok(s) = serde_json::to_string_pretty(h) {
                    v.push("  headers:".to_string());
                    for ln in s.lines() {
                        v.push(format!("    {}", clamp_chat_field(ln)));
                    }
                }
            }
            if let Some(b) = body {
                if let Ok(s) = serde_json::to_string_pretty(b) {
                    v.push("  body:".to_string());
                    for ln in s.lines() {
                        v.push(format!("    {}", clamp_chat_field(ln)));
                    }
                }
            }
            if let Some(i) = inject_secret_as {
                v.push(format!("  inject_secret_as: {i}"));
            }
            v
        }
        ScheduledAction::CredentialPrompt {
            service,
            credential_id,
            message,
            secret_fields,
            ..
        } => {
            let mut v = vec![
                "type: credential_prompt".to_string(),
                format!("  service: {}", clamp_chat_field(service)),
                format!("  credential_id: {}", clamp_chat_field(credential_id)),
                format!("  message: {}", clamp_chat_field(message)),
            ];
            if !secret_fields.is_empty() {
                v.push(format!(
                    "  secret_fields: {}",
                    secret_fields
                        .iter()
                        .map(|f| f.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            v
        }
        ScheduledAction::SandboxExec {
            command,
            dependencies,
            detected_hosts,
            ..
        } => {
            let mut v = vec![
                "type: sandbox_exec".to_string(),
                format!("  command: {}", clamp_chat_field(command)),
            ];
            if let Some(deps) = dependencies {
                if let Ok(s) = serde_json::to_string(deps) {
                    v.push(format!("  dependencies: {}", clamp_chat_field(&s)));
                }
            }
            if let Some(hosts) = detected_hosts {
                v.push(format!(
                    "  detected_hosts: {}",
                    hosts
                        .iter()
                        .map(|h| clamp_chat_field(h.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            v
        }
        ScheduledAction::WriteFile {
            path,
            content,
            ..
        } => {
            let preview = clamp_chat_field(content);
            vec![
                "type: write_file".to_string(),
                format!("  path: {}", clamp_chat_field(path)),
                format!("  content (preview):\n{}", indent_block("    ", &preview)),
            ]
        }
        ScheduledAction::AgentInstall {
            agent_id,
            summary,
            requested_by_agent_id,
            install_fingerprint,
            ..
        } => {
            vec![
                "type: agent_install".to_string(),
                format!("  agent_id: {}", clamp_chat_field(agent_id)),
                format!("  summary: {}", clamp_chat_field(summary)),
                format!("  requested_by: {}", clamp_chat_field(requested_by_agent_id)),
                format!("  install_fingerprint: {}", clamp_chat_field(install_fingerprint)),
            ]
        }
        ScheduledAction::ProfileShare {
            user_id,
            agent_id,
            scope,
        } => {
            vec![
                "type: profile_share".to_string(),
                format!("  user_id: {}", clamp_chat_field(user_id)),
                format!("  agent_id: {}", clamp_chat_field(agent_id)),
                format!("  scope: {}", clamp_chat_field(scope)),
            ]
        }
        ScheduledAction::SessionEscalate {
            session_id,
            root_session_id,
            requested_by_agent_id,
            reason,
            context,
            urgency,
            suggested_actions,
            ..
        } => {
            let mut v = vec![
                "type: session_escalate".to_string(),
                format!("  session: {}", clamp_chat_field(session_id)),
                format!("  root: {}", clamp_chat_field(root_session_id)),
                format!("  requested_by: {}", clamp_chat_field(requested_by_agent_id)),
                format!("  urgency: {}", clamp_chat_field(urgency)),
                format!("  reason: {}", clamp_chat_field(reason)),
            ];
            if !context.is_empty() {
                v.push("  context:".to_string());
                for ln in context.lines() {
                    v.push(format!("    {}", clamp_chat_field(ln)));
                }
            }
            if !suggested_actions.is_empty() {
                v.push(format!(
                    "  suggested_actions: {}",
                    suggested_actions
                        .iter()
                        .map(|s| clamp_chat_field(s.as_str()))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            }
            v
        }
        ScheduledAction::LayerMount { layers, command } => {
            let mut v = vec![
                "type: layer_mount".to_string(),
                format!("  command: {}", clamp_chat_field(command)),
                format!("  layers requiring approval: {}", layers.len()),
            ];
            for l in layers {
                v.push(format!(
                    "  - {} (source: {}) unapproved hosts: {}",
                    clamp_chat_field(&l.name),
                    clamp_chat_field(&l.source),
                    l.unapproved_delta.join(", ")
                ));
            }
            v
        }
        ScheduledAction::RevisionPromote {
            agent_id,
            revision_id,
            outgoing_revision_id,
            added_capabilities,
            broadened_capabilities,
            ..
        } => {
            let mut v = vec![
                "type: revision_promote (R++2 capability accretion)".to_string(),
                format!("  agent: {}", clamp_chat_field(agent_id)),
                format!("  outgoing: {}", clamp_chat_field(outgoing_revision_id)),
                format!("  incoming: {}", clamp_chat_field(revision_id)),
            ];
            if !added_capabilities.is_empty() {
                v.push(format!("  added caps: {}", added_capabilities.join(", ")));
            }
            if !broadened_capabilities.is_empty() {
                v.push(format!(
                    "  broadened caps: {}",
                    broadened_capabilities.join(", ")
                ));
            }
            v
        }
    }
}

fn indent_block(prefix: &str, text: &str) -> String {
    text.lines()
        .map(|ln| format!("{prefix}{}", clamp_chat_field(ln)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Multi-line card for a persisted [`ApprovalRequest`] (gateway store sync).
fn format_store_approval_card(req: &ApprovalRequest, approval_instructions: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("⏸ Pending approval — {}", req.request_id));
    lines.push(format!(
        "required approval level: {}",
        approval_level_as_str(&req.approval_level)
    ));
    lines.push(String::new());
    lines.push("Action:".to_string());
    for ln in format_scheduled_action_detail_lines(&req.action) {
        lines.push(ln);
    }
    if let Some(r) = req.reason.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        lines.push(String::new());
        lines.push("Reason:".to_string());
        for ln in r.lines() {
            lines.push(format!("  {}", clamp_chat_field(ln)));
        }
    }
    if let Some(ev) = req.evidence_ref.as_deref().filter(|s| !s.is_empty()) {
        lines.push(String::new());
        lines.push(format!("Evidence ref: {}", clamp_chat_field(ev)));
    }
    lines.push(String::new());
    lines.push(approval_instructions.to_string());
    lines.join("\n")
}

/// Merge pending gateway approvals for this chat's root session into `pending_approval_ids` so
/// approvals that never appear as workflow cards (e.g. `SessionContinue`) still work with Ctrl+A.
/// Returns true if a new system message was added.
fn merge_gateway_store_pending_approvals(
    app: &mut App,
    config: &GatewayConfig,
    store: &GatewayStore,
    session_id: &str,
) -> bool {
    let root = autonoetic_gateway::runtime::content_store::root_session_id(session_id);
    let Ok(mut list) = autonoetic_gateway::scheduler::pending_approval_requests_for_root(
        config,
        Some(store),
        &root,
    ) else {
        return false;
    };
    list.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let store_ordered: Vec<String> = list.iter().map(|r| r.request_id.clone()).collect();
    let store_set: HashSet<String> = store_ordered.iter().cloned().collect();

    let mut merged: Vec<String> = store_ordered;
    for id in &app.pending_approval_ids {
        if !store_set.contains(id) && !merged.contains(id) {
            merged.push(id.clone());
        }
    }
    app.pending_approval_ids = merged;

    let mut announced = false;
    for req in list {
        if app.announced_store_approval_ids.insert(req.request_id.clone()) {
            let detail = if app.inline_approvals_enabled {
                format!(
                    "Approve: Ctrl+A, or `autonoetic gateway approvals approve {} …`.",
                    req.request_id
                )
            } else {
                format!(
                    "Resolve with `autonoetic gateway approvals approve {} …` (set `chat.inline_approvals: true` for Ctrl+A).",
                    req.request_id
                )
            };
            let card = format_store_approval_card(&req, &detail);
            app.add_message(MessageRole::Signal, card);
            announced = true;
        }
    }
    announced
}

/// Check for signals and inject into app. Returns true if signals were processed.
async fn check_signals(
    app: &mut App,
    config: &autonoetic_types::config::GatewayConfig,
    store: Option<&autonoetic_gateway::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
    _tx: &tokio::sync::mpsc::UnboundedSender<(u64, String)>,
) -> bool {
    let mut processed_any = false;

    let snapshot = poll_session_snapshot(config, store, session_id, app.session_overview.latest_signal.clone())
        .unwrap_or_else(|e| {
            tracing::warn!(target: "chat", error = %e, "Failed to poll session snapshot, continuing with empty snapshot");
            SessionPollSnapshot {
                overview: SessionOverview {
                    root_session_id: app.session_overview.root_session_id.clone(),
                    ..SessionOverview::default()
                },
                pending_interactions: Vec::new(),
            }
        });

    let root_session_id = snapshot.overview.root_session_id.clone();

    tracing::debug!(target: "chat", session_id = %session_id, root_session_id = %root_session_id, "check_signals: starting");

    let previous_overview = app.session_overview.clone();
    app.session_overview.root_session_id = snapshot.overview.root_session_id.clone();
    app.session_overview.workflow = snapshot.overview.workflow.clone();
    app.session_overview.pending_user_interactions = snapshot.overview.pending_user_interactions;
    if app.session_overview != previous_overview {
        processed_any = true;
        // Show notification when workflow becomes active or changes
        let prev_is_na = previous_overview.workflow.workflow_id.is_none();
        let curr_is_active = app.session_overview.workflow.workflow_id.is_some();
        if curr_is_active && (prev_is_na || app.current_workflow_id.is_none()) {
            app.add_message(
                MessageRole::System,
                format!(
                    "🔗 Workflow connected: {}",
                    app.session_overview.status_line()
                ),
            );
            processed_any = true;
        }
    }

    match snapshot.overview.workflow.workflow_id.clone() {
        Some(primary_workflow_id) => {
            tracing::debug!(target: "chat", workflow_id = %primary_workflow_id, "Resolved workflow ID");

            let workflow_changed = app.current_workflow_id.as_ref() != Some(&primary_workflow_id);
            if workflow_changed {
                tracing::debug!(
                    target: "chat",
                    old = ?app.current_workflow_id,
                    new = %primary_workflow_id,
                    "Workflow ID changed, resetting event tracking"
                );
                app.seen_workflow_event_ids.clear();
                app.bootstrapped_workflow_ids.clear();
                app.current_workflow_id = Some(primary_workflow_id.clone());
            }

            let mut monitored_workflow_ids = vec![primary_workflow_id.clone()];
            if let Some(store) = store {
                if let Ok(jobs) = store.list_scheduled_jobs_for_root(&root_session_id) {
                    for job in jobs {
                        if matches!(
                            job.status,
                            autonoetic_types::scheduled_job::ScheduledJobStatus::Active
                        ) {
                            monitored_workflow_ids.push(format!("sched-{}", job.job_id));
                        }
                    }
                }
            }

            monitored_workflow_ids.sort();
            monitored_workflow_ids.dedup();

            if let Ok(tasks) = autonoetic_gateway::scheduler::list_task_runs_for_workflow(
                config,
                store,
                &primary_workflow_id,
            ) {
                app.live_tasks = tasks
                    .into_iter()
                    .map(|t| LiveTaskSummary {
                        task_id: t.task_id,
                        agent_id: t.agent_id,
                        status: format!("{:?}", t.status).to_lowercase(),
                    })
                    .collect();
            }

            for workflow_id in monitored_workflow_ids {
                let is_primary = workflow_id == primary_workflow_id;
                match autonoetic_gateway::scheduler::load_workflow_events(config, store, &workflow_id) {
                    Ok(events) => {
                        let was_bootstrapped = app.bootstrapped_workflow_ids.contains(&workflow_id);
                        if !was_bootstrapped {
                            // Primary workflow keeps a short recap; scheduled workflows baseline silently.
                            if is_primary {
                                let recap_count = events.len().min(20);
                                if recap_count > 0 {
                                    let start_idx = events.len().saturating_sub(recap_count);
                                    for event in &events[start_idx..] {
                                        if let Some((card, role)) = format_workflow_event_card(event) {
                                            if matches!(role, MessageRole::SignalLow) {
                                                continue;
                                            }
                                            app.session_overview.latest_signal = Some(card.clone());
                                            push_workflow_event_message(app, role, card);
                                            processed_any = true;
                                        }
                                    }
                                }
                            }

                            for event in &events {
                                app.seen_workflow_event_ids.insert(event.event_id.clone());
                            }
                            app.bootstrapped_workflow_ids.insert(workflow_id);
                            continue;
                        }

                        let mut new_event_count = 0usize;
                        for event in events {
                            if app.seen_workflow_event_ids.insert(event.event_id.clone()) {
                                if let Some((card, role)) = format_workflow_event_card(&event) {
                                    push_workflow_event_message(app, role, card.clone());
                                    app.session_overview.latest_signal = Some(card.clone());

                                    // Track pending approval IDs for inline approval (Ctrl+A)
                                    if app.inline_approvals_enabled {
                                        if event.event_type == "task.awaiting_approval" {
                                            if let Some(apr_id) = event
                                                .payload
                                                .get("approval_request_id")
                                                .and_then(|v| v.as_str())
                                            {
                                                if !app
                                                    .pending_approval_ids
                                                    .contains(&apr_id.to_string())
                                                {
                                                    app.pending_approval_ids
                                                        .push(apr_id.to_string());
                                                }
                                            }
                                        } else if event.event_type == "task.approved"
                                            || event.event_type == "task.rejected"
                                            || event.event_type == "task.cancelled"
                                        {
                                            if let Some(apr_id) = event
                                                .payload
                                                .get("request_id")
                                                .and_then(|v| v.as_str())
                                            {
                                                app.pending_approval_ids.retain(|id| id != apr_id);
                                            }
                                        }
                                    }

                                    processed_any = true;
                                }
                                new_event_count += 1;
                            }
                        }
                        if new_event_count > 0 {
                            tracing::debug!(
                                target: "chat",
                                workflow_id = %workflow_id,
                                new_event_count,
                                total_seen = app.seen_workflow_event_ids.len(),
                                "check_signals: processed new workflow events"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "chat",
                            workflow_id = %workflow_id,
                            error = %e,
                            "Failed to load workflow events"
                        );
                    }
                }
            }
        }
        None => {
            // No workflow found - this is normal if session is not connected to a workflow
            app.live_tasks.clear();
        }
    }

    let new_prompts =
        append_new_pending_user_interaction_prompts(app, &snapshot.pending_interactions);
    if new_prompts > 0 {
        processed_any = true;
    }

    if let Some(store) = store {
        if merge_gateway_store_pending_approvals(app, config, store, session_id) {
            processed_any = true;
        }
    }

    tracing::debug!(target: "chat", processed_any = processed_any, total_messages = app.messages.len(), "check_signals: complete");
    processed_any
}

/// Copy the selected text region to clipboard.
///
/// Uses the persistent `App::clipboard` instance so arboard's background ownership
/// thread stays alive after the write — clipboard managers have time to see the
/// content before it is released.
fn copy_selection_to_clipboard(app: &mut App) {
    let (Some((start_row, start_col)), Some((end_row, end_col))) = (app.sel_start, app.sel_end)
    else {
        return;
    };

    // Normalize selection direction.
    let (top_row, top_col, bot_row, bot_col) = if start_row <= end_row {
        (start_row, start_col, end_row, end_col)
    } else {
        (end_row, end_col, start_row, start_col)
    };

    // Build a flat list of all content lines (without prefix for clipboard).
    let mut lines: Vec<String> = Vec::new();
    for msg in &app.messages {
        for line in msg.content.lines() {
            lines.push(line.to_string());
        }
        lines.push(String::new()); // blank separator between messages
    }
    if !app.pending.is_empty() {
        lines.push(format!("{} Working...", app.spinner()));
    }

    let mut selected: Vec<String> = Vec::new();

    for row in top_row..=bot_row {
        if row >= lines.len() {
            break;
        }
        let line = &lines[row];

        if row == top_row && row == bot_row {
            // Single line selection
            let col_s = top_col.min(line.len());
            let col_e = bot_col.min(line.len());
            if col_e > col_s {
                selected.push(line[col_s..col_e].to_string());
            }
        } else if row == top_row {
            // First line of multi-line selection
            let col_s = top_col.min(line.len());
            selected.push(line[col_s..].to_string());
        } else if row == bot_row {
            // Last line of multi-line selection
            let col_e = bot_col.min(line.len());
            selected.push(line[..col_e].to_string());
        } else {
            // Middle line
            selected.push(line.clone());
        }
    }

    let selected_text = selected.join("\n");
    if selected_text.is_empty() {
        return;
    }

    // Safe clipboard copy - catch panics from arboard
    // arboard can panic on systems without a clipboard manager (headless, SSH, etc.)
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Reuse the persistent clipboard object; fall back to a fresh one if it was
        // never initialised (e.g. running in a headless environment).
        if let Some(cb) = app.clipboard.as_mut() {
            if cb.set_text(&selected_text).is_ok() {
                return true;
            }
        }
        // Last-resort: try allocating a new clipboard
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if cb.set_text(&selected_text).is_ok() {
                app.clipboard = Some(cb);
                return true;
            }
        }
        false
    }));

    if result.is_err() {
        // Clipboard operation panicked - silently ignore to avoid terminal corruption
        tracing::warn!("Clipboard operation panicked, ignoring");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        extract_approval_request_id, extract_structured_approval, format_user_interaction_prompt,
        format_workflow_event_card,
    };
    use autonoetic_types::background::{
        UserInteraction, UserInteractionKind, UserInteractionOption, UserInteractionStatus,
    };
    use autonoetic_types::workflow::WorkflowEventRecord;

    fn workflow_event(
        event_type: &str,
        task_id: Option<&str>,
        payload: serde_json::Value,
    ) -> WorkflowEventRecord {
        WorkflowEventRecord {
            event_id: "wevt-test".to_string(),
            workflow_id: "wf-test".to_string(),
            task_id: task_id.map(str::to_string),
            event_type: event_type.to_string(),
            agent_id: Some("tester".to_string()),
            payload,
            occurred_at: "2026-03-24T12:34:56Z".to_string(),
        }
    }

    #[test]
    fn test_extract_approval_request_id_short_form() {
        let text = "Install requires approval. request_id: apr-1234abcd";
        assert_eq!(
            extract_approval_request_id(text).as_deref(),
            Some("apr-1234abcd")
        );
    }

    #[test]
    fn test_extract_structured_approval_sandbox_exec() {
        let payload = serde_json::json!({
            "ok": false,
            "approval_required": true,
            "request_id": "apr-1234abcd",
            "approval": {
                "kind": "sandbox_exec",
                "reason": "Remote access detected",
                "summary": "Sandbox exec: curl https://api.example.com",
                "retry_field": "approval_ref",
                "subject": {
                    "command": "curl https://api.example.com",
                    "hosts": ["api.example.com"]
                }
            }
        })
        .to_string();

        let parsed = extract_structured_approval(&payload).expect("structured approval expected");
        assert_eq!(parsed.request_id.as_deref(), Some("apr-1234abcd"));
        assert!(parsed.card.contains("kind: sandbox_exec"));
        assert!(parsed.card.contains("retry field: approval_ref"));
        assert!(parsed.card.contains("hosts: api.example.com"));
    }

    #[test]
    fn test_extract_structured_approval_agent_install() {
        let payload = serde_json::json!({
            "ok": false,
            "approval_required": true,
            "request_id": "apr-89abcdef",
            "approval": {
                "kind": "agent_install",
                "reason": "High-risk install requires approval",
                "summary": "weather.fetcher with NetworkAccess",
                "retry_field": "promotion_gate.install_approval_ref",
                "subject": {
                    "agent_id": "weather.fetcher",
                    "artifact_id": "art_123",
                    "risk_factors": ["network_access", "scheduled_action"],
                    "capabilities": ["NetworkAccess"]
                }
            }
        })
        .to_string();

        let parsed = extract_structured_approval(&payload).expect("structured approval expected");
        assert_eq!(parsed.request_id.as_deref(), Some("apr-89abcdef"));
        assert!(parsed.card.contains("kind: agent_install"));
        assert!(parsed.card.contains("agent: weather.fetcher"));
        assert!(parsed
            .card
            .contains("retry field: promotion_gate.install_approval_ref"));
    }

    #[test]
    fn test_format_workflow_event_card_awaiting_approval() {
        let event = workflow_event(
            "task.awaiting_approval",
            Some("task-42"),
            serde_json::json!({
                "status": "awaiting_approval",
                "approval": "sandbox_exec",
                "approval_request_id": "apr-test123"
            }),
        );
        let line = format_workflow_event_card(&event).map(|(s, _)| s).expect("event should render");
        assert!(line.contains("Suspended for approval: task-42"));
        assert!(line.contains("sandbox_exec"));
        assert!(line.contains("resumes automatically"));
    }

    #[test]
    fn test_format_workflow_event_card_task_approved() {
        let event = workflow_event(
            "task.approved",
            Some("task-42"),
            serde_json::json!({ "status": "runnable" }),
        );
        let line = format_workflow_event_card(&event).map(|(s, _)| s).expect("event should render");
        assert!(line.contains("Approval granted"));
        assert!(line.contains("resuming: task-42"));
    }

    #[test]
    fn test_format_workflow_event_card_task_rejected() {
        let event = workflow_event(
            "task.rejected",
            Some("task-42"),
            serde_json::json!({ "status": "failed" }),
        );
        let line = format_workflow_event_card(&event).map(|(s, _)| s).expect("event should render");
        assert!(line.contains("Approval rejected: task-42"));
    }

    #[test]
    fn test_format_user_interaction_prompt_lists_options() {
        let interaction = UserInteraction {
            interaction_id: "ui-deadbeef".to_string(),
            session_id: "s1".to_string(),
            root_session_id: "s1".to_string(),
            agent_id: "lead".to_string(),
            turn_id: "turn-1".to_string(),
            kind: UserInteractionKind::Decision,
            question: "Ship it?".to_string(),
            context: Some("Release is tagged.".to_string()),
            options: vec![
                UserInteractionOption {
                    id: "yes".to_string(),
                    label: "Yes".to_string(),
                    value: "ship".to_string(),
                },
                UserInteractionOption {
                    id: "no".to_string(),
                    label: "No".to_string(),
                    value: "hold".to_string(),
                },
            ],
            allow_freeform: true,
            status: UserInteractionStatus::Pending,
            answer_option_id: None,
            answer_text: None,
            answered_by: None,
            created_at: "2026-03-25T00:00:00Z".to_string(),
            answered_at: None,
            expires_at: None,
            workflow_id: None,
            task_id: None,
            checkpoint_turn_id: None,
        };
        let card = format_user_interaction_prompt(&interaction);
        assert!(card.contains("ui-deadbeef"));
        assert!(card.contains("kind: decision"));
        assert!(card.contains("Ship it?"));
        assert!(card.contains("Release is tagged."));
        assert!(card.contains("[yes]"));
        assert!(card.contains("→ ship"));
        assert!(card.contains("--option <id>"));
        assert!(card.contains("--text"));
    }
}
