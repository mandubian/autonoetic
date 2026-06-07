//! Interactive Session Room shell (#363) — the ratatui renderer.
//!
//! A scrollable, live-tailing view of the canonical timeline with an altitude
//! dial, squash, drill-down, and conversational gate resolution. P3.b-2 (#392):
//! a **gateway API client** — reads via `session.timeline.list`, resolves gates
//! via `approvals.approve`/`reject` and `interaction.resolve_and_answer`. No
//! direct store access. chat.rs untouched.

use super::channel::{Channel, GateAction, GateKind, GateOption, GateRef, TuiChannel};
use super::client::RoomClient;
use super::render::{self, ActorKind, RenderedRow, RowSource, RowSpec, RowTone};
use super::slash::SlashCommand;
use autonoetic_types::session_timeline::{Altitude, SessionTimelineEntry, SessionTimelineListResult};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;
use std::collections::{HashMap, HashSet};
use std::io;
use std::time::Duration;

/// Spinner frames — a gentle breathing indicator on the in-flight row. The
/// current frame is rotated on each TUI tick (the existing 250 ms poll loop).
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Hard cap on plumbing/tool rows — keeps the list scannable.
const MAX_ROW_LINES: usize = 8;
/// Agent/operator narrative rows may wrap across more lines before folding.
const MAX_NARRATIVE_ROW_LINES: usize = 24;

/// Expanded footer height while composing a multi-line message.
const COMPOSE_PANEL_HEIGHT: u16 = 7;

/// Rows visible in the main timeline list for the current terminal height.
fn main_list_page_step(terminal_height: u16, compose_open: bool) -> usize {
    let chrome = 2u16 + if compose_open { COMPOSE_PANEL_HEIGHT } else { 0 };
    terminal_height.saturating_sub(chrome).max(1) as usize
}

/// Compute the scroll offset for the timeline list so the selected row stays
/// visible AND the last row of the list does not scroll off the bottom of the
/// viewport when the cursor moves up from the end.
///
/// Without this helper, calling `ListState::select` on every frame resets
/// `state.offset` to 0, and the ratatui List widget then re-derives the
/// viewport from offset 0 — which pushes the last row off the bottom the moment
/// the cursor moves up, leaving a blank row at the bottom of the list.
fn compute_viewport_offset(selected: usize, list_height: usize, row_count: usize) -> usize {
    if list_height == 0 || row_count <= list_height {
        return 0;
    }
    let max_offset = row_count - list_height;
    // Pin to the bottom: when the cursor is in the bottom window, keep the
    // last row at the bottom of the viewport so the operator never sees it
    // vanish as they scroll the cursor up.
    if selected >= max_offset {
        return max_offset;
    }
    // Otherwise, follow the cursor (centered with a half-window padding).
    selected.saturating_sub(list_height / 2)
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

    fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    fn insert_str(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\r' {
                continue;
            }
            self.insert_char(c);
        }
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
        // Detect start of a multi-line string value (indented content after a key)
        // that looks like markdown. The render_payload_lines formatter outputs
        // string values split on \n with 6-space indent.
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        if in_markdown_block {
            // End of multi-line string: a line with less indent or a comma/closing brace
            if indent <= 4 && (trimmed.starts_with('}') || trimmed.starts_with(',') || trimmed.is_empty()) {
                // Flush markdown buffer
                let md_text = md_buf.join("\n");
                out.extend(markdown::render_markdown(
                    &markdown::normalize_prose_sections(&md_text),
                ));
                in_markdown_block = false;
                md_buf.clear();
                out.push(Line::from(line.clone()));
            } else {
                md_buf.push(trimmed.to_string());
            }
            continue;
        }

        // Detect a key followed by split multi-line content (from render_payload_lines)
        // Pattern: `    "key":` followed by indented lines
        // Or detect markdown in a single string value
        if indent == 6
            && !trimmed.is_empty()
            && !trimmed.starts_with('"')
            && !trimmed.starts_with('{')
            && !trimmed.starts_with('[')
        {
            if markdown::looks_like_markdown(trimmed) {
                in_markdown_block = true;
                md_buf.push(trimmed.to_string());
                continue;
            }
        }

        out.push(Line::from(line.clone()));
    }

    if in_markdown_block && !md_buf.is_empty() {
        let md_text = md_buf.join("\n");
        out.extend(markdown::render_markdown(
            &markdown::normalize_prose_sections(&md_text),
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
struct SessionStats {
    total_input: u64,
    total_output: u64,
    llm_calls: u64,
    models: Vec<String>,
}

fn compute_session_stats(entries: &[SessionTimelineEntry]) -> SessionStats {
    let mut stats = SessionStats {
        total_input: 0,
        total_output: 0,
        llm_calls: 0,
        models: Vec::new(),
    };
    for e in entries {
        if e.event_type != "llm.round" {
            continue;
        }
        if let Some(p) = e.payload.as_deref() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(p) {
                let inp = v.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                let out = v.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                if inp > 0 || out > 0 {
                    stats.total_input += inp;
                    stats.total_output += out;
                    stats.llm_calls += 1;
                    if let Some(model) = v.get("model").and_then(|m| m.as_str()) {
                        let short = model.split('/').last().unwrap_or(model);
                        if !stats.models.contains(&short.to_string()) {
                            stats.models.push(short.to_string());
                        }
                    }
                }
            }
        }
    }
    stats
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
    let options = payload
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
    (options, allow_freeform)
}

/// Restores the terminal on drop, even on early return / panic-unwind.
struct TerminalRestore;
impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
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
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    // Guard constructed before entering the alternate screen, so raw mode is
    // restored even if `EnterAlternateScreen` (or anything after) fails.
    let _restore = TerminalRestore;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mut entries: Vec<SessionTimelineEntry> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut floor = initial_floor;
    let mut squash = true;
    let mut follow = true; // pin to newest
    let mut selected: usize = 0;
    let mut detail: Option<Vec<String>> = None; // drill-down pane content
    let mut detail_scroll: u16 = 0; // vertical scroll offset for detail pane
    let mut detail_h_scroll: u16 = 0; // horizontal scroll offset for detail pane
    let mut input: Option<GateInput> = None; // in-flight gate decision
    let mut compose: Option<ComposeInput> = None; // in-flight free-form message to the session
    let mut clipboard =
        std::panic::catch_unwind(|| arboard::Clipboard::new().ok()).unwrap_or(None);
    let mut slash: Option<String> = None; // in-flight slash-command buffer (no leading `/`)
    let mut session_pick_list: Option<Vec<String>> = None; // ids from /session list for number-pick
    let mut status: Option<String> = None; // last action / connection result
    // Gates no longer offerable: approvals resolved on the timeline, plus
    // anything the operator just acted on (covers interactions, which have no
    // timeline resolution event yet).
    let mut resolved: HashSet<String> = HashSet::new();
    let mut acted: HashSet<String> = HashSet::new();
    // Display toggles + spinner state for the in-flight row indicator.
    let mut show_reasoning = true;
    let mut spinner_frame: usize = 0;

    loop {
        // Fetch at most one page per tick via the gateway API. On error (gateway
        // down), surface it and keep retrying — don't crash the UI.
        match rpc(
            client,
            "session.timeline.list",
            serde_json::json!({
                "root_session_id": &*root_session_id,
                "after_event_id": cursor,
                "limit": limit,
                // Always fetch at `detail` and filter for display below. Approval
                // resolution events are `Normal` altitude, so fetching at the
                // display floor (e.g. Attention) would drop them and leave
                // `resolved` unpopulated — making already-decided gates look
                // re-decidable. Fetch everything; filter is purely a view concern.
                "min_altitude": "detail",
            }),
        ) {
            Ok(value) => match serde_json::from_value::<SessionTimelineListResult>(value) {
                Ok(page) => {
                    if let Some(last) = page.entries.last() {
                        cursor = Some(last.event_id.clone());
                    }
                    for e in &page.entries {
                        if matches!(
                            e.event_type.as_str(),
                            "approval.approved" | "approval.rejected" | "approval.cancelled"
                        ) {
                            if let Some(id) = &e.refs.approval_request_id {
                                resolved.insert(id.clone());
                            }
                        }
                    }
                    entries.extend(page.entries);
                    if status.as_deref().map(|s| s.starts_with("✗ gateway")).unwrap_or(false) {
                        status = None; // recovered
                    }
                }
                Err(e) => status = Some(format!("✗ bad timeline response: {e}")),
            },
            Err(e) => status = Some(format!("✗ gateway: {e}")),
        }

        // `entries` holds everything (fetched at `detail`); the display floor is
        // applied here as a pure view filter. RowSource indices below therefore
        // index into `visible`, so gate selection and drill-down use it too.
        let visible: Vec<SessionTimelineEntry> =
            entries.iter().filter(|e| e.altitude >= floor).cloned().collect();
        // Detect in-flight turns: any turn_id we've seen `turn.start` for but
        // not yet `turn.end` is still open. The TUI marks the most recent row
        // in such a turn with a spinner.
        let mut open_turns: HashSet<String> = HashSet::new();
        for e in &entries {
            match e.event_type.as_str() {
                "turn.start" => {
                    if let Some(t) = &e.turn_id {
                        open_turns.insert(t.clone());
                    }
                }
                "turn.end" => {
                    if let Some(t) = &e.turn_id {
                        open_turns.remove(t);
                    }
                }
                _ => {}
            }
        }
        // Rows + their source mapping (lets Enter drill into the underlying event).
        let mut indexed: Vec<(RenderedRow, RowSource)> = if squash {
            render::coalesce_indexed(&visible)
        } else {
            visible
                .iter()
                .enumerate()
                .map(|(i, e)| (RenderedRow::Line(render::render_spec(e)), RowSource::Single(i)))
                .collect()
        };
        // Annotate each row with turn membership + the in-flight bit, and
        // track the previous turn_id so the renderer can draw a faint
        // divider when the turn changes. The in-flight spinner is reserved
        // for the **most recent** row in an open turn — earlier rows of the
        // same turn stay in their normal altitude glyph so the operator can
        // read the chain.
        let turn_boundaries = annotate_turns_and_in_flight(
            &mut indexed,
            &open_turns,
            show_reasoning,
        );
        let rows: Vec<RenderedRow> = indexed.iter().map(|(r, _)| r.clone()).collect();

        if follow {
            selected = rows.len().saturating_sub(1);
        } else {
            selected = selected.min(rows.len().saturating_sub(1));
        }

        let gate = active_gate(&entries, &visible, indexed.get(selected), &resolved, &acted);

        spinner_frame = (spinner_frame + 1) % SPINNER_FRAMES.len();
        let spinner_glyph = SPINNER_FRAMES[spinner_frame];
        let session_stats = compute_session_stats(&entries);

        terminal.draw(|f| {
            draw(
                f,
                root_session_id,
                floor,
                squash,
                follow,
                &rows,
                selected,
                detail.as_deref(),
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
            )
        })?;

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Paste(text) if compose.is_some() => {
                    if let Some(c) = compose.as_mut() {
                        c.insert_str(&text);
                    }
                }
                Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Compose mode: multi-line editor with cursor + clipboard (#405).
                if let Some(c) = compose.as_mut() {
                    match handle_compose_key(c, &key, &mut clipboard) {
                        ComposeKeyResult::Continue => {}
                        ComposeKeyResult::Cancel => compose = None,
                        ComposeKeyResult::Send(text) => {
                            status = Some(send_message(
                                client,
                                root_session_id,
                                &text,
                                target_agent_id.as_deref(),
                            ));
                            compose = None;
                            follow = true;
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
                                SlashCommand::Quit => break,
                                SlashCommand::Help => {
                                    detail = Some(super::slash::help_lines());
                                    detail_scroll = 0;
                                    detail_h_scroll = 0;
                                    session_pick_list = None;
                                    status = Some("help: Esc to close".to_string());
                                }
                                SlashCommand::Test { name } => {
                                    if name.is_empty() || name == "help" {
                                        detail = Some(
                                            super::test_scenarios::scenario_help()
                                                .lines()
                                                .map(String::from)
                                                .collect(),
                                        );
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
                                        detail = Some(lines);
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
                                        );
                                        status = Some(format!("→ switched to session {new_id}"));
                                    }
                                }
                                SlashCommand::ListSessions { agent } => {
                                    let (lines, ids) = list_sessions_detail(client, agent.as_deref());
                                    detail = Some(lines);
                                    session_pick_list = Some(ids);
                                }
                                SlashCommand::ListCronJobs => {
                                    detail = Some(list_cron_detail(client, root_session_id));
                                    session_pick_list = None;
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
                                            );
                                            status = Some(format!("→ resumed session {resolved_id}"));
                                        }
                                    } else {
                                        status = Some(
                                            "✗ /session resume: no sessions found".to_string(),
                                        );
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

                // Text-capture mode takes over all input while open.
                if let Some(gi) = input.as_mut() {
                    // Instant single-digit pick — only when it's unambiguous: a
                    // pure-choice question (no free-text) with ≤9 options and an
                    // empty buffer. Otherwise digits go into the buffer so multi-
                    // digit ordinals (>9 options) and free-text starting with a
                    // digit still work; Enter then resolves the ordinal.
                    let chosen = if gi.action == GateAction::Answer
                        && gi.buffer.is_empty()
                        && !gi.allow_freeform
                        && gi.options.len() <= 9
                    {
                        if let KeyCode::Char(c @ '1'..='9') = key.code {
                            gi.options.get((c as usize) - ('1' as usize)).cloned()
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    // Number selection and Enter both commit; share the resolve path.
                    let commit = chosen.is_some() || key.code == KeyCode::Enter;
                    if commit {
                        let gi = input.take().unwrap();
                        if gi.id.starts_with("test-") {
                            acted.insert(gi.id.clone());
                            let verb = match gi.action {
                                GateAction::Approve => "approved",
                                GateAction::Reject => "rejected",
                                GateAction::Answer => "answered",
                            };
                            let answer_text = chosen
                                .as_ref()
                                .map(|o| o.label.as_str())
                                .or_else(|| {
                                    let b = gi.buffer.trim();
                                    (!b.is_empty()).then_some(b)
                                });
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
                            match resolve_gate(client, &gi, chosen.as_ref()) {
                                Ok(msg) => {
                                    acted.insert(gi.id.clone());
                                    status = Some(msg);
                                }
                                Err(msg) => {
                                    status = Some(msg);
                                    input = Some(gi);
                                }
                            }
                        }
                        continue;
                    }
                    match key.code {
                        KeyCode::Esc => input = None,
                        KeyCode::Backspace => {
                            gi.buffer.pop();
                        }
                        KeyCode::Char(c) => gi.buffer.push(c),
                        _ => {}
                    }
                    continue;
                }

                let ctrl_c = key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('q') => break,
                    _ if ctrl_c => break,
                    KeyCode::Esc => {
                        if detail.is_some() {
                            detail = None;
                            detail_scroll = 0;
                            detail_h_scroll = 0;
                            session_pick_list = None;
                        } else {
                            break;
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
                                    );
                                    status = Some(format!("→ switched to session {picked_id}"));
                                } else {
                                    status = Some(format!("→ already viewing {picked_id}"));
                                }
                                detail = None;
                                session_pick_list = None;
                            }
                        }
                    }
                    // y/n: approve/reject the selected pending approval.
                    KeyCode::Char('y') | KeyCode::Char('n') => {
                        if let Some(g) = gate.as_ref().filter(|g| g.kind == GateKind::Approval) {
                            detail = None;
                            input = Some(GateInput {
                                action: if key.code == KeyCode::Char('y') {
                                    GateAction::Approve
                                } else {
                                    GateAction::Reject
                                },
                                id: g.id.clone(),
                                buffer: String::new(),
                                options: Vec::new(),
                                allow_freeform: true,
                            });
                            status = None;
                        }
                    }
                    // r: reply to the selected pending interaction (user.ask) —
                    // load its pre-digested choices so a number key picks one.
                    KeyCode::Char('r') => {
                        if let Some(g) = gate.as_ref().filter(|g| g.kind == GateKind::Interaction) {
                            detail = None;
                            let (options, allow_freeform) = interaction_choices(&entries, &g.id);
                            input = Some(GateInput {
                                action: GateAction::Answer,
                                id: g.id.clone(),
                                buffer: String::new(),
                                options,
                                allow_freeform,
                            });
                            status = None;
                        }
                    }
                    KeyCode::Enter => {
                        if detail.is_some() {
                            detail = None;
                            detail_scroll = 0;
                            detail_h_scroll = 0;
                        } else if let Some(g) =
                            gate.as_ref().filter(|g| g.kind == GateKind::Interaction)
                        {
                            let (options, allow_freeform) = interaction_choices(&entries, &g.id);
                            input = Some(GateInput {
                                action: GateAction::Answer,
                                id: g.id.clone(),
                                buffer: String::new(),
                                options,
                                allow_freeform,
                            });
                            status = None;
                        } else {
                            detail = indexed.get(selected).map(|(_, src)| detail_for(&visible, *src));
                            detail_scroll = 0;
                            detail_h_scroll = 0;
                        }
                    }
                    KeyCode::Char('a') => {
                        // Pure view change now (we always fetch at `detail`) — no
                        // reload, so already-fetched history re-filters instantly.
                        floor = cycle_floor(floor);
                        detail = None;
                    }
                    KeyCode::Char('s') => squash = !squash,
                    // R: toggle the 💭 reasoning prefix on/off everywhere. Off
                    // hides the prefix; the reasoning row itself stays visible
                    // (it's a Detail-altitude event, so it's normally hidden
                    // by the floor or by squash — but the toggle matters for
                    // any channel that doesn't filter on altitude).
                    KeyCode::Char('R') => show_reasoning = !show_reasoning,
                    // i: compose a free-form message into the session (#405).
                    KeyCode::Char('i') => {
                        if let Some(g) = gate.as_ref().filter(|g| g.kind == GateKind::Interaction) {
                            let (options, allow_freeform) = interaction_choices(&entries, &g.id);
                            detail = None;
                            input = Some(GateInput {
                                action: GateAction::Answer,
                                id: g.id.clone(),
                                buffer: String::new(),
                                options,
                                allow_freeform,
                            });
                            status = None;
                        } else {
                            detail = None;
                            compose = Some(ComposeInput::new());
                            status = None;
                        }
                    }
                    // /: slash-command mode (vim/Discord convention). `:`
                    // and `?` are accepted aliases for muscle memory.
                    KeyCode::Char('/') | KeyCode::Char(':') | KeyCode::Char('?') => {
                        detail = None;
                        slash = Some(String::new());
                        status = None;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if detail.is_some() {
                            detail_scroll = detail_scroll.saturating_add(1);
                        } else {
                            follow = false;
                            selected = (selected + 1).min(rows.len().saturating_sub(1));
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if detail.is_some() {
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
                                selected = (selected + step).min(rows.len().saturating_sub(1));
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
                _ => {}
            }
        }
    }
    Ok(())
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
    open_turns: &HashSet<String>,
    show_reasoning: bool,
) -> HashMap<usize, bool> {
    let mut last_turn: Option<String> = None;
    let mut last_row_for_turn: HashMap<String, usize> = HashMap::new();
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
    for (i, (row, _)) in rows.iter_mut().enumerate() {
        if let RenderedRow::Line(spec) = row {
            if let Some(t) = &spec.turn_id {
                if last_row_for_turn.get(t).copied() == Some(i) {
                    spec.in_flight = true;
                }
            }
            if !show_reasoning && spec.headline.contains('\u{1F4AD}') {
                spec.show_reasoning = false;
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
            (!resolved.contains(&id) && !acted.contains(&id)).then_some(GateRef {
                kind: GateKind::Approval,
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
    if !gi.allow_freeform {
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
fn resolve_gate(client: &RoomClient, gi: &GateInput, chosen: Option<&GateOption>) -> Result<String, String> {
    let text = gi.buffer.trim();
    let reason = (!text.is_empty()).then(|| text.to_string());
    let (result, verb) = match gi.action {
        GateAction::Approve => (
            rpc(
                client,
                "approvals.approve",
                serde_json::json!({ "request_id": gi.id, "decided_by": "operator", "reason": reason }),
            ),
            "approved",
        ),
        GateAction::Reject => (
            rpc(
                client,
                "approvals.reject",
                serde_json::json!({ "request_id": gi.id, "decided_by": "operator", "reason": reason }),
            ),
            "rejected",
        ),
        GateAction::Answer => (
            rpc(client, "interaction.resolve_and_answer", answer_params(gi, chosen)?),
            "answered",
        ),
    };
    match result {
        Ok(_) => Ok(format!("✓ {verb} {}", gi.id)),
        Err(e) => Err(format!("✗ {e}")),
    }
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
            if let Some(turn_lines) = render::turn_summary(e, entries) {
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
            let mut lines = vec![
                format!("collapsed run — {len} routine events"),
                "(press 's' to unsquash and inspect individually)".to_string(),
                String::new(),
            ];
            for e in entries.iter().skip(start).take(len) {
                lines.push(format!("  · {}", render::render_line(e)));
            }
            lines
        }
    }
}

/// Render a single rich `RowSpec` as a styled multi-line `ListItem`. Capped at
/// `MAX_ROW_LINES` physical lines; longer content gets a `…` ellipsis on the
/// last line. The actor rail on the left uses the actor's color, giving an
/// at-a-glance map of "who said what."
#[allow(clippy::too_many_arguments)]
fn render_rich_row(
    spec: &RowSpec,
    row_index: usize,
    turn_boundaries: &HashMap<usize, bool>,
    content_w: usize,
    glyph_w: usize,
    rail_w: usize,
    label_w: usize,
    spinner_glyph: &'static str,
    show_reasoning: bool,
) -> ListItem<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if turn_boundaries.contains_key(&row_index) {
        let bar = "─".repeat(content_w + glyph_w + label_w + rail_w + 2);
        lines.push(Line::from(Span::styled(
            bar,
            Style::default().fg(Color::DarkGray),
        )));
    }
    let rail_style = Style::default().fg(row_rail_color(spec));
    let rail_block = "▌".repeat(rail_w);
    let glyph = if spec.in_flight {
        spinner_glyph
    } else {
        render::altitude_glyph(spec.altitude)
    };
    let label_text = format!(
        "[{}]",
        truncate(&spec.actor_label, label_w.saturating_sub(2))
    );
    let label_padded = format!("{label_text:>label_w$}");
    let head_style = row_headline_style(spec);
    let label_style = row_label_style(spec);
    let detail_style = row_detail_style(spec);
    let cont_pad = " ".repeat(rail_w + glyph_w + label_w + 1);
    let first_prefix = vec![
        Span::styled(rail_block.clone(), rail_style),
        Span::raw(" "),
        Span::styled(format!("{glyph:<2}"), head_style),
        Span::styled(label_padded.clone(), label_style),
        Span::raw(" "),
    ];

    if spec.tone == RowTone::AgentNarrative {
        let mut wrote = false;
        if !spec.headline.is_empty() {
            push_wrapped_narrative(
                &mut lines,
                &spec.headline,
                content_w,
                &cont_pad,
                first_prefix.clone(),
                head_style,
            );
            wrote = true;
        }
        if let Some(body) = spec.detail.as_deref().filter(|s| !s.is_empty()) {
            if wrote {
                push_agent_message_detail(
                    &mut lines,
                    body,
                    content_w,
                    &cont_pad,
                    detail_style,
                    head_style,
                );
            } else {
                push_wrapped_narrative(
                    &mut lines,
                    body,
                    content_w,
                    &cont_pad,
                    first_prefix,
                    head_style,
                );
            }
        } else if !wrote {
            push_wrapped_narrative(
                &mut lines,
                "",
                content_w,
                &cont_pad,
                first_prefix,
                head_style,
            );
        }
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
                    let prefix = if i == 0 { "  ↳ " } else { "    " };
                    let avail = content_w.saturating_sub(prefix.chars().count());
                    for (j, chunk) in word_wrap_text(sub.trim_end(), avail).into_iter().enumerate() {
                        let line_prefix = if i == 0 && j == 0 { prefix } else { "    " };
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
    ListItem::new(Text::from(out))
}

fn is_divider_line(line: &Line) -> bool {
    line.spans.len() == 1
        && line
            .spans
            .first()
            .map(|s| s.content.starts_with('─'))
            .unwrap_or(false)
}

fn render_collapsed_row(count: usize, summary: &str) -> ListItem<'static> {
    let style = Style::default().fg(Color::DarkGray);
    let text = format!(
        "{} ⟨{} {}⟩",
        render::altitude_glyph(Altitude::Detail),
        count,
        summary
    );
    ListItem::new(Line::from(Span::styled(text, style)))
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
    let normalized = markdown::normalize_prose_sections(body);
    for md_line in markdown::render_markdown(&normalized) {
        let text = line_display_text(&md_line);
        if text.trim().is_empty() {
            lines.push(Line::from(Span::raw(cont_pad.to_string())));
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

/// Render structured agent-message detail: optional `[status]` subline, then prose/markdown body.
fn push_agent_message_detail(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    content_w: usize,
    cont_pad: &str,
    detail_style: Style,
    body_style: Style,
) {
    if let Some((subline, body)) = text.split_once("\n\n") {
        if !subline.trim().is_empty() {
            push_wrapped_detail_lines(lines, subline, content_w, cont_pad, detail_style);
        }
        if !body.trim().is_empty() {
            if super::markdown::looks_like_markdown(body) {
                push_wrapped_markdown_body(lines, body, content_w, cont_pad, None, body_style);
            } else {
                push_wrapped_narrative(
                    lines,
                    body,
                    content_w,
                    cont_pad,
                    vec![Span::raw(cont_pad.to_string())],
                    body_style,
                );
            }
        }
        return;
    }
    if super::markdown::looks_like_markdown(text) {
        push_wrapped_markdown_body(lines, text, content_w, cont_pad, None, body_style);
    } else {
        push_wrapped_detail_lines(lines, text, content_w, cont_pad, detail_style);
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

/// Render agent/operator narrative: preserve paragraph breaks, wrap each block.
fn push_wrapped_narrative(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    content_w: usize,
    cont_pad: &str,
    first_prefix: Vec<Span<'static>>,
    body_style: Style,
) {
    if super::markdown::looks_like_markdown(text) {
        push_wrapped_markdown_body(
            lines,
            text,
            content_w,
            cont_pad,
            Some(first_prefix),
            body_style,
        );
        return;
    }
    let mut first = true;
    for paragraph in text.split('\n') {
        let chunks = if paragraph.is_empty() {
            vec![String::new()]
        } else {
            word_wrap_text(paragraph, content_w)
        };
        for chunk in chunks {
            if first {
                let mut spans = first_prefix.clone();
                spans.push(Span::styled(chunk, body_style));
                lines.push(Line::from(spans));
                first = false;
            } else {
                lines.push(Line::from(vec![
                    Span::raw(cont_pad.to_string()),
                    Span::styled(chunk, body_style),
                ]));
            }
        }
    }
    if first {
        let mut spans = first_prefix;
        spans.push(Span::styled(String::new(), body_style));
        lines.push(Line::from(spans));
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
    match spec.tone {
        RowTone::ToolCall => Color::Blue,
        RowTone::Reasoning => Color::DarkGray,
        RowTone::AgentNarrative | RowTone::Default => actor_color(spec.actor),
    }
}

/// Headline emphasis: agent messages pop; tool calls stay subdued.
fn row_headline_style(spec: &RowSpec) -> Style {
    let alt = altitude_style(spec.altitude);
    match spec.tone {
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
        RowTone::ToolCall => Style::default().fg(Color::Blue).add_modifier(Modifier::DIM),
        RowTone::Reasoning => Style::default().fg(Color::DarkGray),
        RowTone::AgentNarrative | RowTone::Default => {
            Style::default().fg(actor_color(spec.actor))
        }
    }
}

fn row_detail_style(spec: &RowSpec) -> Style {
    match spec.tone {
        RowTone::ToolCall => Style::default().fg(Color::Indexed(67)),
        RowTone::AgentNarrative => Style::default().fg(Color::Gray),
        _ => Style::default().fg(Color::DarkGray),
    }
}

/// Map an `ActorKind` to a stable color. The TUI's left rail uses this so the
/// operator can scan the room and tell *who* is speaking at a glance.
fn actor_color(actor: ActorKind) -> Color {
    match actor {
        ActorKind::Operator => Color::Cyan,
        ActorKind::Planner => Color::Green,
        ActorKind::Specialist => Color::LightGreen,
        ActorKind::Sentinel => Color::Yellow,
        ActorKind::Curator => Color::Magenta,
        ActorKind::Auditor => Color::LightMagenta,
        ActorKind::Tool => Color::DarkGray,
        ActorKind::ExternalSurface => Color::Blue,
        ActorKind::Runtime => Color::Red,
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
    detail: &mut Option<Vec<String>>,
    follow: &mut bool,
    resolved: &mut HashSet<String>,
    acted: &mut HashSet<String>,
    floor: &mut Altitude,
    root_session_id: &mut String,
    _target_agent_id: &mut Option<String>,
    _limit: u32,
    new_id: &str,
) {
    *root_session_id = new_id.to_string();
    entries.clear();
    *cursor = None;
    *selected = 0;
    *detail = None;
    *follow = true;
    resolved.clear();
    acted.clear();
    // Don't reset `floor` — the operator's altitude dial is a view preference,
    // not a session property. Keep the previous setting.
    let _ = floor;
}

/// Fetch the most recent session id, optionally filtered by agent. Returns
/// `None` when the gateway has no matching session.
fn resolve_latest_session(client: &RoomClient, agent: Option<&str>) -> Option<String> {
    let params = serde_json::json!({
        "agent_id": agent,
        "limit": 1,
    });
    let value = rpc(client, "session.list", params).ok()?;
    let parsed: serde_json::Result<autonoetic_types::session_timeline::SessionListResult> =
        serde_json::from_value(value);
    parsed
        .ok()?
        .sessions
        .into_iter()
        .next()
        .map(|e| e.root_session_id)
}

/// Build a multi-line session list for `/session` and `/session list [agent]`,
/// returned as (display lines, pickable session ids). Rows are numbered [1]-[9]
/// so the operator can switch by pressing a single digit while the detail pane
/// is open.
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

fn list_sessions_detail(client: &RoomClient, agent: Option<&str>) -> (Vec<String>, Vec<String>) {
    let params = serde_json::json!({
        "agent_id": agent,
        "limit": 9,
    });
    match rpc(client, "session.list", params) {
        Ok(value) => match serde_json::from_value::<
            autonoetic_types::session_timeline::SessionListResult,
        >(value)
        {
            Ok(parsed) if parsed.sessions.is_empty() => {
                let hint = agent
                    .map(|a| format!(" for agent '{a}'"))
                    .unwrap_or_default();
                (
                    vec![format!("(no sessions{hint}) — /session <id> or start one with `autonoetic run`")],
                    Vec::new(),
                )
            }
            Ok(parsed) => {
                let mut lines = if let Some(a) = agent {
                    vec![format!("sessions for agent '{a}':")]
                } else {
                    vec!["recent sessions:".to_string()]
                };
                let ids: Vec<String> = parsed
                    .sessions
                    .iter()
                    .map(|s| s.root_session_id.clone())
                    .collect();
                for (i, s) in parsed.sessions.iter().enumerate() {
                    lines.push(format!(
                        "  [{}] {} [{}] @ {}",
                        i + 1,
                        s.root_session_id,
                        s.agent_id,
                        s.last_active_at
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
    rows: &[RenderedRow],
    selected: usize,
    detail: Option<&[String]>,
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
) {
    let compose_open = compose.is_some() && detail.is_none();
    let chunks = if compose_open {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(COMPOSE_PANEL_HEIGHT),
            Constraint::Length(1),
        ])
        .split(f.area())
    } else {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area())
    };
    let footer_idx = if compose_open { 3 } else { 2 };
    let list_idx = 1usize;

    let stats_tag = if stats.llm_calls > 0 {
        let models_tag = if stats.models.len() == 1 {
            stats.models[0].clone()
        } else {
            format!("{} models", stats.models.len())
        };
        format!(
            "   in:{} out:{} calls:{} [{}]",
            format_tokens(stats.total_input),
            format_tokens(stats.total_output),
            stats.llm_calls,
            models_tag,
        )
    } else {
        String::new()
    };
    let header = format!(
        " Session Room [{}] — {root}   floor: {}   squash: {}   reasoning: {}   {} rows{stats_tag}{}{}",
        TuiChannel.kind(),
        floor.as_str(),
        if squash { "on" } else { "off" },
        if show_reasoning { "on" } else { "off" },
        rows.len(),
        if follow { "   (following)" } else { "" },
        status.map(|s| format!("   {s}")).unwrap_or_default(),
    );
    f.render_widget(
        Paragraph::new(header).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    if let Some(lines) = detail {
        let inner_width = chunks[list_idx].width.saturating_sub(2);
        let inner_height = chunks[list_idx].height.saturating_sub(2) as usize;
        let text = render_detail_lines(lines);
        let total_lines = detail_wrap_line_count(&text, inner_width);
        let max_scroll = total_lines.saturating_sub(inner_height) as u16;
        let scroll = detail_scroll.min(max_scroll);
        let h = detail_h_scroll;
        f.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title(" event detail "))
                .wrap(Wrap { trim: false })
                .scroll((scroll, h)),
            chunks[list_idx],
        );
        let scroll_hint = if max_scroll > 0 || h > 0 {
            format!(" · j/k ↓↑ ({}/{}) · PgUp/PgDn · h/l ←→ ({})", scroll, max_scroll, h)
        } else {
            String::new()
        };
        f.render_widget(
            Paragraph::new(format!(" Esc/Enter close · q quit{scroll_hint}")).style(Style::default().fg(Color::DarkGray)),
            chunks[footer_idx],
        );
        return;
    }

    // The terminal width caps each line. Reserve 2 cells for the actor rail
    // and 3 cells for the altitude glyph + space, leaving the rest for the
    // label + headline + detail.
    let width = chunks[list_idx].width as usize;
    let rail_w = 2usize;
    let glyph_w = 3usize;
    let label_w = 12usize.min(width / 4);
    let content_w = width.saturating_sub(rail_w + glyph_w + label_w + 2);

    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| match row {
            RenderedRow::Line(spec) => render_rich_row(
                spec,
                i,
                turn_boundaries,
                content_w,
                glyph_w,
                rail_w,
                label_w,
                spinner_glyph,
                show_reasoning,
            ),
            RenderedRow::Collapsed { count, summary } => {
                render_collapsed_row(*count, summary)
            }
        })
        .collect();

    let mut state = ListState::default();
    if !rows.is_empty() {
        // Use `selected_mut` + manual `offset_mut` instead of `state.select`:
        // `state.select` resets `offset` to 0 every call, which causes the
        // List widget to re-derive the viewport from offset 0 and pushes the
        // last row off the bottom when the cursor moves up.
        let safe_selected = selected.min(rows.len() - 1);
        *state.selected_mut() = Some(safe_selected);
        *state.offset_mut() =
            compute_viewport_offset(safe_selected, chunks[list_idx].height as usize, rows.len());
    }
    f.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::NONE))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        chunks[list_idx],
        &mut state,
    );

    if let Some(c) = compose {
        draw_compose_input(f, c, chunks[2]);
    }

    let footer = if let Some(buf) = slash {
        Paragraph::new(format!(
            " : /{buf}▏   [Enter run · Esc cancel]   {HELP}",
            HELP = super::slash::HELP_TEXT
        ))
        .style(Style::default().fg(Color::Magenta))
    } else if compose.is_some() {
        Paragraph::new(
            " Enter send · Shift+Enter newline · ←→↑↓ edit · Ctrl+V paste · Ctrl+C copy · Esc cancel",
        )
        .style(Style::default().fg(Color::Green))
    } else if let Some(gi) = input {
        let label = match gi.action {
            GateAction::Approve => "APPROVE — motivation (optional)",
            GateAction::Reject => "REJECT — motivation (optional)",
            GateAction::Answer => "ANSWER",
        };
        // For an interaction with pre-digested choices, list them so a number
        // key picks one (the §3.5 one-tap path); free-text stays available when
        // the question allows it.
        let choices = gi
            .options
            .iter()
            .enumerate()
            // Flatten/truncate labels so a multi-line or long label can't break
            // the one-line footer.
            .map(|(i, o)| format!("[{}] {}", i + 1, render::one_line(&o.label, 24)))
            .collect::<Vec<_>>()
            .join(" · ");
        let hint = if gi.options.is_empty() {
            "[Enter submit · Esc cancel]".to_string()
        } else if gi.allow_freeform {
            format!("{choices}   [number choose · or type a reply · Esc cancel]")
        } else {
            format!("{choices}   [number choose · Esc cancel]")
        };
        Paragraph::new(format!(" {label}: {}▏   {hint}", gi.buffer))
            .style(Style::default().fg(Color::Cyan))
    } else {
        // The gate affordance hint is the channel's concern (#393) — route it
        // through the channel so a Discord/WhatsApp bridge can render its own.
        let gate_hint = gate.map(|g| TuiChannel.gate_prompt(g)).unwrap_or_default();
        let follow_indicator = if follow { " ● following" } else { " ○ paused" };
        Paragraph::new(format!(
            " q quit · j/k scroll · PgUp/PgDn page · f/Space follow{follow_indicator} · g/G top/bottom · a altitude · s squash · R reasoning · i message · / cmd · ⏎ detail{gate_hint}"
        ))
        .style(Style::default().fg(Color::DarkGray))
    };
    f.render_widget(footer, chunks[footer_idx]);
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
    fn main_list_page_step_accounts_for_chrome() {
        assert_eq!(main_list_page_step(24, false), 22);
        assert_eq!(main_list_page_step(24, true), 15);
        assert_eq!(main_list_page_step(1, false), 1);
    }

    #[test]
    fn viewport_offset_pins_to_bottom_when_cursor_near_end() {
        // 7 rows, 5 visible — once the cursor is in the bottom window the
        // last row must stay at the bottom of the viewport.
        assert_eq!(compute_viewport_offset(0, 5, 7), 0);
        assert_eq!(compute_viewport_offset(1, 5, 7), 0);
        assert_eq!(compute_viewport_offset(2, 5, 7), 2);
        assert_eq!(compute_viewport_offset(3, 5, 7), 2);
        assert_eq!(compute_viewport_offset(4, 5, 7), 2);
        assert_eq!(compute_viewport_offset(5, 5, 7), 2);
        assert_eq!(compute_viewport_offset(6, 5, 7), 2);
    }

    #[test]
    fn viewport_offset_returns_zero_when_list_fits() {
        assert_eq!(compute_viewport_offset(0, 5, 0), 0);
        assert_eq!(compute_viewport_offset(0, 5, 5), 0);
        assert_eq!(compute_viewport_offset(4, 5, 5), 0);
        assert_eq!(compute_viewport_offset(2, 5, 3), 0);
    }

    #[test]
    fn viewport_offset_centers_cursor_in_middle_of_long_list() {
        // 20 rows, 5 visible — cursor at row 10 should land near the middle.
        // ideal = max(0, 10 - 2) = 8, min(8, 15) = 8. Visible: 8..=12.
        assert_eq!(compute_viewport_offset(10, 5, 20), 8);
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
            RenderedRow::Collapsed { count: 2, summary: "x".into() },
            RowSource::Run { start: 0, len: 2 },
        );
        assert!(selectable_gate(&appr, Some(&run), &empty, &empty).is_none());

        // A non-gate event is not resolvable.
        let other = vec![gate_entry("tool.completed")];
        assert!(selectable_gate(&other, Some(&single), &empty, &empty).is_none());
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
    fn empty_interaction_answer_is_rejected_before_any_rpc() {
        // An empty answer is rejected locally (the gateway requires non-empty),
        // so the caller never marks the gate acted on a doomed submission.
        let client = RoomClient::for_test();
        let gi = GateInput {
            action: GateAction::Answer,
            id: "int-1".into(),
            buffer: "   ".into(), // whitespace-only ⇒ empty after trim
            options: Vec::new(),
            allow_freeform: true,
        };
        let err = resolve_gate(&client, &gi, None).unwrap_err();
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

        // Empty with options ⇒ guidance to type a number; empty without ⇒ "empty".
        assert!(answer_params(&mk("", opts(), true), None).unwrap_err().contains("number"));
        assert!(answer_params(&mk("", Vec::new(), true), None).unwrap_err().contains("empty"));

        // Plain free-text with no options ⇒ answer_text.
        let p = answer_params(&mk("ship it", Vec::new(), true), None).unwrap();
        assert_eq!(p["answer_text"], "ship it");
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
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].id, "o1");
        assert_eq!(opts[1].label, "No");
        assert!(!freeform);
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
                in_flight: false,
                show_reasoning: true,
            }),
            RowSource::Single(0),
        )
    }

    #[test]
    fn in_flight_marker_only_on_most_recent_row_of_open_turn() {
        // 3 rows in turn-A, all un-closed. Only the LAST should get the
        // spinner — earlier rows keep their normal altitude glyph.
        let mut rows = vec![
            spec_with_turn(Some("A"), "first"),
            spec_with_turn(Some("A"), "second"),
            spec_with_turn(Some("A"), "third"),
        ];
        let open: HashSet<String> = ["A".into()].into_iter().collect();
        let boundaries = annotate_turns_and_in_flight(&mut rows, &open, true);
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
    }

    #[test]
    fn closed_turn_marks_no_rows_as_in_flight() {
        // Turn B has turn.start and turn.end, so it's closed — no spinner.
        let mut rows = vec![
            spec_with_turn(Some("B"), "early"),
            spec_with_turn(Some("B"), "late"),
        ];
        let open: HashSet<String> = HashSet::new();
        annotate_turns_and_in_flight(&mut rows, &open, true);
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
        annotate_turns_and_in_flight(&mut rows, &open, false);
        let shown: Vec<bool> = rows
            .iter()
            .map(|(r, _)| match r {
                RenderedRow::Line(s) => s.show_reasoning,
                _ => false,
            })
            .collect();
        assert_eq!(shown, vec![true, false]);
    }
}
