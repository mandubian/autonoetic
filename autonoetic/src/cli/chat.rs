//! TUI Chat interface using ratatui + crossterm.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use tokio::net::TcpStream;

use crossterm::{
    cursor::{Hide, Show},
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
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
use autonoetic_types::agent::LlmExchangeUsage;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction, UserInteraction,
    UserInteractionStatus,
};
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
const HINTS_PANE_WIDTH: u16 = 44;
const FOOTER_INPUT_MIN_WIDTH: u16 = 28;
const POLICY_CAUSAL_POLL_LIMIT: i64 = 48;
const POLICY_CAUSAL_PANE_MAX: usize = 8;
/// Max lines kept for input-line recall (↑/↓ in chat TUI).
const PROMPT_HISTORY_MAX: usize = 500;

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

#[derive(Debug, Clone, Copy)]
enum MessageRole {
    User,
    Assistant,
    System,
    Signal,
    SignalLow,
    AgentOutput,
}

#[derive(Debug, Clone)]
enum RichCard {
    UserInteraction(Box<UserInteraction>),
    Approval {
        request: Box<ApprovalRequest>,
        detail: String,
        enrichment: Vec<autonoetic_gateway::runtime::human_gate::GateMessage>,
    },
}

#[derive(Debug, Clone)]
struct ChatMessage {
    role: MessageRole,
    content: String,
    rich_card: Option<RichCard>,
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
    active_executions: usize,
    latest_signal: Option<String>,
    /// (session_id, agent_id, turn_count, status) for each in-progress session
    /// under the root. Status is one of "active" (currently executing a turn)
    /// or "suspended" (paused between turns or awaiting external input).
    active_sessions: Vec<(String, String, i64, String)>,
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

        let active_exec = if self.active_executions > 0 {
            format!(" | exec:{}", self.active_executions)
        } else {
            String::new()
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

        let turns = if self.active_sessions.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = self.active_sessions.iter().take(3).map(|(_, agent, turns, status)| {
                let agent_short = agent.split('.').next().unwrap_or(agent);
                let paused = if status == "suspended" { " (paused)" } else { "" };
                format!("{}:t{}{}", agent_short, turns, paused)
            }).collect();
            let suffix = if self.active_sessions.len() > 3 {
                format!(" +{}", self.active_sessions.len() - 3)
            } else {
                String::new()
            };
            format!(" | turns:{}{}", parts.join(","), suffix)
        };

        format!("{}{}{}{}{}", workflow, active_exec, ask, turns, latest_signal)
    }
}

#[derive(Debug, Clone)]
struct SessionPollSnapshot {
    overview: SessionOverview,
    pending_interactions: Vec<UserInteraction>,
    active_workbench: Option<WorkbenchOverview>,
}

#[derive(Debug, Clone, Default)]
struct LiveTaskSummary {
    task_id: String,
    agent_id: String,
    status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WorkbenchOverview {
    workbench_id: String,
    status: String,
    base_artifact_id: String,
    file_count: usize,
    changed_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KnownSessionEntry {
    session_id: String,
    primary_agent_id: Option<String>,
    agent_ids: Vec<String>,
    first_timestamp: Option<String>,
    last_timestamp: Option<String>,
    event_count: usize,
}

#[derive(Debug, Clone)]
enum PendingPrompt {
    SessionSelection { sessions: Vec<KnownSessionEntry> },
    NewSessionName,
}

#[derive(Debug, Clone)]
enum PendingItem {
    Approval(Box<ApprovalRequest>),
    Interaction(Box<UserInteraction>),
}

struct PendingOverlay {
    items: Vec<PendingItem>,
    selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SlashCommand {
    Help,
    Quit,
    Status,
    Session,
    SessionNew(Option<String>),
    SessionSwitch(String),
    Cancel,
    Why(Option<String>),
    Persona(Option<String>),
    Policy(String),
    Pending,
    WbReconcile,
    WbDiscard,
    WbDiff,
    WbStatus,
    /// `/return [--force] [note...]` — hand the active workbench back to the
    /// selected orchestrator (default: planner.default). Refuses to proceed
    /// when the workbench has unsaved edits unless `--force` is supplied.
    ReturnToAgent { force: bool, message: Option<String> },
}

#[derive(Debug, Clone)]
enum ChatOutbound {
    Chat(String),
    PolicyAuthor(String),
    /// Query session.status for the async planner reply after workflow completion.
    SessionStatusQuery { session_id: String },
    /// Notify the gateway that an approval was resolved so it can transition
    /// `async_results` from `SuspendedApproval` to `Processing`.
    ApprovalResolved { session_id: String, root_session_id: Option<String> },
    /// Hand the active workbench back to a selected orchestrator. The
    /// `message` is the natural-language wake-up string the orchestrator
    /// sees; `target_agent_id` is the resolved orchestrator id
    /// (planner.default by default). `metadata` carries the structured
    /// `workbench_reconciled` payload for the gateway to attach to the
    /// event for downstream tooling.
    ReturnToAgent {
        message: String,
        target_agent_id: String,
        metadata: serde_json::Value,
    },
}

#[derive(Debug, Clone)]
struct TaskLifecycle {
    agent_suffix: String,
    /// Display labels, e.g. `completed` or `completed (gate: fail)`.
    stages: Vec<String>,
    /// The spawn reason / intent from the task.spawned event (200-char preview).
    spawn_reason: Option<String>,
    /// Full spawn reason from task.spawned event (for expand).
    spawn_reason_full: Option<String>,
    /// Result summary from task.completed/failed event payload.
    result_summary: Option<String>,
    /// Whether the user has toggled this lifecycle to expanded view.
    expanded: bool,
}

/// Aggregate token statistics across all completions in the session.
#[derive(Debug, Clone)]
struct TokenStats {
    count: u64,
    total_input: u64,
    total_output: u64,
    max_input: u64,
    min_input: u64,
    max_output: u64,
    min_output: u64,
    total_cost: f64,
    cost_count: u64,
}

impl TokenStats {
    fn new() -> Self {
        Self {
            count: 0,
            total_input: 0,
            total_output: 0,
            max_input: 0,
            min_input: u64::MAX,
            max_output: 0,
            min_output: u64::MAX,
            total_cost: 0.0,
            cost_count: 0,
        }
    }

    fn record(&mut self, usage: &LlmExchangeUsage) {
        self.count += 1;
        self.total_input = self.total_input.saturating_add(usage.input_tokens);
        self.total_output = self.total_output.saturating_add(usage.output_tokens);
        self.max_input = self.max_input.max(usage.input_tokens);
        self.min_input = self.min_input.min(usage.input_tokens);
        self.max_output = self.max_output.max(usage.output_tokens);
        self.min_output = self.min_output.min(usage.output_tokens);
        if let Some(cost) = usage.estimated_cost_usd {
            self.total_cost += cost;
            self.cost_count += 1;
        }
    }

    fn avg_input(&self) -> u64 {
        if self.count == 0 { 0 } else { self.total_input / self.count }
    }

    fn avg_output(&self) -> u64 {
        if self.count == 0 { 0 } else { self.total_output / self.count }
    }
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
    sender_id: String,
    channel_id: String,
    // Mouse selection - stored as CONTENT positions (row, col), not screen positions
    selecting: bool,
    sel_start: Option<(usize, usize)>, // (content_row, content_col)
    sel_end: Option<(usize, usize)>,   // (content_row, content_col)
    click_down_screen: Option<(u16, u16)>,
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
    /// Action-type summaries for pending approvals, synced with pending_approval_ids.
    /// Each entry is (request_id, action_type_or_summary_snippet).
    pending_approval_summaries: Vec<(String, String)>,
    /// Whether inline approvals are enabled (from `config.chat.inline_approvals`).
    inline_approvals_enabled: bool,
    /// Store-derived approval IDs we already announced (avoid repeating every poll).
    announced_store_approval_ids: HashSet<String>,
    /// Gateway TCP JSON-RPC connection state.
    gateway_connected: bool,
    /// Causal events already surfaced in the policy pane.
    seen_causal_policy_event_ids: HashSet<String>,
    /// Newest-first policy decision lines for the right pane.
    policy_causal_pane: Vec<String>,
    /// Gate history: resolved approvals for this session tree.
    gate_history_approvals: Vec<String>,
    /// Gate history: resolved user interactions for this session tree.
    gate_history_interactions: Vec<String>,
    /// Question text(s) for pending user interactions — shown in the right panel.
    pending_question_summaries: Vec<String>,
    /// Submitted user lines, newest last — for ↑/↓ recall in the prompt.
    prompt_history: Vec<String>,
    /// When set, the input shows `prompt_history[len - 1 - k]` (`k == 0` is the most recent submission).
    prompt_history_scroll_back: Option<usize>,
    /// Draft in the input before the first ↑ during this browse; restored when ↓ passes the newest recall.
    prompt_history_draft: Option<String>,
    /// If set, the next submitted line is handled as structured input instead of chat.
    pending_prompt: Option<PendingPrompt>,
    /// Aggregate token statistics across all completions this session.
    token_stats: TokenStats,
    /// Latest prompt footprint from the gateway: largest `input_tokens` among `llm_usage` entries
    /// for the last completed `event.ingest` (sync) response.
    last_llm_context: Option<LlmExchangeUsage>,
    /// Synthetic pending IDs added after inline approval so the spinner shows
    /// until the scheduler picks up the Runnable task.
    post_approval_pending_ids: Vec<u64>,
    /// Internal pending IDs that correspond to session.status queries (not chat ingests).
    pending_session_status_ids: HashSet<u64>,
    /// Root session IDs for which we already sent a session.status query after workflow completion.
    queried_session_status_for_workflows: HashSet<String>,
    task_lifecycles: HashMap<String, TaskLifecycle>,
    task_lifecycle_msg_idx: HashMap<String, usize>,
    pending_overlay: Option<PendingOverlay>,
    wrap_width: u16,
    messages_area_row_end: u16,
    active_workbench: Option<WorkbenchOverview>,
}

impl App {
    fn new(session_id: String, target_hint: String, sender_id: String, channel_id: String) -> Self {
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
            sender_id,
            channel_id,
            selecting: false,
            sel_start: None,
            sel_end: None,
            click_down_screen: None,
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
            pending_approval_summaries: Vec::new(),
            inline_approvals_enabled: false,
            announced_store_approval_ids: HashSet::new(),
            gateway_connected: false,
            seen_causal_policy_event_ids: HashSet::new(),
            policy_causal_pane: Vec::new(),
            gate_history_approvals: Vec::new(),
            gate_history_interactions: Vec::new(),
            pending_question_summaries: Vec::new(),
            prompt_history: Vec::new(),
            prompt_history_scroll_back: None,
            prompt_history_draft: None,
            pending_prompt: None,
            token_stats: TokenStats::new(),
            last_llm_context: None,
            post_approval_pending_ids: Vec::new(),
            pending_session_status_ids: HashSet::new(),
            queried_session_status_for_workflows: HashSet::new(),
            task_lifecycles: HashMap::new(),
            task_lifecycle_msg_idx: HashMap::new(),
            pending_overlay: None,
            wrap_width: 80,
            messages_area_row_end: 0,
            active_workbench: None,
        }
    }

    fn input_prefix(&self) -> &'static str {
        match self.pending_prompt {
            Some(PendingPrompt::SessionSelection { .. }) => "session> ",
            Some(PendingPrompt::NewSessionName) => "session name> ",
            None => "> ",
        }
    }

    fn clear_prompt_history_browse(&mut self) {
        self.prompt_history_scroll_back = None;
        self.prompt_history_draft = None;
    }

    fn push_prompt_history(&mut self, line: String) {
        let line = line.trim_end().to_string();
        if line.is_empty() {
            return;
        }
        if self.prompt_history.last().map(|s| s.as_str()) == Some(line.as_str()) {
            return;
        }
        self.prompt_history.push(line);
        while self.prompt_history.len() > PROMPT_HISTORY_MAX {
            self.prompt_history.remove(0);
        }
    }

    fn apply_prompt_history_line(&mut self, scroll_back: usize) {
        let len = self.prompt_history.len();
        if len == 0 {
            return;
        }
        let idx = len - 1 - scroll_back;
        if idx < len {
            self.input = self.prompt_history[idx].clone();
            self.cursor_pos = self.input.len();
        }
    }

    fn prompt_history_up(&mut self) {
        let len = self.prompt_history.len();
        if len == 0 {
            return;
        }
        match self.prompt_history_scroll_back {
            None => {
                self.prompt_history_draft = Some(self.input.clone());
                self.prompt_history_scroll_back = Some(0);
                self.apply_prompt_history_line(0);
            }
            Some(k) if k + 1 < len => {
                let nk = k + 1;
                self.prompt_history_scroll_back = Some(nk);
                self.apply_prompt_history_line(nk);
            }
            Some(_) => {}
        }
    }

    fn prompt_history_down(&mut self) {
        match self.prompt_history_scroll_back {
            None => {}
            Some(0) => {
                self.prompt_history_scroll_back = None;
                self.input = self.prompt_history_draft.take().unwrap_or_default();
                self.cursor_pos = self.input.len();
            }
            Some(k) => {
                let nk = k.saturating_sub(1);
                self.prompt_history_scroll_back = Some(nk);
                self.apply_prompt_history_line(nk);
            }
        }
    }

    /// Leaving recall mode on the first edit after ↑ (draft is discarded).
    fn end_prompt_history_if_browsing(&mut self) {
        if self.prompt_history_scroll_back.take().is_some() {
            self.prompt_history_draft = None;
        }
    }

    fn add_message(&mut self, role: MessageRole, content: String) {
        self.messages.push(ChatMessage {
            role,
            content,
            rich_card: None,
        });
        if self.follow_output {
            self.scroll_offset = self.last_max_scroll_offset;
        }
    }

    fn add_rich_card(&mut self, role: MessageRole, fallback: String, card: RichCard) {
        self.messages.push(ChatMessage {
            role,
            content: fallback,
            rich_card: Some(card),
        });
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

    fn has_active_work(&self) -> bool {
        !self.pending.is_empty()
            || !self.pending_approval_ids.is_empty()
            || self.session_overview.pending_user_interactions > 0
            || self.session_overview.workflow.running > 0
            || self.session_overview.workflow.queued > 0
            || self.session_overview.workflow.awaiting > 0
            || self.session_overview.active_executions > 0
            || !self.session_overview.active_sessions.is_empty()
    }

    /// Same selection logic as the bottom-of-transcript indicator in `draw_messages`.
    /// Centralized so `content_line_count` and `draw_messages` cannot drift apart and
    /// hide the spinner by miscounting transcript height.
    fn active_work_text(&self) -> Option<String> {
        if !self.has_active_work() {
            return None;
        }

        let primary_activity = self.current_activity_summary();

        let text = if !self.pending.is_empty() {
            let activity = primary_activity.as_deref().map(|a| format!(" │ {}", a)).unwrap_or_default();
            format!(
                "{} Working... ({} pending, {}s){}",
                self.spinner(),
                self.pending.len(),
                self.oldest_secs(),
                activity
            )
        } else if !self.pending_approval_ids.is_empty() {
            let activity = primary_activity.as_deref().map(|a| format!(" │ {}", a)).unwrap_or_default();
            format!(
                "{} Waiting for approval ({} pending){}",
                self.spinner(),
                self.pending_approval_ids.len(),
                activity
            )
        } else if self.session_overview.pending_user_interactions > 0 {
            format!(
                "{} Awaiting your response... ({} pending)",
                self.spinner(),
                self.session_overview.pending_user_interactions
            )
        } else if self.session_overview.workflow.awaiting > 0 {
            let activity = primary_activity.as_deref().map(|a| format!(" │ {}", a)).unwrap_or_default();
            format!(
                "{} Waiting on approval ({} task(s)){}",
                self.spinner(),
                self.session_overview.workflow.awaiting,
                activity
            )
        } else if self.session_overview.active_executions > 0 {
            let activity = primary_activity.as_deref().map(|a| format!(" │ {}", a)).unwrap_or_default();
            format!(
                "{} Working... ({} active execution(s)){}",
                self.spinner(),
                self.session_overview.active_executions,
                activity
            )
        } else {
            let activity = primary_activity.as_deref().map(|a| format!(" │ {}", a)).unwrap_or_default();
            format!("{} Working...{}", self.spinner(), activity)
        };
        Some(text)
    }

    fn current_activity_summary(&self) -> Option<String> {
        let sessions = &self.session_overview.active_sessions;
        if sessions.is_empty() {
            return None;
        }
        let (_session_id, agent_id, turns, status) = sessions.first()?;
        let agent_short = agent_id.split('.').next().unwrap_or(agent_id);
        let mut summary = format!("{}:t{}", agent_short, turns);
        if status == "suspended" {
            summary.push_str(" (paused)");
        }
        if sessions.len() > 1 {
            summary.push_str(&format!(" +{} more", sessions.len() - 1));
        }
        Some(summary)
    }

    fn tick_spinner(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
    }

    fn spinner(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_frame]
    }

    fn insert_char(&mut self, c: char) {
        self.end_prompt_history_if_browsing();
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    fn delete_char(&mut self) {
        self.end_prompt_history_if_browsing();
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

    fn content_line_count(&self, wrap_width: u16) -> usize {
        let ww = wrap_width.max(1);
        let mut count = 0usize;
        for msg in &self.messages {
            if let Some(ref card) = msg.rich_card {
                let rich_lines = match card {
                    RichCard::UserInteraction(interaction) => {
                        render_interaction_card(interaction, wrap_width)
                    }
                    RichCard::Approval {
                        request,
                        detail,
                        enrichment,
                    } => render_approval_card(request, detail, enrichment, wrap_width),
                };
                for rl in &rich_lines {
                    count = count.saturating_add(transcript_wrap_line_count(rl.clone(), ww));
                }
                count = count.saturating_add(transcript_wrap_line_count(Line::raw(""), ww));
            } else {
                let icon = match msg.role {
                    MessageRole::User => "> ",
                    MessageRole::Assistant => "🤖 ",
                    MessageRole::System => "ℹ ",
                    MessageRole::Signal => "🔔 ",
                    MessageRole::SignalLow => "  ",
                    MessageRole::AgentOutput => "📝 ",
                };
                let style = message_role_style(msg.role);

                for (i, text_line) in msg.content.lines().enumerate() {
                    let prefix = if i == 0 { icon } else { "  " };
                    count = count.saturating_add(transcript_wrap_line_count(
                        Line::from(vec![
                            Span::raw(prefix),
                            Span::styled(text_line.to_string(), style),
                        ]),
                        ww,
                    ));
                }
                count = count.saturating_add(transcript_wrap_line_count(Line::raw(""), ww));
            }
        }
        if let Some(pending_text) = self.active_work_text() {
            count = count.saturating_add(transcript_wrap_line_count(
                Line::from(vec![Span::styled(
                    pending_text,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                )]),
                ww,
            ));
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

    fn in_messages_area(&self, row: u16) -> bool {
        // Allow 1-row tolerance at the bottom boundary to handle imprecise
        // mouse clicks on [click to expand] text near the input area.
        // The input area has Borders::TOP at messages_area_row_end, so the
        // last message line sits right above it with zero gap.
        row >= 2 && row < self.messages_area_row_end + 1
    }

    fn toggle_lifecycle_at_content_row(&mut self, content_row: usize) -> bool {
        let ww = self.wrap_width.max(1);
        let mut row: usize = 0;
        let mut found_task_id: Option<String> = None;
        for (msg_idx, msg) in self.messages.iter().enumerate() {
            let msg_height = if let Some(ref card) = msg.rich_card {
                let rich_lines = match card {
                    RichCard::UserInteraction(interaction) => {
                        render_interaction_card(interaction, ww)
                    }
                    RichCard::Approval {
                        request,
                        detail,
                        enrichment,
                    } => render_approval_card(request, detail, enrichment, ww),
                };
                let mut h = 0usize;
                for rl in &rich_lines {
                    h = h.saturating_add(transcript_wrap_line_count(rl.clone(), ww));
                }
                h.saturating_add(transcript_wrap_line_count(Line::raw(""), ww))
            } else {
                let icon = match msg.role {
                    MessageRole::User => "> ",
                    MessageRole::Assistant => "🤖 ",
                    MessageRole::System => "ℹ ",
                    MessageRole::Signal => "🔔 ",
                    MessageRole::SignalLow => "  ",
                    MessageRole::AgentOutput => "📝 ",
                };
                let style = message_role_style(msg.role);
                let mut h = 0usize;
                for (i, text_line) in msg.content.lines().enumerate() {
                    let prefix = if i == 0 { icon } else { "  " };
                    h = h.saturating_add(transcript_wrap_line_count(
                        Line::from(vec![
                            Span::raw(prefix),
                            Span::styled(text_line.to_string(), style),
                        ]),
                        ww,
                    ));
                }
                h.saturating_add(transcript_wrap_line_count(Line::raw(""), ww))
            };
            if content_row >= row && content_row < row + msg_height {
                for (tid, &lc_idx) in &self.task_lifecycle_msg_idx {
                    if lc_idx == msg_idx {
                        found_task_id = Some(tid.clone());
                        break;
                    }
                }
                break;
            }
            row = row.saturating_add(msg_height);
        }
        if let Some(tid) = found_task_id {
            if let Some(lc) = self.task_lifecycles.get_mut(&tid) {
                lc.expanded = !lc.expanded;
                if let Some(&idx) = self.task_lifecycle_msg_idx.get(&tid) {
                    if idx < self.messages.len() {
                        let collapsed = format_lifecycle_line(
                            &lc.agent_suffix,
                            &tid,
                            &lc.stages,
                            lc.spawn_reason.as_deref(),
                            lc.spawn_reason_full.as_deref(),
                            lc.result_summary.as_deref(),
                            lc.expanded,
                        );
                        self.messages[idx].content = collapsed;
                        return true;
                    }
                }
            }
        }
        false
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

    fn context_health_badge(&self) -> Option<&'static str> {
        match self.last_llm_context {
            Some(ref u) => {
                let pct = u
                    .input_context_pct
                    .map(f64::from)
                    .or_else(|| u.context_window_tokens.and_then(|w| {
                        if w == 0 {
                            None
                        } else {
                            Some((u.input_tokens as f64 / f64::from(w)) * 100.0)
                        }
                    }));
                match pct {
                    Some(x) if x >= 95.0 => Some("ctx:🔥"),
                    Some(x) if x >= 80.0 => Some("ctx:⚠"),
                    _ => None,
                }
            }
            None => None,
        }
    }
}

fn parse_slash_command(input: &str) -> Result<SlashCommand, String> {
    let trimmed = input.trim();
    let mut parts = trimmed.split_whitespace();
    let command = parts.next().unwrap_or_default();
    match command {
        "/help" => Ok(SlashCommand::Help),
        "/quit" | "/exit" => Ok(SlashCommand::Quit),
        "/status" => Ok(SlashCommand::Status),
        "/cancel" => Ok(SlashCommand::Cancel),
        "/session" => match parts.next() {
            None => Ok(SlashCommand::Session),
            Some("new") => {
                let rest = parts.collect::<Vec<_>>().join(" ");
                if rest.trim().is_empty() {
                    Ok(SlashCommand::SessionNew(None))
                } else {
                    Ok(SlashCommand::SessionNew(Some(rest.trim().to_string())))
                }
            }
            Some("switch") => {
                let rest = parts.collect::<Vec<_>>().join(" ");
                if rest.trim().is_empty() {
                    Err("Usage: /session switch <session-id>".to_string())
                } else {
                    Ok(SlashCommand::SessionSwitch(rest.trim().to_string()))
                }
            }
            Some(other) => {
                if other.starts_with("session-") {
                    let rest = parts.collect::<Vec<_>>().join(" ");
                    let full_id = if rest.is_empty() {
                        other.to_string()
                    } else {
                        format!("{} {}", other, rest)
                    };
                    Ok(SlashCommand::SessionSwitch(full_id.trim().to_string()))
                } else {
                    Err(format!(
                        "Unknown /session subcommand '{}'. Try /session, /session new, or /session switch <session-id>.",
                        other
                    ))
                }
            }
        },
        "/why" => {
            let rest = parts.collect::<Vec<_>>().join(" ");
            if rest.trim().is_empty() {
                Ok(SlashCommand::Why(None))
            } else {
                Ok(SlashCommand::Why(Some(rest.trim().to_string())))
            }
        },
        "/policy" => {
            let rest = parts.collect::<Vec<_>>().join(" ");
            let trimmed = rest.trim();
            if trimmed.is_empty() {
                Err("Usage: /policy <natural language policy request>".to_string())
            } else {
                Ok(SlashCommand::Policy(trimmed.to_string()))
            }
        }
        "/persona" => {
            let rest = parts.collect::<Vec<_>>().join(" ");
            if rest.trim().is_empty() {
                Ok(SlashCommand::Persona(None))
            } else {
                Ok(SlashCommand::Persona(Some(rest.trim().to_string())))
            }
        }
        "/pending" | "/approvals" => Ok(SlashCommand::Pending),
        "/wb" => {
            let sub = parts.next();
            match sub.map(|s| s.to_lowercase()).as_deref() {
                Some("reconcile") => Ok(SlashCommand::WbReconcile),
                Some("discard") => Ok(SlashCommand::WbDiscard),
                Some("diff") => Ok(SlashCommand::WbDiff),
                Some("status") | None => Ok(SlashCommand::WbStatus),
                Some(other) => Err(format!("Unknown /wb subcommand '{}'. Try: status, diff, reconcile, discard.", other)),
            }
        }
        "/return" => {
            let mut force = false;
            let mut note_tokens: Vec<String> = Vec::new();
            for tok in parts {
                if tok == "--force" || tok == "-f" {
                    force = true;
                } else {
                    note_tokens.push(tok.to_string());
                }
            }
            let message = if note_tokens.is_empty() {
                None
            } else {
                Some(note_tokens.join(" "))
            };
            Ok(SlashCommand::ReturnToAgent { force, message })
        }
        _ => Err(format!("Unknown command '{}'. Try /help.", trimmed)),
    }
}

fn format_help_card() -> String {
    [
        "Chat commands:",
        "  /session               Show known sessions and open the session picker",
        "  /session new [name]    Create and switch to a new session",
        "  /session switch <id>   Switch to an existing session",
        "  /status                Show current session details",
        "  /pending               Open pending approvals & interactions overlay (Ctrl+P)",
        "  /wb [status|diff|reconcile|discard]  Workbench actions",
        "  /return [--force] [note]   Hand the active workbench back to the orchestrator (planner.default).",
        "                            Refuses if there are unsaved edits; use --force to override (edits are dropped).",
        "  /why [request_id]      Explain why an approval was triggered (constitutional rules)",
        "  /policy <text>          Route natural language governance requests to governance-author.default",
        "  /persona [text]        Show or set session persona (user context/preferences)",
        "  /cancel                Leave the current picker/prompt",
        "  /quit                  Exit chat",
    ]
    .join("\n")
}

fn format_session_status(app: &App) -> String {
    let workflow_id = app
        .session_overview
        .workflow
        .workflow_id
        .as_deref()
        .unwrap_or("n/a");
    let root_session_id = get_root_session_id(app);
    format!(
        "Session: {}\nRoot: {}\nTarget: {}\nWorkflow: {}\nPending approvals: {}\nPending questions: {}\nResolved approvals: {}\nAnswered questions: {}\nGateway: {}",
        app.session_id,
        root_session_id,
        app.target_hint,
        workflow_id,
        app.pending_approval_ids.len(),
        app.session_overview.pending_user_interactions,
        app.gate_history_approvals.len(),
        app.gate_history_interactions.len(),
        if app.gateway_connected { "connected" } else { "disconnected" },
    )
}

fn generate_session_id() -> String {
    format!("session-{}", &uuid::Uuid::new_v4().to_string()[..8])
}

fn open_gateway_store(
    config: &autonoetic_types::config::GatewayConfig,
) -> anyhow::Result<GatewayStore> {
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(config);
    GatewayStore::open(&gateway_dir)
}

fn resolve_latest_session(config_path: &Path, config: &GatewayConfig) -> String {
    let gw_store = open_gateway_store(config).ok();
    let sessions = load_known_sessions(config_path, "", "", gw_store.as_ref());
    if let Some(entry) = sessions.into_iter().next() {
        eprintln!("Resuming session: {}", entry.session_id);
        return entry.session_id;
    }

    // Fallback: read the `.gateway/sessions/latest` symlink target
    // The symlink is maintained by SessionReportWriter and points to the
    // root session directory name (= session_id).
    let latest_link = autonoetic_gateway::execution::gateway_root_dir(config)
        .join("sessions")
        .join("latest");
    if let Ok(target) = std::fs::read_link(&latest_link) {
        let session_id = target.to_string_lossy().to_string();
        if !session_id.is_empty() {
            eprintln!("Resuming session: {}", session_id);
            return session_id;
        }
    }

    eprintln!("No previous sessions found, starting a new session.");
    generate_session_id()
}

fn load_known_sessions(
    config_path: &Path,
    current_session_id: &str,
    current_target_hint: &str,
    gateway_store: Option<&GatewayStore>,
) -> Vec<KnownSessionEntry> {
    let mut by_session: BTreeMap<String, KnownSessionEntry> = BTreeMap::new();
    if let Ok(traces) = super::trace::load_agent_traces(config_path, None) {
        for summary in super::common::collect_session_summaries(&traces) {
            let entry = by_session
                .entry(summary.session_id.clone())
                .or_insert_with(|| KnownSessionEntry {
                    session_id: summary.session_id.clone(),
                    primary_agent_id: Some(summary.agent_id.clone()),
                    agent_ids: vec![summary.agent_id.clone()],
                    first_timestamp: Some(summary.first_timestamp.clone()),
                    last_timestamp: Some(summary.last_timestamp.clone()),
                    event_count: 0,
                });
            if !entry.agent_ids.contains(&summary.agent_id) {
                entry.agent_ids.push(summary.agent_id.clone());
                entry.agent_ids.sort();
            }
            entry.event_count = entry.event_count.saturating_add(summary.event_count);
            if entry.first_timestamp.as_ref().is_none_or(|ts| summary.first_timestamp < *ts) {
                entry.first_timestamp = Some(summary.first_timestamp.clone());
            }
            if entry.last_timestamp.as_ref().is_none_or(|ts| summary.last_timestamp > *ts) {
                entry.last_timestamp = Some(summary.last_timestamp.clone());
                entry.primary_agent_id = Some(summary.agent_id.clone());
            }
        }
    }

    if let Some(store) = gateway_store {
        if let Ok(db_sessions) = store.list_recent_sessions(200) {
            for (session_id, agent_id, last_ts) in db_sessions {
                let entry = by_session
                    .entry(session_id.clone())
                    .or_insert_with(|| KnownSessionEntry {
                        session_id: session_id.clone(),
                        primary_agent_id: Some(agent_id.clone()),
                        agent_ids: vec![agent_id.clone()],
                        first_timestamp: None,
                        last_timestamp: None,
                        event_count: 0,
                    });
                if !entry.agent_ids.contains(&agent_id) {
                    entry.agent_ids.push(agent_id.clone());
                    entry.agent_ids.sort();
                }
                if entry.last_timestamp.as_ref().is_none_or(|ts| last_ts > *ts) {
                    entry.last_timestamp = Some(last_ts.clone());
                    entry.primary_agent_id = Some(agent_id.clone());
                }
                if entry.first_timestamp.as_ref().is_none_or(|ts| last_ts < *ts) {
                    entry.first_timestamp = Some(last_ts.clone());
                }
            }
        }
    }

    by_session
        .entry(current_session_id.to_string())
        .or_insert_with(|| KnownSessionEntry {
            session_id: current_session_id.to_string(),
            primary_agent_id: Some(current_target_hint.to_string()),
            agent_ids: vec![current_target_hint.to_string()],
            first_timestamp: None,
            last_timestamp: None,
            event_count: 0,
        });

    // Only root sessions are meaningful to restore — child sessions (containing `/`)
    // are transient workflow agents that cannot be resumed independently.
    by_session.retain(|sid, _| !sid.contains('/'));
    let mut sessions: Vec<KnownSessionEntry> = by_session.into_values().collect();
    sessions.sort_by(|a, b| {
        b.last_timestamp
            .cmp(&a.last_timestamp)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    sessions
}

fn format_known_sessions_card(app: &App, sessions: &[KnownSessionEntry]) -> String {
    let mut lines = vec![
        format!("Current session: {}", app.session_id),
        String::new(),
        "Known sessions:".to_string(),
    ];

    for (idx, session) in sessions.iter().enumerate() {
        let marker = if session.session_id == app.session_id {
            "*"
        } else {
            " "
        };
        let agents = if session.agent_ids.is_empty() {
            "-".to_string()
        } else {
            session.agent_ids.join(", ")
        };
        let last_seen = session.last_timestamp.as_deref().unwrap_or("new");
        lines.push(format!(
            "  {} {}. {} | agents: {} | last: {} | events: {}",
            marker,
            idx + 1,
            session.session_id,
            agents,
            last_seen,
            session.event_count
        ));
    }

    lines.push(String::new());
    lines.push("Reply with a number, an exact session id, or `new`.".to_string());
    lines.push("You can also use `/session switch <id>` or `/session new [name]`.".to_string());
    lines.push("Use `/cancel` to close the picker.".to_string());
    lines.join("\n")
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

fn add_session_banner(
    app: &mut App,
    config: &autonoetic_types::config::GatewayConfig,
    session_id: &str,
) {
    let root_session = autonoetic_gateway::runtime::content_store::root_session_id(session_id);
    let wf_hint =
        autonoetic_gateway::scheduler::resolve_workflow_id_for_root_session(config, root_session)
            .ok()
            .flatten()
            .map(|wf_id| format!(" · wf:{}", &wf_id[..8.min(wf_id.len())]))
            .unwrap_or_default();
    app.add_message(
        MessageRole::System,
        format!("{}{}", &session_id[..20.min(session_id.len())], wf_hint),
    );
}

fn refresh_gate_history(
    app: &mut App,
    store: &autonoetic_gateway::scheduler::gateway_store::GatewayStore,
) {
    let active_session_id = app.session_id.clone();
    let root = get_root_session_id(app);

    if let Ok(all_approvals) = store.list_all_approvals_for_session(&active_session_id) {
        app.gate_history_approvals = all_approvals
            .into_iter()
            .filter_map(|a| {
                let st = a.status?;
                let status_label = match st {
                    ApprovalStatus::Approved => "approved",
                    ApprovalStatus::Rejected => "rejected",
                    ApprovalStatus::Cancelled => "cancelled",
                };
                let decision = a.decision_reason.as_deref().unwrap_or(status_label);
                Some(format!(
                    "[{}] {} ({})",
                    status_label,
                    a.action.kind(),
                    truncate_str(decision, 60)
                ))
            })
            .collect();
    }
    if let Ok(all_interactions) = store.list_user_interactions_for_session_trace(&root) {
        app.gate_history_interactions = all_interactions
            .into_iter()
            .filter(|i| i.status != UserInteractionStatus::Pending)
            .map(|i| {
                let answer = i
                    .answer_text
                    .as_deref()
                    .or(i.answer_option_id.as_deref())
                    .unwrap_or("—");
                let status_label = match i.status {
                    UserInteractionStatus::Pending => "pending",
                    UserInteractionStatus::Answered => "answered",
                    UserInteractionStatus::Cancelled => "cancelled",
                    UserInteractionStatus::Expired => "expired",
                };
                let q = truncate_str(&i.question.replace('\n', " "), 80);
                let a = truncate_str(answer.replace('\n', " ").as_str(), 40);
                format!("[{}] {} → {}", status_label, q, a)
            })
            .collect();
    }
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}…", prefix)
    } else {
        prefix
    }
}

fn refresh_session_snapshot(
    app: &mut App,
    config: &autonoetic_types::config::GatewayConfig,
    gateway_store: Option<&GatewayStore>,
) {
    let active_session_id = app.session_id.clone();
    if let Some(store) = gateway_store {
        if let Ok(snapshot) = poll_session_snapshot(
            config,
            Some(store),
            &active_session_id,
            app.session_overview.latest_signal.clone(),
        ) {
            app.session_overview = snapshot.overview.clone();
            app.active_workbench = snapshot.active_workbench;
            app.pending_question_summaries = snapshot
                .pending_interactions
                .iter()
                .map(|i| {
                    if i.question.len() > 42 {
                        format!("{}…", &i.question[..42])
                    } else {
                        i.question.clone()
                    }
                })
                .collect();
            let _ = append_new_pending_user_interaction_prompts(app, &snapshot.pending_interactions);
        }
        let _ = merge_gateway_store_pending_approvals(app, config, store, &active_session_id);
        refresh_gate_history(app, store);
    }
}

fn reset_for_session_switch(
    app: &mut App,
    new_session_id: String,
    new_target_hint: Option<String>,
) {
    app.messages.clear();
    app.pending.clear();
    app.scroll_offset = 0;
    app.last_max_scroll_offset = 0;
    app.follow_output = true;
    app.session_paused = false;
    app.disarm_cancel_window();
    app.session_id = new_session_id;
    if let Some(target_hint) = new_target_hint {
        app.target_hint = target_hint;
    }
    app.selecting = false;
    app.sel_start = None;
    app.sel_end = None;
    app.signal_resume_by_internal_id.clear();
    app.signal_resume_inflight.clear();
    app.seen_workflow_event_ids.clear();
    app.bootstrapped_workflow_ids.clear();
    app.current_workflow_id = None;
    app.session_overview = SessionOverview::default();
    app.live_tasks.clear();
    app.seen_user_interaction_prompts.clear();
    app.pending_approval_ids.clear();
    app.announced_store_approval_ids.clear();
    app.post_approval_pending_ids.clear();
    app.task_lifecycles.clear();
    app.task_lifecycle_msg_idx.clear();
    app.seen_causal_policy_event_ids.clear();
    app.policy_causal_pane.clear();
    app.gate_history_approvals.clear();
    app.gate_history_interactions.clear();
    app.pending_prompt = None;
    app.active_workbench = None;
}

fn switch_session(
    app: &mut App,
    config: &autonoetic_types::config::GatewayConfig,
    gateway_store: Option<&GatewayStore>,
    pending_map: &mut HashMap<String, u64>,
    new_session_id: String,
    new_target_hint: Option<String>,
    reason: &str,
) {
    let switched_target = new_target_hint.clone().unwrap_or_else(|| app.target_hint.clone());
    reset_for_session_switch(app, new_session_id.clone(), new_target_hint);
    pending_map.clear();
    add_session_banner(app, config, &new_session_id);
    if let Ok(restored) = hydrate_session_history(app, config, &new_session_id) {
        if restored > 0 {
            app.add_message(
                MessageRole::System,
                format!("Restored {} message(s) from previous session history", restored),
            );
        }
    }
    app.add_message(
        MessageRole::System,
        format!(
            "Switched to session {} ({}, target: {}).",
            new_session_id, reason, switched_target
        ),
    );
    refresh_session_snapshot(app, config, gateway_store);
}

fn begin_session_picker(
    app: &mut App,
    config_path: &Path,
    gateway_store: Option<&GatewayStore>,
) {
    let sessions = load_known_sessions(config_path, &app.session_id, &app.target_hint, gateway_store);
    let card = format_known_sessions_card(app, &sessions);
    app.pending_prompt = Some(PendingPrompt::SessionSelection { sessions });
    app.add_message(MessageRole::System, card);
}

fn create_named_or_generated_session(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        generate_session_id()
    } else {
        trimmed.to_string()
    }
}

fn handle_prompt_submission(
    app: &mut App,
    config_path: &Path,
    config: &autonoetic_types::config::GatewayConfig,
    gateway_store: Option<&GatewayStore>,
    pending_map: &mut HashMap<String, u64>,
    submitted: &str,
) -> bool {
    let Some(prompt) = app.pending_prompt.clone() else {
        return false;
    };

    match prompt {
        PendingPrompt::SessionSelection { sessions } => {
            let trimmed = submitted.trim();
            if trimmed.eq_ignore_ascii_case("new") {
                app.pending_prompt = Some(PendingPrompt::NewSessionName);
                app.add_message(
                    MessageRole::System,
                    "Enter a new session name. Leave it blank to auto-generate one, or use /cancel."
                        .to_string(),
                );
                return true;
            }

            let selected = trimmed
                .parse::<usize>()
                .ok()
                .and_then(|index| sessions.get(index.saturating_sub(1)).cloned())
                .or_else(|| {
                    sessions
                        .iter()
                        .find(|session| session.session_id == trimmed)
                        .cloned()
                });

            if let Some(session) = selected {
                switch_session(
                    app,
                    config,
                    gateway_store,
                    pending_map,
                    session.session_id,
                    session.primary_agent_id,
                    "picker",
                );
            } else if trimmed.starts_with("session-") {
                switch_session(
                    app,
                    config,
                    gateway_store,
                    pending_map,
                    trimmed.to_string(),
                    None,
                    "picker",
                );
            } else {
                app.add_message(
                    MessageRole::System,
                    format!(
                        "Unknown session selection '{}'. Reply with a number, an exact session id, `new`, or /cancel.",
                        trimmed
                    ),
                );
                app.pending_prompt = Some(PendingPrompt::SessionSelection { sessions });
            }
            true
        }
        PendingPrompt::NewSessionName => {
            let session_id = create_named_or_generated_session(submitted);
            switch_session(
                app,
                config,
                gateway_store,
                pending_map,
                session_id,
                None,
                "new session",
            );
            let _ = config_path;
            true
        }
    }
}

fn handle_slash_command_submission(
    app: &mut App,
    config_path: &Path,
    config: &autonoetic_types::config::GatewayConfig,
    gateway_store: Option<&GatewayStore>,
    pending_map: &mut HashMap<String, u64>,
    command: SlashCommand,
) -> bool {
    match command {
        SlashCommand::Help => {
            app.add_message(MessageRole::System, format_help_card());
            true
        }
        SlashCommand::Quit => false,
        SlashCommand::Status => {
            app.add_message(MessageRole::System, format_session_status(app));
            true
        }
        SlashCommand::Session => {
            begin_session_picker(app, config_path, gateway_store);
            true
        }
        SlashCommand::SessionNew(name) => {
            if let Some(name) = name {
                switch_session(
                    app,
                    config,
                    gateway_store,
                    pending_map,
                    create_named_or_generated_session(&name),
                    None,
                    "new session",
                );
            } else {
                app.pending_prompt = Some(PendingPrompt::NewSessionName);
                app.add_message(
                    MessageRole::System,
                    "Enter a new session name. Leave it blank to auto-generate one, or use /cancel."
                        .to_string(),
                );
            }
            true
        }
        SlashCommand::SessionSwitch(session_id) => {
            let target_hint = load_known_sessions(config_path, &app.session_id, &app.target_hint, gateway_store)
                .into_iter()
                .find(|session| session.session_id == session_id)
                .and_then(|session| session.primary_agent_id);
            switch_session(
                app,
                config,
                gateway_store,
                pending_map,
                session_id,
                target_hint,
                "command",
            );
            true
        }
        SlashCommand::Cancel => {
            if app.pending_prompt.take().is_some() {
                app.add_message(MessageRole::System, "Prompt cancelled.".to_string());
            } else {
                app.add_message(
                    MessageRole::System,
                    "No active prompt. Try /session or /help.".to_string(),
                );
            }
            true
        }
        SlashCommand::Why(request_id) => {
            let explanation = format_why_explanation(gateway_store, config, &app.session_id, request_id.as_deref());
            app.add_message(MessageRole::System, explanation);
            true
        }
        SlashCommand::Persona(new_persona) => {
            let persona_path = config
                .persona_path
                .clone()
                .unwrap_or_else(|| {
                    config
                        .agents_dir
                        .parent()
                        .unwrap_or(std::path::Path::new("."))
                        .join("persona.md")
                });

            if let Some(text) = new_persona {
                match std::fs::write(&persona_path, &text) {
                    Ok(()) => {
                        app.add_message(
                            MessageRole::System,
                            format!(
                                "Persona saved to {}. It will apply to new agent sessions.",
                                persona_path.display()
                            ),
                        );
                    }
                    Err(e) => {
                        app.add_message(
                            MessageRole::System,
                            format!("Failed to write persona: {e}"),
                        );
                    }
                }
            } else {
                match std::fs::read_to_string(&persona_path) {
                    Ok(content) if !content.trim().is_empty() => {
                        app.add_message(
                            MessageRole::System,
                            format!("Current persona ({}):\n\n{}", persona_path.display(), content.trim()),
                        );
                    }
                    _ => {
                        app.add_message(
                            MessageRole::System,
                            format!(
                                "No persona set. Use `/persona <text>` to set one.\n\
                                 Or create {} with your preferred context.",
                                persona_path.display()
                            ),
                        );
                    }
                }
            }
            true
        }
        SlashCommand::Policy(_) => {
            app.add_message(
                MessageRole::System,
                "/policy is only available in interactive TUI mode.".to_string(),
            );
            true
        }
        SlashCommand::Pending => {
            if let Some(store) = gateway_store {
                let root = autonoetic_gateway::runtime::content_store::root_session_id(&app.session_id);
                let mut items: Vec<PendingItem> = Vec::new();

                if let Ok(mut approvals) = autonoetic_gateway::scheduler::pending_approval_requests_for_root(
                    config, Some(store), &root,
                ) {
                    approvals.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                    for req in approvals {
                        items.push(PendingItem::Approval(Box::new(req)));
                    }
                }

                if let Ok(interactions) = list_pending_user_interactions_for_terminal_session(store, &app.session_id) {
                    for ui in interactions {
                        items.push(PendingItem::Interaction(Box::new(ui)));
                    }
                }

                if items.is_empty() {
                    app.add_message(
                        MessageRole::System,
                        "No pending approvals or interactions.".to_string(),
                    );
                } else {
                    app.pending_overlay = Some(PendingOverlay {
                        items,
                        selected: 0,
                    });
                }
            } else {
                app.add_message(
                    MessageRole::System,
                    "Gateway store not available.".to_string(),
                );
            }
            true
        }
        SlashCommand::WbStatus => {
            match &app.active_workbench {
                Some(wb) => {
                    app.add_message(
                        MessageRole::System,
                        format!(
                            "Workbench {} ({})\n  base: {}\n  files: {}  changed: {}\n  commands: /wb reconcile | /wb discard | /wb diff",
                            wb.workbench_id, wb.status, wb.base_artifact_id,
                            wb.file_count, wb.changed_files
                        ),
                    );
                }
                None => {
                    app.add_message(
                        MessageRole::System,
                        "No active workbench.".to_string(),
                    );
                }
            }
            true
        }
        SlashCommand::WbDiff => {
            match &app.active_workbench {
                Some(wb) => {
                    let msg = format_workbench_diff(gateway_store, &wb.workbench_id);
                    app.add_message(MessageRole::System, msg);
                }
                None => {
                    app.add_message(
                        MessageRole::System,
                        "No active workbench.".to_string(),
                    );
                }
            }
            true
        }
        // `/return` is handled in the main event loop (where `tx` is in
        // scope) so it can dispatch the wake-up via the channel. This arm
        // should never be reached in practice; if it is, the user gets a
        // clear error instead of a silent no-op.
        SlashCommand::ReturnToAgent { .. } => {
            app.add_message(
                MessageRole::System,
                "/return could not be dispatched from this code path. Please retry."
                    .to_string(),
            );
            true
        }
        SlashCommand::WbReconcile | SlashCommand::WbDiscard => {
            match &app.active_workbench {
                Some(wb) => {
                    let msg = format!(
                        "To {} workbench {}, ask the agent or use the CLI:\n  autonoetic workbench {} {}",
                        if matches!(command, SlashCommand::WbReconcile) { "reconcile" } else { "discard" },
                        wb.workbench_id,
                        if matches!(command, SlashCommand::WbReconcile) { "reconcile" } else { "discard" },
                        wb.workbench_id,
                    );
                    app.add_message(MessageRole::System, msg);
                }
                None => {
                    app.add_message(
                        MessageRole::System,
                        "No active workbench.".to_string(),
                    );
                }
            }
            true
        }
    }
}

fn format_why_explanation(
    gateway_store: Option<&GatewayStore>,
    config: &GatewayConfig,
    session_id: &str,
    request_id: Option<&str>,
) -> String {
    let Some(store) = gateway_store else {
        return "Gateway store not available.".to_string();
    };

    if let Some(rid) = request_id {
        match store.get_approval(rid) {
            Ok(Some(req)) => {
                let mut lines = vec![format!("Approval: {}", req.request_id)];
                let status_str = req
                    .status
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_else(|| "pending".to_string());
                lines.push(format!("Status: {}", status_str));
                lines.push(format!("Agent: {}", req.agent_id));
                lines.push(String::new());
                lines.push("Action:".to_string());
                for ln in format_scheduled_action_detail_lines(&req.action) {
                    lines.push(ln);
                }
                if let Some(r) = req.reason.as_deref().filter(|s| !s.is_empty()) {
                    lines.push(String::new());
                    lines.push(format!("Reason: {}", r));
                }
                if let Some(ref dr) = req.decision_reason {
                    lines.push(String::new());
                    lines.push(format!("Decision reason: {}", dr));
                }
                // Look up causal events for this approval to find enforced rules
                if let Ok(events) = store.search_causal_events(Some(&req.session_id), None, 100) {
                    let gate_rules: Vec<String> = events
                        .iter()
                        .filter(|e| {
                            (e.action == "gate_suspended" || e.action == "approval_created")
                                && e.target.as_deref() == Some(rid)
                        })
                        .flat_map(|e| e.enforced_rules.iter().cloned())
                        .collect();
                    if !gate_rules.is_empty() {
                        lines.push(String::new());
                        lines.push(
                            autonoetic_gateway::constitution_glossary::format_enforced_rules(&gate_rules),
                        );
                    }
                }
                lines.join("\n")
            }
            Ok(None) => format!("No approval found with id '{}'.", rid),
            Err(e) => format!("Error looking up approval: {}", e),
        }
    } else {
        let pending = match autonoetic_gateway::scheduler::approval::pending_approval_requests_for_session(
            config,
            Some(store),
            session_id,
        ) {
            Ok(p) => p,
            Err(_) => Vec::new(),
        };
        if pending.is_empty() {
            "No pending approvals. Use /why <request_id> to inspect a specific approval.".to_string()
        } else {
            let mut lines = vec![format!("{} pending approval(s):", pending.len())];
            for req in pending.iter().take(5) {
                let line = format!("  {} — {}", req.request_id, req.action.kind());
                lines.push(line);
            }
            lines.push(String::new());
            lines.push("Use /why <request_id> for details.".to_string());
            lines.join("\n")
        }
    }
}

fn file_sha256(path: &Path) -> std::result::Result<String, std::io::Error> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn format_workbench_diff(
    gateway_store: Option<&GatewayStore>,
    workbench_id: &str,
) -> String {
    let Some(store) = gateway_store else {
        return "Gateway store not available.".to_string();
    };
    let wb = match store.load_workbench(workbench_id) {
        Ok(Some(wb)) => wb,
        _ => return format!("Workbench {} not found.", workbench_id),
    };
    let source_dir = Path::new(&wb.workspace_path);
    let meta_dir = source_dir.parent().map(|p| p.join(".autonoetic"));
    let base_digests: std::collections::HashMap<String, String> = meta_dir
        .and_then(|d| {
            let p = d.join("base_digests.json");
            if p.exists() {
                serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
            } else {
                None
            }
        })
        .unwrap_or_default();

    let mut current_names: Vec<String> = Vec::new();
    if source_dir.exists() {
        for entry in walkdir::WalkDir::new(source_dir).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let rel = entry.path().strip_prefix(source_dir).unwrap();
                let rel_str = rel.to_string_lossy().to_string();
                if rel_str.starts_with(".autonoetic/") {
                    continue;
                }
                current_names.push(rel_str);
            }
        }
    }
    let current_set: std::collections::HashSet<&str> =
        current_names.iter().map(|s| s.as_str()).collect();

    let mut added: Vec<&str> = Vec::new();
    let mut deleted: Vec<&str> = Vec::new();
    let mut modified: Vec<&str> = Vec::new();
    let mut unchanged = 0usize;

    for name in &current_names {
        match base_digests.get(name.as_str()) {
            Some(base_digest) => {
                match file_sha256(&source_dir.join(name)) {
                    Ok(current_digest) if current_digest == *base_digest => unchanged += 1,
                    _ => modified.push(name.as_str()),
                }
            }
            None => added.push(name.as_str()),
        }
    }
    for name in base_digests.keys() {
        if !current_set.contains(name.as_str()) {
            deleted.push(name.as_str());
        }
    }

    let mut lines = Vec::new();
    lines.push(format!("Workbench {} diff:", workbench_id));
    if added.is_empty() && deleted.is_empty() && modified.is_empty() {
        lines.push("  No changes.".to_string());
    } else {
        for f in &added { lines.push(format!("  + {}", f)); }
        for f in &modified { lines.push(format!("  ~ {}", f)); }
        for f in &deleted { lines.push(format!("  - {}", f)); }
        if unchanged > 0 {
            lines.push(format!("  ({} unchanged)", unchanged));
        }
    }
    lines.join("\n")
}

/// Inputs for `build_return_to_agent_wakeup`. Kept as a plain struct so the
/// builder stays unit-testable without constructing a full workbench record.
/// Owns all of its data (no borrowed references) so the caller can free
/// scratch workbench lookups after constructing the input.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReturnToAgentInput {
    workbench_id: String,
    base_artifact_id: String,
    /// True when the workbench has been reconciled (i.e. the operator already
    /// committed the edits into a new artifact revision). False when the
    /// workbench is still active and the wake-up is being sent without a
    /// prior reconcile (operator chose --force).
    reconciled: bool,
    /// Optional new artifact ref/id from the most recent reconcile.
    new_artifact_ref: Option<String>,
    new_artifact_id: Option<String>,
    /// Optional operator note typed alongside `/return ...`.
    operator_note: Option<String>,
    /// Number of files that differ from the base artifact (modified+added+deleted).
    /// 0 when the workbench is in sync with the base (or already reconciled).
    unsaved_change_count: usize,
    /// IDs of files modified by the operator since the projection. May be
    /// empty when the workbench is reconciled or unsaved_change_count is 0.
    operator_modified_files: Vec<String>,
    /// IDs of files added by the operator since the projection.
    operator_added_files: Vec<String>,
    /// IDs of files deleted by the operator since the projection.
    deleted_files: Vec<String>,
}

/// Output of `build_return_to_agent_wakeup`. The `message` is the natural
/// language text the orchestrator will read; `metadata` is the structured
/// `workbench_reconciled` payload attached to the event.ingest call for
/// downstream tooling and the agent's own state updates.
#[derive(Debug, Clone, PartialEq)]
struct ReturnToAgentWakeup {
    message: String,
    metadata: serde_json::Value,
}

fn build_return_to_agent_wakeup(input: &ReturnToAgentInput) -> ReturnToAgentWakeup {
    let mut structured = serde_json::json!({
        "event": "workbench_reconciled",
        "workbench_id": input.workbench_id,
        "base_artifact_id": input.base_artifact_id,
        "operator_modified": !input.operator_modified_files.is_empty()
            || !input.operator_added_files.is_empty()
            || !input.deleted_files.is_empty(),
    });

    if let Some(new_artifact_ref) = &input.new_artifact_ref {
        structured["new_artifact_ref"] = serde_json::Value::String(new_artifact_ref.clone());
    }
    if let Some(new_artifact_id) = &input.new_artifact_id {
        structured["new_artifact_id"] = serde_json::Value::String(new_artifact_id.clone());
    }
    if !input.operator_modified_files.is_empty() {
        structured["operator_modified_files"] = serde_json::Value::Array(
            input
                .operator_modified_files
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
    }
    if !input.operator_added_files.is_empty() {
        structured["operator_added_files"] = serde_json::Value::Array(
            input
                .operator_added_files
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
    }
    if !input.deleted_files.is_empty() {
        structured["deleted_files"] = serde_json::Value::Array(
            input
                .deleted_files
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
    }

    let artifact_ref_label = input
        .new_artifact_ref
        .as_ref()
        .map(|r| format!("`{}`", r))
        .unwrap_or_else(|| format!("base `{}`", input.base_artifact_id));

    let mut message = String::new();
    message.push_str(&format!(
        "Operator returned workbench `{}` to you. Active artifact: {}.",
        input.workbench_id, artifact_ref_label
    ));
    if input.reconciled {
        message.push_str(" Status: reconciled.");
    } else if input.unsaved_change_count > 0 {
        message.push_str(&format!(
            " Status: active with {} unsaved change(s) (sent with --force; edits were not committed).",
            input.unsaved_change_count
        ));
    } else {
        message.push_str(" Status: active, in sync with base artifact (no edits).");
    }
    if let Some(note) = &input.operator_note {
        if !note.trim().is_empty() {
            message.push_str(&format!(" Operator note: {}.", note.trim()));
        }
    }
    message.push_str(" Please continue the workflow.");

    ReturnToAgentWakeup {
        message,
        metadata: serde_json::json!({
            "workbench_reconciled": structured,
        }),
    }
}

/// Read the active workbench's operator-edited file lists from the gateway
/// store. Returns `None` when the workbench is not found, the store is
/// missing, or the workspace dir is gone. The `unsaved_change_count` in the
/// returned `ReturnToAgentInput` reflects modified+added+deleted files.
fn read_return_to_agent_input(
    gateway_store: Option<&GatewayStore>,
    workbench_id: &str,
    operator_note: Option<&str>,
) -> Option<ReturnToAgentInput> {
    let store = gateway_store?;
    let wb = store.load_workbench(workbench_id).ok().flatten()?;

    let reconciled = matches!(wb.status, autonoetic_types::workbench::WorkbenchStatus::Reconciled);

    let source_dir = Path::new(&wb.workspace_path);
    if !source_dir.exists() {
        return Some(ReturnToAgentInput {
            workbench_id: wb.workbench_id,
            base_artifact_id: wb.base_artifact_id,
            reconciled,
            new_artifact_ref: None,
            new_artifact_id: None,
            operator_note: operator_note.map(|s| s.to_string()),
            unsaved_change_count: 0,
            operator_modified_files: Vec::new(),
            operator_added_files: Vec::new(),
            deleted_files: Vec::new(),
        });
    }

    let meta_dir = source_dir.parent().map(|p| p.join(".autonoetic"));
    let base_digests: std::collections::HashMap<String, String> = meta_dir
        .and_then(|d| {
            let p = d.join("base_digests.json");
            if p.exists() {
                serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
            } else {
                None
            }
        })
        .unwrap_or_default();

    let mut current_names: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(source_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(source_dir).unwrap();
            let rel_str = rel.to_string_lossy().to_string();
            if rel_str.starts_with(".autonoetic/") {
                continue;
            }
            current_names.push(rel_str);
        }
    }
    let current_set: std::collections::HashSet<&str> =
        current_names.iter().map(|s| s.as_str()).collect();

    let mut modified: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    for name in &current_names {
        match base_digests.get(name.as_str()) {
            Some(base_digest) => {
                match file_sha256(&source_dir.join(name)) {
                    Ok(current_digest) if current_digest == *base_digest => {}
                    _ => modified.push(name.clone()),
                }
            }
            None => added.push(name.clone()),
        }
    }
    for name in base_digests.keys() {
        if !current_set.contains(name.as_str()) {
            deleted.push(name.clone());
        }
    }

    let unsaved_change_count = modified.len() + added.len() + deleted.len();

    Some(ReturnToAgentInput {
        workbench_id: wb.workbench_id,
        base_artifact_id: wb.base_artifact_id,
        reconciled,
        new_artifact_ref: None,
        new_artifact_id: None,
        operator_note: operator_note.map(|s| s.to_string()),
        unsaved_change_count,
        operator_modified_files: modified,
        operator_added_files: added,
        deleted_files: deleted,
    })
}

/// Outcome of `prepare_return_to_agent_wakeup`. Drives the TUI's response
/// to a `/return` slash command: either render an inline error and stop, or
/// dispatch the wake-up to the orchestrator.
#[derive(Debug)]
enum ReturnToAgentStatus {
    /// No active workbench — nothing to return. TUI shows a friendly message.
    NoWorkbench,
    /// Workbench has unsaved edits and `--force` was not supplied. TUI
    /// shows the refusal (with the list of edited files) and stops.
    Refused { reason: String },
    /// Wake-up is built and ready to send. TUI dispatches via the channel.
    Ready {
        target_agent_id: String,
        outbound_message: String,
        metadata: serde_json::Value,
    },
}

/// Prepare the wake-up that `/return` will dispatch. Pulls the active
/// workbench from the gateway store, applies the unsaved-edits safety
/// check, and produces the structured payload for `event.ingest`. The
/// second tuple element is unused for control flow but kept so callers
/// can render a one-line status without re-running the builder.
fn prepare_return_to_agent_wakeup(
    gateway_store: Option<&GatewayStore>,
    active_workbench: Option<&WorkbenchOverview>,
    force: bool,
    operator_note: Option<&str>,
) -> (ReturnToAgentStatus, String) {
    let Some(wb_overview) = active_workbench else {
        return (ReturnToAgentStatus::NoWorkbench, String::new());
    };

    let Some(input) = read_return_to_agent_input(
        gateway_store,
        &wb_overview.workbench_id,
        operator_note,
    ) else {
        return (
            ReturnToAgentStatus::Refused {
                reason: format!(
                    "Workbench {} is no longer in the gateway store. Cannot return.",
                    wb_overview.workbench_id
                ),
            },
            String::new(),
        );
    };

    if !force && input.unsaved_change_count > 0 {
        let mut lines = Vec::new();
        lines.push(format!(
            "Workbench {} has {} unsaved edit(s). Refusing to silently drop them.",
            input.workbench_id, input.unsaved_change_count
        ));
        if !input.operator_modified_files.is_empty() {
            lines.push("  Modified:".to_string());
            for f in &input.operator_modified_files {
                lines.push(format!("    ~ {}", f));
            }
        }
        if !input.operator_added_files.is_empty() {
            lines.push("  Added:".to_string());
            for f in &input.operator_added_files {
                lines.push(format!("    + {}", f));
            }
        }
        if !input.deleted_files.is_empty() {
            lines.push("  Deleted:".to_string());
            for f in &input.deleted_files {
                lines.push(format!("    - {}", f));
            }
        }
        lines.push(
            "Reconcile them first (autonoetic workbench reconcile <wb>) or re-run with --force to drop the edits and return the base artifact.".to_string(),
        );
        return (
            ReturnToAgentStatus::Refused {
                reason: lines.join("\n"),
            },
            String::new(),
        );
    }

    let target_agent_id = "planner.default".to_string();
    let wakeup = build_return_to_agent_wakeup(&input);
    let body = wakeup.message.clone();

    (
        ReturnToAgentStatus::Ready {
            target_agent_id,
            outbound_message: wakeup.message,
            metadata: wakeup.metadata,
        },
        body,
    )
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

    let mut workflow_captured = 0usize;

    let workflow = if let Some(workflow_id) = workflow_id {
        let status = autonoetic_gateway::scheduler::load_workflow_run(config, None, &workflow_id)
            .ok()
            .flatten()
            .map(|run| run.status.as_str().to_string())
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

        workflow_captured = running + queued + awaiting;

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

    // Count active executions not already represented in workflow task tracking.
    // Catches orphaned or non-workflow child sessions that would otherwise be
    // invisible to the working indicator.
    let active_executions = store
        .and_then(|s| s.list_active_executions_for_root_sqlite(root_session_id).ok())
        .map(|execs| {
            let active = execs
                .iter()
                .filter(|e| e.status == "running" || e.status == "stop_requested")
                .count();
            active.saturating_sub(workflow_captured)
        })
        .unwrap_or(0);

    let active_sessions = store
        .and_then(|s| s.list_active_session_turn_counts(&root_session_id).ok())
        .unwrap_or_default();

    let active_workbench = store.and_then(|s| {
        if let Some(ref wf_id) = workflow.workflow_id {
            s.load_active_workbench_for_workflow(wf_id).ok().flatten()
        } else {
            None
        }
    }).map(|wb| {
        let source_dir = std::path::Path::new(&wb.workspace_path);
        let meta_dir = source_dir.parent().map(|p| p.join(".autonoetic"));
        let base_digests: std::collections::HashMap<String, String> = meta_dir
            .and_then(|d| {
                let p = d.join("base_digests.json");
                if p.exists() {
                    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
                } else {
                    None
                }
            })
            .unwrap_or_default();
        let current_files: Vec<String> = if source_dir.exists() {
            let mut files = Vec::new();
            for entry in walkdir::WalkDir::new(source_dir).into_iter().filter_map(|e| e.ok()) {
                if entry.file_type().is_file() {
                    let rel = entry.path().strip_prefix(source_dir).unwrap();
                    let rel_str = rel.to_string_lossy().to_string();
                    if rel_str.starts_with(".autonoetic/") {
                        continue;
                    }
                    files.push(rel_str);
                }
            }
            files
        } else {
            Vec::new()
        };
        let current_set: std::collections::HashSet<&str> =
            current_files.iter().map(|s| s.as_str()).collect();
        let mut changed = 0usize;
        for name in &current_files {
            match base_digests.get(name.as_str()) {
                Some(base_digest) => {
                    if file_sha256(&source_dir.join(name)).map_or(true, |d| d != *base_digest) {
                        changed += 1;
                    }
                }
                None => changed += 1,
            }
        }
        for name in base_digests.keys() {
            if !current_set.contains(name.as_str()) {
                changed += 1;
            }
        }
        WorkbenchOverview {
            workbench_id: wb.workbench_id.clone(),
            status: wb.status.as_str().to_string(),
            base_artifact_id: wb.base_artifact_id.clone(),
            file_count: current_files.len(),
            changed_files: changed,
        }
    });

    Ok(SessionPollSnapshot {
        overview: SessionOverview {
            root_session_id: root_session_id.to_string(),
            workflow,
            pending_user_interactions: pending_interactions.len(),
            active_executions,
            latest_signal: previous_latest_signal,
            active_sessions,
        },
        pending_interactions,
        active_workbench,
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

fn rich_box_top(header: &str, width: u16) -> Line<'static> {
    let inner = width as usize;
    let header_with_pad = if header.is_empty() {
        String::new()
    } else {
        format!(" {} ", header)
    };
    let header_len = UnicodeWidthStr::width(header_with_pad.as_str());
    let dash_count = inner.saturating_sub(4 + header_len).max(0);
    let line_str = format!("╭─{}{}─╮", header_with_pad, "─".repeat(dash_count));
    Line::from(Span::styled(line_str, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)))
}

fn rich_box_mid(text: &str, width: u16) -> Line<'static> {
    let inner = width as usize;
    let padded = format!("│ {}", text);
    let pad = inner.saturating_sub(UnicodeWidthStr::width(padded.as_str()) + 1).max(0);
    let line_str = format!("{}{}│", padded, " ".repeat(pad));
    Line::from(Span::raw(line_str))
}

fn rich_box_mid_styled(text: &str, width: u16, style: Style) -> Line<'static> {
    let inner = width as usize;
    let padded = format!("│ {}", text);
    let pad = inner.saturating_sub(UnicodeWidthStr::width(padded.as_str()) + 1).max(0);
    let line_str = format!("{}{}│", padded, " ".repeat(pad));
    Line::from(Span::styled(line_str, style))
}

fn rich_box_separator(label: &str, width: u16) -> Line<'static> {
    let inner = width as usize;
    let label_with_pad = if label.is_empty() {
        String::new()
    } else {
        format!(" {} ", label)
    };
    let label_len = UnicodeWidthStr::width(label_with_pad.as_str());
    let dash_count = inner.saturating_sub(4 + label_len).max(0);
    let line_str = format!("│{}{}{}│", "┈".repeat(2), label_with_pad, "┈".repeat(dash_count));
    Line::from(Span::styled(line_str, Style::default().fg(Color::DarkGray)))
}

fn rich_box_empty(width: u16) -> Line<'static> {
    rich_box_mid("", width)
}

fn rich_box_bottom(width: u16) -> Line<'static> {
    let inner = width as usize;
    let dash_count = inner.saturating_sub(2).max(0);
    let line_str = format!("╰{}╯", "─".repeat(dash_count));
    Line::from(Span::styled(line_str, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)))
}

fn word_wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return text.lines().map(|l| l.to_string()).collect();
    }
    let mut result = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            result.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0usize;
        for word in line.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            let sep = if current.is_empty() { "" } else { " " };
            let sep_width = if current.is_empty() { 0 } else { 1 };
            if current_width + sep_width + word_width > max_width && !current.is_empty() {
                result.push(std::mem::take(&mut current));
                current = word.to_string();
                current_width = word_width;
            } else {
                current.push_str(sep);
                current.push_str(word);
                current_width += sep_width + word_width;
            }
        }
        if !current.is_empty() {
            result.push(current);
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

fn render_interaction_card(interaction: &UserInteraction, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let inner_width = width.saturating_sub(2).max(10) as usize;
    let text_width = inner_width.saturating_sub(4).max(8);

    let header = format!("🔔 {} ─ {}", interaction.kind, interaction.interaction_id);
    lines.push(rich_box_top(&header, width));
    lines.push(rich_box_empty(width));

    let question_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    for wl in word_wrap_text(&interaction.question, text_width) {
        lines.push(rich_box_mid_styled(&wl, width, question_style));
    }

    if let Some(ctx) = &interaction.context {
        if !ctx.trim().is_empty() {
            lines.push(rich_box_empty(width));
            lines.push(rich_box_separator("context", width));
            let ctx_style = Style::default().fg(Color::DarkGray);
            for cl in ctx.lines() {
                for wl in word_wrap_text(cl, text_width) {
                    lines.push(rich_box_mid_styled(&wl, width, ctx_style));
                }
            }
        }
    }

    if !interaction.options.is_empty() {
        lines.push(rich_box_empty(width));
        lines.push(rich_box_separator("options", width));
        for (n, o) in interaction.options.iter().enumerate() {
            let opt_text = format!("{}. {} → {}", n + 1, o.label, o.value);
            for wl in word_wrap_text(&opt_text, text_width) {
                lines.push(rich_box_mid_styled(&wl, width, Style::default().fg(Color::White)));
            }
        }
    }

    lines.push(rich_box_empty(width));
    let hint_style = Style::default().fg(Color::Green);
    lines.push(rich_box_mid_styled("💬 Type your answer below", width, hint_style));
    lines.push(rich_box_empty(width));
    lines.push(rich_box_bottom(width));

    lines
}

fn render_approval_card(
    req: &ApprovalRequest,
    detail: &str,
    enrichment: &[autonoetic_gateway::runtime::human_gate::GateMessage],
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let inner_width = width.saturating_sub(2).max(10) as usize;
    let text_width = inner_width.saturating_sub(4).max(8);

    let action_label = action_summary(&req.action);
    let header = format!("⏸ Approval Required ─ {} ─ {}", action_label, req.request_id);
    lines.push(rich_box_top(&header, width));
    lines.push(rich_box_empty(width));

    let action_lines = format_scheduled_action_detail_lines(&req.action);
    let action_style = Style::default().fg(Color::White);
    for al in &action_lines {
        for wl in word_wrap_text(al, text_width) {
            lines.push(rich_box_mid_styled(&wl, width, action_style));
        }
    }

    if let Some(r) = req.reason.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        lines.push(rich_box_empty(width));
        lines.push(rich_box_separator("reason", width));
        let reason_style = Style::default().fg(Color::DarkGray);
        for rl in r.lines() {
            for wl in word_wrap_text(&clamp_chat_field(rl), text_width) {
                lines.push(rich_box_mid_styled(&wl, width, reason_style));
            }
        }
    }

    if let Some(ev) = req.evidence_ref.as_deref().filter(|s| !s.is_empty()) {
        lines.push(rich_box_empty(width));
        let ev_text = format!("Evidence ref: {}", clamp_chat_field(ev));
        for wl in word_wrap_text(&ev_text, text_width) {
            lines.push(rich_box_mid_styled(&wl, width, Style::default().fg(Color::DarkGray)));
        }
    }

    if !enrichment.is_empty() {
        lines.push(rich_box_empty(width));
        lines.push(rich_box_separator("context", width));
        let ctx_style = Style::default().fg(Color::DarkGray);
        for msg in enrichment {
            for (i, ln) in msg.content.lines().enumerate() {
                let text = if i == 0 {
                    format!("[{}] {}", msg.sender, clamp_chat_field(ln))
                } else {
                    clamp_chat_field(ln)
                };
                for wl in word_wrap_text(&text, text_width) {
                    lines.push(rich_box_mid_styled(&wl, width, ctx_style));
                }
            }
        }
    }

    if let Some(ref risk) = req.risk_summary {
        let mut risk_parts: Vec<String> = Vec::new();
        if risk.host_count > 0 {
            risk_parts.push(format!("{} host(s)", risk.host_count));
        }
        if !risk.dangerous_patterns.is_empty() {
            risk_parts.push(format!("{} risk(s)", risk.dangerous_patterns.len()));
        }
        if let Some(ref v) = risk.auditor_verdict {
            risk_parts.push(format!("auditor: {}", v));
        }
        if !risk_parts.is_empty() {
            lines.push(rich_box_empty(width));
            let risk_text = format!("Risk: {}", risk_parts.join(" | "));
            for wl in word_wrap_text(&risk_text, text_width) {
                lines.push(rich_box_mid_styled(&wl, width, Style::default().fg(Color::Yellow)));
            }
        }
    }

    lines.push(rich_box_empty(width));
    if let Some(ref phrase) = req.confirm_phrase {
        let phrase_style = Style::default().fg(Color::Yellow);
        for wl in word_wrap_text(&format!("Confirm phrase: '{}'", phrase), text_width) {
            lines.push(rich_box_mid_styled(&wl, width, phrase_style));
        }
        lines.push(rich_box_empty(width));
    }
    let hint_style = Style::default().fg(Color::Green);
    for dl in detail.lines() {
        for wl in word_wrap_text(dl, text_width) {
            lines.push(rich_box_mid_styled(&wl, width, hint_style));
        }
    }
    lines.push(rich_box_empty(width));
    lines.push(rich_box_bottom(width));

    lines
}

/// Structured display data extracted from an agent's JSON assistant_reply.
struct AssistantReplyDisplay {
    display: String,
    intent: Option<String>,
    goal_status: Option<String>,
}

/// Parse an assistant_reply JSON string and extract summary, result, intent, goal_status.
/// Try to extract a JSON object from a reply that has prose + JSON code fence.
fn extract_fenced_json(text: &str) -> Option<serde_json::Value> {
    // Find the first `{` that starts a JSON object. Bracket-match to find the end.
    let brace_start = text.find('{')?;
    let json_text = &text[brace_start..];
    let mut depth = 0;
    let mut end_pos = 0;
    for (i, ch) in json_text.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end_pos = i + '}'.len_utf8();
                    break;
                }
            }
            _ => {}
        }
    }
    if end_pos == 0 {
        return None;
    }
    let json_str = &json_text[..end_pos];
    serde_json::from_str(json_str).ok()
}

/// Strip leading prose and trailing text/code-fences from a reply, keeping just the prose.
fn strip_prose_around_json(text: &str) -> String {
    let brace_start = text.find('{').unwrap_or(text.len());
    let prefix = text[..brace_start].trim();
    let json_end = text[brace_start..]
        .find('}')
        .map(|i| brace_start + i + '}'.len_utf8())
        .unwrap_or(text.len());
    let suffix = text[json_end..].trim();
    // Remove trailing code fence markers
    let suffix = suffix.trim_start_matches('`').trim();
    if prefix.is_empty() {
        suffix.to_string()
    } else if suffix.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}\n{suffix}")
    }
}

fn format_assistant_reply(reply: &str) -> AssistantReplyDisplay {
    let parsed = serde_json::from_str::<serde_json::Value>(reply).ok();

    // If the full reply isn't JSON, try to extract a JSON block from markdown fences.
    let is_fenced = parsed.is_none();
    let fenced_json = if is_fenced { extract_fenced_json(reply) } else { None };
    let source = parsed.or_else(|| fenced_json);

    let summary = source
        .as_ref()
        .and_then(|v| v.get("summary").and_then(|s| s.as_str()))
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_owned());
    let result_str = source
        .as_ref()
        .and_then(|v| v.get("result").map(|r| format_json_value_as_text(r)));

    // When the full reply wasn't JSON but we extracted a fenced block,
    // show the prose text (excluding the JSON block) as the display
    // and extract summary/result from the fenced JSON.
    let display = match (summary.as_deref(), result_str.as_deref()) {
        (Some(s), Some(r)) if is_fenced => {
            let prose = strip_prose_around_json(reply);
            if prose.is_empty() {
                format!("{}\n\n{}", s, r)
            } else {
                format!("{}\n\n{}\n\n{}", prose, s, r)
            }
        }
        (Some(s), Some(r)) => format!("{}\n\n{}", s, r),
        (Some(s), None) => s.to_string(),
        (None, Some(r)) => r.to_string(),
        (None, None) => source
            .as_ref()
            .filter(|v| v.is_object() || v.is_array())
            .map(|_v| strip_prose_around_json(reply))
            .unwrap_or_else(|| strip_prose_around_json(reply)),
    };
    let intent = source
        .as_ref()
        .and_then(|v| v.get("intent").and_then(|s| s.as_str()))
        .map(|s| s.to_owned());
    let goal_status = source
        .as_ref()
        .and_then(|v| v.get("goal_status").and_then(|s| s.as_str()))
        .map(|s| s.to_owned());
    AssistantReplyDisplay {
        display,
        intent,
        goal_status,
    }
}

/// Add structured intent, goal_status, and artifact_count SignalLow messages.
fn display_assistant_metadata(
    app: &mut App,
    formatted: &AssistantReplyDisplay,
    artifact_count: Option<usize>,
) {
    if let Some(intent) = &formatted.intent {
        app.add_message(
            MessageRole::SignalLow,
            format!("🎯 Intent: {}", intent),
        );
    }
    if let Some(gs) = &formatted.goal_status {
        app.add_message(
            MessageRole::SignalLow,
            format!("📊 Goal: {}", gs),
        );
    }
    if let Some(n) = artifact_count.filter(|c| *c > 0) {
        app.add_message(
            MessageRole::SignalLow,
            format!("📦 {} artifact(s) produced", n),
        );
    }
}

/// Strip `response validation failed:` prefix and session noise for cleaner TUI messages.
fn clean_validation_error(e: &str) -> String {
    let main = e
        .strip_prefix("response validation failed: ")
        .unwrap_or(e);
    let main = main.split(". Session:").next().unwrap_or(main);
    format!("Schema validation: {}", main)
}

/// Resolve the root session ID, falling back to computation from the active session.
fn get_root_session_id(app: &App) -> String {
    if app.session_overview.root_session_id.is_empty() {
        autonoetic_gateway::runtime::content_store::root_session_id(&app.session_id).to_string()
    } else {
        app.session_overview.root_session_id.clone()
    }
}

/// Truncate and flatten a spawn_reason string for inline display.
fn preview_spawn_reason(reason: &str, max_chars: usize) -> String {
    let preview: String = reason.chars().take(max_chars).collect();
    preview.replace('\n', " ")
}

/// Recursively format a JSON value as readable text for chat display.
fn format_json_value_as_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .map(|item| format!("- {}", format_json_value_as_text(item)))
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, val)| {
                let label = k.replace('_', " ").replace('-', " ");
                match val {
                    serde_json::Value::String(s) => format!("**{}:** {}", label, s),
                    serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                        let inner = format_json_value_as_text(val);
                        if inner.contains('\n') {
                            format!("**{}:**\n{}", label, inner)
                        } else {
                            format!("**{}:** {}", label, inner)
                        }
                    }
                    other => format!("**{}:** {}", label, other),
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
    }
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
        let fallback = format_user_interaction_prompt(&interaction);
        app.session_overview.latest_signal =
            Some(format!("user.ask {}", interaction.interaction_id));
        app.add_rich_card(
            MessageRole::Signal,
            fallback,
            RichCard::UserInteraction(Box::new(interaction.clone())),
        );
        added += 1;
    }
    added
}

fn signal_resume_key(signal_session_id: &str, request_id: &str) -> String {
    format!("{}::{}", signal_session_id, request_id)
}

/// Dedup with [`merge_gateway_store_pending_approvals`]: both emit a transcript card for the same
/// `apr-*`. Returns false when this `approval_request_id` was already announced (rich store card
/// wins — `check_signals` merges pending approvals before processing workflow events).
fn should_show_workflow_awaiting_approval_card(
    app: &mut App,
    event: &autonoetic_types::workflow::WorkflowEventRecord,
) -> bool {
    if event.event_type != "task.awaiting_approval" {
        return true;
    }
    let Some(apr_id) = event
        .payload
        .get("approval_request_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return true;
    };
    app.announced_store_approval_ids.insert(apr_id.to_string())
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
            let reason = event
                .payload
                .get("spawn_reason")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let mut line = format!("🚀 [{}] Spawned: {} → {}", ts_short, task, target);
            if let Some(r) = reason {
                let oneline = preview_spawn_reason(r, 120);
                line.push_str(&format!("\n   {}", oneline));
            }
            Some((line, MessageRole::Signal))
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
            let pres =
                autonoetic_types::task_completion::TaskCompletionPresentation::from_event_payload(
                    &event.payload,
                    true,
                );
            let icon = terminal_icon_for_completion(&pres);
            let gate_note = pres.detail_suffix().unwrap_or("");
            if event.workflow_id.starts_with("sched-") && !result_summary.is_empty() {
                Some((
                    format!("🔔 [{}] {}: {}", ts_short, agent_id, result_summary),
                    MessageRole::AgentOutput,
                ))
            } else if !result_summary.is_empty() {
                let preview = clamp_chat_field(result_summary);
                Some((
                    format!(
                        "{} [{}] Task completed: {}{}{}\n   Result: {}",
                        icon,
                        ts_short,
                        task,
                        agent_suffix,
                        gate_note,
                        preview
                    ),
                    MessageRole::Signal,
                ))
            } else {
                Some((
                    format!(
                        "{} [{}] Task completed: {}{}{}",
                        icon, ts_short, task, agent_suffix, gate_note
                    ),
                    MessageRole::Signal,
                ))
            }
        }
        "task.failed" => {
            let detail = event
                .payload
                .get("result_summary")
                .and_then(|v| v.as_str())
                .or_else(|| event.payload.get("reason").and_then(|v| v.as_str()))
                .filter(|s| !s.is_empty());
            let mut line = format!("❌ [{}] Task failed: {}{}", ts_short, task, agent_suffix);
                if let Some(d) = detail {
                    let stripped = d.strip_prefix("event.ingest failed: ").unwrap_or(d);
                    let clean = if stripped.starts_with("response validation failed") {
                        clean_validation_error(stripped)
                    } else {
                        stripped.to_string()
                    };
                    line.push_str(&format!("\n   {}", clamp_chat_field(&clean)));
            }
            Some((line, MessageRole::Signal))
        }
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
        "workflow.completed" => Some((
            format!("✅ [{}] Workflow completed — all tasks done", ts_short),
            MessageRole::Signal,
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
        "scheduled_job.cancelled" => {
            let job_id = event
                .payload
                .get("job_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let reason = event
                .payload
                .get("cancel_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let cron = event
                .payload
                .get("cron_expr")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut line = format!(
                "🚫 [{}] Scheduled job cancelled: {} (target {})",
                ts_short, job_id, agent_id
            );
            if !cron.is_empty() {
                line.push_str(&format!(" [{}]", cron));
            }
            if !reason.is_empty() {
                line.push_str(&format!(" — {}", reason));
            }
            Some((line, MessageRole::Signal))
        }
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

fn is_collapsible_lifecycle_event(event_type: &str) -> Option<&'static str> {
    match event_type {
        "task.spawned" => Some("spawned"),
        "task.queued" => Some("queued"),
        "task.started" => Some("started"),
        "task.completed" => Some("completed"),
        "task.failed" => Some("failed"),
        "task.cancelled" => Some("cancelled"),
        _ => None,
    }
}

/// Lifecycle stage label; `task.completed` may include gate outcome from payload.
fn lifecycle_stage_label(event_type: &str, payload: &serde_json::Value) -> String {
    match is_collapsible_lifecycle_event(event_type) {
        Some(_) if event_type == "task.completed" => {
            autonoetic_types::task_completion::TaskCompletionPresentation::from_event_payload(
                payload,
                true,
            )
            .lifecycle_stage()
        }
        Some(stage) => stage.to_string(),
        None => event_type.to_string(),
    }
}

/// Terminal TUI icon for a task completion (adapter-specific; not in shared types).
fn terminal_icon_for_completion(
    pres: &autonoetic_types::task_completion::TaskCompletionPresentation,
) -> &'static str {
    use autonoetic_types::task_completion::CompletionSeverity;
    match pres.severity {
        CompletionSeverity::Success => "✅",
        CompletionSeverity::Caveat => "⚠️",
        CompletionSeverity::Failure => "❌",
    }
}

fn format_lifecycle_line(
    agent_suffix: &str,
    task: &str,
    stages: &[String],
    spawn_reason: Option<&str>,
    spawn_reason_full: Option<&str>,
    result_summary: Option<&str>,
    expanded: bool,
) -> String {
    let icon = lifecycle_icon_for_stage(stages.last().map(String::as_str));
    let chain = stages.join(" → ");
    let mut line = format!("{} {} ({}) {}", icon, agent_suffix.trim_start_matches(" → "), task, chain);
    let is_terminal = stages
        .last()
        .map_or(false, |s| matches!(s.as_str(), "completed" | "failed" | "cancelled") || s.contains("(gate:"));
    let full_reason = spawn_reason_full.unwrap_or(spawn_reason.unwrap_or(""));
    if expanded {
        if !full_reason.is_empty() {
            for ln in full_reason.lines() {
                line.push_str(&format!("\n   💬 {}", ln));
            }
        }
        if let Some(result) = result_summary {
            line.push_str("\n   📋 Result:");
            for ln in result.lines() {
                line.push_str(&format!("\n      {}", ln));
            }
        }
    } else {
        if let Some(reason) = spawn_reason {
            let oneline = preview_spawn_reason(reason, 100);
            line.push_str(&format!("\n   💬 {}", oneline));
        }
        if is_terminal {
            if let Some(result) = result_summary {
                let preview = preview_spawn_reason(result, 120);
                line.push_str(&format!("\n   📋 {}", preview));
            }
        }
        let has_full_content = spawn_reason_full.is_some()
            || result_summary.map_or(false, |r| r.lines().count() > 1);
        if has_full_content {
            line.push_str("\n   [click to expand]");
        }
    }
    line
}

fn lifecycle_icon_for_stage(stage: Option<&str>) -> &'static str {
    match stage {
        Some(s) if s.contains("(gate:") => "⚠️",
        Some("completed") => "✅",
        Some("failed") => "❌",
        Some("cancelled") => "🚫",
        Some("started") => "▶",
        Some("queued") => "📥",
        Some("spawned") => "🚀",
        _ => "📋",
    }
}

fn push_workflow_event_message(
    app: &mut App,
    role: MessageRole,
    card: String,
    event_type: &str,
    task_id: &str,
    agent_suffix: &str,
    payload: &serde_json::Value,
) {
    if is_collapsible_lifecycle_event(event_type).is_some() {
        let stage = lifecycle_stage_label(event_type, payload);
        let spawn_reason = if event_type == "task.spawned" {
            payload
                .get("spawn_reason")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        } else {
            None
        };
        let spawn_reason_full = if event_type == "task.spawned" {
            payload
                .get("spawn_reason_full")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        } else {
            None
        };
        let result_summary = if event_type == "task.completed" || event_type == "task.failed" {
            payload
                .get("result_summary")
                .and_then(|v| v.as_str())
                .or_else(|| payload.get("reason").and_then(|v| v.as_str()))
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        } else {
            None
        };
        let should_collapse = app.task_lifecycles.contains_key(task_id);
        if should_collapse {
            let lc = app.task_lifecycles.get_mut(task_id).unwrap();
            lc.stages.push(stage);
            if spawn_reason.is_some() {
                lc.spawn_reason = spawn_reason;
            }
            if spawn_reason_full.is_some() {
                lc.spawn_reason_full = spawn_reason_full;
            }
            if result_summary.is_some() {
                lc.result_summary = result_summary;
            }
            let collapsed = format_lifecycle_line(
                &lc.agent_suffix,
                task_id,
                &lc.stages,
                lc.spawn_reason.as_deref(),
                lc.spawn_reason_full.as_deref(),
                lc.result_summary.as_deref(),
                lc.expanded,
            );
            if let Some(&idx) = app.task_lifecycle_msg_idx.get(task_id) {
                if idx < app.messages.len() {
                    app.messages[idx].content = collapsed.clone();
                    app.messages[idx].role = role;
                    if app.follow_output {
                        app.scroll_offset = app.last_max_scroll_offset;
                    }
                    return;
                }
            }
            app.add_message(role, collapsed);
        } else {
            let collapsed = format_lifecycle_line(
                agent_suffix,
                task_id,
                std::slice::from_ref(&stage),
                spawn_reason.as_deref(),
                spawn_reason_full.as_deref(),
                result_summary.as_deref(),
                false,
            );
            app.task_lifecycles.insert(
                task_id.to_string(),
                TaskLifecycle {
                    agent_suffix: agent_suffix.to_string(),
                    stages: vec![stage],
                    spawn_reason,
                    spawn_reason_full,
                    result_summary,
                    expanded: false,
                },
            );
            app.add_message(role, collapsed);
        }
        let msg_idx = app.messages.len().saturating_sub(1);
        app.task_lifecycle_msg_idx.insert(task_id.to_string(), msg_idx);
        return;
    }

    match role {
        MessageRole::SignalLow => {
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

/// Compact token counts for the narrow right pane (width ~42).
fn format_tokens_compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 100_000 {
        format!("{:.0}k", n as f64 / 1000.0)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n >= 1_000 {
        format!("{:.2}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Prefer the completion with the largest `input_tokens` in `llm_usage` so the pane reflects peak prompt size for that ingest.
fn pick_peak_llm_usage_from_result(result: &serde_json::Value) -> Option<LlmExchangeUsage> {
    let arr = result.get("llm_usage")?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let mut best: Option<LlmExchangeUsage> = None;
    for v in arr {
        let u: LlmExchangeUsage = serde_json::from_value(v.clone()).ok()?;
        best = Some(match best {
            None => u,
            Some(prev) if u.input_tokens > prev.input_tokens => u,
            Some(prev) => prev,
        });
    }
    best
}

struct ChatLayout {
    status: Rect,
    separator: Rect,
    messages: Rect,
    right_pane: Option<Rect>,
    input: Rect,
    hints: Option<Rect>,
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
            hints: None,
        }
    } else {
        let footer = rows[3];
        let show_hints = footer.width > HINTS_PANE_WIDTH + FOOTER_INPUT_MIN_WIDTH;
        if show_hints {
            let footer_cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(FOOTER_INPUT_MIN_WIDTH),
                    Constraint::Length(HINTS_PANE_WIDTH),
                ])
                .split(footer);
            ChatLayout {
                status: rows[0],
                separator: rows[1],
                messages: body,
                right_pane: None,
                input: footer_cols[0],
                hints: Some(footer_cols[1]),
            }
        } else {
            ChatLayout {
                status: rows[0],
                separator: rows[1],
                messages: body,
                right_pane: None,
                input: footer,
                hints: None,
            }
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
    if let Some(hints) = layout.hints {
        draw_hints_pane(f, app, hints);
    }

    // Software cursor (white-background span in draw_input) is the sole
    // visible cursor. Do NOT call f.set_cursor_position here — ratatui shows
    // the OS hardware cursor whenever set_cursor_position is called, which
    // overrides the Hide from terminal setup and produces a duplicated cursor
    // on native Linux terminals (WSL terminals tend to suppress the OS cursor
    // in raw+alt-screen mode regardless, which is why this bug was invisible
    // there).
    //
    // Tradeoff: IME composition windows (fcitx, ibus, etc.) anchor to the
    // OS cursor position. Without set_cursor_position, IME popups for
    // non-Latin input may appear at (0, 0) or stale positions rather than
    // tracking the input column. If IME support becomes a requirement,
    // re-introduce set_cursor_position and instead drop the software cursor
    // span in draw_input so the OS cursor is the sole indicator.

    if let Some(ref overlay) = app.pending_overlay {
        draw_pending_overlay(f, overlay, area);
    }
}

fn draw_pending_overlay(f: &mut Frame, overlay: &PendingOverlay, area: Rect) {
    let overlay_height = area.height.min(28);
    let overlay_width = (area.width - 4).min(90);
    let overlay_rect = Rect::new(
        (area.width.saturating_sub(overlay_width)) / 2,
        (area.height.saturating_sub(overlay_height)) / 2,
        overlay_width,
        overlay_height,
    );

    f.render_widget(Clear, overlay_rect);

    let border_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(
            " Pending Items ({}) │ ↑↓ j/k: nav │ a: approve │ Enter: answer │ Esc: close ",
            overlay.items.len()
        ))
        .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    let inner = border_block.inner(overlay_rect);
    f.render_widget(border_block, overlay_rect);

    if overlay.items.is_empty() {
        let p = Paragraph::new("No pending approvals or interactions.")
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, inner);
        return;
    }

    let list_height = (inner.height / 2).max(3);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(list_height), Constraint::Min(3)])
        .split(inner);

    let list_lines: Vec<Line> = overlay
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let is_selected = i == overlay.selected;
            let style = if is_selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let marker = if is_selected { "▶ " } else { "  " };
            match item {
                PendingItem::Approval(req) => {
                    let label = action_summary(&req.action);
                    let id_short = &req.request_id[..req.request_id.len().min(16)];
                    Line::from(vec![
                        Span::styled(marker.to_string(), style),
                        Span::styled(format!("⏸ {:16} ", id_short), style),
                        Span::styled(label.to_string(), Style::default().fg(Color::White)),
                    ])
                }
                PendingItem::Interaction(ui) => {
                    let id_short = &ui.interaction_id[..ui.interaction_id.len().min(16)];
                    let kind_str = format!("{}", ui.kind);
                    Line::from(vec![
                        Span::styled(marker.to_string(), style),
                        Span::styled(format!("🔔 {:16} ", id_short), style),
                        Span::styled(kind_str, Style::default().fg(Color::Magenta)),
                    ])
                }
            }
        })
        .collect();

    let visible_start = if overlay.selected >= list_height as usize {
        overlay.selected - list_height as usize + 1
    } else {
        0
    };
    let visible_items: Vec<Line> = list_lines
        .into_iter()
        .skip(visible_start)
        .take(list_height as usize)
        .collect();
    let list_p = Paragraph::new(visible_items);
    f.render_widget(list_p, chunks[0]);

    let detail_lines = if let Some(item) = overlay.items.get(overlay.selected) {
        match item {
            PendingItem::Approval(req) => {
                let mut lines: Vec<Line> = Vec::new();
                lines.push(Line::from(Span::styled(
                    "APPROVAL",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(format!("ID:       {}", req.request_id)));
                lines.push(Line::from(format!("Agent:    {}", req.agent_id)));
                lines.push(Line::from(format!("Session:  {}", req.session_id)));
                lines.push(Line::from(format!("Created:  {}", req.created_at)));
                if let Some(r) = req.reason.as_deref() {
                    let reason_preview: String = r.chars().take(120).collect();
                    lines.push(Line::from(format!("Reason:   {}", reason_preview)));
                }
                lines.push(Line::from(""));
                let action_lines = format_scheduled_action_detail_lines(&req.action);
                for al in action_lines.iter().take(6) {
                    let preview: String = al.chars().take((inner.width as usize).saturating_sub(2)).collect();
                    lines.push(Line::from(Span::styled(
                        preview,
                        Style::default().fg(Color::White),
                    )));
                }
                if let Some(ref risk) = req.risk_summary {
                    lines.push(Line::from(""));
                    let mut parts = Vec::new();
                    if risk.host_count > 0 {
                        parts.push(format!("{} host(s)", risk.host_count));
                    }
                    if !risk.dangerous_patterns.is_empty() {
                        parts.push(format!("{} risk(s)", risk.dangerous_patterns.len()));
                    }
                    if !parts.is_empty() {
                        lines.push(Line::from(Span::styled(
                            format!("Risk: {}", parts.join(" | ")),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Press [a] to approve",
                    Style::default().fg(Color::Green),
                )));
                lines
            }
            PendingItem::Interaction(ui) => {
                let mut lines: Vec<Line> = Vec::new();
                lines.push(Line::from(Span::styled(
                    format!("INTERACTION ({})", ui.kind),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(format!("ID:       {}", ui.interaction_id)));
                lines.push(Line::from(format!("Agent:    {}", ui.agent_id)));
                lines.push(Line::from(format!("Session:  {}", ui.session_id)));
                lines.push(Line::from(""));

                let q_preview: String = ui.question.chars().take(200).collect();
                lines.push(Line::from(Span::styled(
                    "Question:",
                    Style::default().fg(Color::White),
                )));
                for ln in q_preview.lines().take(3) {
                    lines.push(Line::from(format!("  {}", ln)));
                }

                if !ui.options.is_empty() {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "Options:",
                        Style::default().fg(Color::White),
                    )));
                    for (i, opt) in ui.options.iter().enumerate() {
                        lines.push(Line::from(format!(
                            "  {}. {} — {}",
                            i + 1,
                            opt.label,
                            opt.value
                        )));
                    }
                }

                if ui.allow_freeform {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "Freeform input accepted",
                        Style::default().fg(Color::DarkGray),
                    )));
                }

                lines.push(Line::from(""));
                if !ui.options.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "Press [1-9] to select option, or [Enter] to answer with freeform text",
                        Style::default().fg(Color::Green),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        "Press [Enter] to answer with freeform text",
                        Style::default().fg(Color::Green),
                    )));
                }
                lines
            }
        }
    } else {
        vec![Line::from(Span::styled(
            "No selection",
            Style::default().fg(Color::DarkGray),
        ))]
    };

    let detail_p = Paragraph::new(detail_lines)
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(detail_p, chunks[1]);
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
        Line::from(Span::styled(
            "LLM context",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];

    match &app.last_llm_context {
        None => {
            lines.push(Line::raw("(after first reply)"));
        }
        Some(u) => {
            let model = if u.model.len() > 22 {
                format!("{}…", &u.model[..22])
            } else {
                u.model.clone()
            };
            let context_unknown = u.context_window_tokens.is_none() || u.context_window_tokens == Some(0);
            let model_line = if context_unknown {
                format!("{} ⚠", model)
            } else {
                model
            };
            lines.push(Line::from(Span::styled(
                model_line,
                if context_unknown {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                },
            )));
            let in_s = format_tokens_compact(u.input_tokens);
            let derived_pct = u.context_window_tokens.map(|w| {
                if w == 0 {
                    None
                } else {
                    Some((u.input_tokens as f64 / f64::from(w)) * 100.0)
                }
            });
            let pct_for_style = u
                .input_context_pct
                .map(f64::from)
                .or(derived_pct.flatten());
            let (detail, pct_style) = match (u.context_window_tokens, u.input_context_pct) {
                (Some(w), Some(p)) => {
                    let w_s = format_tokens_compact(u64::from(w));
                    (
                        format!("{}/{} tok · {:.0}%", in_s, w_s, f64::from(p)),
                        match f64::from(p) {
                            x if x >= 95.0 => Style::default().fg(Color::Red),
                            x if x >= 80.0 => Style::default().fg(Color::Yellow),
                            _ => Style::default(),
                        },
                    )
                }
                (Some(w), None) => {
                    let w_s = format_tokens_compact(u64::from(w));
                    let body = if let Some(p) = derived_pct.flatten() {
                        format!("{}/{} tok · {:.0}%", in_s, w_s, p)
                    } else {
                        format!("{}/{} tok", in_s, w_s)
                    };
                    let style = match pct_for_style {
                        Some(x) if x >= 95.0 => Style::default().fg(Color::Red),
                        Some(x) if x >= 80.0 => Style::default().fg(Color::Yellow),
                        _ => Style::default(),
                    };
                    (body, style)
                }
                _ => (
                    format!("prompt {} tok · max n/a", in_s),
                    Style::default(),
                ),
            };
            lines.push(Line::from(Span::styled(detail, pct_style)));
        }
    }

    // Aggregate session token stats
    let ts = &app.token_stats;
    if ts.count > 0 {
        let in_min = format_tokens_compact(ts.min_input);
        let in_avg = format_tokens_compact(ts.avg_input());
        let in_max = format_tokens_compact(ts.max_input);
        let out_min = format_tokens_compact(ts.min_output);
        let out_avg = format_tokens_compact(ts.avg_output());
        let out_max = format_tokens_compact(ts.max_output);
        lines.push(Line::raw(format!("in: avg {}·max {}·min {}", in_avg, in_max, in_min)));
        lines.push(Line::raw(format!("out: avg {}·max {}·min {}", out_avg, out_max, out_min)));
        lines.push(Line::raw(format!("calls: {}", ts.count)));
        if ts.cost_count > 0 {
            lines.push(Line::raw(format!("cost: ${:.6}", ts.total_cost)));
        }
    }

    lines.extend([
        Line::raw(""),
        Line::from(Span::styled("Active Agents", Style::default().add_modifier(Modifier::BOLD))),
    ]);

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

    let has_pending_approvals = !app.pending_approval_ids.is_empty();
    let has_pending_questions = !app.pending_question_summaries.is_empty();

    if has_pending_approvals || has_pending_questions {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            if has_pending_approvals && has_pending_questions {
                format!("⚠ Pending ({}/{})", app.pending_approval_ids.len(), app.pending_question_summaries.len())
            } else if has_pending_approvals {
                format!("⚠ Pending ({} approval{})", app.pending_approval_ids.len(), if app.pending_approval_ids.len() == 1 { "" } else { "s" })
            } else {
                format!("⚠ Pending ({} question{})", app.pending_question_summaries.len(), if app.pending_question_summaries.len() == 1 { "" } else { "s" })
            },
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )));
        for (_, summary) in app.pending_approval_summaries.iter().take(3) {
            lines.push(Line::from(Span::styled(
                format!("  ⏸ {}", summary),
                Style::default().fg(Color::Yellow),
            )));
        }
        for q in app.pending_question_summaries.iter().take(2) {
            lines.push(Line::from(Span::styled(
                format!("  💬 {}", q),
                Style::default().fg(Color::Cyan),
            )));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        if has_pending_approvals {
            "Approvals:"
        } else {
            "Approvals"
        },
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if app.pending_approval_ids.is_empty() {
        lines.push(Line::raw("none"));
    } else {
        for (i, (id, _)) in app.pending_approval_summaries.iter().rev().enumerate() {
            if i >= 5 {
                break;
            }
            lines.push(Line::raw(format!("- {}", id)));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Policy Decisions",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if app.policy_causal_pane.is_empty() {
        lines.push(Line::raw("none"));
    } else {
        for line in app.policy_causal_pane.iter().take(POLICY_CAUSAL_PANE_MAX) {
            lines.push(Line::raw(line.clone()));
        }
    }

    let gate_count = app.gate_history_approvals.len() + app.gate_history_interactions.len();
    if gate_count > 0 {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Gate History",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for entry in app.gate_history_approvals.iter().rev().take(4) {
            lines.push(Line::raw(entry.clone()));
        }
        for entry in app.gate_history_interactions.iter().rev().take(4) {
            lines.push(Line::raw(entry.clone()));
        }
    }

    if let Some(ref wb) = app.active_workbench {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Workbench",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        let id_short = if wb.workbench_id.len() > 14 {
            format!("{}…", &wb.workbench_id[..14])
        } else {
            wb.workbench_id.clone()
        };
        lines.push(Line::raw(format!("{} ({})", id_short, wb.status)));
        lines.push(Line::raw(format!(
            "files:{} changed:{}",
            wb.file_count, wb.changed_files
        )));
        let art_short = if wb.base_artifact_id.len() > 18 {
            format!("{}…", &wb.base_artifact_id[..18])
        } else {
            wb.base_artifact_id.clone()
        };
        lines.push(Line::from(Span::styled(
            format!("base: {}", art_short),
            Style::default().fg(Color::DarkGray),
        )));
        if wb.status == "active" {
            lines.push(Line::from(Span::styled(
                "  → /wb reconcile | /wb discard",
                Style::default().fg(Color::Cyan),
            )));
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

fn draw_hints_pane(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        "Keys",
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    lines.push(Line::raw("Enter send"));
    lines.push(Line::raw("Esc pause/cancel"));
    lines.push(Line::raw("Ctrl+C quit"));
    lines.push(Line::raw("↑↓ history"));
    lines.push(Line::raw("Shift+↑↓ scroll"));
    lines.push(Line::raw("Ctrl+F jump live"));
    lines.push(Line::raw("/session picker"));
    if app.inline_approvals_enabled {
        if app.pending_approval_ids.is_empty() {
            lines.push(Line::raw("Ctrl+A approve: none"));
        } else {
            lines.push(Line::raw(format!(
                "Ctrl+A approve: {}",
                app.pending_approval_ids.len()
            )));
        }
    }
    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::LEFT | Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
    f.render_widget(paragraph, area);
}

fn message_role_style(role: MessageRole) -> Style {
    match role {
        MessageRole::User => Style::default().fg(Color::Green),
        MessageRole::Assistant => Style::default().fg(Color::Reset),
        MessageRole::System => Style::default().fg(Color::Yellow),
        MessageRole::Signal => Style::default().fg(Color::LightCyan),
        MessageRole::SignalLow => Style::default().fg(Color::DarkGray),
        MessageRole::AgentOutput => Style::default().fg(Color::Magenta),
    }
}

/// Lightweight markdown: headings, lists, blockquotes, HR,
/// **bold**, `inline code`, ~~strikethrough~~, [links](url).
/// `in_code_block` toggles dim background rendering for fenced code blocks.
fn parse_inline_markdown(
    text: &str,
    base_style: Style,
    in_code_block: bool,
) -> (Vec<Span<'static>>, usize) {
    if in_code_block {
        let s = Span::styled(text.to_string(), base_style.bg(Color::Black).add_modifier(Modifier::DIM));
        return (vec![s], text.len());
    }

    let trimmed = text.trim_start();

    // Horizontal rules: ---, ***, ___
    if is_horizontal_rule(trimmed) {
        let rule = "─".repeat(text.len().max(1));
        return (vec![Span::styled(rule, base_style.add_modifier(Modifier::DIM))], text.len());
    }

    // Headings: # ## ### #### ##### ######
    if let Some(level) = heading_level(trimmed) {
        let content = trimmed.trim_start_matches('#').trim();
        let heading_style = match level {
            1 => base_style
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::UNDERLINED),
            _ => base_style.fg(Color::LightYellow).add_modifier(Modifier::BOLD),
        };
        let (spans, plain_len) = parse_inline_spans(content, base_style);
        let styled: Vec<Span<'static>> = spans
            .into_iter()
            .map(|s| Span::styled(s.content.clone(), heading_style))
            .collect();
        return (styled, plain_len);
    }

    // Unordered lists: - * +
    if let Some(content) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        let bullet = Span::styled("• ", base_style);
        let (spans, plain_len) = parse_inline_spans(content, base_style);
        let mut result = vec![bullet];
        result.extend(spans);
        return (result, plain_len + 2);
    }

    // Ordered lists: 1. 2. etc.
    if let Some(num_str) = ordered_list_prefix(trimmed) {
        let prefix_text = format!("{}. ", num_str);
        let prefix_len = prefix_text.len();
        let prefix = Span::styled(prefix_text, base_style);
        let content = trimmed.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.')
            .trim_start();
        let (spans, plain_len) = parse_inline_spans(content, base_style);
        let mut result = vec![prefix];
        result.extend(spans);
        return (result, plain_len + prefix_len);
    }

    // Blockquotes: >
    if trimmed.starts_with("> ") || trimmed.starts_with('>') {
        let content = trimmed.trim_start_matches('>').trim_start();
        let pipe = Span::styled("│ ".to_string(), base_style.add_modifier(Modifier::DIM).fg(Color::DarkGray));
        let (spans, plain_len) = parse_inline_spans(content, base_style);
        let mut result = vec![pipe];
        result.extend(spans);
        return (result, plain_len + 2);
    }

    parse_inline_spans(text, base_style)
}

/// Detects heading level (1-6) or returns None.
fn heading_level(s: &str) -> Option<usize> {
    let mut count = 0;
    for ch in s.chars() {
        if ch == '#' {
            count += 1;
        } else if ch == ' ' && count > 0 && count <= 6 {
            return Some(count);
        } else {
            return None;
        }
    }
    None
}

/// True when line is purely `---`, `***`, `___` (with optional spaces).
fn is_horizontal_rule(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 3 {
        return false;
    }
    let ch = s.chars().next().unwrap();
    (ch == '-' || ch == '*' || ch == '_') && s.chars().all(|c| c == ch || c == ' ')
}

/// Returns the numeric prefix of an ordered list item (e.g. "1" from "1. text").
fn ordered_list_prefix(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let num_end = s.find(|c: char| !c.is_ascii_digit())?;
    let after = &s[num_end..];
    if after.starts_with(". ") || after.starts_with('.') {
        Some(&s[..num_end])
    } else {
        None
    }
}

/// Inline span parsing: **bold**, `code`, ~~strikethrough~~, [text](url).
fn parse_inline_spans(text: &str, base_style: Style) -> (Vec<Span<'static>>, usize) {
    use std::fmt::Write;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut rest = text;

    while !rest.is_empty() {
        // Bold: **text**
        if let Some(pos) = rest.find("**") {
            if pos > 0 {
                let before = &rest[..pos];
                spans.push(Span::styled(before.to_string(), base_style));
                let _ = write!(plain, "{}", before);
            }
            let after_marker = &rest[pos + 2..];
            if let Some(end) = after_marker.find("**") {
                let bold = &after_marker[..end];
                spans.push(Span::styled(
                    bold.to_string(),
                    base_style.add_modifier(Modifier::BOLD),
                ));
                let _ = write!(plain, "{}", bold);
                rest = &after_marker[end + 2..];
            } else {
                spans.push(Span::styled("**".to_string(), base_style));
                plain.push_str("**");
                rest = after_marker;
            }
        // Strikethrough: ~~text~~
        } else if let Some(pos) = rest.find("~~") {
            if pos > 0 {
                let before = &rest[..pos];
                spans.push(Span::styled(before.to_string(), base_style));
                let _ = write!(plain, "{}", before);
            }
            let after_marker = &rest[pos + 2..];
            if let Some(end) = after_marker.find("~~") {
                let strike = &after_marker[..end];
                spans.push(Span::styled(
                    strike.to_string(),
                    base_style.add_modifier(Modifier::CROSSED_OUT),
                ));
                let _ = write!(plain, "{}", strike);
                rest = &after_marker[end + 2..];
            } else {
                spans.push(Span::styled("~~".to_string(), base_style));
                plain.push_str("~~");
                rest = after_marker;
            }
        // Inline code: `code`
        } else if let Some(pos) = rest.find('`') {
            if pos > 0 {
                let before = &rest[..pos];
                spans.push(Span::styled(before.to_string(), base_style));
                let _ = write!(plain, "{}", before);
            }
            let after_marker = &rest[pos + 1..];
            if let Some(end) = after_marker.find('`') {
                let code = &after_marker[..end];
                spans.push(Span::styled(
                    code.to_string(),
                    base_style.fg(Color::LightGreen).bg(Color::Black),
                ));
                let _ = write!(plain, "{}", code);
                rest = &after_marker[end + 1..];
            } else {
                spans.push(Span::styled("`".to_string(), base_style));
                plain.push('`');
                rest = after_marker;
            }
        // Links: [text](url)
        } else if let Some(pos) = rest.find('[') {
            if pos > 0 {
                let before = &rest[..pos];
                spans.push(Span::styled(before.to_string(), base_style));
                let _ = write!(plain, "{}", before);
            }
            let after_bracket = &rest[pos + 1..];
            if let Some(close) = after_bracket.find(']') {
                let link_text = &after_bracket[..close];
                let after_close = &after_bracket[close + 1..];
                if let Some(url_start) = after_close.strip_prefix('(') {
                    if let Some(url_end) = url_start.find(')') {
                        let _url = &url_start[..url_end];
                        spans.push(Span::styled(
                            link_text.to_string(),
                            base_style
                                .fg(Color::LightCyan)
                                .add_modifier(Modifier::UNDERLINED),
                        ));
                        let _ = write!(plain, "{}", link_text);
                        rest = &url_start[url_end + 1..];
                    } else {
                        // Unterminated — render literal
                        spans.push(Span::styled(
                            format!("[{link_text}]("),
                            base_style,
                        ));
                        let _ = write!(plain, "[{link_text}](",);
                        rest = url_start;
                    }
                } else {
                    spans.push(Span::styled(
                        format!("[{link_text}]"),
                        base_style,
                    ));
                    let _ = write!(plain, "[{link_text}]");
                    rest = after_close;
                }
            } else {
                spans.push(Span::styled("[".to_string(), base_style));
                plain.push('[');
                rest = after_bracket;
            }
        } else {
            spans.push(Span::styled(rest.to_string(), base_style));
            let _ = write!(plain, "{}", rest);
            break;
        }
    }

    (spans, plain.len())
}

/// Vertical line count for transcript lines as ratatui's `Paragraph::wrap(Wrap { trim: false })`
/// will render them — must match [`draw_messages`] or follow-mode scroll drifts from the true end.
fn transcript_wrap_line_count(paragraph_line: Line<'static>, wrap_width: u16) -> usize {
    Paragraph::new(paragraph_line)
        .wrap(Wrap { trim: false })
        .line_count(wrap_width.max(1))
        .max(1)
}

fn draw_messages(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let wrap_width = area.width.saturating_sub(1).max(1);
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

    let content_width = area.width.saturating_sub(1);
    for msg in &app.messages {
        if let Some(ref card) = msg.rich_card {
            let rich_lines = match card {
                RichCard::UserInteraction(interaction) => {
                    render_interaction_card(interaction, content_width)
                }
                RichCard::Approval {
                    request,
                    detail,
                    enrichment,
                } => render_approval_card(request, detail, enrichment, content_width),
            };
            for rl in &rich_lines {
                let visual_line_count = transcript_wrap_line_count(rl.clone(), wrap_width);
                let visual_line_end = visual_row.saturating_add(visual_line_count);
                let include_line = visual_line_end > visual_window_start;

                if include_line {
                    if render_base_visual_row.is_none() {
                        render_base_visual_row = Some(visual_row);
                    }
                    lines.push(rl.clone());
                }

                visual_row = visual_line_end;
                row = row.saturating_add(1);
            }
        } else {
            let icon = match msg.role {
                MessageRole::User => "> ",
                MessageRole::Assistant => "🤖 ",
                MessageRole::System => "ℹ ",
                MessageRole::Signal => "🔔 ",
                MessageRole::SignalLow => "  ",
                MessageRole::AgentOutput => "📝 ",
            };
            let style = message_role_style(msg.role);

            let mut in_code_block = false;
            for (i, text_line) in msg.content.lines().enumerate() {
                let trimmed = text_line.trim();
                if trimmed.starts_with("```") {
                    in_code_block = !in_code_block;
                }

                let prefix = if i == 0 { icon } else { "  " };
                let visual_line_count = transcript_wrap_line_count(
                    Line::from(vec![
                        Span::raw(prefix),
                        Span::styled(text_line.to_string(), style),
                    ]),
                    wrap_width,
                );
                let visual_line_end = visual_row.saturating_add(visual_line_count);
                let include_line = visual_line_end > visual_window_start;

                let is_selected =
                    row >= content_sel_top
                        && row <= content_sel_bot
                        && content_sel_top != usize::MAX;

                if include_line && is_selected {
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

                    let (sel_start, sel_end) = if sel_col_start <= sel_col_end {
                        (sel_col_start, sel_col_end)
                    } else {
                        (sel_col_end, sel_col_start)
                    };

                    let text_style = if in_code_block {
                        style.bg(Color::Black).add_modifier(Modifier::DIM)
                    } else {
                        style
                    };

                    let mut spans: Vec<Span> = Vec::new();
                    spans.push(Span::raw(prefix));

                    let sel_start_clamped = sel_start.min(text_line.len());
                    let sel_end_clamped = sel_end.min(text_line.len());

                    let sel_start_clamped =
                        if text_line.is_char_boundary(sel_start_clamped) {
                            sel_start_clamped
                        } else {
                            (0..sel_start_clamped)
                                .rfind(|&i| text_line.is_char_boundary(i))
                                .unwrap_or(0)
                        };
                    let sel_end_clamped =
                        if text_line.is_char_boundary(sel_end_clamped) {
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
                        spans.push(Span::styled(before_sel.to_string(), text_style));
                    }
                    if !in_sel.is_empty() {
                        spans
                            .push(Span::styled(in_sel.to_string(), text_style.bg(Color::DarkGray)));
                    }
                    if !after_sel.is_empty() {
                        spans.push(Span::styled(after_sel.to_string(), text_style));
                    }

                    lines.push(Line::from(spans));
                } else if include_line {
                    let line_spans = if matches!(msg.role, MessageRole::Assistant) {
                        let (spans, _) = parse_inline_markdown(text_line, style, in_code_block);
                        let mut all = vec![Span::raw(prefix)];
                        all.extend(spans);
                        all
                    } else {
                        vec![
                            Span::raw(prefix),
                            Span::styled(text_line.to_string(), style),
                        ]
                    };
                    lines.push(Line::from(line_spans));
                }

                if include_line && render_base_visual_row.is_none() {
                    render_base_visual_row = Some(visual_row);
                }

                visual_row = visual_line_end;
                row = row.saturating_add(1);
            }
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

    // Active-work indicator (pending RPCs, approvals, user interactions, workflow tasks)
    if let Some(pending_text) = app.active_work_text() {
        let pending_line = Line::from(vec![Span::styled(
            pending_text,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        )]);
        let include_pending = visual_row
            .saturating_add(transcript_wrap_line_count(pending_line.clone(), wrap_width))
            > visual_window_start;
        if include_pending {
            if render_base_visual_row.is_none() {
                render_base_visual_row = Some(visual_row);
            }
            lines.push(pending_line);
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

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let health = app.context_health_badge().unwrap_or("");
    let workflow = if health.is_empty() {
        app.session_overview.status_line()
    } else {
        format!("{} {}", app.session_overview.status_line(), health)
    };
    let gateway = if app.gateway_connected {
        "Gateway: connected"
    } else {
        "Gateway: reconnecting"
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
        "Follow: OFF"
    };
    let text = if !app.pending.is_empty() {
        format!(
            "{} {} pending | {} | {} | {} | {} | {}",
            app.spinner(),
            app.pending.len(),
            workflow,
            gateway,
            pause_hint,
            esc_hint,
            follow_hint,
        )
    } else if !app.pending_approval_ids.is_empty() {
        format!(
            "{} {} approval(s) pending | {} | {} | {} | {} | {}",
            app.spinner(),
            app.pending_approval_ids.len(),
            workflow,
            gateway,
            pause_hint,
            esc_hint,
            follow_hint,
        )
    } else if app.session_overview.pending_user_interactions > 0 {
        format!(
            "{} awaiting your response | {} | {} | {} | {} | {}",
            app.spinner(),
            workflow,
            gateway,
            pause_hint,
            esc_hint,
            follow_hint,
        )
    } else if app.session_overview.workflow.running > 0
        || app.session_overview.workflow.queued > 0
        || app.session_overview.workflow.awaiting > 0
    {
        format!(
            "{} {} | {} | {} | {} | {}",
            app.spinner(),
            workflow,
            gateway,
            pause_hint,
            esc_hint,
            follow_hint,
        )
    } else {
        format!(
            "Session: {} | Target: {} | {} | {} | {} | {} | {}",
            &app.session_id[..20.min(app.session_id.len())],
            app.target_hint,
            gateway,
            workflow,
            pause_hint,
            esc_hint,
            follow_hint,
        )
    };

    let p = Paragraph::new(Span::styled(text, Style::default().fg(Color::White)));
    f.render_widget(p, area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let prefix = Span::styled(
        app.input_prefix(),
        Style::default().fg(Color::Green),
    );

    let inner_width = area.width.saturating_sub(2) as usize;

    let text = if app.input.is_empty() {
        let mut lines = wrap_spans(&[prefix], inner_width);
        if let Some(last) = lines.last_mut() {
            let mut last_spans = std::mem::take(last);
            last_spans.spans.push(Span::styled(" ", Style::default().bg(Color::White)));
            *lines.last_mut().unwrap() = last_spans;
        }
        Text::from(lines)
    } else {
        let before = &app.input[..app.cursor_pos];
        let after = &app.input[app.cursor_pos..];

        let mut spans = vec![prefix];

        if !before.is_empty() {
            spans.push(Span::raw(before.to_string()));
        }
        if after.is_empty() {
            spans.push(Span::styled(" ", Style::default().bg(Color::White)));
        } else {
            let c = after.chars().next().unwrap();
            let c_len = c.len_utf8();
            spans.push(Span::styled(
                c.to_string(),
                Style::default().fg(Color::Black).bg(Color::White),
            ));
            if c_len < after.len() {
                spans.push(Span::raw(after[c_len..].to_string()));
            }
        }

        Text::from(wrap_spans(&spans, inner_width))
    };

    let p = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn wrap_spans(spans: &[Span], max_width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = vec![];
    let mut current_line: Vec<Span<'static>> = vec![];
    let mut current_width: usize = 0;

    for span in spans {
        let mut chars: Vec<(char, Style)> = Vec::new();
        for c in span.content.chars() {
            chars.push((c, span.style));
        }
        if chars.is_empty() {
            continue;
        }

        let mut i = 0;
        while i < chars.len() {
            let (c, style) = &chars[i];
            let s = c.to_string();
            let cw = UnicodeWidthStr::width(s.as_str());
            if current_width + cw > max_width && !current_line.is_empty() {
                lines.push(Line::from(std::mem::take(&mut current_line)));
                current_width = 0;
            }
            current_width += cw;
            current_line.push(Span::styled(c.to_string(), *style));
            i += 1;
        }
    }

    if !current_line.is_empty() {
        lines.push(Line::from(current_line));
    }
    if lines.is_empty() {
        lines.push(Line::from(vec![]));
    }
    lines
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

async fn handle_chat_test_mode(
    config_path: &Path,
    _config: &autonoetic_types::config::GatewayConfig,
    gateway_addr: &str,
    jsonrpc_auth_token: &str,
    initial_session_id: String,
    initial_target_hint: String,
    sender_id: String,
    channel_id: String,
) -> anyhow::Result<()> {
    let mut current_session_id = initial_session_id;
    let mut current_target_hint = initial_target_hint;
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut request_counter = 1u64;

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim_end().to_string();
        if trimmed.trim().is_empty() {
            continue;
        }

        if trimmed.trim_start().starts_with('/') {
            match parse_slash_command(&trimmed) {
                Ok(SlashCommand::Quit) => break,
                Ok(SlashCommand::Help) => {
                    println!("{}", format_help_card());
                }
                Ok(SlashCommand::Status) => {
                    println!("Session: {}\nTarget: {}", current_session_id, current_target_hint);
                }
                Ok(SlashCommand::Session) => {
                    let gw_store = open_gateway_store(_config).ok();
                    let sessions =
                        load_known_sessions(config_path, &current_session_id, &current_target_hint, gw_store.as_ref());
                    let mut probe_app = App::new(
                        current_session_id.clone(),
                        current_target_hint.clone(),
                        sender_id.clone(),
                        channel_id.clone(),
                    );
                    println!("{}", format_known_sessions_card(&probe_app, &sessions));
                    probe_app.pending_prompt = None;
                }
                Ok(SlashCommand::SessionNew(name)) => {
                    current_session_id = create_named_or_generated_session(
                        name.as_deref().unwrap_or(""),
                    );
                    println!("Switched to new session {}", current_session_id);
                }
                Ok(SlashCommand::SessionSwitch(session_id)) => {
                    let gw_store = open_gateway_store(_config).ok();
                    if let Some(target_hint) =
                        load_known_sessions(config_path, &current_session_id, &current_target_hint, gw_store.as_ref())
                            .into_iter()
                            .find(|session| session.session_id == session_id)
                            .and_then(|session| session.primary_agent_id)
                    {
                        current_target_hint = target_hint;
                    }
                    current_session_id = session_id;
                    println!("Switched to session {}", current_session_id);
                }
                Ok(SlashCommand::Cancel) => {
                    println!("No active prompt in test mode.");
                }
                Ok(SlashCommand::Why(_)) => {
                    println!("/why is not supported in test mode.");
                }
                Ok(SlashCommand::Persona(_)) => {
                    println!("/persona is not supported in test mode.");
                }
                 Ok(SlashCommand::Policy(_)) => {
                     println!("/policy is not supported in test mode.");
                 }
                 Ok(SlashCommand::Pending) => {
                     println!("/pending is not supported in test mode.");
                 }
                 Ok(SlashCommand::WbStatus | SlashCommand::WbDiff | SlashCommand::WbReconcile | SlashCommand::WbDiscard) => {
                     println!("/wb commands are not supported in test mode.");
                 }
                 Ok(SlashCommand::ReturnToAgent { .. }) => {
                     println!("/return is not supported in test mode.");
                 }
                 Err(error) => {
                    println!("{}", error);
                }
            }
            continue;
        }

        let mut stream = TcpStream::connect(gateway_addr).await?;
        let request_id = format!("test-{}", request_counter);
        request_counter = request_counter.saturating_add(1);
        let request = GatewayJsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: request_id,
            method: "event.ingest".to_string(),
            params: serde_json::json!({
                "event_type": "chat",
                "message": trimmed,
                "session_id": current_session_id,
                "target_agent_id": current_target_hint,
                "metadata": terminal_channel_envelope(&channel_id, &sender_id, &current_session_id),
            }),
            auth_token: Some(jsonrpc_auth_token.to_string()),
        };
        let encoded = serde_json::to_string(&request)?;
        stream.write_all(encoded.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut response_line = String::new();
        let mut reader = BufReader::new(stream);
        let read = reader.read_line(&mut response_line).await?;
        if read == 0 {
            println!("[No response]");
            continue;
        }

        let response: GatewayJsonRpcResponse = serde_json::from_str(response_line.trim_end())?;
        if let Some(error) = response.error {
            println!("Error: {}", error.message);
            continue;
        }
        let reply = response
            .result
            .as_ref()
            .and_then(|v| v.get("assistant_reply"))
            .and_then(|v| v.as_str())
            .unwrap_or("[No response]");
        println!("{}", reply);
    }

    Ok(())
}

// ============================================================================
// Main Entry Point
// ============================================================================

pub async fn handle_chat(config_path: &Path, args: &super::common::ChatArgs) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let target_hint = args.agent_id.as_deref().unwrap_or("planner.default");
    let session_id = match &args.session_id {
        Some(sid) => sid.clone(),
        None if args.resume => resolve_latest_session(config_path, &config),
        None => generate_session_id(),
    };
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
    let config = Arc::new(config);

    let jsonrpc_auth_token = match std::env::var("AUTONOETIC_SHARED_SECRET") {
        Ok(value) => value,
        Err(_) if args.test_mode => "test-secret".to_string(),
        Err(_) => {
            return Err(anyhow::anyhow!(
                "Missing required environment variable AUTONOETIC_SHARED_SECRET for chat JSON-RPC ingress authentication"
            ))
        }
    };

    if args.test_mode {
        return handle_chat_test_mode(
            config_path,
            config.as_ref(),
            &gateway_addr,
            &jsonrpc_auth_token,
            session_id,
            target_hint.to_string(),
            sender_id,
            channel_id,
        )
        .await;
    }

    // Setup terminal (only after prerequisites—early `?` must not leave raw mode / alt screen on)
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let _terminal_restore = ChatTerminalRestore;

    let mut app = App::new(
        session_id.clone(),
        target_hint.to_string(),
        sender_id.clone(),
        channel_id.clone(),
    );
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
    add_session_banner(&mut app, config.as_ref(), &session_id);

    // Channel for sending messages from TUI to gateway
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, ChatOutbound)>();

    // When the user answers a `user_ask` via the TUI, resume runs in a background task (no
    // `event.ingest` JSON-RPC round-trip). Notify the UI loop when that work finishes so we can
    // clear the same `Working...` pending row used for normal chat sends.
    let (interaction_resume_tx, mut interaction_resume_rx) =
        tokio::sync::mpsc::unbounded_channel::<(u64, Option<String>, Option<String>)>();

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

    refresh_session_snapshot(&mut app, config.as_ref(), gateway_store.as_deref());

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
                app.gateway_connected = true;
                s
            }
            Err(e) => {
                app.gateway_connected = false;
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
            config_path,
            &config,
            gateway_store
                .as_ref()
                .map(|s| s.as_ref()),
            execution_for_interactions.as_ref(),
            &tx,
            &mut rx,
            &mut pending_map,
            &mut signal_interval,
            &shutdown,
            &jsonrpc_auth_token,
            interaction_resume_tx.clone(),
            &mut interaction_resume_rx,
        )
        .await?;

        if !disconnected {
            break; // User quit explicitly
        }

        reconnect_attempts = reconnect_attempts.saturating_add(1);
        app.gateway_connected = false;

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
    config_path: &Path,
    config: &Arc<autonoetic_types::config::GatewayConfig>,
    gateway_store: Option<&autonoetic_gateway::scheduler::gateway_store::GatewayStore>,
    execution_for_interactions: Option<&std::sync::Arc<autonoetic_gateway::execution::GatewayExecutionService>>,
    tx: &tokio::sync::mpsc::UnboundedSender<(u64, ChatOutbound)>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, ChatOutbound)>,
    pending_map: &mut std::collections::HashMap<String, u64>,
    signal_interval: &mut tokio::time::Interval,
    shutdown: &std::sync::Arc<tokio::sync::Notify>,
    jsonrpc_auth_token: &str,
    interaction_resume_tx: tokio::sync::mpsc::UnboundedSender<(u64, Option<String>, Option<String>)>,
    interaction_resume_rx: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, Option<String>, Option<String>)>,
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
            app.wrap_width = messages_content_width;
            app.messages_area_row_end = layout.messages.y.saturating_add(layout.messages.height);
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
                let active_session_id = app.session_id.clone();
                if check_signals(app, config, gateway_store, &active_session_id, tx).await {
                    needs_redraw = true;
                }
            }

            // Background `answer_and_orchestrate_resume` finished (user answered `user_ask` in TUI)
            Some((done_pending_id, assistant_reply, error_msg)) = interaction_resume_rx.recv() => {
                app.remove_pending(done_pending_id);
                if let Some(reply) = assistant_reply {
                    let formatted = format_assistant_reply(&reply);
                    app.add_message(MessageRole::Assistant, formatted.display);
                } else if let Some(err) = error_msg {
                    app.add_message(
                        MessageRole::System,
                        format!("⚠ Background orchestration failed: {}", err),
                    );
                }
                needs_redraw = true;
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

                                // Check if this was a session.status query (workflow completion reply).
                                let is_session_status =
                                    app.pending_session_status_ids.remove(&internal_id);

                                if is_session_status {
                                    // Surface the planner's final reply when status == "completed".
                                    if let Some(result) = resp.result.as_ref() {
                                        let status = result
                                            .get("status")
                                            .and_then(|s| s.as_str())
                                            .unwrap_or("");
                                        let reply = result
                                            .get("assistant_reply")
                                            .and_then(|r| r.as_str());
                                        let workflow_note = result
                                            .get("workflow_note")
                                            .and_then(|n| n.as_str());
                                        let is_terminal = status == "completed" || status == "failed";
                                        if is_terminal {
                                            let ids: Vec<u64> = app.post_approval_pending_ids.drain(..).collect();
                                            for pid in ids {
                                                app.remove_pending(pid);
                                            }
                                        }
                                        if status == "completed" {
                                            if let Some(r) = reply.filter(|s| !s.trim().is_empty()) {
                                                let formatted = format_assistant_reply(r);
                                                let is_duplicate = app.messages.iter().rev().take(5).any(|m| {
                                                    matches!(m.role, MessageRole::Assistant) && m.content == formatted.display
                                                });
                                                if !is_duplicate {
                                                    display_assistant_metadata(app, &formatted, None);
                                                    app.add_message(MessageRole::Assistant, formatted.display);
                                                }
                                            }
                                            if let Some(note) = workflow_note.filter(|s| !s.trim().is_empty()) {
                                                app.add_message(MessageRole::SignalLow, note.to_owned());
                                            }
                                        } else if status == "failed" {
                                            let error_msg = result
                                                .get("error")
                                                .and_then(|e| e.as_str())
                                                .unwrap_or("Unknown failure");
                                            if error_msg.contains("waiting for approval") {
                                                app.add_message(
                                                    MessageRole::System,
                                                    "⏳ Session status: Suspended — awaiting approval".to_string(),
                                                );
                                            } else {
                                                app.add_message(
                                                    MessageRole::System,
                                                    format!("❌ Session failed: {}", error_msg),
                                                );
                                            }
                                        } else {
                                            // For workflow sessions, a suspended_approval
                                            // response is a race condition (the planner's
                                            // async_results entry hasn't been updated yet
                                            // after WorkflowJoinSatisfied).  Don't display
                                            // the stale status; remove from the dedup set
                                            // so check_signals re-queries on the next cycle.
                                            let response_sid = result
                                                .get("session_id")
                                                .and_then(|s| s.as_str());
                                            let is_stale_workflow_suspension = status == "suspended_approval"
                                                && response_sid.map_or(false, |sid| {
                                                    app.queried_session_status_for_workflows.contains(sid)
                                                });
                                            if is_stale_workflow_suspension {
                                                if let Some(sid) = response_sid {
                                                    app.queried_session_status_for_workflows.remove(sid);
                                                }
                                            } else {
                                                let status_label = match status {
                                                    "processing" => Some("Processing"),
                                                    "suspended_approval" => Some("Suspended — awaiting approval"),
                                                    "suspended_user_input" => {
                                                        if app.session_overview.pending_user_interactions > 0 {
                                                            Some("Suspended — awaiting user input")
                                                        } else {
                                                            None
                                                        }
                                                    }
                                                    s => Some(s),
                                                };
                                                if let Some(label) = status_label {
                                                    app.add_message(
                                                        MessageRole::System,
                                                        format!("⏳ Session status: {}", label),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    needs_redraw = true;
                                    continue;
                                }

                                if let Some(error) = resp.error {
                                    let clean = if error.message.starts_with("agent.spawn failed") {
                                        let body = error.message.strip_prefix("agent.spawn failed: ").unwrap_or(&error.message);
                                        if body.starts_with("response validation failed") {
                                            format!("❌ {}", clean_validation_error(body))
                                        } else {
                                            format!("❌ Spawn failed: {}", body)
                                        }
                                    } else if error.message.starts_with("event.ingest failed") {
                                        let body = error.message.strip_prefix("event.ingest failed: ").unwrap_or(&error.message);
                                        if body.starts_with("response validation failed") {
                                            format!("❌ {}", clean_validation_error(body))
                                        } else {
                                            format!("❌ {}", body)
                                        }
                                    } else {
                                        format!("❌ Error: {}", error.message)
                                    };
                                    let clean = if let Some(data) = &error.data {
                                        let data_str = if let Some(s) = data.as_str() {
                                            s.to_owned()
                                        } else {
                                            format_json_value_as_text(data)
                                        };
                                        if data_str.len() < 200 {
                                            format!("{}\n📎 Detail: {}", clean, data_str)
                                        } else {
                                            clean
                                        }
                                    } else {
                                        clean
                                    };
                                    app.add_message(MessageRole::Signal, clean);
                                } else {
                                    let result_json = resp.result.as_ref();
                                    if let Some(v) = result_json {
                                        // Record all LLM usages for aggregate stats, then pick peak for display
                                        if let Some(usages) = v.get("llm_usage").and_then(|a| a.as_array()) {
                                            for u_val in usages {
                                                if let Ok(u) = serde_json::from_value::<LlmExchangeUsage>(u_val.clone()) {
                                                    app.token_stats.record(&u);
                                                }
                                            }
                                        }
                                        if let Some(usage) = pick_peak_llm_usage_from_result(v) {
                                            app.last_llm_context = Some(usage);
                                        }
                                    }
                                    let raw_assistant_reply: Option<String> = result_json
                                        .and_then(|v| v.get("assistant_reply").and_then(|r| r.as_str()))
                                        .map(|r| r.to_owned());
                                    let formatted = raw_assistant_reply
                                        .as_deref()
                                        .map(format_assistant_reply)
                                        .unwrap_or(AssistantReplyDisplay {
                                            display: "[No response]".to_string(),
                                            intent: None,
                                            goal_status: None,
                                        });
                                    let artifact_count = result_json
                                        .and_then(|v| v.get("artifacts"))
                                        .and_then(|a| a.as_array())
                                        .map(|a| a.len());
                                    display_assistant_metadata(app, &formatted, artifact_count);
                                    let reply_text = formatted.display;
                                    let workflow_note = result_json
                                        .and_then(|v| {
                                            v.get("workflow_note")
                                                .and_then(|n| n.as_str().map(ToOwned::to_owned))
                                        });

                                    // Try to extract user interactions directly from response (zero latency),
                                    // then fall back to store polling.
                                    let new_user_prompts_from_response = result_json
                                        .and_then(|v| v.get("pending_user_interactions"))
                                        .and_then(|v| serde_json::from_value::<Vec<UserInteraction>>(v.clone()).ok())
                                        .map(|interactions| append_new_pending_user_interaction_prompts(app, &interactions));

                                    let new_user_prompts = if new_user_prompts_from_response.is_some() {
                                        new_user_prompts_from_response.unwrap()
                                    } else if let Some(store) = gateway_store {
                                        match poll_session_snapshot(config, Some(store), &app.session_id, app.session_overview.latest_signal.clone()) {
                                            Ok(snapshot) => {
                                                app.session_overview.root_session_id = snapshot.overview.root_session_id.clone();
                                                app.session_overview.workflow = snapshot.overview.workflow.clone();
                                                app.session_overview.pending_user_interactions = snapshot.overview.pending_user_interactions;
                                                app.active_workbench = snapshot.active_workbench;
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
                                        reply_text.trim().is_empty() || reply_text == "[No response]";

                                    let reply_for_extraction = raw_assistant_reply
                                        .as_deref()
                                        .unwrap_or(&reply_text);

                                    if let Some(structured) = extract_structured_approval(reply_for_extraction) {
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
                                    } else if let Some(req_id) = extract_approval_request_id(reply_for_extraction) {
                                        app.session_overview.latest_signal =
                                            Some(format!("approval {}", req_id));
                                        app.add_message(
                                            MessageRole::Signal,
                                            format!("Approval required: {}", req_id),
                                        );
                                    }

                                    if !(new_user_prompts > 0 && reply_is_placeholder) {
                                        app.add_message(MessageRole::Assistant, reply_text);
                                    }
                                    if let Some(note) = workflow_note.filter(|s| !s.trim().is_empty()) {
                                        app.add_message(MessageRole::SignalLow, note);
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
                if let Some((id, outbound)) = msg {
                    // Handle session status query (fired after workflow.completed to surface planner reply).
                    if let ChatOutbound::SessionStatusQuery { session_id: status_sid } = &outbound {
                        let req_id = format!("session-status-{}", id);
                        let request = GatewayJsonRpcRequest {
                            jsonrpc: "2.0".to_string(),
                            id: req_id.clone(),
                            method: "session.status".to_string(),
                            params: serde_json::json!({ "session_id": status_sid }),
                            auth_token: Some(jsonrpc_auth_token.to_string()),
                        };
                        let encoded = serde_json::to_string(&request)?;
                        write_half.write_all(encoded.as_bytes()).await?;
                        write_half.write_all(b"\n").await?;
                        write_half.flush().await?;
                        pending_map.insert(req_id, id);
                        // pending_session_status_ids was already populated in check_signals
                        needs_redraw = true;
                        continue;
                    }

                    // Notify the gateway that an approval was resolved so it
                    // transitions async_results from SuspendedApproval to Processing.
                    if let ChatOutbound::ApprovalResolved { session_id: sid, root_session_id: root_sid } = &outbound {
                        let req_id = format!("approval-resolved-{}", id);
                        let request = GatewayJsonRpcRequest {
                            jsonrpc: "2.0".to_string(),
                            id: req_id.clone(),
                            method: "session.approval_resolved".to_string(),
                            params: serde_json::json!({
                                "session_id": sid,
                                "root_session_id": root_sid,
                            }),
                            auth_token: Some(jsonrpc_auth_token.to_string()),
                        };
                        let encoded = serde_json::to_string(&request)?;
                        write_half.write_all(encoded.as_bytes()).await?;
                        write_half.write_all(b"\n").await?;
                        write_half.flush().await?;
                        // Fire-and-forget: no response expected
                        needs_redraw = true;
                        continue;
                    }

                    let message_text = match &outbound {
                        ChatOutbound::Chat(s) | ChatOutbound::PolicyAuthor(s) => s.clone(),
                        ChatOutbound::ReturnToAgent { message, .. } => message.clone(),
                        ChatOutbound::SessionStatusQuery { .. } | ChatOutbound::ApprovalResolved { .. } => unreachable!(),
                    };
                    // Pending user.ask: gateway-owned answer + resume (workflow Runnable or session checkpoint).
                    let mut skip_chat_ingest = false;
                    let mut defer_pending_clear_for_interaction_resume = false;
                    if let (Some(store), Some(exec)) = (gateway_store, execution_for_interactions) {
                        if let Ok(mut pending) =
                            list_pending_user_interactions_for_terminal_session(store, &app.session_id)
                        {
                            if pending.len() > 1 {
                                let ids = pending
                                    .iter()
                                    .map(|i| i.interaction_id.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                app.add_message(
                                    MessageRole::System,
                                    format!(
                                        "Multiple pending user interactions ({ids}). Answer one explicitly with: autonoetic gateway interactions answer --interaction-id <id> --text \"...\""
                                    ),
                                );
                                skip_chat_ingest = true;
                            } else if let Some(interaction) = pending.pop() {
                                if !interaction.allow_freeform {
                                    app.add_message(
                                        MessageRole::System,
                                        format!(
                                            "Interaction {} requires --option; freeform text is not allowed.",
                                            interaction.interaction_id
                                        ),
                                    );
                                    skip_chat_ingest = true;
                                } else if autonoetic_gateway::log_redaction::looks_like_secret_value(
                                    &message_text,
                                ) {
                                    app.add_message(
                                        MessageRole::System,
                                        "Secret-like values are not accepted via interaction answers. Use credential setup/prompt flows for secrets.".to_string(),
                                    );
                                    skip_chat_ingest = true;
                                } else {
                                    let trimmed = message_text.trim().to_lowercase();
                                    let matched_option = interaction.options.iter().enumerate().find(|(i, opt)| {
                                        opt.id == trimmed
                                            || opt.label.to_lowercase() == trimmed
                                            || opt.value.to_lowercase() == trimmed
                                            || format!("{}", i + 1) == trimmed
                                    }).map(|(_, opt)| opt);
                                    let (opt_id, txt) = if let Some(opt) = matched_option {
                                        (Some(opt.id.clone()), None)
                                    } else {
                                        (None, Some(message_text.clone()))
                                    };
                                    use autonoetic_gateway::interaction_answer::{
                                        answer_and_orchestrate_resume, InteractionAnswerParams,
                                    };
                                    let interaction_id = interaction.interaction_id.clone();
                                    let interaction_id_for_task = interaction.interaction_id.clone();
                                    let exec = std::sync::Arc::clone(exec);
                                    let answer_text = message_text.clone();
                                    let resume_notify = interaction_resume_tx.clone();
                                    let pending_row_id = id;
                                    tokio::spawn(async move {
                                        let result = answer_and_orchestrate_resume(
                                            &exec,
                                            InteractionAnswerParams {
                                                interaction_id: interaction_id_for_task.clone(),
                                                answer_text: txt,
                                                answer_option_id: opt_id,
                                                answered_by: Some("chat-tui".to_string()),
                                                follow_up_message: Some(answer_text),
                                            },
                                        )
                                        .await;
                                        let error_msg = result.as_ref().err().map(|e| e.to_string());
                                        let reply = match &result {
                                            Ok(outcome) => outcome.assistant_reply.clone(),
                                            Err(_) => None,
                                        };
                                        if let Err(e) = result {
                                            tracing::warn!(
                                                target: "chat",
                                                interaction_id = %interaction_id_for_task,
                                                error = %e,
                                                "Background interaction answer orchestration failed"
                                            );
                                        }
                                        let _ = resume_notify.send((pending_row_id, reply, error_msg));
                                    });
                                    app.add_message(
                                        MessageRole::System,
                                        format!(
                                            "Answered interaction {}. Resume is running in background.",
                                            interaction_id
                                        ),
                                    );
                                    skip_chat_ingest = true;
                                    defer_pending_clear_for_interaction_resume = true;
                                }
                            }
                        }
                    }

                    if skip_chat_ingest {
                        // We skipped `event.ingest` (interaction answer path or validation error).
                        // For a background resume, keep `Working...` until orchestration finishes
                        // (see `interaction_resume_rx` branch).
                        if !defer_pending_clear_for_interaction_resume {
                            app.remove_pending(id);
                        }
                        needs_redraw = true;
                        continue;
                    }

                    let req_id = format!("tui-{}", id);
                    pending_map.insert(req_id.clone(), id);

                    let mut metadata_value = terminal_channel_envelope(
                        &app.channel_id,
                        &app.sender_id,
                        &app.session_id,
                    );

                    let root_session_id = get_root_session_id(app);

                    if matches!(&outbound, ChatOutbound::PolicyAuthor(_)) {
                        if let serde_json::Value::Object(ref mut map) = metadata_value {
                            map.insert(
                                "root_session_id".to_string(),
                                serde_json::json!(root_session_id),
                            );
                        }
                    }

                    let (target_agent_id, ingest_message, ingest_event_type, ingest_metadata) = match &outbound {
                        ChatOutbound::Chat(_) => (
                            app.target_hint.clone(),
                            message_text.clone(),
                            "chat".to_string(),
                            metadata_value,
                        ),
                        ChatOutbound::PolicyAuthor(_) => (
                            "governance-author.default".to_string(),
                            message_text.clone(),
                            "chat".to_string(),
                            metadata_value,
                        ),
                        ChatOutbound::ReturnToAgent { message, target_agent_id, metadata } => (
                            target_agent_id.clone(),
                            message.clone(),
                            "workbench_reconciled".to_string(),
                            metadata.clone(),
                        ),
                        ChatOutbound::SessionStatusQuery { .. } | ChatOutbound::ApprovalResolved { .. } => unreachable!(),
                    };

                    let params = serde_json::json!({
                        "event_type": ingest_event_type,
                        "message": ingest_message,
                        "session_id": app.session_id.clone(),
                        "target_agent_id": target_agent_id,
                        "metadata": ingest_metadata,
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
                                HandleKeyAction::SubmitInput(input) => {
                                    if input.trim_start().starts_with('/') {
                                        match parse_slash_command(&input) {
                                            Ok(SlashCommand::Policy(text)) => {
                                                let id = app.next_id();
                                                app.add_pending(id);
                                                app.add_message(MessageRole::User, input.clone());
                                                let _ = tx.send((id, ChatOutbound::PolicyAuthor(text)));
                                            }
                                            Ok(SlashCommand::ReturnToAgent { force, message }) => {
                                                let id = app.next_id();
                                                let (return_status, body) =
                                                    prepare_return_to_agent_wakeup(
                                                        gateway_store,
                                                        app.active_workbench.as_ref(),
                                                        force,
                                                        message.as_deref(),
                                                    );
                                                match return_status {
                                                    ReturnToAgentStatus::Refused { reason } => {
                                                        app.add_message(
                                                            MessageRole::System,
                                                            reason,
                                                        );
                                                    }
                                                    ReturnToAgentStatus::NoWorkbench => {
                                                        app.add_message(
                                                            MessageRole::System,
                                                            "No active workbench to return. Use /wb status to check.".to_string(),
                                                        );
                                                    }
                                                    ReturnToAgentStatus::Ready {
                                                        target_agent_id,
                                                        outbound_message,
                                                        metadata,
                                                    } => {
                                                        let force_label = if force { " --force" } else { "" };
                                                        let note_label = message
                                                            .as_deref()
                                                            .filter(|m| !m.trim().is_empty())
                                                            .map(|m| format!(" \"{}\"", m))
                                                            .unwrap_or_default();
                                                        let echo = format!(
                                                            "/return{force}{note} → {target} (workbench `{wb}`)",
                                                            force = force_label,
                                                            note = note_label,
                                                            target = target_agent_id,
                                                            wb = app
                                                                .active_workbench
                                                                .as_ref()
                                                                .map(|w| w.workbench_id.as_str())
                                                                .unwrap_or("?"),
                                                        );
                                                        app.add_message(MessageRole::User, echo);
                                                        app.add_pending(id);
                                                        let _ = tx.send((
                                                            id,
                                                            ChatOutbound::ReturnToAgent {
                                                                message: outbound_message,
                                                                target_agent_id,
                                                                metadata,
                                                            },
                                                        ));
                                                    }
                                                }
                                            }
                                            Ok(command) => {
                                                if !handle_slash_command_submission(
                                                    app,
                                                    config_path,
                                                    config,
                                                    gateway_store,
                                                    pending_map,
                                                    command,
                                                ) {
                                                    return Ok(false);
                                                }
                                            }
                                            Err(error) => {
                                                app.add_message(MessageRole::System, error);
                                            }
                                        }
                                    } else if app.pending_prompt.is_some() {
                                        handle_prompt_submission(
                                            app,
                                            config_path,
                                            config,
                                            gateway_store,
                                            pending_map,
                                            &input,
                                        );
                                    } else {
                                        let id = app.next_id();
                                        app.add_pending(id);
                                        app.add_message(MessageRole::User, input.clone());
                                        let _ = tx.send((id, ChatOutbound::Chat(input)));
                                    }
                                }
                                HandleKeyAction::PauseSession => {
                                    app.session_paused = true;
                                    let root_session_id = get_root_session_id(app);

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
                                    let root_session_id = get_root_session_id(app);

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
                                    if let Some(store) = gateway_store {
                                        let approver_level =
                                            autonoetic_types::background::ApprovalLevel::Operator;
                                        let mut options =
                                            autonoetic_gateway::scheduler::ApproveOptions::default();
                                        if let Ok(Some(ref approval)) = store.get_approval(&apr_id) {
                                            if let Some(ref phrase) = approval.confirm_phrase {
                                                options.confirm_phrase = Some(phrase.clone());
                                            }
                                            if let autonoetic_types::background::ScheduledAction::RevisionPromote {
                                                added_capabilities,
                                                broadened_capabilities,
                                                ..
                                            } = &approval.action
                                            {
                                                options.acknowledged_capabilities =
                                                    added_capabilities.iter()
                                                        .chain(broadened_capabilities.iter())
                                                        .cloned()
                                                        .collect();
                                            }
                                        }
                                        match autonoetic_gateway::scheduler::approve_request_with_options(
                                            config,
                                            Some(store),
                                            &apr_id,
                                            "chat-tui",
                                            None,
                                            None,
                                            Some(&approver_level),
                                            None,
                                            options,
                                        ) {
                                            Ok(_decision) => {
                                                app.pending_approval_ids
                                                    .retain(|id| id != &apr_id);
                                                // Keep apr_id in announced_store_approval_ids
                                                // so the next merge_gateway_store_pending_approvals
                                                // poll won't re-announce this card if the DB row
                                                // is still transiently visible as "pending".
                                                app.add_message(
                                                    MessageRole::System,
                                                    format!("Approved: {}", apr_id),
                                                );
                                                // Show working spinner until the scheduler
                                                // re-queues and finishes the task.
                                                let pid = app.next_id();
                                                app.add_pending(pid);
                                                app.post_approval_pending_ids.push(pid);

                                                // Notify the gateway so async_results transitions
                                                // from SuspendedApproval to Processing.
                                                if let Ok(Some(approval)) = store.get_approval(&apr_id) {
                                                    let notify_id = app.next_id();
                                                    let _ = tx.send((
                                                        notify_id,
                                                        ChatOutbound::ApprovalResolved {
                                                            session_id: approval.session_id.clone(),
                                                            root_session_id: approval.root_session_id.clone(),
                                                        },
                                                    ));
                                                    // For approvals that lack workflow task events
                                                    // (e.g. session_continue, emergency_stop), also
                                                    // send a session.status query to surface the reply.
                                                    if approval.workflow_id.is_none() || approval.task_id.is_none() {
                                                        let status_id = app.next_id();
                                                        app.pending_session_status_ids.insert(status_id);
                                                        let _ = tx.send((
                                                            status_id,
                                                            ChatOutbound::SessionStatusQuery {
                                                                session_id: approval.session_id,
                                                            },
                                                        ));
                                                    }
                                                }
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
                                HandleKeyAction::OpenPendingOverlay => {
                                    if let Some(store) = gateway_store {
                                        let root = autonoetic_gateway::runtime::content_store::root_session_id(&app.session_id);
                                        let mut items: Vec<PendingItem> = Vec::new();

                                        if let Ok(mut approvals) = autonoetic_gateway::scheduler::pending_approval_requests_for_root(
                                            config, Some(store), &root,
                                        ) {
                                            approvals.sort_by(|a, b| a.created_at.cmp(&b.created_at));
                                            for req in approvals {
                                                items.push(PendingItem::Approval(Box::new(req)));
                                            }
                                        }

                                        if let Ok(interactions) = list_pending_user_interactions_for_terminal_session(store, &app.session_id) {
                                            for ui in interactions {
                                                items.push(PendingItem::Interaction(Box::new(ui)));
                                            }
                                        }

                                        if items.is_empty() {
                                            app.add_message(
                                                MessageRole::System,
                                                "No pending approvals or interactions.".to_string(),
                                            );
                                        } else {
                                            app.pending_overlay = Some(PendingOverlay {
                                                items,
                                                selected: 0,
                                            });
                                        }
                                    } else {
                                        app.add_message(
                                            MessageRole::System,
                                            "Gateway store not available.".to_string(),
                                        );
                                    }
                                }
                                HandleKeyAction::OverlayApprove(idx) => {
                                    if let Some(store) = gateway_store {
                                        if let Some(PendingItem::Approval(req)) = app.pending_overlay.as_ref().and_then(|o| o.items.get(idx)) {
                                            let apr_id = req.request_id.clone();
                                            let approver_level =
                                                autonoetic_types::background::ApprovalLevel::Operator;
                                            let mut options =
                                                autonoetic_gateway::scheduler::ApproveOptions::default();
                                            if let Ok(Some(ref approval)) = store.get_approval(&apr_id) {
                                                if let Some(ref phrase) = approval.confirm_phrase {
                                                    options.confirm_phrase = Some(phrase.clone());
                                                }
                                                if let autonoetic_types::background::ScheduledAction::RevisionPromote {
                                                    added_capabilities,
                                                    broadened_capabilities,
                                                    ..
                                                } = &approval.action
                                                {
                                                    options.acknowledged_capabilities =
                                                        added_capabilities.iter()
                                                            .chain(broadened_capabilities.iter())
                                                            .cloned()
                                                            .collect();
                                                }
                                            }
                                            match autonoetic_gateway::scheduler::approve_request_with_options(
                                                config,
                                                Some(store),
                                                &apr_id,
                                                "chat-tui",
                                                None,
                                                None,
                                                Some(&approver_level),
                                                None,
                                                options,
                                            ) {
                                                Ok(_decision) => {
                                                    app.pending_approval_ids.retain(|id| id != &apr_id);
                                                    app.add_message(
                                                        MessageRole::System,
                                                        format!("Approved: {}", apr_id),
                                                    );
                                                    let pid = app.next_id();
                                                    app.add_pending(pid);
                                                    app.post_approval_pending_ids.push(pid);
                                                    if let Ok(Some(approval)) = store.get_approval(&apr_id) {
                                                        let notify_id = app.next_id();
                                                        let _ = tx.send((
                                                            notify_id,
                                                            ChatOutbound::ApprovalResolved {
                                                                session_id: approval.session_id.clone(),
                                                                root_session_id: approval.root_session_id.clone(),
                                                            },
                                                        ));
                                                        if approval.workflow_id.is_none() || approval.task_id.is_none() {
                                                            let status_id = app.next_id();
                                                            app.pending_session_status_ids.insert(status_id);
                                                            let _ = tx.send((
                                                                status_id,
                                                                ChatOutbound::SessionStatusQuery {
                                                                    session_id: approval.session_id,
                                                                },
                                                            ));
                                                        }
                                                    }
                                                    if let Some(ref mut overlay) = app.pending_overlay {
                                                        overlay.items.remove(idx);
                                                        if overlay.selected >= overlay.items.len() && overlay.selected > 0 {
                                                            overlay.selected -= 1;
                                                        }
                                                        if overlay.items.is_empty() {
                                                            app.pending_overlay = None;
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    app.add_message(
                                                        MessageRole::System,
                                                        format!("Failed to approve: {}", e),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                HandleKeyAction::OverlayAnswerInteraction { index, option_id } => {
                                    if let Some(_store) = gateway_store {
                                        if let Some(PendingItem::Interaction(ui)) = app.pending_overlay.as_ref().and_then(|o| o.items.get(index)) {
                                            let interaction_id = ui.interaction_id.clone();
                                            let resolved_option_id = option_id.clone();
                                            if let Some(ref exec) = execution_for_interactions {
                                                let exec = std::sync::Arc::clone(exec);
                                                tokio::spawn(async move {
                                                    use autonoetic_gateway::interaction_answer::{
                                                        answer_and_orchestrate_resume, InteractionAnswerParams,
                                                    };
                                                    let _ = answer_and_orchestrate_resume(
                                                        &exec,
                                                        InteractionAnswerParams {
                                                            interaction_id: interaction_id.clone(),
                                                            answer_option_id: resolved_option_id,
                                                            answer_text: None,
                                                            answered_by: Some("chat-tui-overlay".to_string()),
                                                            follow_up_message: None,
                                                        },
                                                    )
                                                    .await;
                                                });
                                                app.add_message(
                                                    MessageRole::System,
                                                    format!("Answered interaction: {}", ui.interaction_id),
                                                );
                                                if let Some(ref mut overlay) = app.pending_overlay {
                                                    overlay.items.remove(index);
                                                    if overlay.selected >= overlay.items.len() && overlay.selected > 0 {
                                                        overlay.selected -= 1;
                                                    }
                                                    if overlay.items.is_empty() {
                                                        app.pending_overlay = None;
                                                    }
                                                }
                                            }
                                        }
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
            if app.in_messages_area(mouse.row) {
                app.scroll_messages_up(3);
            }
            true
        }
        crossterm::event::MouseEventKind::ScrollDown => {
            if app.in_messages_area(mouse.row) {
                app.scroll_messages_down(3);
            }
            true
        }
        crossterm::event::MouseEventKind::Down(btn) => {
            if btn == crossterm::event::MouseButton::Left {
                if app.in_messages_area(mouse.row) {
                    let content_row = (mouse.row as usize - 2) + app.effective_scroll_offset();
                    let content_col = (mouse.column as usize).saturating_sub(3);
                    app.selecting = true;
                    app.sel_start = Some((content_row, content_col));
                    app.sel_end = Some((content_row, content_col));
                    app.click_down_screen = Some((mouse.row, mouse.column));
                    true
                } else {
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
                if app.in_messages_area(mouse.row) {
                    let content_row = (mouse.row as usize - 2) + app.effective_scroll_offset();
                    let content_col = (mouse.column as usize).saturating_sub(3);
                    app.sel_end = Some((content_row, content_col));
                    app.selecting = false;
                    let was_click = app
                        .click_down_screen
                        .map_or(false, |(r, c)| r == mouse.row && c == mouse.column);
                    if was_click {
                        let toggled = app.toggle_lifecycle_at_content_row(content_row);
                        if toggled {
                            app.sel_start = None;
                            app.sel_end = None;
                            return true;
                        }
                    }
                    copy_selection_to_clipboard(app);
                } else {
                    app.selecting = false;
                    app.sel_start = None;
                    app.sel_end = None;
                }
                app.click_down_screen = None;
                true
            } else {
                false
            }
        }
        crossterm::event::MouseEventKind::Drag(btn) => {
            if btn == crossterm::event::MouseButton::Left && app.selecting {
                if app.in_messages_area(mouse.row) {
                    let content_row = (mouse.row as usize - 2) + app.effective_scroll_offset();
                    let content_col = (mouse.column as usize).saturating_sub(3);
                    app.sel_end = Some((content_row, content_col));
                }
                true
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
    SubmitInput(String),
    ApproveInline(String),
    PauseSession,
    CancelSession,
    OpenPendingOverlay,
    OverlayApprove(usize),
    OverlayAnswerInteraction { index: usize, option_id: Option<String> },
}

fn handle_overlay_key(
    key: crossterm::event::KeyEvent,
    app: &mut App,
) -> anyhow::Result<HandleKeyAction> {
    let overlay = match app.pending_overlay.as_mut() {
        Some(o) => o,
        None => return Ok(HandleKeyAction::Continue),
    };

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.pending_overlay = None;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if overlay.selected > 0 {
                overlay.selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if overlay.selected + 1 < overlay.items.len() {
                overlay.selected += 1;
            }
        }
        KeyCode::Char('a') => {
            let idx = overlay.selected;
            if let Some(PendingItem::Approval(_)) = overlay.items.get(idx) {
                return Ok(HandleKeyAction::OverlayApprove(idx));
            }
        }
        KeyCode::Char('r') => {
            let idx = overlay.selected;
            if let Some(PendingItem::Approval(_)) = overlay.items.get(idx) {
                app.pending_overlay = None;
                app.add_message(
                    MessageRole::System,
                    "Use `autonoetic gateway approvals reject <id>` to reject.".to_string(),
                );
            }
        }
        KeyCode::Enter => {
            let idx = overlay.selected;
            if let Some(PendingItem::Interaction(_)) = overlay.items.get(idx) {
                return Ok(HandleKeyAction::OverlayAnswerInteraction {
                    index: idx,
                    option_id: None,
                });
            }
        }
        KeyCode::Char(c) if c >= '1' && c <= '9' => {
            let idx = overlay.selected;
            if let Some(PendingItem::Interaction(ui)) = overlay.items.get(idx) {
                let opt_idx = (c as usize) - ('1' as usize);
                if let Some(opt) = ui.options.get(opt_idx) {
                    return Ok(HandleKeyAction::OverlayAnswerInteraction {
                        index: idx,
                        option_id: Some(opt.id.clone()),
                    });
                }
            }
        }
        _ => {}
    }
    Ok(HandleKeyAction::Continue)
}

fn handle_key(
    key: crossterm::event::KeyEvent,
    app: &mut App,
    _tx: &tokio::sync::mpsc::UnboundedSender<(u64, ChatOutbound)>,
) -> anyhow::Result<HandleKeyAction> {
    if app.pending_overlay.is_some() {
        return handle_overlay_key(key, app);
    }

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
                app.clear_prompt_history_browse();
                app.push_prompt_history(msg.clone());
                return Ok(HandleKeyAction::SubmitInput(msg));
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
            app.end_prompt_history_if_browsing();
            if app.cursor_pos < app.input.len() {
                app.input.remove(app.cursor_pos);
            }
        }

        // Type
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.insert_char(c);
        }

        // Recall previous prompts (plain arrows); transcript scroll uses Shift/Ctrl.

        KeyCode::Up => {
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL)
            {
                app.scroll_messages_up(3);
            } else {
                app.prompt_history_up();
            }
        }
        KeyCode::Down => {
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL)
            {
                app.scroll_messages_down(3);
            } else {
                app.prompt_history_down();
            }
        }

        // Inline approval: Ctrl+A approves the latest pending approval
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !app.inline_approvals_enabled {
                app.add_message(
                    MessageRole::System,
                    "Inline approvals are off (`chat.inline_approvals: false`). Set to true in gateway config or use `autonoetic gateway approvals`.".to_string(),
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

        // Open pending approvals/interactions overlay
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Ok(HandleKeyAction::OpenPendingOverlay);
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
    let redacted = action.redact_for_display();
    match &redacted {
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
        ScheduledAction::WebFetch {
            url,
            timeout_secs,
            max_chars,
            detected_hosts,
            ..
        } => {
            let mut v = vec![
                "type: web_fetch".to_string(),
                format!("  url: {}", clamp_chat_field(url)),
            ];
            if let Some(t) = timeout_secs {
                v.push(format!("  timeout_secs: {t}"));
            }
            if let Some(m) = max_chars {
                v.push(format!("  max_chars: {m}"));
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
        ScheduledAction::WebCall {
            url,
            method,
            headers,
            body,
            timeout_secs,
            max_chars,
            detected_hosts,
            ..
        } => {
            let mut v = vec![
                "type: web_call".to_string(),
                format!("  url: {}", clamp_chat_field(url)),
            ];
            if let Some(m) = method {
                v.push(format!("  method: {m}"));
            }
            if let Some(t) = timeout_secs {
                v.push(format!("  timeout_secs: {t}"));
            }
            if let Some(m) = max_chars {
                v.push(format!("  max_chars: {m}"));
            }
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
        ScheduledAction::WebSearch {
            query,
            provider,
            max_results,
            timeout_secs,
            ..
        } => {
            let mut v = vec![
                "type: web_search".to_string(),
                format!("  query: {}", clamp_chat_field(query)),
            ];
            if let Some(p) = provider {
                v.push(format!("  provider: {p}"));
            }
            if let Some(m) = max_results {
                v.push(format!("  max_results: {m}"));
            }
            if let Some(t) = timeout_secs {
                v.push(format!("  timeout_secs: {t}"));
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
fn format_store_approval_card(
    req: &ApprovalRequest,
    approval_instructions: &str,
    enrichment: &[autonoetic_gateway::runtime::human_gate::GateMessage],
) -> String {
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
    if !enrichment.is_empty() {
        lines.push(String::new());
        lines.push("Context:".to_string());
        for msg in enrichment {
            for (i, ln) in msg.content.lines().enumerate() {
                if i == 0 {
                    lines.push(format!("  [{}] {}", msg.sender, clamp_chat_field(ln)));
                } else {
                    lines.push(format!("  {}", clamp_chat_field(ln)));
                }
            }
        }
    }
    if let Some(ref excerpts) = req.code_excerpts {
        if !excerpts.is_empty() {
            lines.push(String::new());
            lines.push(format!("Code files ({}):", excerpts.len()));
            for exc in excerpts {
                let size = if exc.truncated {
                    format!(" ({} bytes, truncated)", exc.size_bytes)
                } else {
                    format!(" ({} bytes)", exc.size_bytes)
                };
                lines.push(format!("  {}{}", exc.file_name, size));
            }
            lines.push("  (use `gateway approvals show <id>` or the interactive TUI to view code)".to_string());
        }
    }
    if let Some(ref risk) = req.risk_summary {
        let mut risk_parts: Vec<String> = Vec::new();
        if risk.host_count > 0 {
            risk_parts.push(format!("{} host(s)", risk.host_count));
        }
        if !risk.dangerous_patterns.is_empty() {
            risk_parts.push(format!("{} risk(s)", risk.dangerous_patterns.len()));
        }
        if let Some(ref v) = risk.auditor_verdict {
            risk_parts.push(format!("auditor: {}", v));
        }
        if !risk_parts.is_empty() {
            lines.push(String::new());
            lines.push(format!("Risk: {}", risk_parts.join(" | ")));
        }
    }
    let inferred_rules = infer_rules_for_action(&req.action);
    if !inferred_rules.is_empty() {
        lines.push(String::new());
        lines.push(autonoetic_gateway::constitution_glossary::format_enforced_rules(&inferred_rules));
    }
    if let Some(ref phrase) = req.confirm_phrase {
        lines.push(String::new());
        lines.push(format!("Confirm phrase: '{}'", phrase));
    }
    lines.push(String::new());
    lines.push(approval_instructions.to_string());
    lines.join("\n")
}

fn infer_rules_for_action(action: &autonoetic_types::background::ScheduledAction) -> Vec<&'static str> {
    match action {
        autonoetic_types::background::ScheduledAction::SandboxExec { .. }
        | autonoetic_types::background::ScheduledAction::WebSearch { .. }
        | autonoetic_types::background::ScheduledAction::WebFetch { .. }
        | autonoetic_types::background::ScheduledAction::WebCall { .. }
        | autonoetic_types::background::ScheduledAction::CredentialPrompt { .. }
        | autonoetic_types::background::ScheduledAction::CredentialRequest { .. } => {
            vec!["P-2.1", "P-2.18"]
        }
        autonoetic_types::background::ScheduledAction::RevisionPromote { .. } => {
            vec!["P-2.16", "P-2.18"]
        }
        autonoetic_types::background::ScheduledAction::SessionEscalate { .. } => {
            vec!["P-2.18"]
        }
        _ => vec!["P-2.18"],
    }
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

    // Rebuild pending_approval_summaries from the full list.
    app.pending_approval_summaries = list
        .iter()
        .map(|req| {
            let action_type = action_summary(&req.action);
            (req.request_id.clone(), action_type.to_string())
        })
        .collect();

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
                    "Resolve with `autonoetic gateway approvals approve {} …` (or enable `chat.inline_approvals` and use Ctrl+A).",
                    req.request_id
                )
            };
            let enrichment = match store.get_gate_messages(&req.request_id) {
                Ok(msgs) => msgs,
                Err(e) => {
                    tracing::debug!(target: "chat", error = %e, request_id = %req.request_id, "Failed to load gate enrichment");
                    Vec::new()
                }
            };
            let card = format_store_approval_card(&req, &detail, &enrichment);
            app.add_rich_card(
                MessageRole::Signal,
                card,
                RichCard::Approval {
                    request: Box::new(req),
                    detail,
                    enrichment,
                },
            );
            announced = true;
        }
    }
    announced
}

/// Short human-readable summary of an action for the right-pane display.
fn action_summary(action: &autonoetic_types::background::ScheduledAction) -> &'static str {
    match action {
        autonoetic_types::background::ScheduledAction::SandboxExec { .. } => "sandbox.exec",
        autonoetic_types::background::ScheduledAction::WebSearch { .. } => "web.search",
        autonoetic_types::background::ScheduledAction::WebFetch { .. } => "web.fetch",
        autonoetic_types::background::ScheduledAction::WebCall { .. } => "web.call",
        autonoetic_types::background::ScheduledAction::CredentialPrompt { .. } => "credential.prompt",
        autonoetic_types::background::ScheduledAction::CredentialRequest { .. } => "credential.request",
        autonoetic_types::background::ScheduledAction::SessionContinue { .. } => "session.continue",
        autonoetic_types::background::ScheduledAction::SessionEscalate { .. } => "session.escalate",
        autonoetic_types::background::ScheduledAction::RevisionPromote { .. } => "revision.promote",
        autonoetic_types::background::ScheduledAction::WriteFile { .. } => "write.file",
        autonoetic_types::background::ScheduledAction::AgentInstall { .. } => "agent.install",
        autonoetic_types::background::ScheduledAction::ProfileShare { .. } => "profile.share",
        autonoetic_types::background::ScheduledAction::LayerMount { .. } => "layer.mount",
        _ => "other",
    }
}

fn refresh_policy_causal_pane(
    app: &mut App,
    store: &autonoetic_gateway::scheduler::gateway_store::GatewayStore,
    root_session_id: &str,
) {
    let events = match store.search_causal_events(
        Some(root_session_id),
        None,
        POLICY_CAUSAL_POLL_LIMIT,
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(target: "chat", error = %e, "Failed to query causal events for policy pane");
            return;
        }
    };
    for ev in events.into_iter().rev() {
        if !autonoetic_types::causal_chain::causal_event_notifies_policy_decision(&ev) {
            continue;
        }
        if !app.seen_causal_policy_event_ids.insert(ev.event_id.clone()) {
            continue;
        }
        let ts: String = ev.timestamp.chars().take(19).collect();
        let agent = if ev.agent_id.is_empty() {
            "unknown".to_string()
        } else {
            ev.agent_id
        };
        let mut line = format!("[{}] {} {} ({})", ts, ev.status, ev.action, agent);
        if let Some(reason) = ev.reason.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let snippet: String = reason.chars().take(48).collect();
            if reason.chars().count() > 48 {
                line.push_str(&format!(" — {}…", snippet));
            } else {
                line.push_str(&format!(" — {}", snippet));
            }
        }
        app.policy_causal_pane.insert(0, line);
    }
    app.policy_causal_pane.truncate(POLICY_CAUSAL_PANE_MAX);
}

/// Check for signals and inject into app. Returns true if signals were processed.
async fn check_signals(
    app: &mut App,
    config: &autonoetic_types::config::GatewayConfig,
    store: Option<&autonoetic_gateway::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<(u64, ChatOutbound)>,
) -> bool {
    let mut processed_any = false;

    // On poll failure, preserve the previous overview instead of zeroing workflow
    // counts / pending interactions. Replacing them with defaults briefly hides the
    // working indicator even when the session is still active. The store-backed
    // pending-approval merge still runs so Ctrl+A stays responsive, and the next
    // 1s tick retries.
    let snapshot = match poll_session_snapshot(
        config,
        store,
        session_id,
        app.session_overview.latest_signal.clone(),
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "chat",
                error = %e,
                "Failed to poll session snapshot, preserving previous overview"
            );
            if let Some(st) = store {
                if merge_gateway_store_pending_approvals(app, config, st, session_id) {
                    processed_any = true;
                }
            }
            return processed_any;
        }
    };

    let root_session_id = snapshot.overview.root_session_id.clone();
    app.active_workbench = snapshot.active_workbench;

    tracing::debug!(target: "chat", session_id = %session_id, root_session_id = %root_session_id, "check_signals: starting");

    // Pending-approval cards from the gateway store include the full `ScheduledAction` (e.g.
    // sandbox command). Workflow `task.awaiting_approval` uses the same `announced_store_approval_ids`
    // set for dedup — if workflow runs first, it "wins" with a one-line card and the rich store
    // card is skipped. Merge store-backed approvals before workflow events so operators see what
    // they are approving.
    if let Some(st) = store {
        if merge_gateway_store_pending_approvals(app, config, st, session_id) {
            processed_any = true;
        }
    }

    let previous_overview = app.session_overview.clone();
    app.session_overview.root_session_id = snapshot.overview.root_session_id.clone();
    app.session_overview.workflow = snapshot.overview.workflow.clone();
    app.session_overview.pending_user_interactions = snapshot.overview.pending_user_interactions;
    // Sync pending_question_summaries for the right panel.
    app.pending_question_summaries = snapshot
        .pending_interactions
        .iter()
        .map(|i| {
            if i.question.len() > 42 {
                format!("{}…", &i.question[..42])
            } else {
                i.question.clone()
            }
        })
        .collect();
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
                        status: t.status.as_str().to_string(),
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
                                            if !should_show_workflow_awaiting_approval_card(app, event)
                                            {
                                                continue;
                                            }
                                            app.session_overview.latest_signal = Some(card.clone());
                                            push_workflow_event_message(
                                                app,
                                                role,
                                                card,
                                                &event.event_type,
                                                event.task_id.as_deref().unwrap_or("-"),
                                                &event
                                                    .payload
                                                    .get("agent_id")
                                                    .and_then(|v| v.as_str())
                                                    .or_else(|| event.agent_id.as_deref())
                                                    .map(|a| format!(" → {}", a))
                                                    .unwrap_or_default(),
                                                &event.payload,
                                            );
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
                                    let show_card =
                                        should_show_workflow_awaiting_approval_card(app, &event);
                                    if show_card {
                                        push_workflow_event_message(
                                            app,
                                            role,
                                            card.clone(),
                                            &event.event_type,
                                            event.task_id.as_deref().unwrap_or("-"),
                                            &event
                                                .payload
                                                .get("agent_id")
                                                .and_then(|v| v.as_str())
                                                .or_else(|| event.agent_id.as_deref())
                                                .map(|a| format!(" → {}", a))
                                                .unwrap_or_default(),
                                            &event.payload,
                                        );
                                        app.session_overview.latest_signal = Some(card.clone());
                                    }

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

                                    // Clear post-approval spinners on task lifecycle events
                                    // that indicate the scheduler resumed (or finished) work.
                                    // Only send a session.status query for non-completion events
                                    // that actually have updated async_results (task.started,
                                    // task.completed, task.failed).  task.approved is excluded
                                    // because the ApprovalResolved notification already
                                    // transitioned async_results to Processing, and querying at
                                    // this point would hit stale data before the scheduler picks
                                    // up the task.
                                    let is_completion_event = event.event_type == "workflow.completed";
                                    if matches!(
                                        event.event_type.as_str(),
                                        "task.started"
                                            | "task.completed"
                                            | "task.failed"
                                            | "task.approved"
                                            | "workflow.completed"
                                    ) && !app.post_approval_pending_ids.is_empty()
                                    {
                                        let ids: Vec<u64> =
                                            app.post_approval_pending_ids.drain(..).collect();
                                        for pid in ids {
                                            app.remove_pending(pid);
                                        }
                                        if !is_completion_event && event.event_type != "task.approved" {
                                            let wf_root_sid = event
                                                .payload
                                                .get("root_session_id")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or(&root_session_id);
                                            app.queried_session_status_for_workflows
                                                .remove(wf_root_sid);
                                            let query_id = app.next_id();
                                            app.pending_session_status_ids.insert(query_id);
                                            let _ = tx.send((
                                                query_id,
                                                ChatOutbound::SessionStatusQuery {
                                                    session_id: wf_root_sid.to_string(),
                                                },
                                            ));
                                        }
                                    }

                                    // When workflow completes, query session.status to surface the
                                    // planner's final assistant_reply (stored in async_results on
                                    // the gateway after WorkflowJoinSatisfied is processed).
                                    if event.event_type == "workflow.completed" {
                                        let wf_root_sid = event
                                            .payload
                                            .get("root_session_id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or(&root_session_id);
                                        if !app
                                            .queried_session_status_for_workflows
                                            .contains(wf_root_sid)
                                        {
                                            app.queried_session_status_for_workflows
                                                .insert(wf_root_sid.to_string());
                                            let query_id = app.next_id();
                                            app.pending_session_status_ids.insert(query_id);
                                            let _ = tx.send((
                                                query_id,
                                                ChatOutbound::SessionStatusQuery {
                                                    session_id: wf_root_sid.to_string(),
                                                },
                                            ));
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
        refresh_policy_causal_pane(app, store, &root_session_id);
        refresh_gate_history(app, store);
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

        let col_to_byte = |col: usize| -> usize {
            let mut col_pos = 0usize;
            for (i, c) in line.char_indices() {
                let c_w = UnicodeWidthStr::width(c.to_string().as_str());
                if col_pos + c_w > col {
                    return i;
                }
                col_pos += c_w;
            }
            line.len()
        };

        if row == top_row && row == bot_row {
            // Single line selection
            let col_s = col_to_byte(top_col);
            let col_e = col_to_byte(bot_col);
            if col_e > col_s {
                selected.push(line[col_s..col_e].to_string());
            }
        } else if row == top_row {
            // First line of multi-line selection
            let col_s = col_to_byte(top_col);
            selected.push(line[col_s..].to_string());
        } else if row == bot_row {
            // Last line of multi-line selection
            let col_e = col_to_byte(bot_col);
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
        build_return_to_agent_wakeup, extract_approval_request_id, extract_structured_approval,
        format_lifecycle_line, format_user_interaction_prompt, format_workflow_event_card,
        parse_slash_command, prepare_return_to_agent_wakeup, ReturnToAgentInput, ReturnToAgentStatus,
        SlashCommand, WorkbenchOverview,
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
    fn test_format_workflow_event_card_task_completed_includes_result_summary() {
        let event = workflow_event(
            "task.completed",
            Some("task-42"),
            serde_json::json!({
                "status": "succeeded",
                "result_summary": "exit=1 stdout=ERR registering agent"
            }),
        );
        let line = format_workflow_event_card(&event).map(|(s, _)| s).expect("event should render");
        assert!(line.contains("Task completed: task-42"));
        assert!(line.contains("Result:"));
        assert!(line.contains("exit=1"));
    }

    #[test]
    fn test_task_completed_lifecycle_stage_gate_fail() {
        let payload = serde_json::json!({
            "agent_outcome": "fail",
            "result_summary": "No test files found"
        });
        let pres =
            autonoetic_types::task_completion::TaskCompletionPresentation::from_event_payload(
                &payload,
                true,
            );
        assert_eq!(pres.lifecycle_stage(), "completed (gate: fail)");
        assert_eq!(
            super::terminal_icon_for_completion(&pres),
            "⚠️"
        );
        assert!(pres.detail_suffix().unwrap_or("").contains("gate: fail"));
    }

    #[test]
    fn test_format_lifecycle_line_gate_fail_icon() {
        let stages = vec![
            "spawned".to_string(),
            "queued".to_string(),
            "started".to_string(),
            "completed (gate: fail)".to_string(),
        ];
        let line = format_lifecycle_line("unit_test_runner.default", "task-866b7287", &stages, None, None, None, false);
        assert!(line.starts_with("⚠️"));
        assert!(line.contains("completed (gate: fail)"));
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
    fn test_parse_slash_command_session_new() {
        assert_eq!(
            parse_slash_command("/session new branch-a").unwrap(),
            SlashCommand::SessionNew(Some("branch-a".to_string()))
        );
        assert_eq!(
            parse_slash_command("/session new").unwrap(),
            SlashCommand::SessionNew(None)
        );
    }

    #[test]
    fn test_parse_slash_command_session_switch() {
        assert_eq!(
            parse_slash_command("/session switch alpha").unwrap(),
            SlashCommand::SessionSwitch("alpha".to_string())
        );
    }

    #[test]
    fn test_parse_slash_command_return_to_agent_default() {
        assert_eq!(
            parse_slash_command("/return").unwrap(),
            SlashCommand::ReturnToAgent {
                force: false,
                message: None
            }
        );
    }

    #[test]
    fn test_parse_slash_command_wb_subcommands() {
        assert_eq!(
            parse_slash_command("/wb").unwrap(),
            SlashCommand::WbStatus
        );
        assert_eq!(
            parse_slash_command("/wb status").unwrap(),
            SlashCommand::WbStatus
        );
        assert_eq!(
            parse_slash_command("/wb diff").unwrap(),
            SlashCommand::WbDiff
        );
        assert_eq!(
            parse_slash_command("/wb reconcile").unwrap(),
            SlashCommand::WbReconcile
        );
        assert_eq!(
            parse_slash_command("/wb discard").unwrap(),
            SlashCommand::WbDiscard
        );
        // Subcommand is case-insensitive.
        assert_eq!(
            parse_slash_command("/wb DIFF").unwrap(),
            SlashCommand::WbDiff
        );
        // Unknown subcommands return an error.
        assert!(parse_slash_command("/wb unknown").is_err());
    }

    #[test]
    fn test_parse_slash_command_return_to_agent_force_flag() {
        assert_eq!(
            parse_slash_command("/return --force").unwrap(),
            SlashCommand::ReturnToAgent {
                force: true,
                message: None
            }
        );
        // Short form flag is accepted too.
        assert_eq!(
            parse_slash_command("/return -f").unwrap(),
            SlashCommand::ReturnToAgent {
                force: true,
                message: None
            }
        );
    }

    #[test]
    fn test_parse_slash_command_return_to_agent_with_note() {
        // Multiple tokens after /return are joined into a single operator note.
        assert_eq!(
            parse_slash_command("/return please look at the auth flow").unwrap(),
            SlashCommand::ReturnToAgent {
                force: false,
                message: Some("please look at the auth flow".to_string())
            }
        );
        // Force flag can appear anywhere in the token list.
        assert_eq!(
            parse_slash_command("/return --force ship it").unwrap(),
            SlashCommand::ReturnToAgent {
                force: true,
                message: Some("ship it".to_string())
            }
        );
    }

    fn make_wakeup_input(
        workbench_id: &str,
        base_artifact_id: &str,
        reconciled: bool,
        modified: Vec<String>,
        added: Vec<String>,
        deleted: Vec<String>,
        note: Option<&str>,
    ) -> ReturnToAgentInput {
        let unsaved_change_count = modified.len() + added.len() + deleted.len();
        ReturnToAgentInput {
            workbench_id: workbench_id.to_string(),
            base_artifact_id: base_artifact_id.to_string(),
            reconciled,
            new_artifact_ref: None,
            new_artifact_id: None,
            operator_note: note.map(|s| s.to_string()),
            unsaved_change_count,
            operator_modified_files: modified,
            operator_added_files: added,
            deleted_files: deleted,
        }
    }

    #[test]
    fn test_build_wakeup_reconciled_no_note() {
        let input = make_wakeup_input(
            "wb-abc",
            "art-base",
            true,
            vec![],
            vec![],
            vec![],
            None,
        );
        let wakeup = build_return_to_agent_wakeup(&input);
        assert!(wakeup.message.contains("wb-abc"));
        assert!(wakeup.message.contains("art-base"));
        assert!(wakeup.message.contains("reconciled"));
        assert!(!wakeup.message.contains("Operator note"));
        let payload = &wakeup.metadata["workbench_reconciled"];
        assert_eq!(payload["event"], "workbench_reconciled");
        assert_eq!(payload["workbench_id"], "wb-abc");
        assert_eq!(payload["base_artifact_id"], "art-base");
        assert_eq!(payload["operator_modified"], false);
    }

    #[test]
    fn test_build_wakeup_unsaved_with_note_includes_modified_files() {
        let input = make_wakeup_input(
            "wb-xyz",
            "art-base",
            false,
            vec!["src/main.rs".to_string()],
            vec!["newfile.txt".to_string()],
            vec!["old.rs".to_string()],
            Some("check the security review"),
        );
        let wakeup = build_return_to_agent_wakeup(&input);
        assert!(wakeup.message.contains("wb-xyz"));
        assert!(wakeup.message.contains("3 unsaved change(s)"));
        assert!(wakeup.message.contains("--force"));
        assert!(wakeup.message.contains("Operator note: check the security review"));
        let payload = &wakeup.metadata["workbench_reconciled"];
        assert_eq!(payload["operator_modified"], true);
        let modified = payload["operator_modified_files"].as_array().unwrap();
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0], "src/main.rs");
        let added = payload["operator_added_files"].as_array().unwrap();
        assert_eq!(added[0], "newfile.txt");
        let deleted = payload["deleted_files"].as_array().unwrap();
        assert_eq!(deleted[0], "old.rs");
    }

    #[test]
    fn test_build_wakeup_active_no_edits_no_force_message() {
        // An active workbench with zero unsaved changes (operator projected
        // then never touched the files) should send a calm "in sync" message
        // and `operator_modified: false` in the payload.
        let input = make_wakeup_input(
            "wb-clean",
            "art-base",
            false,
            vec![],
            vec![],
            vec![],
            None,
        );
        let wakeup = build_return_to_agent_wakeup(&input);
        assert!(wakeup.message.contains("in sync with base artifact"));
        let payload = &wakeup.metadata["workbench_reconciled"];
        assert_eq!(payload["operator_modified"], false);
        assert!(payload.get("operator_modified_files").is_none());
        assert!(payload.get("operator_added_files").is_none());
        assert!(payload.get("deleted_files").is_none());
    }

    #[test]
    fn test_prepare_return_no_workbench() {
        let (status, _body) = prepare_return_to_agent_wakeup(None, None, false, None);
        assert!(matches!(status, ReturnToAgentStatus::NoWorkbench));
    }

    #[test]
    fn test_prepare_return_refuses_unsaved_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let gateway_dir = dir.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
            .unwrap();
        let workspace = dir.path().join("wb-src");
        std::fs::create_dir_all(&workspace).unwrap();
        // Write a base file and modify it to simulate an operator edit.
        let original = b"original content";
        std::fs::write(workspace.join("hello.txt"), original).unwrap();
        use sha2::Digest;
        let base_digest = format!("{:x}", sha2::Sha256::digest(original));
        let meta_dir = workspace.parent().unwrap().join(".autonoetic");
        std::fs::create_dir_all(&meta_dir).unwrap();
        let mut digests = std::collections::HashMap::new();
        digests.insert("hello.txt".to_string(), base_digest);
        std::fs::write(
            meta_dir.join("base_digests.json"),
            serde_json::to_string(&digests).unwrap(),
        )
        .unwrap();
        // Now modify the file.
        std::fs::write(workspace.join("hello.txt"), b"edited content").unwrap();
        let wb = autonoetic_types::workbench::WorkbenchProjection {
            workbench_id: "wb-test-1".to_string(),
            workflow_id: "wf-1".to_string(),
            root_session_id: "root-1".to_string(),
            plan_id: None,
            base_artifact_id: "art-base-1".to_string(),
            base_artifact_canonical_digest: "deadbeef".repeat(8),
            workspace_path: workspace.to_string_lossy().to_string(),
            status: autonoetic_types::workbench::WorkbenchStatus::Active,
            created_by_agent_id: "planner.default".to_string(),
            created_at: "2026-06-01T00:00:00Z".to_string(),
            last_checkpoint_at: None,
            reconciled_at: None,
            discarded_at: None,
        };
        store.save_workbench(&wb).unwrap();

        let overview = WorkbenchOverview {
            workbench_id: "wb-test-1".to_string(),
            status: "active".to_string(),
            base_artifact_id: "art-base-1".to_string(),
            file_count: 1,
            changed_files: 1,
        };
        let (status, _body) =
            prepare_return_to_agent_wakeup(Some(&store), Some(&overview), false, None);
        match status {
            ReturnToAgentStatus::Refused { reason } => {
                assert!(reason.contains("1 unsaved edit"));
                assert!(reason.contains("hello.txt"));
                assert!(reason.contains("--force"));
            }
            other => panic!("expected Refused, got {:?}", other),
        }
    }

    #[test]
    fn test_prepare_return_force_overrides_unsaved_check() {
        let dir = tempfile::tempdir().unwrap();
        let gateway_dir = dir.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
            .unwrap();
        let workspace = dir.path().join("wb-src");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("hello.txt"), b"edited content").unwrap();
        let meta_dir = workspace.parent().unwrap().join(".autonoetic");
        std::fs::create_dir_all(&meta_dir).unwrap();
        let mut digests = std::collections::HashMap::new();
        digests.insert("hello.txt".to_string(), "deadbeef".repeat(8));
        std::fs::write(
            meta_dir.join("base_digests.json"),
            serde_json::to_string(&digests).unwrap(),
        )
        .unwrap();
        let wb = autonoetic_types::workbench::WorkbenchProjection {
            workbench_id: "wb-test-2".to_string(),
            workflow_id: "wf-2".to_string(),
            root_session_id: "root-2".to_string(),
            plan_id: None,
            base_artifact_id: "art-base-2".to_string(),
            base_artifact_canonical_digest: "deadbeef".repeat(8),
            workspace_path: workspace.to_string_lossy().to_string(),
            status: autonoetic_types::workbench::WorkbenchStatus::Active,
            created_by_agent_id: "planner.default".to_string(),
            created_at: "2026-06-01T00:00:00Z".to_string(),
            last_checkpoint_at: None,
            reconciled_at: None,
            discarded_at: None,
        };
        store.save_workbench(&wb).unwrap();

        let overview = WorkbenchOverview {
            workbench_id: "wb-test-2".to_string(),
            status: "active".to_string(),
            base_artifact_id: "art-base-2".to_string(),
            file_count: 1,
            changed_files: 1,
        };
        let (status, body) =
            prepare_return_to_agent_wakeup(Some(&store), Some(&overview), true, Some("ok"));
        match status {
            ReturnToAgentStatus::Ready {
                target_agent_id,
                outbound_message,
                metadata,
            } => {
                assert_eq!(target_agent_id, "planner.default");
                assert!(outbound_message.contains("1 unsaved change(s)"));
                assert!(outbound_message.contains("--force"));
                assert!(outbound_message.contains("Operator note: ok"));
                let payload = &metadata["workbench_reconciled"];
                assert_eq!(payload["operator_modified"], true);
            }
            other => panic!("expected Ready, got {:?}", other),
        }
        assert!(body.contains("Operator note: ok"));
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
