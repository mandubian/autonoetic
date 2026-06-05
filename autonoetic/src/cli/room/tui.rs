//! Interactive Session Room shell (#363) — the ratatui renderer.
//!
//! A scrollable, live-tailing view of the canonical timeline with an altitude
//! dial, squash, drill-down, and conversational gate resolution. P3.b-2 (#392):
//! a **gateway API client** — reads via `session.timeline.list`, resolves gates
//! via `approvals.approve`/`reject` and `interaction.resolve_and_answer`. No
//! direct store access. chat.rs untouched.

use super::channel::{Channel, GateAction, GateKind, GateRef, TuiChannel};
use super::client::RoomClient;
use super::render::{self, ActorKind, RenderedRow, RowSource, RowSpec};
use super::slash::SlashCommand;
use autonoetic_types::session_timeline::{Altitude, SessionTimelineEntry, SessionTimelineListResult};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::time::Duration;

/// Spinner frames — a gentle breathing indicator on the in-flight row. The
/// current frame is rotated on each TUI tick (the existing 250 ms poll loop).
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Hard cap on the number of physical text lines a single rich row may occupy.
/// Anything over this gets a `…` ellipsis on the last line. Keeps the visible
/// list from collapsing to one giant paragraph when a long agent message
/// arrives — the full text is always one ⏎ away in the detail pane.
const MAX_ROW_LINES: usize = 2;

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

/// One pre-digested choice surfaced from a `user.ask.pending` event payload.
#[derive(Clone)]
struct GateOption {
    id: String,
    label: String,
}

/// Read the pre-digested choices + freeform policy for an interaction from its
/// `user.ask.pending` timeline entry (the gateway embeds them, #393). Returns
/// `(options, allow_freeform)`; missing `allow_freeform` defaults to permissive.
fn interaction_choices(entries: &[SessionTimelineEntry], interaction_id: &str) -> (Vec<GateOption>, bool) {
    let entry = entries.iter().find(|e| {
        e.event_type == "user.ask.pending"
            && e.refs.interaction_id.as_deref() == Some(interaction_id)
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
    let mut input: Option<GateInput> = None; // in-flight gate decision
    let mut compose: Option<String> = None; // in-flight free-form message to the session
    let mut slash: Option<String> = None; // in-flight slash-command buffer (no leading `/`)
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

        let gate = selectable_gate(&visible, indexed.get(selected), &resolved, &acted);

        spinner_frame = (spinner_frame + 1) % SPINNER_FRAMES.len();
        let spinner_glyph = SPINNER_FRAMES[spinner_frame];

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
                input.as_ref(),
                compose.as_deref(),
                slash.as_deref(),
                status.as_deref(),
                gate.as_ref(),
                spinner_glyph,
                &turn_boundaries,
                show_reasoning,
            )
        })?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Compose mode: capturing a free-form message to the session (#405).
                if let Some(buf) = compose.as_mut() {
                    match key.code {
                        KeyCode::Esc => compose = None,
                        KeyCode::Enter => {
                            let text = buf.trim().to_string();
                            if text.is_empty() {
                                compose = None; // nothing to send
                            } else {
                                // Async so the sync loop never blocks on the agent
                                // turn; the operator line + reply stream in via polling.
                                status = Some(send_message(client, root_session_id, &text, target_agent_id.as_deref()));
                                compose = None;
                                follow = true; // jump to newest to watch the exchange
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
                                    status = Some(format!(
                                        "help: {}",
                                        super::slash::HELP_TEXT
                                    ));
                                }
                                SlashCommand::SwitchSession(new_id) => {
                                    if new_id.is_empty() {
                                        status = Some("✗ /session: missing id".to_string());
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
                                    status = Some(list_sessions_status(client, agent.as_deref()));
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
                        match resolve_gate(client, &gi, chosen.as_ref()) {
                            // Mark acted only on success — a failed RPC or an
                            // invalid answer leaves the gate offerable.
                            Ok(msg) => {
                                acted.insert(gi.id.clone());
                                status = Some(msg);
                            }
                            // Reopen capture with the buffer intact so the operator
                            // can fix the input and resubmit.
                            Err(msg) => {
                                status = Some(msg);
                                input = Some(gi);
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
                        } else {
                            break;
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
                            let (options, allow_freeform) = interaction_choices(&visible, &g.id);
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
                        detail = if detail.is_some() {
                            None
                        } else {
                            indexed.get(selected).map(|(_, src)| detail_for(&visible, *src))
                        };
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
                        detail = None;
                        compose = Some(String::new());
                        status = None;
                    }
                    // /: slash-command mode (vim/Discord convention). `:`
                    // and `?` are accepted aliases for muscle memory.
                    KeyCode::Char('/') | KeyCode::Char(':') | KeyCode::Char('?') => {
                        detail = None;
                        slash = Some(String::new());
                        status = None;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        follow = false;
                        detail = None;
                        selected = (selected + 1).min(rows.len().saturating_sub(1));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        follow = false;
                        detail = None;
                        selected = selected.saturating_sub(1);
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
                    _ => {}
                }
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

/// The still-resolvable gate at the selection (a single, not-yet-resolved
/// `approval.pending` or `user.ask.pending` row).
fn selectable_gate(
    entries: &[SessionTimelineEntry],
    src: Option<&(RenderedRow, RowSource)>,
    resolved: &HashSet<String>,
    acted: &HashSet<String>,
) -> Option<GateRef> {
    let (_, RowSource::Single(i)) = src? else {
        return None;
    };
    let e = entries.get(*i)?;
    match e.event_type.as_str() {
        "approval.pending" => {
            let id = e.refs.approval_request_id.clone()?;
            (!resolved.contains(&id) && !acted.contains(&id)).then_some(GateRef {
                kind: GateKind::Approval,
                id,
            })
        }
        "user.ask.pending" => {
            let id = e.refs.interaction_id.clone()?;
            (!acted.contains(&id)).then_some(GateRef {
                kind: GateKind::Interaction,
                id,
            })
        }
        _ => None,
    }
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
        RowSource::Single(i) => entries.get(i).map(render::format_detail).unwrap_or_default(),
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
    let rail_style = Style::default().fg(actor_color(spec.actor));
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
    let mut headline = spec.headline.clone();
    if !show_reasoning && headline.starts_with('💭') {
        if let Some(stripped) = headline.strip_prefix('💭') {
            headline = stripped.trim_start().to_string();
        }
    }
    let headline_capped = cap_to_width(&headline, content_w);
    let head_style = altitude_style(spec.altitude).patch(rail_style);
    lines.push(Line::from(vec![
        Span::styled(rail_block, rail_style),
        Span::raw(" "),
        Span::styled(format!("{glyph:<2}"), head_style),
        Span::styled(label_padded, Style::default().fg(actor_color(spec.actor))),
        Span::raw(" "),
        Span::styled(headline_capped, head_style),
    ]));
    if let Some(d) = &spec.detail {
        if !d.is_empty() {
            let prefix = "  ↳ ";
            let avail = content_w.saturating_sub(prefix.chars().count());
            let detail_capped = cap_to_width(d, avail);
            let pad = " ".repeat(rail_w + glyph_w + label_w + 1);
            lines.push(Line::from(vec![
                Span::styled(pad, Style::default()),
                Span::styled(
                    format!("{prefix}{detail_capped}"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
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
    if physical.len() > MAX_ROW_LINES {
        let dropped = physical.len() - MAX_ROW_LINES;
        let mut kept: Vec<Line<'static>> =
            physical.into_iter().take(MAX_ROW_LINES).collect();
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

/// Cap a string to `width` visible characters; append `…` if truncated. Counts
/// Unicode chars, not bytes (so emoji don't blow the budget).
fn cap_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    let truncated: String = s.chars().take(width - 1).collect();
    format!("{truncated}…")
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

/// Build a one-line-per-row status for `/session list [agent]` showing the
/// top few recent sessions. We don't render a real modal here — the operator
/// picks by typing `/session <id>` from the list. Returns a status string the
/// caller drops into the footer.
fn list_sessions_status(client: &RoomClient, agent: Option<&str>) -> String {
    let params = serde_json::json!({
        "agent_id": agent,
        "limit": 10,
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
                format!("(no sessions{hint}) — /session <id> or start one with `autonoetic run`")
            }
            Ok(parsed) => {
                let head = if let Some(a) = agent {
                    format!("sessions for agent '{a}':")
                } else {
                    "recent sessions:".to_string()
                };
                let rows: Vec<String> = parsed
                    .sessions
                    .iter()
                    .take(5)
                    .map(|s| format!("  {} [{}] @ {}", s.root_session_id, s.agent_id, s.last_active_at))
                    .collect();
                let more = if parsed.sessions.len() > 5 {
                    format!("  …(+{} more)", parsed.sessions.len() - 5)
                } else {
                    String::new()
                };
                format!("{head}\n{}\n{more}\n  → type /session <id> to switch", rows.join("\n"))
            }
            Err(e) => format!("✗ malformed session.list response: {e}"),
        },
        Err(e) => format!("✗ session.list failed: {e}"),
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
    input: Option<&GateInput>,
    compose: Option<&str>,
    slash: Option<&str>,
    status: Option<&str>,
    gate: Option<&GateRef>,
    spinner_glyph: &'static str,
    turn_boundaries: &HashMap<usize, bool>,
    show_reasoning: bool,
) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(f.area());

    let header = format!(
        " Session Room [{}] — {root}   floor: {}   squash: {}   reasoning: {}   {} rows{}{}",
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
        let text: Vec<Line> = lines.iter().map(|l| Line::from(l.as_str())).collect();
        f.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" event detail ")),
            chunks[1],
        );
        f.render_widget(
            Paragraph::new(" Esc/Enter close detail · q quit").style(Style::default().fg(Color::DarkGray)),
            chunks[2],
        );
        return;
    }

    // The terminal width caps each line. Reserve 2 cells for the actor rail
    // and 3 cells for the altitude glyph + space, leaving the rest for the
    // label + headline + detail.
    let width = chunks[1].width as usize;
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
        state.select(Some(selected.min(rows.len() - 1)));
    }
    f.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::NONE))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        chunks[1],
        &mut state,
    );

    let footer = if let Some(buf) = slash {
        Paragraph::new(format!(
            " : /{buf}▏   [Enter run · Esc cancel]   {HELP}",
            HELP = super::slash::HELP_TEXT
        ))
        .style(Style::default().fg(Color::Magenta))
    } else if let Some(buf) = compose {
        Paragraph::new(format!(" MESSAGE: {buf}▏   [Enter send · Esc cancel]"))
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
        Paragraph::new(format!(
            " q quit · j/k scroll · g/G top/bottom · a altitude · s squash · R reasoning · i message · / cmd · ⏎ detail{gate_hint}"
        ))
        .style(Style::default().fg(Color::DarkGray))
    };
    f.render_widget(footer, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;

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
