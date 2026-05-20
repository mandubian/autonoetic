//! Compact session overview for tool-free divergence judgment (and other
//! consumers that want a single bounded text bundle per session).
//!
//! Reads existing tables and files — does **not** persist new state. The
//! shape is structured so the same data can be rendered as JSON for
//! dashboards or as compact Markdown for an LLM kickoff (~1-3 KB
//! typically). The Markdown render is the primary consumer today: the
//! sentinel validation experiment in `--no-tools` mode bundles the
//! overview into the watchdog's kickoff message so the watchdog can
//! produce a verdict in a single LLM completion (no tool calls).
//!
//! Data sources:
//! - `execution_traces` SQLite table → tool histogram + recent errors +
//!   turn-count estimate + wall-clock window
//! - `causal_events` SQLite table → divergence snapshot taken at the
//!   **highest level the session ever reached** (not its latest level).
//!   A session that briefly hit `critical` is still meaningfully
//!   critical even if its tail settled back to `watching`. See
//!   [`highest_divergence_snapshot`] for the picker.
//! - `{gateway_dir}/sessions/{root_session_id}/digest.md` on disk →
//!   trailing excerpt (last few turns of the narrative)
//!
//! All reads are best-effort: failures are coerced into the overview's
//! `notes` / `Option` fields rather than propagated, since this is a
//! diagnostic aggregator, not a transactional path.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;

use crate::runtime::live_digest::base_session_id;
use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::causal_chain::ExecutionTraceRecord;

/// Per-tool success/failure counts.
#[derive(Debug, Clone, Serialize)]
pub struct ToolStat {
    pub name: String,
    pub success_count: u32,
    pub failure_count: u32,
}

impl ToolStat {
    pub fn total(&self) -> u32 {
        self.success_count + self.failure_count
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorEntry {
    pub tool_name: String,
    pub error_type: Option<String>,
    pub error_summary: Option<String>,
    pub turn_id: Option<String>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectorySnapshot {
    pub highest_level: String,
    pub signals: Vec<TrajectorySignalSummary>,
    pub recorded_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectorySignalSummary {
    pub kind: String,
    pub severity: String,
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionOverview {
    pub session_id: String,
    pub root_session_id: String,
    /// Distinct turn_ids observed in `execution_traces` (lower bound on
    /// actual turn count — a turn that issued no tool calls won't appear).
    pub turn_count_estimate: u32,
    pub total_tool_calls: u32,
    pub total_errors: u32,
    /// `(end - start)` of the execution_traces window for this session.
    /// `None` when no traces exist.
    pub wall_clock_secs: Option<f64>,
    pub tool_histogram: Vec<ToolStat>,
    /// Most recent errors first. Capped at [`MAX_RECENT_ERRORS`].
    pub recent_errors: Vec<ErrorEntry>,
    pub trajectory_snapshot: Option<TrajectorySnapshot>,
    /// Tail of `digest.md` (last [`DIGEST_EXCERPT_BYTES`] bytes, cut at
    /// a Markdown heading boundary when one is available).
    pub digest_excerpt: Option<String>,
    /// Best-effort diagnostic notes (e.g., "digest.md not found"). Empty
    /// when everything resolved.
    pub notes: Vec<String>,
}

/// Cap on entries in `recent_errors`. Sized so the rendered list stays
/// under ~1 KB.
pub const MAX_RECENT_ERRORS: usize = 10;

/// Cap on entries in the rendered `tool_histogram` (most-frequent
/// retained).
pub const MAX_TOOLS_RENDERED: usize = 12;

/// Cap on the tail bytes pulled from `digest.md`.
pub const DIGEST_EXCERPT_BYTES: usize = 2048;

impl SessionOverview {
    /// Build an overview for `session_id`. Resolves the root session
    /// (everything before the first `/`) and aggregates all child-session
    /// traces under it.
    pub fn for_session(
        store: &Arc<GatewayStore>,
        gateway_dir: &Path,
        session_id: &str,
    ) -> Self {
        let root = base_session_id(session_id).to_string();
        let mut notes = Vec::new();

        // Traces: hits both the root and any nested child sessions via
        // `session_branch`'s `OR session_id LIKE 'root/%'` clause.
        let traces = match store.search_execution_traces(
            None, None, None, None, None, Some(&root), 1000,
        ) {
            Ok(t) => t,
            Err(e) => {
                notes.push(format!("execution_traces query failed: {}", e));
                Vec::new()
            }
        };

        let (turn_count_estimate, wall_clock_secs) = compute_turn_window(&traces);
        let tool_histogram = compute_tool_histogram(&traces);
        let total_tool_calls: u32 = tool_histogram.iter().map(|s| s.total()).sum();
        let total_errors: u32 = tool_histogram.iter().map(|s| s.failure_count).sum();
        let recent_errors = compute_recent_errors(&traces);

        let trajectory_snapshot = match store.search_causal_events(Some(&root), None, 500) {
            Ok(events) => highest_divergence_snapshot(&events),
            Err(e) => {
                notes.push(format!("causal_events query failed: {}", e));
                None
            }
        };

        let digest_excerpt = read_digest_excerpt(gateway_dir, &root, &mut notes);

        SessionOverview {
            session_id: session_id.to_string(),
            root_session_id: root,
            turn_count_estimate,
            total_tool_calls,
            total_errors,
            wall_clock_secs,
            tool_histogram,
            recent_errors,
            trajectory_snapshot,
            digest_excerpt,
            notes,
        }
    }

    /// Render as compact Markdown suitable for an LLM kickoff message.
    /// Typical output is ~1-3 KB. The order is fixed (matters for prompt
    /// stability) and tested below.
    pub fn render_markdown(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();

        let _ = writeln!(out, "## Session Overview: `{}`", self.session_id);
        if self.root_session_id != self.session_id {
            let _ = writeln!(out, "Root session: `{}`", self.root_session_id);
        }
        let wall = self.wall_clock_secs
            .map(|s| format!("{:.0}s", s))
            .unwrap_or_else(|| "—".to_string());
        let _ = writeln!(
            out,
            "Turns (lower bound): {} · Tool calls: {} · Errors: {} · Wall: {}\n",
            self.turn_count_estimate, self.total_tool_calls, self.total_errors, wall
        );

        // Tool histogram
        let _ = writeln!(out, "### Tool histogram");
        if self.tool_histogram.is_empty() {
            let _ = writeln!(out, "_(no tool calls recorded)_");
        } else {
            for stat in self.tool_histogram.iter().take(MAX_TOOLS_RENDERED) {
                let _ = writeln!(
                    out,
                    "- `{}`: {} calls, {} errors",
                    stat.name, stat.total(), stat.failure_count
                );
            }
            if self.tool_histogram.len() > MAX_TOOLS_RENDERED {
                let _ = writeln!(out,
                    "- _… {} more tools omitted_",
                    self.tool_histogram.len() - MAX_TOOLS_RENDERED);
            }
        }
        let _ = writeln!(out);

        // Recent errors
        let _ = writeln!(out, "### Recent errors (newest first, max {})", MAX_RECENT_ERRORS);
        if self.recent_errors.is_empty() {
            let _ = writeln!(out, "_(none)_");
        } else {
            for e in &self.recent_errors {
                let turn = e.turn_id.as_deref().unwrap_or("?");
                let err_type = e.error_type.as_deref().unwrap_or("unknown");
                let summary = e.error_summary.as_deref().unwrap_or("");
                let trimmed: String = summary.chars().take(200).collect();
                let _ = writeln!(
                    out,
                    "- `{}` · `{}` · `{}`: {}",
                    turn, e.tool_name, err_type, trimmed
                );
            }
        }
        let _ = writeln!(out);

        // Trajectory snapshot (Layer 1)
        let _ = writeln!(out, "### Layer 1 trajectory snapshot");
        match &self.trajectory_snapshot {
            Some(snap) => {
                let _ = writeln!(out, "Highest level reached: **{}**  (at {})", snap.highest_level, snap.recorded_at);
                for s in &snap.signals {
                    let ev = s.evidence.as_deref().unwrap_or("(no evidence string)");
                    let _ = writeln!(out, "- `{}` ({}) — {}", s.kind, s.severity, ev);
                }
            }
            None => {
                let _ = writeln!(out, "_no `divergence.*` events recorded for this session_");
            }
        }
        let _ = writeln!(out);

        // Digest excerpt
        let _ = writeln!(out, "### Digest excerpt (tail)");
        match &self.digest_excerpt {
            Some(text) => {
                let _ = writeln!(out, "```markdown");
                let _ = writeln!(out, "{}", text.trim_end());
                let _ = writeln!(out, "```");
            }
            None => {
                let _ = writeln!(out, "_no digest available_");
            }
        }
        let _ = writeln!(out);

        // Diagnostic notes — surface query failures so a reviewer knows
        // the overview is partial.
        if !self.notes.is_empty() {
            let _ = writeln!(out, "### Overview build notes");
            for n in &self.notes {
                let _ = writeln!(out, "- {}", n);
            }
            let _ = writeln!(out);
        }

        out
    }
}

fn compute_turn_window(traces: &[ExecutionTraceRecord]) -> (u32, Option<f64>) {
    let mut turn_ids = std::collections::BTreeSet::new();
    let mut min_ts: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut max_ts: Option<chrono::DateTime<chrono::Utc>> = None;
    for t in traces {
        if let Some(tid) = &t.turn_id {
            turn_ids.insert(tid.clone());
        }
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&t.timestamp) {
            let utc = parsed.with_timezone(&chrono::Utc);
            min_ts = Some(min_ts.map(|m| m.min(utc)).unwrap_or(utc));
            max_ts = Some(max_ts.map(|m| m.max(utc)).unwrap_or(utc));
        }
    }
    let wall = match (min_ts, max_ts) {
        (Some(a), Some(b)) if b > a => Some((b - a).num_milliseconds() as f64 / 1000.0),
        _ => None,
    };
    (turn_ids.len() as u32, wall)
}

fn compute_tool_histogram(traces: &[ExecutionTraceRecord]) -> Vec<ToolStat> {
    let mut by_name: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for t in traces {
        let entry = by_name.entry(t.tool_name.clone()).or_insert((0, 0));
        if t.success == 1 {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    }
    let mut stats: Vec<ToolStat> = by_name
        .into_iter()
        .map(|(name, (success_count, failure_count))| ToolStat {
            name,
            success_count,
            failure_count,
        })
        .collect();
    // Sort by total descending, then by failure count descending (so the
    // worst-failing tools stay at the top even on ties), then by name
    // ascending for deterministic output.
    stats.sort_by(|a, b| {
        b.total()
            .cmp(&a.total())
            .then_with(|| b.failure_count.cmp(&a.failure_count))
            .then_with(|| a.name.cmp(&b.name))
    });
    stats
}

fn compute_recent_errors(traces: &[ExecutionTraceRecord]) -> Vec<ErrorEntry> {
    // `traces` come back from the store ordered by timestamp DESC (see
    // `search_execution_traces`). Filter to failures, take the first N.
    traces
        .iter()
        .filter(|t| t.success != 1)
        .take(MAX_RECENT_ERRORS)
        .map(|t| ErrorEntry {
            tool_name: t.tool_name.clone(),
            error_type: t.error_type.clone(),
            error_summary: t.error_summary.clone(),
            turn_id: t.turn_id.clone(),
            timestamp: t.timestamp.clone(),
        })
        .collect()
}

fn highest_divergence_snapshot(
    events: &[autonoetic_types::causal_chain::CausalEventRecord],
) -> Option<TrajectorySnapshot> {
    // Events come back ordered by timestamp DESC. Pick the highest level
    // ever reached and use its signal payload as the snapshot evidence —
    // a session that hit `critical` once is still meaningfully divergent
    // even if its last event was `observed`.
    let mut best_rank: u8 = 0;
    let mut best: Option<&autonoetic_types::causal_chain::CausalEventRecord> = None;
    for e in events.iter().filter(|e| e.category == "divergence") {
        let level = e
            .payload
            .as_ref()
            .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
            .and_then(|v| v.get("level").and_then(|l| l.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| e.action.clone());
        let rank: u8 = match level.as_str() {
            "watching" => 1,
            "diverging" => 2,
            "critical" => 3,
            _ => 0,
        };
        if rank > best_rank {
            best_rank = rank;
            best = Some(e);
        }
    }
    let event = best?;
    let payload: serde_json::Value = event
        .payload
        .as_ref()
        .and_then(|p| serde_json::from_str(p).ok())
        .unwrap_or(serde_json::Value::Null);
    let level = payload
        .get("level")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| event.action.clone());
    let signals: Vec<TrajectorySignalSummary> = payload
        .get("signals")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|sig| {
                    let kind = sig.get("kind")?.as_str()?.to_string();
                    let severity = sig.get("severity")?.as_str()?.to_string();
                    let evidence = sig
                        .get("evidence")
                        .and_then(|e| e.as_str())
                        .map(|s| s.to_string());
                    Some(TrajectorySignalSummary { kind, severity, evidence })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(TrajectorySnapshot {
        highest_level: level,
        signals,
        recorded_at: event.timestamp.clone(),
    })
}

fn read_digest_excerpt(
    gateway_dir: &Path,
    root_session_id: &str,
    notes: &mut Vec<String>,
) -> Option<String> {
    let path = gateway_dir.join("sessions").join(root_session_id).join("digest.md");
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            notes.push(format!("digest.md not found at {}", path.display()));
            return None;
        }
    };

    if content.len() <= DIGEST_EXCERPT_BYTES {
        return Some(content);
    }

    // Take the trailing DIGEST_EXCERPT_BYTES bytes, then advance the
    // start forward to the next Markdown heading so the excerpt begins
    // at a clean section boundary rather than mid-sentence.
    //
    // `content.is_char_boundary` guards against splitting a multi-byte
    // UTF-8 codepoint when computing the start offset.
    let mut start = content.len().saturating_sub(DIGEST_EXCERPT_BYTES);
    while !content.is_char_boundary(start) && start < content.len() {
        start += 1;
    }
    let tail = &content[start..];
    // Prefer to start at the next `\n## ` or `\n### ` line if one
    // exists within the tail; otherwise keep the raw byte tail.
    let tail = if let Some(pos) = tail.find("\n## ").or_else(|| tail.find("\n### ")) {
        &tail[pos + 1..]
    } else {
        tail
    };
    Some(format!("…[truncated; {} earlier bytes omitted]\n\n{}", start, tail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::causal_chain::CausalEventRecord;

    fn trace(
        tool: &str,
        success: bool,
        err_type: Option<&str>,
        turn: &str,
        ts: &str,
    ) -> ExecutionTraceRecord {
        ExecutionTraceRecord {
            trace_id: "t".into(),
            event_id: None,
            agent_id: "a".into(),
            session_id: "s".into(),
            turn_id: Some(turn.to_string()),
            timestamp: ts.into(),
            tool_name: tool.into(),
            command: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            duration_ms: 100,
            success: if success { 1 } else { 0 },
            error_type: err_type.map(|s| s.to_string()),
            error_summary: err_type.map(|s| format!("{} happened", s)),
            approval_required: None,
            approval_request_id: None,
            arguments: None,
            result: None,
        }
    }

    // ── histogram + recent errors ──────────────────────────────────────

    #[test]
    fn histogram_groups_by_tool_and_sorts_by_total_desc() {
        let traces = vec![
            trace("web.search", true, None, "turn-1", "2026-05-20T10:00:00Z"),
            trace("web.search", false, Some("timeout"), "turn-2", "2026-05-20T10:01:00Z"),
            trace("web.search", false, Some("timeout"), "turn-3", "2026-05-20T10:02:00Z"),
            trace("sandbox.exec", true, None, "turn-1", "2026-05-20T10:00:00Z"),
        ];
        let hist = compute_tool_histogram(&traces);
        assert_eq!(hist.len(), 2);
        // web.search has 3 calls, sandbox.exec has 1.
        assert_eq!(hist[0].name, "web.search");
        assert_eq!(hist[0].success_count, 1);
        assert_eq!(hist[0].failure_count, 2);
        assert_eq!(hist[1].name, "sandbox.exec");
    }

    #[test]
    fn histogram_tie_breaks_by_failure_count_then_name() {
        // Two tools with same total → higher failure count wins; then alpha.
        let traces = vec![
            trace("alpha", true, None, "turn-1", "2026-05-20T10:00:00Z"),
            trace("alpha", true, None, "turn-2", "2026-05-20T10:01:00Z"),
            trace("zebra", false, Some("e"), "turn-1", "2026-05-20T10:00:00Z"),
            trace("zebra", true, None, "turn-2", "2026-05-20T10:01:00Z"),
        ];
        let hist = compute_tool_histogram(&traces);
        // zebra has 1 failure, alpha has 0 → zebra first.
        assert_eq!(hist[0].name, "zebra");
        assert_eq!(hist[1].name, "alpha");
    }

    #[test]
    fn recent_errors_filters_to_failures_and_caps() {
        let mut traces: Vec<ExecutionTraceRecord> = (0..15)
            .map(|i| trace("t", false, Some("e"), "turn", &format!("2026-05-20T10:{:02}:00Z", i)))
            .collect();
        traces.push(trace("ok", true, None, "turn", "2026-05-20T10:30:00Z"));
        let errs = compute_recent_errors(&traces);
        assert_eq!(errs.len(), MAX_RECENT_ERRORS);
        assert!(errs.iter().all(|e| e.tool_name == "t"));
    }

    // ── turn window ────────────────────────────────────────────────────

    #[test]
    fn turn_window_distinct_turns_and_wall_clock() {
        let traces = vec![
            trace("a", true, None, "turn-1", "2026-05-20T10:00:00Z"),
            trace("a", true, None, "turn-2", "2026-05-20T10:00:30Z"),
            trace("a", true, None, "turn-2", "2026-05-20T10:00:45Z"),
            trace("a", true, None, "turn-3", "2026-05-20T10:01:00Z"),
        ];
        let (turns, wall) = compute_turn_window(&traces);
        assert_eq!(turns, 3);
        assert!((wall.unwrap() - 60.0).abs() < 1e-6);
    }

    #[test]
    fn turn_window_handles_empty() {
        let (turns, wall) = compute_turn_window(&[]);
        assert_eq!(turns, 0);
        assert!(wall.is_none());
    }

    // ── divergence snapshot ────────────────────────────────────────────

    fn ev(action: &str, level: &str, ts: &str, signal_kind: &str) -> CausalEventRecord {
        CausalEventRecord {
            event_id: "e".into(),
            agent_id: "a".into(),
            session_id: "s".into(),
            turn_id: None,
            event_seq: 0,
            timestamp: ts.into(),
            category: "divergence".into(),
            action: action.into(),
            status: "SUCCESS".into(),
            enforced_rules: vec![],
            target: None,
            payload: Some(serde_json::json!({
                "level": level,
                "signals": [
                    {"kind": signal_kind, "severity": "warn", "current": 0.8,
                     "threshold": 0.8, "evidence": "tool X failed"}
                ]
            }).to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        }
    }

    #[test]
    fn snapshot_picks_highest_level_not_most_recent() {
        // Latest is `watching`, but session also hit `critical` earlier.
        // Snapshot should reflect `critical`.
        let events = vec![
            ev("observed", "watching", "2026-05-20T10:10:00Z", "loop_pressure"),
            ev("escalated", "critical", "2026-05-20T10:05:00Z", "failure_pressure"),
        ];
        let snap = highest_divergence_snapshot(&events).unwrap();
        assert_eq!(snap.highest_level, "critical");
        assert_eq!(snap.signals[0].kind, "failure_pressure");
    }

    #[test]
    fn snapshot_returns_none_with_no_divergence_events() {
        let events = vec![]; // Plus a non-divergence category would also be filtered.
        assert!(highest_divergence_snapshot(&events).is_none());
    }

    // ── render ─────────────────────────────────────────────────────────

    fn fixture_overview() -> SessionOverview {
        SessionOverview {
            session_id: "abc-123".into(),
            root_session_id: "abc-123".into(),
            turn_count_estimate: 12,
            total_tool_calls: 18,
            total_errors: 5,
            wall_clock_secs: Some(503.0),
            tool_histogram: vec![
                ToolStat { name: "web.search".into(), success_count: 2, failure_count: 3 },
                ToolStat { name: "sandbox.exec".into(), success_count: 3, failure_count: 1 },
            ],
            recent_errors: vec![
                ErrorEntry {
                    tool_name: "web.search".into(),
                    error_type: Some("timeout".into()),
                    error_summary: Some("request timed out".into()),
                    turn_id: Some("turn-008".into()),
                    timestamp: "2026-05-20T10:00:00Z".into(),
                },
            ],
            trajectory_snapshot: Some(TrajectorySnapshot {
                highest_level: "diverging".into(),
                signals: vec![TrajectorySignalSummary {
                    kind: "loop_pressure".into(),
                    severity: "warn".into(),
                    evidence: Some("4/5 cycles".into()),
                }],
                recorded_at: "2026-05-20T10:05:00Z".into(),
            }),
            digest_excerpt: Some("### turn-12 (planner)\nThe session is stuck.".into()),
            notes: vec![],
        }
    }

    #[test]
    fn render_markdown_contains_all_sections() {
        let md = fixture_overview().render_markdown();
        for needle in [
            "## Session Overview: `abc-123`",
            "Turns (lower bound): 12",
            "### Tool histogram",
            "`web.search`: 5 calls, 3 errors",
            "### Recent errors",
            "`turn-008`",
            "### Layer 1 trajectory snapshot",
            "Highest level reached: **diverging**",
            "### Digest excerpt (tail)",
            "```markdown",
            "The session is stuck.",
        ] {
            assert!(md.contains(needle), "missing `{}` in:\n{}", needle, md);
        }
    }

    #[test]
    fn render_markdown_size_is_bounded_for_typical_session() {
        let md = fixture_overview().render_markdown();
        // Sanity bound: a small overview should render under 2 KB. The
        // real cap depends on histogram/digest contents but this catches
        // accidental verbosity changes.
        assert!(md.len() < 2048, "overview rendered {} bytes; expected < 2048", md.len());
    }

    #[test]
    fn render_markdown_handles_empty_state_gracefully() {
        let empty = SessionOverview {
            session_id: "x".into(),
            root_session_id: "x".into(),
            turn_count_estimate: 0,
            total_tool_calls: 0,
            total_errors: 0,
            wall_clock_secs: None,
            tool_histogram: vec![],
            recent_errors: vec![],
            trajectory_snapshot: None,
            digest_excerpt: None,
            notes: vec!["digest.md not found".into()],
        };
        let md = empty.render_markdown();
        assert!(md.contains("_(no tool calls recorded)_"));
        assert!(md.contains("_(none)_"));
        assert!(md.contains("_no `divergence.*` events recorded for this session_"));
        assert!(md.contains("_no digest available_"));
        assert!(md.contains("digest.md not found"));
    }
}
