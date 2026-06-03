//! Interactive Session Room shell (#363 P2) — the ratatui renderer.
//!
//! A scrollable, live-tailing view of the canonical timeline with an altitude
//! dial and squash toggle. Greenfield (chat.rs untouched). Read-only for now —
//! conversational gate *input* is a later slice. Built on the shared render
//! core, so styling/summaries match the CLI viewer and future channel bridges.

use super::render::{self, RenderedRow, RowSource};
use autonoetic_gateway::scheduler::GatewayStore;
use autonoetic_types::session_timeline::{Altitude, SessionTimelineEntry};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
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

        terminal.draw(|f| draw(f, root_session_id, floor, squash, follow, &rows, selected, detail.as_deref()))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
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
) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(f.area());

    let header = format!(
        " Session Room — {root}   floor: {}   squash: {}   {} rows{}",
        floor.as_str(),
        if squash { "on" } else { "off" },
        rows.len(),
        if follow { "   (following)" } else { "" },
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

    f.render_widget(
        Paragraph::new(" q quit · j/k scroll · g/G top/bottom · a altitude · s squash · ⏎ detail")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}
