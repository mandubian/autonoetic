//! Interactive Session Room shell (#363) — the ratatui renderer.
//!
//! A scrollable, live-tailing view of the canonical timeline with an altitude
//! dial, squash, drill-down, and conversational gate resolution. P3.b-2 (#392):
//! a **gateway API client** — reads via `session.timeline.list`, resolves gates
//! via `approvals.approve`/`reject` and `interaction.resolve_and_answer`. No
//! direct store access. chat.rs untouched.

use super::channel::{Channel, GateAction, GateKind, GateOption, GateRef, TuiChannel};
use super::client::RoomClient;
use super::markdown;
use super::render::{self, ActorKind, RenderedRow, RowSource, RowSpec, RowTone};
use super::slash::SlashCommand;
use autonoetic_types::principal::Principal;
use autonoetic_types::session_timeline::{
    Altitude, SessionRole, SessionSpawnLineageEntry, SessionTimelineEntry, SessionTimelineListResult,
};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
        EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

/// Spinner frames — a gentle breathing indicator on the in-flight row. The
/// current frame is rotated on each TUI frame tick.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Main-loop frame budget — input is drained at 0 ms; this caps idle spin rate.
const FRAME_MS: u64 = 50;
/// Idle frame budget when no turns are open and no async work is happening.
/// This dramatically lowers CPU by letting the process sleep longer.
const IDLE_FRAME_MS: u64 = 250;
/// How often to pull new timeline events from the gateway when idle (no open
/// turns, no async processing). Keeps CPU low on long completed sessions.
const IDLE_TIMELINE_POLL_MS: u64 = 2000;
/// Timeline poll rate when the session may be active.
const TIMELINE_POLL_MS: u64 = 400;
/// How often to poll `session.status` for async ingest still `processing`.
const SESSION_STATUS_POLL_MS: u64 = 2000;

/// Hard cap on plumbing/tool rows — keeps the list scannable.
const MAX_ROW_LINES: usize = 8;
/// Agent/operator narrative rows may wrap across more lines before folding.
const MAX_NARRATIVE_ROW_LINES: usize = 24;

/// Expanded footer height while composing a multi-line message.
const COMPOSE_PANEL_HEIGHT: u16 = 7;
/// Height of the attention footer: mode hint, pending strip, selected detail.
const FOOTER_HEIGHT: u16 = 3;

/// Seconds the operator has to press `q`/`Ctrl+C` again after arming quit.
const QUIT_ARM_SECS: u64 = 3;
const QUIT_ARM_STATUS: &str = "Quit? press q or Ctrl+C again within 3s — Esc cancels";

/// Seconds the operator has to press Esc again after arming to trigger
/// an emergency stop of the running session.
const ESTOP_ARM_SECS: u64 = 2;
const ESTOP_ARM_STATUS: &str = "Interrupt session? press Esc again within 2s";

fn quit_armed(armed_until: &Option<Instant>) -> bool {
    armed_until
        .filter(|until| Instant::now() < *until)
        .is_some()
}

fn arm_quit(armed_until: &mut Option<Instant>, status: &mut Option<String>) {
    *armed_until = Some(Instant::now() + Duration::from_secs(QUIT_ARM_SECS));
    *status = Some(QUIT_ARM_STATUS.to_string());
}

fn disarm_quit(armed_until: &mut Option<Instant>, status: &mut Option<String>) {
    *armed_until = None;
    if status.as_deref() == Some(QUIT_ARM_STATUS) {
        *status = None;
    }
}

fn estop_armed(armed_until: &Option<Instant>) -> bool {
    armed_until
        .filter(|until| Instant::now() < *until)
        .is_some()
}

fn arm_estop(armed_until: &mut Option<Instant>, status: &mut Option<String>) {
    *armed_until = Some(Instant::now() + Duration::from_secs(ESTOP_ARM_SECS));
    *status = Some(ESTOP_ARM_STATUS.to_string());
}

fn disarm_estop(armed_until: &mut Option<Instant>, status: &mut Option<String>) {
    *armed_until = None;
    if status.as_deref() == Some(ESTOP_ARM_STATUS) {
        *status = None;
    }
}

/// Rows visible in the main timeline list for the current terminal height.
fn main_list_page_step(terminal_height: u16, compose_open: bool) -> usize {
    let chrome = 1 + FOOTER_HEIGHT + if compose_open { COMPOSE_PANEL_HEIGHT } else { 0 };
    terminal_height.saturating_sub(chrome).max(1) as usize
}

/// Compute the scroll offset for the timeline list so the selected row stays
/// visible AND the last row of the list does not scroll off the bottom of the
/// viewport when the cursor moves up from the end.
///
/// Takes `row_heights` (one entry per rendered row, in terminal lines) so
/// multi-line rows (title + preview) are accounted for — a single multi-line
/// row at the bottom can take 2–24 lines, so a row-count-based offset would
/// either hide the last row or leave blank lines at the bottom.
///
/// Pinned-to-bottom rule: the viewport is anchored to the last row, with as
/// many preceding rows as fit packed in above. The cursor is free to move
/// within this window. The moment the cursor moves above the window, the
/// viewport scrolls up to follow, with the cursor as far down in the new
/// window as possible (i.e. the most-recent rows visible alongside it).
///
/// When `prev_offset` is `Some`, the function first checks whether the
/// selected row is already visible in the previous viewport. If so, the
/// viewport stays fixed — the cursor moves freely within it. This avoids
/// re-pinning the cursor to the top of the viewport on every incremental
/// down-arrow move after scrolling up.
fn compute_viewport_offset(
    selected: usize,
    list_height: usize,
    row_heights: &[usize],
    prev_offset: Option<usize>,
) -> usize {
    if list_height == 0 || row_heights.is_empty() {
        return 0;
    }
    let row_count = row_heights.len();
    let total_height: usize = row_heights.iter().sum();
    if total_height <= list_height {
        return 0;
    }

    // When a previous viewport exists, edge-scroll from it in either
    // direction instead of always starting from the bottom. This keeps the
    // viewport stable as the cursor moves within it — no more re-pinning
    // the cursor to the top of the window on every down-arrow.
    if let Some(mut offset) = prev_offset {
        if offset < row_count {
            let mut height = 0usize;
            let mut end = offset;
            while end < row_count && height + row_heights[end] <= list_height {
                height += row_heights[end];
                end += 1;
            }

            // Cursor still inside the previous viewport — nothing to do.
            if selected >= offset && selected < end {
                return offset;
            }

            // Edge-scroll upward (cursor moved above the viewport).
            while selected < offset && offset > 0 {
                offset -= 1;
                height += row_heights[offset];
                while height > list_height {
                    end -= 1;
                    height -= row_heights[end];
                }
            }

            // Edge-scroll downward (cursor moved below the viewport).
            while selected >= end && end < row_count {
                height += row_heights[end];
                end += 1;
                while height > list_height {
                    height -= row_heights[offset];
                    offset += 1;
                }
            }

            return offset;
        }
    }

    // Fallback — bottom-anchored window then edge-scroll up. Used when there
    // is no previous viewport (follow mode, first frame, invalidated state).
    let mut offset = row_count;
    let mut height = 0usize;
    for i in (0..row_count).rev() {
        if height + row_heights[i] > list_height {
            break;
        }
        height += row_heights[i];
        offset = i;
    }
    let mut end = row_count;

    while selected < offset && offset > 0 {
        offset -= 1;
        height += row_heights[offset];
        while height > list_height {
            end -= 1;
            height -= row_heights[end];
        }
    }

    offset
}

/// Map a mouse click's terminal row to a timeline row index.
/// Returns `None` if the click is outside the list area.
fn click_to_row_index(
    click_y: u16,
    list_area_y: u16,
    list_height: usize,
    viewport_offset: usize,
    row_heights: &[usize],
) -> Option<usize> {
    if click_y < list_area_y {
        return None;
    }
    let rel_y = (click_y - list_area_y) as usize;
    if rel_y >= list_height {
        return None;
    }
    let mut acc = 0usize;
    let mut i = viewport_offset;
    while i < row_heights.len() {
        let h = row_heights[i];
        if acc + h > rel_y {
            return Some(i);
        }
        acc += h;
        i += 1;
    }
    None
}

/// Milliseconds within which two left-clicks on the same row count as a double-click.
const DOUBLE_CLICK_MS: u128 = 450;

/// Returns true when this click completes a double-click on the same row.
fn click_opens_detail(
    last: &mut Option<(Instant, usize, u16, u16)>,
    now: Instant,
    row_index: usize,
    column: u16,
    row: u16,
) -> bool {
    const MAX_DRIFT: i16 = 3;
    let is_double = last.is_some_and(|(t, idx, lc, lr)| {
        idx == row_index
            && now.duration_since(t).as_millis() <= DOUBLE_CLICK_MS
            && (column as i16 - lc as i16).abs() <= MAX_DRIFT
            && (row as i16 - lr as i16).abs() <= MAX_DRIFT
    });
    if is_double {
        *last = None;
    } else {
        *last = Some((now, row_index, column, row));
    }
    is_double
}

/// Lines visible in the detail pane for the current terminal height.
fn detail_page_step(terminal_height: u16) -> u16 {
    // header(1) + footer(1) + detail block borders(2)
    terminal_height.saturating_sub(4).max(1)
}

/// Multi-line message editor for compose mode (`i`).
struct ComposeInput {
    buffer: String,
    cursor_pos: usize,
}

impl ComposeInput {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor_pos: 0,
        }
    }

    fn with_prefill(text: &str) -> Self {
        let buffer = text.to_string();
        let cursor_pos = buffer.len();
        Self { buffer, cursor_pos }
    }

    fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    fn insert_str(&mut self, text: &str) {
        let normalized: String = text.chars().filter(|c| *c != '\r').collect();
        if normalized.is_empty() {
            return;
        }
        self.buffer.insert_str(self.cursor_pos, &normalized);
        self.cursor_pos += normalized.len();
    }

    fn delete_before(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let prev = self.buffer[..self.cursor_pos].chars().last().unwrap();
        let len = prev.len_utf8();
        self.cursor_pos -= len;
        self.buffer.remove(self.cursor_pos);
    }

    fn delete_after(&mut self) {
        if self.cursor_pos < self.buffer.len() {
            self.buffer.remove(self.cursor_pos);
        }
    }

    fn cursor_left(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let prev = self.buffer[..self.cursor_pos].chars().last().unwrap();
        self.cursor_pos -= prev.len_utf8();
    }

    fn cursor_right(&mut self) {
        if self.cursor_pos >= self.buffer.len() {
            return;
        }
        let next = self.buffer[self.cursor_pos..].chars().next().unwrap();
        self.cursor_pos += next.len_utf8();
    }

    fn cursor_up(&mut self) {
        let (line, col) = self.line_col();
        if line > 0 {
            self.cursor_pos = self.pos_at_line_col(line - 1, col);
        }
    }

    fn cursor_down(&mut self) {
        let (line, col) = self.line_col();
        if line + 1 < self.line_count() {
            self.cursor_pos = self.pos_at_line_col(line + 1, col);
        }
    }

    fn home(&mut self) {
        let (line, _) = self.line_col();
        self.cursor_pos = self.pos_at_line_col(line, 0);
    }

    fn end(&mut self) {
        let (line, _) = self.line_col();
        self.cursor_pos = self.pos_at_line_col(line, usize::MAX);
    }

    fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        for (i, b) in self.buffer.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        starts
    }

    fn line_count(&self) -> usize {
        self.line_starts().len()
    }

    fn line_col(&self) -> (usize, usize) {
        let starts = self.line_starts();
        let line = starts
            .iter()
            .rposition(|&s| s <= self.cursor_pos)
            .unwrap_or(0);
        let col = self.cursor_pos.saturating_sub(starts[line]);
        (line, col)
    }

    fn pos_at_line_col(&self, line: usize, col: usize) -> usize {
        let starts = self.line_starts();
        let line_start = *starts.get(line).unwrap_or_else(|| starts.last().unwrap_or(&0));
        let line_end = starts
            .get(line + 1)
            .map(|s| s.saturating_sub(1))
            .unwrap_or(self.buffer.len());
        let line_len = line_end.saturating_sub(line_start);
        line_start + col.min(line_len)
    }
}

enum ComposeKeyResult {
    Continue,
    Send(String),
    Cancel,
}

fn handle_compose_key(
    compose: &mut ComposeInput,
    key: &event::KeyEvent,
    clipboard: &mut Option<arboard::Clipboard>,
) -> ComposeKeyResult {
    match key.code {
        KeyCode::Esc => ComposeKeyResult::Cancel,
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            compose.insert_newline();
            ComposeKeyResult::Continue
        }
        KeyCode::Enter => {
            let text = compose.buffer.trim().to_string();
            if text.is_empty() {
                ComposeKeyResult::Cancel
            } else {
                ComposeKeyResult::Send(text)
            }
        }
        KeyCode::Backspace => {
            compose.delete_before();
            ComposeKeyResult::Continue
        }
        KeyCode::Delete => {
            compose.delete_after();
            ComposeKeyResult::Continue
        }
        KeyCode::Left => {
            compose.cursor_left();
            ComposeKeyResult::Continue
        }
        KeyCode::Right => {
            compose.cursor_right();
            ComposeKeyResult::Continue
        }
        KeyCode::Up => {
            compose.cursor_up();
            ComposeKeyResult::Continue
        }
        KeyCode::Down => {
            compose.cursor_down();
            ComposeKeyResult::Continue
        }
        KeyCode::Home => {
            compose.home();
            ComposeKeyResult::Continue
        }
        KeyCode::End => {
            compose.end();
            ComposeKeyResult::Continue
        }
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            paste_clipboard(compose, clipboard);
            ComposeKeyResult::Continue
        }
        KeyCode::Insert if key.modifiers.contains(KeyModifiers::SHIFT) => {
            paste_clipboard(compose, clipboard);
            ComposeKeyResult::Continue
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            copy_clipboard(compose, clipboard);
            ComposeKeyResult::Continue
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            compose.insert_char(c);
            ComposeKeyResult::Continue
        }
        _ => ComposeKeyResult::Continue,
    }
}

fn paste_clipboard(compose: &mut ComposeInput, clipboard: &mut Option<arboard::Clipboard>) {
    let text = clipboard
        .as_mut()
        .and_then(|cb| cb.get_text().ok())
        .unwrap_or_default();
    compose.insert_str(&text);
}

fn copy_clipboard(compose: &ComposeInput, clipboard: &mut Option<arboard::Clipboard>) {
    if let Some(cb) = clipboard.as_mut() {
        let _ = cb.set_text(&compose.buffer);
    }
}

/// Copy `text` to the system clipboard via an OSC 52 terminal escape. This is
/// the fallback when `arboard` has no display server (headless boxes, SSH
/// without X11/Wayland forwarding): the sequence is interpreted by the terminal
/// emulator, so the clipboard lands on the machine running the terminal. When
/// inside tmux, the sequence is wrapped in a DCS passthrough (also requires
/// `set-clipboard on`). Success here means "written to the terminal", not
/// "confirmed stored" — terminals that don't support OSC 52 silently ignore it.
fn copy_via_osc52(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let payload = osc52_sequence(text, std::env::var_os("TMUX").is_some());
    let mut out = std::io::stdout();
    out.write_all(payload.as_bytes())?;
    out.flush()
}

/// Build the OSC 52 clipboard-set escape for `text`. When `in_tmux`, wrap it in
/// a tmux DCS passthrough (doubling every ESC in the inner sequence).
fn osc52_sequence(text: &str, in_tmux: bool) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let inner = format!("\x1b]52;c;{b64}\x07");
    if in_tmux {
        format!("\x1bPtmux;{}\x1b\\", inner.replace('\x1b', "\x1b\x1b"))
    } else {
        inner
    }
}

/// Drill-down pane: raw event metadata or a structured plan review.
struct DetailPane {
    /// Pre-rendered lines (markdown expanded once at open — not every frame).
    rendered: Vec<Line<'static>>,
    /// When set, the pane is a plan review (not event metadata).
    plan_id: Option<String>,
    /// When set, the pane title names the `agent_spawn` target.
    spawn_agent_id: Option<String>,
}

impl DetailPane {
    fn event(lines: Vec<String>, spawn_agent_id: Option<String>) -> Self {
        Self {
            rendered: render_detail_lines(&lines),
            plan_id: None,
            spawn_agent_id,
        }
    }

    fn plan_review(plan_id: String, lines: Vec<String>) -> Self {
        Self {
            rendered: render_detail_lines(&lines),
            plan_id: Some(plan_id),
            spawn_agent_id: None,
        }
    }

    fn is_plan_review(&self) -> bool {
        self.plan_id.is_some()
    }

    fn block_title(&self) -> String {
        if self.is_plan_review() {
            return " plan review ".to_string();
        }
        if let Some(id) = &self.spawn_agent_id {
            return format!(" agent_spawn → {id} ");
        }
        " event detail ".to_string()
    }
}

fn clear_detail(
    detail: &mut Option<DetailPane>,
    scroll: &mut u16,
    h_scroll: &mut u16,
) {
    *detail = None;
    *scroll = 0;
    *h_scroll = 0;
}

struct InfoPanel {
    lines: Vec<String>,
}

impl InfoPanel {
    fn new(lines: Vec<String>) -> Self {
        Self { lines }
    }
}

fn build_info_panel(
    root: &str,
    channel_kind: &str,
    stats: &SessionStats,
    floor: Altitude,
    squash: bool,
    follow: bool,
    show_reasoning: bool,
    row_count: usize,
    checkpoint_count: usize,
    gate: Option<&GateRef>,
    pending_plan_count: usize,
    status: Option<&str>,
    selected_spawn_agent: Option<&str>,
) -> InfoPanel {
    let mut lines = Vec::new();
    let short_id = if root.len() > 32 {
        format!("{}…{}", &root[..12], &root[root.len()-8..])
    } else {
        root.to_string()
    };
    lines.push(format!("  Session    {short_id}"));
    lines.push(format!("  Channel    {channel_kind}"));
    lines.push(String::new());
    if stats.llm_calls > 0 {
        if stats.per_model.len() <= 1 {
            let model_tag = if stats.models.len() == 1 {
                stats.models[0].clone()
            } else {
                format!("{} models", stats.models.len())
            };
            lines.push(format!("  Model      {model_tag}  ({} calls)", stats.llm_calls));
        } else {
            lines.push(format!("  Models     {} total  ({} calls)", stats.models.len(), stats.llm_calls));
            // Per-model breakdown: sort by calls descending
            let mut sorted: Vec<_> = stats.per_model.iter().collect();
            sorted.sort_by(|a, b| b.1.calls.cmp(&a.1.calls));
            for (model, m) in &sorted {
                let err_tag = if m.errors > 0 {
                    format!("  {} err", m.errors)
                } else {
                    String::new()
                };
                lines.push(format!(
                    "    {:<24} {:>4} calls  in {}  out {}{}",
                    model,
                    m.calls,
                    format_tokens(m.input_tokens),
                    format_tokens(m.output_tokens),
                    err_tag,
                ));
            }
        }
        lines.push(format!("  Tokens     in {}   out {}", format_tokens(stats.total_input), format_tokens(stats.total_output)));
        let avg_in = stats.total_input / stats.llm_calls as u64;
        let avg_out = stats.total_output / stats.llm_calls as u64;
        lines.push(format!("  Avg/call   in {}   out {}", format_tokens(avg_in), format_tokens(avg_out)));
        if let Some(pct) = avg_context_pct(stats) {
            let window = stats.context_window.map_or_else(|| String::new(), |w| format_tokens(w as u64));
            lines.push(format!("  Context    {:.0}% used{}", pct, if window.is_empty() { String::new() } else { format!(" (of {window})") }));
        }
    }
    lines.push(String::new());
    lines.push(format!("  Toggles    floor:{}  squash:{}  reasoning:{}  follow:{}",
        floor.as_str(),
        if squash { "on" } else { "off" },
        if show_reasoning { "on" } else { "off" },
        if follow { "●" } else { "○" },
    ));
    lines.push(format!("  Rows       {row_count}"));
    if checkpoint_count > 0 {
        lines.push(format!(
            "  Checkpoints {checkpoint_count}  ([ / ] jump)"
        ));
    }
    lines.push(String::new());
    let mut active = Vec::new();
    if let Some(g) = gate {
        active.push(format!("  ⏸  gate: {} ({:?})", g.id, g.kind));
    }
    if pending_plan_count > 0 {
        active.push(format!("  ⚠  {pending_plan_count} plan(s) pending"));
    }
    if let Some(agent) = selected_spawn_agent {
        active.push(format!("  ↳  agent_spawn → {agent}"));
    }
    if active.is_empty() {
        lines.push("  Active     —".to_string());
    } else {
        lines.push("  Active".to_string());
        for a in active {
            lines.push(a);
        }
    }
    lines.push(String::new());
    lines.push("  Help       /help  (all keys & commands)".to_string());
    if let Some(s) = status {
        lines.push(String::new());
        lines.push(format!("  Status     {s}"));
    }
    InfoPanel::new(lines)
}

fn truncate_id(id: &str, max: usize) -> String {
    if id.len() <= max {
        id.to_string()
    } else {
        format!("{}…{}", &id[..max / 2], &id[id.len() - (max - max / 2 - 1)..])
    }
}

fn build_header(
    root: &str,
    channel_kind: &str,
    stats: &SessionStats,
    gate_count: usize,
    follow: bool,
    floor: Altitude,
    squash: bool,
    width: u16,
) -> String {
    let left = format!(" Session Room [{}] — {}", channel_kind, truncate_id(root, 28));
    let mut right_parts = Vec::new();
    if stats.llm_calls > 0 {
        right_parts.push(format!("{} → {} ●{}", format_tokens(stats.total_input), format_tokens(stats.total_output), stats.llm_calls));
    }
    if gate_count > 0 {
        right_parts.push(format!("⚠{gate_count}"));
    }
    // In-flight async tool calls: `⋯2 spawn,workflow_wait Δ3t`.
    if let Some(chip) = pending_chip(&stats.pending_calls, stats.pending_age_turns) {
        right_parts.push(chip);
    }
    let floor_ind = format!("{}{}", render::altitude_glyph(floor), floor.as_str());
    right_parts.push(floor_ind);
    if !squash {
        right_parts.push("unsquashed".to_string());
    }
    if follow {
        right_parts.push("[following]".to_string());
    }
    let right = right_parts.join("  ");
    let left_w = left.width();
    let right_w = right.width();
    let avail = width as usize;
    if left_w + right_w + 2 >= avail {
        format!("{left} {right}")
    } else {
        let pad = avail - left_w - right_w;
        format!("{}{}{}", left, " ".repeat(pad), right)
    }
}

/// Turns-since-request beyond which a still-pending call is flagged stale (`!`).
const STALE_PENDING_TURNS: u64 = 5;

/// Header chip for in-flight async tool calls: `⋯N name1,name2 Δ3t` (up to two
/// distinct names). The count `N` is total pending calls; the trailing `,…`
/// appears only when there are more *distinct* tool names than shown — repeated
/// calls of a displayed tool (e.g. 3×workflow_wait → `⋯3 workflow_wait`) don't
/// add one. `Δ{n}t` is how many turns the oldest call has been waiting, with a
/// trailing `!` once it exceeds `STALE_PENDING_TURNS`. Empty names are skipped.
/// Returns `None` when nothing is pending.
fn pending_chip(pending: &[String], age_turns: Option<u64>) -> Option<String> {
    if pending.is_empty() {
        return None;
    }
    const MAX_NAMES: usize = 2;
    let n = pending.len();
    let mut distinct: Vec<&str> = Vec::new();
    for name in pending {
        let short = name.as_str();
        if !short.is_empty() && !distinct.contains(&short) {
            distinct.push(short);
        }
    }
    let shown = distinct.len().min(MAX_NAMES);
    let more = if distinct.len() > shown { ",…" } else { "" };
    let age = match age_turns {
        Some(t) if t >= 1 => {
            let stale = if t > STALE_PENDING_TURNS { "!" } else { "" };
            format!(" Δ{t}t{stale}")
        }
        _ => String::new(),
    };
    Some(format!("⋯{n} {}{}{}", distinct[..shown].join(","), more, age))
}

/// Count pending gates from the rendered rows: approval, plan, interaction, escalation.
/// Delegates to `gate_for_entry` so plan gates honour version-keyed resolution
/// (a superseded `plan.pending` vN is no longer counted).
fn count_active_gates(
    entries: &[SessionTimelineEntry],
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> usize {
    entries
        .iter()
        .filter(|e| gate_for_entry(e, resolved, acted).is_some())
        .count()
}

/// Map a plan approval request id (`apr-plan-{plan_id}-vN`) back to the
/// version-keyed plan gate key (`{plan_id}:vN`) so a cancelled plan approval
/// resolves the matching `plan.pending` — even on sessions predating
/// `plan.withdrawn`.
fn plan_gate_key_from_approval_id(request_id: &str) -> Option<String> {
    let rest = request_id.strip_prefix("apr-plan-")?;
    let (plan_id, version) = rest.rsplit_once("-v")?;
    Some(format!("{plan_id}:v{version}"))
}

/// Fold one timeline entry's resolution effect into `resolved`. Shared by the
/// initial fetch and the incremental page loop so both stay consistent.
fn record_timeline_resolution(resolved: &mut HashSet<String>, e: &SessionTimelineEntry) {
    match e.event_type.as_str() {
        "approval.approved" | "approval.rejected" | "approval.cancelled" => {
            if let Some(id) = &e.refs.approval_request_id {
                resolved.insert(id.clone());
                if let Some(key) = plan_gate_key_from_approval_id(id) {
                    resolved.insert(key);
                }
            }
        }
        "plan.approved" | "plan.withdrawn" | "plan.cancelled" | "plan.rejected" => {
            if let Some(key) = plan_gate_key(e) {
                resolved.insert(key);
            }
        }
        _ => {}
    }
}

fn approval_or_interaction_id(e: &SessionTimelineEntry) -> Option<String> {
    if e.event_type == "user.ask.pending" {
        e.payload.as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.get("interaction_id").and_then(|x| x.as_str()).map(String::from))
    } else {
        e.payload.as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.get("request_id").and_then(|x| x.as_str()).map(String::from))
    }
}

struct ApprovalRow {
    id: String,
    kind: &'static str,
    is_pending: bool,
    summary: String,
}

fn collect_approval_rows(
    entries: &[SessionTimelineEntry],
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> Vec<ApprovalRow> {
    let mut rows: Vec<ApprovalRow> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for e in entries.iter().rev() {
        let (kind, id) = match e.event_type.as_str() {
            "escalation.pending" => {
                ("ESCALATION", e.refs.approval_request_id.clone())
            }
            "approval.pending" => {
                ("APPROVAL", e.refs.approval_request_id.clone()
                    .or_else(|| approval_or_interaction_id(e)))
            }
            "plan.pending" => ("PLAN", plan_gate_key(e)),
            "user.ask.pending" => {
                ("ASK", e.refs.interaction_id.clone()
                    .or_else(|| approval_or_interaction_id(e)))
            }
            _ => continue,
        };
        let Some(id) = id else { continue };
        if !seen.insert(id.clone()) { continue; }
        let is_pending = !resolved.contains(&id) && !acted.contains(&id);
        let summary = extract_gate_summary(e);
        rows.push(ApprovalRow { id, kind, is_pending, summary });
    }
    rows.sort_by_key(|r| !r.is_pending);
    rows
}

fn extract_gate_summary(e: &SessionTimelineEntry) -> String {
    let Some(payload) = e.payload.as_deref() else { return String::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else { return String::new() };
    for field in &["reason", "action", "synthesis", "title", "question", "agent_id"] {
        if let Some(s) = v.get(*field).and_then(|x| x.as_str()) {
            return truncate_str(s, 70);
        }
    }
    String::new()
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

fn action_hint_for_gate_kind(kind: GateKind) -> &'static str {
    match kind {
        GateKind::Approval => "y approve · n reject",
        GateKind::Interaction => "Enter/i/r answer",
        GateKind::Plan => "Enter/p review · y approve · n revise",
        GateKind::WikiProposal => "y accept · n reject",
        GateKind::Escalation => "y acknowledge · n reject",
    }
}

fn action_hint_for_kind_str(kind: &str) -> &'static str {
    match kind {
        "APPROVAL" => "y approve · n reject",
        "ASK" => "Enter/i/r answer",
        "PLAN" => "Enter/p review · y approve · n revise",
        "ESCALATION" => "y acknowledge · n reject",
        _ => "y/n",
    }
}

fn build_attention_strip_line(rows: &[ApprovalRow], width: usize) -> Line<'static> {
    let pending: Vec<&ApprovalRow> = rows.iter().filter(|r| r.is_pending).collect();
    let resolved_count = rows.iter().filter(|r| !r.is_pending).count();

    if pending.is_empty() && resolved_count == 0 {
        return Line::from(Span::styled(
            " No pending interactions",
            Style::default().fg(Color::DarkGray),
        ));
    }

    let mut spans = vec![Span::raw(" ")];
    let label = if !pending.is_empty() {
        format!("⚠ {} pending", pending.len())
    } else {
        format!("✓ {} resolved", resolved_count)
    };
    let label_color = if !pending.is_empty() { Color::Yellow } else { Color::Green };
    let label_width = label.width();
    spans.push(Span::styled(
        label,
        Style::default().fg(label_color).add_modifier(Modifier::BOLD),
    ));

    let mut current_width = 1 + label_width;
    let available = width.saturating_sub(1);

    for r in pending.iter().take(6) {
        let sep = " · ";
        let item = format!("{} {}", r.kind, r.summary);
        let item_w = item.width();
        let sep_w = sep.width();
        if current_width + sep_w + item_w > available {
            spans.push(Span::styled(" · …", Style::default().fg(Color::DarkGray)));
            break;
        }
        spans.push(Span::styled(sep.to_string(), Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(item, Style::default().fg(Color::Yellow)));
        current_width += sep_w + item_w;
    }

    Line::from(spans)
}

fn build_attention_detail_line(
    gate: Option<&GateRef>,
    rows: &[ApprovalRow],
    width: usize,
) -> Line<'static> {
    let relevant = gate
        .and_then(|g| rows.iter().find(|r| r.id == g.id))
        .or_else(|| rows.iter().find(|r| r.is_pending))
        .or_else(|| rows.first());

    let Some(r) = relevant else {
        return Line::from(Span::styled(
            " Press A for approvals list · y/n or Enter to act on selected item",
            Style::default().fg(Color::DarkGray),
        ));
    };

    let action_hint = gate
        .map(|g| action_hint_for_gate_kind(g.kind))
        .unwrap_or_else(|| action_hint_for_kind_str(r.kind));
    let state_label = if r.is_pending { "pending" } else { "resolved" };
    let state_color = if r.is_pending { Color::Yellow } else { Color::Green };

    let kind_span = Span::styled(
        format!(" {} ", r.kind),
        Style::default().fg(state_color).add_modifier(Modifier::BOLD),
    );
    let action_span = Span::styled(
        format!("— {} ", action_hint),
        Style::default().fg(Color::DarkGray),
    );
    let state_span = Span::styled(
        format!("[{}]", state_label),
        Style::default().fg(state_color),
    );

    // Width-aware truncation of the summary so the state tag stays visible.
    let overhead = kind_span.content.width() + action_span.content.width() + state_span.content.width() + 1;
    let available = width.saturating_sub(overhead);
    let summary = if r.summary.width() > available {
        truncate_str(&r.summary, available)
    } else {
        r.summary.clone()
    };
    let summary_span = Span::styled(format!("{} ", summary), Style::default().fg(Color::White));

    Line::from(vec![kind_span, summary_span, action_span, state_span])
}

fn build_footer(
    slash: Option<&str>,
    compose: Option<&ComposeInput>,
    input: Option<&GateInput>,
    status: Option<&str>,
    gate: Option<&GateRef>,
    approval_rows: &[ApprovalRow],
    footer_w: usize,
    info_panel: Option<&InfoPanel>,
    turn_hint: Option<String>,
) -> Paragraph<'static> {
    // Line 1: mode-specific content (preserves the old one-line footer behaviour).
    let line1 = if let Some(buf) = slash {
        Line::from(Span::styled(
            format!(" : /{buf}▏   [Enter run · Esc cancel]   {}", super::slash::HELP_TEXT),
            Style::default().fg(Color::Magenta),
        ))
    } else if compose.is_some() {
        Line::from(Span::styled(
            " Enter send · Shift+Enter newline · ←→↑↓ edit · Ctrl+V / Shift+Insert paste (multi-line) · Ctrl+C copy · Esc cancel",
            Style::default().fg(Color::Green),
        ))
    } else if let Some(gi) = input {
        let label = gate_input_label(gi);
        let choices = gi
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| format!("[{}] {}", i + 1, render::one_line(&o.label, 24)))
            .collect::<Vec<_>>()
            .join(" · ");
        let hint = if gi.options.is_empty() {
            "[Enter submit · Esc cancel]".to_string()
        } else if gi.allow_freeform {
            format!("{choices}   [number choose · or type a reply · Esc cancel]")
        } else {
            format!("{choices}   [Enter submit · Esc cancel]")
        };
        let err = status
            .filter(|s| s.starts_with('✗'))
            .map(|s| format!("   {s}"))
            .unwrap_or_default();
        Line::from(Span::styled(
            format!(" {label}: {}▏{err}   {hint}", rendered_input_buffer(gi)),
            Style::default().fg(Color::Cyan),
        ))
    } else if let Some(s) = status {
        let color = if s.starts_with('✗') {
            Color::Red
        } else if s.starts_with('✓') {
            Color::Green
        } else {
            Color::Yellow
        };
        Line::from(Span::styled(format!(" {s}"), Style::default().fg(color)))
    } else {
        let gate_hint = gate.map(|g| TuiChannel.gate_prompt(g)).unwrap_or_default();
        let nav = "q quit · j↓ k↑ · /help · c content · G grants · o artifact · ? info";
        let nav_display = if footer_w < 50 {
            "j↓ k↑ · /help · ?"
        } else if footer_w < 70 {
            "q · j↓ k↑ · /help · o · ?"
        } else {
            nav
        };
        let center = turn_hint.unwrap_or_else(|| "—".to_string());
        let right = if info_panel.is_some() {
            "info: j/k scroll · Esc close".to_string()
        } else if !gate_hint.is_empty() {
            gate_hint
        } else {
            String::new()
        };
        let nav_w = nav_display.width();
        let center_w = center.width();
        let right_w = right.width();
        let total = nav_w + center_w + right_w + 4;
        let text = if total <= footer_w && !right.is_empty() {
            let pad1 = (footer_w - total) / 3;
            let pad2 = footer_w - nav_w - center_w - right_w - pad1;
            format!(" {nav_display}{}{center}{}{right}", " ".repeat(pad1), " ".repeat(pad2))
        } else if total <= footer_w {
            let pad = footer_w - nav_w - center_w;
            format!(" {nav_display}{}{center}", " ".repeat(pad))
        } else {
            let right_part = if right.is_empty() { String::new() } else { format!("  {right}") };
            format!(" {nav_display}{right_part}")
        };
        Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
    };

    let line2 = build_attention_strip_line(approval_rows, footer_w);
    let line3 = build_attention_detail_line(gate, approval_rows, footer_w);

    Paragraph::new(Text::from(vec![line1, line2, line3]))
}

fn fetch_approval_rows(client: &RoomClient, root_session_id: &str) -> Vec<ApprovalRow> {
    let params = serde_json::json!({
        "root_session_id": root_session_id,
        "limit": 500u32,
        // Resolution events (`approval.cancelled`, `plan.withdrawn`) are Normal
        // altitude, gate lifecycle (`plan.pending`, `approval.approved`) is
        // Attention — `normal` captures both while excluding high-volume Detail
        // plumbing that would otherwise crowd out gates under `limit`.
        "min_altitude": "normal",
    });
    let entries: Vec<SessionTimelineEntry> = match rpc(client, "session.timeline.list", params) {
        Ok(v) => v
            .get("events")
            .and_then(|e| e.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| serde_json::from_value(e.clone()).ok())
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut resolved: HashSet<String> = HashSet::new();
    for e in &entries {
        record_timeline_resolution(&mut resolved, e);
    }
    collect_approval_rows(&entries, &resolved, &HashSet::new())
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

struct ArtifactFileEntry {
    name: String,
    alias: String,
}

/// A dependency layer reference shown in the artifact viewer popup.
#[derive(Clone)]
struct ArtifactLayerEntry {
    layer_id: String,
    name: String,
    mount_path: String,
    digest: String,
}

struct ArtifactViewer {
    artifact_id: String,
    artifact_ref: String,
    kind: String,
    files: Vec<ArtifactFileEntry>,
    /// Dependency layers attached to this artifact (rendered above files).
    layers: Vec<ArtifactLayerEntry>,
    selected: usize,
    scroll: u16,
}

struct ArtifactFileView {
    artifact_id: String,
    file_name: String,
    content: String,
    scroll: u16,
}

/// One selectable node in the live session content pane.
/// Sections show a plan, an artifact, a plan step (indented), an artifact file (indented),
/// or a content-store draft.
#[derive(Clone)]
enum LiveContentNode {
    Plan {
        plan_id: String,
        title: String,
        status: String,
        version: u32,
        is_latest: bool,
    },
    PlanStep {
        title: String,
    },
    Artifact {
        artifact_id: String,
        artifact_ref: String,
        kind: String,
        name: String,
    },
    ArtifactFile {
        name: String,
        alias: String,
        artifact_id: String,
        artifact_ref: String,
    },
    /// A dependency layer attached to the preceding `Artifact` node.
    /// Carries back-references like `ArtifactFile` so the `o` handler can act
    /// on it standalone.
    ArtifactLayer {
        layer_id: String,
        name: String,
        mount_path: String,
        digest: String,
        artifact_id: String,
        artifact_ref: String,
    },
    Draft {
        name: String,
        alias: String,
        visibility: String,
    },
}

impl LiveContentNode {
    /// Human-readable label for the node.
    fn label(&self) -> String {
        match self {
            LiveContentNode::Plan { title, status, version, is_latest, .. } => {
                let latest_tag = if *is_latest { " · latest" } else { "" };
                let label = if title.is_empty() {
                    format!("plan v{version}")
                } else {
                    title.clone()
                };
                format!("📋 {} v{}{} [{}]", label, version, latest_tag, status)
            }
            LiveContentNode::PlanStep { title } => {
                format!("  📄 {}", title)
            }
            LiveContentNode::Artifact { name, kind, .. } => {
                format!("📦 {} ({})", name, kind)
            }
            LiveContentNode::ArtifactFile { name, .. } => {
                format!("  📄 {}", name)
            }
            LiveContentNode::ArtifactLayer { name, digest, .. } => {
                format!("  🧱 {} · {}", name, short_digest(digest))
            }
            LiveContentNode::Draft { name, visibility, .. } => {
                let vis_tag = match visibility.as_str() {
                    "private" => " 🔒",
                    "global" => " 🌐",
                    _ => "",
                };
                format!("📝 {}{}", name, vis_tag)
            }
        }
    }
}

/// Short, human-readable form of a digest for compact TUI display
/// (e.g. `sha256:abcd1234…` → `abcd1234…`). Falls back to the raw string
/// when it doesn't start with the `sha256:` prefix.
fn short_digest(digest: &str) -> String {
    let core = digest.strip_prefix("sha256:").unwrap_or(digest);
    if core.len() >= 12 {
        format!("{}…", &core[..12])
    } else {
        core.to_string()
    }
}

/// Live session content pane: a sectioned tree showing plans, artifacts, and drafts.
/// Toggle with `c`, `j`/`k` to navigate, `o` to open the selected item.
#[derive(Clone)]
struct LiveContentPane {
    /// All selectable nodes in order (no section headers — sections are rendered as
    /// separators based on the section_bounds).
    nodes: Vec<LiveContentNode>,
    /// Start index of each section in `nodes`.
    /// e.g. [(0, "Plans"), (3, "Artifacts"), (7, "Drafts"), (10, "")] means
    /// nodes[0..3] are Plans, nodes[3..7] are Artifacts, nodes[7..10] are Drafts.
    sections: Vec<(usize, &'static str)>,
    selected: usize,
    scroll: u16,
/// Plan_id -> set of folded (hidden) older versions. The latest version per
/// plan_id is always shown; older versions are hidden by default and can be
/// toggled with `x` when a plan node is selected.
folded: std::collections::HashMap<String, bool>,
/// artifact_id -> folded (children hidden). Defaults to expanded (false),
/// so artifacts show their files/layers on first open; `x` collapses them.
artifact_folded: std::collections::HashMap<String, bool>,
}

impl LiveContentPane {
    /// Return the node indices that are currently visible given folding state.
    fn visible_indices(&self) -> Vec<usize> {
        let mut visible = Vec::new();
        let mut last_plan_id: Option<String> = None;
        let mut last_plan_was_latest = false;
        let mut last_artifact_id: Option<String> = None;
        for (idx, node) in self.nodes.iter().enumerate() {
            match node {
                LiveContentNode::Plan {
                    plan_id,
                    is_latest,
                    ..
                } => {
                    last_plan_id = Some(plan_id.clone());
                    last_plan_was_latest = *is_latest;
                    last_artifact_id = None;
                    if *is_latest || !self.is_folded(plan_id) {
                        visible.push(idx);
                    }
                }
                LiveContentNode::PlanStep { .. } => {
                    last_artifact_id = None;
                    // Only hide steps under a folded older (non-latest) plan.
                    if let Some(ref pid) = last_plan_id {
                        if !last_plan_was_latest && self.is_folded(pid) {
                            continue;
                        }
                    }
                    visible.push(idx);
                }
                LiveContentNode::Artifact { artifact_id, .. } => {
                    last_plan_id = None;
                    last_artifact_id = Some(artifact_id.clone());
                    // Parent is always visible.
                    visible.push(idx);
                }
                LiveContentNode::ArtifactFile { artifact_id, .. }
                | LiveContentNode::ArtifactLayer { artifact_id, .. } => {
                    last_artifact_id = Some(artifact_id.clone());
                    // Hide a file/layer child when its artifact is folded.
                    if self.is_artifact_folded(artifact_id) {
                        continue;
                    }
                    visible.push(idx);
                }
                _ => {
                    last_plan_id = None;
                    last_artifact_id = None;
                    visible.push(idx);
                }
            }
        }
        visible
    }

    /// Move selection to the nearest visible node at or before the current
    /// selection. Called after folding state changes.
    fn clamp_selection_to_visible(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.selected = 0;
            return;
        }
        if let Some(pos) = visible.iter().position(|&idx| idx >= self.selected) {
            self.selected = visible[pos];
        } else {
            self.selected = *visible.last().unwrap();
        }
    }

    /// Move selection to the next visible node, wrapping at the end.
    fn select_next_visible(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.selected = 0;
            return;
        }
        if let Some(pos) = visible.iter().position(|&idx| idx > self.selected) {
            self.selected = visible[pos];
        } else {
            self.selected = *visible.last().unwrap();
        }
    }

    /// Move selection to the previous visible node, wrapping at the start.
    fn select_prev_visible(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            self.selected = 0;
            return;
        }
        if let Some(pos) = visible.iter().rposition(|&idx| idx < self.selected) {
            self.selected = visible[pos];
        } else {
            self.selected = visible[0];
        }
    }

    /// Toggle the fold for the selected node's parent: older plan versions
    /// (plan_id-keyed) or an artifact's children (artifact_id-keyed).
    fn toggle_fold(&mut self) {
        // Artifact case: an Artifact node directly, or an ArtifactFile /
        // ArtifactLayer child walking back to its nearest preceding Artifact.
        let artifact_id = match self.nodes.get(self.selected) {
            Some(LiveContentNode::Artifact { artifact_id, .. }) => Some(artifact_id.clone()),
            Some(LiveContentNode::ArtifactFile { .. }) | Some(LiveContentNode::ArtifactLayer { .. }) => {
                self.nodes[..=self.selected]
                    .iter()
                    .rev()
                    .find_map(|n| match n {
                        LiveContentNode::Artifact { artifact_id, .. } => Some(artifact_id.clone()),
                        _ => None,
                    })
            }
            _ => None,
        };
        if let Some(aid) = artifact_id {
            let entry = self.artifact_folded.entry(aid).or_insert(false);
            *entry = !*entry;
            self.clamp_selection_to_visible();
            return;
        }
        // Plan case: a Plan node directly, or a PlanStep walking back.
        let plan_id = match self.nodes.get(self.selected) {
            Some(LiveContentNode::Plan { plan_id, .. }) => plan_id.clone(),
            Some(LiveContentNode::PlanStep { .. }) => {
                match self.nodes[..=self.selected]
                    .iter()
                    .rev()
                    .find_map(|n| match n {
                        LiveContentNode::Plan { plan_id, .. } => Some(plan_id.clone()),
                        _ => None,
                    }) {
                    Some(pid) => pid,
                    None => return,
                }
            }
            _ => return,
        };
        let entry = self.folded.entry(plan_id).or_insert(true);
        *entry = !*entry;
        self.clamp_selection_to_visible();
    }

    /// Whether older revisions of `plan_id` are currently folded.
    fn is_folded(&self, plan_id: &str) -> bool {
        self.folded.get(plan_id).copied().unwrap_or(true)
    }

    /// Whether an artifact's children (files/layers) are currently folded.
    /// Defaults to expanded (false) — artifacts are visible on first open.
    fn is_artifact_folded(&self, artifact_id: &str) -> bool {
        self.artifact_folded.get(artifact_id).copied().unwrap_or(false)
    }
}

/// Markdown-aware viewer for one content-store entry (mirror of
/// `ArtifactFileView` but rendered through the markdown pipeline).
struct ContentView {
    name: String,
    /// Content version (SHA-256 handle) being viewed — the anchor for any
    /// operator comment composed against this file.
    handle: String,
    content: String,
    scroll: u16,
}

/// `content.read` for the given draft name; surfaces RPC errors via `status`.
fn open_content_draft(
    client: &RoomClient,
    root_session_id: &str,
    name: &str,
    status: &mut Option<String>,
) -> Option<ContentView> {
    match rpc(
        client,
        "content.read",
        serde_json::json!({
            "session_id": root_session_id,
            "name": name,
        }),
    ) {
        Ok(v) => {
            if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                let handle = v
                    .get("handle")
                    .and_then(|h| h.as_str())
                    .unwrap_or("")
                    .to_string();
                Some(ContentView {
                    name: name.to_string(),
                    handle,
                    content: content.to_string(),
                    scroll: 0,
                })
            } else {
                *status = Some("content.read: no content field in response".to_string());
                None
            }
        }
        Err(e) => {
            *status = Some(format!("content read failed: {e}"));
            None
        }
    }
}

/// Format a byte count as a compact human-readable string (e.g. "1.2 MiB").
fn humanize_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// Build the detail-pane lines for a `artifact.layer_inspect` response.
/// `name`/`mount_path`/`digest` come from the node (the list_files view); the
/// richer manifest fields (file_count, size_bytes, resolved_packages,
/// approval_scope, files) come from the RPC response `v`.
fn format_layer_inspect_lines(
    layer_id: &str,
    name: &str,
    mount_path: &str,
    digest: &str,
    v: &serde_json::Value,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("🧱 Layer: {} [{}]", name, layer_id));
    lines.push(String::new());
    lines.push(format!("  digest:      {}", digest));
    if !mount_path.is_empty() {
        lines.push(format!("  mount_path:  {}", mount_path));
    }
    if let Some(file_count) = v.get("file_count").and_then(|x| x.as_u64()) {
        lines.push(format!("  file_count:  {}", file_count));
    }
    if let Some(size_bytes) = v.get("size_bytes").and_then(|x| x.as_u64()) {
        lines.push(format!("  size:        {} ({})", humanize_bytes(size_bytes), size_bytes));
    }
    if let Some(created_at) = v.get("created_at").and_then(|x| x.as_str()) {
        lines.push(format!("  created_at:  {}", created_at));
    }

    // Resolved packages (provenance) — only present for dependency layers.
    if let Some(packages) = v.get("resolved_packages").and_then(|x| x.as_array()) {
        if !packages.is_empty() {
            lines.push(String::new());
            lines.push(format!("Resolved packages ({}):", packages.len()));
            for pkg in packages {
                let pname = pkg.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                let pver = pkg.get("version").and_then(|x| x.as_str()).unwrap_or("?");
                lines.push(format!("  · {} {}", pname, pver));
            }
        }
    }

    // Approval scope — only present for layers captured with network access.
    if let Some(scope) = v.get("approval_scope") {
        if !scope.is_null() {
            lines.push(String::new());
            lines.push("Approval scope:".to_string());
            if let Some(hosts) = scope.get("approved_hosts").and_then(|x| x.as_array()) {
                let host_list: Vec<&str> = hosts.iter().filter_map(|h| h.as_str()).collect();
                lines.push(format!("  approved_hosts: {}", host_list.join(", ")));
            }
            if let Some(built_by) = scope.get("built_by_agent_id").and_then(|x| x.as_str()) {
                lines.push(format!("  built_by_agent_id: {}", built_by));
            }
            if let Some(captured_at) = scope.get("captured_at").and_then(|x| x.as_str()) {
                lines.push(format!("  captured_at: {}", captured_at));
            }
        }
    }

    // Capped file listing streamed from the archive (no extraction).
    if let Some(files) = v.get("files").and_then(|x| x.as_array()) {
        lines.push(String::new());
        let total = v.get("files_total").and_then(|x| x.as_u64()).unwrap_or(files.len() as u64);
        let truncated = v.get("files_truncated").and_then(|x| x.as_bool()).unwrap_or(false);
        let header = if truncated {
            format!("Files (showing {}, {} total):", files.len(), total)
        } else {
            format!("Files ({}):", total)
        };
        lines.push(header);
        for f in files {
            let path = f.get("path").and_then(|x| x.as_str()).unwrap_or("?");
            let size = f.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
            lines.push(format!("  {} ({})", path, humanize_bytes(size)));
        }
    } else if let Some(err) = v.get("files_error").and_then(|x| x.as_str()) {
        lines.push(String::new());
        lines.push(format!("Files: unavailable ({})", err));
    }

    lines
}

/// Open the selected node in the live content pane.
/// Shared by the Enter and `o` key handlers.
/// Returns `true` if a view/detail was opened (and the popup should be closed).
fn open_content_pane_node(
    pane: &LiveContentPane,
    idx: usize,
    client: &RoomClient,
    root_session_id: &str,
    detail: &mut Option<DetailPane>,
    detail_scroll: &mut u16,
    detail_h_scroll: &mut u16,
    artifact_viewer: &mut Option<ArtifactViewer>,
    artifact_file_view: &mut Option<ArtifactFileView>,
    content_view: &mut Option<ContentView>,
    status: &mut Option<String>,
    live_content_pane: &mut Option<LiveContentPane>,
) -> bool {
    let idx = idx.min(pane.nodes.len().saturating_sub(1));
    if let Some(ref node) = pane.nodes.get(idx) {
        match node {
            LiveContentNode::Plan { .. } | LiveContentNode::PlanStep { .. } => {
                let pid = match node {
                    LiveContentNode::Plan { plan_id, .. } => Some(plan_id.clone()),
                    _ => {
                        pane.nodes[..=idx].iter().rev().find_map(|n| match n {
                            LiveContentNode::Plan { plan_id, .. } => Some(plan_id.clone()),
                            _ => None,
                        })
                    }
                };
                if let Some(pid) = pid {
                    if let Ok(v) = rpc(client, "planframes.get", serde_json::json!({ "plan_id": pid })) {
                        if let Some(plan) = v.get("plan") {
                            if let Ok(frame) = serde_json::from_value::<autonoetic_types::plan_frame::PlanFrame>(plan.clone()) {
                                let lines = format_plan_frame_lines(&frame, false);
                                *detail = Some(DetailPane::plan_review(frame.plan_id.clone(), lines));
                                *detail_scroll = 0;
                                *detail_h_scroll = 0;
                                *status = Some("plan detail · Esc close".to_string());
                                *live_content_pane = None;
                                return true;
                            }
                        }
                    }
                }
            }
            LiveContentNode::Artifact { artifact_ref, .. } => {
                let result = rpc(client, "artifact.list_files", serde_json::json!({
                    "artifact_ref": artifact_ref,
                    "session_id": root_session_id,
                }));
                match result {
                    Ok(v) => {
                        let artifact_id = v.get("artifact_id").and_then(|a| a.as_str()).unwrap_or("").to_string();
                        let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string();
                        let files: Vec<ArtifactFileEntry> = v
                            .get("files")
                            .and_then(|f| f.as_array())
                            .map(|arr| {
                                arr.iter().filter_map(|f| {
                                    Some(ArtifactFileEntry {
                                        name: f.get("name")?.as_str()?.to_string(),
                                        alias: f.get("alias").and_then(|a| a.as_str()).unwrap_or("").to_string(),
                                    })
                                }).collect()
                            })
                            .unwrap_or_default();
                        let layers: Vec<ArtifactLayerEntry> = v
                            .get("layers")
                            .and_then(|l| l.as_array())
                            .map(|arr| {
                                arr.iter().filter_map(|l| {
                                    let layer_id = l.get("layer_id")?.as_str()?.to_string();
                                    Some(ArtifactLayerEntry {
                                        layer_id,
                                        name: l.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                        mount_path: l.get("mount_path").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                        digest: l.get("digest").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                    })
                                }).collect()
                            })
                            .unwrap_or_default();
                        if files.is_empty() && layers.is_empty() {
                            *status = Some("no files or layers in artifact".to_string());
                        } else {
                            *artifact_viewer = Some(ArtifactViewer {
                                artifact_id,
                                artifact_ref: artifact_ref.clone(),
                                kind,
                                files,
                                layers,
                                selected: 0,
                                scroll: 0,
                            });
                            *status = Some("artifact files · j/k navigate · o open · Esc close".to_string());
                            *live_content_pane = None;
                            return true;
                        }
                    }
                    Err(e) => *status = Some(format!("artifact list failed: {e}")),
                }
            }
            LiveContentNode::ArtifactFile { artifact_ref, name, .. } => {
                let result = rpc(client, "artifact.read_file", serde_json::json!({
                    "artifact_ref": artifact_ref,
                    "file_name": name,
                    "session_id": root_session_id,
                }));
                match result {
                    Ok(v) => {
                        if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                            *artifact_file_view = Some(ArtifactFileView {
                                artifact_id: artifact_ref.clone(),
                                file_name: name.clone(),
                                content: content.to_string(),
                                scroll: 0,
                            });
                            *live_content_pane = None;
                            return true;
                        } else {
                            *status = Some("artifact.read_file: no content field".to_string());
                        }
                    }
                    Err(e) => *status = Some(format!("artifact read failed: {e}")),
                }
            }
            LiveContentNode::ArtifactLayer { layer_id, name, mount_path, digest, .. } => {
                // Inspect the layer's manifest + (capped) file listing, then
                // render into a detail pane. No extraction — files are streamed
                // from the compressed archive by the gateway.
                match rpc(client, "artifact.layer_inspect", serde_json::json!({
                    "layer_id": layer_id,
                    "include_files": true,
                })) {
                    Ok(v) => {
                        let lines = format_layer_inspect_lines(
                            layer_id, name, mount_path, digest, &v,
                        );
                        *detail = Some(DetailPane::event(lines, None));
                        *detail_scroll = 0;
                        *detail_h_scroll = 0;
                        *status = Some("layer detail · Esc close".to_string());
                        *live_content_pane = None;
                        return true;
                    }
                    Err(e) => *status = Some(format!("layer inspect failed: {e}")),
                }
            }
            LiveContentNode::Draft { name, .. } => {
                *content_view = open_content_draft(client, root_session_id, name, status);
                if content_view.is_some() {
                    *live_content_pane = None;
                    return true;
                }
            }
        }
    }
    false
}

/// Extract artifact_ref from a timeline entry, if it has one.
/// Returns an `ar.*` or `art_*` ref that the gateway can resolve.
fn artifact_ref_for_entry(entry: &SessionTimelineEntry) -> Option<String> {
    if let Some(ref aid) = entry.refs.artifact_id {
        return Some(aid.clone());
    }
    if entry.event_type == "tool.completed" {
        if let Some(ref payload) = entry.payload {
            if let Ok(p) = serde_json::from_str::<serde_json::Value>(payload) {
                let tool_name = p.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
                if tool_name.starts_with("artifact") {
                    if let Some(result_str) = p.get("result").and_then(|v| v.as_str()) {
                        if let Ok(result) = serde_json::from_str::<serde_json::Value>(result_str) {
                            if let Some(aref) = result.get("artifact_ref").and_then(|v| v.as_str()) {
                                return Some(aref.to_string());
                            }
                            if let Some(aid) = result.get("artifact_id").and_then(|v| v.as_str()) {
                                return Some(aid.to_string());
                            }
                        }
                    }
                    if let Some(preview) = p.get("args_preview").and_then(|v| v.as_str()) {
                        if preview.starts_with("ar.") || preview.starts_with("art_") {
                            return Some(preview.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// An in-flight operator decision — captures an optional motivation (approvals,
/// §3.5) or the answer (interactions) before committing. `GateRef`, `GateKind`,
/// and `GateAction` are the channel-neutral primitives, shared from
/// [`super::channel`].
struct GateInput {
    action: GateAction,
    id: String,
    buffer: String,
    /// Pre-digested choices for an interaction answer (empty for approvals and
    /// option-less asks). A number key picks one → `answer_option_id`.
    options: Vec<GateOption>,
    /// Whether free-text is accepted alongside any options (interactions).
    allow_freeform: bool,
    /// True after the operator selected the synthetic "Give more details" option.
    /// This lets them type a free-text follow-up even when the original payload
    /// was choice-only, without claiming the underlying interaction allows freeform.
    details_mode: bool,
    /// Rejections always require motivation (§O). Approvals may require it for
    /// elevated/external actions — the gateway enforces that on submit.
    motivation_required: bool,
    /// R++4: destructive approval classes require typing this phrase exactly
    /// (case-insensitive) instead of optional motivation.
    required_confirm_phrase: Option<String>,
    /// R++2: `revision_promote` approvals — auto-filled from the timeline payload.
    acknowledged_capabilities: Vec<String>,
    /// CredentialPrompt approvals: secret fields to collect (empty = not a cred prompt).
    /// Populated from `approvals.inspect` when the approval action is `credential_prompt`.
    secret_fields: Vec<autonoetic_types::agent::SecretFieldSpec>,
    /// Collected secret values, parallel to `secret_fields`. Length = fields completed.
    secret_values: Vec<String>,
    /// True while collecting secrets (Phase 1). The `buffer` holds the current field's input.
    secret_phase: bool,
}

/// A gate RPC that is running in the background so the TUI event loop stays
/// responsive while waiting for the gateway.
struct PendingGateResolve {
    /// The gate the operator committed; restored on error so they can retry.
    /// Wrapped in `Option` so `poll_pending_gate` can take ownership on error.
    gi: Option<GateInput>,
    /// Shared result cell populated by the async task; checked each frame.
    result: Arc<StdMutex<Option<Result<String, String>>>>,
}

/// Blocking popup for operator gates — auto-opens when a new approval, question,
/// plan, or escalation appears so the operator does not have to hunt the timeline.
struct GateModal {
    gate: GateRef,
    scroll: u16,
    peek_timeline: bool,
    inspect_lines: Vec<String>,
    /// For plan gates, the plan version the inspect_lines were fetched from.
    /// Refetch when the live pending revision changes (e.g. v1 → v2 amend).
    plan_version: Option<u64>,
}

struct ApprovalsPopup {
    selected: usize,
    scroll: u16,
    rows: Vec<ApprovalRow>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Grants panel (`G`) — operator live view of all active grants for the root
// session (session-approval + egress-declassification) plus the current egress
// taint, with per-row revoke. Mirrors the approvals-popup pattern. Data comes
// from `grants.list`; revoke goes through `grants.revoke` (grant_id/by-id).
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum GrantKind {
    SessionApproval,
    EgressDeclassification,
}

impl GrantKind {
    fn rpc_str(self) -> &'static str {
        match self {
            GrantKind::SessionApproval => "session_approval",
            GrantKind::EgressDeclassification => "egress_declassification",
        }
    }
    fn label(self) -> &'static str {
        match self {
            GrantKind::SessionApproval => "approval",
            GrantKind::EgressDeclassification => "declass",
        }
    }
}

/// One normalized grant row displayed by the panel. Carries everything needed
/// to revoke it by id (the primary path) — `host` is kept only for display.
struct GrantRow {
    kind: GrantKind,
    id: i64,
    /// Short summary of WHAT the grant widens (hosts / target / sink).
    summary: String,
    /// Provenance line: scope · by <granted_by> [exp <expires>].
    detail: String,
}

struct GrantsPanel {
    selected: usize,
    scroll: u16,
    rows: Vec<GrantRow>,
    /// Current root-session egress taint display name (None ⇒ unrestricted).
    taint: Option<String>,
    /// When Some, the selected row is armed for a confirm-revoke (`r` again).
    pending_revoke: Option<()>,
}

impl GrantsPanel {
    fn clamp_selection(&mut self) {
        let max = self.rows.len().saturating_sub(1);
        if self.selected > max {
            self.selected = max;
        }
    }
}

/// Render a session-approval `Vec<GrantTarget>` to a compact string. Mirrors
/// the `gateway grants list` CLI formatting (background.rs has no shared impl).
fn render_grant_targets(targets: &[serde_json::Value]) -> String {
    let parts: Vec<String> = targets
        .iter()
        .filter_map(|t| {
            // Each target serializes (from SessionApprovalGrant.targets) as:
            // {"kind":"exact_host","value":"x"} etc.
            let kind = t.get("kind")?.as_str()?;
            let value = t.get("value").and_then(|v| v.as_str()).unwrap_or("");
            Some(match kind {
                "any" => "*".to_string(),
                "exact_host" => value.to_string(),
                "host_suffix" => format!("*.{value}"),
                "host_and_port" => value.to_string(),
                "url_prefix" => value.to_string(),
                other => format!("{other}:{value}"),
            })
        })
        .collect();
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(", ")
    }
}

/// Render an egress-declassification target (kind/value pair) + `allowed_sink`.
fn render_declass_target(target_kind: &str, target_value: &str, allowed_sink: &str) -> String {
    format!("{target_kind}:{target_value} → {allowed_sink}")
}

/// Extract (kind, value) from a declassification target JSON value.
fn declass_target_kv(target: Option<&serde_json::Value>) -> (String, String) {
    let t = match target {
        Some(t) => t,
        None => return ("?".to_string(), "?".to_string()),
    };
    let kind = t
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let value = t
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    (kind, value)
}

/// Build the provenance detail line from a grant JSON object: scope · by X [exp Y].
fn format_grant_detail(g: &serde_json::Value) -> String {
    let scope = g
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let granted_by = g
        .get("granted_by")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let mut s = format!("{scope} · by {granted_by}");
    if let Some(exp) = g.get("expires_at").and_then(|v| v.as_str()) {
        if !exp.is_empty() {
            s.push_str(&format!(" · exp {exp}"));
        }
    }
    s
}

/// Poll `grants.list` and fold into display rows + the taint label. Returns
/// `(rows, taint)`. Errors are swallowed (empty rows) — the caller surfaces a
/// status message — so a gateway blip never crashes the UI. Mirrors
/// `fetch_approval_rows`.
fn fetch_grant_rows(client: &RoomClient, root_session_id: &str) -> (Vec<GrantRow>, Option<String>) {
    let params = serde_json::json!({ "root_session_id": root_session_id });
    let value = match rpc(client, "grants.list", params) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(target: "room", error = %e, "grants.list failed");
            return (Vec::new(), None);
        }
    };

    let mut rows: Vec<GrantRow> = Vec::new();

    // Session approval grants.
    if let Some(grants) = value.get("session_approval_grants").and_then(|v| v.as_array()) {
        for g in grants {
            let id = g.get("id").and_then(|v| v.as_i64()).unwrap_or(-1);
            let targets = g
                .get("targets")
                .and_then(|v| v.as_array())
                .map(|a| a.to_vec())
                .unwrap_or_default();
            let summary = render_grant_targets(&targets);
            let detail = format_grant_detail(g);
            rows.push(GrantRow {
                kind: GrantKind::SessionApproval,
                id,
                summary,
                detail,
            });
        }
    }

    // Egress declassification grants.
    if let Some(grants) = value
        .get("egress_declassification_grants")
        .and_then(|v| v.as_array())
    {
        for g in grants {
            let id = g.get("id").and_then(|v| v.as_i64()).unwrap_or(-1);
            let (tkind, tvalue) = declass_target_kv(g.get("target"));
            let sink = g
                .get("allowed_sink")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let summary = render_declass_target(&tkind, &tvalue, sink);
            let detail = format_grant_detail(g);
            rows.push(GrantRow {
                kind: GrantKind::EgressDeclassification,
                id,
                summary,
                detail,
            });
        }
    }

    // Taint: null ⇒ unrestricted. Otherwise it is a JSON array of sink strings;
    // surface the count (mirrors `label_display_name`'s restricted(N)).
    let taint = value.get("current_taint").and_then(|v| {
        if v.is_null() {
            None
        } else if let Some(arr) = v.as_array() {
            Some(format!("restricted({} sinks)", arr.len()))
        } else {
            Some("restricted".to_string())
        }
    });

    (rows, taint)
}

fn gate_modal_kind(gate: &GateRef) -> bool {
    matches!(
        gate.kind,
        GateKind::Approval
            | GateKind::WikiProposal
            | GateKind::Escalation
            | GateKind::Interaction
            | GateKind::Plan
    )
}

/// Newest unresolved operator gate that should block the session (plans now use
/// the same blocking GateModal as other critical gates).
fn newest_blocking_gate_event(
    entries: &[SessionTimelineEntry],
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> Option<(GateRef, String)> {
    for e in entries.iter().rev() {
        if let Some(gate) = gate_for_entry(e, resolved, acted) {
            if gate_modal_kind(&gate) {
                return Some((gate, e.event_id.clone()));
            }
        }
    }
    None
}

fn gate_inspect_detail_lines(value: &serde_json::Value) -> Vec<String> {
    let field = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut lines = Vec::new();
    if let Some(action) = field("action") {
        lines.push(format!("  action: {action}"));
    }
    if let Some(level) = field("approval_level") {
        lines.push(format!("  level: {level}"));
    }
    match field("action").as_deref() {
        Some("session_escalate") => {
            if let Some(agent) = field("requested_by_agent_id") {
                lines.push(format!("  requested by: {agent}"));
            }
            if let Some(u) = field("urgency") {
                lines.push(format!("  urgency: {u}"));
            }
            if let Some(reason) = field("reason").or_else(|| field("summary")) {
                lines.push("  reason:".to_string());
                for line in render::wrap_display_lines(&reason, 76) {
                    lines.push(format!("    {line}"));
                }
            }
            if let Some(ctx) = field("context") {
                lines.push("  context:".to_string());
                for line in render::wrap_display_lines(&ctx, 76) {
                    lines.push(format!("    {line}"));
                }
            }
            if let Some(actions) = value.get("suggested_actions").and_then(|v| v.as_array()) {
                let joined: Vec<_> = actions.iter().filter_map(|v| v.as_str()).collect();
                if !joined.is_empty() {
                    lines.push(format!("  suggested: {}", joined.join(" · ")));
                }
            }
        }
        Some("revision_promote") => {
            if let Some(agent) = field("agent_id") {
                lines.push(format!("  agent: {agent}"));
            }
            if let Some(rev) = field("revision_id") {
                lines.push(format!("  revision: {}", render::one_line(&rev, 120)));
            }
            if let Some(summary) = field("summary") {
                lines.push("  about:".to_string());
                for line in render::wrap_display_lines(&summary, 76) {
                    lines.push(format!("    {line}"));
                }
            }
        }
        Some("credential_prompt") => {
            if let Some(service) = field("service") {
                lines.push(format!("  service: {service}"));
            }
            if let Some(cred_id) = field("credential_id") {
                lines.push(format!("  credential: {}", render::one_line(&cred_id, 120)));
            }
            if let Some(message) = field("message") {
                lines.push("  prompt:".to_string());
                for line in render::wrap_display_lines(&message, 76) {
                    lines.push(format!("    {line}"));
                }
            }
            // secret_fields is an array of {name, label, masked} — surface the
            // human-readable labels so the operator knows what they'll be asked
            // for before pressing 'y'.
            if let Some(fields) = value.get("secret_fields").and_then(|v| v.as_array()) {
                let labels: Vec<String> = fields.iter().filter_map(|f| {
                    f.get("label").and_then(|l| l.as_str()).map(String::from)
                        .or_else(|| f.get("name").and_then(|n| n.as_str()).map(String::from))
                }).collect();
                if !labels.is_empty() {
                    lines.push(format!("  secrets required: {}", labels.join(", ")));
                }
            }
            lines.push("  🔒 values go to the encrypted vault — the LLM never sees them".to_string());
        }
        _ => {
            if let Some(summary) = field("summary") {
                lines.push("  about:".to_string());
                for line in render::wrap_display_lines(&summary, 76) {
                    lines.push(format!("    {line}"));
                }
            }
            if let Some(risk) = field("risk_summary") {
                lines.push(format!("  risk: {}", render::one_line(&risk, 120)));
            }
        }
    }
    lines
}

fn fetch_gate_inspect_detail(client: &RoomClient, request_id: &str) -> Vec<String> {
    rpc(
        client,
        "approvals.inspect",
        serde_json::json!({ "request_id": request_id }),
    )
    .map(|v| gate_inspect_detail_lines(&v))
    .unwrap_or_default()
}

/// Fetch human-readable detail lines for a plan gate. Used by the blocking
/// GateModal when a plan.pending event appears.
fn fetch_plan_detail(client: &RoomClient, root_session_id: &str, plan_id: &str) -> Vec<String> {
    let params = serde_json::json!({ "root_session_id": root_session_id });
    match rpc(client, "planframes.list_pending", params) {
        Ok(value) => match serde_json::from_value::<
            autonoetic_types::plan_frame::PlanFramesListPendingResult,
        >(value)
        {
            Ok(parsed) => parsed
                .plans
                .into_iter()
                .find(|p| p.plan_id == plan_id)
                .map(|p| format_plan_frame_lines(&p, true))
                .unwrap_or_else(|| {
                    vec![format!(
                        "(plan {plan_id} is no longer pending — it may have been approved or withdrawn)"
                    )]
                }),
            Err(e) => vec![format!("✗ malformed planframes.list_pending response: {e}")],
        },
        Err(e) => vec![format!("✗ planframes.list_pending failed: {e}")],
    }
}

fn gate_detail_for_modal(
    client: &RoomClient,
    root_session_id: &str,
    gate: &GateRef,
) -> Vec<String> {
    match gate.kind {
        GateKind::Plan => fetch_plan_detail(client, root_session_id, &gate.id),
        _ => fetch_gate_inspect_detail(client, &gate.id),
    }
}

fn gate_entry_for_ref<'a>(
    entries: &'a [SessionTimelineEntry],
    gate: &GateRef,
) -> Option<&'a SessionTimelineEntry> {
    entries.iter().rev().find(|e| match gate.kind {
        GateKind::Approval | GateKind::WikiProposal => {
            e.event_type == "approval.pending"
                && approval_id_for(e).as_deref() == Some(gate.id.as_str())
        }
        GateKind::Escalation => {
            (e.event_type == "escalation.pending"
                && e.refs.approval_request_id.as_deref() == Some(gate.id.as_str()))
                || (e.event_type == "approval.pending"
                    && approval_id_for(e).as_deref() == Some(gate.id.as_str()))
        }
        GateKind::Interaction => {
            e.event_type == "user.ask.pending"
                && interaction_id_for(e).as_deref() == Some(gate.id.as_str())
        }
        GateKind::Plan => {
            plan_id_for(e).as_deref() == Some(gate.id.as_str())
                || render::extract_plan_proposal_id(e).as_deref() == Some(gate.id.as_str())
        }
    })
}

fn approval_entry_for_id<'a>(
    entries: &'a [SessionTimelineEntry],
    request_id: &str,
) -> Option<&'a SessionTimelineEntry> {
    entries.iter().find(|entry| approval_id_for(entry).as_deref() == Some(request_id))
}

fn approval_gate_requirements_from_entry(
    entry: &SessionTimelineEntry,
) -> (Option<String>, Vec<String>) {
    let confirm_phrase = payload_field_str(entry, "confirm_phrase");
    let mut acknowledged_capabilities = Vec::new();
    if let Some(payload) = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
    {
        for key in ["added_capabilities", "broadened_capabilities"] {
            if let Some(values) = payload.get(key).and_then(|v| v.as_array()) {
                for value in values {
                    if let Some(cap) = value.as_str() {
                        acknowledged_capabilities.push(cap.to_string());
                    }
                }
            }
        }
    }
    acknowledged_capabilities.sort();
    acknowledged_capabilities.dedup();
    (confirm_phrase, acknowledged_capabilities)
}

fn approval_gate_requirements(
    client: &RoomClient,
    entries: &[SessionTimelineEntry],
    request_id: &str,
) -> (Option<String>, Vec<String>) {
    if let Some(entry) = approval_entry_for_id(entries, request_id) {
        let (mut confirm_phrase, mut acknowledged_capabilities) =
            approval_gate_requirements_from_entry(entry);
        if confirm_phrase.is_none() || acknowledged_capabilities.is_empty() {
            if let Ok(value) = rpc(
                client,
                "approvals.inspect",
                serde_json::json!({ "request_id": request_id }),
            ) {
                if confirm_phrase.is_none() {
                    confirm_phrase = value
                        .get("confirm_phrase")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
                if acknowledged_capabilities.is_empty() {
                    for key in ["added_capabilities", "broadened_capabilities"] {
                        if let Some(values) = value.get(key).and_then(|v| v.as_array()) {
                            for cap in values {
                                if let Some(name) = cap.as_str() {
                                    acknowledged_capabilities.push(name.to_string());
                                }
                            }
                        }
                    }
                    acknowledged_capabilities.sort();
                    acknowledged_capabilities.dedup();
                }
            }
        }
        return (confirm_phrase, acknowledged_capabilities);
    }
    (None, Vec::new())
}

fn approval_gate_input(
    client: &RoomClient,
    action: GateAction,
    id: String,
    entries: &[SessionTimelineEntry],
    motivation_required: bool,
) -> GateInput {
    let (required_confirm_phrase, acknowledged_capabilities) =
        approval_gate_requirements(client, entries, &id);
    // CredentialPrompt approvals need the operator to enter secret values at
    // approval time. Detect that here so the modal opens directly into the
    // secret-entry phase instead of showing a generic confirm-phrase prompt
    // that fails with a cryptic backend error on submit.
    let secret_fields = if action == GateAction::Approve
        && is_credential_prompt_entry(entries, &id)
    {
        fetch_credential_prompt_secret_fields(client, &id)
    } else {
        Vec::new()
    };
    let secret_phase = !secret_fields.is_empty();
    GateInput {
        action,
        id,
        buffer: String::new(),
        options: Vec::new(),
        allow_freeform: true,
        details_mode: false,
        motivation_required,
        required_confirm_phrase,
        acknowledged_capabilities,
        secret_fields,
        secret_values: Vec::new(),
        secret_phase,
    }
}

/// Whether the timeline entry for `request_id` is a `credential_prompt` approval.
fn is_credential_prompt_entry(entries: &[SessionTimelineEntry], request_id: &str) -> bool {
    entries.iter().any(|e| {
        e.event_type == "approval.pending"
            && approval_id_for(e).as_deref() == Some(request_id)
            && payload_field_str(e, "action").as_deref() == Some("credential_prompt")
    })
}

/// Fetch the secret-field spec for a CredentialPrompt approval via `approvals.inspect`.
/// Returns an empty vec on any failure — the operator can still fall back to the
/// CLI `gateway approvals approve <id> --secret KEY=VALUE` flow.
fn fetch_credential_prompt_secret_fields(
    client: &RoomClient,
    request_id: &str,
) -> Vec<autonoetic_types::agent::SecretFieldSpec> {
    rpc(
        client,
        "approvals.inspect",
        serde_json::json!({ "request_id": request_id }),
    )
    .ok()
    .and_then(|v| {
        v.get("secret_fields")
            .and_then(|s| serde_json::from_value(s.clone()).ok())
    })
    .unwrap_or_default()
}

fn gate_commit_validation_error(gi: &GateInput) -> Option<String> {
    if matches!(gi.action, GateAction::Approve) {
        if let Some(ref required) = gi.required_confirm_phrase {
            if !gi.buffer.trim().eq_ignore_ascii_case(required) {
                return Some(format!(
                    "✗ type confirm phrase exactly: '{required}'"
                ));
            }
            return None;
        }
    }
    if matches!(gi.action, GateAction::Approve | GateAction::Reject)
        && gi.motivation_required
        && gi.buffer.trim().is_empty()
    {
        Some("✗ motivation required — type a reason and press Enter".to_string())
    } else {
        None
    }
}

fn gate_input_label(gi: &GateInput) -> String {
    match gi.action {
        GateAction::Answer => "ANSWER".to_string(),
        GateAction::Approve => {
            if gi.required_confirm_phrase.is_some() {
                "APPROVE — confirm phrase (required)".to_string()
            } else if gi.motivation_required {
                "APPROVE — motivation (required)".to_string()
            } else {
                "APPROVE — motivation (optional)".to_string()
            }
        }
        GateAction::Reject => "REJECT — motivation (required)".to_string(),
    }
}

/// Return the input buffer for rendering, masking it with asterisks when the
/// gate is in the CredentialPrompt secret-entry phase and the current field is
/// declared `masked`. Non-secret phases and unmasked fields render verbatim.
fn rendered_input_buffer(gi: &GateInput) -> String {
    if gi.secret_phase {
        let idx = gi.secret_values.len();
        let masked = gi.secret_fields.get(idx).map(|f| f.masked).unwrap_or(true);
        if masked {
            "*".repeat(gi.buffer.chars().count())
        } else {
            gi.buffer.clone()
        }
    } else {
        gi.buffer.clone()
    }
}

/// Render detail lines, detecting markdown content in string values
/// (anything indented under the payload section) and rendering it with
/// styled ratatui Lines instead of plain text.
fn render_detail_lines(raw: &[String]) -> Vec<Line<'static>> {
    use super::markdown;
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_markdown_block = false;
    let mut md_buf: Vec<String> = Vec::new();

    for line in raw {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        if trimmed == markdown::NARRATIVE_MD_START
            || trimmed.ends_with(markdown::NARRATIVE_MD_START)
        {
            in_markdown_block = true;
            md_buf.clear();
            continue;
        }
        if trimmed.starts_with(markdown::NARRATIVE_MD_END) {
            let md_text = md_buf.join("\n");
            out.extend(markdown::render_markdown(
                &markdown::normalize_narrative_prose(&md_text),
            ));
            in_markdown_block = false;
            md_buf.clear();
            continue;
        }

        if in_markdown_block {
            if indent <= 4
                && (trimmed.starts_with('}') || trimmed.starts_with(',') || trimmed.is_empty())
            {
                let md_text = md_buf.join("\n");
                out.extend(markdown::render_markdown(
                    &markdown::normalize_narrative_prose(&md_text),
                ));
                in_markdown_block = false;
                md_buf.clear();
                if !trimmed.is_empty() {
                    out.push(Line::from(line.clone()));
                }
            } else {
                md_buf.push(trimmed.to_string());
            }
            continue;
        }

        // Legacy: indented multiline strings without narrative markers.
        if indent >= 6
            && !trimmed.is_empty()
            && !trimmed.starts_with('"')
            && !trimmed.starts_with('{')
            && !trimmed.starts_with('[')
            && markdown::looks_like_narrative_content(trimmed)
        {
            in_markdown_block = true;
            md_buf.push(trimmed.to_string());
            continue;
        }

        out.push(Line::from(line.clone()));
    }

    if in_markdown_block && !md_buf.is_empty() {
        let md_text = md_buf.join("\n");
        out.extend(markdown::render_markdown(
            &markdown::normalize_narrative_prose(&md_text),
        ));
    }

    out
}

/// Wrapped line count for detail-pane content. Must match the `Paragraph::wrap`
/// settings in [`draw`] or vertical scroll will drift from the true end.
fn detail_wrap_line_count(lines: &[Line<'static>], wrap_width: u16) -> usize {
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(wrap_width.max(1))
}

fn payload_field_str(entry: &SessionTimelineEntry, key: &str) -> Option<String> {
    entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get(key).and_then(|x| x.as_str().map(str::to_string)))
}

fn interaction_id_for(entry: &SessionTimelineEntry) -> Option<String> {
    entry
        .refs
        .interaction_id
        .clone()
        .or_else(|| payload_field_str(entry, "interaction_id"))
}

fn approval_id_for(entry: &SessionTimelineEntry) -> Option<String> {
    entry
        .refs
        .approval_request_id
        .clone()
        .or_else(|| payload_field_str(entry, "request_id"))
}

fn plan_id_for(entry: &SessionTimelineEntry) -> Option<String> {
    entry
        .refs
        .plan_id
        .clone()
        .or_else(|| payload_field_str(entry, "plan_id"))
}

fn plan_version_for(entry: &SessionTimelineEntry) -> Option<u64> {
    fn version_in_value(v: &serde_json::Value) -> Option<u64> {
        v.get("version")
            .and_then(|n| n.as_u64())
            .or_else(|| {
                v.get("result")
                    .and_then(|r| r.get("plan_version"))
                    .and_then(|n| n.as_u64())
            })
    }
    let payload = entry.payload.as_deref()?;
    let v = serde_json::from_str::<serde_json::Value>(payload).ok()?;
    version_in_value(&v).or_else(|| {
        v.get("message")
            .and_then(version_in_value)
    })
}

/// Versioned gate key — amendments reuse `plan_id` but bump `version`.
fn plan_gate_key(entry: &SessionTimelineEntry) -> Option<String> {
    let id = plan_id_for(entry).or_else(|| render::extract_plan_proposal_id(entry))?;
    let version = plan_version_for(entry).unwrap_or(1);
    Some(format!("{id}:v{version}"))
}

fn plan_gate_unresolved(
    entry: &SessionTimelineEntry,
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> bool {
    plan_gate_key(entry).is_some_and(|key| !resolved.contains(&key) && !acted.contains(&key))
}

fn unresolved_plan_gate_key(
    entries: &[SessionTimelineEntry],
    plan_id: &str,
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> Option<String> {
    let mut newest_approved_version: Option<u64> = None;
    for e in entries.iter().rev() {
        if plan_id_for(e).as_deref() != Some(plan_id)
            && render::extract_plan_proposal_id(e).as_deref() != Some(plan_id)
        {
            continue;
        }
        if e.event_type == "plan.approved" {
            if let Some(v) = plan_version_for(e) {
                newest_approved_version = Some(newest_approved_version.map_or(v, |m| m.max(v)));
            } else {
                // Legacy approval without version: treat as resolving all pending versions.
                return None;
            }
        }
        if e.event_type != "plan.pending" {
            continue;
        }
        let key = plan_gate_key(e)?;
        if resolved.contains(&key) || acted.contains(&key) {
            continue;
        }
        let pending_version = plan_version_for(e).unwrap_or(1);
        if let Some(approved_version) = newest_approved_version {
            if approved_version >= pending_version {
                // A newer (or same) version has been approved; this pending
                // revision is superseded.
                continue;
            }
        }
        return Some(key);
    }
    None
}

fn mark_plan_version_resolved(
    entries: &[SessionTimelineEntry],
    plan_id: &str,
    resolved: &mut HashSet<String>,
    acted: &mut HashSet<String>,
) {
    if let Some(key) = unresolved_plan_gate_key(entries, plan_id, resolved, acted) {
        acted.insert(key.clone());
        resolved.insert(key);
    }
}

/// Unresolved pending plan ids still visible in the timeline (newest first).
fn unresolved_pending_plan_ids(
    entries: &[SessionTimelineEntry],
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for e in entries.iter().rev() {
        let id = if e.event_type == "plan.pending" {
            if !plan_gate_unresolved(e, resolved, acted) {
                continue;
            }
            plan_id_for(e)
        } else {
            render::extract_plan_proposal_id(e)
        };
        if let Some(id) = id {
            let unresolved = if e.event_type == "plan.pending" {
                true
            } else {
                unresolved_plan_gate_key(entries, &id, resolved, acted).is_some()
            };
            if unresolved && seen.insert(id.clone()) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Map a visible timeline index to a rendered row index (post-coalesce).
fn row_index_for_visible(
    indexed: &[(RenderedRow, RowSource)],
    visible_index: usize,
) -> Option<usize> {
    indexed.iter().position(|(_, src)| match src {
        RowSource::Single(i) => *i == visible_index,
        RowSource::Run { start, len } => (*start..start + len).contains(&visible_index),
    })
}

/// Newest unresolved plan gate row in the current view (`plan.pending` preferred).
fn newest_pending_plan_event(
    visible: &[SessionTimelineEntry],
    indexed: &[(RenderedRow, RowSource)],
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> Option<(usize, String, String)> {
    for (vis_idx, e) in visible.iter().enumerate().rev() {
        let id = if e.event_type == "plan.pending" {
            plan_id_for(e)
        } else {
            render::extract_plan_proposal_id(e)
        };
        let Some(id) = id else { continue };
        if !plan_gate_unresolved(e, resolved, acted) {
            continue;
        }
        let row_idx = row_index_for_visible(indexed, vis_idx)?;
        return Some((row_idx, id, e.event_id.clone()));
    }
    None
}

fn notification_approval_id(entry: &SessionTimelineEntry) -> Option<String> {
    let msg = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(|m| m.to_string()))?;
    let parsed = serde_json::from_str::<serde_json::Value>(&msg).ok()?;
    if parsed.get("type").and_then(|v| v.as_str()) != Some("child_state_notification") {
        return None;
    }
    let notif = parsed.get("notification")?;
    let status = notif.get("child_status").and_then(|v| v.as_str());
    if status != Some("awaiting_approval") {
        return None;
    }
    notif.get("approval_request_id").and_then(|v| v.as_str()).map(String::from)
}

/// Read the pre-digested choices + freeform policy for an interaction from its
#[derive(Default)]
struct ModelStats {
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    errors: u64,
}

struct SessionStats {
    total_input: u64,
    total_output: u64,
    llm_calls: u64,
    models: Vec<String>,
    /// Per-model breakdown of calls, tokens, and errors.
    per_model: HashMap<String, ModelStats>,
    context_total_pct: f64,
    context_samples: u64,
    context_window: Option<u32>,
    /// Tool names for calls that were requested but not yet completed — async
    /// work in flight (approval-gated tools, workflow_wait, resumed
    /// continuations). Surfaced as a subtle header chip so a launched-but-not-
    /// returned tool is never silently lost between request and response.
    pending_calls: Vec<String>,
    /// Turns since the oldest still-open call was requested (see `PendingSummary`).
    pending_age_turns: Option<u64>,
}

fn short_model(model: &str) -> String {
    model.split('/').last().unwrap_or(model).to_string()
}

fn compute_session_stats(entries: &[SessionTimelineEntry]) -> SessionStats {
    let pending = pending_tool_summary(entries);
    let mut stats = SessionStats {
        total_input: 0,
        total_output: 0,
        llm_calls: 0,
        models: Vec::new(),
        per_model: HashMap::new(),
        context_total_pct: 0.0,
        context_samples: 0,
        context_window: None,
        pending_calls: pending.names,
        pending_age_turns: pending.age_turns,
    };
    for e in entries {
        if let Some(p) = e.payload.as_deref() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(p) {
                match e.event_type.as_str() {
                    "llm.round" => {
                        let inp = v.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        let out = v.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        if inp > 0 || out > 0 {
                            stats.total_input += inp;
                            stats.total_output += out;
                            stats.llm_calls += 1;
                        }

                        let usage = v.get("usage");
                        let (ctx_window, ctx_pct) = match usage.and_then(|u| u.get("context_window_tokens")) {
                            Some(w) => (
                                w.as_u64().map(|x| x as u32),
                                usage
                                    .and_then(|u| u.get("input_context_pct"))
                                    .and_then(|p| p.as_f64()),
                            ),
                            None => (
                                v.get("context_window_tokens")
                                    .and_then(|t| t.as_u64())
                                    .map(|x| x as u32),
                                v.get("input_context_pct").and_then(|p| p.as_f64()),
                            ),
                        };

                        if let Some(w) = ctx_window {
                            stats.context_window = Some(w);
                        }
                        if let Some(pct) = ctx_pct {
                            stats.context_total_pct += pct;
                            stats.context_samples += 1;
                        }

                        if let Some(model) = v.get("model").and_then(|m| m.as_str()) {
                            let short = short_model(model);
                            if !stats.models.contains(&short) {
                                stats.models.push(short.clone());
                            }
                            let m = stats.per_model.entry(short).or_default();
                            m.calls += 1;
                            m.input_tokens += inp;
                            m.output_tokens += out;
                        }
                    }
                    "llm.request_failed" => {
                        let model = v
                            .get("model")
                            .and_then(|m| m.as_str())
                            .map(short_model)
                            .unwrap_or_else(|| "unknown".to_string());
                        let m = stats.per_model.entry(model).or_default();
                        m.errors += 1;
                    }
                    "llm.empty_response" => {
                        let model = v
                            .get("model")
                            .and_then(|m| m.as_str())
                            .map(short_model)
                            .unwrap_or_else(|| "unknown".to_string());
                        let m = stats.per_model.entry(model.clone()).or_default();
                        m.errors += 1;
                        if !stats.models.contains(&model) {
                            stats.models.push(model);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    stats
}

/// Tool names for calls seen as `tool.requested` but not yet `tool.completed`,
/// in request order. Correlation is by `call_id` when present; older rows
/// without one fall back to matching a completion to the earliest open request
/// of the same tool name (FIFO). Non-async calls complete in the same batch, so
/// this list is normally empty — a non-empty result means real in-flight work.
struct PendingSummary {
    /// Tool names of still-open calls, in request (chronological) order.
    names: Vec<String>,
    /// How many turns ago the *oldest* still-open call was requested, measured
    /// against the latest turn seen. `None` when nothing is pending or turns are
    /// unknown. A large value flags a call that has been waiting a long time.
    age_turns: Option<u64>,
}

fn pending_tool_summary(entries: &[SessionTimelineEntry]) -> PendingSummary {
    // Open calls in chronological order: (call_id, tool_name, requested_turn).
    // A completion removes the first matching open call (by call_id, else by
    // name for rows predating call_id). Chronological order means the first
    // still-open entry is the oldest.
    let mut open: Vec<(Option<String>, String, Option<u64>)> = Vec::new();
    let mut latest_turn: Option<u64> = None;
    for e in entries {
        let turn = e.turn_id.as_deref().and_then(turn_number_of);
        if let Some(t) = turn {
            latest_turn = Some(latest_turn.map_or(t, |m| m.max(t)));
        }
        let ty = e.event_type.as_str();
        if ty != "tool.requested" && ty != "tool.completed" {
            continue;
        }
        let tool = payload_field_str(e, "tool_name").unwrap_or_else(|| "tool".into());
        let call_id = payload_field_str(e, "call_id");
        match ty {
            "tool.requested" => open.push((call_id, tool, turn)),
            "tool.completed" => {
                let pos = match &call_id {
                    Some(id) => open
                        .iter()
                        .position(|(oid, _, _)| oid.as_deref() == Some(id.as_str())),
                    None => open
                        .iter()
                        .position(|(oid, n, _)| oid.is_none() && n == &tool),
                };
                if let Some(p) = pos {
                    open.remove(p);
                }
            }
            _ => {}
        }
    }
    let names = open.iter().map(|(_, n, _)| n.clone()).collect();
    // Age is measured from the genuinely-oldest still-open call (the first, since
    // `open` is chronological). If *that* call has no turn, the oldest age is
    // unknown — don't substitute a younger request's turn, which would understate
    // it. `l >= o` so a call requested this same turn reports 0 (known), not None.
    let oldest_turn = open.first().and_then(|(_, _, t)| *t);
    let age_turns = match (latest_turn, oldest_turn) {
        (Some(l), Some(o)) if l >= o => Some(l - o),
        _ => None,
    };
    PendingSummary { names, age_turns }
}

/// View-row indices whose content matches `query` (case-insensitive). Searches
/// the rendered one-line text plus the raw payload, so it finds both visible
/// text and hidden fields (agent ids, refs). Collapsed runs match if any of
/// their underlying entries match, so folded content is still discoverable.
fn compute_search_matches(
    indexed: &[(RenderedRow, RowSource)],
    visible: &[SessionTimelineEntry],
    query: &str,
) -> Vec<usize> {
    let q = query.to_lowercase();
    let haystack = |e: &SessionTimelineEntry| -> String {
        let mut s = render::render_line(e).to_lowercase();
        if let Some(p) = &e.payload {
            s.push('\n');
            s.push_str(&p.to_lowercase());
        }
        s
    };
    indexed
        .iter()
        .enumerate()
        .filter_map(|(vi, (_, src))| {
            let matched = match src {
                RowSource::Single(i) => {
                    visible.get(*i).map(|e| haystack(e).contains(&q)).unwrap_or(false)
                }
                RowSource::Run { start, len } => visible
                    .get(*start..start + len)
                    .map(|slice| slice.iter().any(|e| haystack(e).contains(&q)))
                    .unwrap_or(false),
            };
            matched.then_some(vi)
        })
        .collect()
}

fn avg_context_pct(stats: &SessionStats) -> Option<f64> {
    if stats.context_samples > 0 {
        Some(stats.context_total_pct / stats.context_samples as f64)
    } else {
        None
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 100_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n >= 1_000 {
        format!("{:.2}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// `user.ask.pending` timeline entry (the gateway embeds them, #393). Returns
/// `(options, allow_freeform)`; missing `allow_freeform` defaults to permissive.
/// Adds a synthetic "Give more details" option when there are pre-defined
/// choices so the operator can always elaborate without guessing. The returned
/// `allow_freeform` is the original payload value, not overridden by the
/// presence of options; the synthetic details option is handled locally.
fn interaction_choices(entries: &[SessionTimelineEntry], interaction_id: &str) -> (Vec<GateOption>, bool) {
    let entry = entries.iter().find(|e| {
        e.event_type == "user.ask.pending"
            && interaction_id_for(e).as_deref() == Some(interaction_id)
    });
    let Some(payload) = entry
        .and_then(|e| e.payload.as_deref())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
    else {
        return (Vec::new(), true);
    };
    let mut options: Vec<GateOption> = payload
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|o| {
                    Some(GateOption {
                        id: o.get("id")?.as_str()?.to_string(),
                        label: o.get("label")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let allow_freeform = payload
        .get("allow_freeform")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    // Always offer an explicit "give details" option when there are pre-defined
    // choices. Selecting it lets the operator type a free-text follow-up without
    // the TUI guessing whether freeform is allowed.
    if !options.is_empty() {
        options.push(GateOption {
            id: "__details__".to_string(),
            label: "Give more details / explain".to_string(),
        });
    }
    (options, allow_freeform)
}

/// Restores the terminal on drop, even on early return / panic-unwind.
struct TerminalRestore;
impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste, LeaveAlternateScreen);
    }
}

/// One JSON-RPC round-trip from the sync TUI loop. `block_in_place` keeps the
/// multi-thread runtime healthy while we briefly block on the call.
fn rpc(
    client: &RoomClient,
    method: &str,
    params: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(client.call(method, params)))
}

pub fn run(
    client: &RoomClient,
    root_session_id: &mut String,
    initial_floor: Altitude,
    limit: u32,
    target_agent_id: &mut Option<String>,
    presets: &[String],
) -> anyhow::Result<()> {
    crate::cli::terminal::require_interactive_terminal("Session Room")?;
    enable_raw_mode()?;
    // Guard constructed before entering the alternate screen, so raw mode is
    // restored even if `EnterAlternateScreen` (or anything after) fails.
    let _restore = TerminalRestore;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    // Force a full clear of the (alternate) screen before the first paint. On
    // terminals where switching to the alternate buffer is a no-op — notably
    // GNU `screen` with `altscreen off` (its default) — `EnterAlternateScreen`
    // does not present a fresh, cleared surface, so ratatui's first diff-based
    // draw paints over whatever was already on the terminal. An explicit clear
    // makes the first frame deterministic there and is a harmless no-op on
    // terminals that did switch to a blank alternate buffer.
    terminal.clear()?;

    let mut entries: Vec<SessionTimelineEntry> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut floor = initial_floor;
    let mut squash = true;
    let mut follow = true; // pin to newest
    let mut selected: usize = 0;
    // View-row indices (into the squashed `rows` vec) that are first-class
    // checkpoints (plan/approval/escalation/operator/session boundaries).
    // Recomputed each frame after coalescing; the `[` / `]` keys jump across.
    let mut checkpoint_rows: Vec<usize> = Vec::new();
    // View-row indices at Attention altitude or above (failures, gates,
    // interventions) — the `e` / `E` keys jump across these to reach problems.
    let mut attention_rows: Vec<usize> = Vec::new();
    // Turn numbers with a runnable checkpoint (forkable). Refreshed alongside
    // the timeline; used to mark turn dividers distinctly.
    let mut forkable_turns: HashSet<u64> = HashSet::new();
    // In-timeline text search. `search_input` is the buffer while typing (Ctrl+F);
    // `search_query` is the committed term. `search_matches` (view-row indices) is
    // recomputed each frame; `n`/`N` cycle `search_current` through it.
    let mut search_input: Option<String> = None;
    let mut search_query: Option<String> = None;
    let mut search_matches: Vec<usize> = Vec::new();
    let mut search_current: usize = 0;
    let mut search_needs_jump = false;
    let mut detail: Option<DetailPane> = None;
    let mut detail_scroll: u16 = 0; // vertical scroll offset for detail pane
    let mut detail_h_scroll: u16 = 0; // horizontal scroll offset for detail pane
    let mut input: Option<GateInput> = None; // in-flight gate decision
    let mut pending_gate: Option<PendingGateResolve> = None; // background gate RPC
    let mut compose: Option<ComposeInput> = None; // in-flight free-form message to the session
    // When Some, the active compose targets a file comment (name, version handle)
    // and submits via `content.comment` instead of a freeform session message.
    let mut compose_comment: Option<(String, String)> = None;
    let mut clipboard =
        std::panic::catch_unwind(|| arboard::Clipboard::new().ok()).unwrap_or(None);
    let mut slash: Option<String> = None; // in-flight slash-command buffer (no leading `/`)
    let mut session_pick_list: Option<Vec<String>> = None; // ids from /session list for number-pick
    let mut wiki_request_ids: Option<Vec<String>> = None; // ids from /wiki proposals for number-detail
    let mut status: Option<String> = None; // last action / connection result
    // Gates no longer offerable: approvals resolved on the timeline, plus
    // anything the operator just acted on (covers interactions, which have no
    // timeline resolution event yet).
    let mut resolved: HashSet<String> = HashSet::new();
    let mut acted: HashSet<String> = HashSet::new();
    // Display toggles + spinner state for the in-flight row indicator.
    let mut show_reasoning = true;
    let mut spinner_frame: usize = 0;
    let mut info_panel_open = false;
    let mut info_scroll: u16 = 0;
    let mut artifact_viewer: Option<ArtifactViewer> = None;
    let mut artifact_file_view: Option<ArtifactFileView> = None;
    // Pillar D: live session content tree (drafts visible from t=0) + viewer.
    let mut live_content_pane: Option<LiveContentPane> = None;
    let mut content_view: Option<ContentView> = None;
    let mut quit_armed_until: Option<Instant> = None;
    let mut estop_armed_until: Option<Instant> = None;
    let mut last_mouse_click: Option<(Instant, usize, u16, u16)> = None;
    let mut last_announced_plan_event: Option<String> = None;
    let mut last_announced_gate_event: Option<String> = None;
    let mut gate_modal: Option<GateModal> = None;
    let mut approvals_popup: Option<ApprovalsPopup> = None;
    let mut grants_panel: Option<GrantsPanel> = None;
    let mut last_grants_poll = Instant::now();
    let mut last_session_status_poll = Instant::now();
    let mut last_timeline_poll = Instant::now();
    let mut force_timeline_refresh = true;
    let mut session_async_processing = false;
    let mut spawn_lineage: HashMap<String, SessionSpawnLineageEntry> = HashMap::new();
    // Last rendered frame — input is drained before the next timeline fetch.
    let mut view_rows: Vec<RenderedRow> = Vec::new();
    let mut view_indexed: Vec<(RenderedRow, RowSource)> = Vec::new();
    let mut view_visible: Vec<SessionTimelineEntry> = Vec::new();
    let mut view_gate: Option<GateRef> = None;
    let mut view_row_count = 0usize;
    let mut view_row_heights: Vec<usize> = Vec::new();
    let mut view_viewport_offset = 0usize;
    let mut view_list_height = 0usize;
    let mut view_turn_boundaries: HashMap<usize, bool> = HashMap::new();
    // Idle-frame optimization: only rebuild and redraw when something changed.
    let mut needs_redraw = true;
    let mut cached_open_turns: HashSet<String> = HashSet::new();
    let mut cached_open_turns_valid = false;
    let mut cached_floor = floor;
    let mut cached_squash = squash;
    let mut cached_show_reasoning = show_reasoning;

    'room: loop {
        // Drain pending input before any blocking gateway work so arrows / wheel
        // stay responsive even when timeline RPCs are slow.
        let mut repaint_after_input = false;
        while event::poll(Duration::from_millis(0))? {
            match event::read()? {
                Event::Paste(text) => {
                    if let Some(c) = compose.as_mut() {
                        c.insert_str(&text);
                        repaint_after_input = true;
                    } else if let Some(gi) = input.as_mut() {
                        if gi.allow_freeform || gi.details_mode {
                            gi.buffer.push_str(&text.replace('\r', ""));
                            repaint_after_input = true;
                        }
                    }
                }
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    repaint_after_input = true;
                    // Compose mode: multi-line editor with cursor + clipboard (#405).
                    if let Some(c) = compose.as_mut() {
                        match handle_compose_key(c, &key, &mut clipboard) {
                            ComposeKeyResult::Continue => {}
                            ComposeKeyResult::Cancel => {
                                compose = None;
                                compose_comment = None;
                            }
                            ComposeKeyResult::Send(text) => {
                                status = Some(if let Some((name, handle)) = compose_comment.take() {
                                    send_comment(
                                        client,
                                        root_session_id,
                                        &name,
                                        &handle,
                                        &text,
                                    )
                                } else {
                                    send_message(
                                        client,
                                        root_session_id,
                                        &text,
                                        target_agent_id.as_deref(),
                                    )
                                });
                                compose = None;
                                follow = true;
                                force_timeline_refresh = true;
                            }
                        }
                        continue;
                    }

                    // Slash mode: capture a command and dispatch on Enter. Lives
                    // before the other key handlers so `:` and `?` can also
                    // enter it (matching vim/Discord conventions). The parser
                    // classifies the buffer; we never execute a raw string.
                    if let Some(buf) = slash.as_mut() {
                        match key.code {
                            KeyCode::Esc => slash = None,
                            KeyCode::Enter => {
                                let cmdline = buf.trim().to_string();
                                slash = None;
                                match super::slash::parse(&cmdline) {
                                    SlashCommand::Quit => {
                                        arm_quit(&mut quit_armed_until, &mut status);
                                    }
                                    SlashCommand::Help => {
                                        detail = Some(DetailPane::event(super::slash::help_lines(), None));
                                        detail_scroll = 0;
                                        detail_h_scroll = 0;
                                        session_pick_list = None;
                                        wiki_request_ids = None;
                                        status = Some("help: Esc to close".to_string());
                                    }
                                    SlashCommand::Test { name } => {
                                        if name.is_empty() || name == "help" {
                                            detail = Some(DetailPane::event(
                                                super::test_scenarios::scenario_help()
                                                    .lines()
                                                    .map(String::from)
                                                    .collect(),
                                                None,
                                            ));
                                            detail_scroll = 0;
                                            detail_h_scroll = 0;
                                            status = Some("test: pick a scenario, e.g. /test full-session".to_string());
                                        } else if let Some(events) =
                                            super::test_scenarios::run(&name, &root_session_id)
                                        {
                                            let count = events.len();
                                            entries.extend(events);
                                            status = Some(format!(
                                                "✓ injected {count} test events for '{name}'"
                                            ));
                                        } else {
                                            status = Some(format!(
                                                "✗ unknown test scenario '{name}' — /test help to list"
                                            ));
                                        }
                                    }
                                    SlashCommand::SwitchSession(new_id) => {
                                        if new_id.is_empty() {
                                            let (lines, ids) = list_sessions_detail(client, None);
                                            detail = Some(DetailPane::event(lines, None));
                                            session_pick_list = Some(ids);
                                        } else if new_id == *root_session_id {
                                            status = Some(format!("→ already viewing {new_id}"));
                                        } else {
                                            switch_session(
                                                client,
                                                &mut entries,
                                                &mut cursor,
                                                &mut selected,
                                                &mut detail,
                                                &mut follow,
                                                &mut resolved,
                                                &mut acted,
                                                &mut floor,
                                                root_session_id,
                                                target_agent_id,
                                                limit,
                                                &new_id,
                                                &mut force_timeline_refresh,
                                                &mut spawn_lineage,
                                            );
                                            status = Some(format!("→ switched to session {new_id}"));
                                        }
                                    }
                                    SlashCommand::ListSessions { agent } => {
                                        let (lines, ids) = list_sessions_detail(client, agent.as_deref());
                                        detail = Some(DetailPane::event(lines, None));
                                        session_pick_list = Some(ids);
                                    }
                                    SlashCommand::ListCronJobs => {
                                        detail = Some(DetailPane::event(list_cron_detail(client, root_session_id), None));
                                        session_pick_list = None;
                                        wiki_request_ids = None;
                                    }
                                    SlashCommand::ListPlans => {
                                        detail = Some(DetailPane::event(list_plans_detail(client, root_session_id), None));
                                        session_pick_list = None;
                                        wiki_request_ids = None;
                                        status = Some(
                                            "plan list: Enter/p on row for review · y approve · Esc close"
                                                .to_string(),
                                        );
                                    }
                                    SlashCommand::ListWikiProposals => {
                                        let (lines, ids) = list_wiki_proposals_detail(client, root_session_id);
                                        detail = Some(DetailPane::event(lines, None));
                                        session_pick_list = None;
                                        wiki_request_ids = None;
                                        wiki_request_ids = Some(ids);
                                        status = Some(
                                            "wiki proposals: press number to view details · Esc close"
                                                .to_string(),
                                        );
                                    }
                                    SlashCommand::ApprovePlan { plan_id } => {
                                        let target = match plan_id {
                                            Some(id) if !id.is_empty() => id,
                                            _ => match latest_pending_plan_id(client, root_session_id) {
                                                Some(id) => id,
                                                None => {
                                                    detail = Some(DetailPane::event(list_plans_detail(
                                                        client, root_session_id,
                                                    ), None));
                                                    status = Some(
                                                        "✗ no pending plan — /plan to list".to_string(),
                                                    );
                                                    continue;
                                                }
                                            },
                                        };
                                        match approve_plan_and_wake(
                                            client,
                                            root_session_id,
                                            &target,
                                            target_agent_id.as_deref(),
                                        ) {
                                            Ok(msg) => {
                                            mark_plan_version_resolved(
                                                &entries,
                                                &target,
                                                &mut resolved,
                                                &mut acted,
                                            );
                                                if detail
                                                    .as_ref()
                                                    .is_some_and(|d| d.plan_id.as_deref() == Some(target.as_str()))
                                                {
                                                    clear_detail(
                                                        &mut detail,
                                                        &mut detail_scroll,
                                                        &mut detail_h_scroll,
                                                    );
                                                }
                                                status = Some(msg);
                                                follow = true;
                                                force_timeline_refresh = true;
                                            }
                                            Err(e) => status = Some(e),
                                        }
                                    }
                                    SlashCommand::ReturnToAgent { force, message } => {
                                        status = Some(return_workbench_to_agent(
                                            client,
                                            root_session_id,
                                            force,
                                            message.as_deref(),
                                        ));
                                        force_timeline_refresh = true;
                                        follow = true;
                                    }
                                    SlashCommand::ResumeSession { agent } => {
                                        if let Some(resolved_id) =
                                            resolve_latest_session(client, agent.as_deref())
                                        {
                                            if resolved_id == *root_session_id {
                                                status = Some(format!("→ already viewing {resolved_id}"));
                                            } else {
                                                switch_session(
                                                    client,
                                                    &mut entries,
                                                    &mut cursor,
                                                    &mut selected,
                                                    &mut detail,
                                                    &mut follow,
                                                    &mut resolved,
                                                    &mut acted,
                                                    &mut floor,
                                                    root_session_id,
                                                    target_agent_id,
                                                    limit,
                                                    &resolved_id,
                                                    &mut force_timeline_refresh,
                                                    &mut spawn_lineage,
                                                );
                                                status = Some(format!("→ resumed session {resolved_id}"));
                                            }
                                        } else {
                                            status = Some(
                                                "✗ /session resume: no sessions found".to_string(),
                                            );
                                        }
                                    }
                                    SlashCommand::ForkSession { at_turn, message } => {
                                        match fork_session(
                                            client,
                                            root_session_id,
                                            at_turn,
                                            message.as_deref(),
                                        ) {
                                            Ok((new_id, fork_turn)) => {
                                                switch_session(
                                                    client,
                                                    &mut entries,
                                                    &mut cursor,
                                                    &mut selected,
                                                    &mut detail,
                                                    &mut follow,
                                                    &mut resolved,
                                                    &mut acted,
                                                    &mut floor,
                                                    root_session_id,
                                                    target_agent_id,
                                                    limit,
                                                    &new_id,
                                                    &mut force_timeline_refresh,
                                                    &mut spawn_lineage,
                                                );
                                                session_pick_list = None;
                                        wiki_request_ids = None;
                                                status = Some(format!(
                                                    "→ forked at turn {fork_turn} → {new_id} · send a message to continue this branch"
                                                ));
                                            }
                                            Err(e) => status = Some(e),
                                        }
                                    }
                                    SlashCommand::EmergencyStopAndRedirect { message } => {
                                        let reason = "Operator emergency stop from session room TUI";
                                        match rpc(
                                            client,
                                            "root_session.emergency_stop",
                                            serde_json::json!({
                                                "root_session_id": root_session_id,
                                                "reason": reason,
                                                "requested_by_type": "operator",
                                                "requested_by_id": "session-room",
                                                "trigger_kind": "manual",
                                                "notify_where_practical": true,
                                            }),
                                        ) {
                                            Ok(_) => {
                                                if let Some(msg) = message
                                                    .as_deref()
                                                    .filter(|m| !m.is_empty())
                                                {
                                                    let send_status = send_message(
                                                        client,
                                                        root_session_id,
                                                        msg,
                                                        target_agent_id.as_deref(),
                                                    );
                                                    status = Some(format!(
                                                        "✓ emergency stop — {send_status}"
                                                    ));
                                                } else {
                                                    status = Some("✓ emergency stop issued".to_string());
                                                }
                                                force_timeline_refresh = true;
                                                follow = true;
                                            }
                                            Err(e) => {
                                                status =
                                                    Some(format!("✗ emergency stop failed: {e}"));
                                            }
                                        }
                                    }
                                    SlashCommand::Curate { notes } => {
                                        // Fire memory curation on the session
                                        // the operator is viewing, with optional
                                        // focus notes that steer the curator.
                                        let mut params = serde_json::json!({
                                            "root_session_id": &*root_session_id,
                                        });
                                        if let Some(n) = notes
                                            .as_deref()
                                            .filter(|n| !n.is_empty())
                                        {
                                            params["focus_notes"] = serde_json::json!(n);
                                        }
                                        match rpc(client, "curation.run_for_session", params) {
                                            Ok(v) => {
                                                let task = v
                                                    .get("task_id")
                                                    .and_then(|x| x.as_str())
                                                    .unwrap_or("?");
                                                let queued_for = v
                                                    .get("session_id")
                                                    .and_then(|x| x.as_str())
                                                    .unwrap_or(root_session_id);
                                                status = Some(format!(
                                                    "✓ curation queued for session {queued_for} (task {task}) — watch the timeline"
                                                ));
                                                force_timeline_refresh = true;
                                                follow = true;
                                            }
                                            Err(e) => {
                                                status =
                                                    Some(format!("✗ curate failed: {e}"));
                                            }
                                        }
                                    }
                                    SlashCommand::Crystallize { notes } => {
                                        // Make what worked in this session
                                        // reusable. The crystallizer picks the
                                        // route (instruction / wrapper / new
                                        // skill); notes name the tactic.
                                        let mut params = serde_json::json!({
                                            "root_session_id": &*root_session_id,
                                        });
                                        if let Some(n) = notes
                                            .as_deref()
                                            .filter(|n| !n.is_empty())
                                        {
                                            params["focus_notes"] = serde_json::json!(n);
                                        }
                                        match rpc(
                                            client,
                                            "skill.crystallize_from_session",
                                            params,
                                        ) {
                                            Ok(v) => {
                                                let task = v
                                                    .get("task_id")
                                                    .and_then(|x| x.as_str())
                                                    .unwrap_or("?");
                                                let queued_for = v
                                                    .get("session_id")
                                                    .and_then(|x| x.as_str())
                                                    .unwrap_or(root_session_id);
                                                // A singleton already running is
                                                // reported as such rather than as
                                                // a second queued run.
                                                let deduped = v
                                                    .get("status")
                                                    .and_then(|x| x.as_str())
                                                    == Some("deduplicated");
                                                status = Some(if deduped {
                                                    format!(
                                                        "✓ crystallizer already running for session {queued_for} (task {task}) — watch the timeline"
                                                    )
                                                } else {
                                                    format!(
                                                        "✓ crystallization queued for session {queued_for} (task {task}) — watch the timeline"
                                                    )
                                                });
                                                force_timeline_refresh = true;
                                                follow = true;
                                            }
                                            Err(e) => {
                                                status = Some(format!(
                                                    "✗ crystallize failed: {e}"
                                                ));
                                            }
                                        }
                                    }
                                    SlashCommand::ListSkills => {
                                        // Read-only standing view; nothing here
                                        // acts, so no confirmation and no refresh
                                        // of the timeline.
                                        detail = Some(DetailPane::event(
                                            list_skills_detail(client),
                                            None,
                                        ));
                                        session_pick_list = None;
                                        status = Some(
                                            "skill work: proposals + decisions · Esc close"
                                                .to_string(),
                                        );
                                    }
                                    SlashCommand::ModelShow => {
                                        match rpc(client, "session.inference.get", serde_json::json!({
                                            "session_id": &*root_session_id,
                                        })) {
                                            Ok(v) => {
                                                let pn = v.get("preset_name").and_then(|x| x.as_str()).unwrap_or("?");
                                                let prov = v.get("provider").and_then(|x| x.as_str()).unwrap_or("?");
                                                let mdl = v.get("model").and_then(|x| x.as_str()).unwrap_or("?");
                                                let ovr = v.get("session_override_preset").and_then(|x| x.as_str()).filter(|x| !x.is_empty());
                                                let avail = presets.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" · ");
                                                let binding = match ovr {
                                                    Some(p) => format!(" (override: {p})"),
                                                    None => String::new(),
                                                };
                                                status = Some(format!(
                                                    "→ {pn}{binding}: {prov}/{mdl}   available: {avail}"
                                                ));
                                            }
                                            Err(e) => status = Some(format!("✗ {e}")),
                                        }
                                    }
                                    SlashCommand::ModelSet { preset } => {
                                        match rpc(client, "session.inference.set", serde_json::json!({
                                            "session_id": &*root_session_id,
                                            "preset": &preset,
                                            "set_by": "operator:tui",
                                        })) {
                                            Ok(v) => {
                                                let prov = v.get("resolved").and_then(|r| r.get("provider")).and_then(|x| x.as_str()).unwrap_or("?");
                                                let mdl = v.get("resolved").and_then(|r| r.get("model")).and_then(|x| x.as_str()).unwrap_or("?");
                                                status = Some(format!("✓ model override → {preset} ({prov}/{mdl})"));
                                            }
                                            Err(e) => status = Some(format!("✗ {e}")),
                                        }
                                    }
                                    SlashCommand::ModelClear => {
                                        match rpc(client, "session.inference.clear", serde_json::json!({
                                            "session_id": &*root_session_id,
                                            "set_by": "operator:tui",
                                        })) {
                                            Ok(v) => {
                                                let cleared = v.get("cleared").and_then(|x| x.as_bool()).unwrap_or(false);
                                                if cleared {
                                                    status = Some("✓ session model override cleared".to_string());
                                                } else {
                                                    status = Some("ℹ no override was set".to_string());
                                                }
                                            }
                                            Err(e) => status = Some(format!("✗ {e}")),
                                        }
                                    }
                                    SlashCommand::Unknown(verb) => {
                                        let v = if verb.is_empty() {
                                            "(empty)".to_string()
                                        } else {
                                            format!("/{verb}")
                                        };
                                        status = Some(format!("✗ unknown command {v} — type /help"));
                                    }
                                }
                            }
                            KeyCode::Backspace => {
                                buf.pop();
                            }
                            KeyCode::Char(c) => buf.push(c),
                            _ => {}
                        }
                        continue;
                    }

                    // Search-typing mode (opened with Ctrl+F): capture the query
                    // until Enter (commit) or Esc (cancel). Matches are computed
                    // each frame from the committed query.
                    if search_input.is_some() {
                        match key.code {
                            KeyCode::Esc => {
                                search_input = None;
                                status = Some("search cancelled".to_string());
                            }
                            KeyCode::Enter => {
                                let q = search_input.take().unwrap_or_default().trim().to_string();
                                if q.is_empty() {
                                    search_query = None;
                                    search_matches.clear();
                                    status = Some("search cleared".to_string());
                                } else {
                                    search_query = Some(q);
                                    search_current = 0;
                                    search_needs_jump = true;
                                }
                            }
                            KeyCode::Backspace => {
                                if let Some(b) = search_input.as_mut() {
                                    b.pop();
                                }
                            }
                            KeyCode::Char(c) => {
                                if let Some(b) = search_input.as_mut() {
                                    b.push(c);
                                }
                            }
                            _ => {}
                        }
                        if let Some(b) = &search_input {
                            status = Some(format!("search: {b}▏  (Enter find · Esc cancel)"));
                        }
                        continue;
                    }

                    // Text-capture mode takes over all input while open.
                    if let Some(gi) = input.as_mut() {
                        if key.code == KeyCode::Enter {
                            // CredentialPrompt secret-entry phase (Phase 1): each Enter
                            // stores the current field's value and advances. After the
                            // last field we either fall through to the confirm-phrase
                            // phase (Phase 2) or submit immediately.
                            if gi.secret_phase {
                                if gi.buffer.trim().is_empty() {
                                    status = Some(
                                        "✗ secret value must not be empty — type it and press Enter"
                                            .to_string(),
                                    );
                                    continue;
                                }
                                gi.secret_values.push(gi.buffer.clone());
                                gi.buffer.clear();
                                if gi.secret_values.len() >= gi.secret_fields.len() {
                                    // All secrets collected → exit Phase 1.
                                    gi.secret_phase = false;
                                    if gi.required_confirm_phrase.is_some() {
                                        // Phase 2: confirm phrase (existing rendering below).
                                        status = None;
                                        continue;
                                    }
                                    // No confirm phrase → submit now via the shared path.
                                    let gi = input.take().unwrap();
                                    match resolve_gate(client, gi, None) {
                                        Ok(pending) => {
                                            pending_gate = Some(pending);
                                            status = Some(
                                                "⏳ submitting credential…".to_string(),
                                            );
                                        }
                                        Err((msg, gi)) => {
                                            status = Some(msg);
                                            input = Some(gi);
                                        }
                                    }
                                } else {
                                    status = None;
                                }
                                continue;
                            }
                            // The synthetic "Give more details" option switches the input
                            // into free-text follow-up mode without submitting.
                            let details = gi
                                .options
                                .iter()
                                .position(|o| o.id == "__details__")
                                .and_then(|idx| {
                                    gi.buffer
                                        .trim()
                                        .parse::<usize>()
                                        .ok()
                                        .filter(|n| *n == idx + 1)
                                })
                                .is_some();
                            if details {
                                gi.buffer.clear();
                                gi.details_mode = true;
                                status = Some(
                                    "✓ type your details, then press Enter".to_string(),
                                );
                                continue;
                            }
                            let gi = input.take().unwrap();
                            if let Some(err) = gate_commit_validation_error(&gi) {
                                status = Some(err.to_string());
                                input = Some(gi);
                                continue;
                            }
                            if gi.id.starts_with("test-") {
                                acted.insert(gi.id.clone());
                                let verb = match gi.action {
                                    GateAction::Approve => "approved",
                                    GateAction::Reject => "rejected",
                                    GateAction::Answer => "answered",
                                };
                                let answer_text = {
                                    let b = gi.buffer.trim();
                                    (!b.is_empty()).then_some(b)
                                };
                                let followup = super::test_scenarios::resolve_followup(
                                    &gi.id,
                                    gi.action == GateAction::Approve,
                                    answer_text,
                                    &root_session_id,
                                );
                                let n = followup.len();
                                entries.extend(followup);
                                status = Some(format!(
                                    "✓ {verb} {} (test) — {n} follow-up events injected",
                                    gi.id
                                ));
                            } else {
                                match resolve_gate(client, gi, None) {
                                    Ok(pending) => {
                                        pending_gate = Some(pending);
                                        status = Some(
                                            "⏳ submitting answer…".to_string()
                                        );
                                    }
                                    Err((msg, gi)) => {
                                        status = Some(msg);
                                        input = Some(gi);
                                    }
                                }
                            }
                            continue;
                        }
                        match key.code {
                            KeyCode::Esc if gi.details_mode => {
                                gi.details_mode = false;
                                gi.buffer.clear();
                                status = None;
                            }
                            KeyCode::Esc => input = None,
                            KeyCode::Backspace => {
                                gi.buffer.pop();
                            }
                            KeyCode::Char(c) => gi.buffer.push(c),
                            _ => {}
                        }
                        continue;
                    }

                    // Gate modal — full overlay blocks nav; peek mode leaves timeline usable.
                    if let Some(modal) = gate_modal.as_ref() {
                        if modal.peek_timeline {
                            match key.code {
                                KeyCode::Char('g') | KeyCode::Enter => {
                                    if let Some(m) = gate_modal.as_mut() {
                                        m.peek_timeline = false;
                                    }
                                    continue;
                                }
                                KeyCode::Char('y') | KeyCode::Char('n')
                                    if input.is_none()
                                        && matches!(
                                            modal.gate.kind,
                                            GateKind::Approval
                                                | GateKind::WikiProposal
                                                | GateKind::Escalation
                                                | GateKind::Plan
                                        ) =>
                                {
                                    let gate_id = modal.gate.id.clone();
                                    let kind = modal.gate.kind;
                                    let approve = key.code == KeyCode::Char('y');
                                    if let Some(m) = gate_modal.as_mut() {
                                        m.peek_timeline = false;
                                    }
                                    if kind == GateKind::Plan {
                                        if approve {
                                            match approve_plan_and_wake(
                                                client,
                                                root_session_id,
                                                &gate_id,
                                                target_agent_id.as_deref(),
                                            ) {
                                                Ok(msg) => {
                                                    mark_plan_version_resolved(
                                                        &entries,
                                                        &gate_id,
                                                        &mut resolved,
                                                        &mut acted,
                                                    );
                                                    gate_modal = None;
                                                    status = Some(msg);
                                                    follow = true;
                                                    force_timeline_refresh = true;
                                                }
                                                Err(e) => status = Some(e),
                                            }
                                        } else {
                                            gate_modal = None;
                                            compose = Some(ComposeInput::with_prefill(
                                                &format!("Please revise plan {gate_id}: "),
                                            ));
                                            status = Some(
                                                "revision request — edit and Enter to send to planner"
                                                    .to_string(),
                                            );
                                        }
                                    } else {
                                        // Modal stays open during text capture — the in-popup
                                        // input panel (`gate_modal_input_panel_lines`) renders
                                        // the confirm-phrase / motivation box inside the overlay.
                                        // Auto-dismissed by poll_pending_gate on success; on
                                        // error `input` is restored so the operator can retry in
                                        // place. Mirrors the Interaction gate path below.
                                        input = Some(approval_gate_input(
                                            client,
                                            if approve {
                                                GateAction::Approve
                                            } else {
                                                GateAction::Reject
                                            },
                                            gate_id,
                                            &entries,
                                            !approve,
                                        ));
                                        status = None;
                                    }
                                    continue;
                                }
                                _ => {}
                            }
                        } else if input.is_none() && pending_gate.is_none() {
                            match key.code {
                                KeyCode::Esc => {
                                    if let Some(m) = gate_modal.as_mut() {
                                        m.peek_timeline = true;
                                        m.scroll = 0;
                                    }
                                    status = Some(
                                        "approval peeking — browse timeline · g resolve · y/n act"
                                            .to_string(),
                                    );
                                    continue;
                                }
                                KeyCode::Char('y') | KeyCode::Char('n')
                                    if matches!(
                                        modal.gate.kind,
                                        GateKind::Approval
                                            | GateKind::WikiProposal
                                            | GateKind::Escalation
                                            | GateKind::Plan
                                    ) =>
                                {
                                    let gate_id = modal.gate.id.clone();
                                    let kind = modal.gate.kind;
                                    let approve = key.code == KeyCode::Char('y');
                                    if kind == GateKind::Plan {
                                        if approve {
                                            match approve_plan_and_wake(
                                                client,
                                                root_session_id,
                                                &gate_id,
                                                target_agent_id.as_deref(),
                                            ) {
                                                Ok(msg) => {
                                                    mark_plan_version_resolved(
                                                        &entries,
                                                        &gate_id,
                                                        &mut resolved,
                                                        &mut acted,
                                                    );
                                                    gate_modal = None;
                                                    status = Some(msg);
                                                    follow = true;
                                                    force_timeline_refresh = true;
                                                }
                                                Err(e) => status = Some(e),
                                            }
                                        } else {
                                            gate_modal = None;
                                            compose = Some(ComposeInput::with_prefill(
                                                &format!("Please revise plan {gate_id}: "),
                                            ));
                                            status = Some(
                                                "revision request — edit and Enter to send to planner"
                                                    .to_string(),
                                            );
                                        }
                                        continue;
                                    }
                                    // Modal stays open during text capture — the in-popup input
                                    // panel (`gate_modal_input_panel_lines`) renders the
                                    // confirm-phrase / motivation box inside the overlay.
                                    // Auto-dismissed by poll_pending_gate on success; on error
                                    // `input` is restored so the operator can retry in place.
                                    // Mirrors the Interaction gate path below.
                                    input = Some(approval_gate_input(
                                        client,
                                        if approve {
                                            GateAction::Approve
                                        } else {
                                            GateAction::Reject
                                        },
                                        gate_id,
                                        &entries,
                                        !approve,
                                    ));
                                    status = None;
                                    continue;
                                }
                                KeyCode::Char('r') | KeyCode::Enter
                                    if modal.gate.kind == GateKind::Interaction =>
                                {
                                    let (options, allow_freeform) =
                                        interaction_choices(&entries, &modal.gate.id);
                                    input = Some(GateInput {
                                        action: GateAction::Answer,
                                        id: modal.gate.id.clone(),
                                        buffer: String::new(),
                                        options,
                                        allow_freeform,
                                        details_mode: false,
                                        motivation_required: false,
                                        required_confirm_phrase: None,
                                        acknowledged_capabilities: Vec::new(),
                                        secret_fields: Vec::new(),
                                        secret_values: Vec::new(),
                                        secret_phase: false,
                                    });
                                    status = None;
                                    continue;
                                }
                                KeyCode::Char('j') | KeyCode::Down => {
                                    if let Some(m) = gate_modal.as_mut() {
                                        m.scroll = m.scroll.saturating_add(1);
                                    }
                                    continue;
                                }
                                KeyCode::Char('k') | KeyCode::Up => {
                                    if let Some(m) = gate_modal.as_mut() {
                                        m.scroll = m.scroll.saturating_sub(1);
                                    }
                                    continue;
                                }
                                KeyCode::Char('q') => {
                                    if quit_armed(&quit_armed_until) {
                                        break 'room;
                                    }
                                    arm_quit(&mut quit_armed_until, &mut status);
                                    continue;
                                }
                                _ => {
                                    // Let compose / slash triggers fall through to the
                                    // main handler — the full-screen gate overlay must
                                    // not block the operator from messaging the session.
                                    if matches!(key.code, KeyCode::Char('i') | KeyCode::Char('I')
                                        | KeyCode::Char('/') | KeyCode::Char(':'))
                                    {
                                        // fall through
                                    } else {
                                    let ctrl_c = key.code == KeyCode::Char('c')
                                        && key.modifiers.contains(KeyModifiers::CONTROL);
                                    if ctrl_c {
                                        if quit_armed(&quit_armed_until) {
                                            break 'room;
                                        }
                                        arm_quit(&mut quit_armed_until, &mut status);
                                    }
                                    continue;
                                    }
                                }
                            }
                        } else {
                            // Modal is open but a gate RPC is in flight (input already
                            // taken on Enter). Keep navigation responsive: allow scroll
                            // + quit + Esc-back to bare timeline, but block re-arming
                            // y/n/r until the RPC settles — otherwise a second
                            // `input = Some(...)` could race the in-flight resolve and
                            // re-submit the same gate. Applies to every modal kind
                            // (approval/escalation/interaction/plan).
                            match key.code {
                                KeyCode::Char('j') | KeyCode::Down => {
                                    if let Some(m) = gate_modal.as_mut() {
                                        m.scroll = m.scroll.saturating_add(1);
                                    }
                                    continue;
                                }
                                KeyCode::Char('k') | KeyCode::Up => {
                                    if let Some(m) = gate_modal.as_mut() {
                                        m.scroll = m.scroll.saturating_sub(1);
                                    }
                                    continue;
                                }
                                KeyCode::Esc => {
                                    gate_modal = None;
                                    status = Some(
                                        "⏳ still submitting… (Esc closed overlay)".to_string(),
                                    );
                                    continue;
                                }
                                KeyCode::Char('q') => {
                                    if quit_armed(&quit_armed_until) {
                                        break 'room;
                                    }
                                    arm_quit(&mut quit_armed_until, &mut status);
                                    continue;
                                }
                                _ => {
                                    // Let compose / slash triggers fall through to the
                                    // main handler — the overlay must not block messaging.
                                    if matches!(
                                        key.code,
                                        KeyCode::Char('i') | KeyCode::Char('I')
                                            | KeyCode::Char('/')
                                            | KeyCode::Char(':')
                                    ) {
                                        // fall through to compose / slash handlers below
                                    } else {
                                        let ctrl_c = key.code == KeyCode::Char('c')
                                            && key.modifiers.contains(KeyModifiers::CONTROL);
                                        if ctrl_c {
                                            if quit_armed(&quit_armed_until) {
                                                break 'room;
                                            }
                                            arm_quit(&mut quit_armed_until, &mut status);
                                        }
                                        if pending_gate.is_some() && status.is_none() {
                                            status =
                                                Some("⏳ still submitting… (wait or Esc)".to_string());
                                        }
                                        continue;
                                    }
                                }
                            }
                        }
                    }

                    // Grants panel — navigate + two-step confirm revoke. Owns its
                    // keys (ends in `continue;`) so they never reach the timeline.
                    if let Some(ref mut panel) = grants_panel {
                        if panel.rows.is_empty() {
                            grants_panel = None;
                        } else {
                            let ctrl_c = key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL);
                            if matches!(key.code, KeyCode::Char('q')) || ctrl_c {
                                // fall through to global quit
                            } else {
                                let row_count = panel.rows.len();
                                match key.code {
                                    KeyCode::Esc => {
                                        grants_panel = None;
                                    }
                                    KeyCode::Char('j') | KeyCode::Down => {
                                        panel.pending_revoke = None;
                                        if panel.selected + 1 < row_count {
                                            panel.selected += 1;
                                        }
                                    }
                                    KeyCode::Char('k') | KeyCode::Up => {
                                        panel.pending_revoke = None;
                                        panel.selected = panel.selected.saturating_sub(1);
                                    }
                                    KeyCode::Char('g') => {
                                        panel.pending_revoke = None;
                                        panel.selected = 0;
                                    }
                                    KeyCode::Char('G') => {
                                        panel.pending_revoke = None;
                                        panel.selected = row_count.saturating_sub(1);
                                    }
                                    KeyCode::PageDown => {
                                        panel.pending_revoke = None;
                                        panel.selected = (panel.selected + 5).min(row_count - 1);
                                    }
                                    KeyCode::PageUp => {
                                        panel.pending_revoke = None;
                                        panel.selected = panel.selected.saturating_sub(5);
                                    }
                                    KeyCode::Char('r') => {
                                        // Two-step confirm: first `r` arms, second
                                        // `r` on the same row fires the revoke.
                                        let idx = panel.selected;
                                        let row = &panel.rows[idx];
                                        if panel.pending_revoke.is_some() {
                                            let grant_id = row.id;
                                            let kind = row.kind;
                                            match rpc(
                                                client,
                                                "grants.revoke",
                                                serde_json::json!({
                                                    "root_session_id": &*root_session_id,
                                                    "grant_kind": kind.rpc_str(),
                                                    "grant_id": grant_id,
                                                    "revoked_by": "operator:tui",
                                                }),
                                            ) {
                                                Ok(_) => {
                                                    status = Some(format!(
                                                        "✓ revoked {} grant #{}",
                                                        kind.label(),
                                                        grant_id
                                                    ));
                                                    // Re-fetch so the list updates
                                                    // immediately.
                                                    let (rows, taint) =
                                                        fetch_grant_rows(client, &root_session_id);
                                                    panel.rows = rows;
                                                    panel.taint = taint;
                                                    panel.pending_revoke = None;
                                                    panel.clamp_selection();
                                                    needs_redraw = true;
                                                }
                                                Err(e) => {
                                                    status = Some(format!("✗ {e}"));
                                                    panel.pending_revoke = None;
                                                }
                                            }
                                        } else {
                                            panel.pending_revoke = Some(());
                                            status = Some(format!(
                                                "press r again to revoke {} grant #{}",
                                                row.kind.label(),
                                                row.id
                                            ));
                                        }
                                    }
                                    _ => {
                                        // Any other key cancels a pending revoke.
                                        panel.pending_revoke = None;
                                    }
                                }
                                continue;
                            }
                        }
                    }

                    // Approvals popup — list all pending + resolved gates; act directly.
                    if let Some(ref mut popup) = approvals_popup {
                        if popup.rows.is_empty() {
                            approvals_popup = None;
                        } else {
                            let ctrl_c = key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL);
                            if matches!(key.code, KeyCode::Char('q')) || ctrl_c {
                                // fall through to global quit
                            } else if matches!(key.code, KeyCode::Char('i') | KeyCode::Char('I')
                                | KeyCode::Char('/') | KeyCode::Char(':'))
                            {
                                approvals_popup = None;
                                // fall through to compose / slash handlers
                            } else {
                                let row_count = popup.rows.len();
                                match key.code {
                                    KeyCode::Esc => { approvals_popup = None; }
                                    KeyCode::Char('j') | KeyCode::Down => {
                                        if popup.selected + 1 < row_count {
                                            popup.selected += 1;
                                        }
                                    }
                                    KeyCode::Char('k') | KeyCode::Up => {
                                        popup.selected = popup.selected.saturating_sub(1);
                                    }
                                    KeyCode::Char('G') | KeyCode::End => {
                                        popup.selected = row_count.saturating_sub(1);
                                    }
                                    KeyCode::Char('g') | KeyCode::Home => {
                                        popup.selected = 0;
                                    }
                                    KeyCode::Char('y') | KeyCode::Char('n') | KeyCode::Char('o') => {
                                        let idx = popup.selected.min(row_count.saturating_sub(1));
                                        let row = &popup.rows[idx];
                                        if !row.is_pending {
                                            status = Some(format!("already resolved: {}", row.id));
                                        } else if row.kind == "ASK" {
                                            status = Some(
                                                "ASK: Esc to close, then answer from timeline"
                                                    .to_string(),
                                            );
                                        } else if row.kind == "PLAN" {
                                            let approve = key.code == KeyCode::Char('y');
                                            let plan_key = row.id.clone();
                                            let plan_id = plan_key.split(':').next().unwrap_or(&plan_key).to_string();
                                            if approve {
                                                match approve_plan_and_wake(
                                                    client,
                                                    root_session_id,
                                                    &plan_id,
                                                    target_agent_id.as_deref(),
                                                ) {
                                                    Ok(msg) => {
                                                        mark_plan_version_resolved(
                                                            &entries,
                                                            &plan_id,
                                                            &mut resolved,
                                                            &mut acted,
                                                        );
                                                        status = Some(msg);
                                                        follow = true;
                                                        force_timeline_refresh = true;
                                                        approvals_popup = None;
                                                    }
                                                    Err(e) => status = Some(e),
                                                }
                                            } else {
                                                approvals_popup = None;
                                                compose = Some(ComposeInput::with_prefill(
                                                    &format!("Please revise plan {plan_id}: "),
                                                ));
                                                status = Some(
                                                    "revision request — edit and Enter to send to planner"
                                                        .to_string(),
                                                );
                                            }
                                        } else {
                                            let approve = key.code == KeyCode::Char('y')
                                                || key.code == KeyCode::Char('o');
                                            let create_grant = if key.code == KeyCode::Char('o') {
                                                Some(false)
                                            } else {
                                                None
                                            };
                                            let row_id = row.id.clone();
                                            let method = if approve { "approvals.approve" } else { "approvals.reject" };
                                            let mut rpc_params = serde_json::json!({
                                                "request_id": &row_id,
                                                "decided_by": "operator",
                                            });
                                            if let Some(cg) = create_grant {
                                                rpc_params["create_grant"] = serde_json::json!(cg);
                                            }
                                            match rpc(client, method, rpc_params) {
                                                Ok(_) => {
                                                    acted.insert(row_id.clone());
                                                    status = Some(format!(
                                                        "✓ {} {}",
                                                        if !approve {
                                                            "rejected"
                                                        } else if create_grant == Some(false) {
                                                            "approved (one-shot, no grant)"
                                                        } else {
                                                            "approved"
                                                        },
                                                        row_id
                                                    ));
                                                    force_timeline_refresh = true;
                                                    popup.rows = fetch_approval_rows(client, &root_session_id);
                                                    if popup.selected >= popup.rows.len() {
                                                        popup.selected = popup.rows.len().saturating_sub(1);
                                                    }
                                                }
                                                Err(e) => status = Some(format!("✗ {e}")),
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                                continue;
                            }
                        }
                    }

                    let ctrl_c = key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                    if matches!(key.code, KeyCode::Char('q')) || ctrl_c {
                        disarm_estop(&mut estop_armed_until, &mut status);
                        if quit_armed(&quit_armed_until) {
                            break 'room;
                        }
                        arm_quit(&mut quit_armed_until, &mut status);
                        continue;
                    }
                    match key.code {
                        KeyCode::Esc => {
                            // Batch-close: when a sub-view is open from the content pane,
                            // one Esc closes everything back to the main timeline view.
                            if live_content_pane.is_some()
                                && (content_view.is_some()
                                    || artifact_file_view.is_some()
                                    || artifact_viewer.is_some())
                            {
                                content_view = None;
                                artifact_file_view = None;
                                artifact_viewer = None;
                                live_content_pane = None;
                            } else if content_view.is_some() {
                                content_view = None;
                            } else if artifact_file_view.is_some() {
                                artifact_file_view = None;
                            } else if artifact_viewer.is_some() {
                                artifact_viewer = None;
                            } else if live_content_pane.is_some() {
                                live_content_pane = None;
                            } else if info_panel_open {
                                info_panel_open = false;
                                info_scroll = 0;
                            } else if detail.is_some() {
                                detail = None;
                                detail_scroll = 0;
                                detail_h_scroll = 0;
                                session_pick_list = None;
                                        wiki_request_ids = None;
                            } else if search_query.is_some() {
                                // Clear an active search before falling through to
                                // the emergency-stop arming path.
                                search_query = None;
                                search_matches.clear();
                                status = Some("search cleared".to_string());
                            } else if estop_armed(&estop_armed_until) {
                                // Double-Esc within the arm window → emergency stop
                                disarm_estop(&mut estop_armed_until, &mut status);
                                match rpc(
                                    client,
                                    "root_session.emergency_stop",
                                    serde_json::json!({
                                        "root_session_id": &*root_session_id,
                                        "reason": "Interrupted by operator (double Esc in room TUI)",
                                        "requested_by_type": "operator",
                                        "requested_by_id": "session-room",
                                        "trigger_kind": "manual",
                                        "notify_where_practical": true,
                                    }),
                                ) {
                                    Ok(_) => {
                                        status = Some("✓ session interrupted — press i to send a new message, F to fork from a turn".to_string());
                                        force_timeline_refresh = true;
                                        follow = true;
                                    }
                                    Err(e) => {
                                        status = Some(format!("✗ interrupt failed: {e}"));
                                    }
                                }
                            } else {
                                // First Esc with nothing open → arm the interrupt window
                                disarm_quit(&mut quit_armed_until, &mut status);
                                arm_estop(&mut estop_armed_until, &mut status);
                            }
                        }
                        // Number pick from session list: when the detail pane is
                        // showing a numbered session list, a digit 1-9 switches
                        // to that session instantly.
                        KeyCode::Char(c @ '1'..='9') => {
                            if let Some(ref ids) = session_pick_list {
                                let idx = (c as usize) - ('1' as usize);
                                if let Some(picked_id) = ids.get(idx).cloned() {
                                    if picked_id != *root_session_id {
                                        switch_session(
                                            client,
                                            &mut entries,
                                            &mut cursor,
                                            &mut selected,
                                            &mut detail,
                                            &mut follow,
                                            &mut resolved,
                                            &mut acted,
                                            &mut floor,
                                            root_session_id,
                                            target_agent_id,
                                            limit,
                                            &picked_id,
                                            &mut force_timeline_refresh,
                                            &mut spawn_lineage,
                                        );
                                        status = Some(format!("→ switched to session {picked_id}"));
                                    } else {
                                        status = Some(format!("→ already viewing {picked_id}"));
                                    }
                                    detail = None;
                                    session_pick_list = None;
                                        wiki_request_ids = None;
                                }
                            } else if let Some(ref ids) = wiki_request_ids {
                                let idx = (c as usize) - ('1' as usize);
                                if let Some(request_id) = ids.get(idx).cloned() {
                                    detail = Some(DetailPane::event(
                                        wiki_proposal_detail(client, &request_id),
                                        None,
                                    ));
                                    detail_scroll = 0;
                                    detail_h_scroll = 0;
                                    status = Some(format!("wiki proposal {request_id} — Esc to close"));
                                }
                            }
                        }
                        // n/N cycle search matches when a search is active. Guards
                        // give these priority over the gate `n` (reject) below; the
                        // matches vec is recomputed each frame from `search_query`.
                        KeyCode::Char('n') if search_query.is_some() => {
                            if search_matches.is_empty() {
                                status = Some("no matches".to_string());
                            } else {
                                search_current = (search_current + 1) % search_matches.len();
                                selected = search_matches[search_current];
                                follow = false;
                            }
                        }
                        KeyCode::Char('N') if search_query.is_some() => {
                            if search_matches.is_empty() {
                                status = Some("no matches".to_string());
                            } else {
                                search_current = (search_current + search_matches.len() - 1)
                                    % search_matches.len();
                                selected = search_matches[search_current];
                                follow = false;
                            }
                        }
                        // y/n: approve/reject the selected pending approval; y on a
                        // plan row (or in the plan review pane) approves the PlanFrame.
                        KeyCode::Char('y') | KeyCode::Char('n') => {
                            let plan_target = detail
                                .as_ref()
                                .and_then(|d| d.plan_id.clone())
                                .or_else(|| {
                                    view_gate
                                        .as_ref()
                                        .filter(|g| g.kind == GateKind::Plan)
                                        .map(|g| g.id.clone())
                                });
                            if let Some(plan_id) = plan_target {
                                clear_detail(&mut detail, &mut detail_scroll, &mut detail_h_scroll);
                                if key.code == KeyCode::Char('y') {
                                    match approve_plan_and_wake(
                                        client,
                                        root_session_id,
                                        &plan_id,
                                        target_agent_id.as_deref(),
                                    ) {
                                        Ok(msg) => {
                                            mark_plan_version_resolved(
                                                &entries,
                                                &plan_id,
                                                &mut resolved,
                                                &mut acted,
                                            );
                                            status = Some(msg);
                                            follow = true;
                                            force_timeline_refresh = true;
                                        }
                                        Err(e) => status = Some(e),
                                    }
                                } else {
                                    compose = Some(ComposeInput::with_prefill(&format!(
                                        "Please revise plan {plan_id}: "
                                    )));
                                    status = Some(
                                        "revision request — edit and Enter to send to planner"
                                            .to_string(),
                                    );
                                }
                            } else if let Some(g) =
                                view_gate.as_ref().filter(|g| matches!(g.kind, GateKind::Approval | GateKind::WikiProposal | GateKind::Escalation))
                            {
                                clear_detail(&mut detail, &mut detail_scroll, &mut detail_h_scroll);
                                input = Some(approval_gate_input(
                                    client,
                                    if key.code == KeyCode::Char('y') {
                                        GateAction::Approve
                                    } else {
                                        GateAction::Reject
                                    },
                                    g.id.clone(),
                                    &entries,
                                    key.code == KeyCode::Char('n'),
                                ));
                                status = None;
                                continue;
                            }
                        }
                        // r: reply to the selected pending interaction (user.ask) —
                        // load its pre-digested choices so a number key picks one.
                        KeyCode::Char('r') => {
                            if let Some(g) =
                                view_gate.as_ref().filter(|g| g.kind == GateKind::Interaction)
                            {
                                detail = None;
                                let (options, allow_freeform) = interaction_choices(&entries, &g.id);
                                input = Some(GateInput {
                                    action: GateAction::Answer,
                                    id: g.id.clone(),
                                    buffer: String::new(),
                                    options,
                                    allow_freeform,
                                    details_mode: false,
                                    motivation_required: false,
                                    required_confirm_phrase: None,
                                    acknowledged_capabilities: Vec::new(),
                                    secret_fields: Vec::new(),
                                    secret_values: Vec::new(),
                                    secret_phase: false,
                                });
                                status = None;
                            }
                        }
                        KeyCode::Char('p') => {
                            if let Some(g) =
                                view_gate.as_ref().filter(|g| g.kind == GateKind::Plan)
                            {
                                if open_plan_review(
                                    client,
                                    root_session_id,
                                    &g.id,
                                    &mut detail,
                                    &mut detail_scroll,
                                    &mut detail_h_scroll,
                                ) {
                                    status = Some(format!("plan review: {}", g.id));
                                } else {
                                    status = Some(format!("✗ could not load plan {}", g.id));
                                }
                            }
                        }
                        KeyCode::Enter => {
                            if detail.is_some() {
                                clear_detail(&mut detail, &mut detail_scroll, &mut detail_h_scroll);
                            } else if let Some(pane) = live_content_pane.clone() {
                                let _ = open_content_pane_node(
                                    &pane, pane.selected, client, root_session_id,
                                    &mut detail, &mut detail_scroll, &mut detail_h_scroll,
                                    &mut artifact_viewer, &mut artifact_file_view,
                                    &mut content_view, &mut status,
                                    &mut live_content_pane,
                                );
                            } else if let Some((_, src)) = view_indexed.get(selected) {
                                // Open detail for the selected row
                                if let Some(msg) = open_row_detail_or_plan_review(
                                    client,
                                    root_session_id,
                                    &view_visible,
                                    *src,
                                    &mut detail,
                                    &mut detail_scroll,
                                    &mut detail_h_scroll,
                                ) {
                                    status = Some(msg);
                                }
                            }
                        }
                        KeyCode::Char('a') => {
                            floor = cycle_floor(floor);
                            detail = None;
                        }
                        KeyCode::Char('A') => {
                            if content_view.is_some() || artifact_file_view.is_some()
                                || artifact_viewer.is_some() || detail.is_some()
                                || input.is_some() || pending_gate.is_some() || compose.is_some()
                                || gate_modal.is_some()
                            {
                            } else if approvals_popup.is_some() {
                                approvals_popup = None;
                            } else {
                                let rows = fetch_approval_rows(client, &root_session_id);
                                if rows.is_empty() {
                                    status = Some("no approvals in this session".to_string());
                                } else {
                                    approvals_popup = Some(ApprovalsPopup {
                                        selected: 0,
                                        scroll: 0,
                                        rows,
                                    });
                                    status = Some(
                                        "approvals: j/k navigate · y approve+grant · o approve once · n reject · Esc close"
                                            .to_string(),
                                    );
                                }
                            }
                        }
                        // G: grants panel — live view of all active grants for
                        // the root session (both kinds) + the current taint,
                        // with per-row revoke. Toggles off if open.
                        KeyCode::Char('G') => {
                            if content_view.is_some() || artifact_file_view.is_some()
                                || artifact_viewer.is_some() || detail.is_some()
                                || input.is_some() || pending_gate.is_some() || compose.is_some()
                                || gate_modal.is_some() || approvals_popup.is_some()
                            {
                                // don't grab G while another overlay is active
                            } else if grants_panel.is_some() {
                                grants_panel = None;
                            } else {
                                let (rows, taint) = fetch_grant_rows(client, &root_session_id);
                                if rows.is_empty() {
                                    status = Some("no active grants in this session".to_string());
                                } else {
                                    grants_panel = Some(GrantsPanel {
                                        selected: 0,
                                        scroll: 0,
                                        rows,
                                        taint,
                                        pending_revoke: None,
                                    });
                                    last_grants_poll = Instant::now();
                                    status = Some(
                                        "grants: j/k navigate · r revoke (confirm) · Esc close"
                                            .to_string(),
                                    );
                                }
                            }
                        }
                        KeyCode::Char('s') => squash = !squash,
                        // Y: copy the selected row to the clipboard — the
                        // actionable token for a tool row (command/path/ref/id),
                        // else the row's visible text. Handy for grabbing an
                        // artifact ref, a failing command, or an error message.
                        KeyCode::Char('Y') => {
                            if detail.is_none() {
                                // Copy what the selected row shows: a collapsed run
                                // yields its visible summary; a Line yields the
                                // actionable token (command/path/ref/id) or its text.
                                let copy_text = view_indexed.get(selected).and_then(|(row, src)| {
                                    match row {
                                        RenderedRow::Collapsed { count, summary, .. } => {
                                            Some(format!("{count} {summary}"))
                                        }
                                        RenderedRow::Line(_) => {
                                            let idx = match src {
                                                RowSource::Single(i) => *i,
                                                RowSource::Run { start, .. } => *start,
                                            };
                                            view_visible.get(idx).map(render::row_copy_text)
                                        }
                                    }
                                });
                                if let Some(text) = copy_text {
                                    // Prefer the OS clipboard (arboard). It needs a
                                    // display server, so on headless / SSH hosts it's
                                    // unavailable — fall back to an OSC 52 escape,
                                    // which routes through the terminal emulator and
                                    // works remotely when the terminal supports it.
                                    let via_os = clipboard
                                        .as_mut()
                                        .map(|cb| cb.set_text(&text).is_ok())
                                        .unwrap_or(false);
                                    let preview = render::one_line(&text, 60);
                                    if via_os {
                                        status = Some(format!("copied: {preview}"));
                                    } else if copy_via_osc52(&text).is_ok() {
                                        status =
                                            Some(format!("copied (osc52): {preview}"));
                                    } else {
                                        status = Some("✗ copy failed".into());
                                    }
                                }
                            }
                        }
                        // [ / ]: jump to the previous / next first-class
                        // checkpoint (plan, approval, escalation, operator
                        // message, session start). Makes the decision narrative
                        // scannable without scrolling past routine plumbing.
                        KeyCode::Char('[') => {
                            if detail.is_none() && !checkpoint_rows.is_empty() {
                                follow = false;
                                if let Some(&p) =
                                    checkpoint_rows.iter().rev().find(|&&r| r < selected)
                                {
                                    selected = p;
                                } else if let Some(&last) = checkpoint_rows.last() {
                                    selected = last; // wrap to the end
                                }
                            }
                        }
                        KeyCode::Char(']') => {
                            if detail.is_none() && !checkpoint_rows.is_empty() {
                                follow = false;
                                if let Some(&n) =
                                    checkpoint_rows.iter().find(|&&r| r > selected)
                                {
                                    selected = n;
                                } else if let Some(&first) = checkpoint_rows.first() {
                                    selected = first; // wrap to the start
                                }
                            }
                        }
                        // e / E: jump to the next / previous Attention row —
                        // failures, gates, and interventions — so problems are
                        // reachable without hunting. Wraps around; `[`/`]` remain
                        // for the checkpoint (decision) narrative.
                        KeyCode::Char('e') => {
                            if detail.is_none() {
                                if attention_rows.is_empty() {
                                    status = Some("no attention rows (errors/gates) in view".to_string());
                                } else {
                                    follow = false;
                                    selected = attention_rows
                                        .iter()
                                        .find(|&&r| r > selected)
                                        .copied()
                                        .unwrap_or_else(|| attention_rows[0]); // wrap to start
                                }
                            }
                        }
                        KeyCode::Char('E') => {
                            if detail.is_none() {
                                if attention_rows.is_empty() {
                                    status = Some("no attention rows (errors/gates) in view".to_string());
                                } else {
                                    follow = false;
                                    selected = attention_rows
                                        .iter()
                                        .rev()
                                        .find(|&&r| r < selected)
                                        .copied()
                                        .unwrap_or_else(|| *attention_rows.last().unwrap()); // wrap to end
                                }
                            }
                        }
                        // R: toggle the 💭 reasoning prefix on/off everywhere. Off
                        // hides the prefix; the reasoning row itself stays visible
                        // (it's a Detail-altitude event, so it's normally hidden
                        // by the floor or by squash — but the toggle matters for
                        // any channel that doesn't filter on altitude).
                        KeyCode::Char('R') => show_reasoning = !show_reasoning,
                        // F: fork the session from the selected row's turn and
                        // switch to the new branch — backtrack to a past state to
                        // try a different approach. Checkpoints exist only at
                        // yield points, so the gateway rejects turns it has no
                        // checkpoint for (it lists the forkable ones).
                        KeyCode::Char('F') => {
                            match selected_turn_id(&view_indexed, &view_visible, selected)
                                .and_then(|tid| turn_number_of(&tid))
                            {
                                Some(turn) => {
                                    match fork_session(client, root_session_id, Some(turn), None) {
                                        Ok((new_id, fork_turn)) => {
                                            switch_session(
                                                client,
                                                &mut entries,
                                                &mut cursor,
                                                &mut selected,
                                                &mut detail,
                                                &mut follow,
                                                &mut resolved,
                                                &mut acted,
                                                &mut floor,
                                                root_session_id,
                                                target_agent_id,
                                                limit,
                                                &new_id,
                                                &mut force_timeline_refresh,
                                                &mut spawn_lineage,
                                            );
                                            session_pick_list = None;
                                        wiki_request_ids = None;
                                            status = Some(format!(
                                                "→ forked at turn {fork_turn} → {new_id} · send a message to continue this branch"
                                            ));
                                        }
                                        Err(e) => status = Some(e),
                                    }
                                }
                                None => {
                                    status = Some(
                                        "✗ select a row with a turn to fork from (or use /fork --at-turn N)"
                                            .to_string(),
                                    )
                                }
                            }
                        }
                        // i: compose a free-form message into the session (#405).
                        KeyCode::Char('i') | KeyCode::Char('I') => {
                            if let Some(g) =
                                view_gate.as_ref().filter(|g| g.kind == GateKind::Interaction)
                            {
                                let (options, allow_freeform) = interaction_choices(&entries, &g.id);
                                detail = None;
                                input = Some(GateInput {
                                    action: GateAction::Answer,
                                    id: g.id.clone(),
                                    buffer: String::new(),
                                    options,
                                    allow_freeform,
                                    details_mode: false,
                                    motivation_required: false,
                                    required_confirm_phrase: None,
                                    acknowledged_capabilities: Vec::new(),
                                    secret_fields: Vec::new(),
                                    secret_values: Vec::new(),
                                    secret_phase: false,
                                });
                                status = None;
                            } else {
                                gate_modal = None;
                                approvals_popup = None;
                                detail = None;
                                compose = Some(ComposeInput::new());
                                status = None;
                            }
                        }
                        // /: slash-command mode (vim/Discord convention). `:`
                        // and `?` are accepted aliases for muscle memory.
                        KeyCode::Char('/') | KeyCode::Char(':') => {
                            detail = None;
                            info_panel_open = false;
                            artifact_viewer = None;
                            artifact_file_view = None;
                            slash = Some(String::new());
                            status = None;
                        }
                        KeyCode::Char('?') => {
                            if info_panel_open {
                                info_panel_open = false;
                                info_scroll = 0;
                            } else {
                                info_panel_open = true;
                                info_scroll = 0;
                                status = Some("info: j/k scroll · Esc close".to_string());
                            }
                        }
                        // c: toggle the live session content pane — a sectioned tree
                        // showing plans, artifacts, and session drafts.
                        // Navigate with j/k, open with o.
                        KeyCode::Char('c') => {
                            if content_view.is_some() || artifact_file_view.is_some()
                                || artifact_viewer.is_some() || detail.is_some()
                                || input.is_some() || pending_gate.is_some() || compose.is_some()
                            {
                                // don't grab 'c' while another overlay/text input is active
                            } else if live_content_pane.is_some() {
                                live_content_pane = None;
                            } else {
                                let mut all_nodes: Vec<LiveContentNode> = Vec::new();
                                let mut sections: Vec<(usize, &'static str)> = Vec::new();

                                // --- Plans section ---
                                let plan_start = all_nodes.len();
                                // Fetch pending plans
                                let mut plan_families: std::collections::BTreeMap<
                                    String,
                                    Vec<(u32, String, String, Vec<String>)>,
                                > = std::collections::BTreeMap::new();
                                if let Ok(v) = rpc(
                                    client,
                                    "planframes.list_pending",
                                    serde_json::json!({ "root_session_id": &*root_session_id }),
                                ) {
                                    if let Some(arr) = v.get("plans").and_then(|p| p.as_array()) {
                                        for plan in arr {
                                            let plan_id = plan.get("plan_id").and_then(|p| p.as_str()).unwrap_or("").to_string();
                                            let title = plan.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
                                            let version = plan.get("version").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                                            let status = plan
                                                .get("status")
                                                .and_then(|s| s.as_str())
                                                .unwrap_or("awaiting_approval")
                                                .to_string();
                                            let step_titles: Vec<String> = plan
                                                .get("steps")
                                                .and_then(|s| s.as_array())
                                                .map(|steps| {
                                                    steps
                                                        .iter()
                                                        .filter_map(|step| step.get("title").and_then(|t| t.as_str()).map(String::from))
                                                        .collect()
                                                })
                                                .unwrap_or_default();
                                            plan_families
                                                .entry(plan_id)
                                                .or_default()
                                                .push((version, title, status, step_titles));
                                        }
                                    }
                                }
                                // Scan entries for plan.approved events with plan_ids
                                // not already fetched as pending.
                                let mut seen_plan_versions: std::collections::HashSet<(String, u32)> = plan_families
                                    .iter()
                                    .flat_map(|(pid, versions)| {
                                        versions.iter().map(move |(v, _, _, _)| (pid.clone(), *v))
                                    })
                                    .collect();
                                for entry in entries.iter().rev() {
                                    if entry.event_type == "plan.approved" {
                                        if let Some(ref pid) = entry.refs.plan_id {
                                            let event_version = plan_version_for(entry)
                                                .and_then(|v| u32::try_from(v).ok());
                                            let already_seen = match event_version {
                                                Some(v) => seen_plan_versions.contains(&(pid.clone(), v)),
                                                None => seen_plan_versions.iter().any(|(p, _)| p == pid),
                                            };
                                            if !already_seen {
                                                let mut req_params = serde_json::json!({ "plan_id": pid });
                                                if let Some(v) = event_version {
                                                    req_params["version"] = serde_json::json!(v);
                                                }
                                                if let Ok(v) = rpc(client, "planframes.get", req_params) {
                                                    if let Some(plan) = v.get("plan") {
                                                        let title = plan.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
                                                        let version = plan.get("version").and_then(|v| v.as_u64()).and_then(|v| u32::try_from(v).ok()).unwrap_or(1);
                                                        let status = plan
                                                            .get("status")
                                                            .and_then(|s| s.as_str())
                                                            .unwrap_or("approved")
                                                            .to_string();
                                                        let step_titles: Vec<String> = plan
                                                            .get("steps")
                                                            .and_then(|s| s.as_array())
                                                            .map(|steps| {
                                                                steps
                                                                    .iter()
                                                                    .filter_map(|step| step.get("title").and_then(|t| t.as_str()).map(String::from))
                                                                    .collect()
                                                            })
                                                            .unwrap_or_default();
                                                        seen_plan_versions.insert((pid.clone(), version));
                                                        plan_families
                                                            .entry(pid.clone())
                                                            .or_default()
                                                            .push((version, title, status, step_titles));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                for (_plan_id, mut versions) in plan_families {
                                    versions.sort_by(|a, b| a.0.cmp(&b.0).reverse());
                                    let max_version = versions.first().map(|v| v.0).unwrap_or(0);
                                    for (idx, (version, title, status, steps)) in versions.iter().enumerate() {
                                        let is_latest = *version == max_version;
                                        all_nodes.push(LiveContentNode::Plan {
                                            plan_id: _plan_id.clone(),
                                            title: title.clone(),
                                            status: status.clone(),
                                            version: *version,
                                            is_latest,
                                        });
                                        // Steps are only shown for the latest version to avoid
                                        // visual clutter; older versions are folded by default
                                        // and show only the plan header.
                                        if is_latest || !versions.iter().skip(idx + 1).next().is_some() {
                                            // keep latest expanded; older versions add no steps
                                        }
                                        if is_latest {
                                            for step_title in steps {
                                                all_nodes.push(LiveContentNode::PlanStep { title: step_title.clone() });
                                            }
                                        }
                                    }
                                }
                                let plan_count = all_nodes.len() - plan_start;
                                if plan_count > 0 {
                                    sections.push((plan_start, "Plans"));
                                }

                                // --- Artifacts section ---
                                let artifact_start = all_nodes.len();
                                let mut seen_artifact_ids = std::collections::HashSet::new();
                                for entry in entries.iter().rev() {
                                    if let Some(ref aid) = entry.refs.artifact_id {
                                        if seen_artifact_ids.insert(aid.clone()) {
                                            if let Ok(v) = rpc(
                                                client,
                                                "artifact.list_files",
                                                serde_json::json!({
                                                    "artifact_ref": aid,
                                                    "session_id": &*root_session_id,
                                                }),
                                            ) {
                                                let name = v.get("name").and_then(|n| n.as_str()).unwrap_or(aid).to_string();
                                                let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("artifact").to_string();
                                                all_nodes.push(LiveContentNode::Artifact {
                                                    artifact_id: aid.clone(),
                                                    artifact_ref: aid.clone(),
                                                    kind,
                                                    name,
                                                });
                                                // Dependency layers render before files so deps
                                                // group under the artifact header. Same response,
                                                // no extra round-trip.
                                                if let Some(layers) = v.get("layers").and_then(|l| l.as_array()) {
                                                    for layer in layers {
                                                        let layer_id = layer.get("layer_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                                                        if layer_id.is_empty() {
                                                            continue;
                                                        }
                                                        let lname = layer.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                                                        let mount_path = layer.get("mount_path").and_then(|x| x.as_str()).unwrap_or("").to_string();
                                                        let digest = layer.get("digest").and_then(|x| x.as_str()).unwrap_or("").to_string();
                                                        all_nodes.push(LiveContentNode::ArtifactLayer {
                                                            layer_id,
                                                            name: lname,
                                                            mount_path,
                                                            digest,
                                                            artifact_id: aid.clone(),
                                                            artifact_ref: aid.clone(),
                                                        });
                                                    }
                                                }
                                                if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
                                                    for file in files {
                                                        let fname = file.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                                        let alias = file.get("alias").and_then(|a| a.as_str()).unwrap_or("").to_string();
                                                        all_nodes.push(LiveContentNode::ArtifactFile {
                                                            name: fname,
                                                            alias,
                                                            artifact_id: aid.clone(),
                                                            artifact_ref: aid.clone(),
                                                        });
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    if seen_artifact_ids.len() >= 5 {
                                        break;
                                    }
                                }
                                let artifact_count = all_nodes.len() - artifact_start;
                                if artifact_count > 0 {
                                    sections.push((artifact_start, "Artifacts"));
                                }

                                // --- Drafts section ---
                                let draft_start = all_nodes.len();
                                if let Ok(v) = rpc(
                                    client,
                                    "content.list",
                                    serde_json::json!({ "session_id": root_session_id.clone() }),
                                ) {
                                    let drafts: Vec<LiveContentNode> = v
                                        .get("files")
                                        .and_then(|f| f.as_array())
                                        .map(|arr| {
                                            arr.iter().filter_map(|f| {
                                                let name = f.get("name")?.as_str()?;
                                                if name.starts_with("impl_") {
                                                    return None;
                                                }
                                                Some(LiveContentNode::Draft {
                                                    name: name.to_string(),
                                                    alias: f.get("alias").and_then(|a| a.as_str()).unwrap_or("").to_string(),
                                                    visibility: f.get("visibility").and_then(|v| v.as_str()).unwrap_or("session").to_string(),
                                                })
                                            }).collect()
                                        })
                                        .unwrap_or_default();
                                    let draft_count = drafts.len();
                                    all_nodes.extend(drafts);
                                    if draft_count > 0 {
                                        sections.push((draft_start, "Drafts"));
                                    }
                                }

                                if sections.is_empty() {
                                    status = Some("no content found for this session".to_string());
                                } else {
                                    // Preserve existing fold state and selection across rebuilds (e.g. on 'c').
                                    let folded = live_content_pane
                                        .as_ref()
                                        .map(|p| p.folded.clone())
                                        .unwrap_or_default();
                                    let prev_selected = live_content_pane
                                        .as_ref()
                                        .map(|p| p.selected)
                                        .unwrap_or(0);
                                    let mut pane = LiveContentPane {
                                        nodes: all_nodes,
                                        sections,
                                        selected: prev_selected,
                                        scroll: 0,
                                        folded,
                                        artifact_folded: std::collections::HashMap::new(),
                                    };
                                    pane.clamp_selection_to_visible();
                                    live_content_pane = Some(pane);
                                    status = Some("content: j/k navigate · Enter/o open · x fold/unfold · Esc close".to_string());
                                }
                            }
                        }
                        KeyCode::Char('x') => {
                            if let Some(pane) = live_content_pane.as_mut() {
                                pane.toggle_fold();
                            }
                        }
                        KeyCode::Char('o') => {
                            // Open the selected item in the live content pane
                            if content_view.is_some() {
                                // already viewing content; ignore
                            } else if let Some(pane) = live_content_pane.clone() {
                                let _ = open_content_pane_node(
                                    &pane, pane.selected, client, root_session_id,
                                    &mut detail, &mut detail_scroll, &mut detail_h_scroll,
                                    &mut artifact_viewer, &mut artifact_file_view,
                                    &mut content_view, &mut status,
                                    &mut live_content_pane,
                                );
                            } else if artifact_file_view.is_some() {
                            } else if let Some(ref viewer) = artifact_viewer {
                                if let Some(file) = viewer.files.get(viewer.selected) {
                                    let result = rpc(
                                        client,
                                        "artifact.read_file",
                                        serde_json::json!({
                                            "artifact_ref": viewer.artifact_ref,
                                            "file_name": file.name,
                                            "session_id": root_session_id.clone(),
                                        }),
                                    );
                                    match result {
                                        Ok(v) => {
                                            if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                                                artifact_file_view = Some(ArtifactFileView {
                                                    artifact_id: viewer.artifact_id.clone(),
                                                    file_name: file.name.clone(),
                                                    content: content.to_string(),
                                                    scroll: 0,
                                                });
                                            } else {
                                                status = Some("artifact.read_file: no content field".to_string());
                                            }
                                        }
                                        Err(e) => status = Some(format!("artifact read failed: {e}")),
                                    }
                                }
                            } else if let Some((_, src)) = view_indexed.get(selected) {
                                let idx = match src {
                                    RowSource::Single(i) => *i,
                                    RowSource::Run { start, .. } => *start,
                                };
                                if let Some(entry) = view_visible.get(idx) {
                                    if let Some(artifact_ref) = artifact_ref_for_entry(entry) {
                                        let result = rpc(
                                            client,
                                            "artifact.list_files",
                                            serde_json::json!({
                                                "artifact_ref": artifact_ref,
                                                "session_id": root_session_id.clone(),
                                            }),
                                        );
                                        match result {
                                            Ok(v) => {
                                                let resolved_id = v.get("artifact_id").and_then(|i| i.as_str()).unwrap_or(&artifact_ref).to_string();
                                                let files: Vec<ArtifactFileEntry> = v.get("files")
                                                    .and_then(|f| f.as_array())
                                                    .map(|arr| {
                                                        arr.iter().filter_map(|item| {
                                                            Some(ArtifactFileEntry {
                                                                name: item.get("name")?.as_str()?.to_string(),
                                                                alias: item.get("alias").and_then(|a| a.as_str()).unwrap_or("").to_string(),
                                                            })
                                                        }).collect()
                                                    })
                                                    .unwrap_or_default();
                                                let layers: Vec<ArtifactLayerEntry> = v.get("layers")
                                                    .and_then(|l| l.as_array())
                                                    .map(|arr| {
                                                        arr.iter().filter_map(|item| {
                                                            let layer_id = item.get("layer_id")?.as_str()?.to_string();
                                                            Some(ArtifactLayerEntry {
                                                                layer_id,
                                                                name: item.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                                                mount_path: item.get("mount_path").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                                                digest: item.get("digest").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                                            })
                                                        }).collect()
                                                    })
                                                    .unwrap_or_default();
                                                if files.is_empty() && layers.is_empty() {
                                                    status = Some("artifact has no files or layers".to_string());
                                                } else {
                                                    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("unknown").to_string();
                                                    artifact_viewer = Some(ArtifactViewer {
                                                        artifact_id: resolved_id,
                                                        artifact_ref,
                                                        kind,
                                                        files,
                                                        layers,
                                                        selected: 0,
                                                        scroll: 0,
                                                    });
                                                    status = Some("artifact: o to view · Esc close".to_string());
                                                }
                                            }
                                            Err(e) => status = Some(format!("artifact list failed: {e}")),
                                        }
                                    } else {
                                        let tool_name = entry.payload.as_ref()
                                            .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                                            .and_then(|v| v.get("tool_name").and_then(|t| t.as_str()).map(String::from))
                                            .unwrap_or_default();
                                        status = Some(format!("no artifact on this row (type={} tool={})", entry.event_type, tool_name));
                                    }
                                }
                            }
                        }
                        // m: comment on the file currently open in the content
                        // viewer. Opens the composer in comment mode; the comment
                        // anchors to the viewed version and is delivered to the
                        // agent at its next turn. Prefix the body with `L12:` or
                        // `L12-14:` to attach an optional line hint.
                        KeyCode::Char('m') if content_view.is_some() => {
                            if let Some(view) = content_view.as_ref() {
                                compose_comment =
                                    Some((view.name.clone(), view.handle.clone()));
                                compose = Some(ComposeInput::new());
                                status = Some(format!(
                                    "comment on `{}` · prefix `L12:` or `L12-14:` for a line hint · Enter send · Esc cancel",
                                    view.name
                                ));
                            }
                        }
                        // O: project the live session drafts to a real directory
                        // and open it in an external editor (read-only snapshot).
                        // Available whenever the content pane is up.
                        KeyCode::Char('O')
                            if content_view.is_some() || live_content_pane.is_some() =>
                        {
                            status = Some(project_live_and_open(client, root_session_id));
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if let Some(view) = content_view.as_mut() {
                                view.scroll = view.scroll.saturating_add(1);
                            } else if let Some(pane) = live_content_pane.as_mut() {
                                pane.select_next_visible();
                            } else if artifact_file_view.is_some() {
                                artifact_file_view.as_mut().unwrap().scroll = artifact_file_view.as_ref().unwrap().scroll.saturating_add(1);
                            } else if artifact_viewer.is_some() {
                                let viewer = artifact_viewer.as_mut().unwrap();
                                viewer.selected = (viewer.selected + 1).min(viewer.files.len().saturating_sub(1));
                            } else if info_panel_open {
                                info_scroll = info_scroll.saturating_add(1);
                            } else if detail.is_some() {
                                detail_scroll = detail_scroll.saturating_add(1);
                            } else {
                                follow = false;
                                selected =
                                    (selected + 1).min(view_rows.len().saturating_sub(1));
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if let Some(view) = content_view.as_mut() {
                                view.scroll = view.scroll.saturating_sub(1);
                            } else if let Some(pane) = live_content_pane.as_mut() {
                                pane.select_prev_visible();
                            } else if artifact_file_view.is_some() {
                                artifact_file_view.as_mut().unwrap().scroll = artifact_file_view.as_ref().unwrap().scroll.saturating_sub(1);
                            } else if artifact_viewer.is_some() {
                                let viewer = artifact_viewer.as_mut().unwrap();
                                viewer.selected = viewer.selected.saturating_sub(1);
                            } else if info_panel_open {
                                info_scroll = info_scroll.saturating_sub(1);
                            } else if detail.is_some() {
                                detail_scroll = detail_scroll.saturating_sub(1);
                            } else {
                                follow = false;
                                selected = selected.saturating_sub(1);
                            }
                        }
                        KeyCode::PageDown => {
                            if detail.is_some() {
                                if let Ok(size) = terminal.size() {
                                    let step = detail_page_step(size.height);
                                    detail_scroll = detail_scroll.saturating_add(step);
                                }
                            } else {
                                follow = false;
                                if let Ok(size) = terminal.size() {
                                    let step = main_list_page_step(size.height, compose.is_some());
                                    selected = (selected + step)
                                        .min(view_rows.len().saturating_sub(1));
                                }
                            }
                        }
                        KeyCode::PageUp => {
                            if detail.is_some() {
                                if let Ok(size) = terminal.size() {
                                    let step = detail_page_step(size.height);
                                    detail_scroll = detail_scroll.saturating_sub(step);
                                }
                            } else {
                                follow = false;
                                if let Ok(size) = terminal.size() {
                                    let step = main_list_page_step(size.height, compose.is_some());
                                    selected = selected.saturating_sub(step);
                                }
                            }
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            if detail.is_some() {
                                detail_h_scroll = detail_h_scroll.saturating_add(4);
                            }
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            if detail.is_some() {
                                detail_h_scroll = detail_h_scroll.saturating_sub(4);
                            }
                        }
                        KeyCode::Char('g') | KeyCode::Home => {
                            follow = false;
                            detail = None;
                            selected = 0;
                        }
                        KeyCode::Char('G') | KeyCode::End => {
                            follow = true;
                            detail = None;
                        }
                        // Ctrl+F opens search typing; guarded arm must precede the
                        // plain `f` (follow) arm since the match ignores modifiers.
                        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            search_input = Some(String::new());
                            status = Some("search: ▏  (type, Enter find · Esc cancel)".to_string());
                        }
                        KeyCode::Char('f') | KeyCode::Char(' ') => {
                            follow = !follow;
                            if follow {
                                detail = None;
                            }
                            status = Some(if follow { "following newest".into() } else { "follow paused".into() });
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => {
                    repaint_after_input = true;
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            if detail.is_some() {
                                detail_scroll = detail_scroll.saturating_sub(1);
                            } else if selected > 0 {
                                selected = selected.saturating_sub(1);
                                follow = false;
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if detail.is_some() {
                                detail_scroll = detail_scroll.saturating_add(1);
                            } else if selected < view_row_count.saturating_sub(1) {
                                selected =
                                    (selected + 1).min(view_row_count.saturating_sub(1));
                                follow = false;
                            }
                        }
                        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                            let click_row = click_to_row_index(
                                mouse.row,
                                1u16,
                                view_list_height,
                                view_viewport_offset,
                                &view_row_heights,
                            );
                            if let Some(idx) = click_row {
                                selected = idx;
                                follow = false;
                                if detail.is_some() {
                                    clear_detail(
                                        &mut detail,
                                        &mut detail_scroll,
                                        &mut detail_h_scroll,
                                    );
                                    last_mouse_click = None;
                                } else if click_opens_detail(
                                    &mut last_mouse_click,
                                    Instant::now(),
                                    idx,
                                    mouse.column,
                                    mouse.row,
                                ) {
                                    if let Some((_, src)) = view_indexed.get(idx) {
                                        let _ = open_row_detail_or_plan_review(
                                            client,
                                            root_session_id,
                                            &view_visible,
                                            *src,
                                            &mut detail,
                                            &mut detail_scroll,
                                            &mut detail_h_scroll,
                                        );
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if repaint_after_input {
            needs_redraw = true;
        }

        // Check whether a background gate RPC has finished. This keeps the TUI
        // responsive during slow gateway answers and gives the operator an
        // immediate success/error status without blocking the event loop.
        if let Some(mut pending) = pending_gate.take() {
            if let Some(result) = poll_pending_gate(&mut pending) {
                match result {
                    Ok(msg) => {
                        acted.insert(pending.gi.as_ref().map(|g| g.id.clone()).unwrap_or_default());
                        if gate_modal
                            .as_ref()
                            .is_some_and(|m| m.gate.id == pending.gi.as_ref().map(|g| g.id.clone()).unwrap_or_default())
                        {
                            gate_modal = None;
                        }
                        status = Some(msg);
                        follow = true;
                        force_timeline_refresh = true;
                    }
                    Err((msg, gi)) => {
                        status = Some(msg);
                        input = Some(gi);
                    }
                }
                needs_redraw = true;
            } else {
                pending_gate = Some(pending);
            }
        }

        // Paint immediately after input so detail panes and scroll don't wait on
        // a blocking timeline RPC or a full row-height pass over the list.
        // Use cached turn_boundaries and spinner to avoid visual jumps vs the
        // next full refresh (empty boundaries change row layout; spinner advances
        // skip frames in double-draw).
        if repaint_after_input {
            let early_spinner = SPINNER_FRAMES[spinner_frame];
            let early_stats = compute_session_stats(&entries);
            let early_gate_count = count_active_gates(&entries, &resolved, &acted);
            let early_approval_rows = collect_approval_rows(&entries, &resolved, &acted);
            let early_pending_plans =
                unresolved_pending_plan_ids(&entries, &resolved, &acted).len();
            let early_safe_selected = selected.min(view_rows.len().saturating_sub(1));
            let early_spawn = view_indexed
                .get(early_safe_selected)
                .and_then(|(_, src)| spawn_agent_for_row_source(&view_visible, *src));
            let early_gate = active_gate(
                &entries,
                &view_visible,
                view_indexed.get(early_safe_selected),
                &resolved,
                &acted,
            );
            let early_info = if info_panel_open {
                Some(build_info_panel(
                    root_session_id,
                    TuiChannel.kind(),
                    &early_stats,
                    floor,
                    squash,
                    follow,
                    show_reasoning,
                    view_row_count,
                    checkpoint_rows.len(),
                    early_gate.as_ref(),
                    early_pending_plans,
                    status.as_deref(),
                    early_spawn.as_deref(),
                ))
            } else {
                None
            };
            terminal.draw(|f| {
                draw(
                    f,
                    root_session_id,
                    floor,
                    squash,
                    follow,
                    if follow { None } else { Some(view_viewport_offset) },
                    &view_rows,
                    selected,
                    detail.as_ref(),
                    detail_scroll,
                    detail_h_scroll,
                    input.as_ref(),
                    compose.as_ref(),
                    slash.as_deref(),
                    status.as_deref(),
                    early_gate.as_ref(),
                    early_spinner,
                    &view_turn_boundaries,
                    show_reasoning,
                    &early_stats,
                    early_pending_plans,
                    early_spawn.as_deref(),
                    early_info.as_ref(),
                    info_scroll,
                    early_gate_count,
                    artifact_viewer.as_ref(),
                    artifact_file_view.as_ref(),
                    live_content_pane.as_ref(),
                    content_view.as_ref(),
                    gate_modal.as_ref(),
                    gate_modal
                        .as_ref()
                        .and_then(|m| gate_entry_for_ref(&entries, &m.gate)),
                    None,
                    &early_approval_rows,
                    grants_panel.as_ref(),
                )
            })?;
        }

        if view_rows.is_empty() {
            if status.is_none() {
                status = Some("Loading timeline…".to_string());
            }
            let boot_stats = compute_session_stats(&entries);
            terminal.draw(|f| {
                draw(
                    f,
                    root_session_id,
                    floor,
                    squash,
                    follow,
                    if follow { None } else { Some(view_viewport_offset) },
                    &view_rows,
                    selected,
                    detail.as_ref(),
                    detail_scroll,
                    detail_h_scroll,
                    input.as_ref(),
                    compose.as_ref(),
                    slash.as_deref(),
                    status.as_deref(),
                    view_gate.as_ref(),
                    SPINNER_FRAMES[spinner_frame],
                    &view_turn_boundaries,
                    show_reasoning,
                    &boot_stats,
                    0,
                    None,
                    None,
                    info_scroll,
                    0,
                    artifact_viewer.as_ref(),
                    artifact_file_view.as_ref(),
                    live_content_pane.as_ref(),
                    content_view.as_ref(),
                    gate_modal.as_ref(),
                    gate_modal
                        .as_ref()
                        .and_then(|m| gate_entry_for_ref(&entries, &m.gate)),
                    approvals_popup.as_ref(),
                    &[],
                    grants_panel.as_ref(),
                )
            })?;
        }

        let mut entries_changed = false;
        let timeline_poll_ms = if cached_open_turns.is_empty()
            && !session_async_processing
            && pending_gate.is_none()
        {
            IDLE_TIMELINE_POLL_MS
        } else {
            TIMELINE_POLL_MS
        };
        if force_timeline_refresh
            || last_timeline_poll.elapsed() >= Duration::from_millis(timeline_poll_ms)
        {
            last_timeline_poll = Instant::now();
            force_timeline_refresh = false;
            // Fetch at most one page per poll via the gateway API. On error (gateway
            // down), surface it and keep retrying — don't crash the UI.
            let timeline_params = serde_json::json!({
                "root_session_id": &*root_session_id,
                "after_event_id": cursor,
                "limit": limit,
                // Always fetch at `detail` and filter for display below. Approval
                // resolution events are `Normal` altitude, so fetching at the
                // display floor (e.g. Attention) would drop them and leave
                // `resolved` unpopulated — making already-decided gates look
                // re-decidable. Fetch everything; filter is purely a view concern.
                "min_altitude": "detail",
            });
            let timeline_result = rpc(
                client,
                "session.timeline.list",
                timeline_params,
            );
            match timeline_result {
                Ok(value) => match serde_json::from_value::<SessionTimelineListResult>(value) {
                    Ok(page) => {
                        if !page.spawn_lineage.is_empty() {
                            spawn_lineage = page
                                .spawn_lineage
                                .into_iter()
                                .map(|e| (e.child_session_id.clone(), e))
                                .collect();
                            entries_changed = true;
                        }
                        if let Some(last) = page.entries.last() {
                            cursor = Some(last.event_id.clone());
                        }
                        for e in &page.entries {
                            record_timeline_resolution(&mut resolved, e);
                            // Emergency stop terminates the session tree; stop
                            // treating it as async-processing so idle
                            // optimization kicks in immediately.
                            if e.event_type == "session.emergency_stop" && session_async_processing {
                                session_async_processing = false;
                                needs_redraw = true;
                            }
                        }
                        if !page.entries.is_empty() {
                            entries_changed = true;
                            entries.extend(page.entries);
                        }
                        if status.as_deref().map(|s| s.starts_with("✗ gateway")).unwrap_or(false)
                        {
                            status = None; // recovered
                            needs_redraw = true;
                        }
                    }
                    Err(e) => {
                        status = Some(format!("✗ bad timeline response: {e}"));
                        needs_redraw = true;
                    }
                },
                Err(e) if e.to_string() == "__room_quit__" => break 'room,
                Err(e) => {
                    status = Some(format!("✗ gateway: {e}"));
                    needs_redraw = true;
                }
            }

            // Refresh the forkable-turn set when the timeline grew (new
            // checkpoints appear with new yield points). Cheap, non-critical RPC.
            if entries_changed {
                match rpc(
                    client,
                    "session.checkpoints",
                    serde_json::json!({ "source_session_id": &*root_session_id }),
                ) {
                    Ok(value) => {
                        if let Some(arr) = value.get("turns").and_then(|v| v.as_array()) {
                            let new_set: HashSet<u64> =
                                arr.iter().filter_map(|v| v.as_u64()).collect();
                            if new_set != forkable_turns {
                                forkable_turns = new_set;
                                needs_redraw = true;
                            }
                        }
                    }
                    Err(e) if e.to_string() == "__room_quit__" => break 'room,
                    Err(_) => {} // non-critical; dividers just won't mark forkable
                }
            }
        }

        // Session status poll runs even when we skip rendering so the TUI keeps
        // tracking whether the root session is actively processing.
        let mut session_async_processing_changed = false;
        if last_session_status_poll.elapsed()
            >= Duration::from_millis(SESSION_STATUS_POLL_MS)
        {
            last_session_status_poll = Instant::now();
            let new_async = match rpc(
                client,
                "session.status",
                serde_json::json!({ "session_id": &*root_session_id }),
            ) {
                Ok(value) => value
                    .get("status")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s == "processing"),
                Err(_) => false,
            };
            if new_async != session_async_processing {
                session_async_processing = new_async;
                session_async_processing_changed = true;
                needs_redraw = true;
            }
        }

        // Grants panel idle refresh — only poll when the panel is open, so a
        // closed panel never spams `grants.list`. Re-fetch rows + taint and
        // replace the panel's data, preserving the selection (mirror
        // LiveContentPane's rebuild pattern).
        if grants_panel.is_some()
            && last_grants_poll.elapsed() >= Duration::from_millis(SESSION_STATUS_POLL_MS)
        {
            last_grants_poll = Instant::now();
            let (rows, taint) = fetch_grant_rows(client, &root_session_id);
            if let Some(panel) = grants_panel.as_mut() {
                let prev_selected = panel.selected;
                let changed = rows.len() != panel.rows.len()
                    || rows
                        .iter()
                        .zip(panel.rows.iter())
                        .any(|(a, b)| a.id != b.id || a.kind != b.kind);
                if changed {
                    panel.rows = rows;
                    panel.taint = taint;
                    panel.selected = prev_selected;
                    panel.clamp_selection();
                    needs_redraw = true;
                }
            }
        }

        // Recompute open turns only when the timeline changed; otherwise reuse
        // the cached set to avoid scanning all entries every frame.
        // An emergency stop terminates every open turn, even if no matching
        // turn.end events arrive, so idle-frame optimization still kicks in.
        let mut terminal_stop_seen = false;
        if entries_changed || !cached_open_turns_valid {
            cached_open_turns.clear();
            for e in &entries {
                match e.event_type.as_str() {
                    "turn.start" => {
                        if let Some(t) = &e.turn_id {
                            cached_open_turns.insert(t.clone());
                        }
                    }
                    "turn.end" => {
                        if let Some(t) = &e.turn_id {
                            cached_open_turns.remove(t);
                        }
                    }
                    "session.emergency_stop" => {
                        terminal_stop_seen = true;
                    }
                    _ => {}
                }
            }
            if terminal_stop_seen {
                cached_open_turns.clear();
            }
            cached_open_turns_valid = true;
        }

        // View configuration (floor/squash/reasoning) only changes through input,
        // which already sets needs_redraw, but keep explicit trackers so a
        // programmatic change cannot be accidentally skipped.
        if floor != cached_floor
            || squash != cached_squash
            || show_reasoning != cached_show_reasoning
        {
            cached_floor = floor;
            cached_squash = squash;
            cached_show_reasoning = show_reasoning;
            needs_redraw = true;
        }

        // Only animate the spinner when something is actually in flight. Idle
        // sessions therefore freeze the spinner and skip redraws entirely.
        let has_in_flight_visual = !cached_open_turns.is_empty()
            || session_async_processing
            || pending_gate.is_some();
        if has_in_flight_visual {
            spinner_frame = (spinner_frame + 1) % SPINNER_FRAMES.len();
            needs_redraw = true;
        }
        let spinner_glyph = SPINNER_FRAMES[spinner_frame];

        let should_render = needs_redraw
            || entries_changed
            || session_async_processing_changed
            || has_in_flight_visual;

        if should_render {
            // `entries` holds everything (fetched at `detail`); the display floor is
            // applied here as a pure view filter. RowSource indices below therefore
            // index into `visible`, so gate selection and drill-down use it too.
            let visible: Vec<SessionTimelineEntry> =
                entries.iter().filter(|e| e.altitude >= floor).cloned().collect();
            // Detect in-flight turns: any turn_id we've seen `turn.start` for but
            // not yet `turn.end` is still open. The TUI marks the most recent row
            // in such a turn with a spinner.
            let open_turns = cached_open_turns.clone();
            // Rows + their source mapping (lets Enter drill into the underlying event).
            let linked_escalation_approvals =
                render::linked_promotion_escalation_approval_ids(&visible);
            let mut indexed: Vec<(RenderedRow, RowSource)> = if squash {
                render::coalesce_indexed(&visible)
            } else {
                visible
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| {
                        !render::is_redundant_promotion_escalation_approval(
                            e,
                            &linked_escalation_approvals,
                        )
                    })
                    .map(|(i, e)| (RenderedRow::Line(render::render_spec(e)), RowSource::Single(i)))
                    .collect()
            };
            // Annotate each row with turn membership + the in-flight bit, and
            // track the previous turn_id so the renderer can draw a faint
            // divider when the turn changes. The in-flight spinner is reserved
            // for the **most recent** row in an open turn — earlier rows of the
            // same turn stay in their normal altitude glyph so the operator can
            // read the chain.
        let mut extra_inflight_rows = HashSet::new();
        if let Some(gate) = find_active_gate(&entries, &resolved, &acted) {
            if let Some(row_idx) =
                newest_gate_row_index(&visible, &indexed, &entries, &resolved, &acted, &gate)
            {
                extra_inflight_rows.insert(row_idx);
            }
        }
        if session_async_processing {
            if let Some(row_idx) = last_line_row_index(&indexed) {
                extra_inflight_rows.insert(row_idx);
            }
        }
        let mut turn_boundaries = annotate_turns_and_in_flight(
            &mut indexed,
            &visible,
            &entries,
            &open_turns,
            show_reasoning,
            &extra_inflight_rows,
            &*root_session_id,
            &spawn_lineage,
        );
        // Repurpose the boundary flag to mean "this turn is forkable" so the
        // divider can render distinctly. (Presence in the map still means "draw
        // a divider here"; the bool now carries forkability.)
        for (row_idx, forkable) in turn_boundaries.iter_mut() {
            let turn = match indexed.get(*row_idx).map(|(r, _)| r) {
                Some(RenderedRow::Line(spec)) => spec.turn_index.map(|n| n as u64),
                _ => None,
            };
            *forkable = turn.is_some_and(|t| forkable_turns.contains(&t));
        }
        let rows: Vec<RenderedRow> = indexed.iter().map(|(r, _)| r.clone()).collect();
        let pending_plan_count =
            unresolved_pending_plan_ids(&entries, &resolved, &acted).len();

        // First-class checkpoint view-row indices — plan/approval/escalation/
        // operator/session-start events that survived coalescing. Collapsed
        // runs are never checkpoints (checkpoints always render individually).
        checkpoint_rows = indexed
            .iter()
            .enumerate()
            .filter_map(|(vi, (_, src))| match src {
                RowSource::Single(i) => {
                    visible.get(*i).filter(|e| render::is_checkpoint(e)).map(|_| vi)
                }
                _ => None,
            })
            .collect();

        // Attention view-rows — failures and other high-altitude events the
        // operator should be able to jump to directly (`e` / `E`). Uses the
        // rendered row's altitude so collapsed runs (always Detail) are excluded.
        attention_rows = rows
            .iter()
            .enumerate()
            .filter_map(|(vi, row)| match row {
                RenderedRow::Line(spec) if spec.altitude >= Altitude::Attention => Some(vi),
                _ => None,
            })
            .collect();

        // Recompute search matches each frame so they track the live timeline.
        if let Some(q) = search_query.clone() {
            search_matches = compute_search_matches(&indexed, &visible, &q);
            if search_matches.is_empty() {
                status = Some(format!("search \"{q}\": no matches (Esc clear)"));
            } else {
                if search_needs_jump {
                    search_current = 0;
                    selected = search_matches[0];
                    follow = false;
                    search_needs_jump = false;
                }
                if search_current >= search_matches.len() {
                    search_current = 0;
                }
                status = Some(format!(
                    "search \"{q}\": {}/{}  (n/N cycle · Esc clear)",
                    search_current + 1,
                    search_matches.len()
                ));
            }
        } else {
            search_matches.clear();
        }

        // Keep an open plan-review pane aligned with the latest pending revision
        // (e.g. v2 amend while v1 review was still on screen).
        if input.is_none() && pending_gate.is_none() && compose.is_none() {
            if let Some(plan_id) = detail
                .as_ref()
                .and_then(|d| d.plan_id.clone())
            {
                let _ = open_plan_review(
                    client,
                    root_session_id,
                    &plan_id,
                    &mut detail,
                    &mut detail_scroll,
                    &mut detail_h_scroll,
                );
            }
        }

        let new_plan = if input.is_none() && pending_gate.is_none() && compose.is_none() {
            newest_pending_plan_event(&visible, &indexed, &resolved, &acted)
        } else {
            None
        };
        if let Some((row_idx, plan_id, event_id)) = new_plan {
            if last_announced_plan_event.as_deref() != Some(event_id.as_str()) {
                last_announced_plan_event = Some(event_id);
                selected = row_idx;
                follow = false;
                // Plans are critical gates and use the blocking GateModal (handled
                // below). Keep the status hint so the operator knows why the modal
                // appeared.
                if !status.as_deref().is_some_and(|s| s.starts_with("✗")) {
                    status = Some(format!(
                        "⚠ Plan {plan_id} awaiting approval — y approve · n revise · Esc close"
                    ));
                }
            }
        } else if follow {
            selected = rows.len().saturating_sub(1);
        } else {
            selected = selected.min(rows.len().saturating_sub(1));
        }

        if input.is_none() && pending_gate.is_none() && compose.is_none() && slash.is_none() {
            if let Some((gate_ref, event_id)) =
                newest_blocking_gate_event(&entries, &resolved, &acted)
            {
                let needs_open = gate_modal
                    .as_ref()
                    .map(|m| m.gate.id != gate_ref.id)
                    .unwrap_or(true);
                if needs_open && last_announced_gate_event.as_deref() != Some(event_id.as_str())
                {
                    last_announced_gate_event = Some(event_id);
                    if let Some(row_idx) = newest_gate_row_index(
                        &visible,
                        &indexed,
                        &entries,
                        &resolved,
                        &acted,
                        &gate_ref,
                    ) {
                        selected = row_idx;
                        follow = false;
                    }
                    detail = None;
                    info_panel_open = false;
                    artifact_viewer = None;
                    artifact_file_view = None;
                    let inspect_lines = gate_detail_for_modal(client, root_session_id, &gate_ref);
                    let plan_version = gate_entry_for_ref(&entries, &gate_ref)
                        .and_then(|e| plan_version_for(e));
                    gate_modal = Some(GateModal {
                        gate: gate_ref,
                        scroll: 0,
                        peek_timeline: false,
                        inspect_lines,
                        plan_version,
                    });
                }
            }
        }
        if let Some(modal) = &gate_modal {
            let still_active = find_active_gate(&entries, &resolved, &acted)
                .is_some_and(|g| g.id == modal.gate.id);
            if !still_active {
                gate_modal = None;
            } else if modal.gate.kind == GateKind::Plan {
                // A plan amendment can replace v1 with v2 while the modal is open
                // (same plan_id, higher version). Refresh inspect_lines so the
                // operator always reviews the live pending revision.
                let live_version = gate_entry_for_ref(&entries, &modal.gate)
                    .and_then(|e| plan_version_for(e));
                if live_version != modal.plan_version {
                    let refreshed = gate_detail_for_modal(client, root_session_id, &modal.gate);
                    gate_modal = Some(GateModal {
                        gate: modal.gate.clone(),
                        scroll: modal.scroll,
                        peek_timeline: modal.peek_timeline,
                        inspect_lines: refreshed,
                        plan_version: live_version,
                    });
                }
            }
        }

        let gate = active_gate(&entries, &visible, indexed.get(selected), &resolved, &acted);

        let session_stats = compute_session_stats(&entries);

        let term_size = terminal.size()?;
        let compose_open = compose.is_some() && detail.is_none();
        let list_area_height = if compose_open {
            term_size.height.saturating_sub(1 + FOOTER_HEIGHT + COMPOSE_PANEL_HEIGHT)
        } else {
            term_size.height.saturating_sub(1 + FOOTER_HEIGHT)
        };
        let list_height = list_area_height as usize;
        let width = term_size.width as usize;
        let rail_w = 2usize;
        let glyph_w = 3usize;
        let label_w = 12usize.min(width / 4);
        let content_w = width.saturating_sub(rail_w + glyph_w + label_w + 2);
        let row_heights: Vec<usize> = if detail.is_some() && input.is_none() {
            vec![1; rows.len()]
        } else {
            (0..rows.len())
                .map(|i| match &rows[i] {
                    RenderedRow::Line(spec) => {
                        build_rich_row_lines(
                            spec, i, &turn_boundaries, content_w, glyph_w, rail_w, label_w,
                            spinner_glyph, show_reasoning,
                        )
                        .len()
                    }
                    RenderedRow::Collapsed { .. } => 1,
                })
                .collect()
        };
        let row_count = rows.len();
        let safe_selected = selected.min(row_count.saturating_sub(1));
        let selected_spawn_agent = indexed
            .get(safe_selected)
            .and_then(|(_, src)| spawn_agent_for_row_source(&visible, *src));
        let viewport_offset = if follow {
            compute_viewport_offset(row_count.saturating_sub(1), list_height, &row_heights, None)
        } else {
            compute_viewport_offset(safe_selected, list_height, &row_heights, Some(view_viewport_offset))
        };
        let gate_count = count_active_gates(&entries, &resolved, &acted);
        let approval_rows = collect_approval_rows(&entries, &resolved, &acted);
        let info_panel = if info_panel_open {
            Some(build_info_panel(
                root_session_id,
                TuiChannel.kind(),
                &session_stats,
                floor,
                squash,
                follow,
                show_reasoning,
                row_count,
                checkpoint_rows.len(),
                gate.as_ref(),
                pending_plan_count,
                status.as_deref(),
                selected_spawn_agent.as_deref(),
            ))
        } else {
            None
        };

        terminal.draw(|f| {
            draw(
                f,
                root_session_id,
                floor,
                squash,
                follow,
                if follow { None } else { Some(view_viewport_offset) },
                &rows,
                selected,
                detail.as_ref(),
                detail_scroll,
                detail_h_scroll,
                input.as_ref(),
                compose.as_ref(),
                slash.as_deref(),
                status.as_deref(),
                gate.as_ref(),
                spinner_glyph,
                &turn_boundaries,
                show_reasoning,
                &session_stats,
                pending_plan_count,
                selected_spawn_agent.as_deref(),
                info_panel.as_ref(),
                info_scroll,
                gate_count,
                artifact_viewer.as_ref(),
                artifact_file_view.as_ref(),
                live_content_pane.as_ref(),
                content_view.as_ref(),
                gate_modal.as_ref(),
                gate_modal
                    .as_ref()
                    .and_then(|m| gate_entry_for_ref(&entries, &m.gate)),
                approvals_popup.as_ref(),
                &approval_rows,
                grants_panel.as_ref(),
            )
        })?;

        view_rows = rows;
        view_indexed = indexed;
        view_visible = visible;
        view_gate = gate;
        view_row_count = row_count;
        view_row_heights = row_heights;
        view_viewport_offset = viewport_offset;
        view_list_height = list_height;
        view_turn_boundaries = turn_boundaries;
        needs_redraw = false;
    }

    let _ = event::poll(Duration::from_millis(
        if cached_open_turns.is_empty()
            && !session_async_processing
            && pending_gate.is_none()
        {
            IDLE_FRAME_MS
        } else {
            FRAME_MS
        }
    ))?;
    }
    Ok(())
}


/// Newest rendered row that matches an unresolved gate (plan / approval / ask).
fn newest_gate_row_index(
    visible: &[SessionTimelineEntry],
    indexed: &[(RenderedRow, RowSource)],
    entries: &[SessionTimelineEntry],
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
    gate: &GateRef,
) -> Option<usize> {
    for (vis_idx, e) in visible.iter().enumerate().rev() {
        if let Some(entry_gate) = gate_for_entry(e, resolved, acted) {
            if entry_gate.kind == gate.kind && entry_gate.id == gate.id {
                return row_index_for_visible(indexed, vis_idx);
            }
        }
    }
    // Embedded plan proposals live on `agent.message` rows outside `visible`
    // when the altitude floor filters sibling events — scan full history.
    if gate.kind == GateKind::Plan {
        for (entry_idx, e) in entries.iter().enumerate().rev() {
            if render::extract_plan_proposal_id(e).as_deref() == Some(gate.id.as_str()) {
                if let Some(vis_idx) = visible.iter().position(|v| v.event_id == e.event_id) {
                    return row_index_for_visible(indexed, vis_idx);
                }
                let _ = entry_idx;
            }
        }
    }
    None
}

fn last_line_row_index(indexed: &[(RenderedRow, RowSource)]) -> Option<usize> {
    indexed
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, (row, _))| matches!(row, RenderedRow::Line(_)).then_some(i))
}

fn child_turn_label(lineage: &SessionSpawnLineageEntry, local_turn: Option<u64>) -> String {
    let short = render::agent_id_short(&lineage.target_agent_id);
    match local_turn {
        Some(n) if n > 1 => format!("{}.{}", lineage.spawned_at_turn, n),
        _ => format!("{} → {}", lineage.spawned_at_turn, short),
    }
}

/// Annotate a row list with turn-boundary flags and in-flight markers. The
/// in-flight spinner is reserved for the **most recent** row of each open
/// turn — earlier rows keep their normal altitude glyph so the operator can
/// read the chain.
///
/// `open_turns` is the set of turn_ids with a `turn.start` but no matching
/// `turn.end` yet. `show_reasoning=false` hides rows whose headline carries
/// the 💭 marker (`agent.reasoning` rows).
///
/// Returns the per-row `turn_boundaries` map (true → draw divider above the
/// row) so the renderer can decorate the boundary.
fn annotate_turns_and_in_flight(
    rows: &mut [(RenderedRow, RowSource)],
    visible: &[SessionTimelineEntry],
    // Full, unfiltered timeline — used only to build the async back-reference
    // map. It must not be floor-filtered: a `tool.requested` is `Detail` and is
    // often filtered out of `visible`, while its `tool.completed` gets bumped to
    // `Attention` on failure and stays visible. Pass `visible` here when there
    // is no separate unfiltered slice (e.g. tests).
    all_entries: &[SessionTimelineEntry],
    open_turns: &HashSet<String>,
    show_reasoning: bool,
    extra_inflight_rows: &HashSet<usize>,
    root_session_id: &str,
    spawn_lineage: &HashMap<String, SessionSpawnLineageEntry>,
) -> HashMap<usize, bool> {
    let mut last_turn: Option<String> = None;
    let mut last_row_for_turn: HashMap<String, usize> = HashMap::new();
    let mut collapsed_open_turn_rows: HashSet<usize> = HashSet::new();
    let mut turn_boundaries: HashMap<usize, bool> = HashMap::new();
    for (i, (row, _)) in rows.iter().enumerate() {
        if let RenderedRow::Line(spec) = row {
            if let Some(t) = &spec.turn_id {
                if open_turns.contains(t) {
                    last_row_for_turn.insert(t.clone(), i);
                }
                if last_turn.as_ref() != Some(t) {
                    turn_boundaries.insert(i, true);
                }
                last_turn = Some(t.clone());
            }
        }
    }
    for (i, (row, src)) in rows.iter().enumerate() {
        if let RenderedRow::Collapsed { .. } = row {
            if let RowSource::Run { start, len } = src {
                if visible[*start..start + len]
                    .iter()
                    .any(|e| e.turn_id.as_ref().is_some_and(|t| open_turns.contains(t)))
                {
                    collapsed_open_turn_rows.insert(i);
                }
            }
        }
    }
    // Map each tool call's `call_id` to the turn it was *requested* in, so a
    // `tool.completed` that lands in a later turn (async: approval-gated tools,
    // workflow_wait, resumed continuations) can point back to its origin.
    let mut requested_turn: HashMap<String, u64> = HashMap::new();
    for e in all_entries {
        if e.event_type == "tool.requested" {
            if let (Some(cid), Some(t)) = (
                payload_field_str(e, "call_id"),
                e.turn_id.as_deref().and_then(turn_number_of),
            ) {
                requested_turn.entry(cid).or_insert(t);
            }
        }
    }
    for (i, (row, src)) in rows.iter_mut().enumerate() {
        match row {
            RenderedRow::Line(spec) => {
                let local_turn = spec
                    .turn_id
                    .as_deref()
                    .and_then(turn_number_of);
                spec.turn_index = local_turn.map(|n| n as u32);
                // Async back-reference: if this row is a completion whose request
                // happened in an earlier turn, annotate where it came from.
                if let RowSource::Single(idx) = src {
                    if let Some(e) = visible.get(*idx) {
                        if e.event_type == "tool.completed" {
                            if let Some(cid) = payload_field_str(e, "call_id") {
                                if let Some(&req_turn) = requested_turn.get(&cid) {
                                    if Some(req_turn) != local_turn {
                                        spec.headline
                                            .push_str(&format!("  ⟵ requested T{req_turn}"));
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(src_id) = spec.source_session_id.as_deref() {
                    if src_id != root_session_id {
                        if let Some(lineage) = spawn_lineage.get(src_id) {
                            spec.turn_label =
                                Some(child_turn_label(lineage, local_turn));
                        }
                    }
                }
                let open_turn_row = spec.turn_id.as_ref().is_some_and(|t| {
                    open_turns.contains(t) && last_row_for_turn.get(t).copied() == Some(i)
                });
                if open_turn_row || extra_inflight_rows.contains(&i) {
                    spec.in_flight = true;
                }
                if !show_reasoning && spec.headline.contains('\u{1F4AD}') {
                    spec.show_reasoning = false;
                }
            }
            RenderedRow::Collapsed { in_flight, .. } => {
                if collapsed_open_turn_rows.contains(&i) || extra_inflight_rows.contains(&i) {
                    *in_flight = true;
                }
            }
        }
    }
    turn_boundaries
}

/// The still-resolvable gate on the selected row, if it is a single
/// `approval.pending` or `user.ask.pending` event.
fn selectable_gate(
    entries: &[SessionTimelineEntry],
    src: Option<&(RenderedRow, RowSource)>,
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> Option<GateRef> {
    let (_, RowSource::Single(i)) = src? else {
        return None;
    };
    gate_for_entry(entries.get(*i)?, resolved, acted)
}

/// Newest unresolved gate anywhere in the fetched timeline. Follow mode pins
/// selection to the latest row, which is often *after* a `user.ask.pending`
/// event — without this, `i`/`r` would never open the answer editor.
fn find_active_gate(
    entries: &[SessionTimelineEntry],
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> Option<GateRef> {
    entries
        .iter()
        .rev()
        .find_map(|e| gate_for_entry(e, resolved, acted))
}

fn gate_for_entry(
    e: &SessionTimelineEntry,
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> Option<GateRef> {
    match e.event_type.as_str() {
        "approval.pending" => {
            let id = approval_id_for(e)?;
            let action = payload_field_str(e, "action");
            let kind = match action.as_deref() {
                Some("wiki_propose") => GateKind::WikiProposal,
                Some("session_escalate") => GateKind::Escalation,
                _ => GateKind::Approval,
            };
            (!resolved.contains(&id) && !acted.contains(&id)).then_some(GateRef { kind, id })
        }
        "plan.pending" => {
            let id = plan_id_for(e)?;
            plan_gate_unresolved(e, resolved, acted).then_some(GateRef {
                kind: GateKind::Plan,
                id,
            })
        }
        "user.ask.pending" => {
            let id = interaction_id_for(e)?;
            (!acted.contains(&id)).then_some(GateRef {
                kind: GateKind::Interaction,
                id,
            })
        }
        "operator.message" => {
            let id = notification_approval_id(e)?;
            (!resolved.contains(&id) && !acted.contains(&id)).then_some(GateRef {
                kind: GateKind::Approval,
                id,
            })
        }
        "agent.message" => render::extract_plan_proposal_id(e).and_then(|id| {
            plan_gate_unresolved(e, resolved, acted).then_some(GateRef {
                kind: GateKind::Plan,
                id,
            })
        }),
        "escalation.pending" => {
            let id = e.refs.approval_request_id.clone()?;
            (!resolved.contains(&id) && !acted.contains(&id)).then_some(GateRef {
                kind: GateKind::Escalation,
                id,
            })
        }
        _ => None,
    }
}

/// Prefer the gate under the cursor; otherwise the newest pending gate in the
/// session (so operators can answer without hunting for the ask row).
fn active_gate(
    entries: &[SessionTimelineEntry],
    visible: &[SessionTimelineEntry],
    src: Option<&(RenderedRow, RowSource)>,
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> Option<GateRef> {
    selectable_gate(visible, src, resolved, acted)
        .or_else(|| find_active_gate(entries, resolved, acted))
}

/// Decide the `interaction.resolve_and_answer` params for an interaction, or a
/// local-rejection message. Pure (no RPC) so the branching is unit-testable.
///
/// A typed number selects the matching pre-digested option — so >9 options and
/// the "type 2 ⏎" flow both work, even when free-text is disallowed. Falls back
/// to a hotkey-`chosen` option, then to free-text (when the question allows it).
/// Rejects locally rather than round-trip a guaranteed rejection (so a gate is
/// never marked acted on a doomed submission).
fn answer_params(gi: &GateInput, chosen: Option<&GateOption>) -> Result<serde_json::Value, String> {
    let text = gi.buffer.trim();
    let by_number = (!gi.options.is_empty())
        .then(|| text.parse::<usize>().ok())
        .flatten()
        .filter(|n| (1..=gi.options.len()).contains(n))
        .map(|n| &gi.options[n - 1]);
    if let Some(opt) = chosen.or(by_number) {
        if opt.id == "__details__" {
            // "Give more details" requires a free-text follow-up. Keep the input
            // open in details mode and do not submit yet.
            return Err("__details__".to_string());
        }
        return Ok(serde_json::json!({
            "interaction_id": gi.id, "answer_option_id": opt.id, "answered_by": "operator"
        }));
    }
    if text.is_empty() {
        return Err(if gi.options.is_empty() {
            "✗ answer cannot be empty".to_string()
        } else {
            "✗ type a number to choose, or type a reply".to_string()
        });
    }
    if !gi.allow_freeform && !gi.details_mode {
        return Err("✗ this question requires choosing a numbered option".to_string());
    }
    Ok(serde_json::json!({
        "interaction_id": gi.id, "answer_text": text, "answered_by": "operator"
    }))
}

/// Resolve a gate over the gateway API (the sanctioned path that unblocks the
/// waiting agent + records the decision incl. decider kind, #361). Decider is
/// the operator; `buffer` is the motivation (approvals) or free-text answer
/// (interactions). `chosen` is a pre-digested choice picked by number, which
/// resolves an interaction via `answer_option_id` instead of free text.
///
/// Returns `Ok(msg)` only when the gateway accepted the decision (the caller
/// uses this to mark the gate acted); `Err(msg)` on validation or transport
/// failure, leaving the gate offerable so the operator can retry.
fn approval_approve_params(gi: &GateInput) -> serde_json::Value {
    let text = gi.buffer.trim();
    let mut params = serde_json::json!({
        "request_id": gi.id,
        "decided_by": "operator",
    });
    // §O: destructive / elevated approvals need a non-empty motivation on the
    // `reason` field. For R++4 confirm-phrase gates the typed phrase satisfies
    // both obligations — do not send confirm_phrase without reason.
    if gi.required_confirm_phrase.is_some() {
        if !text.is_empty() {
            params["confirm_phrase"] = serde_json::json!(text);
            params["reason"] = serde_json::json!(text);
        }
    } else if !text.is_empty() {
        params["reason"] = serde_json::json!(text);
    }
    if !gi.acknowledged_capabilities.is_empty() {
        params["acknowledged_capabilities"] = serde_json::json!(gi.acknowledged_capabilities);
    }
    // CredentialPrompt approvals: the secret values collected in Phase 1 are
    // submitted alongside the approval. The gateway stores them in the vault
    // and upserts the credential — they never reach the agent transcript.
    if !gi.secret_fields.is_empty() {
        let secrets: Vec<[String; 2]> = gi
            .secret_fields
            .iter()
            .zip(gi.secret_values.iter())
            .map(|(f, v)| [f.name.clone(), v.clone()])
            .collect();
        params["secrets"] = serde_json::json!(secrets);
    }
    params
}

/// Start resolving a gate asynchronously so the TUI event loop stays
/// responsive. Returns immediately with a `PendingGateResolve` that the caller
/// must poll each frame; on completion the operator sees the same success/error
/// path as before.
fn resolve_gate(
    client: &RoomClient,
    gi: GateInput,
    chosen: Option<GateOption>,
) -> Result<PendingGateResolve, (String, GateInput)> {
    let text = gi.buffer.trim().to_string();
    let reason = (!text.is_empty()).then(|| text.clone());
    let id = gi.id.clone();
    let (params, verb) = match gi.action {
        GateAction::Approve => (approval_approve_params(&gi), "approved"),
        GateAction::Reject => (
            serde_json::json!({ "request_id": id, "decided_by": "operator", "reason": reason }),
            "rejected",
        ),
        GateAction::Answer => match answer_params(&gi, chosen.as_ref()) {
            Ok(p) => (p, "answered"),
            Err(msg) => return Err((msg, gi)),
        },
    };
    let method = match gi.action {
        GateAction::Approve => "approvals.approve",
        GateAction::Reject => "approvals.reject",
        GateAction::Answer => "interaction.resolve_and_answer",
    };
    let client = client.clone();
    let gate_id = gi.id.clone();
    let result: Arc<StdMutex<Option<Result<String, String>>>> = Arc::new(StdMutex::new(None));
    let result2 = Arc::clone(&result);
    tokio::spawn(async move {
        let outcome = match client
            .call_with_timeout(method, params, Duration::from_secs(30))
            .await
        {
            Ok(_) => Ok(format!("✓ {verb} {gate_id}")),
            Err(e) => Err(format!("✗ {e}")),
        };
        *result2.lock().expect("pending gate result mutex poisoned") = Some(outcome);
    });
    Ok(PendingGateResolve {
        gi: Some(gi),
        result,
    })
}

/// Non-blocking check of a pending gate RPC. On completion returns `Some` with
/// the restored `GateInput` on error (so the operator can retry) and a status
/// message for both outcomes.
fn poll_pending_gate(
    pending: &mut PendingGateResolve,
) -> Option<Result<String, (String, GateInput)>> {
    let mut guard = pending
        .result
        .lock()
        .expect("pending gate result mutex poisoned");
    guard.take().map(|outcome| {
        outcome.map_err(|msg| {
            let gi = pending
                .gi
                .take()
                .expect("pending gate input already consumed");
            (msg, gi)
        })
    })
}

/// Send a free-form operator message into the session over the gateway API
/// (#405) — the same `event.ingest` ingress `chat` uses. Async (`async_mode`)
/// so the sync TUI loop never blocks on the agent turn; the operator's line
/// (recorded gateway-side) and the agent's reply then stream in via polling.
fn send_message(
    client: &RoomClient,
    root_session_id: &str,
    text: &str,
    target_agent_id: Option<&str>,
) -> String {
    let mut params = serde_json::json!({
        "event_type": "chat",
        "message": text,
        "session_id": root_session_id,
        "async_mode": true,
        "metadata": { "source": "session_room" },
    });
    if let Some(agent_id) = target_agent_id {
        if let Some(map) = params.as_object_mut() {
            map.insert("target_agent_id".to_string(), serde_json::json!(agent_id));
        }
    }
    match rpc(client, "event.ingest", params) {
        Ok(_) => "✓ sent".to_string(),
        Err(e) => format!("✗ {e}"),
    }
}

/// Return the active workbench to the orchestrator via the gateway. First
/// calls `workbench.prepare_return_to_agent` to compute the payload safely
/// (the room TUI cannot read the workspace files directly), then sends an
/// `event.ingest` workbench_reconciled wake-up.
fn return_workbench_to_agent(
    client: &RoomClient,
    root_session_id: &str,
    force: bool,
    note: Option<&str>,
) -> String {
    let prepare_params = serde_json::json!({
        "root_session_id": root_session_id,
        "force": force,
        "note": note,
    });
    let prepared = match rpc(client, "workbench.prepare_return_to_agent", prepare_params) {
        Ok(v) => v,
        Err(e) => return format!("✗ {e}"),
    };
    let status = prepared.get("status").and_then(|s| s.as_str()).unwrap_or("");
    match status {
        "no_workbench" => "✗ No active workbench to return.".to_string(),
        "refused" => {
            let reason = prepared
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("return refused by gateway");
            format!("✗ {reason}")
        }
        "ready" => {
            let target_agent_id = prepared
                .get("target_agent_id")
                .and_then(|s| s.as_str())
                .unwrap_or("planner.default");
            let message = prepared
                .get("message")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let metadata = prepared.get("metadata").cloned().unwrap_or(serde_json::Value::Null);
            let mut merged = serde_json::json!({
                "source": "session_room",
                "root_session_id": root_session_id,
            });
            if let serde_json::Value::Object(ref mut map) = merged {
                if let Some(obj) = metadata.as_object() {
                    for (k, v) in obj {
                        map.insert(k.clone(), v.clone());
                    }
                }
            }
            let params = serde_json::json!({
                "event_type": "workbench_reconciled",
                "message": message,
                "session_id": root_session_id,
                "target_agent_id": target_agent_id,
                "async_mode": true,
                "metadata": merged,
            });
            match rpc(client, "event.ingest", params) {
                Ok(_) => "✓ returned workbench to planner".to_string(),
                Err(e) => format!("✗ event.ingest failed: {e}"),
            }
        }
        other => format!("✗ unknown prepare_return status: {other}"),
    }
}

/// Parse an optional leading line hint (`L12:` or `L12-14:`) from a comment
/// body. Returns `(line_start, line_end, remaining_body)`. The hint is best
/// effort — anything that doesn't parse is left as part of the body.
fn parse_line_hint(text: &str) -> (Option<u32>, Option<u32>, String) {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix(['L', 'l']) else {
        return (None, None, text.to_string());
    };
    let Some(colon) = rest.find(':') else {
        return (None, None, text.to_string());
    };
    let (spec, body) = rest.split_at(colon);
    let body = body[1..].trim_start().to_string();
    let parse_u32 = |s: &str| s.trim().parse::<u32>().ok();
    match spec.split_once('-') {
        // A reversed range (`L14-12:`) is a typo, not a valid hint — the gateway
        // would reject it and block the comment. Treat it as malformed and leave
        // the body intact so the comment still sends (just without a line hint).
        Some((a, b)) => match (parse_u32(a), parse_u32(b)) {
            (Some(s), Some(e)) if e >= s => (Some(s), Some(e), body),
            _ => (None, None, text.to_string()),
        },
        None => match parse_u32(spec) {
            Some(s) => (Some(s), None, body),
            None => (None, None, text.to_string()),
        },
    }
}

/// Attach an operator comment to a live content file (`content.comment`). The
/// comment anchors to the version handle the operator was viewing and is
/// delivered to the owning agent at its next turn. An optional `L12:`/`L12-14:`
/// prefix on the body becomes the line hint.
fn send_comment(
    client: &RoomClient,
    root_session_id: &str,
    name: &str,
    handle: &str,
    text: &str,
) -> String {
    let (line_start, line_end, body) = parse_line_hint(text);
    if body.trim().is_empty() {
        return "✗ empty comment".to_string();
    }
    let mut params = serde_json::json!({
        "session_id": root_session_id,
        "name": name,
        "body": body,
    });
    if let Some(map) = params.as_object_mut() {
        if !handle.is_empty() {
            map.insert("handle".to_string(), serde_json::json!(handle));
        }
        if let Some(s) = line_start {
            map.insert("line_start".to_string(), serde_json::json!(s));
        }
        if let Some(e) = line_end {
            map.insert("line_end".to_string(), serde_json::json!(e));
        }
    }
    match rpc(client, "content.comment", params) {
        Ok(v) => {
            if v.get("drifted").and_then(|d| d.as_bool()) == Some(true) {
                "✓ commented (file changed since — agent will re-read)".to_string()
            } else {
                "✓ commented".to_string()
            }
        }
        Err(e) => format!("✗ {e}"),
    }
}

/// Best-effort: launch a GUI editor on `dir`. `code`/`$VISUAL` are GUI launchers
/// that fork and return immediately, so spawning them from inside the TUI does
/// not hijack the terminal. Terminal editors (vim, …) would, so we only try
/// known GUI openers. Returns true if a launch was spawned.
fn try_open_in_editor(dir: &str) -> bool {
    // `$VISUAL` is the conventional GUI editor; otherwise try VS Code by name.
    let candidates: Vec<String> = std::env::var("VISUAL")
        .ok()
        .into_iter()
        .chain(["code".to_string(), "codium".to_string()])
        .collect();
    for cmd in candidates {
        if std::process::Command::new(&cmd)
            .arg(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return true;
        }
    }
    false
}

/// Project the session's live content drafts to a real directory
/// (`content.project_live`) and open it in an external editor. The directory is
/// a read-only snapshot on the **gateway host**; the launch is best-effort and
/// only succeeds when the room runs on that same host. Either way the path is
/// surfaced so the operator can open it manually (or over a remote mount).
fn project_live_and_open(client: &RoomClient, root_session_id: &str) -> String {
    match rpc(
        client,
        "content.project_live",
        serde_json::json!({ "session_id": root_session_id }),
    ) {
        Ok(v) => {
            let path = v.get("path").and_then(|p| p.as_str()).unwrap_or_default();
            let count = v.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
            if path.is_empty() {
                return "✗ project_live: no path in response".to_string();
            }
            if try_open_in_editor(path) {
                format!("✓ opened {count} live file(s) in editor → {path}")
            } else {
                format!("live dir ({count} file(s)) → {path}  (open it in your editor)")
            }
        }
        Err(e) => format!("✗ {e}"),
    }
}

/// Target agent for an `agent_spawn` row (single event or first match in a run).
fn spawn_agent_for_row_source(
    visible: &[SessionTimelineEntry],
    src: RowSource,
) -> Option<String> {
    match src {
        RowSource::Single(i) => visible.get(i).and_then(render::agent_spawn_agent_id),
        RowSource::Run { start, len } => visible
            .iter()
            .skip(start)
            .take(len)
            .find_map(render::agent_spawn_agent_id),
    }
}

/// Drill-down detail for a selected row's source: a single event's full
/// metadata/payload/refs, or what a collapsed run folds. (Deep ref-following —
/// fetching the referenced approval/plan/workbench object — needs RPC read
/// methods that don't exist yet; tracked as a follow-up.)
fn detail_for(entries: &[SessionTimelineEntry], src: RowSource) -> Vec<String> {
    match src {
        RowSource::Single(i) => {
            let Some(e) = entries.get(i) else {
                return vec![];
            };
            if let Some(turn_lines) = render::turn_summary(e, entries, i) {
                let mut base = render::format_detail(e);
                base.push(String::new());
                base.push("── turn summary ──".to_string());
                base.extend(turn_lines);
                base
            } else {
                render::format_detail(e)
            }
        }
        RowSource::Run { start, len } => {
            const MAX_RUN_DETAIL_LINES: usize = 64;
            let show = len.min(MAX_RUN_DETAIL_LINES);
            let mut lines = vec![
                format!("collapsed run — {len} routine events"),
                "(press 's' to unsquash and inspect individually)".to_string(),
                String::new(),
            ];
            for e in entries.iter().skip(start).take(show) {
                lines.push(format!("  · {}", render::render_line(e)));
            }
            if len > show {
                lines.push(format!(
                    "  … (+{} more — press 's' to unsquash)",
                    len - show
                ));
            }
            lines
        }
    }
}

/// Build the styled `Line`s for a single row. Capped at `MAX_ROW_LINES`
/// Actor label column — includes a `T{n}` prefix when the row belongs to a turn.
fn format_row_label(spec: &RowSpec, label_w: usize) -> String {
    let label_text = match (&spec.turn_label, spec.turn_index) {
        (Some(l), _) => {
            let prefix = format!("T{l}·");
            let inner_budget = label_w.saturating_sub(prefix.chars().count() + 2);
            format!(
                "[{prefix}{}]",
                truncate(&spec.actor_label, inner_budget)
            )
        }
        (None, Some(n)) => {
            let prefix = format!("T{n}·");
            let inner_budget = label_w.saturating_sub(prefix.chars().count() + 2);
            format!(
                "[{prefix}{}]",
                truncate(&spec.actor_label, inner_budget)
            )
        }
        (None, None) => format!(
            "[{}]",
            truncate(&spec.actor_label, label_w.saturating_sub(2))
        ),
    };
    format!("{label_text:>label_w$}")
}

/// physical lines; longer content gets a `…` ellipsis on the last line. The
/// actor rail on the left uses the actor's color, giving an at-a-glance map of
/// "who said what." Used by the custom render loop in `draw` so we can
/// measure the row's physical height and stop rendering cleanly when a
/// multi-line row no longer fits in the remaining viewport area.
fn build_rich_row_lines(
    spec: &RowSpec,
    row_index: usize,
    turn_boundaries: &HashMap<usize, bool>,
    content_w: usize,
    glyph_w: usize,
    rail_w: usize,
    label_w: usize,
    spinner_glyph: &'static str,
    show_reasoning: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(&forkable) = turn_boundaries.get(&row_index) {
        let total_w = content_w + glyph_w + label_w + rail_w + 2;
        // Forkable turns (a runnable checkpoint exists) get a heavier rule, a
        // `⑂` marker, and a brighter colour so the operator can see at a glance
        // where `F` / `/fork --at-turn N` will actually work.
        let (rule, tag, style) = if forkable {
            ('═', " ⑂ fork", Style::default().fg(Color::Cyan))
        } else {
            ('─', "", Style::default().fg(Color::DarkGray))
        };
        let label = spec
            .turn_label
            .clone()
            .or_else(|| spec.turn_index.map(|n| n.to_string()));
        let bar = if let Some(l) = label {
            let prefix = format!("{rule}{rule} turn {l}{tag} ");
            let fill = total_w.saturating_sub(prefix.chars().count());
            format!("{prefix}{}", rule.to_string().repeat(fill))
        } else {
            rule.to_string().repeat(total_w)
        };
        lines.push(Line::from(Span::styled(bar, style)));
    }
    let rail_style = Style::default().fg(row_rail_color(spec));
    let rail_block = "▌".repeat(rail_w);
    let glyph = if spec.in_flight {
        spinner_glyph
    } else if spec.tone == RowTone::OperatorGate {
        "◆"
    } else {
        render::altitude_glyph(spec.altitude)
    };
    let label_padded = format_row_label(spec, label_w);
    let head_style = row_headline_style(spec);
    let label_style = row_label_style(spec);
    let detail_style = row_detail_style(spec);
    let cont_pad = " ".repeat(rail_w + glyph_w + label_w + 1);
    let first_prefix = vec![
        Span::styled(rail_block.clone(), rail_style),
        Span::raw(" "),
        Span::styled(format!("{glyph:<2}"), altitude_style(spec.altitude)),
        Span::styled(label_padded.clone(), label_style),
        Span::raw(" "),
    ];

    if spec.tone == RowTone::AgentNarrative {
        push_agent_narrative_row(
            &mut lines,
            spec,
            content_w,
            &cont_pad,
            first_prefix,
            head_style,
            detail_style,
        );
    } else {
        let mut headline = spec.headline.clone();
        if !show_reasoning && headline.starts_with('💭') {
            if let Some(stripped) = headline.strip_prefix('💭') {
                headline = stripped.trim_start().to_string();
            }
        }
        let wrapped_headline = word_wrap_text(&headline, content_w);
        for (i, chunk) in wrapped_headline.iter().enumerate() {
            if i == 0 {
                let mut spans = first_prefix.clone();
                spans.push(Span::styled(chunk.clone(), head_style));
                lines.push(Line::from(spans));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(cont_pad.clone()),
                    Span::styled(chunk.clone(), head_style),
                ]));
            }
        }
        if let Some(d) = &spec.detail {
            if !d.is_empty() {
                for (i, sub) in d.split('\n').enumerate() {
                    if sub.trim().is_empty() {
                        continue;
                    }
                    let prefix = if spec.tone == RowTone::OperatorGate {
                        "    "
                    } else if i == 0 {
                        "  ↳ "
                    } else {
                        "    "
                    };
                    let avail = content_w.saturating_sub(prefix.chars().count());
                    for (j, chunk) in word_wrap_text(sub.trim_end(), avail).into_iter().enumerate() {
                        let line_prefix = if j == 0 { prefix } else { "    " };
                        lines.push(Line::from(vec![
                            Span::raw(cont_pad.clone()),
                            Span::styled(format!("{line_prefix}{chunk}"), detail_style),
                        ]));
                    }
                }
            }
        }
    }
    let physical: Vec<Line<'static>> = lines
        .iter()
        .filter(|l| !is_divider_line(l))
        .cloned()
        .collect();
    let mut out: Vec<Line<'static>> = lines
        .iter()
        .filter(|l| is_divider_line(l))
        .cloned()
        .collect();
    let max_lines = match spec.tone {
        RowTone::AgentNarrative => MAX_NARRATIVE_ROW_LINES,
        RowTone::OperatorGate => 8,
        _ => MAX_ROW_LINES,
    };
    if physical.len() > max_lines {
        let dropped = physical.len() - max_lines;
        let mut kept: Vec<Line<'static>> =
            physical.into_iter().take(max_lines).collect();
        if let Some(last) = kept.last_mut() {
            last.spans.push(Span::styled(
                format!(" …(+{dropped})"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        out.extend(kept);
    } else {
        out.extend(physical);
    }
    out
}

fn is_divider_line(line: &Line) -> bool {
    line.spans.len() == 1
        && line
            .spans
            .first()
            // `─` for ordinary turn dividers, `═` for forkable ones.
            .map(|s| s.content.starts_with('─') || s.content.starts_with('═'))
            .unwrap_or(false)
}

fn build_collapsed_row_line(
    count: usize,
    summary: &str,
    in_flight: bool,
    spinner_glyph: &str,
) -> Line<'static> {
    let style = Style::default().fg(Color::DarkGray);
    let glyph = if in_flight {
        format!("{spinner_glyph:<2}")
    } else {
        format!("{:<2}", render::altitude_glyph(Altitude::Detail))
    };
    let text = format!("{} ⟨{} {}⟩", glyph, count, summary);
    Line::from(Span::styled(text, style))
}

/// Word-wrap prose to terminal cells (Unicode-aware). Blank input ⇒ one empty line.
fn word_wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut result = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);
        let sep_width = if current.is_empty() { 0 } else { 1 };
        if current_width + sep_width + word_width > max_width && !current.is_empty() {
            result.push(std::mem::take(&mut current));
            current = word.to_string();
            current_width = word_width;
        } else {
            if !current.is_empty() {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Plain-text content of a rendered line (for width measurement / wrapping).
fn line_display_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Compact metadata sub-line from structured agent messages (`[ok] · agent: …`).
fn is_compact_meta_line(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t.starts_with('[')
        && t.contains(']')
        && !t.contains('\n')
        && t.chars().count() <= 160
}

/// Split structured `[status] · …` metadata from narrative prose in a row spec.
fn split_agent_narrative_content(headline: &str, detail: Option<&str>) -> (Option<String>, String) {
    let mut prose_parts = Vec::new();
    if !headline.trim().is_empty() {
        prose_parts.push(headline.trim().to_string());
    }
    let mut meta = None;
    if let Some(d) = detail.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some((first, rest)) = d.split_once("\n\n") {
            if is_compact_meta_line(first) {
                meta = Some(first.trim().to_string());
                if !rest.trim().is_empty() {
                    prose_parts.push(rest.trim().to_string());
                }
            } else {
                prose_parts.push(d.to_string());
            }
        } else if is_compact_meta_line(d) {
            meta = Some(d.to_string());
        } else {
            prose_parts.push(d.to_string());
        }
    }
    (meta, prose_parts.join("\n\n"))
}

/// Render an agent/operator narrative row: optional compact metadata, then
/// markdown-formatted prose (same normalization as the detail pane).
fn push_agent_narrative_row(
    lines: &mut Vec<Line<'static>>,
    spec: &RowSpec,
    content_w: usize,
    cont_pad: &str,
    first_prefix: Vec<Span<'static>>,
    body_style: Style,
    detail_style: Style,
) {
    let (meta, prose) = split_agent_narrative_content(&spec.headline, spec.detail.as_deref());
    let mut prefix = Some(first_prefix);
    if let Some(meta_line) = meta {
        push_wrapped_detail_lines(lines, &meta_line, content_w, cont_pad, detail_style);
        prefix = None;
    }
    if prose.trim().is_empty() {
        if prefix.is_some() {
            let mut spans = prefix.take().unwrap();
            spans.push(Span::styled(String::new(), body_style));
            lines.push(Line::from(spans));
        }
        return;
    }
    push_wrapped_markdown_body(
        lines,
        &prose,
        content_w,
        cont_pad,
        prefix,
        body_style,
    );
}

/// Render markdown prose into wrapped list rows with optional rail/glyph prefix.
fn push_wrapped_markdown_body(
    lines: &mut Vec<Line<'static>>,
    body: &str,
    content_w: usize,
    cont_pad: &str,
    mut first_prefix: Option<Vec<Span<'static>>>,
    default_style: Style,
) {
    use super::markdown;
    let normalized = markdown::normalize_narrative_prose(body);
    for md_line in markdown::render_markdown(&normalized) {
        let text = line_display_text(&md_line);
        if text.trim().is_empty() {
            lines.push(Line::from(Span::raw(cont_pad.to_string())));
            continue;
        }
        if markdown::line_is_code_block(&md_line) {
            push_markdown_line(
                lines,
                &md_line,
                cont_pad,
                &mut first_prefix,
                content_w,
                false,
            );
            continue;
        }
        let style = md_line
            .spans
            .iter()
            .find(|s| !s.content.is_empty())
            .map(|s| s.style)
            .unwrap_or(default_style);
        for chunk in word_wrap_text(text.trim_end(), content_w) {
            if let Some(prefix) = first_prefix.take() {
                let mut spans = prefix;
                spans.push(Span::styled(chunk, style));
                lines.push(Line::from(spans));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(cont_pad.to_string()),
                    Span::styled(chunk, style),
                ]));
            }
        }
    }
}

/// Push one rendered markdown line, optionally word-wrapping prose lines.
fn push_markdown_line(
    lines: &mut Vec<Line<'static>>,
    md_line: &Line<'static>,
    cont_pad: &str,
    first_prefix: &mut Option<Vec<Span<'static>>>,
    content_w: usize,
    wrap: bool,
) {
    if !wrap {
        if let Some(prefix) = first_prefix.take() {
            let mut spans = prefix;
            spans.extend(md_line.spans.clone());
            lines.push(Line::from(spans));
        } else {
            let mut spans = vec![Span::raw(cont_pad.to_string())];
            spans.extend(md_line.spans.clone());
            lines.push(Line::from(spans));
        }
        return;
    }
    let text = line_display_text(md_line);
    let style = md_line
        .spans
        .iter()
        .find(|s| !s.content.is_empty())
        .map(|s| s.style)
        .unwrap_or_default();
    for chunk in word_wrap_text(text.trim_end(), content_w) {
        if let Some(prefix) = first_prefix.take() {
            let mut spans = prefix;
            spans.push(Span::styled(chunk, style));
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![
                Span::raw(cont_pad.to_string()),
                Span::styled(chunk, style),
            ]));
        }
    }
}

/// Render compact sub-lines under a narrative headline (`↳ sketch · …`).
fn push_wrapped_detail_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    content_w: usize,
    cont_pad: &str,
    detail_style: Style,
) {
    for (i, sub) in text.split('\n').enumerate() {
        if sub.trim().is_empty() {
            continue;
        }
        let prefix = if i == 0 { "  ↳ " } else { "    " };
        let avail = content_w.saturating_sub(prefix.chars().count());
        for (j, chunk) in word_wrap_text(sub.trim_end(), avail).into_iter().enumerate() {
            let line_prefix = if i == 0 && j == 0 { prefix } else { "    " };
            lines.push(Line::from(vec![
                Span::raw(cont_pad.to_string()),
                Span::styled(format!("{line_prefix}{chunk}"), detail_style),
            ]));
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Left-rail color: agent narrative keeps the seat hue; tool calls use a cool
/// blue so they read as plumbing, not speech.
fn row_rail_color(spec: &RowSpec) -> Color {
    // Altitude takes visual precedence on the rail so severity is visible
    // at a glance regardless of who is speaking.
    match spec.altitude {
        Altitude::Error => return Color::Red,
        Altitude::Attention => return Color::Yellow,
        _ => {}
    }
    match spec.tone {
        RowTone::OperatorGate => Color::Yellow,
        RowTone::ToolCall => Color::Blue,
        RowTone::Reasoning => Color::DarkGray,
        RowTone::AgentNarrative | RowTone::Default => actor_color(spec.actor),
    }
}

/// Headline emphasis: agent messages pop; tool calls stay subdued.
fn row_headline_style(spec: &RowSpec) -> Style {
    let alt = altitude_style(spec.altitude);
    match spec.tone {
        RowTone::OperatorGate => Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        RowTone::AgentNarrative => alt
            .fg(actor_color(spec.actor))
            .add_modifier(Modifier::BOLD),
        RowTone::ToolCall => alt.fg(Color::LightBlue).add_modifier(Modifier::DIM),
        RowTone::Reasoning => alt.fg(Color::DarkGray),
        RowTone::Default => alt.patch(Style::default().fg(actor_color(spec.actor))),
    }
}

fn row_label_style(spec: &RowSpec) -> Style {
    match spec.tone {
        RowTone::OperatorGate => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        RowTone::ToolCall => Style::default().fg(Color::Blue).add_modifier(Modifier::DIM),
        RowTone::Reasoning => Style::default().fg(Color::DarkGray),
        RowTone::AgentNarrative | RowTone::Default => {
            Style::default().fg(actor_color(spec.actor))
        }
    }
}

fn row_detail_style(spec: &RowSpec) -> Style {
    match spec.tone {
        RowTone::OperatorGate => Style::default().fg(Color::LightYellow),
        RowTone::ToolCall => Style::default().fg(Color::Indexed(67)),
        RowTone::AgentNarrative => Style::default().fg(Color::Gray),
        _ => Style::default().fg(Color::DarkGray),
    }
}

/// Map an `ActorKind` to a stable color. The TUI's left rail uses this so the
/// operator can scan the room and tell *who* is speaking at a glance.
///
/// Must avoid `Red`/`Yellow`/`Blue`: `row_rail_color` reserves those for
/// altitude (`Error`→Red, `Attention`→Yellow, overriding actor color) and for
/// `RowTone` (`OperatorGate`→Yellow, `ToolCall`→Blue). Reusing a reserved
/// color for an actor makes that actor's routine chatter indistinguishable,
/// by rail color alone, from an actual gate/error/tool-call row — e.g.
/// Sentinel used to share Yellow with `Attention`/`OperatorGate`, so a
/// Sentinel row read as "needs attention" even when it wasn't.
/// `actor_colors_do_not_collide_with_severity_or_tone` guards this.
fn actor_color(actor: ActorKind) -> Color {
    match actor {
        ActorKind::Operator => Color::Cyan,
        ActorKind::Planner => Color::Green,
        ActorKind::Specialist => Color::LightGreen,
        ActorKind::Sentinel => Color::Indexed(208), // orange — distinct from Attention/Gate yellow
        ActorKind::Curator => Color::Magenta,
        ActorKind::Auditor => Color::LightMagenta,
        ActorKind::Tool => Color::DarkGray,
        ActorKind::ExternalSurface => Color::Indexed(141), // lavender — distinct from ToolCall blue
        ActorKind::Runtime => Color::LightCyan,
        ActorKind::Other => Color::White,
    }
}

fn cycle_floor(floor: Altitude) -> Altitude {
    match floor {
        Altitude::Detail => Altitude::Normal,
        Altitude::Normal => Altitude::Attention,
        Altitude::Attention => Altitude::Error,
        Altitude::Error => Altitude::Detail,
    }
}

fn altitude_style(altitude: Altitude) -> Style {
    match altitude {
        Altitude::Error => Style::default().fg(Color::Red),
        Altitude::Attention => Style::default().fg(Color::Yellow),
        Altitude::Normal => Style::default(),
        Altitude::Detail => Style::default().fg(Color::DarkGray),
    }
}

/// Reset every per-session view state when the operator reloads to a different
/// root session. Cursor, entries, selection, follow, and the resolved-gate
/// sets are all session-scoped — leaving them stale would let an old approval
/// mark look "acted" on a brand-new session.
///
/// The `target_agent_id` is *not* cleared here: a `/session <id>` is just a
/// viewer change, the operator still has the right to send messages addressed
/// to whatever agent is currently selected. `event.ingest` will surface a
/// clear error if the agent doesn't match the new session's binding; that's
/// the natural place to detect a mismatch rather than guessing from a session
/// list (which may not know the agent yet).
#[allow(clippy::too_many_arguments)]
fn switch_session(
    _client: &RoomClient,
    entries: &mut Vec<SessionTimelineEntry>,
    cursor: &mut Option<String>,
    selected: &mut usize,
    detail: &mut Option<DetailPane>,
    follow: &mut bool,
    resolved: &mut HashSet<String>,
    acted: &mut HashSet<String>,
    floor: &mut Altitude,
    root_session_id: &mut String,
    _target_agent_id: &mut Option<String>,
    _limit: u32,
    new_id: &str,
    force_timeline_refresh: &mut bool,
    spawn_lineage: &mut HashMap<String, SessionSpawnLineageEntry>,
) {
    *root_session_id = new_id.to_string();
    entries.clear();
    spawn_lineage.clear();
    *cursor = None;
    *selected = 0;
    *detail = None;
    *follow = true;
    resolved.clear();
    acted.clear();
    *force_timeline_refresh = true;
    // Don't reset `floor` — the operator's altitude dial is a view preference,
    // not a session property. Keep the previous setting.
    let _ = floor;
}

/// Fetch the most recent session id, optionally filtered by agent. Returns
/// `None` when the gateway has no matching session.
/// System sessions (e.g. scheduled system agents, auto-learning jobs, the
/// sentinel) are recorded under the reserved `"system"` root id. The operator
/// can't resume or switch into them, so `/session` hides them.
fn is_system_session(root_session_id: &str) -> bool {
    root_session_id == "system"
}

/// Turn id of the timeline row the cursor is on, if any. Maps the view cursor
/// (`selected`) through the rendered-row → source-entry indirection; collapsed
/// runs resolve to their first entry. Returns `None` for rows with no turn
/// (operator messages, session-level events), which can't be forked from.
fn selected_turn_id(
    view_indexed: &[(RenderedRow, RowSource)],
    view_visible: &[SessionTimelineEntry],
    selected: usize,
) -> Option<String> {
    let (_, src) = view_indexed.get(selected)?;
    let idx = match src {
        RowSource::Single(i) => *i,
        RowSource::Run { start, .. } => *start,
    };
    view_visible.get(idx)?.turn_id.clone()
}

/// Parse the numeric turn from a `turn-000003` id. Turn ids are zero-padded,
/// but `parse::<u64>` handles the leading zeros directly.
fn turn_number_of(turn_id: &str) -> Option<u64> {
    turn_id.strip_prefix("turn-").and_then(|n| n.parse::<u64>().ok())
}

/// Fork the current root session into a new branch via `session.fork`.
/// Returns `(new_session_id, fork_turn)` on success, or a ready-to-display
/// `✗ …` status string on failure. `at_turn = None` forks from the latest
/// checkpoint; checkpoints exist only at yield points, so the gateway rejects
/// turns it has no checkpoint for (its error — listing forkable turns — is
/// surfaced verbatim).
fn fork_session(
    client: &RoomClient,
    source_session_id: &str,
    at_turn: Option<u64>,
    branch_message: Option<&str>,
) -> Result<(String, u64), String> {
    let mut params = serde_json::json!({ "source_session_id": source_session_id });
    if let Some(turn) = at_turn {
        params["at_turn"] = serde_json::json!(turn);
    }
    if let Some(msg) = branch_message {
        params["branch_message"] = serde_json::json!(msg);
    }
    match rpc(client, "session.fork", params) {
        Ok(value) => {
            let new_id = value
                .get("new_session_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    "✗ session.fork: malformed response (no new_session_id)".to_string()
                })?;
            let fork_turn = value.get("fork_turn").and_then(|v| v.as_u64()).unwrap_or(0);
            Ok((new_id, fork_turn))
        }
        Err(e) => Err(format!("✗ /fork failed: {e}")),
    }
}

fn resolve_latest_session(client: &RoomClient, agent: Option<&str>) -> Option<String> {
    // Fetch a small batch (not just 1) so a leading system session doesn't
    // shadow the most recent operator-resumable one.
    let params = serde_json::json!({
        "agent_id": agent,
        "limit": 16,
    });
    let value = rpc(client, "session.list", params).ok()?;
    let parsed: serde_json::Result<autonoetic_types::session_timeline::SessionListResult> =
        serde_json::from_value(value);
    parsed
        .ok()?
        .sessions
        .into_iter()
        .map(|e| e.root_session_id)
        .find(|id| !is_system_session(id))
}

/// Build a multi-line session list for `/session` and `/session list [agent]`,
/// returned as (display lines, pickable session ids). Rows are numbered [1]-[9]
/// so the operator can switch by pressing a single digit while the detail pane
/// is open.
/// Format one pending PlanFrame for the review pane or `/plan` list.
fn format_plan_frame_lines(plan: &autonoetic_types::plan_frame::PlanFrame, for_review: bool) -> Vec<String> {
    use autonoetic_types::plan_frame::ValidationRequirement;
    let mut lines = Vec::new();
    if for_review {
        lines.push("── plan review ──".to_string());
        lines.push(String::new());
    }
    let amend = plan
        .parent_version
        .map(|v| format!(" (amended from v{v})"))
        .unwrap_or_default();
    let indent = if for_review { "" } else { "  " };
    lines.push(format!(
        "{indent}{} v{} — {}{}",
        plan.plan_id, plan.version, plan.title, amend
    ));
    if let Some(reason) = &plan.reason {
        if !reason.trim().is_empty() {
            lines.push(format!("{indent}  reason: {reason}"));
        }
    }
    if !plan.objective.is_empty() {
        lines.push(format!("{indent}  objective: {}", plan.objective));
    }
    lines.push(String::new());
    lines.push(format!("{indent}steps:"));
    for (i, step) in plan.steps.iter().enumerate() {
        let agent = step
            .resolved_agent_id()
            .map(|a| format!(" → {a}"))
            .unwrap_or_default();
        let status_label = if step.status == autonoetic_types::plan_frame::StepStatus::Pending {
            String::new()
        } else {
            format!(" [{}]", step.status.as_str())
        };
        lines.push(format!(
            "{indent}  {}. {}{}{}",
            i + 1,
            step.title,
            agent,
            status_label
        ));
        if let Some(notes) = &step.notes {
            if !notes.trim().is_empty() {
                lines.push(format!("{indent}     notes: {}", render::one_line(notes, 120)));
            }
        }
    }
    let required: Vec<_> = plan
        .validation_policy
        .entries
        .iter()
        .filter(|v| v.requirement == ValidationRequirement::Required)
        .map(|v| v.title.as_str())
        .collect();
    let advisory: Vec<_> = plan
        .validation_policy
        .entries
        .iter()
        .filter(|v| v.requirement == ValidationRequirement::Advisory)
        .map(|v| v.title.as_str())
        .collect();
    if !required.is_empty() {
        lines.push(format!("{indent}  required validations: {}", required.join(", ")));
    }
    if !advisory.is_empty() {
        lines.push(format!("{indent}  advisory validations: {}", advisory.join(", ")));
    }
    if for_review {
        lines.push(String::new());
        lines.push("→ y approve · n request changes (opens message compose)".to_string());
    }
    lines
}

fn plan_id_from_entry(entry: &SessionTimelineEntry) -> Option<String> {
    plan_id_for(entry).or_else(|| render::extract_plan_proposal_id(entry))
}

/// Plan id referenced by a rendered row (including resolved `plan.pending` rows).
fn plan_id_for_row_source(
    visible: &[SessionTimelineEntry],
    src: RowSource,
) -> Option<String> {
    match src {
        RowSource::Single(i) => visible.get(i).and_then(plan_id_from_entry),
        RowSource::Run { start, len } => visible
            .iter()
            .skip(start)
            .take(len)
            .rev()
            .find_map(plan_id_from_entry),
    }
}

/// Drill-down for a timeline row: prefer the live pending PlanFrame revision
/// from the gateway over the row's frozen event payload.
fn open_row_detail_or_plan_review(
    client: &RoomClient,
    root_session_id: &str,
    visible: &[SessionTimelineEntry],
    src: RowSource,
    detail: &mut Option<DetailPane>,
    scroll: &mut u16,
    h_scroll: &mut u16,
) -> Option<String> {
    if let Some(plan_id) = plan_id_for_row_source(visible, src) {
        if open_plan_review(
            client,
            root_session_id,
            &plan_id,
            detail,
            scroll,
            h_scroll,
        ) {
            return Some(format!("plan review: {plan_id} (latest pending revision)"));
        }
    }
    *detail = Some(DetailPane::event(
        detail_for(visible, src),
        spawn_agent_for_row_source(visible, src),
    ));
    *scroll = 0;
    *h_scroll = 0;
    None
}

fn fetch_pending_plan(
    client: &RoomClient,
    root_session_id: &str,
    plan_id: &str,
) -> Option<autonoetic_types::plan_frame::PlanFrame> {
    use autonoetic_types::plan_frame::PlanFramesListPendingResult;
    let params = serde_json::json!({ "root_session_id": root_session_id });
    let value = rpc(client, "planframes.list_pending", params).ok()?;
    let parsed: PlanFramesListPendingResult = serde_json::from_value(value).ok()?;
    parsed
        .plans
        .into_iter()
        .find(|p| p.plan_id == plan_id)
}

fn open_plan_review(
    client: &RoomClient,
    root_session_id: &str,
    plan_id: &str,
    detail: &mut Option<DetailPane>,
    scroll: &mut u16,
    h_scroll: &mut u16,
) -> bool {
    let Some(plan) = fetch_pending_plan(client, root_session_id, plan_id) else {
        return false;
    };
    let lines = format_plan_frame_lines(&plan, true);
    *detail = Some(DetailPane::plan_review(plan_id.to_string(), lines));
    *scroll = 0;
    *h_scroll = 0;
    true
}

/// Build a multi-line plan list for `/plan`, returned as detail-pane lines.
fn list_plans_detail(client: &RoomClient, root_session_id: &str) -> Vec<String> {
    use autonoetic_types::plan_frame::PlanFramesListPendingResult;
    let params = serde_json::json!({ "root_session_id": root_session_id });
    match rpc(client, "planframes.list_pending", params) {
        Ok(value) => match serde_json::from_value::<PlanFramesListPendingResult>(value) {
            Ok(parsed) if parsed.plans.is_empty() => vec![
                format!("(no plans awaiting approval for session '{root_session_id}')"),
                "When the planner proposes a PlanFrame, it appears here and on the timeline."
                    .to_string(),
            ],
            Ok(parsed) => {
                let mut lines = vec![format!("plans awaiting approval ({root_session_id}):")];
                for plan in &parsed.plans {
                    lines.push(String::new());
                    lines.extend(format_plan_frame_lines(plan, false));
                }
                lines.push(String::new());
                lines.push(
                    "→ Enter/p on plan row for review · y approve · n request changes".to_string(),
                );
                lines
            }
            Err(e) => vec![format!("✗ malformed planframes.list_pending response: {e}")],
        },
        Err(e) => vec![format!("✗ planframes.list_pending failed: {e}")],
    }
}

fn latest_pending_plan_id(client: &RoomClient, root_session_id: &str) -> Option<String> {
    let params = serde_json::json!({ "root_session_id": root_session_id });
    let value = rpc(client, "planframes.list_pending", params).ok()?;
    let parsed: autonoetic_types::plan_frame::PlanFramesListPendingResult =
        serde_json::from_value(value).ok()?;
    parsed.plans.last().map(|p| p.plan_id.clone())
}

/// Operator message that resumes the planner after plan approval (mirrors chat
/// TUI inline approve — the planner ends its turn at `awaiting_approval`).
fn plan_execution_wake_message(plan: &autonoetic_types::plan_frame::PlanFrame) -> String {
    plan.execution_wake_hint().unwrap_or_else(|| {
        format!(
            "[Operator approved plan {}] \"{}\" (v{}) is approved. Call planframe_get, then agent_spawn the first agent step — do not call agent_list.",
            plan.plan_id, plan.title, plan.version
        )
    })
}

/// Approve a pending plan and wake the planner session so execution continues.
fn approve_plan_and_wake(
    client: &RoomClient,
    root_session_id: &str,
    plan_id: &str,
    target_agent_id: Option<&str>,
) -> Result<String, String> {
    match rpc(
        client,
        "planframes.approve",
        serde_json::json!({ "plan_id": plan_id, "approved_by": "operator" }),
    ) {
        Ok(value) => {
            let plan: Option<autonoetic_types::plan_frame::PlanFrame> = value
                .get("plan")
                .and_then(|p| serde_json::from_value(p.clone()).ok());
            let title = plan.as_ref().map(|p| p.title.as_str()).unwrap_or("");
            let approval_msg = if title.is_empty() {
                format!("✓ plan approved: {plan_id}")
            } else {
                format!("✓ plan approved: {plan_id} — \"{title}\"")
            };
            let wake = plan
                .as_ref()
                .map(plan_execution_wake_message)
                .unwrap_or_else(|| {
                    format!(
                        "[Operator approved plan {plan_id}] Plan is approved. Call planframe_get, then agent_spawn the first agent step — do not call agent_list."
                    )
                });
            let wake_status = send_message(client, root_session_id, &wake, target_agent_id);
            if wake_status.starts_with('✓') {
                Ok(format!("{approval_msg} — planner notified"))
            } else {
                Ok(format!("{approval_msg} — wake failed: {wake_status}"))
            }
        }
        Err(e) => Err(format!("✗ {e}")),
    }
}

fn list_cron_detail(client: &RoomClient, root_session_id: &str) -> Vec<String> {
    let params = serde_json::json!({
        "root_session_id": root_session_id,
        "limit": 50,
    });
    match rpc(client, "scheduled_jobs.list", params) {
        Ok(value) => match serde_json::from_value::<
            autonoetic_types::scheduled_job::ScheduledJobsListResult,
        >(value)
        {
            Ok(parsed) if parsed.jobs.is_empty() => vec![
                format!("(no scheduled jobs for session '{root_session_id}')"),
                "Jobs survive session close — create one with scheduler.cron in an agent run."
                    .to_string(),
            ],
            Ok(parsed) => {
                let mut lines = vec![format!("scheduled jobs for {root_session_id}:")];
                for job in &parsed.jobs {
                    let msg_preview = if job.message.len() > 48 {
                        format!("{}...", &job.message[..45])
                    } else {
                        job.message.clone()
                    };
                    lines.push(format!(
                        "  {} [{}] → {}@{} · {} · next {}",
                        job.job_id,
                        job.status,
                        job.target_agent_id,
                        job.target_revision_id,
                        job.cron_expr,
                        job.next_run_at
                    ));
                    lines.push(format!("    msg: {msg_preview}"));
                    if let Some(err) = job.last_error.as_deref().filter(|s| !s.is_empty()) {
                        let err_preview = if err.len() > 72 {
                            format!("{}...", &err[..69])
                        } else {
                            err.to_string()
                        };
                        lines.push(format!("    last_error: {err_preview}"));
                    }
                }
                lines.push(
                    "Results appear in this timeline as scheduled_job.* events.".to_string(),
                );
                lines
            }
            Err(e) => vec![format!("✗ malformed scheduled_jobs.list response: {e}")],
        },
        Err(e) => vec![format!("✗ scheduled_jobs.list failed: {e}")],
    }
}

/// `/skills` — the standing view of in-flight skill work (#818).
///
/// Gateway-wide rather than session-scoped: a graduation proposed in one session
/// is enacted in another, and an operator asking "what is in flight" means across
/// the gateway. Rows carry only what is on record — a proposal with no decision
/// shows as `proposed`, never as a guess.
fn list_skills_detail(client: &RoomClient) -> Vec<String> {
    match rpc(
        client,
        "evolution.list_pending",
        serde_json::json!({ "limit": 30 }),
    ) {
        Ok(value) => {
            let pending = value
                .get("pending")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if pending.is_empty() {
                return vec![
                    "(no skill work on record)".to_string(),
                    String::new(),
                    "/crystallize [what worked]  propose one from the current session".to_string(),
                ];
            }
            let counts = value.get("counts").cloned().unwrap_or_default();
            let mut lines = vec![format!(
                "skill work — {} crystallization(s), {} graduation(s), {} awaiting promotion",
                counts
                    .get("crystallizations")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                counts
                    .get("graduations")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                counts
                    .get("awaiting_promotion")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            )];
            lines.push(String::new());
            for row in &pending {
                let str_of = |k: &str| row.get(k).and_then(|v| v.as_str()).unwrap_or("");
                let stage = str_of("stage");
                let verdict = str_of("verdict");
                let outcome = str_of("outcome");
                // stage → outcome reads as a sentence: "judged → landed".
                let state = match (stage, outcome) {
                    (s, "") => s.to_string(),
                    (s, o) => format!("{s} → {o}"),
                };
                let kind = match str_of("kind") {
                    "crystallization" => "crystallize",
                    "graduation" => "graduate  ",
                    "graduation_skipped" => "skipped   ",
                    other => other,
                };
                let target = match row.get("target_agent").and_then(|v| v.as_str()) {
                    Some(a) => a.to_string(),
                    None => "(no target)".to_string(),
                };
                let verdict_part = if verdict.is_empty() {
                    String::new()
                } else {
                    format!(" [{verdict}]")
                };
                lines.push(format!("{kind}  {target}{verdict_part}  — {state}"));
                lines.push(format!("    {}", str_of("summary")));
                let candidates = row
                    .get("target_agent_candidates")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if !candidates.is_empty() {
                    let ids: Vec<String> = candidates
                        .iter()
                        .filter_map(|c| c.as_str())
                        .map(|c| c.chars().take(20).collect())
                        .collect();
                    lines.push(format!(
                        "    ⏳ {} candidate(s) for this agent awaiting promotion: {} \
                         — approve via the gate, not from here",
                        ids.len(),
                        ids.join(", ")
                    ));
                }
                lines.push(format!(
                    "    id {}  {}",
                    str_of("id"),
                    str_of("recorded_at")
                ));
                lines.push(String::new());
            }
            lines.push(
                "read-only — enactment goes through the promotion gate (P-9.15)".to_string(),
            );
            lines
        }
        Err(e) => vec![format!("✗ evolution.list_pending failed: {e}")],
    }
}

fn list_wiki_proposals_detail(client: &RoomClient, root_session_id: &str) -> (Vec<String>, Vec<String>) {
    let params = serde_json::json!({
        "root_session_id": root_session_id,
        "limit": 50,
    });
    match rpc(client, "wiki.proposals_pending", params) {
        Ok(value) => {
            let proposals = value
                .get("proposals")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if proposals.is_empty() {
                return (
                    vec![format!("(no pending wiki proposals for '{root_session_id}')")],
                    Vec::new(),
                );
            }
            let mut lines = vec![format!("pending wiki proposals for {root_session_id}:")];
            let mut ids = Vec::new();
            let max_shown = proposals.len().min(9);
            for (i, p) in proposals.iter().enumerate().take(max_shown) {
                let request_id = p
                    .get("request_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                ids.push(request_id.to_string());
                let agent_id = p
                    .get("agent_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let title = p
                    .get("action")
                    .and_then(|a| a.get("title"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let page_id = p
                    .get("action")
                    .and_then(|a| a.get("page_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                lines.push(format!("  [{}] {} [{}] {} -> {}", i + 1, request_id, agent_id, title, page_id));
            }
            if proposals.len() > 9 {
                lines.push(format!("  ... and {} more (use gateway CLI to list all)", proposals.len() - 9));
            }
            lines.push("→ press a number to view proposal details".to_string());
            (lines, ids)
        }
        Err(e) => (vec![format!("✗ wiki.proposals_pending failed: {e}")], Vec::new()),
    }
}

fn wiki_proposal_detail(client: &RoomClient, request_id: &str) -> Vec<String> {
    match rpc(client, "wiki.proposals_pending", serde_json::json!({})) {
        Ok(value) => {
            let proposals = value
                .get("proposals")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let prop = proposals.into_iter().find(|p| {
                p.get("request_id")
                    .and_then(|v| v.as_str())
                    == Some(request_id)
            });
            match prop {
                Some(p) => {
                    let mut lines = vec![format!("── wiki proposal: {request_id} ──")];
                    lines.push(String::new());
                    if let Some(v) = p.get("agent_id").and_then(|v| v.as_str()) {
                        lines.push(format!("agent:       {v}"));
                    }
                    if let Some(v) = p.get("session_id").and_then(|v| v.as_str()) {
                        lines.push(format!("session:     {v}"));
                    }
                    if let Some(v) = p.get("created_at").and_then(|v| v.as_str()) {
                        lines.push(format!("created:     {v}"));
                    }
                    if let Some(action) = p.get("action") {
                        lines.push(String::new());
                        lines.push("── proposal details ──".to_string());
                        if let Some(v) = action.get("title").and_then(|v| v.as_str()) {
                            lines.push(format!("title:       {v}"));
                        }
                        if let Some(v) = action.get("page_id").and_then(|v| v.as_str()) {
                            lines.push(format!("page_id:     {v}"));
                        }
                        if let Some(v) = action.get("content_sha256").and_then(|v| v.as_str()) {
                            lines.push(format!("content:     {v} (SHA-256)"));
                        }
                        if let Some(tags) = action.get("tags").and_then(|v| v.as_array()) {
                            let tag_str: Vec<&str> = tags.iter()
                                .filter_map(|t| t.as_str())
                                .collect();
                            if !tag_str.is_empty() {
                                lines.push(format!("tags:        {}", tag_str.join(", ")));
                            }
                        }
                        if let Some(v) = p.get("reason").and_then(|v| v.as_str()) {
                            if !v.is_empty() {
                                lines.push(format!("reason:      {v}"));
                            }
                        }
                        if let Some(v) = p.get("decision_reason").and_then(|v| v.as_str()) {
                            if !v.is_empty() {
                                lines.push(format!("decision:    {v}"));
                            }
                        }
                    }
                    lines
                }
                None => vec![format!("✗ proposal {request_id} not found")],
            }
        }
        Err(e) => vec![format!("✗ wiki.proposals_pending failed: {e}")],
    }
}

fn list_sessions_detail(client: &RoomClient, agent: Option<&str>) -> (Vec<String>, Vec<String>) {
    let params = serde_json::json!({
        "agent_id": agent,
        // Over-fetch a little so dropping non-resumable system sessions still
        // leaves a full page of operator-resumable rows. Sessions arrive
        // already ordered by most-recent activity (gateway: `last_ts DESC`).
        "limit": 25,
    });
    match rpc(client, "session.list", params) {
        Ok(value) => match serde_json::from_value::<
            autonoetic_types::session_timeline::SessionListResult,
        >(value)
        {
            Ok(parsed) => {
                // System sessions can't be resumed/switched into — hide them.
                // The remaining rows keep their most-recent-first ordering.
                let sessions: Vec<_> = parsed
                    .sessions
                    .into_iter()
                    .filter(|s| !is_system_session(&s.root_session_id))
                    .take(9)
                    .collect();
                if sessions.is_empty() {
                    let hint = agent
                        .map(|a| format!(" for agent '{a}'"))
                        .unwrap_or_default();
                    return (
                        vec![format!("(no sessions{hint}) — /session <id> or start one with `autonoetic run`")],
                        Vec::new(),
                    );
                }
                let mut lines = if let Some(a) = agent {
                    vec![format!("sessions for agent '{a}':")]
                } else {
                    vec!["recent sessions:".to_string()]
                };
                let ids: Vec<String> = sessions
                    .iter()
                    .map(|s| s.root_session_id.clone())
                    .collect();
                for (i, s) in sessions.iter().enumerate() {
                    // The first row is the most recently active one — flag it.
                    let latest = if i == 0 { "  ← latest" } else { "" };
                    lines.push(format!(
                        "  [{}] {} [{}] @ {}{}",
                        i + 1,
                        s.root_session_id,
                        s.agent_id,
                        s.last_active_at,
                        latest
                    ));
                }
                lines.push("→ press a number to switch, or /session <id>".to_string());
                (lines, ids)
            }
            Err(e) => (
                vec![format!("✗ malformed session.list response: {e}")],
                Vec::new(),
            ),
        },
        Err(e) => (
            vec![format!("✗ session.list failed: {e}")],
            Vec::new(),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    f: &mut Frame,
    root: &str,
    floor: Altitude,
    squash: bool,
    follow: bool,
    prev_viewport_offset: Option<usize>,
    rows: &[RenderedRow],
    selected: usize,
    detail: Option<&DetailPane>,
    detail_scroll: u16,
    detail_h_scroll: u16,
    input: Option<&GateInput>,
    compose: Option<&ComposeInput>,
    slash: Option<&str>,
    status: Option<&str>,
    gate: Option<&GateRef>,
    spinner_glyph: &'static str,
    turn_boundaries: &HashMap<usize, bool>,
    show_reasoning: bool,
    stats: &SessionStats,
    pending_plan_count: usize,
    selected_spawn_agent: Option<&str>,
    info_panel: Option<&InfoPanel>,
    info_scroll: u16,
    gate_count: usize,
    artifact_viewer: Option<&ArtifactViewer>,
    artifact_file_view: Option<&ArtifactFileView>,
    live_content_pane: Option<&LiveContentPane>,
    content_view: Option<&ContentView>,
    gate_modal: Option<&GateModal>,
    gate_modal_entry: Option<&SessionTimelineEntry>,
    approvals_popup: Option<&ApprovalsPopup>,
    approval_rows: &[ApprovalRow],
    grants_panel: Option<&GrantsPanel>,
) {
    let compose_open = compose.is_some() && detail.is_none();
    let chunks = if compose_open {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(COMPOSE_PANEL_HEIGHT),
            Constraint::Length(FOOTER_HEIGHT),
        ])
        .split(f.area())
    } else {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(FOOTER_HEIGHT),
        ])
        .split(f.area())
    };
    let footer_idx = if compose_open { 3 } else { 2 };
    let list_idx = 1usize;

    let header = build_header(root, TuiChannel.kind(), stats, gate_count, follow, floor, squash, chunks[0].width);
    f.render_widget(
        Paragraph::new(header).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    // Gate-input mode owns the screen — never let the detail pane hide the
    // motivation/answer prompt (otherwise Enter appears to do nothing).
    if input.is_none() {
        if let Some(pane) = detail {
            let inner_width = chunks[list_idx].width.saturating_sub(2);
            let inner_height = chunks[list_idx].height.saturating_sub(2) as usize;
            let total_lines = detail_wrap_line_count(&pane.rendered, inner_width);
            let max_scroll = total_lines.saturating_sub(inner_height) as u16;
            let scroll = detail_scroll.min(max_scroll);
            let h = detail_h_scroll;
            let block_title = pane.block_title();
            f.render_widget(
                Paragraph::new(pane.rendered.clone())
                    .block(Block::default().borders(Borders::ALL).title(block_title))
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, h)),
                chunks[list_idx],
            );
            let scroll_hint = if max_scroll > 0 || h > 0 {
                format!(" · j/k ↓↑ ({}/{}) · PgUp/PgDn · h/l ←→ ({})", scroll, max_scroll, h)
            } else {
                String::new()
            };
            let action_hint = if pane.is_plan_review() {
                " y approve · n request changes · Esc/Enter close"
            } else {
                " Esc/Enter close"
            };
            let turn_hint = rows.get(selected).and_then(|r| match r {
                RenderedRow::Line(s) => s
                    .turn_label
                    .as_deref()
                    .map(|l| format!("turn {l}"))
                    .or_else(|| s.turn_index.map(|n| format!("turn {n}"))),
                _ => None,
            });
            let detail_status = format!("{action_hint} · q quit (2×){scroll_hint}");
            let footer = build_footer(
                None,
                None,
                None,
                Some(&detail_status),
                gate,
                approval_rows,
                chunks[footer_idx].width as usize,
                info_panel,
                turn_hint,
            );
            f.render_widget(footer, chunks[footer_idx]);
            return;
        }
    }

    // The terminal width caps each line. Reserve 2 cells for the actor rail
    // and 3 cells for the altitude glyph + space, leaving the rest for the
    // label + headline + detail.
    let width = chunks[list_idx].width as usize;
    let rail_w = 2usize;
    let glyph_w = 3usize;
    let label_w = 12usize.min(width / 4);
    let content_w = width.saturating_sub(rail_w + glyph_w + label_w + 2);

    // Custom viewport renderer. We don't use ratatui's `List` widget here
    // because it overwrites `state.offset` during render — that broke the
    // "keep last row visible when scrolling up" behavior, and it also
    // silently skips multi-line rows (title + preview) that no longer fit
    // in the remaining viewport height, leaving a blank line at the bottom
    // of the list. Rendering each row into its own sub-area gives us full
    // control over both behaviors.
    let list_area = chunks[list_idx];
    let list_height = list_area.height as usize;
    let row_count = rows.len();
    let safe_selected = selected.min(row_count.saturating_sub(1));
    // In follow mode the viewport is pinned to the bottom of the list;
    // compute the per-row heights first so the offset is height-aware.
    let row_heights: Vec<usize> = (0..row_count)
        .map(|i| match &rows[i] {
            RenderedRow::Line(spec) => {
                build_rich_row_lines(
                    spec,
                    i,
                    turn_boundaries,
                    content_w,
                    glyph_w,
                    rail_w,
                    label_w,
                    spinner_glyph,
                    show_reasoning,
                )
                .len()
            }
            RenderedRow::Collapsed { .. } => 1,
        })
        .collect();
    let viewport_offset = if follow {
        // Pin to the bottom; if the last row is multi-line, the offset
        // adjusts to keep the last row fully visible.
        compute_viewport_offset(row_count.saturating_sub(1), list_height, &row_heights, None)
    } else {
        compute_viewport_offset(safe_selected, list_height, &row_heights, prev_viewport_offset)
    };
    let highlight = Style::default().add_modifier(Modifier::REVERSED);

    let mut y: u16 = list_area.y;
    let list_end_y = list_area.y.saturating_add(list_area.height);
    let mut i = viewport_offset;
    while (y as usize) < (list_end_y as usize) && i < row_count {
        let remaining = (list_end_y - y) as usize;
        let (lines, line_count) = match &rows[i] {
            RenderedRow::Line(spec) => {
                let lines = build_rich_row_lines(
                    spec,
                    i,
                    turn_boundaries,
                    content_w,
                    glyph_w,
                    rail_w,
                    label_w,
                    spinner_glyph,
                    show_reasoning,
                );
                let n = lines.len();
                (lines, n)
            }
            RenderedRow::Collapsed {
                count,
                summary,
                in_flight,
            } => {
                let line = build_collapsed_row_line(*count, summary, *in_flight, spinner_glyph);
                (vec![line], 1usize)
            }
        };
        // Defensive: if the multi-line row's height exceeds what's left in
        // the viewport, stop here. With the height-aware offset above this
        // should not happen, but guard against any future code that shifts
        // the offset out of sync with the row heights.
        if line_count == 0 || line_count > remaining {
            break;
        }
        let styled: Vec<Line<'static>> = if i == safe_selected {
            lines
                .into_iter()
                .map(|mut l| {
                    l.style = l.style.patch(highlight);
                    l
                })
                .collect()
        } else {
            lines
        };
        let row_area = Rect {
            x: list_area.x,
            y,
            width: list_area.width,
            height: line_count as u16,
        };
        f.render_widget(Paragraph::new(styled), row_area);
        y = y.saturating_add(line_count as u16);
        i += 1;
    }

    if let Some(c) = compose {
        draw_compose_input(f, c, chunks[2]);
    }

    let turn_hint = rows.get(safe_selected).and_then(|r| match r {
        RenderedRow::Line(s) => s
            .turn_label
            .as_deref()
            .map(|l| format!("turn {l}"))
            .or_else(|| s.turn_index.map(|n| format!("turn {n}"))),
        _ => None,
    });
    let footer = build_footer(
        slash,
        compose,
        input,
        status,
        gate,
        approval_rows,
        chunks[footer_idx].width as usize,
        info_panel,
        turn_hint,
    );
    f.render_widget(footer, chunks[footer_idx]);

    // Overlays render last (on top of everything) so they are never painted over.
    if let Some(panel) = info_panel {
        let area = centered_rect(60, 70, f.area());
        f.render_widget(Clear, area);
        let inner_height = area.height.saturating_sub(2) as usize;
        let total_lines = panel.lines.len();
        let max_scroll = total_lines.saturating_sub(inner_height) as u16;
        let scroll = info_scroll.min(max_scroll);
        let text: Vec<Line> = panel
            .lines
            .iter()
            .map(|l| Line::from(Span::styled(l.clone(), Style::default().bg(Color::Black))))
            .collect();
        f.render_widget(
            Paragraph::new(text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Session Info [?/Esc close] ")
                        .border_style(Style::default().fg(Color::Cyan))
                        .style(Style::default().bg(Color::Black)),
                )
                .scroll((scroll, 0)),
            area,
        );
    }

    if let Some(ref view) = artifact_file_view {
        let area = centered_rect(80, 85, f.area());
        f.render_widget(Clear, area);
        let lines: Vec<Line> = view
            .content
            .lines()
            .map(|l| Line::from(Span::styled(l.to_string(), Style::default().bg(Color::Black))))
            .collect();
        let inner_height = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(inner_height) as u16;
        let scroll = view.scroll.min(max_scroll);
        let title = format!(
            " {} {} → {} [Esc back] ",
            view.artifact_id, view.file_name, lines.len()
        );
        f.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(Style::default().fg(Color::Green))
                        .style(Style::default().bg(Color::Black)),
                )
                .scroll((scroll, 0)),
            area,
        );
    } else if let Some(ref viewer) = artifact_viewer {
        let area = centered_rect(60, 60, f.area());
        f.render_widget(Clear, area);
        // Build a unified line list: a Layers section (when present) rendered
        // as non-selectable info lines, followed by the Files section. The
        // `selected` index only addresses files, so layer + separator lines
        // are offset accordingly.
        let mut lines: Vec<Line> = Vec::new();
        let mut selectable_start = 0usize;
        if !viewer.layers.is_empty() {
            for l in &viewer.layers {
                lines.push(Line::from(Span::styled(
                    format!("🧱 {} · {}", l.name, short_digest(&l.digest)),
                    Style::default().fg(Color::Cyan).bg(Color::Black),
                )));
            }
            lines.push(Line::from(Span::styled(
                format!("── {} files ──", viewer.files.len()),
                Style::default().fg(Color::DarkGray).bg(Color::Black),
            )));
            selectable_start = lines.len();
        }
        for (i, f) in viewer.files.iter().enumerate() {
            let marker = if i == viewer.selected { " > " } else { "   " };
            let style = if i == viewer.selected {
                Style::default().fg(Color::Yellow).bg(Color::Black)
            } else {
                Style::default().bg(Color::Black)
            };
            lines.push(Line::from(Span::styled(
                format!("{}{}", marker, f.name),
                style,
            )));
        }
        let layer_tag = if viewer.layers.is_empty() {
            String::new()
        } else {
            format!(" {} layers ·", viewer.layers.len())
        };
        let title = format!(
            " {} [{}]{} {} files [o/Esc] ",
            viewer.artifact_id, viewer.kind, layer_tag, viewer.files.len()
        );
        let inner_height = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(inner_height);
        // Scroll so the selected file stays in view, accounting for the layer
        // header offset.
        let selected_line = selectable_start + viewer.selected;
        let scroll = selected_line
            .saturating_sub(inner_height / 2)
            .min(max_scroll) as u16;
        f.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(Style::default().fg(Color::Green))
                        .style(Style::default().bg(Color::Black)),
                )
                .scroll((scroll, 0)),
            area,
        );
    }

    // Pillar D: live session content tree + markdown-aware viewer. These layer
    // above the timeline/artifact overlays; the content viewer renders the
    // blob through the markdown pipeline (the key win over ArtifactFileView's
    // plain-text rendering).
    if let Some(ref view) = content_view {
        let area = centered_rect(80, 85, f.area());
        f.render_widget(Clear, area);
        // Detect file extension to choose rendering mode.
        // Markdown files go through the full pipeline; code files are wrapped
        // in a language-tagged code fence for syntax-styled display.
        let ext = std::path::Path::new(&view.name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let display_text = match ext.as_str() {
            "md" | "markdown" | "" => {
                super::markdown::normalize_narrative_prose(&view.content)
            }
            _ => {
                // Wrap in a code fence with the language tag so the markdown
                // renderer applies code-block styling (dimmed, language header).
                if view.content.contains("```") {
                    super::markdown::normalize_narrative_prose(&view.content)
                } else {
                    super::markdown::normalize_narrative_prose(
                        &format!("```{}\n{}\n```", ext, view.content),
                    )
                }
            }
        };
        let lines = super::markdown::render_markdown(&display_text);
        let inner_height = area.height.saturating_sub(2) as usize;
        // Wrap-aware scroll: count wrapped lines, not raw Lines, so the scroll
        // offset tracks what the operator sees (mirrors detail-pane wrap).
        let wrap_w = area.width.saturating_sub(2) as u16;
        let total = detail_wrap_line_count(&lines, wrap_w);
        let max_scroll = total.saturating_sub(inner_height) as u16;
        let scroll = view.scroll.min(max_scroll);
        let title = format!(
            " 📝 {} [draft · not a vetted artifact] [m comment · O open in editor · Esc back] ",
            view.name
        );
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(Style::default().fg(Color::Cyan))
                        .style(Style::default().bg(Color::Black)),
                )
                .scroll((scroll, 0)),
            area,
        );
    } else if let Some(ref pane) = live_content_pane {
        let area = centered_rect(65, 70, f.area());
        f.render_widget(Clear, area);

        // Determine which nodes are visible, honoring plan-version folding
        // and artifact-child folding. Must stay in sync with
        // `LiveContentPane::visible_indices()`.
        let mut visible_nodes: Vec<(usize, &LiveContentNode)> = Vec::new();
        let mut last_plan_id: Option<String> = None;
        let mut last_plan_was_latest = false;
        let mut last_artifact_id: Option<String> = None;
        for (idx, node) in pane.nodes.iter().enumerate() {
            match node {
                LiveContentNode::Plan {
                    plan_id,
                    is_latest,
                    ..
                } => {
                    last_plan_id = Some(plan_id.clone());
                    last_plan_was_latest = *is_latest;
                    last_artifact_id = None;
                    if *is_latest || !pane.is_folded(plan_id) {
                        visible_nodes.push((idx, node));
                    }
                }
                LiveContentNode::PlanStep { .. } => {
                    last_artifact_id = None;
                    // Plan steps only appear under the latest plan version;
                    // hide them if the parent plan is a folded older revision.
                    if let Some(ref pid) = last_plan_id {
                        if !last_plan_was_latest && pane.is_folded(pid) {
                            continue;
                        }
                    }
                    visible_nodes.push((idx, node));
                }
                LiveContentNode::Artifact { artifact_id, .. } => {
                    last_plan_id = None;
                    last_artifact_id = Some(artifact_id.clone());
                    // Parent is always visible.
                    visible_nodes.push((idx, node));
                }
                LiveContentNode::ArtifactFile { artifact_id, .. }
                | LiveContentNode::ArtifactLayer { artifact_id, .. } => {
                    last_artifact_id = Some(artifact_id.clone());
                    if pane.is_artifact_folded(artifact_id) {
                        continue;
                    }
                    visible_nodes.push((idx, node));
                }
                _ => {
                    last_plan_id = None;
                    last_artifact_id = None;
                    visible_nodes.push((idx, node));
                }
            }
        }

        let mut visual_lines: Vec<(usize, String)> = Vec::new();
        let mut node_iter = visible_nodes.into_iter().peekable();
        for (sec_idx, &(_start, label)) in pane.sections.iter().enumerate() {
            visual_lines.push((usize::MAX, format!("── {label} ──")));
            let end = pane
                .sections
                .get(sec_idx + 1)
                .map(|&(s, _)| s)
                .unwrap_or(pane.nodes.len());
            while let Some((node_idx, _node)) = node_iter.peek() {
                if *node_idx >= end {
                    break;
                }
                let (node_idx, node) = node_iter.next().unwrap();
                let text = match node {
                    LiveContentNode::Plan {
                        plan_id,
                        title,
                        status,
                        version,
                        is_latest,
                    } => {
                        let label = if title.is_empty() {
                            format!("plan v{version}")
                        } else {
                            title.clone()
                        };
                        let latest_tag = if *is_latest { " ✦" } else { "" };
                        let count = pane
                            .nodes
                            .iter()
                            .filter(|n| matches!(n, LiveContentNode::Plan { plan_id: pid, .. } if pid == plan_id))
                            .count();
                        let expand_hint = if count > 1 {
                            if pane.is_folded(plan_id) {
                                format!(" [{count} versions · x unfold]")
                            } else {
                                " [x fold older]".to_string()
                            }
                        } else {
                            String::new()
                        };
                        format!("  {label} v{version} [{status}]{latest_tag}{expand_hint}")
                    }
                    LiveContentNode::PlanStep { title } => {
                        format!("    ▶ {title}")
                    }
                    LiveContentNode::Artifact {
                        artifact_id,
                        artifact_ref: _,
                        kind,
                        name,
                    } => {
                        // Count this artifact's direct children (files + layers)
                        // to decide whether to show a fold hint.
                        let child_count = pane
                            .nodes
                            .iter()
                            .filter(|n| match n {
                                LiveContentNode::ArtifactFile { artifact_id: aid, .. }
                                | LiveContentNode::ArtifactLayer { artifact_id: aid, .. } =>
                                    aid == artifact_id,
                                _ => false,
                            })
                            .count();
                        let fold_hint = if child_count > 0 {
                            if pane.is_artifact_folded(artifact_id) {
                                format!(" [{child_count} items · x unfold]")
                            } else {
                                " [x fold]".to_string()
                            }
                        } else {
                            String::new()
                        };
                        format!("  {name} [{kind}]{fold_hint}")
                    }
                    LiveContentNode::ArtifactLayer {
                        layer_id: _,
                        name,
                        mount_path: _,
                        digest,
                        artifact_id: _,
                        artifact_ref: _,
                    } => {
                        format!("    🧱 {name} · {}", short_digest(digest))
                    }
                    LiveContentNode::ArtifactFile {
                        name,
                        alias: _,
                        artifact_id: _,
                        artifact_ref: _,
                    } => {
                        format!("    📄 {name}")
                    }
                    LiveContentNode::Draft {
                        name,
                        alias: _,
                        visibility,
                    } => {
                        let lock = match visibility.as_str() {
                            "private" => " 🔒",
                            "global" => " 🌐",
                            _ => "",
                        };
                        format!("  {name}{lock}")
                    }
                };
                visual_lines.push((node_idx, text));
            }
        }

        let selected_visual = visual_lines
            .iter()
            .position(|(idx, _)| *idx == pane.selected)
            .unwrap_or(0);

        let lines: Vec<Line> = visual_lines
            .iter()
            .enumerate()
            .map(|(vi, (node_idx, text))| {
                let is_header = *node_idx == usize::MAX;
                let is_selected = vi == selected_visual;
                let style = if is_header {
                    Style::default()
                        .fg(Color::DarkGray)
                        .bg(Color::Black)
                } else if is_selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .bg(Color::Black)
                } else {
                    Style::default().bg(Color::Black)
                };
                let marker = if is_selected && !is_header {
                    " >"
                } else {
                    "  "
                };
                Line::from(Span::styled(
                    format!("{marker}{text}"),
                    style,
                ))
            })
            .collect();

        let inner_height = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(inner_height);
        let scroll = selected_visual
            .saturating_sub(inner_height / 2)
            .min(max_scroll) as u16;

        let title = format!(
            " 📋 Live Content — {} items (Plans · Artifacts · Drafts) [j/k nav · o open · x fold/unfold · Esc close] ",
            pane.nodes.len()
        );
        f.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(Style::default().fg(Color::Cyan))
                        .style(Style::default().bg(Color::Black)),
                )
                .scroll((scroll, 0)),
            area,
        );
    }

    if let Some(modal) = gate_modal {
        if compose.is_none() {
            draw_gate_modal(f, modal, gate_modal_entry, input, status);
        }
    }

    if let Some(popup) = approvals_popup {
        draw_approvals_popup(f, popup);
    }

    // Grants panel renders last (on top of everything).
    if let Some(panel) = grants_panel {
        draw_grants_panel(f, panel);
    }
}

fn draw_approvals_popup(f: &mut Frame, popup: &ApprovalsPopup) {
    let rows = &popup.rows;
    let pending = rows.iter().filter(|r| r.is_pending).count();
    let resolved_count = rows.len() - pending;
    let title = format!(
        " Approvals — {} pending · {} resolved [A/Esc close · j/k nav · y approve+grant · o once · n reject] ",
        pending, resolved_count,
    );
    let area = centered_rect(75, 70, f.area());
    f.render_widget(Clear, area);

    let inner_height = area.height.saturating_sub(2) as usize;
    let max_scroll = rows.len().saturating_sub(inner_height) as u16;
    let scroll = popup.scroll.min(max_scroll);

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let marker = if i == popup.selected { "▸" } else { " " };
            let status_icon = if r.is_pending { "⏳" } else { "✓" };
            let id_short: String = r.id.chars().take(20).collect();
            let style = if i == popup.selected {
                Style::default().fg(Color::Yellow).bg(Color::Black)
            } else if r.is_pending {
                Style::default().fg(Color::Cyan).bg(Color::Black)
            } else {
                Style::default().fg(Color::DarkGray).bg(Color::Black)
            };
            Line::from(Span::styled(
                format!(" {} {} {:<8} {:<20} {}", marker, status_icon, r.kind, id_short, r.summary),
                style,
            ))
        })
        .collect();

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(Color::Yellow))
                    .style(Style::default().bg(Color::Black)),
            )
            .scroll((scroll, 0)),
        area,
    );
}

fn draw_grants_panel(f: &mut Frame, panel: &GrantsPanel) {
    let rows = &panel.rows;
    let n_approval = rows
        .iter()
        .filter(|r| r.kind == GrantKind::SessionApproval)
        .count();
    let n_declass = rows.len() - n_approval;
    let taint_str = panel
        .taint
        .clone()
        .unwrap_or_else(|| "unrestricted".to_string());
    let title = format!(
        " Grants — {} approval · {} declass · taint {} [G/Esc close · j/k nav · r revoke] ",
        n_approval, n_declass, taint_str,
    );
    let area = centered_rect(82, 78, f.area());
    f.render_widget(Clear, area);

    let inner_height = area.height.saturating_sub(2) as usize;
    let max_scroll = rows.len().saturating_sub(inner_height) as u16;
    let scroll = panel.scroll.min(max_scroll);

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let selected = i == panel.selected;
            let marker = if selected { "▸" } else { " " };
            let kind_str = r.kind.label();
            let kind_style = if r.kind == GrantKind::EgressDeclassification {
                Color::Magenta
            } else {
                Color::Cyan
            };
            let mut spans: Vec<Span> = vec![
                Span::raw(format!(" {marker} ")),
                Span::styled(format!("{kind_str:<8}"), Style::default().fg(kind_style)),
                Span::raw(format!(" #{:<5} ", r.id)),
                Span::raw(&r.summary),
            ];
            if selected {
                spans.push(Span::raw(format!("   · {}", r.detail)));
                if panel.pending_revoke.is_some() {
                    spans.push(Span::styled(
                        "  [press r again to CONFIRM revoke]",
                        Style::default().fg(Color::Red).bold(),
                    ));
                }
            }
            let style = if selected {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            Line::from(spans).style(style)
        })
        .collect();

    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(Color::Yellow))
                    .style(Style::default().bg(Color::Black)),
            )
            .scroll((scroll, 0)),
        area,
    );
}

fn gate_modal_title(modal: &GateModal, entry: Option<&SessionTimelineEntry>) -> String {
    if let Some(entry) = entry {
        match payload_field_str(entry, "action").as_deref() {
            Some("revision_promote") => {
                let agent = payload_field_str(entry, "agent_id").unwrap_or_else(|| "agent".into());
                return format!(" ⚠ PROMOTE AGENT — {agent} ");
            }
            Some("session_escalate") => {
                let reason = payload_field_str(entry, "reason")
                    .or_else(|| payload_field_str(entry, "summary"))
                    .unwrap_or_else(|| "needs guidance".into());
                return format!(
                    " ⚠ SESSION ESCALATION — {} ",
                    render::one_line(
                        &markdown::strip_markdown_if_markdown(&reason),
                        48
                    )
                );
            }
            Some("credential_prompt") => {
                // `service` is not in the timeline payload for CredentialPrompt
                // (it arrives via approvals.inspect, rendered in the modal body).
                // Use a service-agnostic title; the body shows service + fields.
                return " ⚠ CREDENTIAL SETUP — enter secret ".to_string();
            }
            _ => {}
        }
        if entry.event_type == "escalation.pending" {
            let synthesis = payload_field_str(entry, "synthesis")
                .unwrap_or_else(|| "operator decision".into());
            return format!(
                " ⚠ PROMOTION ESCALATION — {} ",
                render::one_line(
                    &markdown::strip_markdown_if_markdown(&synthesis),
                    48
                )
            );
        }
    }
    match modal.gate.kind {
        GateKind::Approval => " ⚠ APPROVAL REQUIRED ".to_string(),
        GateKind::WikiProposal => " ⚠ WIKI PROPOSAL ".to_string(),
        GateKind::Escalation => " ⚠ ESCALATION ".to_string(),
        GateKind::Interaction => " ❓ QUESTION PENDING ".to_string(),
        GateKind::Plan => {
            let title = entry
                .and_then(|e| payload_field_str(e, "title"))
                .unwrap_or_default();
            if title.is_empty() {
                " ⚠ PLAN AWAITING APPROVAL ".to_string()
            } else {
                format!(" ⚠ PLAN — {} ", render::one_line(&title, 48))
            }
        }
    }
}

fn gate_modal_peek_summary(modal: &GateModal, entry: Option<&SessionTimelineEntry>) -> String {
    if let Some(entry) = entry {
        let spec = render::render_spec(entry);
        // Escalation synthesis often contains markdown (**bold**, lists, etc.) that
        // looks jarring in a single-line title. Strip it when it looks structured.
        let headline = if entry.event_type == "escalation.pending" {
            markdown::strip_markdown_if_markdown(&spec.headline)
        } else {
            spec.headline
        };
        return render::one_line(&headline, 72);
    }
    format!("Gate {}", modal.gate.id)
}

fn gate_modal_input_panel_lines(
    gi: &GateInput,
    width: usize,
    status: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let wrap_w = width.saturating_sub(2).max(20);

    // CredentialPrompt secret-entry phase: dedicated panel that makes the
    // vault-boundary guarantee explicit before the operator types anything.
    if gi.secret_phase {
        lines.push(Line::from(Span::styled(
            "🔒 Enter the requested secret below.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        for assurance in [
            "Values go directly into the encrypted vault —",
            "the agent (LLM) never sees them.",
        ] {
            for wrapped in render::wrap_display_lines(assurance, wrap_w) {
                lines.push(Line::from(Span::styled(
                    wrapped,
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        lines.push(Line::raw(""));

        let idx = gi.secret_values.len();
        let total = gi.secret_fields.len();
        if let Some(field) = gi.secret_fields.get(idx) {
            let mask_label = if field.masked { "masked" } else { "visible" };
            let label_line = format!(
                "{} (field {} of {}, {}):",
                field.label, idx + 1, total, mask_label
            );
            lines.push(Line::from(Span::styled(
                label_line,
                Style::default().fg(Color::Cyan),
            )));
        }
        lines.push(Line::from(Span::styled(
            format!("{}▏", rendered_input_buffer(gi)),
            Style::default().fg(Color::White),
        )));

        if let Some(err) = status.filter(|s| s.starts_with('✗')) {
            lines.push(Line::from(Span::styled(
                err.to_string(),
                Style::default().fg(Color::Red),
            )));
        }

        let is_last = idx + 1 >= total;
        let hint = if is_last {
            "Enter submit · Esc cancel".to_string()
        } else {
            "Enter next field · Esc cancel".to_string()
        };
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }

    if let Some(ref phrase) = gi.required_confirm_phrase {
        lines.push(Line::from(Span::styled(
            "Type this phrase exactly (also records your §O motivation):",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        for line in render::wrap_display_lines(phrase, wrap_w) {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(Color::Green),
            )));
        }
        lines.push(Line::raw(""));
    } else if matches!(gi.action, GateAction::Reject) {
        lines.push(Line::from(Span::styled(
            "Rejection reason (required):",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    } else if gi.motivation_required {
        lines.push(Line::from(Span::styled(
            "Approval motivation (required):",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    }

    let input_label = if gi.details_mode {
        "DETAILS".to_string()
    } else if gi.required_confirm_phrase.is_some() {
        "Your input".to_string()
    } else {
        gate_input_label(gi).trim_end_matches(':').to_string()
    };
    lines.push(Line::from(vec![
        Span::styled(format!("{input_label}: "), Style::default().fg(Color::Cyan)),
        Span::styled(format!("{}▏", rendered_input_buffer(gi)), Style::default().fg(Color::White)),
    ]));

    if let Some(err) = status.filter(|s| s.starts_with('✗')) {
        lines.push(Line::from(Span::styled(
            err.to_string(),
            Style::default().fg(Color::Red),
        )));
    } else if gi.details_mode {
        lines.push(Line::from(Span::styled(
            "✓ type your details, then press Enter".to_string(),
            Style::default().fg(Color::Green),
        )));
    }

    let choices = gi
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| format!("[{}] {}", i + 1, render::one_line(&o.label, 28)))
        .collect::<Vec<_>>()
        .join(" · ");
    let hint = if gi.details_mode {
        "Enter submit details · Esc cancel details".to_string()
    } else if gi.options.is_empty() {
        "Enter submit · Esc back · Esc×2 peek timeline".to_string()
    } else {
        format!("{choices}   Enter submit · Esc back")
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )));
    lines
}

fn gate_modal_input_panel_height(gi: &GateInput, width: u16, status: Option<&str>) -> u16 {
    let lines = gate_modal_input_panel_lines(gi, width as usize, status);
    lines.len().max(3) as u16 + 1
}

fn draw_gate_modal_peek_banner(
    f: &mut Frame,
    modal: &GateModal,
    entry: Option<&SessionTimelineEntry>,
) {
    let full = f.area();
    let banner_h = 3u16.min(full.height);
    let area = Rect {
        x: full.x,
        y: full.y,
        width: full.width,
        height: banner_h,
    };
    f.render_widget(Clear, area);
    let summary = gate_modal_peek_summary(modal, entry);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" ⚠ OPERATOR ACTION PENDING ")
        .border_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(area);
    let id_line = match modal.gate.kind {
        GateKind::Interaction => {
            "id: {id}  ·  g or Enter answer  ·  r reply  ·  Esc peek".to_string()
        }
        GateKind::Plan => {
            "id: {id}  ·  g or Enter review  ·  y approve  ·  n revise".to_string()
        }
        _ => "id: {id}  ·  g or Enter resolve  ·  y approve  ·  n reject".to_string(),
    };
    let id_line = id_line.replace("{id}", &modal.gate.id);
    let text = vec![
        Line::from(Span::styled(
            summary,
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            id_line,
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(text), inner);
}

fn draw_gate_modal(
    f: &mut Frame,
    modal: &GateModal,
    entry: Option<&SessionTimelineEntry>,
    input: Option<&GateInput>,
    status: Option<&str>,
) {
    let inspect_lines = &modal.inspect_lines;
    if modal.peek_timeline {
        draw_gate_modal_peek_banner(f, modal, entry);
        return;
    }

    let full = f.area();
    f.render_widget(Clear, full);
    f.render_widget(
        Paragraph::new("").style(Style::default().bg(Color::Rgb(20, 20, 20))),
        full,
    );

    let area = centered_rect(82, 78, full);
    f.render_widget(Clear, area);

    let border_color = match modal.gate.kind {
        GateKind::Interaction => Color::Cyan,
        _ => Color::Yellow,
    };
    let title = gate_modal_title(modal, entry);

    let mut content_lines: Vec<Line<'static>> = Vec::new();
    if let Some(entry) = entry {
        if entry.event_type == "escalation.pending" {
            let synthesis_raw = payload_field_str(entry, "synthesis")
                .unwrap_or_else(|| "operator decision".into());
            let synthesis_plain = markdown::strip_markdown_if_markdown(&synthesis_raw);

            let field = |key: &str| payload_field_str(entry, key);
            if let Some(id) = field("escalation_id") {
                content_lines.push(Line::from(vec![
                    Span::styled("escalation: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(id),
                ]));
            }
            if let Some(req) = entry
                .refs
                .approval_request_id
                .clone()
                .or_else(|| field("request_id"))
            {
                content_lines.push(Line::from(vec![
                    Span::styled("approval: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(req),
                ]));
            }
            if let Some(agent) = field("agent_id") {
                content_lines.push(Line::from(vec![
                    Span::styled("agent: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(agent),
                ]));
            }
            if let Some(rev) = field("revision_id").filter(|r| !r.is_empty()) {
                content_lines.push(Line::from(vec![
                    Span::styled("revision: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(rev),
                ]));
            }
            if let Some(kind) = field("escalation_type") {
                content_lines.push(Line::from(vec![
                    Span::styled("type: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(kind),
                ]));
            }
            if let Some(artifact) = entry.refs.artifact_id.clone() {
                content_lines.push(Line::from(vec![
                    Span::styled("artifact: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(artifact),
                ]));
            }
            content_lines.push(Line::from(Span::styled(
                "synthesis:",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            let spec = render::render_spec(entry);
            content_lines.push(Line::from(Span::styled(
                spec.headline,
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            )));
            // For plan gates the full proposal is rendered below under "Plan details:".
            // The timeline entry's detail duplicates the same fields (objective, steps,
            // version) and, after an amendment, can show a stale v1 summary next to the
            // live v2 detail — which looks like two versions. Keep only non-redundant
            // amendment context from the entry (diff_summary / reason) and let the plan
            // detail section carry the canonical steps/validations.
            if modal.gate.kind != GateKind::Plan {
                if let Some(detail) = spec.detail {
                    for line in detail.lines() {
                        content_lines.push(Line::from(line.to_string()));
                    }
                }
            } else {
                // Surface amendment context: diff_summary first, then reason.
                // Either field can explain why v2 replaced v1.
                if let Some(diff) = payload_field_str(entry, "diff_summary") {
                    if !diff.trim().is_empty() {
                        content_lines.push(Line::from(Span::styled(
                            format!("Amendment context: {diff}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
                if let Some(reason) = payload_field_str(entry, "reason") {
                    if !reason.trim().is_empty() {
                        content_lines.push(Line::from(Span::styled(
                            format!("Amendment reason: {reason}"),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
            }
        }
    } else {
        content_lines.push(Line::from(format!("Gate id: {}", modal.gate.id)));
    }
    // Plan gates render the canonical plan detail in a dedicated section below.
    // Do not auto-inject inspect_lines for plan gates in either branch or the plan
    // would appear twice (the headline is already shown above the dedicated section).
    if content_lines.len() <= 2 && !inspect_lines.is_empty() && modal.gate.kind != GateKind::Plan {
        content_lines.push(Line::from(Span::styled(
            "From approval record:",
            Style::default().fg(Color::DarkGray),
        )));
        for line in inspect_lines {
            content_lines.push(Line::from(line.clone()));
        }
    } else if !inspect_lines.is_empty()
        && modal.gate.kind != GateKind::Plan
        && entry.is_some_and(|e| {
            payload_field_str(e, "reason").is_none()
                && payload_field_str(e, "synthesis").is_none()
                && payload_field_str(e, "summary").is_none()
        })
    {
        for line in inspect_lines {
            content_lines.push(Line::from(line.clone()));
        }
    }

    // For plan gates, always append the formatted plan detail (steps, validations,
    // review hints) so the operator can read the full proposal inside the modal.
    if modal.gate.kind == GateKind::Plan && !inspect_lines.is_empty() {
        content_lines.push(Line::from(Span::styled(
            "Plan details:",
            Style::default().fg(Color::DarkGray),
        )));
        for line in inspect_lines {
            content_lines.push(Line::from(line.clone()));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(border_color).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(area);
    let inner_width = inner.width;

    // Render the promotion-escalation synthesis body as markdown now that we
    // know the available width. This is done after block creation because the
    // wrapped lines depend on the modal's inner width.
    if entry.is_some_and(|e| e.event_type == "escalation.pending") {
        let synthesis_raw = payload_field_str(entry.unwrap(), "synthesis")
            .unwrap_or_else(|| "operator decision".into());
        let synthesis_plain = markdown::strip_markdown_if_markdown(&synthesis_raw);
        let content_w = inner_width.saturating_sub(4) as usize;
        let normalized = markdown::normalize_narrative_prose(&synthesis_plain);
        for md_line in markdown::render_markdown(&normalized) {
            let text = line_display_text(&md_line);
            if text.trim().is_empty() {
                content_lines.push(Line::from("  ".to_string()));
                continue;
            }
            let style = md_line
                .spans
                .iter()
                .find(|s| !s.content.is_empty())
                .map(|s| s.style)
                .unwrap_or_default();
            for (i, chunk) in word_wrap_text(text.trim_end(), content_w).into_iter().enumerate() {
                let prefix = if i == 0 { "  " } else { "    " };
                content_lines.push(Line::from(vec![
                    Span::raw(prefix.to_string()),
                    Span::styled(chunk, style),
                ]));
            }
        }
    }

    let preview_phrase = input
        .is_none()
        .then(|| entry.and_then(|e| payload_field_str(e, "confirm_phrase")))
        .flatten();
    let footer_h = if let Some(gi) = input {
        gate_modal_input_panel_height(gi, inner_width, status)
    } else if preview_phrase.is_some() {
        4 + render::wrap_display_lines(preview_phrase.as_deref().unwrap_or(""), inner_width as usize - 2)
            .len() as u16
    } else {
        2
    };
    let chunks = Layout::vertical([
        Constraint::Min(6),
        Constraint::Length(footer_h),
    ])
    .split(inner);

    let inner_height = chunks[0].height as usize;
    let max_scroll = content_lines.len().saturating_sub(inner_height) as u16;
    let scroll = modal.scroll.min(max_scroll);

    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(content_lines).scroll((scroll, 0)),
        chunks[0],
    );

    if let Some(gi) = input {
        let panel_lines = gate_modal_input_panel_lines(gi, inner_width as usize, status);
        let panel_block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray))
            .style(Style::default().bg(Color::Black));
        let panel_inner = panel_block.inner(chunks[1]);
        f.render_widget(panel_block, chunks[1]);
        f.render_widget(Paragraph::new(panel_lines), panel_inner);
    } else {
        let mut footer_lines: Vec<Line<'static>> = Vec::new();
        if let Some(ref phrase) = preview_phrase {
            footer_lines.push(Line::from(Span::styled(
                "Confirm phrase (shown now — type after y):",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
            for line in render::wrap_display_lines(phrase, inner_width as usize - 2) {
                footer_lines.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(Color::Green),
                )));
            }
            footer_lines.push(Line::raw(""));
        }
        let action_hint = match modal.gate.kind {
            GateKind::Approval | GateKind::WikiProposal | GateKind::Escalation => {
                "y approve · n reject · j/k scroll details · Esc peek timeline"
            }
            GateKind::Interaction => "Enter/r answer · j/k scroll · Esc peek timeline",
            GateKind::Plan => {
                "y approve · n request changes · j/k scroll details · Esc peek timeline"
            }
        };
        footer_lines.push(Line::from(Span::styled(
            action_hint,
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(Paragraph::new(footer_lines), chunks[1]);
    }
}

/// Render the compose editor with wrapped lines and an inverted cursor cell.
fn draw_compose_input(f: &mut Frame, compose: &ComposeInput, area: Rect) {
    let prefix = Span::styled("MESSAGE: ", Style::default().fg(Color::Green));
    let inner_width = area.width.saturating_sub(2) as usize;

    let text = if compose.buffer.is_empty() {
        let mut lines = wrap_spans(&[prefix], inner_width);
        if let Some(last) = lines.last_mut() {
            let mut last_spans = std::mem::take(last);
            last_spans
                .spans
                .push(Span::styled(" ", Style::default().bg(Color::White)));
            *last = Line::from(last_spans);
        }
        Text::from(lines)
    } else {
        let before = &compose.buffer[..compose.cursor_pos];
        let after = &compose.buffer[compose.cursor_pos..];
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

/// Wrap styled spans to `max_width` display cells (Unicode-aware).
fn wrap_spans(spans: &[Span], max_width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_line: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;

    for span in spans {
        for c in span.content.chars() {
            let style = span.style;
            if c == '\n' {
                if !current_line.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut current_line)));
                    current_width = 0;
                } else {
                    lines.push(Line::raw(""));
                }
                continue;
            }
            let s = c.to_string();
            let cw = UnicodeWidthStr::width(s.as_str());
            if current_width + cw > max_width && !current_line.is_empty() {
                lines.push(Line::from(std::mem::take(&mut current_line)));
                current_width = 0;
            }
            current_width += cw;
            current_line.push(Span::styled(s, style));
        }
    }

    if !current_line.is_empty() {
        lines.push(Line::from(current_line));
    }
    if lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_sessions_are_hidden_from_session_list() {
        // The reserved `"system"` root id is used by scheduled system agents,
        // auto-learning jobs, and the sentinel — none are operator-resumable.
        assert!(is_system_session("system"));
        assert!(!is_system_session("session-abc123"));
        assert!(!is_system_session("systematic-session"));
    }

    #[test]
    fn actor_colors_do_not_collide_with_severity_or_tone() {
        // `row_rail_color` reserves Red/Yellow/Blue for altitude (Error/
        // Attention) and RowTone (OperatorGate/ToolCall). If an actor's color
        // matched one of these, that actor's ordinary rows would be visually
        // indistinguishable from an error/gate/tool-call row by rail color
        // alone (this happened: Sentinel used to share Yellow with
        // Attention/OperatorGate).
        let reserved = [Color::Red, Color::Yellow, Color::Blue];
        for actor in [
            ActorKind::Operator,
            ActorKind::Planner,
            ActorKind::Specialist,
            ActorKind::Sentinel,
            ActorKind::Curator,
            ActorKind::Auditor,
            ActorKind::Tool,
            ActorKind::ExternalSurface,
            ActorKind::Runtime,
            ActorKind::Other,
        ] {
            let color = actor_color(actor);
            assert!(
                !reserved.contains(&color),
                "{actor:?} uses {color:?}, which collides with a severity/tone-reserved color"
            );
        }
    }

    #[test]
    fn main_list_page_step_accounts_for_chrome() {
        assert_eq!(main_list_page_step(24, false), 20);
        assert_eq!(main_list_page_step(24, true), 13);
        assert_eq!(main_list_page_step(1, false), 1);
    }

    #[test]
    fn attention_strip_line_shows_pending_count_and_summaries() {
        let rows = vec![
            ApprovalRow {
                id: "a1".into(),
                kind: "APPROVAL",
                is_pending: true,
                summary: "fetch foo.com".into(),
            },
            ApprovalRow {
                id: "i1".into(),
                kind: "ASK",
                is_pending: true,
                summary: "which provider?".into(),
            },
        ];
        let line = build_attention_strip_line(&rows, 120);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("⚠ 2 pending"), "{text}");
        assert!(text.contains("APPROVAL fetch foo.com"), "{text}");
        assert!(text.contains("ASK which provider?"), "{text}");
    }

    #[test]
    fn attention_strip_line_falls_back_to_resolved_count() {
        let rows = vec![ApprovalRow {
            id: "a1".into(),
            kind: "APPROVAL",
            is_pending: false,
            summary: "done".into(),
        }];
        let line = build_attention_strip_line(&rows, 120);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("✓ 1 resolved"), "{text}");
    }

    #[test]
    fn attention_detail_line_uses_selected_gate() {
        let rows = vec![
            ApprovalRow {
                id: "a1".into(),
                kind: "APPROVAL",
                is_pending: true,
                summary: "fetch foo.com".into(),
            },
            ApprovalRow {
                id: "i1".into(),
                kind: "ASK",
                is_pending: true,
                summary: "which provider?".into(),
            },
        ];
        let gate = GateRef { kind: GateKind::Interaction, id: "i1".into() };
        let line = build_attention_detail_line(Some(&gate), &rows, 120);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("ASK"), "{text}");
        assert!(text.contains("which provider?"), "{text}");
        assert!(text.contains("Enter/i/r answer"), "{text}");
        assert!(text.contains("[pending]"), "{text}");
    }

    #[test]
    fn attention_detail_line_hints_when_empty() {
        let line = build_attention_detail_line(None, &[], 120);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Press A for approvals list"), "{text}");
    }

    #[test]
    fn viewport_offset_pins_to_bottom_when_cursor_near_end() {
        // 7 single-line rows, 5 visible. With edge-scrolling the viewport only
        // moves when the cursor crosses the viewport edge, so the cursor stays
        // inside the window while ↑/↓ move line-by-line.
        let h = vec![1usize; 7];
        assert_eq!(compute_viewport_offset(0, 5, &h, None), 0);
        assert_eq!(compute_viewport_offset(1, 5, &h, None), 1);
        assert_eq!(compute_viewport_offset(2, 5, &h, None), 2);
        assert_eq!(compute_viewport_offset(3, 5, &h, None), 2);
        assert_eq!(compute_viewport_offset(4, 5, &h, None), 2);
        assert_eq!(compute_viewport_offset(5, 5, &h, None), 2);
        assert_eq!(compute_viewport_offset(6, 5, &h, None), 2);
    }

    #[test]
    fn viewport_offset_returns_zero_when_list_fits() {
        assert_eq!(compute_viewport_offset(0, 5, &[], None), 0);
        assert_eq!(compute_viewport_offset(0, 5, &[1, 1, 1, 1, 1], None), 0);
        assert_eq!(compute_viewport_offset(2, 5, &[1, 1, 1], None), 0);
    }

    #[test]
    fn viewport_offset_keeps_multiline_last_row_visible() {
        // 7 rows, last two are 2 lines tall, viewport 5 lines tall.
        // Bottom window: row 4 (1) + row 5 (2) + row 6 (2) = 5 → fits.
        // Last row (6) stays visible as the cursor moves inside the bottom
        // window. Moving above it scrolls up one row at a time.
        let h = vec![1usize, 1, 1, 1, 1, 2, 2];
        assert_eq!(compute_viewport_offset(4, 5, &h, None), 4);
        assert_eq!(compute_viewport_offset(5, 5, &h, None), 4);
        assert_eq!(compute_viewport_offset(6, 5, &h, None), 4);
        // selected=3 leaves the bottom window; viewport scrolls up by one row
        // so row 3 is at the top instead of snapping a whole page.
        assert_eq!(compute_viewport_offset(3, 5, &h, None), 3);
    }

    #[test]
    fn viewport_offset_stable_when_cursor_within_prev_viewport() {
        // 10 single-line rows, 5 visible. Starting from prev_offset=2,
        // the cursor should be able to move freely within [2, 7) without
        // changing the viewport. Only edge-crossings should scroll.
        let h = vec![1usize; 10];
        let prev = Some(2);
        // Cursor within: [2, 7)
        assert_eq!(compute_viewport_offset(2, 5, &h, prev), 2);
        assert_eq!(compute_viewport_offset(3, 5, &h, prev), 2);
        assert_eq!(compute_viewport_offset(4, 5, &h, prev), 2);
        assert_eq!(compute_viewport_offset(5, 5, &h, prev), 2);
        assert_eq!(compute_viewport_offset(6, 5, &h, prev), 2);
        // Cursor above: edge-scroll up
        assert_eq!(compute_viewport_offset(1, 5, &h, prev), 1);
        assert_eq!(compute_viewport_offset(0, 5, &h, prev), 0);
        // Cursor below: edge-scroll down
        assert_eq!(compute_viewport_offset(7, 5, &h, prev), 3);
        assert_eq!(compute_viewport_offset(8, 5, &h, prev), 4);
        // Cursor to bottom: re-pin with edge-scroll
        assert_eq!(compute_viewport_offset(9, 5, &h, prev), 5);
    }

    #[test]
    fn viewport_offset_follow_mode_empty_timeline_does_not_underflow() {
        // Fresh session: row_count == 0 → follow uses saturating_sub(1) == 0.
        assert_eq!(compute_viewport_offset(0, 10, &[], None), 0);
    }

    #[test]
    fn viewport_offset_anchors_to_bottom_in_follow_mode_with_multiline() {
        // 5 rows, last one is 3 lines tall, viewport 4 tall.
        // bottom window: row 2 (1) + row 3 (1) + row 4 (3) = 5 > 4, can't fit.
        // shrink: row 3 (1) + row 4 (3) = 4 → fits, offset=3.
        let h = vec![1usize, 1, 1, 1, 3];
        assert_eq!(compute_viewport_offset(4, 4, &h, None), 3);
    }

    #[test]
    fn detail_page_step_accounts_for_chrome() {
        assert_eq!(detail_page_step(24), 20);
        assert_eq!(detail_page_step(3), 1);
    }

    fn gate_entry(event_type: &str) -> SessionTimelineEntry {
        use autonoetic_types::principal::Principal;
        use autonoetic_types::session_timeline::{SessionRole, TimelineRefs};
        SessionTimelineEntry {
            event_id: "ev".into(),
            root_session_id: "r".into(),
            source_session_id: "r".into(),
            turn_id: None,
            principal: Principal::agent("planner.default"),
            role: SessionRole::Planner,
            event_type: event_type.into(),
            altitude: Altitude::Attention,
            occurred_at: "t".into(),
            payload: None,
            refs: TimelineRefs {
                approval_request_id: Some("apr-1".into()),
                interaction_id: Some("int-1".into()),
                ..Default::default()
            },
        }
    }

    #[test]
    fn selectable_gate_classifies_and_respects_resolved() {
        let single = (
            RenderedRow::Line(render::RowSpec {
                altitude: Altitude::Attention,
                actor: render::ActorKind::Planner,
                tone: RowTone::Default,
                actor_label: "planner".into(),
                headline: "x".into(),
                detail: None,
                turn_id: None,
                source_session_id: None,
                turn_index: None,
                turn_label: None,
                in_flight: false,
                show_reasoning: true,
            }),
            RowSource::Single(0),
        );
        let empty = HashSet::new();

        // approval.pending → resolvable Approval gate.
        let appr = vec![gate_entry("approval.pending")];
        let g = selectable_gate(&appr, Some(&single), &empty, &empty).unwrap();
        assert!(g.kind == GateKind::Approval && g.id == "apr-1");

        // plan.pending → resolvable Plan gate.
        let mut plan_evt = gate_entry("plan.pending");
        plan_evt.refs.plan_id = Some("plan-549".into());
        let plan = vec![plan_evt];
        let g = selectable_gate(&plan, Some(&single), &empty, &empty).unwrap();
        assert!(g.kind == GateKind::Plan && g.id == "plan-549");

        // user.ask.pending → resolvable Interaction gate.
        let ask = vec![gate_entry("user.ask.pending")];
        let g = selectable_gate(&ask, Some(&single), &empty, &empty).unwrap();
        assert!(g.kind == GateKind::Interaction && g.id == "int-1");

        // A resolved approval (its request id seen on the timeline) is not offered.
        let mut resolved = HashSet::new();
        resolved.insert("apr-1".to_string());
        assert!(selectable_gate(&appr, Some(&single), &resolved, &empty).is_none());

        // A locally-acted gate is not offered.
        let mut acted = HashSet::new();
        acted.insert("int-1".to_string());
        assert!(selectable_gate(&ask, Some(&single), &empty, &acted).is_none());

        // A collapsed run is never a single resolvable gate.
        let run = (
            RenderedRow::Collapsed { count: 2, summary: "x".into(), in_flight: false },
            RowSource::Run { start: 0, len: 2 },
        );
        assert!(selectable_gate(&appr, Some(&run), &empty, &empty).is_none());

        // A non-gate event is not resolvable.
        let other = vec![gate_entry("tool.completed")];
        assert!(selectable_gate(&other, Some(&single), &empty, &empty).is_none());
    }

    #[test]
    fn plan_id_for_row_source_reads_resolved_plan_pending_row() {
        let mut v1 = gate_entry("plan.pending");
        v1.refs.plan_id = Some("plan-1".into());
        v1.payload = Some(r#"{"plan_id":"plan-1","version":1}"#.into());
        let visible = vec![v1];
        assert_eq!(
            plan_id_for_row_source(&visible, RowSource::Single(0)).as_deref(),
            Some("plan-1")
        );
    }

    #[test]
    fn plan_amend_reopens_gate_after_prior_version_approved() {
        let mut v1_pending = gate_entry("plan.pending");
        v1_pending.refs.plan_id = Some("plan-1".into());
        v1_pending.payload =
            Some(r#"{"plan_id":"plan-1","version":1}"#.into());

        let mut v1_approved = gate_entry("plan.approved");
        v1_approved.refs.plan_id = Some("plan-1".into());
        v1_approved.payload =
            Some(r#"{"plan_id":"plan-1","version":1,"approved_by":"operator"}"#.into());

        let mut v2_pending = gate_entry("plan.pending");
        v2_pending.refs.plan_id = Some("plan-1".into());
        v2_pending.payload =
            Some(r#"{"plan_id":"plan-1","version":2,"requires_regate":true}"#.into());

        let entries = vec![v1_pending, v1_approved, v2_pending];
        let mut resolved = HashSet::new();
        resolved.insert("plan-1:v1".to_string());

        let gate = find_active_gate(&entries, &resolved, &HashSet::new());
        assert_eq!(gate.as_ref().map(|g| g.id.as_str()), Some("plan-1"));
        assert_eq!(gate.map(|g| g.kind), Some(GateKind::Plan));
    }

    #[test]
    fn plan_v1_pending_hidden_after_same_version_approved() {
        let mut v1_pending = gate_entry("plan.pending");
        v1_pending.refs.plan_id = Some("plan-1".into());
        v1_pending.payload = Some(r#"{"plan_id":"plan-1","version":1}"#.into());

        let entries = vec![v1_pending];
        let resolved = HashSet::from(["plan-1:v1".to_string()]);
        assert!(find_active_gate(&entries, &resolved, &HashSet::new()).is_none());
    }

    #[test]
    fn plan_v1_pending_hidden_when_plan_approval_cancelled() {
        let mut v1_pending = gate_entry("plan.pending");
        v1_pending.refs.plan_id = Some("plan-2c568f2ca6c3".into());
        v1_pending.payload =
            Some(r#"{"plan_id":"plan-2c568f2ca6c3","version":1}"#.into());

        let mut v1_cancelled = gate_entry("approval.cancelled");
        v1_cancelled.refs.approval_request_id =
            Some("apr-plan-plan-2c568f2ca6c3-v1".into());

        let entries = vec![v1_pending, v1_cancelled];
        let mut resolved = HashSet::new();
        record_timeline_resolution(&mut resolved, &entries[1]);
        assert!(find_active_gate(&entries, &resolved, &HashSet::new()).is_none());
    }

    #[test]
    fn plan_v1_pending_hidden_after_superseded_by_amend() {
        let mut v1_pending = gate_entry("plan.pending");
        v1_pending.refs.plan_id = Some("plan-1".into());
        v1_pending.payload = Some(r#"{"plan_id":"plan-1","version":1}"#.into());

        let mut v1_withdrawn = gate_entry("plan.withdrawn");
        v1_withdrawn.refs.plan_id = Some("plan-1".into());
        v1_withdrawn.payload =
            Some(r#"{"plan_id":"plan-1","version":1,"superseded_by":2}"#.into());

        let mut v2_pending = gate_entry("plan.pending");
        v2_pending.refs.plan_id = Some("plan-1".into());
        v2_pending.payload = Some(r#"{"plan_id":"plan-1","version":2}"#.into());

        let entries = vec![v1_pending, v1_withdrawn, v2_pending];
        let mut resolved = HashSet::new();
        record_timeline_resolution(&mut resolved, &entries[1]);
        let gate = find_active_gate(&entries, &resolved, &HashSet::new()).unwrap();
        assert_eq!(gate.id, "plan-1");
        assert_eq!(gate.kind, GateKind::Plan);
        assert_eq!(count_active_gates(&entries, &resolved, &HashSet::new()), 1);
    }

    #[test]
    fn plan_pending_without_version_defaults_to_v1() {
        let mut pending = gate_entry("plan.pending");
        pending.refs.plan_id = Some("plan-549".into());
        let entries = vec![pending];
        let gate = find_active_gate(&entries, &HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!(gate.id, "plan-549");
    }

    #[test]
    fn agent_message_v2_proposal_visible_after_v1_approved() {
        let mut msg = gate_entry("agent.message");
        msg.payload = Some(
            serde_json::json!({
                "message": {
                    "status": "awaiting_approval",
                    "plan_id": "plan-1",
                    "result": { "plan_version": 2 }
                }
            })
            .to_string(),
        );
        let entries = vec![msg];
        let mut resolved = HashSet::from(["plan-1:v1".to_string()]);
        let gate = find_active_gate(&entries, &resolved, &HashSet::new()).unwrap();
        assert_eq!(gate.id, "plan-1");
        assert_eq!(gate.kind, GateKind::Plan);
    }

    #[test]
    fn mark_plan_version_resolved_targets_newest_pending_revision() {
        let mut v1_pending = gate_entry("plan.pending");
        v1_pending.refs.plan_id = Some("plan-1".into());
        v1_pending.payload = Some(r#"{"plan_id":"plan-1","version":1}"#.into());
        let mut v2_pending = gate_entry("plan.pending");
        v2_pending.refs.plan_id = Some("plan-1".into());
        v2_pending.payload = Some(r#"{"plan_id":"plan-1","version":2}"#.into());
        let entries = vec![v1_pending, v2_pending];

        let mut resolved = HashSet::from(["plan-1:v1".to_string()]);
        let mut acted = HashSet::new();
        mark_plan_version_resolved(&entries, "plan-1", &mut resolved, &mut acted);

        assert!(resolved.contains("plan-1:v2"));
        assert!(acted.contains("plan-1:v2"));
        assert!(find_active_gate(&entries, &resolved, &acted).is_none());
    }

    #[test]
    fn selectable_gate_resolves_wiki_proposal_from_approval_pending() {
        let single = (
            RenderedRow::Line(render::RowSpec {
                altitude: Altitude::Attention,
                actor: render::ActorKind::Planner,
                tone: RowTone::Default,
                actor_label: "planner".into(),
                headline: "x".into(),
                detail: None,
                turn_id: None,
                source_session_id: None,
                turn_index: None,
                turn_label: None,
                in_flight: false,
                show_reasoning: true,
            }),
            RowSource::Single(0),
        );
        let empty = HashSet::new();
        let mut wiki = gate_entry("approval.pending");
        // approval_request_id from refs is "apr-1" (set by gate_entry helper);
        // the action field in payload distinguishes wiki proposals.
        wiki.payload = Some(
            serde_json::json!({ "action": "wiki_propose", "page_id": "my-page", "title": "My Page" }).to_string(),
        );
        let entries = vec![wiki];
        let g = selectable_gate(&entries, Some(&single), &empty, &empty).unwrap();
        assert_eq!(g.kind, GateKind::WikiProposal);
        assert_eq!(g.id, "apr-1");

        // A regular approval.pending (no wiki_propose action) stays Approval.
        let mut reg = gate_entry("approval.pending");
        reg.payload = Some(
            serde_json::json!({ "action": "sandbox_exec" }).to_string(),
        );
        let entries2 = vec![reg];
        let g2 = selectable_gate(&entries2, Some(&single), &empty, &empty).unwrap();
        assert_eq!(g2.kind, GateKind::Approval);
        assert_eq!(g2.id, "apr-1");
    }

    #[test]
    fn escalation_pending_gate_uses_approval_ref() {
        use autonoetic_types::session_timeline::TimelineRefs;
        let single = (
            RenderedRow::Line(render::RowSpec {
                altitude: Altitude::Attention,
                actor: render::ActorKind::Planner,
                tone: RowTone::Default,
                actor_label: "planner".into(),
                headline: "x".into(),
                detail: None,
                turn_id: None,
                source_session_id: None,
                turn_index: None,
                turn_label: None,
                in_flight: false,
                show_reasoning: true,
            }),
            RowSource::Single(0),
        );
        let empty = HashSet::new();
        let mut esc = gate_entry("escalation.pending");
        esc.refs = TimelineRefs {
            approval_request_id: Some("apr-esc-test123".into()),
            ..Default::default()
        };
        let entries = vec![esc];
        let g = selectable_gate(&entries, Some(&single), &empty, &empty).unwrap();
        assert_eq!(g.kind, GateKind::Escalation);
        assert_eq!(g.id, "apr-esc-test123");

        let mut resolved = HashSet::new();
        resolved.insert("apr-esc-test123".to_string());
        assert!(selectable_gate(&entries, Some(&single), &resolved, &empty).is_none());
    }

    #[test]
    fn find_active_gate_finds_newest_pending_ask_without_row_selection() {
        let ask = gate_entry("user.ask.pending");
        let mut entries = vec![
            ask,
            gate_entry("tool.completed"),
            gate_entry("agent.message"),
        ];
        // Simulate refs_json missing on older rows — id still in payload.
        entries[0].refs.interaction_id = None;
        entries[0].payload = Some(
            serde_json::json!({ "interaction_id": "int-1", "question": "Pick one?" }).to_string(),
        );
        let empty = HashSet::new();
        let later_row = (
            RenderedRow::Line(render::RowSpec {
                altitude: Altitude::Normal,
                actor: render::ActorKind::Planner,
                tone: RowTone::Default,
                actor_label: "planner".into(),
                headline: "tool done".into(),
                detail: None,
                turn_id: None,
                source_session_id: None,
                turn_index: None,
                turn_label: None,
                in_flight: false,
                show_reasoning: true,
            }),
            RowSource::Single(2),
        );
        assert!(selectable_gate(&entries, Some(&later_row), &empty, &empty).is_none());
        let g = find_active_gate(&entries, &empty, &empty).unwrap();
        assert_eq!(g.kind, GateKind::Interaction);
        assert_eq!(g.id, "int-1");
        let active = active_gate(&entries, &entries, Some(&later_row), &empty, &empty).unwrap();
        assert_eq!(active.id, "int-1");
    }

    #[test]
    fn live_content_drafts_remain_visible_when_plan_versions_folded() {
        // Drafts are session-scoped, not plan-scoped. Folding an older plan
        // version must not hide drafts or artifacts that appear later.
        let nodes = vec![
            LiveContentNode::Plan {
                plan_id: "plan-1".into(),
                title: "Roadmap".into(),
                status: "approved".into(),
                version: 2,
                is_latest: true,
            },
            LiveContentNode::PlanStep { title: "step A".into() },
            LiveContentNode::Plan {
                plan_id: "plan-1".into(),
                title: "Roadmap".into(),
                status: "approved".into(),
                version: 1,
                is_latest: false,
            },
            LiveContentNode::PlanStep { title: "old step".into() },
            LiveContentNode::Artifact {
                artifact_id: "art-1".into(),
                artifact_ref: "art-1".into(),
                kind: "patch".into(),
                name: "changes.patch".into(),
            },
            LiveContentNode::Draft {
                name: "notes.md".into(),
                alias: "".into(),
                visibility: "session".into(),
            },
        ];
        let mut pane = LiveContentPane {
            nodes,
            sections: vec![(0, "Plans"), (5, "Artifacts"), (6, "Drafts")],
            selected: 0,
            scroll: 0,
            folded: std::collections::HashMap::from([("plan-1".into(), true)]),
            artifact_folded: std::collections::HashMap::new(),
        };

        let visible = pane.visible_indices();
        assert!(visible.contains(&0), "latest plan must be visible");
        assert!(visible.contains(&1), "latest plan steps must be visible");
        assert!(!visible.contains(&2), "older plan version must be folded");
        assert!(!visible.contains(&3), "older plan steps must be folded");
        assert!(visible.contains(&4), "artifact must remain visible");
        assert!(visible.contains(&5), "draft must remain visible");
    }

    #[test]
    fn live_content_plan_label_includes_version_even_without_title() {
        let node = LiveContentNode::Plan {
            plan_id: "plan-1".into(),
            title: "".into(),
            status: "pending".into(),
            version: 3,
            is_latest: false,
        };
        let label = node.label();
        assert!(label.contains("plan v3"), "{label}");
        assert!(label.contains("v3"), "{label}");
        assert!(label.contains("[pending]"), "{label}");
    }

    #[test]
    fn newest_blocking_gate_event_prefers_latest_approval_over_older_ask() {
        let mut approval = gate_entry("approval.pending");
        approval.event_id = "ev-appr".into();
        approval.refs.approval_request_id = Some("apr-new".into());
        let mut ask = gate_entry("user.ask.pending");
        ask.event_id = "ev-ask".into();
        let entries = vec![ask, gate_entry("tool.completed"), approval];
        let empty = HashSet::new();
        let (gate, event_id) = newest_blocking_gate_event(&entries, &empty, &empty).unwrap();
        assert_eq!(gate.kind, GateKind::Approval);
        assert_eq!(gate.id, "apr-new");
        assert_eq!(event_id, "ev-appr");
        // Plans are critical gates and now use the same blocking modal as other
        // operator approvals, so gate_modal_kind must include Plan.
        assert!(gate_modal_kind(&GateRef {
            kind: GateKind::Plan,
            id: "plan-1".into(),
        }));
    }

    #[test]
    fn approval_approve_params_sends_phrase_as_reason_for_section_o() {
        let gi = GateInput {
            action: GateAction::Approve,
            id: "apr-1".into(),
            buffer: "promote weather-lookup rev_sha256:abc".into(),
            options: Vec::new(),
            allow_freeform: true,
            details_mode: false,
            motivation_required: false,
            required_confirm_phrase: Some("promote weather-lookup rev_sha256:abc".into()),
            acknowledged_capabilities: vec!["NetworkAccess".into()],
            secret_fields: Vec::new(),
            secret_values: Vec::new(),
            secret_phase: false,
        };
        let params = approval_approve_params(&gi);
        assert_eq!(
            params.get("confirm_phrase").and_then(|v| v.as_str()),
            Some("promote weather-lookup rev_sha256:abc")
        );
        assert_eq!(
            params.get("reason").and_then(|v| v.as_str()),
            Some("promote weather-lookup rev_sha256:abc")
        );
        assert_eq!(
            params.get("acknowledged_capabilities")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(1)
        );
    }

    #[test]
    fn gate_commit_validation_requires_confirm_phrase_for_promotion() {
        let gi = GateInput {
            action: GateAction::Approve,
            id: "apr-promote".into(),
            buffer: "agreed".into(),
            options: Vec::new(),
            allow_freeform: true,
            details_mode: false,
            motivation_required: false,
            required_confirm_phrase: Some("promote weather-lookup rev_sha256:abc".into()),
            acknowledged_capabilities: vec!["NetworkAccess".into()],
            secret_fields: Vec::new(),
            secret_values: Vec::new(),
            secret_phase: false,
        };
        assert!(gate_commit_validation_error(&gi).is_some());
        let mut ok = gi;
        ok.buffer = "promote weather-lookup rev_sha256:abc".into();
        assert!(gate_commit_validation_error(&ok).is_none());
    }

    #[test]
    fn gate_commit_validation_requires_motivation_for_reject() {
        let gi = GateInput {
            action: GateAction::Reject,
            id: "apr-1".into(),
            buffer: String::new(),
            options: Vec::new(),
            allow_freeform: true,
            details_mode: false,
            motivation_required: true,
            required_confirm_phrase: None,
            acknowledged_capabilities: Vec::new(),
            secret_fields: Vec::new(),
            secret_values: Vec::new(),
            secret_phase: false,
        };
        assert!(gate_commit_validation_error(&gi).is_some());
        let mut ok = gi;
        ok.buffer = "out of scope".into();
        assert!(gate_commit_validation_error(&ok).is_none());
    }

    #[test]
    fn empty_interaction_answer_is_rejected_before_any_rpc() {
        // An empty answer is rejected locally (the gateway requires non-empty),
        // so the caller never marks the gate acted on a doomed submission.
        let gi = GateInput {
            action: GateAction::Answer,
            id: "int-1".into(),
            buffer: "   ".into(), // whitespace-only ⇒ empty after trim
            options: Vec::new(),
            allow_freeform: true,
            details_mode: false,
            motivation_required: false,
            required_confirm_phrase: None,
            acknowledged_capabilities: Vec::new(),
            secret_fields: Vec::new(),
            secret_values: Vec::new(),
            secret_phase: false,
        };
        let err = answer_params(&gi, None).unwrap_err();
        assert!(err.contains("empty"), "expected empty-answer rejection, got: {err}");
    }

    #[test]
    fn answer_params_handles_numbers_options_and_freeform() {
        let opts = || vec![
            GateOption { id: "o1".into(), label: "Yes".into() },
            GateOption { id: "o2".into(), label: "No".into() },
        ];
        let mk = |buffer: &str, options: Vec<GateOption>, allow_freeform: bool| GateInput {
            action: GateAction::Answer,
            id: "int-1".into(),
            buffer: buffer.into(),
            options,
            allow_freeform,
            details_mode: false,
            motivation_required: false,
            required_confirm_phrase: None,
            acknowledged_capabilities: Vec::new(),
            secret_fields: Vec::new(),
            secret_values: Vec::new(),
            secret_phase: false,
        };

        // A typed number selects the matching option — even when free-text is
        // disallowed (the "type 2 ⏎" / >9-options flow the reviewer flagged).
        let p = answer_params(&mk("2", opts(), false), None).unwrap();
        assert_eq!(p["answer_option_id"], "o2");
        assert!(p.get("answer_text").is_none());

        // An out-of-range number is treated as free-text (when allowed).
        let p = answer_params(&mk("99", opts(), true), None).unwrap();
        assert_eq!(p["answer_text"], "99");

        // A hotkey-chosen option wins regardless of buffer.
        let chosen = GateOption { id: "o1".into(), label: "Yes".into() };
        let p = answer_params(&mk("", opts(), false), Some(&chosen)).unwrap();
        assert_eq!(p["answer_option_id"], "o1");

        // Free-text where only options are allowed ⇒ rejected locally.
        assert!(answer_params(&mk("maybe later", opts(), false), None)
            .unwrap_err()
            .contains("numbered option"));

        // Details mode allows free-text even when the original payload was
        // choice-only, because the operator explicitly asked to elaborate.
        let mut details = mk("need context", opts(), false);
        details.details_mode = true;
        let p = answer_params(&details, None).unwrap();
        assert_eq!(p["answer_text"], "need context");

        // Empty with options ⇒ guidance to type a number; empty without ⇒ "empty".
        assert!(answer_params(&mk("", opts(), true), None).unwrap_err().contains("number"));
        assert!(answer_params(&mk("", Vec::new(), true), None).unwrap_err().contains("empty"));

        // Plain free-text with no options ⇒ answer_text.
        let p = answer_params(&mk("ship it", Vec::new(), true), None).unwrap();
        assert_eq!(p["answer_text"], "ship it");
    }

    #[test]
    fn click_opens_detail_on_double_click_same_row() {
        let t0 = Instant::now();
        let mut last = None;
        assert!(!click_opens_detail(&mut last, t0, 3, 10, 5));
        assert!(click_opens_detail(
            &mut last,
            t0 + Duration::from_millis(200),
            3,
            10,
            5
        ));
        assert!(last.is_none());
    }

    #[test]
    fn click_opens_detail_ignores_slow_second_click() {
        let t0 = Instant::now();
        let mut last = None;
        assert!(!click_opens_detail(&mut last, t0, 1, 4, 2));
        assert!(!click_opens_detail(
            &mut last,
            t0 + Duration::from_millis(DOUBLE_CLICK_MS as u64 + 50),
            1,
            4,
            2
        ));
        assert!(last.is_some());
    }

    #[test]
    fn compose_input_supports_multiline_cursor_navigation() {
        let mut c = ComposeInput::new();
        c.insert_str("line one\nline two");
        c.cursor_pos = 0;
        c.cursor_down();
        assert_eq!(c.line_col(), (1, 0));
        c.cursor_right();
        c.cursor_right();
        c.cursor_right();
        assert_eq!(c.line_col(), (1, 3));
        c.cursor_up();
        assert_eq!(c.line_col(), (0, 3));
        c.end();
        assert_eq!(c.cursor_pos, 8, "End moves to the current line end");
        c.cursor_down();
        c.end();
        assert_eq!(c.cursor_pos, c.buffer.len());
        c.home();
        assert_eq!(c.cursor_pos, 9, "Home moves to the current line start");

        let mut d = ComposeInput::new();
        d.insert_str("ab\ncd");
        d.cursor_pos = 2;
        d.delete_after();
        assert_eq!(d.buffer, "abcd");
    }

    #[test]
    fn word_wrap_text_splits_long_prose_without_ellipsis() {
        let text = "Hello! I'm your planner agent. I can help you research topics, build agents, execute code.";
        let wrapped = word_wrap_text(text, 40);
        assert!(wrapped.len() > 1, "expected multiple wrapped lines: {wrapped:?}");
        let joined = wrapped.join(" ");
        assert!(joined.contains("planner agent"));
        assert!(!joined.contains('…'));
    }

    #[test]
    fn detail_wrap_line_count_splits_long_plain_lines() {
        let long = "Problem: The echo.py script reads input from the SDK's load_invocation().input, but artifact_exec's args parameter passes shell arguments (sys.argv), not SDK invocation context.";
        let raw = vec![
            "payload:".to_string(),
            format!("      {long}"),
        ];
        let lines = render_detail_lines(&raw);
        let narrow = detail_wrap_line_count(&lines, 40);
        let wide = detail_wrap_line_count(&lines, 200);
        assert!(
            narrow > wide,
            "long prose should wrap across more visual lines at narrow width (narrow={narrow}, wide={wide})"
        );
        assert_eq!(wide, 2, "payload header + one wrapped line at wide width");
    }

    #[test]
    fn interaction_choices_reads_options_from_payload() {
        let mut e = gate_entry("user.ask.pending");
        e.payload = Some(
            serde_json::json!({
                "interaction_id": "int-1",
                "options": [{"id": "o1", "label": "Yes"}, {"id": "o2", "label": "No"}],
                "allow_freeform": false
            })
            .to_string(),
        );
        let (opts, freeform) = interaction_choices(std::slice::from_ref(&e), "int-1");
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].id, "o1");
        assert_eq!(opts[1].label, "No");
        assert_eq!(opts[2].id, "__details__");
        assert!(!freeform, "original payload allow_freeform=false must be preserved");
        // Unknown interaction ⇒ permissive default, no options.
        let (none, ff) = interaction_choices(std::slice::from_ref(&e), "other");
        assert!(none.is_empty() && ff);
    }

    #[test]
    fn floor_cycles_through_all_levels() {
        let mut a = Altitude::Detail;
        let seq: Vec<Altitude> = (0..4)
            .map(|_| {
                a = cycle_floor(a);
                a
            })
            .collect();
        assert_eq!(
            seq,
            vec![Altitude::Normal, Altitude::Attention, Altitude::Error, Altitude::Detail]
        );
    }

    fn spec_with_turn(turn: Option<&str>, headline: &str) -> (RenderedRow, RowSource) {
        (
            RenderedRow::Line(render::RowSpec {
                altitude: Altitude::Normal,
                actor: render::ActorKind::Planner,
                tone: RowTone::Default,
                actor_label: "planner".into(),
                headline: headline.into(),
                detail: None,
                turn_id: turn.map(str::to_string),
                source_session_id: None,
                turn_index: None,
                turn_label: None,
                in_flight: false,
                show_reasoning: true,
            }),
            RowSource::Single(0),
        )
    }

    fn spec_with_child_session(
        turn: Option<&str>,
        source_session_id: &str,
        headline: &str,
    ) -> (RenderedRow, RowSource) {
        (
            RenderedRow::Line(render::RowSpec {
                altitude: Altitude::Normal,
                actor: render::ActorKind::Specialist,
                tone: RowTone::Default,
                actor_label: "coder".into(),
                headline: headline.into(),
                detail: None,
                turn_id: turn.map(str::to_string),
                source_session_id: Some(source_session_id.to_string()),
                turn_index: None,
                turn_label: None,
                in_flight: false,
                show_reasoning: true,
            }),
            RowSource::Single(0),
        )
    }

    #[test]
    fn in_flight_marker_only_on_most_recent_row_of_open_turn() {
        // 3 rows in turn-000001, all un-closed. Only the LAST should get the
        // spinner — earlier rows keep their normal altitude glyph.
        let mut rows = vec![
            spec_with_turn(Some("turn-000001"), "first"),
            spec_with_turn(Some("turn-000001"), "second"),
            spec_with_turn(Some("turn-000001"), "third"),
        ];
        let open: HashSet<String> = ["turn-000001".into()].into_iter().collect();
        let boundaries = annotate_turns_and_in_flight(
            &mut rows,
            &[],
            &[],
            &open,
            true,
            &HashSet::new(),
            "root",
            &HashMap::new(),
        );
        // Boundary only at the start of the turn (i=0).
        assert!(boundaries.contains_key(&0));
        assert!(!boundaries.contains_key(&1));
        assert!(!boundaries.contains_key(&2));
        // In-flight only on the most recent row.
        let inflight: Vec<bool> = rows
            .iter()
            .map(|(r, _)| match r {
                RenderedRow::Line(s) => s.in_flight,
                _ => false,
            })
            .collect();
        assert_eq!(inflight, vec![false, false, true]);
        let indices: Vec<Option<u32>> = rows
            .iter()
            .map(|(r, _)| match r {
                RenderedRow::Line(s) => s.turn_index,
                _ => None,
            })
            .collect();
        assert_eq!(indices, vec![Some(1), Some(1), Some(1)]);
    }

    #[test]
    fn turn_index_parsed_from_turn_id_not_view_ordinal() {
        // Labels follow turn_counter (parsed from turn_id), not the Nth distinct
        // turn_id visible in the current view — turn 5 stays "5" even when turns
        // 3–4 have no surviving rows.
        let mut rows = vec![
            spec_with_turn(Some("turn-000001"), "t1"),
            spec_with_turn(Some("turn-000001"), "t1b"),
            spec_with_turn(Some("turn-000002"), "t2"),
            spec_with_turn(None, "no turn"),
            spec_with_turn(Some("turn-000005"), "t5"),
        ];
        annotate_turns_and_in_flight(&mut rows, &[], &[], &HashSet::new(), true, &HashSet::new(), "root", &HashMap::new());
        let indices: Vec<Option<u32>> = rows
            .iter()
            .map(|(r, _)| match r {
                RenderedRow::Line(s) => s.turn_index,
                _ => None,
            })
            .collect();
        assert_eq!(indices, vec![Some(1), Some(1), Some(2), None, Some(5)]);
    }

    #[test]
    fn child_spawn_rows_label_parent_planner_turn() {
        let child = "root-session/coder.default-abc123";
        let mut rows = vec![
            spec_with_child_session(Some("turn-000001"), child, "wrote tests"),
            spec_with_child_session(Some("turn-000002"), child, "fixed lint"),
        ];
        let lineage = HashMap::from([(
            child.to_string(),
            SessionSpawnLineageEntry {
                child_session_id: child.to_string(),
                parent_session_id: "root-session".to_string(),
                spawned_at_turn: 3,
                target_agent_id: "coder.default".to_string(),
            },
        )]);
        annotate_turns_and_in_flight(
            &mut rows,
            &[],
            &[],
            &HashSet::new(),
            true,
            &HashSet::new(),
            "root-session",
            &lineage,
        );
        let labels: Vec<Option<String>> = rows
            .iter()
            .map(|(r, _)| match r {
                RenderedRow::Line(s) => s.turn_label.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec![Some("3 → coder".into()), Some("3.2".into())]);
    }

    fn tool_entry(et: &str, turn_id: Option<&str>, tool: &str, call_id: Option<&str>) -> SessionTimelineEntry {
        use autonoetic_types::principal::Principal;
        use autonoetic_types::session_timeline::{SessionRole, TimelineRefs};
        let mut payload = serde_json::json!({ "tool_name": tool });
        if let Some(id) = call_id {
            payload["call_id"] = serde_json::json!(id);
        }
        SessionTimelineEntry {
            event_id: format!("ev-{et}-{}", call_id.unwrap_or(tool)),
            root_session_id: "r".into(),
            source_session_id: "r".into(),
            turn_id: turn_id.map(str::to_string),
            principal: Principal::agent("planner.default"),
            role: SessionRole::Planner,
            event_type: et.into(),
            altitude: Altitude::Detail,
            occurred_at: "2026-06-01T00:00:00Z".into(),
            payload: Some(payload.to_string()),
            refs: TimelineRefs::default(),
        }
    }

    #[test]
    fn pending_tool_calls_lists_requested_but_not_completed() {
        let entries = vec![
            tool_entry("tool.requested", Some("turn-000001"), "read", Some("c1")),
            tool_entry("tool.completed", Some("turn-000001"), "read", Some("c1")),
            // Async: requested but no completion yet.
            tool_entry("tool.requested", Some("turn-000002"), "workflow_wait", Some("c2")),
            tool_entry("tool.requested", Some("turn-000002"), "agent_spawn", Some("c3")),
            tool_entry("tool.completed", Some("turn-000002"), "agent_spawn", Some("c3")),
            // A later turn with no tool activity advances the clock for age.
            tool_entry("llm.round", Some("turn-000004"), "", None),
        ];
        let summary = pending_tool_summary(&entries);
        // Only workflow_wait (c2) stays open.
        assert_eq!(summary.names, vec!["workflow_wait".to_string()]);
        // Requested in turn 2, latest turn is 4 → 2 turns waiting.
        assert_eq!(summary.age_turns, Some(2));
    }

    #[test]
    fn pending_age_is_measured_from_oldest_open_call() {
        // Oldest open call is c1 (turn 1); c2 (turn 3) is newer. Age must track
        // c1, not the first-with-a-turn. Latest turn is 3 → age 2, not 0.
        let entries = vec![
            tool_entry("tool.requested", Some("turn-000001"), "workflow_wait", Some("c1")),
            tool_entry("tool.requested", Some("turn-000003"), "workflow_wait", Some("c2")),
        ];
        assert_eq!(pending_tool_summary(&entries).age_turns, Some(2));
    }

    #[test]
    fn pending_age_is_zero_when_requested_this_turn() {
        // A call opened in the latest turn is known to be 0 turns old (not unknown).
        let entries = vec![tool_entry(
            "tool.requested",
            Some("turn-000005"),
            "workflow_wait",
            Some("c1"),
        )];
        assert_eq!(pending_tool_summary(&entries).age_turns, Some(0));
    }

    #[test]
    fn pending_tool_calls_falls_back_to_name_fifo_without_call_id() {
        let entries = vec![
            tool_entry("tool.requested", Some("turn-000001"), "sandbox_exec", None),
            tool_entry("tool.requested", Some("turn-000001"), "sandbox_exec", None),
            tool_entry("tool.completed", Some("turn-000001"), "sandbox_exec", None),
        ];
        // Two requested, one completed → one still pending.
        assert_eq!(
            pending_tool_summary(&entries).names,
            vec!["sandbox_exec".to_string()]
        );
    }

    #[test]
    fn async_completion_gets_back_reference_to_request_turn() {
        // Request in turn 1, completion in turn 5 (async gap) → back-reference.
        let visible = vec![
            tool_entry("tool.requested", Some("turn-000001"), "workflow_wait", Some("c9")),
            tool_entry("tool.completed", Some("turn-000005"), "workflow_wait", Some("c9")),
        ];
        let mut rows: Vec<(RenderedRow, RowSource)> = vec![(
            RenderedRow::Line(render::render_spec(&visible[1])),
            RowSource::Single(1),
        )];
        annotate_turns_and_in_flight(
            &mut rows,
            &visible,
            &visible,
            &HashSet::new(),
            true,
            &HashSet::new(),
            "r",
            &HashMap::new(),
        );
        let RenderedRow::Line(spec) = &rows[0].0 else { panic!("expected line row") };
        assert!(
            spec.headline.contains("⟵ requested T1"),
            "headline: {}",
            spec.headline
        );
    }

    #[test]
    fn osc52_sequence_encodes_and_wraps_for_tmux() {
        use base64::Engine;
        let text = "art:sha256:abc123";
        let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());

        // Bare terminal: OSC 52 set-clipboard, BEL-terminated.
        let bare = osc52_sequence(text, false);
        assert_eq!(bare, format!("\x1b]52;c;{b64}\x07"));

        // tmux: DCS passthrough wrapper, every inner ESC doubled, ST-terminated.
        let wrapped = osc52_sequence(text, true);
        assert!(wrapped.starts_with("\x1bPtmux;"), "tmux DCS prefix");
        assert!(wrapped.ends_with("\x1b\\"), "ST terminator");
        assert!(wrapped.contains(&b64), "payload preserved");
        assert!(wrapped.contains("\x1b\x1b]52"), "inner ESC doubled");
    }

    #[test]
    fn search_matches_find_visible_text_and_hidden_payload() {
        let visible = vec![
            tool_entry("tool.completed", Some("turn-000001"), "sandbox_exec", Some("c1")),
            tool_entry("tool.completed", Some("turn-000002"), "content_write", Some("c2")),
        ];
        let indexed: Vec<(RenderedRow, RowSource)> = visible
            .iter()
            .enumerate()
            .map(|(i, e)| (RenderedRow::Line(render::render_spec(e)), RowSource::Single(i)))
            .collect();
        // Matches the tool_name carried in the payload.
        assert_eq!(
            compute_search_matches(&indexed, &visible, "content_write"),
            vec![1]
        );
        // Case-insensitive.
        assert_eq!(
            compute_search_matches(&indexed, &visible, "SANDBOX_EXEC"),
            vec![0]
        );
        // No match → empty.
        assert!(compute_search_matches(&indexed, &visible, "zzz-nope").is_empty());
    }

    #[test]
    fn pending_chip_ellipsis_reflects_distinct_names_not_raw_count() {
        assert_eq!(pending_chip(&[], None), None);
        // Three of the same tool → count 3, one name, no ellipsis.
        assert_eq!(
            pending_chip(
                &["workflow_wait".into(), "workflow_wait".into(), "workflow_wait".into()],
                None
            ),
            Some("⋯3 workflow_wait".to_string())
        );
        // Three distinct → two shown, ellipsis for the hidden third.
        assert_eq!(
            pending_chip(&["a".into(), "b".into(), "c".into()], None),
            Some("⋯3 a,b,…".to_string())
        );
        // Exactly two distinct → both shown, no ellipsis.
        assert_eq!(
            pending_chip(&["a".into(), "b".into()], None),
            Some("⋯2 a,b".to_string())
        );
    }

    #[test]
    fn pending_chip_shows_age_and_stale_marker() {
        // Fresh (age 0) → no Δ suffix.
        assert_eq!(
            pending_chip(&["workflow_wait".into()], Some(0)),
            Some("⋯1 workflow_wait".to_string())
        );
        // A few turns waiting → Δ suffix, no stale marker.
        assert_eq!(
            pending_chip(&["workflow_wait".into()], Some(3)),
            Some("⋯1 workflow_wait Δ3t".to_string())
        );
        // Beyond the stale threshold → trailing `!`.
        assert_eq!(
            pending_chip(&["workflow_wait".into()], Some(9)),
            Some("⋯1 workflow_wait Δ9t!".to_string())
        );
    }

    #[test]
    fn back_reference_uses_unfiltered_entries_when_request_below_floor() {
        // Real-world failure case: the `tool.requested` (Detail) is filtered out
        // of `visible`, but its failed `tool.completed` (bumped to Attention) is
        // shown. The map is built from `all_entries`, so the back-reference still
        // resolves. `visible` here omits the request; `all_entries` includes it.
        let all_entries = vec![
            tool_entry("tool.requested", Some("turn-000002"), "sandbox_exec", Some("cX")),
            tool_entry("tool.completed", Some("turn-000007"), "sandbox_exec", Some("cX")),
        ];
        let visible = vec![all_entries[1].clone()];
        let mut rows: Vec<(RenderedRow, RowSource)> = vec![(
            RenderedRow::Line(render::render_spec(&visible[0])),
            RowSource::Single(0),
        )];
        annotate_turns_and_in_flight(
            &mut rows,
            &visible,
            &all_entries,
            &HashSet::new(),
            true,
            &HashSet::new(),
            "r",
            &HashMap::new(),
        );
        let RenderedRow::Line(spec) = &rows[0].0 else { panic!("expected line row") };
        assert!(
            spec.headline.contains("⟵ requested T2"),
            "headline: {}",
            spec.headline
        );
    }

    #[test]
    fn sync_completion_has_no_back_reference() {
        // Same turn → no back-reference (would be noise for synchronous calls).
        let visible = vec![
            tool_entry("tool.requested", Some("turn-000003"), "read", Some("c1")),
            tool_entry("tool.completed", Some("turn-000003"), "read", Some("c1")),
        ];
        let mut rows: Vec<(RenderedRow, RowSource)> = vec![(
            RenderedRow::Line(render::render_spec(&visible[1])),
            RowSource::Single(1),
        )];
        annotate_turns_and_in_flight(
            &mut rows,
            &visible,
            &visible,
            &HashSet::new(),
            true,
            &HashSet::new(),
            "r",
            &HashMap::new(),
        );
        let RenderedRow::Line(spec) = &rows[0].0 else { panic!("expected line row") };
        assert!(!spec.headline.contains("⟵ requested"), "headline: {}", spec.headline);
    }

    #[test]
    fn turn_number_of_parses_canonical_turn_ids() {
        assert_eq!(turn_number_of("turn-000001"), Some(1));
        assert_eq!(turn_number_of("turn-000042"), Some(42));
        assert_eq!(turn_number_of("turn-"), None);
        assert_eq!(turn_number_of("not-a-turn"), None);
    }

    #[test]
    fn extra_inflight_rows_spin_gate_after_turn_ends() {
        let mut rows = vec![
            spec_with_turn(Some("A"), "tool done"),
            (
                RenderedRow::Line(render::RowSpec {
                    altitude: Altitude::Attention,
                    actor: render::ActorKind::Planner,
                    tone: render::RowTone::OperatorGate,
                    actor_label: "planner".into(),
                    headline: "📋 PLAN AWAITING APPROVAL".into(),
                    detail: None,
                    turn_id: Some("A".into()),
                    source_session_id: None,
                    turn_index: None,
                    turn_label: None,
                    in_flight: false,
                    show_reasoning: true,
                }),
                RowSource::Single(1),
            ),
        ];
        let open: HashSet<String> = HashSet::new();
        let extra: HashSet<usize> = [1].into_iter().collect();
        annotate_turns_and_in_flight(&mut rows, &[], &[], &open, true, &extra, "root", &HashMap::new());
        let inflight: Vec<bool> = rows
            .iter()
            .map(|(r, _)| match r {
                RenderedRow::Line(s) => s.in_flight,
                _ => false,
            })
            .collect();
        assert_eq!(inflight, vec![false, true]);
    }

    #[test]
    fn closed_turn_marks_no_rows_as_in_flight() {
        // Turn B has turn.start and turn.end, so it's closed — no spinner.
        let mut rows = vec![
            spec_with_turn(Some("B"), "early"),
            spec_with_turn(Some("B"), "late"),
        ];
        let open: HashSet<String> = HashSet::new();
        annotate_turns_and_in_flight(&mut rows, &[], &[], &open, true, &HashSet::new(), "root", &HashMap::new());
        let inflight: Vec<bool> = rows
            .iter()
            .map(|(r, _)| match r {
                RenderedRow::Line(s) => s.in_flight,
                _ => false,
            })
            .collect();
        assert_eq!(inflight, vec![false, false]);
    }

    #[test]
    fn show_reasoning_off_hides_thought_bubble_rows() {
        // agent.reasoning rows carry a 💭 in the headline; they get the
        // show_reasoning=false flag when the toggle is off.
        let mut rows = vec![
            spec_with_turn(Some("A"), "tool edit"),
            spec_with_turn(Some("A"), "\u{1F4AD} thinking out loud"),
        ];
        let open: HashSet<String> = ["A".into()].into_iter().collect();
        annotate_turns_and_in_flight(&mut rows, &[], &[], &open, false, &HashSet::new(), "root", &HashMap::new());
        let shown: Vec<bool> = rows
            .iter()
            .map(|(r, _)| match r {
                RenderedRow::Line(s) => s.show_reasoning,
                _ => false,
            })
            .collect();
        assert_eq!(shown, vec![true, false]);
    }

    #[test]
    fn collapsed_run_shows_spinner_when_it_contains_open_turn_event() {
        // Routine turn-internal events are collapsed. When the turn is still
        // open (turn.start seen but no turn.end yet), the collapsed row must
        // show the spinner so the operator knows the session is working.
        use autonoetic_types::principal::Principal;
        use autonoetic_types::session_timeline::{SessionRole, TimelineRefs};

        fn entry(et: &str, turn_id: Option<&str>) -> SessionTimelineEntry {
            SessionTimelineEntry {
                event_id: format!("ev-{et}"),
                root_session_id: "r".into(),
                source_session_id: "r".into(),
                turn_id: turn_id.map(str::to_string),
                principal: Principal::agent("planner.default"),
                role: SessionRole::Planner,
                event_type: et.into(),
                altitude: Altitude::Detail,
                occurred_at: "2026-06-01T00:00:00Z".into(),
                payload: None,
                refs: TimelineRefs::default(),
            }
        }

        let visible = vec![
            entry("turn.start", Some("A")),
            entry("llm.round", Some("A")),
            entry("tool.requested", Some("A")),
        ];
        let mut rows: Vec<(RenderedRow, RowSource)> = render::coalesce_indexed(&visible);
        let open: HashSet<String> = ["A".into()].into_iter().collect();
        annotate_turns_and_in_flight(&mut rows, &visible, &visible, &open, true, &HashSet::new(), "root", &HashMap::new());

        assert_eq!(rows.len(), 1);
        assert!(
            matches!(
                &rows[0].0,
                RenderedRow::Collapsed { in_flight: true, .. }
            ),
            "collapsed run containing an open-turn event should be in_flight"
        );
    }

    #[test]
    fn collapsed_run_not_spinner_for_closed_turn() {
        use autonoetic_types::principal::Principal;
        use autonoetic_types::session_timeline::{SessionRole, TimelineRefs};

        fn entry(et: &str, turn_id: Option<&str>) -> SessionTimelineEntry {
            SessionTimelineEntry {
                event_id: format!("ev-{et}"),
                root_session_id: "r".into(),
                source_session_id: "r".into(),
                turn_id: turn_id.map(str::to_string),
                principal: Principal::agent("planner.default"),
                role: SessionRole::Planner,
                event_type: et.into(),
                altitude: Altitude::Detail,
                occurred_at: "2026-06-01T00:00:00Z".into(),
                payload: None,
                refs: TimelineRefs::default(),
            }
        }

        let visible = vec![
            entry("turn.start", Some("A")),
            entry("turn.end", Some("A")),
        ];
        let mut rows: Vec<(RenderedRow, RowSource)> = render::coalesce_indexed(&visible);
        annotate_turns_and_in_flight(&mut rows, &visible, &visible, &HashSet::new(), true, &HashSet::new(), "root", &HashMap::new());

        assert!(
            matches!(
                &rows[0].0,
                RenderedRow::Collapsed { in_flight: false, .. }
            ),
            "collapsed run for a closed turn should not be in_flight"
        );
    }

    #[test]
    fn collapsed_run_line_uses_spinner_glyph_when_in_flight() {
        let line = build_collapsed_row_line(3, "turn.start×3", true, "⠋");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("⠋ "), "in-flight collapsed row should start with spinner glyph: {text}");

        let line = build_collapsed_row_line(3, "turn.start×3", false, "⠋");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.starts_with('·') || text.starts_with('⟨'),
            "closed collapsed row should not start with spinner: {text}"
        );
    }

    #[test]
    fn plan_execution_wake_message_uses_hint_when_plan_is_approved() {
        use autonoetic_types::plan_frame::{
            PlanFrame, PlanStatus, PlanStep, StepOwner, ValidationPolicy,
        };
        let plan = PlanFrame {
            plan_id: "plan-abc".into(),
            version: 1,
            parent_version: None,
            workflow_id: "wf".into(),
            root_session_id: "sess".into(),
            title: "Fix bug".into(),
            objective: "Patch SDK init".into(),
            status: PlanStatus::Approved,
            steps: vec![PlanStep {
                step_id: "s1".into(),
                title: "Implement fix".into(),
                owner: StepOwner::Agent,
                depends_on: Vec::new(),
                required_capabilities: Vec::new(),
                agent_id: Some("coder.default".into()),
                notes: None,
                status: Default::default(),
            }],
            validation_policy: ValidationPolicy::default(),
            capability_envelope: Vec::new(),
            approved_by: Some("operator".into()),
            approved_at: Some("t".into()),
            created_by_agent_id: "planner.default".into(),
            reason: None,
            created_at: "t".into(),
            expires_at: None,
        };
        let wake = plan_execution_wake_message(&plan);
        assert!(wake.contains("coder.default"));
        assert!(wake.contains("agent_spawn"));
        assert!(wake.to_lowercase().contains("do not call agent_list"));
    }

    #[test]
    fn format_plan_frame_lines_includes_steps_validations_and_review_hints() {
        use autonoetic_types::plan_frame::{
            PlanFrame, PlanStep, PlanStatus, ValidationClass, ValidationEntry,
            ValidationPolicy, ValidationRequirement,
        };
        let plan = PlanFrame {
            plan_id: "plan-abc".into(),
            version: 2,
            parent_version: Some(1),
            workflow_id: "wf".into(),
            root_session_id: "sess".into(),
            title: "Ship feature".into(),
            objective: "Deliver the widget".into(),
            status: PlanStatus::AwaitingApproval,
            steps: vec![
                PlanStep {
                    step_id: "s1".into(),
                    title: "Research APIs".into(),
                    owner: autonoetic_types::plan_frame::StepOwner::Agent,
                    depends_on: Vec::new(),
                    required_capabilities: Vec::new(),
                    agent_id: Some("researcher.default".into()),
                    notes: None,
                    status: Default::default(),
                },
                PlanStep {
                    step_id: "s2".into(),
                    title: "Implement".into(),
                    owner: autonoetic_types::plan_frame::StepOwner::Agent,
                    depends_on: Vec::new(),
                    required_capabilities: Vec::new(),
                    agent_id: None,
                    notes: Some("Keep it small".into()),
                    status: Default::default(),
                },
            ],
            validation_policy: ValidationPolicy {
                entries: vec![
                    ValidationEntry {
                        validation_id: "v1".into(),
                        title: "Tests pass".into(),
                        class: ValidationClass::CorrectnessCheck,
                        requirement: ValidationRequirement::Required,
                    },
                    ValidationEntry {
                        validation_id: "v2".into(),
                        title: "Docs updated".into(),
                        class: ValidationClass::QualityCheck,
                        requirement: ValidationRequirement::Advisory,
                    },
                ],
            },
            capability_envelope: Vec::new(),
            approved_by: None,
            approved_at: None,
            created_by_agent_id: "planner.default".into(),
            reason: Some("Tighten scope".into()),
            created_at: "t".into(),
            expires_at: None,
        };
        let list = format_plan_frame_lines(&plan, false);
        let joined = list.join("\n");
        assert!(joined.contains("plan-abc v2"));
        assert!(joined.contains("amended from v1"));
        assert!(joined.contains("Tighten scope"));
        assert!(joined.contains("Research APIs → researcher.default"));
        assert!(joined.contains("notes: Keep it small"));
        assert!(joined.contains("required validations: Tests pass"));
        assert!(joined.contains("advisory validations: Docs updated"));
        assert!(!joined.contains("plan review"));

        let review = format_plan_frame_lines(&plan, true);
        let review_text = review.join("\n");
        assert!(review_text.contains("── plan review ──"));
        assert!(review_text.contains("→ y approve · n request changes"));
        assert!(review_text.starts_with("── plan review ──"));
    }

    #[test]
    fn quit_arm_expires_after_window() {
        let mut armed = Some(Instant::now() + Duration::from_millis(50));
        assert!(quit_armed(&armed));
        std::thread::sleep(Duration::from_millis(60));
        assert!(!quit_armed(&armed));
        let mut status = Some(QUIT_ARM_STATUS.to_string());
        disarm_quit(&mut armed, &mut status);
        assert!(armed.is_none());
        assert!(status.is_none());
    }

    #[test]
    fn artifact_ref_for_entry_extracts_from_tool_completed() {
        let result_json = serde_json::json!({
            "ok": true,
            "artifact_ref": "ar.abc12345",
            "artifact_canonical_digest": "sha256:def456",
            "kind": "binary",
            "files": [{"name": "main.py", "alias": "main.py"}],
            "message": "Created new artifact"
        });
        let result_str = serde_json::to_string(&result_json).unwrap();
        let payload = serde_json::json!({
            "tool_name": "artifact_build",
            "result": result_str,
        });
        let entry = SessionTimelineEntry {
            event_id: "ev1".into(),
            root_session_id: "root".into(),
            source_session_id: "src".into(),
            turn_id: None,
            principal: Principal::agent("test"),
            role: SessionRole::Specialist { kind: "coder".into() },
            event_type: "tool.completed".into(),
            altitude: Altitude::Normal,
            occurred_at: "2026-01-01T00:00:00Z".into(),
            payload: Some(serde_json::to_string(&payload).unwrap()),
            refs: Default::default(),
        };
        assert_eq!(
            artifact_ref_for_entry(&entry),
            Some("ar.abc12345".to_string())
        );
    }

    #[test]
    fn artifact_ref_for_entry_returns_none_for_non_artifact() {
        let payload = serde_json::json!({
            "tool_name": "sandbox_exec",
            "result": "{\"ok\":true}",
        });
        let entry = SessionTimelineEntry {
            event_id: "ev1".into(),
            root_session_id: "root".into(),
            source_session_id: "src".into(),
            turn_id: None,
            principal: Principal::agent("test"),
            role: SessionRole::Specialist { kind: "coder".into() },
            event_type: "tool.completed".into(),
            altitude: Altitude::Normal,
            occurred_at: "2026-01-01T00:00:00Z".into(),
            payload: Some(serde_json::to_string(&payload).unwrap()),
            refs: Default::default(),
        };
        assert_eq!(artifact_ref_for_entry(&entry), None);
    }

    #[test]
    fn artifact_ref_for_entry_extracts_from_args_preview() {
        let payload = serde_json::json!({
            "tool_name": "artifact_build",
            "result": "{}",
            "args_preview": "ar.xyz99999",
        });
        let entry = SessionTimelineEntry {
            event_id: "ev1".into(),
            root_session_id: "root".into(),
            source_session_id: "src".into(),
            turn_id: None,
            principal: Principal::agent("test"),
            role: SessionRole::Specialist { kind: "coder".into() },
            event_type: "tool.completed".into(),
            altitude: Altitude::Normal,
            occurred_at: "2026-01-01T00:00:00Z".into(),
            payload: Some(serde_json::to_string(&payload).unwrap()),
            refs: Default::default(),
        };
        assert_eq!(
            artifact_ref_for_entry(&entry),
            Some("ar.xyz99999".to_string())
        );
    }

    #[test]
    fn diag_wrapped_table_repro() {
        use autonoetic_types::principal::Principal;
        use autonoetic_types::session_timeline::{SessionRole, TimelineRefs};
        // A clean multi-column GFM table emitted by an agent in an
        // agent.message payload. This used to be destroyed by the
        // GLUED_TABLE_ROW normalizer (which split header/delimiter/data rows
        // of any 3+ column table), causing the table to render as raw `|`
        // fragments. It must now render as an aligned table.
        let message = "Paris Weather — Next 12 Hours\n\nRight now (15:00 CEST): 26.7°C, mainly clear.\n\nHourly Forecast:\n\n| Time | Temp | Conditions | Precip | Wind |\n|------|------|------------|--------|------|\n| 15:00 | 26.7°C | Mainly clear | 0% | 13.3 km/h |\n| 16:00 | 26.4°C | Clear | 0% | 12.1 km/h |";
        let payload = serde_json::json!({ "message": message });
        let e = SessionTimelineEntry {
            event_id: "ev".into(),
            root_session_id: "r".into(),
            source_session_id: "r".into(),
            turn_id: None,
            principal: Principal::agent("planner.collaborative"),
            role: SessionRole::Planner,
            event_type: "agent.message".into(),
            altitude: Altitude::Normal,
            occurred_at: "2026-06-13T13:02:42Z".into(),
            payload: Some(payload.to_string()),
            refs: TimelineRefs::default(),
        };
        let raw = render::format_detail(&e);
        let rendered = render_detail_lines(&raw);
        let text: Vec<String> = rendered.iter().map(|l| l.to_string()).collect();
        let joined = text.join("\n");
        // A rendered GFM table produces a separator line of box-drawing `─`.
        assert!(
            text.iter().any(|t| t.contains('─')),
            "clean multi-column table should render as an aligned table with a \
             separator line; got:\n{joined}"
        );
        // The raw delimiter fragment `|------|------|` must NOT leak through.
        assert!(
            !text.iter().any(|t| t.contains("|------")),
            "raw delimiter should not appear; table should be rendered, got:\n{joined}"
        );
    }

    #[test]
    fn parse_line_hint_extracts_single_and_range() {
        assert_eq!(
            parse_line_hint("L12: secret here"),
            (Some(12), None, "secret here".to_string())
        );
        assert_eq!(
            parse_line_hint("L12-14: range"),
            (Some(12), Some(14), "range".to_string())
        );
        // Case-insensitive prefix.
        assert_eq!(
            parse_line_hint("l3: lower"),
            (Some(3), None, "lower".to_string())
        );
    }

    #[test]
    fn parse_line_hint_leaves_plain_and_malformed_bodies_intact() {
        // No prefix.
        assert_eq!(
            parse_line_hint("just a comment"),
            (None, None, "just a comment".to_string())
        );
        // Looks like a hint but isn't a number → treat whole thing as body.
        assert_eq!(
            parse_line_hint("Login: broken"),
            (None, None, "Login: broken".to_string())
        );
        // Prefix without colon → body untouched.
        assert_eq!(
            parse_line_hint("L12 no colon"),
            (None, None, "L12 no colon".to_string())
        );
        // Reversed range is a typo, not a valid hint → leave the body intact so
        // the comment still sends (the gateway would otherwise reject it).
        assert_eq!(
            parse_line_hint("L14-12: oops"),
            (None, None, "L14-12: oops".to_string())
        );
    }
}
