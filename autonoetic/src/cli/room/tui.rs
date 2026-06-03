//! Interactive Session Room shell (#363 P2) — the ratatui renderer.
//!
//! A scrollable, live-tailing view of the canonical timeline with an altitude
//! dial and squash toggle. Greenfield (chat.rs untouched). Read-only for now —
//! conversational gate *input* is a later slice. Built on the shared render
//! core, so styling/summaries match the CLI viewer and future channel bridges.

use super::render::{self, RenderedRow, RowSource};
use autonoetic_gateway::scheduler::GatewayStore;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::session_timeline::{Altitude, SessionTimelineEntry};

/// An in-flight operator decision on a pending approval gate — captures an
/// optional motivation before committing (§3.5 conversational gate).
struct GateInput {
    approve: bool,
    request_id: String,
    buffer: String,
}
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::io;
use std::time::Duration;

/// Restores the terminal on drop, even on early return / panic-unwind.
struct TerminalRestore;
impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn run(
    config: &GatewayConfig,
    store: &GatewayStore,
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
    let mut status: Option<String> = None; // last action result

    loop {
        // Fetch at most one page per tick so a large backlog drains across ticks
        // without ever blocking input (incl. quit). The 250ms poll paces catch-up.
        let page = store.list_session_timeline(root_session_id, cursor.as_deref(), limit, Some(floor), None)?;
        if let Some(last) = page.entries.last() {
            cursor = Some(last.event_id.clone());
        }
        entries.extend(page.entries);

        // Rows + their source mapping (lets Enter drill into the underlying event).
        let indexed: Vec<(RenderedRow, RowSource)> = if squash {
            render::coalesce_indexed(&entries)
        } else {
            entries
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

        let pending_approval = pending_approval_id(&entries, indexed.get(selected));

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
                status.as_deref(),
                pending_approval.is_some(),
            )
        })?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Motivation-capture mode takes over all input while open.
                if let Some(gi) = input.as_mut() {
                    match key.code {
                        KeyCode::Esc => input = None,
                        KeyCode::Enter => {
                            let gi = input.take().unwrap();
                            let reason = {
                                let t = gi.buffer.trim();
                                (!t.is_empty()).then(|| t.to_string())
                            };
                            status = Some(resolve_gate(config, store, &gi, reason));
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
                    // Esc closes the detail pane if open, otherwise quits.
                    KeyCode::Esc => {
                        if detail.is_some() {
                            detail = None;
                        } else {
                            break;
                        }
                    }
                    // y/n: resolve the selected pending approval (captures an
                    // optional motivation first — §3.5; recording-only, no
                    // BLOCKING enforcement yet).
                    KeyCode::Char('y') | KeyCode::Char('n') => {
                        if let Some(request_id) = pending_approval.clone() {
                            input = Some(GateInput {
                                approve: key.code == KeyCode::Char('y'),
                                request_id,
                                buffer: String::new(),
                            });
                            status = None;
                        }
                    }
                    // Enter toggles drill-down on the selected row.
                    KeyCode::Enter => {
                        detail = if detail.is_some() {
                            None
                        } else {
                            indexed.get(selected).map(|(_, src)| detail_for(&entries, *src))
                        };
                    }
                    KeyCode::Char('a') => {
                        // Cycle the altitude floor; refetch from scratch since the
                        // gateway filters server-side.
                        floor = cycle_floor(floor);
                        entries.clear();
                        cursor = None;
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

/// The approval request_id if the selected row is a single, still-`pending`
/// approval gate (so it can be resolved in-flow).
fn pending_approval_id(
    entries: &[SessionTimelineEntry],
    src: Option<&(RenderedRow, RowSource)>,
) -> Option<String> {
    if let Some((_, RowSource::Single(i))) = src {
        let e = entries.get(*i)?;
        if e.event_type == "approval.pending" {
            return e.refs.approval_request_id.clone();
        }
    }
    None
}

/// Resolve an approval through the sanctioned scheduler path (which records the
/// decision — incl. decider kind via #361 — and queues the session-resume
/// notification). Decider is the operator; the motivation is the optional reason.
fn resolve_gate(
    config: &GatewayConfig,
    store: &GatewayStore,
    gi: &GateInput,
    reason: Option<String>,
) -> String {
    let result = if gi.approve {
        autonoetic_gateway::scheduler::approve_request_with_options(
            config,
            Some(store),
            &gi.request_id,
            "operator",
            reason,
            None,
            None,
            None,
            autonoetic_gateway::scheduler::ApproveOptions::default(),
        )
    } else {
        autonoetic_gateway::scheduler::reject_request(
            config,
            Some(store),
            &gi.request_id,
            "operator",
            reason,
            None,
        )
    };
    match result {
        Ok(d) => format!(
            "✓ {} {}",
            if gi.approve { "approved" } else { "rejected" },
            d.request_id
        ),
        Err(e) => format!("✗ {e}"),
    }
}

/// Build the drill-down detail for a selected row's source. A single event
/// shows its full metadata/payload; a collapsed run shows what it folds.
fn detail_for(entries: &[SessionTimelineEntry], src: RowSource) -> Vec<String> {
    match src {
        RowSource::Single(i) => entries
            .get(i)
            .map(render::format_detail)
            .unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn approval_entry(event_type: &str) -> SessionTimelineEntry {
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
                ..Default::default()
            },
        }
    }

    #[test]
    fn pending_approval_id_only_for_selected_pending_gate() {
        let row = (
            RenderedRow::Line { text: "x".into(), altitude: Altitude::Attention },
            RowSource::Single(0),
        );
        // A selected approval.pending row → its request id is resolvable.
        let pending = vec![approval_entry("approval.pending")];
        assert_eq!(pending_approval_id(&pending, Some(&row)), Some("apr-1".into()));
        // Any other event (incl. an already-resolved approval) → not resolvable.
        let resolved = vec![approval_entry("approval.approved")];
        assert_eq!(pending_approval_id(&resolved, Some(&row)), None);
        // A collapsed run is never a single resolvable gate.
        let run = (
            RenderedRow::Collapsed { count: 2, summary: "x".into() },
            RowSource::Run { start: 0, len: 2 },
        );
        assert_eq!(pending_approval_id(&pending, Some(&run)), None);
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
            vec![
                Altitude::Normal,
                Altitude::Attention,
                Altitude::Error,
                Altitude::Detail
            ]
        );
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
    can_resolve: bool,
) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(f.area());

    let header = format!(
        " Session Room — {root}   floor: {}   squash: {}   {} rows{}{}",
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
        // Drill-down pane replaces the list while open.
        let text: Vec<Line> = lines.iter().map(|l| Line::from(l.as_str())).collect();
        f.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" event detail ")),
            chunks[1],
        );
        f.render_widget(
            Paragraph::new(" Esc/Enter close detail · q quit")
                .style(Style::default().fg(Color::DarkGray)),
            chunks[2],
        );
        return;
    }

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            let style = altitude_style(render::row_altitude(row));
            // Pass the Cow through — borrows for `Line` rows, allocates only for collapsed.
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
        Paragraph::new(format!(
            " {} — motivation (optional): {}▏   [Enter submit · Esc cancel]",
            if gi.approve { "APPROVE" } else { "REJECT" },
            gi.buffer,
        ))
        .style(Style::default().fg(Color::Cyan))
    } else {
        let resolve_hint = if can_resolve { " · y/n approve/reject" } else { "" };
        Paragraph::new(format!(
            " q quit · j/k scroll · g/G top/bottom · a altitude · s squash · ⏎ detail{resolve_hint}"
        ))
        .style(Style::default().fg(Color::DarkGray))
    };
    f.render_widget(footer, chunks[2]);
}
