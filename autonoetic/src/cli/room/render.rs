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

/// Collapse a possibly multi-line string into a single timeline line: runs of
/// whitespace (incl. newlines) become one space, then truncate with an ellipsis.
/// Keeps a rich `user.ask` question or any prose from breaking the one-line feed.
/// The result is a **hard cap** of `max` chars — the ellipsis counts toward it,
/// so a truncated string keeps `max - 1` chars + `…`.
pub(crate) fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    if max == 0 {
        return String::new();
    }
    // Reserve one char for the ellipsis so the total never exceeds `max`.
    let truncated: String = flat.chars().take(max - 1).collect();
    format!("{truncated}…")
}

/// Render embedded pre-digested choices as a compact inline hint, e.g.
/// ` — [1] Yes · [2] No`. Reads the `options` array (objects with a `label`)
/// the gateway embeds in the `user.ask.pending` payload (#393). Empty ⇒ "".
fn choices_hint(payload: Option<&serde_json::Value>) -> String {
    let Some(opts) = payload.and_then(|v| v.get("options")).and_then(|v| v.as_array()) else {
        return String::new();
    };
    let parts: Vec<String> = opts
        .iter()
        .enumerate()
        .filter_map(|(i, o)| o.get("label").and_then(|l| l.as_str()).map(|l| (i, l)))
        .map(|(i, label)| format!("[{}] {}", i + 1, one_line(label, 24)))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" — {}", parts.join(" · "))
    }
}

/// Format the preceding action chain a failure carries (#367) as ` ⟵ after: a → b`.
/// Reads the `preceding` array of action labels; empty/absent ⇒ "".
fn preceding_chain(payload: Option<&serde_json::Value>) -> String {
    let Some(arr) = payload.and_then(|v| v.get("preceding")).and_then(|v| v.as_array()) else {
        return String::new();
    };
    let parts: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("  ⟵ after: {}", parts.join(" → "))
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
        // The operator's (or any actor's) own message into the session (#405).
        // The actor label already shows who; here we just show the text.
        "operator.message" => one_line(&field("message").unwrap_or_default(), 120),
        // The agent's own narrative (#367 P4): what it says, and (hidable) its
        // reasoning — so a turn reads intent → actions → result. Actor label
        // shows which agent; the 💭 marks reasoning as the "why".
        "agent.message" => one_line(&field("message").unwrap_or_default(), 160),
        "agent.reasoning" => format!("💭 {}", one_line(&field("reasoning").unwrap_or_default(), 160)),
        "user.ask.pending" => format!(
            "asks: {}{}",
            one_line(&field("question").unwrap_or_default(), 100),
            choices_hint(p.as_ref()),
        ),
        // Payload key is `tool_name`; keep `tool` as a fallback for older rows.
        "tool.completed" => format!(
            "tool {}",
            field("tool_name")
                .or_else(|| field("tool"))
                .unwrap_or_else(|| "completed".into())
        ),
        // A promotion/governance escalation awaiting the operator's decision (#413).
        // Revision ids are already prefixed (`rev-9`, `rev_sha256:…`), so show the
        // id as-is and omit the suffix entirely when absent.
        "escalation.pending" => {
            let synthesis = one_line(
                &field("synthesis").unwrap_or_else(|| "operator decision requested".into()),
                120,
            );
            match field("revision_id").filter(|r| !r.is_empty()) {
                Some(rev) => format!("escalation: {synthesis} ({rev})"),
                None => format!("escalation: {synthesis}"),
            }
        }
        // A sandbox escape attempt during execution (#413) — security-critical.
        "security.sandbox_escape" => format!(
            "SANDBOX ESCAPE ATTEMPT — {}",
            one_line(&field("indicator").unwrap_or_else(|| "blocked".into()), 120)
        ),
        "llm.request_failed" => format!(
            "LLM error: {}{}",
            one_line(&field("error").unwrap_or_default(), 120),
            preceding_chain(p.as_ref()),
        ),
        "runtime.lock_drift" => {
            let overridden = p
                .as_ref()
                .and_then(|v| v.get("override"))
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            let what = field("drift_field").unwrap_or_else(|| "binary".into());
            if overridden {
                format!("runtime lock drift ({what}) — overridden, running anyway")
            } else {
                format!("runtime lock drift ({what}) — execution blocked")
            }
        }
        // The root-session circuit breaker (#413) — kills processes, aborts tasks,
        // cancels gates. The most important thing to surface.
        "session.emergency_stop" => format!(
            "EMERGENCY STOP — {}",
            one_line(&field("reason").unwrap_or_else(|| "session halted".into()), 120)
        ),
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
    /// A single event line, carrying its altitude so consumers can style it.
    Line { text: String, altitude: Altitude },
    Collapsed { count: usize, summary: String },
}

/// The altitude a row renders at (collapsed runs are Detail by definition).
pub fn row_altitude(row: &RenderedRow) -> Altitude {
    match row {
        RenderedRow::Line { altitude, .. } => *altitude,
        RenderedRow::Collapsed { .. } => Altitude::Detail,
    }
}

/// Where a rendered row came from in the input slice — lets an interactive
/// consumer map a selected row back to the underlying event(s) for drill-down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSource {
    /// A single event at this index.
    Single(usize),
    /// A collapsed run covering `entries[start..start+len]`.
    Run { start: usize, len: usize },
}

/// Fold consecutive `Detail` events into a single collapsed row so routine
/// plumbing (turns, workbench bookkeeping, polls) doesn't flood the view when
/// the floor is low. A lone Detail event renders normally — collapsing one is
/// pointless. Higher altitudes always render individually. Coalescing is
/// page-local; a run split across reads collapses per page.
pub fn coalesce(entries: &[SessionTimelineEntry]) -> Vec<RenderedRow> {
    coalesce_indexed(entries).into_iter().map(|(r, _)| r).collect()
}

/// Like [`coalesce`], but also returns each row's [`RowSource`] for drill-down.
pub fn coalesce_indexed(entries: &[SessionTimelineEntry]) -> Vec<(RenderedRow, RowSource)> {
    let mut out = Vec::new();
    let mut run_start: Option<usize> = None;
    let mut run_len: usize = 0;
    for (i, e) in entries.iter().enumerate() {
        if e.altitude == Altitude::Detail {
            if run_start.is_none() {
                run_start = Some(i);
            }
            run_len += 1;
        } else {
            flush_run(entries, &mut run_start, &mut run_len, &mut out);
            out.push((
                RenderedRow::Line { text: render_line(e), altitude: e.altitude },
                RowSource::Single(i),
            ));
        }
    }
    flush_run(entries, &mut run_start, &mut run_len, &mut out);
    out
}

fn flush_run(
    entries: &[SessionTimelineEntry],
    run_start: &mut Option<usize>,
    run_len: &mut usize,
    out: &mut Vec<(RenderedRow, RowSource)>,
) {
    let Some(start) = run_start.take() else { return };
    let len = std::mem::take(run_len);
    match len {
        0 => {}
        1 => out.push((
            RenderedRow::Line {
                text: render_line(&entries[start]),
                altitude: entries[start].altitude,
            },
            RowSource::Single(start),
        )),
        n => {
            let run: Vec<&SessionTimelineEntry> = entries[start..start + n].iter().collect();
            out.push((
                RenderedRow::Collapsed { count: n, summary: collapsed_summary(&run) },
                RowSource::Run { start, len: n },
            ));
        }
    }
}

/// Brief breakdown of a collapsed run: the top event types by count. Sorted by
/// count desc, then name asc for deterministic output.
fn collapsed_summary(run: &[&SessionTimelineEntry]) -> String {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for e in run {
        *counts.entry(e.event_type.as_str()).or_insert(0) += 1;
    }
    let mut ordered: Vec<(&str, usize)> = counts.into_iter().collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let parts: Vec<String> = ordered
        .iter()
        .take(3)
        .map(|(k, c)| format!("{k}×{c}"))
        .collect();
    let more = if ordered.len() > 3 { ", …" } else { "" };
    format!("routine events ({}{})", parts.join(", "), more)
}

/// Multi-line detail view of a single event for the drill-down pane: metadata,
/// refs, and the pretty-printed payload. Pure (no I/O) and channel-neutral.
pub fn format_detail(entry: &SessionTimelineEntry) -> Vec<String> {
    let mut lines = vec![
        format!("event:     {}", entry.event_type),
        format!("at:        {}", entry.occurred_at),
        format!("altitude:  {}", entry.altitude.as_str()),
        format!(
            "actor:     {} ({})",
            entry.principal.id,
            entry.principal.kind.tag()
        ),
        format!("seat:      {}", role_label(&entry.role)),
    ];
    if let Some(turn) = &entry.turn_id {
        lines.push(format!("turn:      {turn}"));
    }
    lines.push(format!("event_id:  {}", entry.event_id));

    let refs = &entry.refs;
    let mut ref_parts: Vec<String> = Vec::new();
    let mut add = |label: &str, v: &Option<String>| {
        if let Some(s) = v {
            ref_parts.push(format!("{label}={s}"));
        }
    };
    add("causal", &refs.causal_event_id);
    add("trace", &refs.execution_trace_id);
    add("artifact", &refs.artifact_id);
    add("interaction", &refs.interaction_id);
    add("approval", &refs.approval_request_id);
    add("plan", &refs.plan_id);
    add("workbench", &refs.workbench_id);
    if !ref_parts.is_empty() {
        lines.push(format!("refs:      {}", ref_parts.join("  ")));
    }

    if let Some(payload) = &entry.payload {
        lines.push(String::new());
        lines.push("payload:".to_string());
        // Pretty-print if it parses as JSON; otherwise show raw.
        match serde_json::from_str::<serde_json::Value>(payload)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
        {
            Some(pretty) => lines.extend(pretty.lines().map(|l| format!("  {l}"))),
            None => lines.push(format!("  {payload}")),
        }
    }
    lines
}

/// Non-interactive rendering of a row. Borrows the existing line for `Line`
/// (no allocation on the hot path); only the collapsed form allocates.
pub fn row_text(row: &RenderedRow) -> std::borrow::Cow<'_, str> {
    match row {
        RenderedRow::Line { text, .. } => std::borrow::Cow::Borrowed(text),
        RenderedRow::Collapsed { count, summary } => std::borrow::Cow::Owned(format!(
            "{} ⟨{} {}⟩",
            altitude_glyph(Altitude::Detail),
            count,
            summary
        )),
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
        assert!(matches!(&rows[1], RenderedRow::Line { text, .. } if text.contains("approval requested")));
        // The trailing lone Detail event is a normal line, not collapsed.
        assert!(matches!(&rows[2], RenderedRow::Line { .. }));
        assert!(row_text(&rows[0]).contains("⟨3 routine events"));
    }

    #[test]
    fn coalesce_indexed_maps_rows_to_sources() {
        let mk = |et: &str, alt: Altitude| {
            entry(SessionRole::Planner, Principal::agent("planner.default"), et, alt, serde_json::json!({}))
        };
        let entries = vec![
            mk("turn.start", Altitude::Detail),   // 0 ┐ run
            mk("turn.end", Altitude::Detail),     // 1 ┘
            mk("approval.pending", Altitude::Attention), // 2 single
            mk("turn.start", Altitude::Detail),   // 3 lone detail → single
        ];
        let rows = coalesce_indexed(&entries);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].1, RowSource::Run { start: 0, len: 2 });
        assert_eq!(rows[1].1, RowSource::Single(2));
        assert_eq!(rows[2].1, RowSource::Single(3));
    }

    #[test]
    fn format_detail_includes_meta_refs_and_pretty_payload() {
        let mut e = entry(
            SessionRole::Operator,
            Principal::human("operator"),
            "approval.rejected",
            Altitude::Attention,
            serde_json::json!({ "request_id": "apr-9", "decided_by": "operator" }),
        );
        e.refs = TimelineRefs { approval_request_id: Some("apr-9".into()), ..Default::default() };
        let detail = format_detail(&e).join("\n");
        assert!(detail.contains("event:     approval.rejected"));
        assert!(detail.contains("actor:     operator (human)"));
        assert!(detail.contains("seat:      operator"));
        assert!(detail.contains("approval=apr-9"));
        assert!(detail.contains("\"request_id\": \"apr-9\""));
    }

    #[test]
    fn multiline_question_flattens_and_truncates_with_choices() {
        let long_q = "Pick a market:\n\n1. US equities\n2. Crypto\n".to_string() + &"x".repeat(200);
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "user.ask.pending",
            Altitude::Attention,
            serde_json::json!({
                "question": long_q,
                "options": [{"id": "o1", "label": "US equities"}, {"id": "o2", "label": "Crypto"}],
            }),
        );
        let line = render_line(&e);
        // One physical line: no embedded newlines, truncated with an ellipsis.
        assert!(!line.contains('\n'));
        assert!(line.contains('…'));
        // Pre-digested choices rendered inline and numbered.
        assert!(line.contains("[1] US equities"));
        assert!(line.contains("[2] Crypto"));
    }

    #[test]
    fn one_line_is_a_hard_cap_including_the_ellipsis() {
        let long = "abcdefghijklmnopqrstuvwxyz";
        let out = one_line(long, 10);
        assert_eq!(out.chars().count(), 10, "must not exceed max incl. ellipsis");
        assert!(out.ends_with('…'));
        // A string within the cap is returned untouched (no ellipsis).
        assert_eq!(one_line("short", 10), "short");
        // Whitespace (incl. newlines) collapses to single spaces.
        assert_eq!(one_line("a\n\n  b\tc", 50), "a b c");
        assert_eq!(one_line("anything", 0), "");
    }

    #[test]
    fn agent_narrative_renders_message_and_reasoning() {
        let msg = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": "I'll scan the repo,\nthen propose a plan." }),
        );
        let line = render_line(&msg);
        assert!(line.starts_with("▸"));
        assert!(line.contains("[planner]"));
        // Flattened to one line.
        assert!(line.contains("I'll scan the repo, then propose a plan."));

        let reasoning = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.reasoning",
            Altitude::Detail,
            serde_json::json!({ "reasoning": "the user wants periodic analysis" }),
        );
        let rline = render_line(&reasoning);
        assert!(rline.starts_with("·")); // Detail glyph — hidable
        assert!(rline.contains("💭 the user wants periodic analysis"));
    }

    #[test]
    fn operator_message_renders_with_human_label() {
        let e = entry(
            SessionRole::Operator,
            Principal::human("operator"),
            "operator.message",
            Altitude::Normal,
            serde_json::json!({ "message": "focus on US equities\nand crypto" }),
        );
        let line = render_line(&e);
        assert!(line.contains("🧑 operator"));
        // Flattened to one line, no embedded newline.
        assert!(line.contains("focus on US equities and crypto"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn tool_completed_uses_tool_name_field() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({ "tool_name": "Edit", "result": "ok" }),
        );
        assert!(render_line(&e).contains("tool Edit"));
    }

    #[test]
    fn llm_failure_links_preceding_action_chain() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "llm.request_failed",
            Altitude::Error,
            serde_json::json!({ "error": "rate limited", "preceding": ["read_file", "edit", "run"] }),
        );
        let line = render_line(&e);
        assert!(line.starts_with("✗"));
        assert!(line.contains("LLM error: rate limited"));
        assert!(line.contains("after: read_file → edit → run"));

        let bare = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "llm.request_failed",
            Altitude::Error,
            serde_json::json!({ "error": "boom" }),
        );
        assert!(!render_line(&bare).contains("after:"));
    }

    #[test]
    fn emergency_stop_renders_prominently_with_operator_label() {
        let e = entry(
            SessionRole::Operator,
            Principal::human("operator"),
            "session.emergency_stop",
            Altitude::Error,
            serde_json::json!({ "reason": "runaway tool loop", "stop_id": "estop-1234" }),
        );
        let line = render_line(&e);
        assert!(line.starts_with("✗"));
        assert!(line.contains("🧑 operator"));
        assert!(line.contains("EMERGENCY STOP — runaway tool loop"));
    }

    #[test]
    fn runtime_lock_drift_renders_blocked_and_overridden() {
        let mk = |overridden: bool, alt: Altitude| {
            entry(
                SessionRole::Runtime,
                Principal { kind: PrincipalKind::Script, id: "gateway".into() },
                "runtime.lock_drift",
                alt,
                serde_json::json!({ "drift_field": "binary_sha256", "override": overridden }),
            )
        };
        let blocked = render_line(&mk(false, Altitude::Error));
        assert!(blocked.starts_with("✗"));
        assert!(blocked.contains("runtime lock drift (binary_sha256) — execution blocked"));

        let overridden = render_line(&mk(true, Altitude::Attention));
        assert!(overridden.starts_with("⚠"));
        assert!(overridden.contains("overridden, running anyway"));
    }

    #[test]
    fn escalation_pending_renders_synthesis_and_revision() {
        let e = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::agent("coder.default"),
            "escalation.pending",
            Altitude::Attention,
            serde_json::json!({ "synthesis": "recommend promote", "revision_id": "rev-9" }),
        );
        let line = render_line(&e);
        assert!(line.starts_with("⚠"));
        assert!(line.contains("[coder]"));
        // Revision id shown as-is (already prefixed), not doubled to "rev rev-9".
        assert!(line.contains("escalation: recommend promote (rev-9)"));
    }

    #[test]
    fn sandbox_escape_renders_prominently() {
        let e = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::agent("coder.default"),
            "security.sandbox_escape",
            Altitude::Error,
            serde_json::json!({ "indicator": "ptrace syscall", "detail": "blocked" }),
        );
        let line = render_line(&e);
        assert!(line.starts_with("✗"));
        assert!(line.contains("[coder]"));
        assert!(line.contains("SANDBOX ESCAPE ATTEMPT — ptrace syscall"));
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
