//! TUI Chat interface using ratatui + crossterm.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use tokio::net::TcpStream;

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};

use super::agent::format_llm_usage_for_cli;
use super::common::{
    default_terminal_channel_id, default_terminal_sender_id, terminal_channel_envelope,
};
use autonoetic_gateway::router::{
    JsonRpcRequest as GatewayJsonRpcRequest, JsonRpcResponse as GatewayJsonRpcResponse,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::LlmExchangeUsage;
use autonoetic_types::background::UserInteraction;

// ============================================================================
// Constants
// ============================================================================

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ============================================================================
// App State
// ============================================================================

#[derive(Debug, Clone)]
enum MessageRole {
    User,
    Assistant,
    System,
    Signal,
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
                let shortened: String = compact.chars().take(60).collect();
                if compact.chars().count() > 60 {
                    format!(" | event:{}...", shortened)
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
    session_id: String,
    target_hint: String,
    // Mouse selection - stored as CONTENT positions (row, col), not screen positions
    selecting: bool,
    sel_start: Option<(usize, usize)>, // (content_row, content_col)
    sel_end: Option<(usize, usize)>,   // (content_row, content_col)
    signal_resume_by_internal_id: HashMap<u64, SignalResumeRef>,
    signal_resume_inflight: HashSet<String>,
    seen_workflow_event_ids: HashSet<String>,
    workflow_events_bootstrapped: bool,
    current_workflow_id: Option<String>,
    session_overview: SessionOverview,
    /// `user.ask` cards we already showed for this TUI session (avoid duplicate polls).
    seen_user_interaction_prompts: HashSet<String>,
    // Persistent clipboard — must stay alive so arboard's background ownership
    // thread keeps running and clipboard managers have time to capture the content.
    clipboard: Option<arboard::Clipboard>,
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
            session_id,
            target_hint,
            selecting: false,
            sel_start: None,
            sel_end: None,
            signal_resume_by_internal_id: HashMap::new(),
            signal_resume_inflight: HashSet::new(),
            seen_workflow_event_ids: HashSet::new(),
            workflow_events_bootstrapped: false,
            current_workflow_id: None,
            session_overview: SessionOverview::default(),
            seen_user_interaction_prompts: HashSet::new(),
            // Safe clipboard initialization - arboard can panic on headless/SSH systems
            clipboard: std::panic::catch_unwind(|| arboard::Clipboard::new().ok()).unwrap_or(None),
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

    fn content_line_count(&self) -> usize {
        let mut count = 0usize;
        for msg in &self.messages {
            count = count.saturating_add(msg.content.lines().count());
            count = count.saturating_add(1);
        }
        if !self.pending.is_empty() {
            count = count.saturating_add(1);
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
            latest_signal: None,
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
        app.session_overview.latest_signal = Some(format!(
            "user.ask {}",
            interaction.interaction_id
        ));
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
) -> Option<String> {
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

    let text = match event.event_type.as_str() {
        "workflow.started" => Some(format!("📋 [{}] Workflow started", ts_short)),
        "task.spawned" => Some(format!("🚀 [{}] Task spawned: {}", ts_short, task)),
        "task.queued" => Some(format!("📥 [{}] Task queued: {}", ts_short, task)),
        "task.awaiting_approval" => {
            // Show what kind of approval is needed
            let kind = if approval.contains("sandbox") {
                "sandbox.exec".to_string()
            } else if approval.contains("agent_install") {
                "agent.install".to_string()
            } else {
                "tool execution".to_string()
            };
            Some(format!(
                "⏸ [{}] Approval required: {} ({})",
                ts_short, task, kind
            ))
        }
        "task.approved" => Some(format!("✅ [{}] Approval approved: {}", ts_short, task)),
        "task.rejected" => Some(format!("❌ [{}] Approval rejected: {}", ts_short, task)),
        "task.started" => Some(format!("▶ [{}] Task started: {}", ts_short, task)),
        "task.completed" => Some(format!("✅ [{}] Task completed: {}", ts_short, task)),
        "task.failed" => Some(format!("❌ [{}] Task failed: {}", ts_short, task)),
        "task.cancelled" => Some(format!("🚫 [{}] Task cancelled: {}", ts_short, task)),
        "task.paused" => Some(format!("⏸ [{}] Task paused: {}", ts_short, task)),
        "workflow.join.satisfied" => Some(format!("✅ [{}] Workflow join satisfied", ts_short)),
        "workflow.checkpoint.saved" => Some(format!("💾 [{}] Workflow checkpoint saved", ts_short)),
        "task.checkpoint.saved" => {
            Some(format!("💾 [{}] Task checkpoint saved: {}", ts_short, task))
        }
        "task.updated" if status == "runnable" => {
            Some(format!("🔁 [{}] Task resumed: {}", ts_short, task))
        }
        "task.updated" => Some(format!(
            "🔄 [{}] Task updated: {} ({})",
            ts_short, task, status
        )),
        other => {
            // Catch-all: show unknown event types instead of silently dropping them
            Some(format!("⚡ [{}] {} (task: {})", ts_short, other, task))
        }
    };

    text
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
        lines.push(format!("subject: {}", details.join(" | ")));
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

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Status
            Constraint::Length(1), // Separator
            Constraint::Min(5),    // Messages
            Constraint::Length(3), // Input
        ])
        .split(area);

    // Status
    draw_status(f, app, chunks[0]);

    // Separator
    let sep = Paragraph::new(Line::from(Span::styled(
        "─".repeat(chunks[1].width as usize),
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(sep, chunks[1]);

    // Messages
    draw_messages(f, app, chunks[2]);

    // Input
    draw_input(f, app, chunks[3]);

    // Pin the terminal cursor inside the input box so it never wanders to the
    // last mouse position during a drag-selection.
    // Layout: top border = +1 row, "> " prefix = +2 cols, cursor_pos = byte offset.
    let before_cursor_display_width = app.input[..app.cursor_pos].chars().count() as u16;
    let cursor_x = (chunks[3].x + 2 + before_cursor_display_width)
        .min(chunks[3].x + chunks[3].width.saturating_sub(1));
    let cursor_y = chunks[3].y + 1;
    f.set_cursor_position((cursor_x, cursor_y));
}

fn draw_messages(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
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
        };

        for (i, text_line) in msg.content.lines().enumerate() {
            let prefix = if i == 0 { icon } else { "  " };

            // Compare content row against selection bounds.
            let is_selected =
                row >= content_sel_top && row <= content_sel_bot && content_sel_top != usize::MAX;

            if is_selected {
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
            } else {
                lines.push(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(text_line.to_string(), style),
                ]));
            }

            row = row.saturating_add(1);
        }
        lines.push(Line::raw(""));
        row = row.saturating_add(1);
    }

    // Pending indicator
    if !app.pending.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            format!(
                "{} Working... ({} pending, {}s)",
                app.spinner(),
                app.pending.len(),
                app.oldest_secs()
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        )]));
    }

    let p = Paragraph::new(Text::from(lines))
        .scroll((app.effective_scroll_offset() as u16, 0))
        .block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
    f.render_widget(p, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let workflow = app.session_overview.status_line();
    let text = if !app.pending.is_empty() {
        format!(
            "{} {} pending | {} | Enter: send | Scroll: Shift+↑↓ | Quit: Ctrl+C",
            app.spinner(),
            app.pending.len(),
            workflow,
        )
    } else {
        format!(
            "Session: {} | Target: {} | {} | Enter: send | Scroll: Shift+↑↓ | Quit: Ctrl+C",
            &app.session_id[..20.min(app.session_id.len())],
            app.target_hint,
            workflow,
        )
    };

    let p = Paragraph::new(Span::styled(text, Style::default().fg(Color::DarkGray)));
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

// ============================================================================
// Main Entry Point
// ============================================================================

pub async fn handle_chat(config_path: &Path, args: &super::common::ChatArgs) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let target_hint = args.agent_id.as_deref().unwrap_or("default-lead");
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

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = App::new(session_id.clone(), target_hint.to_string());
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

    // Show session info and workflow hint
    let root_session = autonoetic_gateway::runtime::content_store::root_session_id(&session_id);
    app.add_message(
        MessageRole::System,
        format!("Session: {} (root: {})", session_id, root_session),
    );

    // Check if workflow exists for this session
    if let Ok(Some(wf_id)) =
        autonoetic_gateway::scheduler::resolve_workflow_id_for_root_session(&config, root_session)
    {
        app.add_message(
            MessageRole::System,
            format!("🔗 Connected to workflow: {}", wf_id),
        );
    } else {
        app.add_message(
            MessageRole::System,
            format!("ℹ No workflow found for root session '{}'. Use --session-id to connect to an existing workflow.", root_session),
        );
    }

    app.add_message(
        MessageRole::System,
        format!("Connecting to {}...", gateway_addr),
    );

    // Channel for sending messages from TUI to gateway
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, String)>();

    // Map gateway request IDs to internal IDs
    let mut pending_map: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    // Signal check interval
    let mut signal_interval = tokio::time::interval(Duration::from_secs(1));
    signal_interval.tick().await;

    // Open gateway store for approvals and signals (same path as gateway daemon)
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(config.as_ref());
    let gateway_store =
        match autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir) {
            Ok(store) => {
                app.add_message(
                    MessageRole::System,
                    format!("✓ Gateway store connected: {}", gateway_dir.display()),
                );
                Some(store)
            }
            Err(e) => {
                app.add_message(
                    MessageRole::System,
                    format!(
                        "⚠ Gateway store unavailable: {} (approvals may not be visible)",
                        e
                    ),
                );
                None
            }
        };

    if let Some(ref store) = gateway_store {
        if let Ok(snapshot) = poll_session_snapshot(config.as_ref(), Some(store), &session_id) {
            app.session_overview = snapshot.overview.clone();
            let _ = append_new_pending_user_interaction_prompts(&mut app, &snapshot.pending_interactions);
        }
    }

    // Main loop
    loop {
        // Connect
        let stream = match TcpStream::connect(&gateway_addr).await {
            Ok(s) => s,
            Err(e) => {
                app.add_message(
                    MessageRole::System,
                    format!("Gateway connection failed (reconnecting in 3s): {}", e),
                );
                terminal.draw(|f| draw(f, &app))?;
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };
        let (read_half, write_half) = stream.into_split();
        let mut gateway_lines = BufReader::new(read_half).lines();

        let disconnected = run_loop(
            &mut terminal,
            &mut app,
            write_half,
            &mut gateway_lines,
            &config,
            gateway_store.as_ref(),
            &session_id,
            &envelope,
            &tx,
            &mut rx,
            &mut pending_map,
            &mut signal_interval,
        )
        .await?;

        if !disconnected {
            break; // User quit explicitly
        }

        app.add_message(
            MessageRole::System,
            "Gateway disconnected, reconnecting in 3s...".to_string(),
        );
        terminal.draw(|f| draw(f, &app))?;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // Cleanup
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    mut write_half: tokio::net::tcp::OwnedWriteHalf,
    gateway_lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>>,
    config: &autonoetic_types::config::GatewayConfig,
    gateway_store: Option<&autonoetic_gateway::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
    envelope: &serde_json::Value,
    tx: &tokio::sync::mpsc::UnboundedSender<(u64, String)>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, String)>,
    pending_map: &mut std::collections::HashMap<String, u64>,
    signal_interval: &mut tokio::time::Interval,
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
            let area = terminal.size()?;
            let messages_height = area.height.saturating_sub(5) as usize;
            app.last_max_scroll_offset = app
                .content_line_count()
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

                                    let new_user_prompts = if let Some(store) = gateway_store {
                                        match poll_session_snapshot(config, Some(store), session_id) {
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

                                    if let Some(arr) =
                                        result_json.and_then(|v| v.get("llm_usage"))
                                    {
                                        if let Ok(usages) =
                                            serde_json::from_value::<Vec<LlmExchangeUsage>>(arr.clone())
                                        {
                                            if let Some(text) = format_llm_usage_for_cli(&usages) {
                                                app.add_message(MessageRole::System, text);
                                            }
                                        }
                                    }


                                }
                                needs_redraw = true;
                            }
                        }
                    }
                    Ok(None) => {
                        return Ok(true); // Disconnected
                    }
                    Err(e) => {
                        app.add_message(MessageRole::System, format!("Gateway error: {}", e));
                        return Ok(true); // Disconnected
                    }
                }
            }

            // User message to send
            msg = rx.recv() => {
                if let Some((id, message)) = msg {
                    let req_id = format!("tui-{}", id);
                    pending_map.insert(req_id.clone(), id);

                    let params = serde_json::json!({
                        "event_type": "chat",
                        "message": message,
                        "session_id": session_id,
                        "metadata": envelope,
                    });

                    let request = GatewayJsonRpcRequest {
                        jsonrpc: "2.0".to_string(),
                        id: req_id,
                        method: "event.ingest".to_string(),
                        params,
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
                            if !handle_key(key, app, tx)? {
                                return Ok(false); // Clean Quit
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

fn handle_key(
    key: crossterm::event::KeyEvent,
    app: &mut App,
    tx: &tokio::sync::mpsc::UnboundedSender<(u64, String)>,
) -> anyhow::Result<bool> {
    match key.code {
        // Quit
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(false),

        // Send
        KeyCode::Enter => {
            if !app.input.is_empty() {
                let msg = std::mem::take(&mut app.input);
                app.cursor_pos = 0;
                let id = app.next_id();
                app.add_pending(id);
                app.add_message(MessageRole::User, msg.clone());
                let _ = tx.send((id, msg));
            }
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

        _ => {}
    }

    Ok(true)
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

    let snapshot = match poll_session_snapshot(config, store, session_id) {
        Ok(snapshot) => snapshot,
        Err(e) => {
            tracing::warn!(target: "chat", error = %e, "Failed to poll session snapshot");
            return false;
        }
    };

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
                format!("🔗 Workflow connected: {}", app.session_overview.status_line()),
            );
            processed_any = true;
        }
    }

    match snapshot.overview.workflow.workflow_id.clone() {
        Some(workflow_id) => {
            tracing::debug!(target: "chat", workflow_id = %workflow_id, "Resolved workflow ID");

            // Detect workflow ID change → force re-bootstrap
            let workflow_changed = app.current_workflow_id.as_ref() != Some(&workflow_id);
            if workflow_changed {
                tracing::info!(
                    target: "chat",
                    old = ?app.current_workflow_id,
                    new = %workflow_id,
                    "Workflow ID changed, resetting event tracking"
                );
                app.workflow_events_bootstrapped = false;
                app.seen_workflow_event_ids.clear();
                app.current_workflow_id = Some(workflow_id.clone());
            }

            match autonoetic_gateway::scheduler::load_workflow_events(config, store, &workflow_id) {
                Ok(events) => {
                    tracing::info!(target: "chat", event_count = events.len(), workflow_id = %workflow_id, "Loaded workflow events");
                    let current_workflow_count = events.len();
                    let previous_seen_count = app.seen_workflow_event_ids.len();

                    let should_bootstrap = !app.workflow_events_bootstrapped
                        || current_workflow_count < previous_seen_count
                        || current_workflow_count > previous_seen_count + 50; // Large jump = new workflow

                    if should_bootstrap {
                        let recap_count = events.len().min(20);
                        if recap_count > 0 {
                            app.add_message(
                                MessageRole::System,
                                "── workflow recap ──".to_string(),
                            );
                            let start_idx = events.len().saturating_sub(recap_count);
                            for event in &events[start_idx..] {
                                if let Some(card) = format_workflow_event_card(event) {
                                    app.session_overview.latest_signal = Some(card.clone());
                                    app.add_message(MessageRole::Signal, card);
                                }
                            }
                            app.add_message(MessageRole::System, "── live updates ──".to_string());
                        }
                        // Mark ALL fetched events as seen, not just the recap window,
                        // so events outside the recap don't re-appear as "new" on next poll.
                        app.seen_workflow_event_ids.clear();
                        for event in &events {
                            app.seen_workflow_event_ids.insert(event.event_id.clone());
                        }
                        app.workflow_events_bootstrapped = true;

                        tracing::debug!(
                            target: "chat",
                            workflow_id = %workflow_id,
                            total_events = events.len(),
                            recap_shown = recap_count,
                            "workflow events bootstrapped"
                        );
                    } else {
                        let mut new_event_count = 0usize;
                        for event in events {
                            if app.seen_workflow_event_ids.insert(event.event_id.clone()) {
                                // NEW event - process it (insert returns true if newly added)
                                tracing::debug!(
                                    target: "chat",
                                    event_id = %event.event_id,
                                    event_type = %event.event_type,
                                    "New workflow event detected"
                                );
                                if let Some(card) = format_workflow_event_card(&event) {
                                    tracing::debug!(
                                        target: "chat",
                                        card = %card,
                                        "Formatted workflow event card"
                                    );
                                    app.session_overview.latest_signal = Some(card.clone());
                                    app.add_message(MessageRole::Signal, card);
                                    processed_any = true;
                                } else {
                                    tracing::warn!(
                                        target: "chat",
                                        event_type = %event.event_type,
                                        "Failed to format workflow event card"
                                    );
                                }
                                new_event_count += 1;
                            }
                        }
                        if new_event_count > 0 {
                            tracing::debug!(
                                target: "chat",
                                new_event_count,
                                total_seen = app.seen_workflow_event_ids.len(),
                                "check_signals: processed new workflow events"
                            );
                        }
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
        None => {
            // No workflow found - this is normal if session is not connected to a workflow
        }
    }

    let new_prompts = append_new_pending_user_interaction_prompts(app, &snapshot.pending_interactions);
    if new_prompts > 0 {
        processed_any = true;
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
    fn test_extract_approval_request_id_uuid_fallback() {
        let text = "Approval required for request id: c19a8a50-d6c8-4c5f-aa3c-6ba119751b11";
        assert_eq!(
            extract_approval_request_id(text).as_deref(),
            Some("c19a8a50-d6c8-4c5f-aa3c-6ba119751b11")
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
                "approval": "sandbox_exec"
            }),
        );
        let line = format_workflow_event_card(&event).expect("event should render");
        assert!(line.contains("Approval required: task-42"));
        assert!(line.contains("sandbox.exec"));
    }

    #[test]
    fn test_format_workflow_event_card_task_approved() {
        let event = workflow_event(
            "task.approved",
            Some("task-42"),
            serde_json::json!({ "status": "runnable" }),
        );
        let line = format_workflow_event_card(&event).expect("event should render");
        assert!(line.contains("Approval approved: task-42"));
    }

    #[test]
    fn test_format_workflow_event_card_task_rejected() {
        let event = workflow_event(
            "task.rejected",
            Some("task-42"),
            serde_json::json!({ "status": "failed" }),
        );
        let line = format_workflow_event_card(&event).expect("event should render");
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
