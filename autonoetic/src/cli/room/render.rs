//! Pure rendering core for the Session Room (#363 P2).
//!
//! Channel-neutral: turns a `SessionTimelineEntry` into a one-line human string.
//! Deliberately free of any I/O or terminal state so the TUI shell, the CLI
//! viewer, and (later) external channel bridges all share the *same* formatting.
//! Presentation only — importance/altitude is decided gateway-side.

use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::session_timeline::{Altitude, SessionRole, SessionTimelineEntry};

/// Altitude glyph — the at-a-glance importance marker.
pub fn altitude_glyph(altitude: Altitude) -> &'static str {
    match altitude {
        Altitude::Error => "✗",
        Altitude::Attention => "⚠",
        Altitude::Normal => "▸",
        Altitude::Detail => "·",
    }
}

/// A compact actor label: the seat, prefixed when the occupant is not a normal
/// autonoetic agent (so a human operator or a foreign agent is obvious).
pub fn actor_label(entry: &SessionTimelineEntry) -> String {
    let seat = role_label(&entry.role);
    match &entry.principal.kind {
        PrincipalKind::Human => format!("🧑 {seat}"),
        PrincipalKind::ForeignAgent { provider } => format!("🌐 {seat}·{provider}"),
        PrincipalKind::Script => seat,
        PrincipalKind::AutonoeticAgent => seat,
    }
}

fn role_label(role: &SessionRole) -> String {
    match role {
        SessionRole::Operator => "operator".into(),
        SessionRole::Planner => "planner".into(),
        SessionRole::Specialist { kind } => kind.clone(),
        SessionRole::Sentinel => "sentinel".into(),
        SessionRole::Curator => "curator".into(),
        SessionRole::Auditor => "auditor".into(),
        SessionRole::Tool { surface } => surface.clone(),
        SessionRole::ExternalSurface { surface } => surface.clone(),
        SessionRole::Runtime => "runtime".into(),
    }
}

/// Human summary of an event, from its type + payload. Keeps the most useful
/// field per known event type; falls back to the bare event type.
pub fn summarize(entry: &SessionTimelineEntry) -> String {
    let p = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let field = |key: &str| -> Option<String> {
        p.as_ref()
            .and_then(|v| v.get(key))
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };

    match entry.event_type.as_str() {
        "approval.pending" => format!(
            "approval requested ({})",
            field("request_id").unwrap_or_default()
        ),
        "approval.approved" => format!("approval granted ({})", field("request_id").unwrap_or_default()),
        "approval.rejected" => format!("approval denied ({})", field("request_id").unwrap_or_default()),
        "approval.cancelled" => format!("approval cancelled ({})", field("request_id").unwrap_or_default()),
        "plan.pending" => format!("plan proposed: {}", field("title").unwrap_or_default()),
        "plan.approved" => format!("plan approved ({})", field("plan_id").unwrap_or_default()),
        "divergence.intervention" => format!(
            "divergence: {} (turn {})",
            field("level").unwrap_or_else(|| "?".into()),
            p.as_ref().and_then(|v| v.get("turn")).and_then(|x| x.as_u64()).unwrap_or(0)
        ),
        "workbench.created" => "workbench projected".into(),
        "workbench.reconciled" => "workbench reconciled".into(),
        "workbench.discarded" => "workbench discarded".into(),
        "user.ask.pending" => format!(
            "asks: {}",
            field("question").unwrap_or_default()
        ),
        "tool.completed" => format!("tool {}", field("tool").unwrap_or_else(|| "completed".into())),
        "llm.request_failed" => format!("LLM error: {}", field("error").unwrap_or_default()),
        other => other.to_string(),
    }
}

/// Full one-line rendering: `<glyph> [<actor>] <summary>`.
pub fn render_line(entry: &SessionTimelineEntry) -> String {
    format!(
        "{} [{}] {}",
        altitude_glyph(entry.altitude),
        actor_label(entry),
        summarize(entry)
    )
}

/// A rendered timeline row: either a single event line, or a *collapsed* run of
/// consecutive low-altitude (Detail) plumbing folded into one count row. The
/// structured form lets the interactive shell expand a collapsed run on demand;
/// non-interactive consumers render it via [`row_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderedRow {
    Line(String),
    Collapsed { count: usize, summary: String },
}

/// Fold consecutive `Detail` events into a single collapsed row so routine
/// plumbing (turns, workbench bookkeeping, polls) doesn't flood the view when
/// the floor is low. A lone Detail event renders normally — collapsing one is
/// pointless. Higher altitudes always render individually. Coalescing is
/// page-local; a run split across reads collapses per page.
pub fn coalesce(entries: &[SessionTimelineEntry]) -> Vec<RenderedRow> {
    let mut out = Vec::new();
    let mut run: Vec<&SessionTimelineEntry> = Vec::new();
    for e in entries {
        if e.altitude == Altitude::Detail {
            run.push(e);
        } else {
            flush_run(&mut run, &mut out);
            out.push(RenderedRow::Line(render_line(e)));
        }
    }
    flush_run(&mut run, &mut out);
    out
}

fn flush_run<'a>(run: &mut Vec<&'a SessionTimelineEntry>, out: &mut Vec<RenderedRow>) {
    match run.len() {
        0 => {}
        1 => out.push(RenderedRow::Line(render_line(run[0]))),
        n => out.push(RenderedRow::Collapsed {
            count: n,
            summary: collapsed_summary(run),
        }),
    }
    run.clear();
}

/// Brief breakdown of a collapsed run: the top event types by count.
fn collapsed_summary(run: &[&SessionTimelineEntry]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for e in run {
        match counts.iter_mut().find(|(k, _)| *k == e.event_type) {
            Some((_, c)) => *c += 1,
            None => counts.push((e.event_type.as_str(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    let parts: Vec<String> = counts
        .iter()
        .take(3)
        .map(|(k, c)| format!("{k}×{c}"))
        .collect();
    let more = if counts.len() > 3 { ", …" } else { "" };
    format!("routine events ({}{})", parts.join(", "), more)
}

/// Non-interactive rendering of a row (the collapsed form shows the count).
pub fn row_text(row: &RenderedRow) -> String {
    match row {
        RenderedRow::Line(s) => s.clone(),
        RenderedRow::Collapsed { count, summary } => {
            format!("{} ⟨{} {}⟩", altitude_glyph(Altitude::Detail), count, summary)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::principal::Principal;
    use autonoetic_types::session_timeline::TimelineRefs;

    fn entry(role: SessionRole, kind_principal: Principal, et: &str, alt: Altitude, payload: serde_json::Value) -> SessionTimelineEntry {
        SessionTimelineEntry {
            event_id: "ev-1".into(),
            root_session_id: "r".into(),
            source_session_id: "r".into(),
            turn_id: None,
            principal: kind_principal,
            role,
            event_type: et.into(),
            altitude: alt,
            occurred_at: "2026-06-01T00:00:00Z".into(),
            payload: Some(payload.to_string()),
            refs: TimelineRefs::default(),
        }
    }

    #[test]
    fn renders_human_operator_approval() {
        let e = entry(
            SessionRole::Operator,
            Principal::human("operator"),
            "approval.rejected",
            Altitude::Attention,
            serde_json::json!({ "request_id": "apr-9" }),
        );
        let line = render_line(&e);
        assert!(line.starts_with("⚠"));
        assert!(line.contains("🧑 operator"));
        assert!(line.contains("approval denied (apr-9)"));
    }

    #[test]
    fn renders_sentinel_divergence_and_foreign_agent() {
        let s = entry(
            SessionRole::Sentinel,
            Principal::agent("sentinel"),
            "divergence.intervention",
            Altitude::Attention,
            serde_json::json!({ "level": "diverging", "turn": 4 }),
        );
        assert!(render_line(&s).contains("sentinel"));
        assert!(render_line(&s).contains("divergence: diverging (turn 4)"));

        let f = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::foreign("claude-code", "fa-1"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({ "tool": "edit" }),
        );
        assert!(render_line(&f).contains("🌐 coder·claude-code"));
    }

    #[test]
    fn coalesce_folds_detail_runs_but_keeps_higher_altitudes() {
        let mk = |et: &str, alt: Altitude| {
            entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                et,
                alt,
                serde_json::json!({}),
            )
        };
        let entries = vec![
            mk("turn.start", Altitude::Detail),
            mk("workbench.created", Altitude::Detail),
            mk("turn.start", Altitude::Detail),
            mk("approval.pending", Altitude::Attention), // breaks the run
            mk("turn.end", Altitude::Detail),            // lone detail ⇒ normal line
        ];
        let rows = coalesce(&entries);
        assert_eq!(rows.len(), 3);
        match &rows[0] {
            RenderedRow::Collapsed { count, summary } => {
                assert_eq!(*count, 3);
                assert!(summary.contains("turn.start×2"));
            }
            other => panic!("expected collapsed run, got {other:?}"),
        }
        assert!(matches!(&rows[1], RenderedRow::Line(s) if s.contains("approval requested")));
        // The trailing lone Detail event is a normal line, not collapsed.
        assert!(matches!(&rows[2], RenderedRow::Line(_)));
        assert!(row_text(&rows[0]).contains("⟨3 routine events"));
    }

    #[test]
    fn unknown_event_falls_back_to_type() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "some.future.event",
            Altitude::Normal,
            serde_json::json!({}),
        );
        assert!(render_line(&e).contains("some.future.event"));
    }
}
