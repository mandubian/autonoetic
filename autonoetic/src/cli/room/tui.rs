//! Interactive Session Room shell (#363) — the ratatui renderer.
//!
//! A scrollable, live-tailing view of the canonical timeline with an altitude
//! dial, squash, drill-down, and conversational gate resolution. P3.b-2 (#392):
//! a **gateway API client** — reads via `session.timeline.list`, resolves gates
//! via `approvals.approve`/`reject` and `interaction.resolve_and_answer`. No
//! direct store access. chat.rs untouched.

use super::channel::{Channel, GateAction, GateKind, GateRef, TuiChannel};
use super::client::RoomClient;
use super::render::{self, RenderedRow, RowSource};
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
use std::collections::HashSet;
use std::io;
use std::time::Duration;

/// An in-flight operator decision — captures an optional motivation (approvals,
/// §3.5) or the answer text (interactions) before committing. `GateRef`,
/// `GateKind`, and `GateAction` are the channel-neutral primitives, shared from
/// [`super::channel`].
struct GateInput {
    action: GateAction,
    id: String,
    buffer: String,
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
    root_session_id: &str,
    initial_floor: Altitude,
    limit: u32,
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
    let mut status: Option<String> = None; // last action / connection result
    // Gates no longer offerable: approvals resolved on the timeline, plus
    // anything the operator just acted on (covers interactions, which have no
    // timeline resolution event yet).
    let mut resolved: HashSet<String> = HashSet::new();
    let mut acted: HashSet<String> = HashSet::new();

    loop {
        // Fetch at most one page per tick via the gateway API. On error (gateway
        // down), surface it and keep retrying — don't crash the UI.
        match rpc(
            client,
            "session.timeline.list",
            serde_json::json!({
                "root_session_id": root_session_id,
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
        // Rows + their source mapping (lets Enter drill into the underlying event).
        let indexed: Vec<(RenderedRow, RowSource)> = if squash {
            render::coalesce_indexed(&visible)
        } else {
            visible
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    (
                        RenderedRow::Line { text: render::render_line(e), altitude: e.altitude },
                        RowSource::Single(i),
                    )
                })
                .collect()
        };
        let rows: Vec<RenderedRow> = indexed.iter().map(|(r, _)| r.clone()).collect();

        if follow {
            selected = rows.len().saturating_sub(1);
        } else {
            selected = selected.min(rows.len().saturating_sub(1));
        }

        let gate = selectable_gate(&visible, indexed.get(selected), &resolved, &acted);

        terminal.draw(|f| {
            draw(f, root_session_id, floor, squash, follow, &rows, selected, detail.as_deref(), input.as_ref(), status.as_deref(), gate.as_ref())
        })?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Text-capture mode takes over all input while open.
                if let Some(gi) = input.as_mut() {
                    match key.code {
                        KeyCode::Esc => input = None,
                        KeyCode::Enter => {
                            let gi = input.take().unwrap();
                            match resolve_gate(client, &gi) {
                                // Mark acted only on success — a failed RPC or an
                                // empty answer leaves the gate offerable.
                                Ok(msg) => {
                                    acted.insert(gi.id.clone());
                                    status = Some(msg);
                                }
                                // Reopen capture with the buffer intact so the
                                // operator can fix the input and resubmit.
                                Err(msg) => {
                                    status = Some(msg);
                                    input = Some(gi);
                                }
                            }
                        }
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
                            });
                            status = None;
                        }
                    }
                    // r: reply to the selected pending interaction (user.ask).
                    KeyCode::Char('r') => {
                        if let Some(g) = gate.as_ref().filter(|g| g.kind == GateKind::Interaction) {
                            detail = None;
                            input = Some(GateInput {
                                action: GateAction::Answer,
                                id: g.id.clone(),
                                buffer: String::new(),
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

/// Resolve a gate over the gateway API (the sanctioned path that unblocks the
/// waiting agent + records the decision incl. decider kind, #361). Decider is
/// the operator; `buffer` is the motivation (approvals) or answer (interactions).
///
/// Returns `Ok(msg)` only when the gateway accepted the decision (the caller
/// uses this to mark the gate acted); `Err(msg)` on validation or transport
/// failure, leaving the gate offerable so the operator can retry.
fn resolve_gate(client: &RoomClient, gi: &GateInput) -> Result<String, String> {
    let text = gi.buffer.trim();
    let reason = (!text.is_empty()).then(|| text.to_string());
    // The gateway requires a non-empty answer for interactions; reject locally
    // rather than round-trip a guaranteed server rejection (and so we never mark
    // the gate acted on an empty submission).
    if gi.action == GateAction::Answer && text.is_empty() {
        return Err("✗ answer cannot be empty".to_string());
    }
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
            rpc(
                client,
                "interaction.resolve_and_answer",
                serde_json::json!({ "interaction_id": gi.id, "answer_text": text, "answered_by": "operator" }),
            ),
            "answered",
        ),
    };
    match result {
        Ok(_) => Ok(format!("✓ {verb} {}", gi.id)),
        Err(e) => Err(format!("✗ {e}")),
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
    status: Option<&str>,
    gate: Option<&GateRef>,
) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(f.area());

    let header = format!(
        " Session Room [{}] — {root}   floor: {}   squash: {}   {} rows{}{}",
        TuiChannel.kind(),
        floor.as_str(),
        if squash { "on" } else { "off" },
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

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let style = altitude_style(render::row_altitude(row));
            ListItem::new(Line::from(Span::styled(render::row_text(row), style)))
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

    let footer = if let Some(gi) = input {
        let label = match gi.action {
            GateAction::Approve => "APPROVE — motivation (optional)",
            GateAction::Reject => "REJECT — motivation (optional)",
            GateAction::Answer => "ANSWER",
        };
        Paragraph::new(format!(
            " {label}: {}▏   [Enter submit · Esc cancel]",
            gi.buffer
        ))
        .style(Style::default().fg(Color::Cyan))
    } else {
        // The gate affordance hint is the channel's concern (#393) — route it
        // through the channel so a Discord/WhatsApp bridge can render its own.
        let gate_hint = gate.map(|g| TuiChannel.gate_prompt(g)).unwrap_or_default();
        Paragraph::new(format!(
            " q quit · j/k scroll · g/G top/bottom · a altitude · s squash · ⏎ detail{gate_hint}"
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
            RenderedRow::Line { text: "x".into(), altitude: Altitude::Attention },
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
        };
        let err = resolve_gate(&client, &gi).unwrap_err();
        assert!(err.contains("empty"), "expected empty-answer rejection, got: {err}");
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
}
