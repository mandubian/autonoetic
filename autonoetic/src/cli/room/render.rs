//! Pure rendering core for the Session Room (#363 P2).
//!
//! Channel-neutral: turns a `SessionTimelineEntry` into a one-line human string.
//! Deliberately free of any I/O or terminal state so the TUI shell, the CLI
//! viewer, and (later) external channel bridges all share the *same* formatting.
//! Presentation only — importance/altitude is decided gateway-side.

use std::collections::HashSet;

use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::session_timeline::{Altitude, SessionRole, SessionTimelineEntry};

/// Short agent id for spawn badges — `coder.default` → `coder`.
pub fn agent_id_short(agent_id: &str) -> &str {
    agent_id
        .strip_suffix(".default")
        .unwrap_or(agent_id)
        .split('.')
        .next()
        .unwrap_or(agent_id)
}

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
        PrincipalKind::ServedUser => format!("👤 {seat}"),
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

/// Truncation vocabulary — one rule for every cut in the room:
/// - a bare `…` means a headline/preview was compacted for the list *by design*
///   (`one_line`/`cap_preview`); nothing beyond it is meant to be read inline.
/// - `…(+N <unit> · Enter)` means real content was cut: the marker says how
///   much and names the key that shows the rest (Enter opens the drill-down
///   pane, which always carries the full payload).
/// Hard ceiling for narrative body text shown inline in the room list. The
/// detail pane (⏎) still shows the full payload; beyond this we add a
/// `…(+N chars · Enter)` marker.
const NARRATIVE_BODY_MAX: usize = 8_000;

/// Human-readable count for truncation markers: 950 → `950`, 12_340 → `12.3k`.
pub(crate) fn compact_count(n: usize) -> String {
    if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Try to parse a string that may be JSON — including gateway-truncated timeline
/// copies ending with `…`.
fn parse_jsonish_string(s: &str) -> Option<serde_json::Value> {
    let trimmed = s.trim();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return None;
    }
    if let Ok(v) = serde_json::from_str(trimmed) {
        return Some(v);
    }
    let without_ellipsis = trimmed.trim_end_matches('…').trim_end();
    if without_ellipsis != trimmed {
        if let Ok(v) = serde_json::from_str(without_ellipsis) {
            return Some(v);
        }
    }
    repair_truncated_json(without_ellipsis)
}

/// Whether a JSON object looks like an agent `io.returns` envelope.
fn looks_like_io_returns(v: &serde_json::Value) -> bool {
    v.as_object().is_some_and(|o| o.contains_key("status") || o.contains_key("summary"))
}

fn try_parse_io_returns(s: &str) -> Option<serde_json::Value> {
    let v = parse_jsonish_string(s).or_else(|| repair_truncated_json(s.trim()))?;
    looks_like_io_returns(&v).then_some(v)
}

/// Extract JSON from a markdown ``` fence when the model wraps its envelope.
fn extract_fenced_json(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut pos = 0;
    while pos < len {
        if bytes[pos] == b'`' && pos + 2 < len && &bytes[pos..pos + 3] == b"```" {
            pos += 3;
            while pos < len && bytes[pos] != b'\n' && bytes[pos] != b'\r' {
                pos += 1;
            }
            if pos < len && bytes[pos] == b'\r' {
                pos += 1;
            }
            if pos < len && bytes[pos] == b'\n' {
                pos += 1;
            }
            let content_start = pos;
            let mut search = content_start;
            while search < len {
                if bytes[search] == b'`'
                    && search + 2 < len
                    && &bytes[search..search + 3] == b"```"
                {
                    let content = s[content_start..search].trim();
                    if try_parse_io_returns(content).is_some() {
                        return Some(content.to_owned());
                    }
                    break;
                }
                search += 1;
            }
            pos = if search + 3 < len { search + 3 } else { len };
        } else {
            pos += 1;
        }
    }
    None
}

/// Positions of `{` that are not inside a JSON string literal.
fn string_aware_brace_positions(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    for (byte_idx, ch) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string && ch == '\\' {
            escape = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if !in_string && ch == '{' {
            out.push(byte_idx);
        }
    }
    out
}

/// Find the earliest `{` starting an `io.returns` object suffix.
fn find_io_returns_suffix_start(s: &str) -> Option<(usize, serde_json::Value)> {
    let mut candidates = Vec::new();
    for sep in ["\n\n{", "\r\n\r\n{", "\n{", "\r\n{"] {
        if let Some(idx) = s.rfind(sep) {
            candidates.push(idx + sep.len() - 1);
        }
    }
    for (i, _) in s.match_indices("\n{").chain(s.match_indices("\r\n{")) {
        candidates.push(i + 1);
    }
    candidates.extend(string_aware_brace_positions(s));
    candidates.sort_unstable();
    candidates.dedup();
    for i in candidates {
        if let Some(v) = try_parse_io_returns(&s[i..]) {
            return Some((i, v));
        }
    }
    None
}

/// Normalize a raw `message` string into either an `io.returns` object (with
/// optional `prose` lead-in) or the original string.
fn coerce_message_string(s: &str) -> serde_json::Value {
    if let Some(v) = try_parse_io_returns(s) {
        return v;
    }
    let (prose, structured) = split_embedded_json_tail(s);
    if let Some(mut obj) = structured.and_then(|v| v.as_object().cloned()) {
        if !prose.is_empty() {
            obj.insert("prose".to_string(), serde_json::Value::String(prose));
        }
        return serde_json::Value::Object(obj);
    }
    // Fallback: the string looks like an io.returns envelope but has unescaped
    // newlines inside string values (common with smaller LLMs). Try to salvage
    // the `summary` field so the operator sees formatted prose, not raw JSON.
    if s.starts_with('{') {
        if let Some(summary) = extract_summary_from_broken_json(s) {
            return serde_json::json!({ "status": "ok", "summary": summary });
        }
    }
    serde_json::Value::String(s.to_string())
}

/// Heuristic extraction of the `"summary"` value from a JSON object that has
/// unescaped newlines in string values (making it unparseable by serde_json).
/// Looks for `"summary":` and reads until the matching closing quote.
fn extract_summary_from_broken_json(s: &str) -> Option<String> {
    let key_pos = s.find("\"summary\"")?;
    let after_key = &s[key_pos + "\"summary\"".len()..];
    let colon = after_key.trim_start();
    let after_colon = colon.strip_prefix(':')?;
    let after_space = after_colon.trim_start();
    let rest = after_space.strip_prefix('"')?;
    // Read until the closing quote: look for `"` followed by `}` or `,` or
    // end-of-string. Since the JSON is broken, we have to be lenient — take
    // everything until the last `"` that's followed by `}` or EOF.
    let raw = rest;
    // Find the last occurrence of `"` followed by `}` (the closing of the object)
    let mut end = raw.len();
    for (i, ch) in raw.char_indices().rev() {
        if ch == '"' {
            let after = &raw[i + 1..];
            if after.trim_start().starts_with('}') || after.trim_start().is_empty() {
                end = i;
                break;
            }
        }
    }
    let summary = &raw[..end];
    if summary.is_empty() {
        None
    } else {
        Some(summary.to_string())
    }
}

/// Split agent text that may lead with prose and end with an embedded JSON object.
fn split_embedded_json_tail(s: &str) -> (String, Option<serde_json::Value>) {
    let trimmed = s.trim();
    if let Some(v) = try_parse_io_returns(trimmed) {
        return (String::new(), Some(v));
    }
    if let Some(fenced) = extract_fenced_json(trimmed) {
        if let Some(v) = try_parse_io_returns(&fenced) {
            let prose = trimmed
                .split("```")
                .next()
                .unwrap_or(trimmed)
                .trim()
                .to_string();
            return (prose, Some(v));
        }
    }
    if let Some((i, v)) = find_io_returns_suffix_start(trimmed) {
        return (trimmed[..i].trim().to_string(), Some(v));
    }
    (trimmed.to_string(), None)
}

/// Expand `agent.message` payloads where `message` is prose + trailing JSON text.
fn expand_agent_message_payload(v: &serde_json::Value) -> serde_json::Value {
    let Some(msg) = v.get("message") else {
        return v.clone();
    };
    match msg {
        serde_json::Value::String(s) => {
            let mut out = v.as_object().cloned().unwrap_or_default();
            out.insert("message".to_string(), coerce_message_string(s));
            serde_json::Value::Object(out)
        }
        serde_json::Value::Object(_) => v.clone(),
        _ => v.clone(),
    }
}

/// List-row projection when `message` contains lead prose plus structured JSON.
fn structured_with_lead_prose(
    obj: &serde_json::Map<String, serde_json::Value>,
    lead_prose: &str,
) -> (String, Option<String>) {
    let lead = lead_prose.trim();
    if is_plan_proposal_object(obj) {
        let (mut plan_headline, mut plan_detail) = plan_proposal_preview(obj);
        if !lead.is_empty() {
            if lead.contains('\n') || lead.chars().count() > 120 {
                let mut parts = vec![preserve_lines(lead, NARRATIVE_BODY_MAX)];
                if let Some(d) = plan_detail {
                    parts.push(d);
                }
                plan_detail = Some(parts.join("\n\n"));
            } else {
                plan_headline = one_line(lead, 120);
            }
        }
        return (plan_headline, plan_detail);
    }
    let headline = if lead.contains('\n') || lead.chars().count() > 120 {
        String::new()
    } else if lead.is_empty() {
        String::new()
    } else {
        one_line(lead, 120)
    };
    let mut detail_parts = Vec::new();
    if !lead.is_empty() {
        detail_parts.push(preserve_lines(lead, NARRATIVE_BODY_MAX));
    }
    if let Some(sub) = structured_subline(obj) {
        detail_parts.push(sub);
    }
    if let Some(s) = obj.get("summary").and_then(|v| v.as_str()) {
        let title = summary_title(s);
        if let Some(body) = summary_detail_body(s, &title) {
            detail_parts.push(body);
        }
    }
    let detail = if detail_parts.is_empty() {
        None
    } else {
        Some(detail_parts.join("\n\n"))
    };
    (headline, detail)
}

/// Read a payload field as JSON — accepts an embedded object/array or a JSON
/// string; plain prose strings are returned as [`Value::String`].
fn payload_field_json(p: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    let v = p.get(key)?;
    match v {
        serde_json::Value::String(s) if s.is_empty() => None,
        serde_json::Value::String(s) if key == "message" => Some(coerce_message_string(s)),
        serde_json::Value::String(s) => Some(
            parse_jsonish_string(s).unwrap_or_else(|| serde_json::Value::String(s.clone())),
        ),
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => Some(v.clone()),
        _ => None,
    }
}

/// Short list-row title from a summary — only explicit `#` markdown headings.
/// Everything else renders as formatted body (models often glue section titles
/// onto the first sentence without newlines).
fn summary_title(s: &str) -> String {
    for line in s.lines() {
        let t = line.trim();
        if t.starts_with("### ")
            || t.starts_with("## ")
            || t.starts_with("# ")
        {
            return one_line(t.trim_start_matches('#').trim(), 72);
        }
    }
    String::new()
}

/// Remaining summary prose for the list body (after the title line).
fn summary_detail_body(s: &str, headline: &str) -> Option<String> {
    if headline.is_empty() {
        let body = s.trim();
        return if body.is_empty() {
            None
        } else {
            Some(preserve_lines(body, NARRATIVE_BODY_MAX))
        };
    }
    if !s.contains('\n') && s.chars().count() <= headline.chars().count().saturating_add(40) {
        return None;
    }
    let mut lines: Vec<&str> = s.lines().collect();
    while lines
        .first()
        .map(|l| l.trim().trim_start_matches('#').trim().is_empty())
        .unwrap_or(false)
    {
        lines.remove(0);
    }
    if lines.first().is_some_and(|l| {
        one_line(l.trim().trim_start_matches('#').trim(), 240) == headline
    }) {
        lines.remove(0);
    }
    while lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    let body = lines.join("\n").trim().to_string();
    if body.is_empty() {
        None
    } else {
        Some(preserve_lines(&body, NARRATIVE_BODY_MAX))
    }
}

/// Render a scalar JSON value as a short inline fact.
fn scalar_fact_preview(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => {
            let preview = one_line(s, 80);
            if preview.is_empty() {
                None
            } else {
                Some(preview)
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Flat string/scalar facts from a structured `result` for the list sub-line.
/// Nested objects/arrays collapse to `(+N fields)` / `[N]` — never full JSON.
fn result_prose_line(result: Option<&serde_json::Value>) -> Option<String> {
    let obj = result?.as_object()?;
    let mut parts = Vec::new();
    if let Some(id) = obj.get("agent_id").and_then(|v| v.as_str()) {
        parts.push(format!("agent: {id}"));
    }
    for (k, v) in obj {
        if k == "agent_id" {
            continue;
        }
        match v {
            serde_json::Value::Object(map) => parts.push(format!("{k} (+{} fields)", map.len())),
            serde_json::Value::Array(arr) => parts.push(format!("{k}[{}]", arr.len())),
            other => {
                if let Some(preview) = scalar_fact_preview(other) {
                    parts.push(format!("{k}: {preview}"));
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn plan_id_from_proposal(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    obj.get("plan_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            obj.get("result")
                .and_then(|r| r.get("plan_id"))
                .and_then(|v| v.as_str())
        })
        .map(str::to_string)
}

fn is_plan_proposal_object(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    obj.get("status").and_then(|v| v.as_str()) == Some("awaiting_approval")
        && plan_id_from_proposal(obj).is_some()
}

/// Target agent id for an `agent_spawn` tool row (`tool.requested` or
/// `tool.completed`), when present in the payload.
pub fn agent_spawn_agent_id(entry: &SessionTimelineEntry) -> Option<String> {
    let p = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .map(|v| {
            if let Some(s) = v.as_str() {
                serde_json::from_str::<serde_json::Value>(s).unwrap_or(v)
            } else {
                v
            }
        })?;
    let tool = p
        .get("tool_name")
        .or_else(|| p.get("tool"))
        .and_then(|v| v.as_str())?;
    if tool != "agent_spawn" {
        return None;
    }
    match entry.event_type.as_str() {
        "tool.requested" => {
            let args = p.get("arguments")?;
            let args_v = if let Some(s) = args.as_str() {
                serde_json::from_str::<serde_json::Value>(s).ok()?
            } else {
                args.clone()
            };
            args_v
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        }
        "tool.completed" => agent_id_from_spawn_result(
            p.get("result")
                .or_else(|| p.get("message"))
                .or_else(|| p.get("summary")),
        ),
        _ => None,
    }
}

/// The declared egress output floor of a spawn's target agent (#971), when the
/// spawn has completed and the gateway surfaced it in the result payload. Used
/// to mark the spawn row with the bundle's own output restriction (e.g.
/// `local_only`) so an operator sees that a delegation went to a local-only
/// bundle. `None` for a pending spawn (`tool.requested`) or an unrestricted /
/// unreadable bundle — distinct from the runtime taint marker (■), which is the
/// spawn *result's* live label, not the bundle's declaration.
pub fn agent_spawn_output_label(entry: &SessionTimelineEntry) -> Option<String> {
    let p = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .map(|v| {
            if let Some(s) = v.as_str() {
                serde_json::from_str::<serde_json::Value>(s).unwrap_or(v)
            } else {
                v
            }
        })?;
    let tool = p
        .get("tool_name")
        .or_else(|| p.get("tool"))
        .and_then(|v| v.as_str())?;
    if tool != "agent_spawn" || entry.event_type != "tool.completed" {
        return None;
    }
    let result = p
        .get("result")
        .or_else(|| p.get("message"))
        .or_else(|| p.get("summary"))?;
    let obj = if let Some(s) = result.as_str() {
        serde_json::from_str::<serde_json::Value>(s).ok()?
    } else {
        result.clone()
    };
    obj.get("target_egress_output_label")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The `message_id` of an agent message row, for correlating it with the label
/// `labels.list`'s `agent_messages` section reports (#971 proposal 2). Message
/// rows are `agent.peer_message` events (agent-to-agent traffic, which is what
/// carries the sender's accumulated egress taint at send time — RFC §5.5);
/// their `message_id` is in the payload, matching `LabeledMessageRow.message_id`.
pub fn agent_message_id(entry: &SessionTimelineEntry) -> Option<String> {
    if entry.event_type != "agent.peer_message" {
        return None;
    }
    let p = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .map(|v| {
            if let Some(s) = v.as_str() {
                serde_json::from_str::<serde_json::Value>(s).unwrap_or(v)
            } else {
                v
            }
        })?;
    p.get("message_id").and_then(|v| v.as_str()).map(str::to_string)
}

fn agent_id_from_spawn_result(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    let parsed = if let Some(s) = value.as_str() {
        serde_json::from_str::<serde_json::Value>(s)
            .unwrap_or_else(|_| serde_json::Value::String(s.to_string()))
    } else {
        value.clone()
    };
    if let Some(id) = parsed.get("agent_id").and_then(|v| v.as_str()) {
        return Some(id.to_string());
    }
    parsed
        .as_str()
        .and_then(agent_id_from_spawn_summary_text)
}

/// Parse test-scenario / digest summaries like `spawned coder.default for s1`.
fn agent_id_from_spawn_summary_text(s: &str) -> Option<String> {
    let rest = s.trim().strip_prefix("spawned ")?;
    let agent = rest.split_whitespace().next()?;
    (!agent.is_empty()).then(|| agent.to_string())
}

/// Extract a pending plan id from an agent/operator message payload, if present.
pub(crate) fn extract_plan_proposal_id(entry: &SessionTimelineEntry) -> Option<String> {
    if entry.event_type != "agent.message" && entry.event_type != "operator.message" {
        return None;
    }
    let p = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())?;
    let msg_v = payload_field_json(&p, "message")?;
    plan_proposal_id_from_value(&msg_v)
}

fn plan_proposal_id_from_value(v: &serde_json::Value) -> Option<String> {
    if let Some(obj) = v.as_object() {
        return if is_plan_proposal_object(obj) {
            plan_id_from_proposal(obj)
        } else {
            None
        };
    }
    if let Some(text) = v.as_str() {
        let (_, structured) = split_embedded_json_tail(text);
        if let Some(obj) = structured.as_ref().and_then(|v| v.as_object()) {
            if is_plan_proposal_object(obj) {
                return plan_id_from_proposal(obj);
            }
        }
    }
    None
}

/// Compact plan-proposal card for list rows (no raw JSON envelope).
fn plan_proposal_preview(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> (String, Option<String>) {
    let plan_id = plan_id_from_proposal(obj).unwrap_or_default();
    let version = obj
        .get("version")
        .and_then(|v| v.as_u64())
        .map(|v| format!("v{v}"))
        .unwrap_or_default();
    let step_count = obj
        .get("result")
        .and_then(|r| r.get("steps"))
        .and_then(|v| v.as_u64())
        .or_else(|| {
            obj.get("steps")
                .and_then(|v| v.as_array())
                .map(|a| a.len() as u64)
        });
    let title = obj
        .get("summary")
        .and_then(|s| s.as_str())
        .and_then(|s| s.lines().find(|l| !l.trim().is_empty()))
        .map(|l| one_line(l.trim(), 72))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("Plan {plan_id}"));

    let headline = format!("📋 PLAN AWAITING APPROVAL — {title}");
    let mut lines = vec![format!(
        "plan: {plan_id}{}",
        if version.is_empty() {
            String::new()
        } else {
            format!(" · {version}")
        }
    )];
    if let Some(n) = step_count {
        lines.push(format!("  {n} steps"));
    }
    if let Some(fix) = obj
        .get("result")
        .and_then(|r| r.get("fix"))
        .and_then(|v| v.as_str())
    {
        lines.push(format!("  fix: {}", one_line(fix, 160)));
    }
    if let Some(next) = obj
        .get("result")
        .and_then(|r| r.get("next_step"))
        .and_then(|v| v.as_str())
    {
        lines.push(format!("  next: {}", one_line(next, 120)));
    }
    if let Some(summary) = obj.get("summary").and_then(|v| v.as_str()) {
        if let Some(body) = summary_detail_body(summary, "") {
            lines.push(body);
        }
    }
    lines.push("  ↳ y approve · Enter/p review · /plan for steps".to_string());
    (headline, Some(lines.join("\n")))
}

/// Status chip + flat result facts (+ error/plan_id when present) for list sub-lines.
fn structured_subline(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(status) = obj.get("status").and_then(|v| v.as_str()) {
        parts.push(format!("[{status}]"));
    }
    if let Some(plan_id) = plan_id_from_proposal(obj) {
        parts.push(format!("plan: {plan_id}"));
    }
    if let Some(err) = obj.get("error").and_then(|v| v.as_str()) {
        let preview = one_line(err, 100);
        if !preview.is_empty() {
            parts.push(format!("error: {preview}"));
        }
    }
    if is_plan_proposal_object(obj) {
        return if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        };
    }
    if let Some(result) = result_prose_line(obj.get("result")) {
        parts.push(result);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// List-row projection for structured agent messages (`summary`/`result`,
/// install intents, child notifications). Returns `(headline, detail)` where
/// the headline is human-readable and detail is an optional compact sketch.
fn structured_object_preview(obj: &serde_json::Map<String, serde_json::Value>) -> Option<(String, Option<String>)> {
    if let Some(notif_msg) = obj
        .get("message")
        .and_then(|v| v.as_str())
        .filter(|_| obj.get("type").is_some())
    {
        return Some((
            String::new(),
            Some(preserve_lines(notif_msg, NARRATIVE_BODY_MAX)),
        ));
    }
    if let Some(reason) = obj.get("reason").and_then(|v| v.as_str()) {
        let headline = one_line(reason, 240);
        let detail = obj
            .get("artifact_ref")
            .and_then(|v| v.as_str())
            .map(|r| format!("artifact: {r}"));
        return Some((headline, detail));
    }
    if is_plan_proposal_object(obj) {
        return Some(plan_proposal_preview(obj));
    }
    let summary_raw = obj.get("summary").and_then(|v| v.as_str());
    let headline = if let Some(s) = summary_raw {
        summary_title(s)
    } else if let Some(status) = obj.get("status").and_then(|v| v.as_str()) {
        format!("status: {status}")
    } else {
        return None;
    };
    let mut detail_parts = Vec::new();
    if let Some(sub) = structured_subline(obj) {
        detail_parts.push(sub);
    }
    if let Some(s) = summary_raw {
        if let Some(body) = summary_detail_body(s, &headline) {
            detail_parts.push(body);
        }
    }
    let detail = if detail_parts.is_empty() {
        None
    } else {
        Some(detail_parts.join("\n\n"))
    };
    Some((headline, detail))
}

fn parse_entry_payload(entry: &SessionTimelineEntry) -> Option<serde_json::Value> {
    entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
}

fn payload_field_str(p: &serde_json::Value, key: &str) -> Option<String> {
    p.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Wrap plain text to `max_width` display cells (Unicode-aware).
/// Preserves intentional newlines — each source line is wrapped
/// independently, and blank lines produce blank output lines.
pub fn wrap_display_lines(text: &str, max_width: usize) -> Vec<String> {
    let width = max_width.max(1);
    let mut lines: Vec<String> = Vec::new();

    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_w = 0usize;
        for word in paragraph.split_whitespace() {
            let word_w = unicode_width::UnicodeWidthStr::width(word);
            let extra = if current.is_empty() { word_w } else { word_w + 1 };
            if current_w + extra > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_w = 0;
            }
            if !current.is_empty() {
                current.push(' ');
                current_w += 1;
            }
            current.push_str(word);
            current_w += word_w;
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// High-visibility approval gate card (`approval.pending`).
fn approval_gate_card(entry: &SessionTimelineEntry) -> (String, Option<String>) {
    let p = parse_entry_payload(entry);
    let field = |key: &str| p.as_ref().and_then(|v| payload_field_str(v, key));
    let request_id = field("request_id")
        .or_else(|| entry.refs.approval_request_id.clone())
        .unwrap_or_default();
    let action = field("action").unwrap_or_else(|| "approval".into());
    let level = field("approval_level");
    let headline = if action == "revision_promote" {
        let agent = field("agent_id").unwrap_or_else(|| "agent".into());
        format!("⏸ PROMOTION APPROVAL — {agent}")
    } else {
        format!("⏸ APPROVAL REQUIRED — {}", one_line(&action, 72))
    };
    let mut lines = vec![format!("  request: {request_id}")];
    if let Some(lvl) = level {
        lines.push(format!("  level: {lvl}"));
    }
    if action == "revision_promote" {
        if let Some(agent) = field("agent_id") {
            lines.push(format!("  agent: {agent}"));
        }
        if let Some(rev) = field("revision_id") {
            lines.push(format!("  revision: {}", one_line(&rev, 120)));
        }
        // Declared egress output floor of the candidate (#971): the bundle's own
        // output restriction, surfaced so an operator can see they are admitting
        // a local-only bundle (the case the issue calls out as invisible). Absent
        // ⇒ unrestricted / not declared.
        if let Some(label) = field("output_label") {
            lines.push(format!("  output floor: {label}"));
        }
        if let Some(summary) = field("summary") {
            lines.push("  about:".to_string());
            for line in wrap_display_lines(&summary, 76) {
                lines.push(format!("    {line}"));
            }
        }
        for (label, key) in [
            ("added capabilities", "added_capabilities"),
            ("broadened capabilities", "broadened_capabilities"),
        ] {
            if let Some(values) = p
                .as_ref()
                .and_then(|v| v.get(key))
                .and_then(|v| v.as_array())
            {
                let joined: Vec<_> = values
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect();
                if !joined.is_empty() {
                    lines.push(format!("  {label}:"));
                    for cap in joined {
                        lines.push(format!("    · {cap}"));
                    }
                }
            }
        }
    }
    if action == "credential_prompt" {
        if let Some(service) = field("service") {
            lines.push(format!("  service: {service}"));
        }
        if let Some(fields) = p
            .as_ref()
            .and_then(|v| v.get("secret_fields"))
            .and_then(|v| v.as_array())
        {
            let joined: Vec<_> = fields.iter().filter_map(|v| v.as_str()).collect();
            if !joined.is_empty() {
                lines.push(format!("  asks for: {}", joined.join(", ")));
            }
        }
        // #1105: secret entry AND egress scope are approved by the same card —
        // the scope must be visible, and a wildcard must be unmistakable.
        if let Some(hosts) = p
            .as_ref()
            .and_then(|v| v.get("allowed_hosts"))
            .and_then(|v| v.as_array())
        {
            let joined: Vec<_> = hosts.iter().filter_map(|v| v.as_str()).collect();
            lines.push("  egress scope (allowed_hosts):".to_string());
            if joined.is_empty() {
                lines.push("    (none declared — requests will not be host-bound)".to_string());
            } else {
                for h in &joined {
                    if *h == "*" {
                        lines.push(
                            "    · * — WILDCARD: the secret can be sent to ANY host".to_string(),
                        );
                    } else {
                        lines.push(format!("    · {h}"));
                    }
                }
            }
            lines.push(
                "  note: approving secret entry also grants this egress scope for the credential's lifetime"
                    .to_string(),
            );
        }
    }
    if let Some(cmd) = field("command") {
        if field("command_kind").as_deref() == Some("content_ref") {
            lines.push(format!("  command: {} (content ref)", one_line(&cmd, 120)));
            if let Some(hint) = field("command_hint") {
                lines.push(format!("  note: {}", one_line(&hint, 140)));
            }
        } else {
            lines.push(format!("  command: {}", one_line(&cmd, 140)));
        }
    }
    if let Some(intent) = field("intent") {
        lines.push("  purpose:".to_string());
        for line in wrap_display_lines(&intent, 76) {
            lines.push(format!("    {line}"));
        }
    }
    if action != "revision_promote" {
        if let Some(summary) = field("summary") {
            lines.push("  details:".to_string());
            for line in wrap_display_lines(&summary, 76) {
                lines.push(format!("    {line}"));
            }
        }
    }
    let has_grant_hosts = {
        let hosts = p
            .as_ref()
            .and_then(|v| v.get("host_patterns"))
            .and_then(|v| v.as_array());
        hosts.is_some_and(|arr| {
            arr.iter().any(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        })
    };
    if let Some(hosts) = p
        .as_ref()
        .and_then(|v| v.get("host_patterns"))
        .and_then(|v| v.as_array())
    {
        let joined: Vec<_> = hosts
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        if !joined.is_empty() {
            lines.push(format!("  hosts: {}", joined.join(", ")));
            lines.push(format!(
                "  grant: {} for this session",
                if has_grant_hosts {
                    "hosts will be pre-approved"
                } else {
                    "no network hosts"
                }
            ));
        }
    }
    if let Some(risk) = field("risk_summary") {
        lines.push(format!("  risk: {}", one_line(&risk, 120)));
    }
    if field("confirm_phrase").is_some() {
        lines.push("  ↳ y approve (confirm phrase shown below) · n reject · Esc peek timeline".to_string());
    } else if has_grant_hosts {
        lines.push("  ↳ y approve+grant · o approve once · n reject · Esc peek timeline".to_string());
    } else {
        lines.push("  ↳ y approve · n reject · Esc peek timeline".to_string());
    }
    (headline, Some(lines.join("\n")))
}

/// High-visibility clarification gate card (`user.ask.pending`).
fn interaction_gate_card(entry: &SessionTimelineEntry) -> (String, Option<String>) {
    let p = parse_entry_payload(entry);
    let field = |key: &str| p.as_ref().and_then(|v| payload_field_str(v, key));
    let interaction_id = field("interaction_id")
        .or_else(|| entry.refs.interaction_id.clone())
        .unwrap_or_default();
    let question = field("question").unwrap_or_else(|| "operator input needed".into());
    let headline = format!("❓ CLARIFICATION — {}", one_line(&question, 88));
    let mut lines = vec![format!("  ask: {interaction_id}")];
    if let Some(opts) = p
        .as_ref()
        .and_then(|v| v.get("options"))
        .and_then(|v| v.as_array())
    {
        for (i, o) in opts.iter().enumerate() {
            if let Some(label) = o.get("label").and_then(|l| l.as_str()) {
                lines.push(format!("  [{}] {}", i + 1, one_line(label, 80)));
            }
        }
    }
    let freeform = p
        .as_ref()
        .and_then(|v| v.get("allow_freeform"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if let Some(ctx) = field("context").filter(|c| !c.is_empty()) {
        lines.push("  context:".to_string());
        for line in wrap_display_lines(&ctx, 76) {
            lines.push(format!("    {line}"));
        }
    }
    if freeform {
        lines.push("  (or type your own answer)".to_string());
    }
    lines.push("  ↳ Enter/i/r to answer · 1–9 pick option".to_string());
    (headline, Some(lines.join("\n")))
}

/// High-visibility plan gate card (`plan.pending`).
fn plan_gate_card(entry: &SessionTimelineEntry) -> (String, Option<String>) {
    let p = parse_entry_payload(entry);
    let field = |key: &str| p.as_ref().and_then(|v| payload_field_str(v, key));
    let plan_id = field("plan_id")
        .or_else(|| entry.refs.plan_id.clone())
        .unwrap_or_default();
    let title = field("title").unwrap_or_default();
    let version = p
        .as_ref()
        .and_then(|v| v.get("version"))
        .and_then(|x| x.as_u64())
        .map(|v| format!("v{v}"))
        .unwrap_or_default();
    let reason = field("reason").filter(|r| !r.trim().is_empty());
    let step_count = p
        .as_ref()
        .and_then(|v| v.get("steps"))
        .and_then(|v| v.as_array())
        .map(|a| a.len());
    let headline = if title.is_empty() {
        format!("📋 PLAN AWAITING APPROVAL — {plan_id}")
    } else {
        format!("📋 PLAN AWAITING APPROVAL — {title}")
    };
    let mut lines = vec![format!(
        "  plan: {plan_id}{}",
        if version.is_empty() {
            String::new()
        } else {
            format!(" · {version}")
        }
    )];
    if let Some(n) = step_count {
        lines.push(format!("  {n} steps"));
    }
    if let Some(r) = reason {
        lines.push(format!("  reason: {}", one_line(&r, 120)));
    }
    if let Some(obj) = field("objective") {
        lines.push(format!("  objective: {}", one_line(&obj, 120)));
    }
    lines.push("  ↳ y approve · Enter/p review · n request changes".to_string());
    (headline, Some(lines.join("\n")))
}

/// True when this `approval.pending` is a federation-escalation mirror.
/// `federation.escalate` emits both `escalation.pending` (the canonical verdict
/// card) and an `approval.pending` with `action: session_escalate` (a resolvable
/// mirror). We always suppress the mirror so the operator only sees one gate.
fn is_session_escalate_mirror(entry: &SessionTimelineEntry) -> bool {
    if entry.event_type != "approval.pending" {
        return false;
    }
    let request_id = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v.get("request_id").and_then(|x| x.as_str()).map(str::to_string))
        .or_else(|| entry.refs.approval_request_id.clone());
    request_id
        .as_ref()
        .is_some_and(|id| id.starts_with("apr-esc-"))
}

/// High-visibility session-escalate gate card (`approval.pending` + session_escalate).
fn session_escalate_gate_card(entry: &SessionTimelineEntry) -> (String, Option<String>) {
    let p = parse_entry_payload(entry);
    let field = |key: &str| p.as_ref().and_then(|v| payload_field_str(v, key));
    let request_id = field("request_id")
        .or_else(|| entry.refs.approval_request_id.clone())
        .unwrap_or_default();
    let reason_field = field("reason");
    let summary_field = field("summary");
    let reason = reason_field
        .clone()
        .or_else(|| summary_field.clone())
        .unwrap_or_else(|| "agent needs human guidance".into());
    let headline = format!("⏸ SESSION ESCALATION — {}", one_line(&reason, 88));
    let mut lines = vec![format!("  request: {request_id}")];
    lines.push("  reason:".to_string());
    for line in wrap_display_lines(&reason, 76) {
        lines.push(format!("    {line}"));
    }
    if let Some(agent) = field("requested_by_agent_id") {
        lines.push(format!("  requested by: {agent}"));
    }
    if let Some(u) = field("urgency") {
        lines.push(format!("  urgency: {u}"));
    }
    if let Some(sid) = field("session_id") {
        lines.push(format!("  session: {sid}"));
    }
    if let Some(ctx) = field("context").filter(|c| !c.is_empty()) {
        lines.push("  context:".to_string());
        for line in wrap_display_lines(&ctx, 76) {
            lines.push(format!("    {line}"));
        }
    }
    if let Some(summary) = summary_field.filter(|s| {
        !s.is_empty() && reason_field.as_ref().map(String::as_str) != Some(s.as_str())
    }) {
        lines.push("  summary:".to_string());
        for line in wrap_display_lines(&summary, 76) {
            lines.push(format!("    {line}"));
        }
    }
    if let Some(actions) = p
        .as_ref()
        .and_then(|v| v.get("suggested_actions"))
        .and_then(|v| v.as_array())
    {
        let joined: Vec<_> = actions
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        if !joined.is_empty() {
            lines.push(format!("  suggested: {}", joined.join(" · ")));
        }
    }
    lines.push("  ↳ y approve (resume with your guidance) · n reject · Esc peek timeline".to_string());
    (headline, Some(lines.join("\n")))
}

/// High-visibility federation/promotion escalation gate card (`escalation.pending`).
fn escalation_gate_card(entry: &SessionTimelineEntry) -> (String, Option<String>) {
    let p = parse_entry_payload(entry);
    let field = |key: &str| p.as_ref().and_then(|v| payload_field_str(v, key));
    let synthesis_raw = field("synthesis").unwrap_or_else(|| "operator decision requested".into());
    let synthesis_plain = if super::markdown::looks_like_markdown(&synthesis_raw) {
        super::markdown::strip_markdown(&synthesis_raw)
    } else {
        synthesis_raw.clone()
    };
    let headline = format!("⏸ PROMOTION ESCALATION — {}", one_line(&synthesis_plain, 88));
    let mut lines = Vec::new();
    if let Some(id) = field("escalation_id") {
        lines.push(format!("  escalation: {id}"));
    }
    if let Some(req) = entry.refs.approval_request_id.clone().or(field("request_id")) {
        lines.push(format!("  approval: {req}"));
    }
    if let Some(agent) = field("agent_id") {
        lines.push(format!("  agent: {agent}"));
    }
    if let Some(rev) = field("revision_id").filter(|r| !r.is_empty()) {
        lines.push(format!("  revision: {}", one_line(&rev, 120)));
    }
    if let Some(kind) = field("escalation_type") {
        lines.push(format!("  type: {kind}"));
    }
    if let Some(artifact) = entry.refs.artifact_id.clone() {
        lines.push(format!("  artifact: {artifact}"));
    }
    lines.push("  synthesis:".to_string());
    for line in wrap_display_lines(&synthesis_plain, 76) {
        lines.push(format!("    {line}"));
    }
    lines.push("  ↳ y approve · n reject · Esc peek timeline".to_string());
    (headline, Some(lines.join("\n")))
}

/// High-visibility wiki proposal gate card (approval.pending with action=wiki_propose).
fn wiki_proposal_gate_card(entry: &SessionTimelineEntry) -> (String, Option<String>) {
    let p = parse_entry_payload(entry);
    let field = |key: &str| p.as_ref().and_then(|v| payload_field_str(v, key));
    let request_id = field("request_id")
        .or_else(|| entry.refs.approval_request_id.clone())
        .unwrap_or_default();
    let title = field("title").unwrap_or_default();
    let page_id = field("page_id").unwrap_or_default();
    let headline = format!("📝 WIKI PROPOSAL — {}", one_line(&title, 88));
    let mut lines = vec![format!("  page: {page_id}")];
    lines.push(format!("  request: {request_id}"));
    if let Some(sha) = field("content_sha256") {
        lines.push(format!("  content_sha256: {sha}"));
    }
    if let Some(tags_str) = p
        .as_ref()
        .and_then(|v| v.get("tags"))
        .and_then(|v| v.as_array())
    {
        let joined: Vec<_> = tags_str
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        if !joined.is_empty() {
            lines.push(format!("  tags: {}", joined.join(", ")));
        }
    }
    lines.push("  ↳ y approve · n reject".to_string());
    (headline, Some(lines.join("\n")))
}

/// Card for wiki lifecycle events (wiki.proposed, wiki.rejected).
fn wiki_lifecycle_card(entry: &SessionTimelineEntry, heading: &str) -> (String, Option<String>) {
    let p = parse_entry_payload(entry);
    let field = |key: &str| p.as_ref().and_then(|v| payload_field_str(v, key));
    let title = field("title").unwrap_or_default();
    let page_id = field("page_id").unwrap_or_default();
    let headline = format!("{heading} — {}", one_line(&title, 88));
    let mut lines = vec![format!("  page: {page_id}")];
    if let Some(agent) = field("proposed_by_agent") {
        lines.push(format!("  proposed by: {agent}"));
    }
    if let Some(by) = field("decided_by").or_else(|| field("cancelled_by")) {
        lines.push(format!("  decided by: {by}"));
    }
    if let Some(reason) = field("reason").filter(|r| !r.trim().is_empty()) {
        lines.push(format!("  reason: {}", one_line(&reason, 120)));
    }
    (headline, Some(lines.join("\n")))
}

/// Build list-row headline + detail for an agent/operator message event.
pub(crate) fn message_list_rows(entry: &SessionTimelineEntry, key: &str) -> (String, Option<String>) {
    let Some(p) = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
    else {
        return (summarize(entry), None);
    };
    let Some(msg_v) = payload_field_json(&p, key) else {
        return (summarize(entry), None);
    };
    if let Some(obj) = msg_v.as_object() {
        if let Some(prose) = obj.get("prose").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            return structured_with_lead_prose(obj, prose);
        }
        if let Some(rows) = structured_object_preview(obj) {
            return rows;
        }
    }
    if let Some(text) = msg_v.as_str() {
        if text.is_empty() {
            return (summarize(entry), None);
        }
        let (prose, structured) = split_embedded_json_tail(text);
        if let Some(v) = structured {
            if let Some(obj) = v.as_object() {
                if !prose.is_empty() {
                    return structured_with_lead_prose(obj, &prose);
                }
                if let Some(rows) = structured_object_preview(obj) {
                    return rows;
                }
            }
        }
        if let Some(parsed) = parse_jsonish_string(text) {
            if let Some(obj) = parsed.as_object() {
                if let Some(rows) = structured_object_preview(obj) {
                    return rows;
                }
            }
        }
        let body = preserve_lines(text, NARRATIVE_BODY_MAX);
        return (String::new(), Some(body));
    }
    (summarize(entry), None)
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

/// Extract a human-readable summary from a tool result payload. The `result`
/// field may be a plain string, a stringified JSON object with a `summary` key,
/// or a structured JSON object. Returns the `summary` field if found.
fn extract_tool_summary(p: Option<&serde_json::Value>) -> Option<String> {
    let p = p?;
    let p = if let Some(s) = p.as_str() {
        serde_json::from_str::<serde_json::Value>(s).unwrap_or_else(|_| p.clone())
    } else {
        p.clone()
    };
    if let Some(s) = p.get("summary").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    // Check inside "message" field (tool.completed wraps result in message)
    let inner = p
        .get("message")
        .cloned()
        .unwrap_or(p.clone());
    // Unfold if message is a string containing JSON
    let inner = if let Some(s) = inner.as_str() {
        serde_json::from_str::<serde_json::Value>(s).unwrap_or(inner)
    } else {
        inner
    };
    if let Some(s) = inner.get("summary").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(result) = inner.get("result") {
        if let Some(s) = result.get("summary").and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
        if let Some(s) = result.as_str() {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                if let Some(summary) = parsed.get("summary").and_then(|v| v.as_str()) {
                    return Some(summary.to_string());
                }
            }
        }
    }
    None
}

/// Extract the key argument from a `tool.requested` or `tool.completed` payload.
/// For `tool.requested` the `arguments` field is a JSON string; for `tool.completed`
/// there is an `args_preview` field written by the tracer.
fn extract_tool_key_param(p: &Option<serde_json::Value>, tool_name: &str) -> Option<String> {
    let v = p.as_ref()?;
    // tool.completed may have a pre-extracted args_preview
    if let Some(preview) = v.get("args_preview").and_then(|x| x.as_str()) {
        return Some(preview.to_string());
    }
    // tool.requested has arguments as a JSON string
    let args_str = v.get("arguments")?.as_str()?;
    let args: serde_json::Value = serde_json::from_str(args_str).ok()?;
    let key = match tool_name {
        "artifact_inspect" => args.get("artifact_ref").and_then(|x| x.as_str()),
        "content_write" | "content_patch" => args.get("name").and_then(|x| x.as_str()),
        "agent_spawn" => args.get("agent_id").and_then(|x| x.as_str()),
        "sandbox_exec" => args.get("command").and_then(|x| x.as_str()),
        "artifact_exec" => args.get("entrypoint").and_then(|x| x.as_str()),
        _ => return None,
    };
    key.map(|s| {
        if s.len() > 80 {
            format!("{}…", &s[..79])
        } else {
            s.to_string()
        }
    })
}

/// Headline for an `agent_spawn` tool row. Spawns are structural events (a
/// child agent is launched), so they get a distinctive `⑂ spawn → <id>` line
/// instead of the generic `tool agent_spawn (<id>)` — the fork glyph and arrow
/// read as "delegated to a child" at a glance, and the child's task goes on the
/// `↳` detail line (see `detail_preview`). When the target declares an egress
/// output floor (#971) it is appended (`· local_only`) so an operator sees that
/// the delegation went to a local-only bundle.
fn spawn_headline(agent_id: Option<&str>, output_label: Option<&str>) -> String {
    let base = match agent_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => format!("⑂ spawn → {id}"),
        None => "⑂ spawn child agent".to_string(),
    };
    match output_label
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "unrestricted")
    {
        Some(label) => format!("{base} · {label}"),
        None => base,
    }
}

/// Headline for `sandbox_exec` / `artifact_exec` rows. Leads with `▶` and the
/// command (sandbox) or `<entrypoint> <args> · <artifact_ref>` (artifact) so the
/// operator sees *what ran* at a glance; the result/output lands on the `↳`
/// detail line (see the exec arm of `detail_preview`).
fn exec_headline(preview: Option<&str>) -> String {
    match preview.map(str::trim).filter(|s| !s.is_empty()) {
        Some(cmd) => format!("▶ {cmd}"),
        None => "▶ run".to_string(),
    }
}

/// Parsed `result` object from a `tool.completed` payload (`result` may be a
/// JSON object or a JSON-encoded string).
fn tool_completed_result_object(p: &serde_json::Value) -> Option<serde_json::Value> {
    p.get("result").and_then(|r| match r {
        serde_json::Value::String(raw) => serde_json::from_str(raw).ok(),
        serde_json::Value::Object(_) => Some(r.clone()),
        _ => None,
    })
}

/// View model for a `promotion_record` tool result — gateway response shape from
/// `PromotionRecordTool` (plus legacy test payloads with flat `role`/`findings`).
struct PromotionRecordView {
    tool_ok: bool,
    pass: bool,
    role: String,
    artifact_hint: Option<String>,
    execution_trace_id: Option<String>,
    findings: Vec<serde_json::Value>,
    roles_recorded: Vec<(String, bool)>,
    error: Option<String>,
}

fn is_promotion_record_tool(tool_name: &str) -> bool {
    matches!(tool_name, "promotion_record" | "promotion.record")
}

fn promotion_role_label(entry: &SessionTimelineEntry, rec: Option<&serde_json::Value>) -> String {
    if let SessionRole::Specialist { kind } = &entry.role {
        return kind.clone();
    }
    if let SessionRole::Auditor = &entry.role {
        return "auditor".to_string();
    }
    if let Some(rec) = rec {
        let slots: [(&str, &str, &str); 5] = [
            ("evaluator", "evaluator_id", "evaluator_timestamp"),
            ("auditor", "auditor_id", "auditor_timestamp"),
            ("static_evaluator", "static_evaluator_id", "static_evaluator_timestamp"),
            ("unit_test_runner", "unit_test_runner_id", "unit_test_runner_timestamp"),
            ("sealed_evaluator", "sealed_evaluator_id", "sealed_evaluator_timestamp"),
        ];
        let mut latest: Option<(String, String)> = None;
        for (role, id_key, ts_key) in slots {
            if rec.get(id_key).and_then(|v| v.as_str()).is_some() {
                let ts = rec
                    .get(ts_key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if latest.as_ref().map(|(_, t)| ts > t.as_str()).unwrap_or(true) {
                    latest = Some((role.to_string(), ts.to_string()));
                }
            }
        }
        if let Some((role, _)) = latest {
            return role;
        }
    }
    role_label(&entry.role)
}

fn promotion_findings_from_value(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v.get("findings")
        .and_then(|f| f.as_array())
        .map(|a| a.clone())
        .unwrap_or_default()
}

fn promotion_roles_recorded(rec: &serde_json::Value) -> Vec<(String, bool)> {
    let slots: [(&str, &str, &str); 5] = [
        ("evaluator", "evaluator_id", "evaluator_pass"),
        ("auditor", "auditor_id", "auditor_pass"),
        ("static_evaluator", "static_evaluator_id", "static_evaluator_pass"),
        ("unit_test_runner", "unit_test_runner_id", "unit_test_runner_pass"),
        ("sealed_evaluator", "sealed_evaluator_id", "sealed_evaluator_pass"),
    ];
    let mut out = Vec::new();
    for (role, id_key, pass_key) in slots {
        if rec.get(id_key).and_then(|v| v.as_str()).is_some() {
            let pass = rec.get(pass_key).and_then(|v| v.as_bool()).unwrap_or(false);
            out.push((role.to_string(), pass));
        }
    }
    out
}

fn promotion_record_view(entry: &SessionTimelineEntry, p: &serde_json::Value) -> Option<PromotionRecordView> {
    let result = tool_completed_result_object(p)?;
    let rec = result.get("promotion_record").filter(|v| v.is_object());
    let tool_ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
    let pass = result
        .get("pass")
        .and_then(|v| v.as_bool())
        .or_else(|| rec.and_then(|r| r.get("pass").and_then(|v| v.as_bool())))
        .unwrap_or(false);
    let role = result
        .get("role")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| promotion_role_label(entry, rec));
    let artifact_hint = entry
        .refs
        .artifact_id
        .clone()
        .or_else(|| {
            rec.and_then(|r| r.get("artifact_ref").and_then(|v| v.as_str()).map(str::to_string))
        })
        .or_else(|| {
            result
                .get("artifact_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let execution_trace_id = result
        .get("execution_trace_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let findings = {
        let mut findings = promotion_findings_from_value(&result);
        if let Some(rec) = rec {
            let role_key = format!("{role}_findings");
            if let Some(arr) = rec.get(&role_key).and_then(|f| f.as_array()) {
                findings.extend(arr.clone());
            }
        }
        findings
    };
    let roles_recorded = rec.map(promotion_roles_recorded).unwrap_or_default();
    let error = result
        .get("error")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(PromotionRecordView {
        tool_ok,
        pass,
        role,
        artifact_hint,
        execution_trace_id,
        findings,
        roles_recorded,
        error,
    })
}

fn short_promotion_artifact(id: &str) -> String {
    let trimmed = id.trim();
    if trimmed.len() <= 20 {
        return trimmed.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("art_") {
        let short: String = rest.chars().take(12).collect();
        return format!("art_{short}…");
    }
    format!("{}…", trimmed.chars().take(16).collect::<String>())
}

fn short_trace_id(id: &str) -> String {
    let trimmed = id.trim();
    if trimmed.len() <= 12 {
        return trimmed.to_string();
    }
    format!("{}…", trimmed.chars().take(8).collect::<String>())
}

fn promotion_finding_counts(findings: &[serde_json::Value]) -> String {
    let mut critical = 0usize;
    let mut error = 0usize;
    let mut warning = 0usize;
    let mut info = 0usize;
    for f in findings {
        match f.get("severity").and_then(|v| v.as_str()) {
            Some("critical") => critical += 1,
            Some("error") => error += 1,
            Some("warning") => warning += 1,
            Some("info") | None => info += 1,
            _ => info += 1,
        }
    }
    let mut parts = Vec::new();
    if critical > 0 {
        parts.push(format!("{critical} critical"));
    }
    if error > 0 {
        parts.push(format!("{error} error"));
    }
    if warning > 0 {
        parts.push(format!("{warning} warning"));
    }
    if info > 0 {
        parts.push(format!("{info} info"));
    }
    if parts.is_empty() {
        "no findings".to_string()
    } else {
        parts.join(", ")
    }
}

fn promotion_record_headline(view: &PromotionRecordView) -> String {
    if !view.tool_ok {
        return "✗ promotion record rejected".to_string();
    }
    let verdict = if view.pass { "PASS" } else { "FAIL" };
    let glyph = if view.pass { "✓" } else { "✗" };
    format!("{glyph} promotion {verdict} · {role}", role = view.role)
}

fn promotion_record_detail(view: &PromotionRecordView) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(err) = &view.error {
        lines.push(format!("error: {}", one_line(err, 160)));
    }
    if let Some(art) = &view.artifact_hint {
        lines.push(format!("artifact: {}", short_promotion_artifact(art)));
    }
    if let Some(trace) = &view.execution_trace_id {
        lines.push(format!("execution trace: {}", short_trace_id(trace)));
    }
    if !view.findings.is_empty() {
        lines.push(format!("findings: {}", promotion_finding_counts(&view.findings)));
        for f in view
            .findings
            .iter()
            .filter(|f| {
                matches!(
                    f.get("severity").and_then(|v| v.as_str()),
                    Some("critical") | Some("error") | Some("warning")
                )
            })
            .take(3)
        {
            let sev = f
                .get("severity")
                .and_then(|v| v.as_str())
                .unwrap_or("finding");
            let desc = f
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("(no description)");
            lines.push(format!("  [{sev}] {}", one_line(desc, 120)));
        }
    }
    if !view.roles_recorded.is_empty() {
        let slots: Vec<String> = view
            .roles_recorded
            .iter()
            .map(|(role, pass)| {
                let mark = if *pass { "✓" } else { "✗" };
                format!("{role} {mark}")
            })
            .collect();
        lines.push(format!("registry: {}", slots.join(" · ")));
    }
    if lines.is_empty() {
        if view.pass {
            "verdict recorded — no additional detail".to_string()
        } else {
            "verdict recorded — promotion blocked".to_string()
        }
    } else {
        lines.join("\n")
    }
}

fn promotion_record_tone(entry: &SessionTimelineEntry) -> Option<RowTone> {
    if entry.event_type != "tool.completed" {
        return None;
    }
    let p = parse_entry_payload(entry)?;
    let tool = p
        .get("tool_name")
        .or_else(|| p.get("tool"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !is_promotion_record_tool(tool) {
        return None;
    }
    let view = promotion_record_view(entry, &p)?;
    if view.tool_ok && view.pass {
        Some(RowTone::VerdictPass)
    } else {
        Some(RowTone::VerdictFail)
    }
}

/// Headline for `content_write` / `content_patch` rows: `✎ wrote <path>` so a
/// file creation reads as an action with its target, not a buried `(name)` suffix.
fn write_headline(verb: &str, path: Option<&str>) -> String {
    match path.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => format!("✎ {verb} {p}"),
        None => format!("✎ {verb} file"),
    }
}

/// Human summary of an event, from its type + payload. Keeps the most useful
/// field per known event type; falls back to the bare event type.
pub fn summarize(entry: &SessionTimelineEntry) -> String {
    let p = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .map(|v| {
            // Unfold double-encoded JSON (payload stored as a string containing JSON)
            if let Some(s) = v.as_str() {
                serde_json::from_str::<serde_json::Value>(s).unwrap_or(v)
            } else {
                v
            }
        });
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
        // `user.ask` gate lifecycle (#363): paired resolution rows for the
        // `user.ask.pending` clarification gate. The Room stops re-offering the
        // ask once it sees these (folded into `resolved` in `record_timeline_resolution`).
        "user.ask.resolved" => {
            let answer = field("answer_text")
                .or_else(|| field("answer_option_id"))
                .unwrap_or_default();
            format!("✓ answered ask ({}): {}", field("interaction_id").unwrap_or_default(), one_line(&answer, 60))
        }
        "user.ask.cancelled" => format!(
            "✗ ask cancelled ({}){}",
            field("interaction_id").unwrap_or_default(),
            field("reason").map(|r| format!(": {}", one_line(&r, 60))).unwrap_or_default()
        ),
        "user.ask.expired" => format!(
            "⌛ ask expired ({})",
            field("interaction_id").unwrap_or_default()
        ),
        "plan.pending" => format!("plan proposed: {}", field("title").unwrap_or_default()),
        "plan.approved" => format!("plan approved ({})", field("plan_id").unwrap_or_default()),
        "plan.withdrawn" => format!(
            "plan withdrawn v{} (superseded by v{})",
            field("version").unwrap_or_else(|| "?".into()),
            field("superseded_by").unwrap_or_else(|| "?".into()),
        ),
        "wiki.proposed" => format!("wiki proposed: {} ({})", field("title").unwrap_or_default(), field("page_id").unwrap_or_default()),
        "wiki.promoted" => format!("wiki promoted: {} ({})", field("title").unwrap_or_default(), field("page_id").unwrap_or_default()),
        "wiki.rejected" => format!("wiki rejected: {} — {}", field("title").unwrap_or_default(), field("reason").unwrap_or_else(|| "no reason".into())),
        "wiki.withdrawn" => format!("wiki withdrawn: {} ({})", field("title").unwrap_or_default(), field("page_id").unwrap_or_default()),
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
        "operator.message" => one_line(&field("message").unwrap_or_default(), 80),
        // The agent's own narrative (#367 P4): what it says, and (hidable) its
        // reasoning — so a turn reads intent → actions → result. Actor label
        // shows which agent; the 💭 marks reasoning as the "why".
        "agent.message" => {
            let msg = field("message").unwrap_or_default();
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&msg) {
                if let Some(s) = parsed.get("summary").and_then(|v| v.as_str()) {
                    one_line(s, 160)
                } else {
                    let plain = if super::markdown::looks_like_markdown(&msg) {
                        super::markdown::strip_markdown(&msg)
                    } else {
                        msg
                    };
                    one_line(&plain, 80)
                }
            } else {
                let plain = if super::markdown::looks_like_markdown(&msg) {
                    super::markdown::strip_markdown(&msg)
                } else {
                    msg
                };
                one_line(&plain, 80)
            }
        }
        // Agent-to-agent direct message (#971 proposal 2). The actor label
        // already names the sender; this renders the (redacted) body, prefixed
        // with a glyph so peer traffic reads as traffic, and the row's
        // message_id keys the egress label marker.
        "agent.peer_message" => {
            let msg = one_line(&field("message").unwrap_or_default(), 80);
            if msg.is_empty() {
                "⇄ peer message".to_string()
            } else {
                format!("⇄ {msg}")
            }
        }
        "agent.reasoning" => format!("💭 {}", one_line(&field("reasoning").unwrap_or_default(), 160)),
        "user.ask.pending" => format!(
            "asks: {}",
            one_line(&field("question").unwrap_or_default(), 200),
        ),
        // Payload key is `tool_name`; keep `tool` as a fallback for older rows.
        // Show the result summary — status is conveyed by altitude color.
        // For tool.completed, also include the key argument (args_preview) when
        // available so the row tells you what artifact/file/agent was involved.
        "tool.requested" => {
            let tool_name = field("tool_name").unwrap_or_default();
            if tool_name == "agent_spawn" {
                return spawn_headline(
                    agent_spawn_agent_id(entry)
                        .or_else(|| extract_tool_key_param(&p, &tool_name))
                        .as_deref(),
                    agent_spawn_output_label(entry).as_deref(),
                );
            }
            let kp = extract_tool_key_param(&p, &tool_name);
            match tool_name.as_str() {
                "sandbox_exec" | "artifact_exec" => return exec_headline(kp.as_deref()),
                "content_write" => return write_headline("write", kp.as_deref()),
                "content_patch" => return write_headline("patch", kp.as_deref()),
                _ => {}
            }
            match kp {
                Some(kp) => format!("{tool_name} → {kp}"),
                None => format!("{tool_name} requested"),
            }
        }
        "tool.completed" => {
            let tool_name = field("tool_name")
                .or_else(|| field("tool"))
                .unwrap_or_else(|| "completed".into());
            // Spawns get a distinctive headline instead of the generic
            // `tool agent_spawn (<id>)`; the child's task lands on the detail line.
            if tool_name == "agent_spawn" {
                return spawn_headline(
                    agent_spawn_agent_id(entry)
                        .or_else(|| extract_tool_key_param(&p, &tool_name))
                        .as_deref(),
                    agent_spawn_output_label(entry).as_deref(),
                );
            }
            let key_param = extract_tool_key_param(&p, &tool_name);
            // Action-first headlines: `▶ <command>` for exec, `✎ wrote <path>`
            // for file mutations. Output/result stays on the detail line.
            match tool_name.as_str() {
                "sandbox_exec" | "artifact_exec" => {
                    return exec_headline(key_param.as_deref())
                }
                "content_write" => return write_headline("wrote", key_param.as_deref()),
                "content_patch" => return write_headline("patched", key_param.as_deref()),
                "promotion_record" | "promotion.record" => {
                    if let Some(ref pv) = p {
                        if let Some(view) = promotion_record_view(entry, pv) {
                            return promotion_record_headline(&view);
                        }
                    }
                }
                _ => {}
            }
            let summary = extract_tool_summary(p.as_ref());
            let key_suffix = key_param
                .map(|kp| format!(" ({kp})"))
                .unwrap_or_default();
            match summary {
                Some(s) => {
                    let plain = if super::markdown::looks_like_markdown(&s) {
                        super::markdown::strip_markdown(&s)
                    } else {
                        s
                    };
                    let base = one_line(&plain, 160);
                    format!("{base}{key_suffix}")
                }
                None => format!("tool {tool_name}{key_suffix}"),
            }
        }
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
        "workflow.child_state" => {
            let status = field("child_status").unwrap_or_else(|| "unknown".into());
            let task = field("task_id").map(|t| format!(" · {t}")).unwrap_or_default();
            format!("workflow: child {status}{task}")
        }
        "workflow.join_satisfied" => {
            let wf = field("workflow_id").map(|w| format!(" ({w})")).unwrap_or_default();
            format!("workflow: join satisfied{wf}")
        }
        "workflow.signal" => one_line(
            &field("message").unwrap_or_else(|| "workflow signal".into()),
            100,
        ),
        "scheduled_job.triggered" => format!(
            "scheduled call → {}",
            field("agent_id").unwrap_or_else(|| "agent".into())
        ),
        "scheduled_job.completed" => format!(
            "scheduled result [{}]",
            field("agent_id").unwrap_or_else(|| "agent".into())
        ),
        "scheduled_job.failed" => format!(
            "scheduled failed [{}]",
            field("agent_id").unwrap_or_else(|| "agent".into())
        ),
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
        "llm.empty_response" => {
            let model = field("model").unwrap_or_default();
            let stop = field("stop_reason").unwrap_or_default();
            let in_tok = p.as_ref().and_then(|v| v.get("input_tokens")).and_then(|x| x.as_u64()).unwrap_or(0);
            let out_tok = p.as_ref().and_then(|v| v.get("output_tokens")).and_then(|x| x.as_u64()).unwrap_or(0);
            format!(
                "LLM empty response: model={model} stop={stop} tokens={in_tok}/{out_tok}{}",
                preceding_chain(p.as_ref()),
            )
        }
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
        // Bounded-progress (loop-guard) trip terminated the session (#287/P-7.x).
        // The actor label shows which agent; the enforcing rule rides on refs
        // (first-class) — fall back to the payload `rule_id` only for older
        // events written before `enforced_rules` existed.
        "guard.tripped" => {
            let reason = field("reason").unwrap_or_else(|| "tripped".into());
            let rule = if !entry.refs.enforced_rules.is_empty() {
                format!(" [{}]", entry.refs.enforced_rules.join(", "))
            } else {
                field("rule_id").map(|r| format!(" [{r}]")).unwrap_or_default()
            };
            format!("loop guard tripped: {reason}{rule}")
        }
        "session.start" => format!(
            "session started{}",
            field("trigger_type").map(|t| format!(" ({t})")).unwrap_or_default()
        ),
        "session.end" => format!(
            "session ended: {}",
            one_line(&field("reason").unwrap_or_else(|| "no reason".into()), 100)
        ),
        "security.escape_threshold" => {
            let level = field("level").unwrap_or_default();
            let count = p.as_ref().and_then(|v| v.get("count")).and_then(|x| x.as_u64()).unwrap_or(0);
            let threshold = p.as_ref().and_then(|v| v.get("threshold")).and_then(|x| x.as_u64()).unwrap_or(0);
            format!("escape threshold reached: {count}/{threshold} attempts ({level})")
        }
        // Operator commented on a live file (#XXX) — Attention: an issue the
        // agent should address. Location + body so the row reads as a
        // sentence, not the bare event type.
        "operator.comment" => {
            let name = field("name").unwrap_or_default();
            let line_start = p.as_ref().and_then(|v| v.get("line_start")).and_then(|x| x.as_u64());
            let line_end = p.as_ref().and_then(|v| v.get("line_end")).and_then(|x| x.as_u64());
            let loc = match (line_start, line_end) {
                (Some(s), Some(e)) if s != e => format!(":{s}-{e}"),
                (Some(s), _) => format!(":{s}"),
                _ => String::new(),
            };
            format!("comment on {name}{loc}: {}", one_line(&field("body").unwrap_or_default(), 100))
        }
        "digest_annotate" => format!(
            "{}: {}",
            field("type").unwrap_or_else(|| "note".into()),
            one_line(&field("content").unwrap_or_default(), 140)
        ),
        "workflow.started" => format!(
            "workflow started{}",
            field("lead_agent_id").map(|a| format!(" (lead: {a})")).unwrap_or_default()
        ),
        "workflow.completed" => {
            let n = p
                .as_ref()
                .and_then(|v| v.get("join_task_ids"))
                .and_then(|x| x.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("workflow completed ({n} joined task{})", if n == 1 { "" } else { "s" })
        }
        "envelope.proposed" => {
            let hosts = p
                .as_ref()
                .and_then(|v| v.get("hosts"))
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            if hosts.is_empty() {
                "network envelope proposed".into()
            } else {
                format!("network envelope proposed: {hosts}")
            }
        }
        "envelope.locked" => {
            let hosts = p
                .as_ref()
                .and_then(|v| v.get("hosts"))
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            let grants = p.as_ref().and_then(|v| v.get("grants_materialized")).and_then(|x| x.as_u64()).unwrap_or(0);
            let grant_word = if grants == 1 { "grant" } else { "grants" };
            if hosts.is_empty() {
                format!("network envelope locked ({grants} {grant_word})")
            } else {
                format!("network envelope locked: {hosts} ({grants} {grant_word})")
            }
        }
        // Operator egress-policy declarations (RFC §5.4) — `/private` and
        // `/taint` in the room, `session egress-policy set/clear` on the CLI.
        // Metadata only: the operation, the operator attribution, and the
        // rule/default summary from the causal event payload.
        "egress.session_policy" => {
            let operation = field("operation").unwrap_or_default();
            let set_by = field("set_by").unwrap_or_default();
            let detail = p.as_ref().and_then(|v| v.get("detail"));
            let mut head = match operation.as_str() {
                "clear" => "egress policy cleared".to_string(),
                "set" => {
                    if let Some(d) = detail {
                        let rules = d.get("rule_count").and_then(|x| x.as_u64()).unwrap_or(0);
                        let sources = d
                            .get("rule_sources")
                            .and_then(|x| x.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        let default = d
                            .get("default_label")
                            .and_then(|x| x.as_str())
                            .unwrap_or_default();
                        let mut s = format!(
                            "egress policy set ({rules} rule{})",
                            if rules == 1 { "" } else { "s" }
                        );
                        if !sources.is_empty() {
                            s.push_str(&format!(": {sources}"));
                        }
                        if !default.is_empty() {
                            s.push_str(&format!(" · default {default}"));
                        }
                        s
                    } else {
                        "egress policy set".to_string()
                    }
                }
                _ => "egress policy change".to_string(),
            };
            if !set_by.is_empty() {
                head.push_str(&format!(" — {set_by}"));
            }
            head
        }
        // Egress enforcement rows (#972) — metadata-only summaries driven by
        // the content-free payloads of the egress.* causal events.
        "egress.envelope_labeled" => {
            let tool = field("tool_name").unwrap_or_else(|| "tool".into());
            let label = egress_label_display(&p, "label");
            let mut s = format!("{tool} labeled → {label}");
            if let Some(rules) = p
                .as_ref()
                .and_then(|v| v.get("matched_rules"))
                .and_then(|x| x.as_array())
            {
                let r: Vec<&str> = rules.iter().filter_map(|v| v.as_str()).collect();
                if !r.is_empty() {
                    s.push_str(&format!(" ({})", r.join(", ")));
                }
            }
            s
        }
        "egress.envelope_withheld" => {
            let tcid = field("tool_call_id").unwrap_or_else(|| "?".into());
            let sink = field("target_sink").unwrap_or_else(|| "?".into());
            let label = egress_label_display(&p, "label");
            format!("content withheld from {sink}: {tcid} ({label})")
        }
        "egress.request_filtered" | "egress.request_forwarded" => {
            let sink = field("target_sink").unwrap_or_else(|| "?".into());
            let withheld = p
                .as_ref()
                .and_then(|v| v.get("withheld_count"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let included = p
                .as_ref()
                .and_then(|v| v.get("included_count"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let suffix = field("model")
                .map(|m| format!(" (forwarded to {m})"))
                .unwrap_or_default();
            format!("egress filter{suffix}: {withheld} withheld, {included} included ({sink})")
        }
        "egress.assertion_violation" => {
            let tcid = field("tool_call_id").unwrap_or_else(|| "?".into());
            let sink = field("target_sink").unwrap_or_else(|| "?".into());
            format!("egress assertion violation: {tcid} aborted before {sink}")
        }
        "egress.boundary_refused" => {
            let surface = field("surface").unwrap_or_else(|| "?".into());
            let label = field("label_name")
                .or_else(|| field("band_label_name"))
                .unwrap_or_else(|| "?".into());
            let mut s = format!("egress boundary refused: {surface} ({label})");
            if let Some(preset) = field("preset_class") {
                s.push_str(&format!(" — {preset} not cleared"));
            }
            s
        }
        "egress.declassified" => {
            // `target` is serialized as the tagged enum (`serde(tag = "kind",
            // content = "value")`) — `{"kind":"source_pattern","value":"…"}` —
            // never a bare string. Same parse as the grants panel's
            // `declass_target_kv` and the audit CLI's `build_egress_audit`.
            let target = p
                .as_ref()
                .and_then(|v| v.get("target"))
                .map(|t| {
                    t.as_str().map(str::to_string).unwrap_or_else(|| {
                        format!(
                            "{}:{}",
                            t.get("kind").and_then(|k| k.as_str()).unwrap_or("?"),
                            t.get("value").and_then(|v| v.as_str()).unwrap_or("?"),
                        )
                    })
                })
                .unwrap_or_else(|| "?".into());
            let sink = field("allowed_sink").unwrap_or_else(|| "?".into());
            format!("egress widened: {target} → {sink}")
        }
        "egress.relabel" => {
            let kind = field("kind").unwrap_or_else(|| "?".into());
            let name = field("new_label_name").unwrap_or_else(|| "?".into());
            format!("egress relabeled: {kind} → {name}")
        }
        "egress.provider_selected" => {
            let batch = field("batch_label_name").unwrap_or_else(|| "?".into());
            let no_eligible = p
                .as_ref()
                .and_then(|v| v.get("no_eligible_provider"))
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            if no_eligible {
                format!("egress routing refused: no provider cleared for {batch}")
            } else {
                format!(
                    "egress routing: {batch} → {}",
                    field("chosen_preset").unwrap_or_else(|| "?".into())
                )
            }
        }
        // Reserved — no emitter today (failures use `tool.completed` with
        // `ok:false`); kept so a future dedicated failure event renders sanely.
        "tool.failed" => {
            let tool_name = field("tool_name").or_else(|| field("tool")).unwrap_or_else(|| "tool".into());
            format!("{tool_name} failed: {}", one_line(&field("error").unwrap_or_default(), 120))
        }
        "llm.retry" => {
            let attempt = p.as_ref().and_then(|v| v.get("attempt")).and_then(|x| x.as_u64()).unwrap_or(0);
            let max = p.as_ref().and_then(|v| v.get("max_retries")).and_then(|x| x.as_u64()).unwrap_or(0);
            format!("LLM retry {attempt}/{max}")
        }
        // Not emitted by the gateway today (reserved alongside their sibling
        // lifecycle events); classified in `event_tier` for forward-compat.
        "plan.rejected" => format!(
            "plan rejected ({}){}",
            field("plan_id").unwrap_or_default(),
            field("reason").map(|r| format!(" — {}", one_line(&r, 100))).unwrap_or_default()
        ),
        "approval.withdrawn" => format!("approval withdrawn ({})", field("request_id").unwrap_or_default()),
        "escalation.approved" => format!("escalation approved ({})", field("revision_id").unwrap_or_default()),
        "escalation.rejected" => format!(
            "escalation rejected ({}){}",
            field("revision_id").unwrap_or_default(),
            field("reason").map(|r| format!(" — {}", one_line(&r, 100))).unwrap_or_default()
        ),
        other => other.to_string(),
    }
}

/// Display name for a label value in an egress event payload: a named string
/// (`local_only`, `no_remote_model`, `unrestricted`) or the wire shape of an
/// `EgressLabel` — a sink-set array — delegated to the room's canonical
/// `sinks_label_name` so summaries agree with the labels panel. Missing/null
/// labels are "?" — an unknown payload must never render as fully cleared.
fn egress_label_display(p: &Option<serde_json::Value>, key: &str) -> String {
    let Some(v) = p.as_ref().and_then(|v| v.get(key)) else {
        return "?".into();
    };
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(arr) = v.as_array() {
        let sinks_json = serde_json::to_string(arr).unwrap_or_default();
        return super::tui::sinks_label_name(&sinks_json);
    }
    if v.is_null() {
        return "?".into();
    }
    "?".into()
}

/// Map a `SessionRole` to the channel-neutral `ActorKind`.
pub fn actor_kind(role: &SessionRole) -> ActorKind {
    match role {
        SessionRole::Operator => ActorKind::Operator,
        SessionRole::Planner => ActorKind::Planner,
        SessionRole::Specialist { .. } => ActorKind::Specialist,
        SessionRole::Sentinel => ActorKind::Sentinel,
        SessionRole::Curator => ActorKind::Curator,
        SessionRole::Auditor => ActorKind::Auditor,
        SessionRole::Tool { .. } => ActorKind::Tool,
        SessionRole::ExternalSurface { .. } => ActorKind::ExternalSurface,
        SessionRole::Runtime => ActorKind::Runtime,
    }
}

/// Cap a `detail` line at `max` visible chars; the trailing `…` counts toward
/// the cap. Used for the second-line preview (tool args, message snippet, etc.)
/// so a long stdout doesn't blow the 2-line row budget.
pub(crate) fn cap_preview(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let truncated: String = s.chars().take(max - 1).collect();
    format!("{truncated}…")
}

/// Like `cap_preview` but preserves newlines, yielding a multi-line string
/// suitable for the `detail` field. Caps total character count at `max`; a cut
/// carries the `…(+N chars · Enter)` marker (see the truncation vocabulary on
/// [`NARRATIVE_BODY_MAX`]) so the operator always knows what was dropped and
/// how to see it.
fn preserve_lines(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let kept = max.saturating_sub(1);
    let truncated: String = s.chars().take(kept).collect();
    format!(
        "{truncated}…(+{} chars · Enter)",
        compact_count(total - kept)
    )
}

/// Build the second-line preview for a known event type. Returns `None` for
/// events that have no useful preview — the row then renders single-line.
fn detail_preview(entry: &SessionTimelineEntry) -> Option<String> {
    let p = entry
        .payload
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .map(|v| {
            if let Some(s) = v.as_str() {
                serde_json::from_str::<serde_json::Value>(s).unwrap_or(v)
            } else {
                v
            }
        });
    let s = |k: &str| -> Option<String> {
        p.as_ref()
            .and_then(|v| v.get(k))
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };

    match entry.event_type.as_str() {
        // Tool requested: show the key argument (artifact_ref, name, agent_id)
        // from the arguments JSON.
        "tool.requested" => {
            let tool = s("tool_name").or_else(|| s("tool"));
            match tool.as_deref() {
                Some(name) => extract_tool_key_param(&p, name).map(|kp| cap_preview(&kp, 80)),
                None => None,
            }
        }
        // Tool calls: a one-line hint at the args or the result preview.
        "tool.completed" => {
            let tool = s("tool_name").or_else(|| s("tool"));
            // For sandbox_exec-like tools, surface the result.stdout (or first
            // 80 chars of any string result). For content_write, surface path
            // or args_preview. For artifact_inspect/agent_spawn, show the key
            // argument from args_preview (extracted by the tracer).
            match tool.as_deref() {
                // The path is already in the headline (`✎ wrote <path>`); use the
                // detail line for the store ref / sandbox path so the operator can
                // reference the file, and fall back to a result summary. Suppress a
                // line that would only repeat the path.
                Some("content_write") | Some("content_patch") => {
                    let result_obj = p.as_ref().and_then(|v| v.get("result")).and_then(|r| {
                        match r {
                            serde_json::Value::String(raw) => {
                                serde_json::from_str::<serde_json::Value>(raw).ok()
                            }
                            serde_json::Value::Object(_) => Some(r.clone()),
                            _ => None,
                        }
                    });
                    let from_result = |k: &str| {
                        result_obj
                            .as_ref()
                            .and_then(|r| r.get(k).and_then(|x| x.as_str()).map(str::to_string))
                    };
                    from_result("summary")
                        .or_else(|| from_result("sandbox_path"))
                        .or_else(|| from_result("ref"))
                        .filter(|d| !d.trim().is_empty())
                        .map(|d| cap_preview(&d, 80))
                }
                Some("artifact_inspect") => s("args_preview")
                    .or_else(|| s("artifact_ref"))
                    .map(|p| cap_preview(&p, 80)),
                Some("promotion_record") | Some("promotion.record") => p
                    .as_ref()
                    .and_then(|payload| promotion_record_view(entry, payload))
                    .map(|view| promotion_record_detail(&view)),
                // `result` is normally a JSON-encoded *string* (see
                // `log_tool_completed_with_approval`), not a nested object —
                // parse it before reading stdout/stderr/exit_code. Without
                // this, `stdout` lookup always misses and the fallback used
                // to dump the raw, truncated JSON string (e.g.
                // `{"command_succeeded":false,"execution_trace_id":"80bc0…`).
                Some("sandbox_exec") | Some("artifact_exec") => {
                    let result_obj = p.as_ref().and_then(|v| v.get("result")).and_then(|r| {
                        match r {
                            serde_json::Value::String(raw) => {
                                serde_json::from_str::<serde_json::Value>(raw).ok()
                            }
                            serde_json::Value::Object(_) => Some(r.clone()),
                            _ => None,
                        }
                    });
                    result_obj.map(|r| {
                        let succeeded = r
                            .get("command_succeeded")
                            .and_then(|x| x.as_bool())
                            .or_else(|| r.get("ok").and_then(|x| x.as_bool()))
                            .unwrap_or(true);
                        let stdout = r.get("stdout").and_then(|x| x.as_str()).unwrap_or("").trim();
                        let stderr = r.get("stderr").and_then(|x| x.as_str()).unwrap_or("").trim();
                        let exit_code = r.get("exit_code").and_then(|x| x.as_i64());
                        if succeeded {
                            let out = if !stdout.is_empty() {
                                stdout
                            } else if !stderr.is_empty() {
                                stderr
                            } else {
                                "(no output)"
                            };
                            cap_preview(out, 160)
                        } else {
                            let detail = if !stderr.is_empty() {
                                stderr
                            } else if !stdout.is_empty() {
                                stdout
                            } else {
                                "no output"
                            };
                            let code = exit_code.map(|c| format!(" (exit {c})")).unwrap_or_default();
                            cap_preview(&format!("✗ command failed{code}: {detail}"), 160)
                        }
                    })
                }
                // The spawn target is already in the headline (`⑂ spawn → <id>`),
                // so the detail line surfaces the child's *task* instead. When the
                // only preview available is the agent id, drop the redundant line.
                Some("agent_spawn") => {
                    // Resolve the target the same way the headline does, then
                    // suppress a detail line that only repeats it.
                    let agent = agent_spawn_agent_id(entry)
                        .or_else(|| extract_tool_key_param(&p, "agent_spawn"));
                    s("message")
                        .filter(|m| !m.trim().is_empty())
                        .or_else(|| {
                            s("args_preview").filter(|ap| {
                                agent.as_deref().map(str::trim) != Some(ap.trim())
                            })
                        })
                        .map(|m| cap_preview(&m, 80))
                }
                // `task_ids` is never a top-level payload field (the real
                // payload only ever has tool_name/result/args_preview — see
                // `log_tool_completed_with_approval`), so this always
                // returned None in production. The real result carries a
                // human-readable `message` (workflow_wait) or `resume_hint`
                // (workflow_state) — surface that instead.
                Some("workflow_wait") | Some("workflow_state") => {
                    let result_val = p.as_ref().and_then(|v| v.get("result")).and_then(|r| {
                        match r {
                            serde_json::Value::String(raw) => {
                                serde_json::from_str::<serde_json::Value>(raw).ok()
                            }
                            serde_json::Value::Object(_) => Some(r.clone()),
                            _ => None,
                        }
                    });
                    result_val
                        .as_ref()
                        .and_then(|r| {
                            r.get("message")
                                .and_then(|x| x.as_str())
                                .or_else(|| r.get("resume_hint").and_then(|x| x.as_str()))
                        })
                        .map(|m| cap_preview(m, 160))
                }
                Some(_) => {
                    // `result` is normally a JSON-encoded *string* (see
                    // `log_tool_completed_with_approval`, used by every tool).
                    // Parse it once so `stdout`/`summary` lookups actually run:
                    // previously `r.as_str()` on the *unparsed* string always
                    // matched first, so every non-special-cased tool (~50+ of
                    // them) showed the raw truncated JSON dump and never
                    // reached `extract_tool_summary` below — same class of bug
                    // as sandbox_exec's `stdout` miss, but affecting the
                    // generic fallback used by most tools in the system.
                    let result_val = p.as_ref().and_then(|v| v.get("result")).and_then(|r| {
                        match r {
                            serde_json::Value::String(raw) => {
                                serde_json::from_str::<serde_json::Value>(raw).ok()
                            }
                            serde_json::Value::Object(_) => Some(r.clone()),
                            _ => None,
                        }
                    });
                    result_val
                        .as_ref()
                        .and_then(|r| r.get("stdout").and_then(|x| x.as_str()).map(|o| cap_preview(o, 120)))
                        .or_else(|| extract_tool_summary(p.as_ref()).map(|s| cap_preview(&s, 120)))
                        .or_else(|| {
                            // Last resort: no stdout, no summary field found —
                            // show the raw result text so there's still
                            // *something* rather than nothing.
                            s("result").map(|r| cap_preview(&r, 120))
                        })
                }
                None => None,
            }
        }
        "user.ask.pending" => {
            let opts = p
                .as_ref()
                .and_then(|v| v.get("options"))
                .and_then(|v| v.as_array());
            let freeform = p
                .as_ref()
                .and_then(|v| v.get("allow_freeform"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let Some(opts) = opts else { return None };
            let mut lines: Vec<String> = opts
                .iter()
                .enumerate()
                .filter_map(|(i, o)| {
                    o.get("label").and_then(|l| l.as_str()).map(|l| {
                        format!("[{}] {}", i + 1, l)
                    })
                })
                .collect();
            if freeform {
                lines.push("(or type your own answer)".to_string());
            }
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        }
        // Agent/operator narrative bodies are assembled in `render_spec` via
        // [`message_list_rows`] — not duplicated here.
        "agent.message" | "operator.message" => None,
        "workflow.child_state" => s("summary")
            .filter(|r| !r.is_empty())
            .map(|r| cap_preview(&r, 200)),
        "scheduled_job.completed" | "scheduled_job.failed" => s("result_summary")
            .filter(|r| !r.is_empty())
            .map(|r| cap_preview(&r, 200)),
        // LLM failure: show the preceding action chain on the second line so
        // the row alone tells the story.
        "llm.request_failed" => {
            let chain = p
                .as_ref()
                .and_then(|v| v.get("preceding"))
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" → ")
                })
                .unwrap_or_default();
            if chain.is_empty() {
                None
            } else {
                Some(cap_preview(&format!("⟵ after: {chain}"), 100))
            }
        }
        "llm.empty_response" => {
            let chain = p
                .as_ref()
                .and_then(|v| v.get("preceding"))
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" → ")
                })
                .unwrap_or_default();
            if chain.is_empty() {
                None
            } else {
                Some(cap_preview(&format!("⟵ after: {chain}"), 100))
            }
        }
        "egress.envelope_withheld" => s("indication")
            .filter(|i| !i.is_empty())
            .map(|i| format!("replaced with: {i}")),
        "egress.boundary_refused" => s("reason")
            .filter(|r| !r.is_empty())
            .map(|r| cap_preview(&r, 160)),
        "egress.assertion_violation" => s("payload_digest")
            .filter(|d| !d.is_empty())
            .map(|d| format!("payload_sha256: {d}")),
        _ => None,
    }
}

fn notification_detail(entry: &SessionTimelineEntry) -> Option<String> {
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
    let child = notif
        .get("child_session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let approval_id = notif
        .get("approval_request_id")
        .and_then(|v| v.as_str());
    let task_id = notif.get("task_id").and_then(|v| v.as_str());
    let mut lines = vec![];
    let short_child = child
        .rsplit('/')
        .next()
        .unwrap_or(child);
    lines.push(format!("child: {short_child}"));
    if let Some(task) = task_id {
        lines.push(format!("task: {task}"));
    }
    if let Some(apr) = approval_id {
        lines.push(format!("approval: {apr}"));
    }
    Some(lines.join("\n"))
}

/// Build a `RowSpec` for a single timeline entry — the channel-neutral, fully
/// structured view. Callers (TUI, CLI viewer, future channels) can render this
/// however they want.
pub fn render_spec(entry: &SessionTimelineEntry) -> RowSpec {
    let (headline, detail) = match entry.event_type.as_str() {
        "agent.message" => message_list_rows(entry, "message"),
        "operator.message" => {
            let (headline, body) = message_list_rows(entry, "message");
            let notif_detail = notification_detail(entry);
            match body {
                Some(b) if b.contains('\n') || b.chars().count() > 120 => {
                    let detail = match notif_detail {
                        Some(nd) => format!("{b}\n{nd}"),
                        None => b,
                    };
                    (headline, Some(detail))
                }
                Some(b) if headline.is_empty() => (one_line(&b, 240), notif_detail),
                Some(b) => (headline, Some(b).or(notif_detail)),
                None => (headline, notif_detail),
            }
        }
        "approval.pending" => {
            // Federation escalations render as `escalation.pending` cards.
            // Their `approval.pending session_escalate` mirror is suppressed so
            // the operator sees exactly one gate per escalation.
            if is_session_escalate_mirror(entry) {
                (
                    String::new(),
                    None,
                )
            } else {
                let action = entry
                    .payload
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                    .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(str::to_string));
                match action.as_deref() {
                    Some("wiki_propose") => wiki_proposal_gate_card(entry),
                    Some("session_escalate") => session_escalate_gate_card(entry),
                    _ => approval_gate_card(entry),
                }
            }
        }
        "user.ask.pending" => interaction_gate_card(entry),
        "plan.pending" => plan_gate_card(entry),
        "escalation.pending" => escalation_gate_card(entry),
        "wiki.proposed" => wiki_lifecycle_card(entry, "📝 WIKI PROPOSED"),
        "wiki.promoted" => wiki_lifecycle_card(entry, "✅ WIKI PROMOTED"),
        "wiki.rejected" => wiki_lifecycle_card(entry, "❌ WIKI REJECTED"),
        "wiki.withdrawn" => wiki_lifecycle_card(entry, "↩️ WIKI WITHDRAWN"),
        _ => (summarize(entry), detail_preview(entry)),
    };
    let actor = actor_kind(&entry.role);
    // Use `actor_label(entry)` (not just `role_label(role)`) so the principal
    // kind is decorated — humans get a 🧑 prefix, foreign agents get a 🌐
    // prefix with provider. Without it the TUI loses the operator-vs-agent
    // distinction on rows that are otherwise identical by role.
    let label = actor_label(entry);
    let show_reasoning = entry.event_type != "agent.reasoning"
        || !headline.is_empty();
    RowSpec {
        altitude: entry.altitude,
        actor,
        actor_label: label,
        tone: tone_for_entry(entry),
        headline,
        detail,
        turn_id: entry.turn_id.clone(),
        source_session_id: Some(entry.source_session_id.clone()),
        turn_index: None, // The TUI fills in from turn_id via turn_number_of (turn_counter).
        turn_label: None, // Child spawn badge (e.g. `3→coder`) filled by the TUI.
        in_flight: false, // The TUI fills this in once it knows turn lifecycle.
        show_reasoning,
        egress_label: None, // The TUI fills this from the labels.list trace map.
    }
}

/// The most useful text to copy for a timeline row. For tool rows this is the
/// actionable token — the command, path, artifact ref, or agent id in
/// `args_preview` (so `Y` grabs exactly what you'd paste into a shell or a ref
/// lookup). For everything else it's the visible content: headline plus detail.
pub fn row_copy_text(entry: &SessionTimelineEntry) -> String {
    if matches!(entry.event_type.as_str(), "tool.completed" | "tool.requested") {
        if let Some(p) = parse_entry_payload(entry) {
            if let Some(kp) = payload_field_str(&p, "args_preview").filter(|s| !s.is_empty()) {
                return kp;
            }
            // tool.requested carries raw `arguments`; extract the key param.
            let tool = payload_field_str(&p, "tool_name").unwrap_or_default();
            if let Some(kp) = extract_tool_key_param(&Some(p), &tool) {
                return kp;
            }
        }
    }
    let spec = render_spec(entry);
    match spec.detail {
        Some(d) if !d.trim().is_empty() => format!("{}\n{}", spec.headline, d),
        _ => spec.headline,
    }
}

/// Backwards-compat: one-line rendering, used by the CLI viewer and tests.
pub fn render_line(entry: &SessionTimelineEntry) -> String {
    let summary = summarize(entry);
    let suffix = if entry.event_type == "user.ask.pending" {
        choices_hint(
            entry.payload.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()).as_ref(),
        )
    } else {
        String::new()
    };
    format!(
        "{} [{}] {}{suffix}",
        altitude_glyph(entry.altitude),
        actor_label(entry),
        summary,
    )
}

/// A rendered timeline row: either a single event with a structured spec, or a
/// *collapsed* run of consecutive low-altitude (Detail) plumbing folded into one
/// count row. The structured form lets the interactive shell style each part
/// independently and expand a collapsed run on demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderedRow {
    /// A single event rendered as a structured spec.
    Line(RowSpec),
    /// A collapsed run of routine events. `in_flight` is set by the TUI when
    /// any event inside the run belongs to an open turn.
    Collapsed {
        count: usize,
        summary: String,
        in_flight: bool,
    },
}

/// Visual class for a timeline row — lets channels style agent narrative
/// separately from tool plumbing even when both share the same seat/actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowTone {
    /// What the agent (or operator) said — primary narrative.
    AgentNarrative,
    /// A completed tool invocation and its result preview.
    ToolCall,
    /// Hidden-by-default reasoning (`agent.reasoning`).
    Reasoning,
    /// Operator gates — plans, approvals, clarifications (high visibility).
    OperatorGate,
    /// Promotion verdict recorded (`promotion_record` pass).
    VerdictPass,
    /// Promotion verdict recorded (`promotion_record` fail/reject).
    VerdictFail,
    /// LLM errors and everything else.
    Default,
}

/// Map a gateway event type to the row's visual tone.
pub fn row_tone(event_type: &str) -> RowTone {
    match event_type {
        "agent.message" | "operator.message" | "agent.peer_message" => RowTone::AgentNarrative,
        "tool.completed" => RowTone::ToolCall,
        "agent.reasoning" => RowTone::Reasoning,
        _ => RowTone::Default,
    }
}

/// Tone for a full timeline entry — includes embedded plan proposals in messages.
pub fn tone_for_entry(entry: &SessionTimelineEntry) -> RowTone {
    if let Some(tone) = promotion_record_tone(entry) {
        return tone;
    }
    match entry.event_type.as_str() {
        "plan.pending" | "approval.pending" | "user.ask.pending" | "escalation.pending"
        | "wiki.proposed" | "wiki.promoted" | "wiki.rejected" | "wiki.withdrawn" => {
            RowTone::OperatorGate
        }
        "agent.message" | "operator.message" if extract_plan_proposal_id(entry).is_some() => {
            RowTone::OperatorGate
        }
        other => row_tone(other),
    }
}

/// Coarse seat classification — drives the colored left rail in the TUI and the
/// icon the CLI viewer prints. Channel-neutral: the TUI maps to a color, the
/// CLI viewer maps to a 2-char tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
    Operator,
    Planner,
    Specialist,
    Sentinel,
    Curator,
    Auditor,
    Tool,
    ExternalSurface,
    Runtime,
    /// A future / unknown seat — default style, no rail coloring.
    /// Kept for forward-compat (the room must render rows even when the
    /// gateway grows a new seat the channel doesn't know yet).
    #[allow(dead_code)]
    Other,
}

impl ActorKind {
    /// 2-char tag for the CLI viewer (`OP`, `PL`, ...). The TUI doesn't use
    /// this — it has a colored rail instead — but the tag is part of the
    /// channel-neutral contract and stays exported for future channels.
    #[allow(dead_code)]
    pub fn tag(self) -> &'static str {
        match self {
            ActorKind::Operator => "OP",
            ActorKind::Planner => "PL",
            ActorKind::Specialist => "SP",
            ActorKind::Sentinel => "SE",
            ActorKind::Curator => "CU",
            ActorKind::Auditor => "AU",
            ActorKind::Tool => "TL",
            ActorKind::ExternalSurface => "EX",
            ActorKind::Runtime => "RT",
            ActorKind::Other => "--",
        }
    }
}

/// Structured, channel-neutral specification of a single timeline row. The TUI
/// maps each field to styled spans; the CLI viewer joins `headline` + (optional)
/// `detail` for the legacy stream. Adding a new field here is additive — old
/// consumers fall back to the headline string via [`to_plain_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowSpec {
    /// Altitude glyph (▸ ⚠ ✗ ·) and the row's color class.
    pub altitude: Altitude,
    /// Which seat/actor — drives the rail color in the TUI.
    pub actor: ActorKind,
    /// Agent narrative vs tool call vs reasoning — drives headline/rail tint.
    pub tone: RowTone,
    /// Human actor name (e.g. "operator", "coder"). Shown as `[name]` in the TUI.
    pub actor_label: String,
    /// Primary headline — the most important thing to show.
    pub headline: String,
    /// Optional second line: a concrete preview (tool args, message preview,
    /// result snippet). Capped per channel rules.
    pub detail: Option<String>,
    /// Turn this event belongs to (if any). Used to draw turn boundaries.
    pub turn_id: Option<String>,
    /// Session that produced this row (`source_session_id` from the timeline).
    pub source_session_id: Option<String>,
    /// Turn counter number parsed from `turn_id` (`turn-000003` → 3). Matches
    /// gateway `turn_counter` / `session.fork --at-turn N`. `None` when untagged
    /// or the id is not a canonical `turn-NNNNNN` string.
    pub turn_index: Option<u32>,
    /// Display override for spawned child rows — e.g. `3→coder` or `3.2`.
    /// When set, used instead of bare `turn_index` in labels and turn dividers.
    pub turn_label: Option<String>,
    /// True when the turn containing this row is still in flight (no matching
    /// `turn.end` has been seen yet). The TUI uses this to show a spinner.
    pub in_flight: bool,
    /// Show the 💭 reasoning prefix — false when reasoning is hidden by toggle.
    pub show_reasoning: bool,
    /// Egress label display name for this row's content (e.g. `local_only`,
    /// `no_remote_model`) when its execution trace is labeled. `None` =
    /// unrestricted / not yet labeled. Operator-surface only (#971); the TUI
    /// renders a compact marker from it.
    pub egress_label: Option<String>,
}

impl RowSpec {
    /// Plain-text projection for the CLI viewer. Joins headline + (optional)
    /// detail with a newline so the legacy stream still reads as a clean
    /// one-or-two-line entry.
    pub fn to_plain_text(&self) -> String {
        match &self.detail {
            Some(d) if !d.is_empty() => format!("{}\n  {}", self.headline, d),
            _ => self.headline.clone(),
        }
    }
}

/// The altitude a row renders at (collapsed runs are Detail by definition).
/// Kept exported — the TUI currently reads `spec.altitude` directly, but
/// `row_altitude` is the single channel-neutral accessor for callers that
/// don't want to pattern-match.
#[allow(dead_code)]
pub fn row_altitude(row: &RenderedRow) -> Altitude {
    match row {
        RenderedRow::Line(spec) => spec.altitude,
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

/// Fold consecutive **routine** events into a single collapsed row so plumbing
/// (turns, LLM rounds, read-only tool calls, workflow bookkeeping) doesn't
/// flood the view. Classification is by [`event_tier`], not raw altitude, so
/// that routine `tool.completed` rows (which the gateway tags `Normal`) also
/// fold — previously only `Detail`-altitude events collapsed, which left the
/// asymmetric `tool.requested` (Detail, folded) vs `tool.completed` (Normal,
/// individual) pair and produced dozens of completion rows.
///
/// A lone routine event renders normally — collapsing one is pointless.
/// Checkpoint and Significant tiers always render individually. Coalescing is
/// page-local; a run split across reads collapses per page.
pub fn coalesce(entries: &[SessionTimelineEntry]) -> Vec<RenderedRow> {
    coalesce_indexed(entries).into_iter().map(|(r, _)| r).collect()
}

/// Approval ids already surfaced by a linked `escalation.pending` row.
///
/// `federation.escalate` emits both `escalation.pending` (verdict summary) and
/// `approval.pending` (`session_escalate` mirror) for the same `apr-esc-*` gate.
pub fn linked_promotion_escalation_approval_ids(
    entries: &[SessionTimelineEntry],
) -> HashSet<String> {
    entries
        .iter()
        .filter(|e| e.event_type == "escalation.pending")
        .filter_map(|e| e.refs.approval_request_id.clone())
        .collect()
}

/// Hide the `session_escalate` approval mirror when the federation escalation
/// card is already on the timeline for the same approval request.
pub fn is_redundant_promotion_escalation_approval(
    entry: &SessionTimelineEntry,
    linked_escalation_approvals: &HashSet<String>,
) -> bool {
    if entry.event_type != "approval.pending" {
        return false;
    }
    let payload = match parse_entry_payload(entry) {
        Some(v) => v,
        None => return false,
    };
    if payload_field_str(&payload, "action").as_deref() != Some("session_escalate") {
        return false;
    }
    let request_id = payload_field_str(&payload, "request_id")
        .or_else(|| entry.refs.approval_request_id.clone());
    if request_id
        .as_ref()
        .is_some_and(|id| id.starts_with("apr-esc-"))
    {
        return true;
    }
    request_id
        .as_ref()
        .is_some_and(|id| linked_escalation_approvals.contains(id))
}

/// Call ids of every `tool.completed` on the page. Used to drop the matching
/// `tool.requested` rows — one call, one row (see [`is_paired_tool_request`]).
fn completed_call_ids(entries: &[SessionTimelineEntry]) -> HashSet<String> {
    entries
        .iter()
        .filter(|e| e.event_type == "tool.completed")
        .filter_map(|e| {
            let p = parse_entry_payload(e)?;
            payload_field_str(&p, "call_id")
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// True for a `tool.requested` whose completion (same `call_id`) is also on
/// the page. The completion row's headline already carries the same key param
/// (`args_preview`), so the pair would render as two rows for one call — the
/// request half is dropped. A request without its completion yet (in-flight
/// call, or the completion is on a later page) still renders.
fn is_paired_tool_request(e: &SessionTimelineEntry, completed: &HashSet<String>) -> bool {
    if e.event_type != "tool.requested" || completed.is_empty() {
        return false;
    }
    parse_entry_payload(e)
        .and_then(|p| payload_field_str(&p, "call_id"))
        .is_some_and(|id| completed.contains(&id))
}

/// Like [`coalesce`], but also returns each row's [`RowSource`] for drill-down.
pub fn coalesce_indexed(entries: &[SessionTimelineEntry]) -> Vec<(RenderedRow, RowSource)> {
    let linked_escalation_approvals = linked_promotion_escalation_approval_ids(entries);
    let completed_calls = completed_call_ids(entries);
    let mut out = Vec::new();
    let mut run_start: Option<usize> = None;
    let mut run_len: usize = 0;
    for (i, e) in entries.iter().enumerate() {
        if is_redundant_promotion_escalation_approval(e, &linked_escalation_approvals) {
            continue;
        }
        if is_paired_tool_request(e, &completed_calls) {
            continue;
        }
        // Defense-in-depth: only fold Routine-tier events that are ALSO below
        // the Attention altitude. This preserves the original invariant that
        // Attention/Error rows (gates, failures, interventions) NEVER collapse,
        // even if a future event type is mis-classified as Routine by `event_tier`.
        let foldable = event_tier(e) == EventTier::Routine
            && e.altitude < Altitude::Attention;
        if foldable {
            if run_start.is_none() {
                run_start = Some(i);
            }
            run_len += 1;
        } else {
            flush_run(entries, &mut run_start, &mut run_len, &mut out);
            out.push((
                RenderedRow::Line(render_spec(e)),
                RowSource::Single(i),
            ));
        }
    }
    flush_run(entries, &mut run_start, &mut run_len, &mut out);
    out
}

/// Visibility tier for a timeline event — drives squashing in the default
/// (squashed) view. Pure-view classification; the gateway-assigned altitude
/// remains the source of truth for glyph/color, and the floor filter still
/// applies before coalescing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTier {
    /// Decisions, boundaries, and errors — plans, approvals, escalations,
    /// operator messages, session starts. Always rendered individually; the
    /// TUI elevates these with banner chrome and jump-to-checkpoint keys.
    Checkpoint,
    /// Agent output and state-changing actions — `agent.message`, audits, and
    /// `tool.completed` for privileged/state-changing tools. Always rendered
    /// individually so the operator sees what the session *produced*.
    Significant,
    /// Plumbing — turns, LLM rounds, reasoning, tool requests, read-only tool
    /// completions, workflow bookkeeping. Folded into collapsed runs.
    Routine,
}

/// Checkpoint-tier event types (see [`EventTier::Checkpoint`]). Single source
/// of truth for `event_tier` — also walked by a test asserting every one of
/// these has a real `summarize()` arm (not the raw-string fallback).
const CHECKPOINT_EVENT_TYPES: &[&str] = &[
    "plan.pending", "plan.approved", "plan.rejected", "plan.withdrawn",
    "approval.pending", "approval.approved", "approval.rejected",
    "approval.cancelled", "approval.withdrawn",
    "escalation.pending", "escalation.approved", "escalation.rejected",
    "user.ask.pending",
    "operator.message",
    "session.start", "session.end",
    "workbench.created", "workbench.reconciled", "workbench.discarded",
    "runtime.lock_drift",
    "divergence.intervention",
    "security.escape_threshold",
    "wiki.proposed", "wiki.promoted", "wiki.rejected", "wiki.withdrawn",
];

/// Significant-tier event types (see [`EventTier::Significant`]). Single
/// source of truth for `event_tier` — also walked by a test asserting every
/// one of these has a real `summarize()` arm (not the raw-string fallback).
const SIGNIFICANT_EVENT_TYPES: &[&str] = &[
    "agent.message", "digest_annotate",
    "llm.request_failed", "llm.empty_response", "llm.retry",
    "tool.failed", "guard.tripped",
    "session.emergency_stop", "security.sandbox_escape",
    // Operator egress-policy declarations (`/private`, `/taint`) — a change to
    // what may leave the machine is consciously kept individual (#977).
    "egress.session_policy",
    // Egress enforcement outcomes — a withheld envelope, a refused boundary,
    // a tripwire violation, or a widening grant is never folded (#972).
    "egress.envelope_withheld",
    "egress.assertion_violation",
    "egress.boundary_refused",
    "egress.declassified",
];

/// Routine-tier event types (see [`EventTier::Routine`]).
const ROUTINE_EVENT_TYPES: &[&str] = &[
    "turn.start", "turn.end", "llm.round", "agent.reasoning",
    "tool.requested",
    "workflow.child_state", "workflow.join_satisfied", "workflow.signal",
    "workflow.started", "workflow.completed",
    "scheduled_job.triggered", "scheduled_job.completed", "scheduled_job.failed",
    // High-volume egress metadata — per-envelope labelings, chokepoint
    // summaries, routing audits, relabel bookkeeping (#972). Folded by
    // default; surfaced on dial-down / investigation.
    "egress.envelope_labeled",
    "egress.request_filtered",
    "egress.request_forwarded",
    "egress.provider_selected",
    "egress.relabel",
];

/// `tool.completed` for these state-changing tools is Significant (shown
/// individually); every other `tool.completed` is Routine (folded).
const SIGNIFICANT_TOOL_NAMES: &[&str] = &[
    "agent_spawn",
    "content_write",
    "content_patch",
    "artifact_build",
    "artifact_project",
    "artifact_exec",
    "sandbox_exec",
    "promotion_record",
    "agent_revision_create",
    "agent_revision_create_from_intent",
    "agent_revision_promote",
    "agent_revision_rollback",
    "skill_install",
    "federation.escalate",
];

/// Classify a timeline event into a visibility tier. Used by the squashed
/// view to decide what folds vs renders individually.
///
/// The `_ => Significant` default is deliberately conservative: an unknown
/// event type renders individually (never folds) until it is consciously
/// classified here. Folding is opt-in for known plumbing only.
pub fn event_tier(entry: &SessionTimelineEntry) -> EventTier {
    let et = entry.event_type.as_str();
    if CHECKPOINT_EVENT_TYPES.contains(&et) {
        return EventTier::Checkpoint;
    }
    if SIGNIFICANT_EVENT_TYPES.contains(&et) {
        return EventTier::Significant;
    }
    if et == "tool.completed" {
        return tool_completed_tier(entry);
    }
    if ROUTINE_EVENT_TYPES.contains(&et) {
        return EventTier::Routine;
    }
    // Unknown event type: render individually (never fold) until classified.
    EventTier::Significant
}

/// `tool.completed` is Significant only for a state-changing tool; routine
/// (folded) otherwise. Reads `tool_name` from the payload defensively.
fn tool_completed_tier(entry: &SessionTimelineEntry) -> EventTier {
    let Some(p) = entry.payload.as_deref() else {
        return EventTier::Significant;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(p) else {
        return EventTier::Significant;
    };
    let tool = v
        .get("tool_name")
        .or_else(|| v.get("tool"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    if SIGNIFICANT_TOOL_NAMES.contains(&tool) {
        EventTier::Significant
    } else {
        EventTier::Routine
    }
}

/// True when the event is a first-class checkpoint (see [`EventTier::Checkpoint`]).
/// Convenience for the TUI to mark checkpoint rows for banner/jump handling.
pub fn is_checkpoint(entry: &SessionTimelineEntry) -> bool {
    event_tier(entry) == EventTier::Checkpoint
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
            RenderedRow::Line(render_spec(&entries[start])),
            RowSource::Single(start),
        )),
        n => {
            let run: Vec<&SessionTimelineEntry> = entries[start..start + n].iter().collect();
            out.push((
                RenderedRow::Collapsed {
                    count: n,
                    summary: collapsed_summary(&run),
                    in_flight: false,
                },
                RowSource::Run { start, len: n },
            ));
        }
    }
}

/// Brief breakdown of a collapsed run.
///
/// When the run contains tool calls, name the tools that ran (e.g.
/// `calls: read×2, grep, sandbox_exec`) so a folded row still tells the
/// operator *which* tools were invoked — the fact that a tool was called is
/// never hidden, only compacted.
///
/// When the run contains `agent.reasoning`, surface it up front with the same
/// `💭` snippet shape as an unsquashed reasoning row (`one_line` of the
/// payload), so thinking is visible without unsquashing the whole run.
/// Falls back to an event-type breakdown for runs with neither tools nor
/// reasoning prose.
fn collapsed_summary(run: &[&SessionTimelineEntry]) -> String {
    // Count distinct tool invocations by name. A single call emits both
    // `tool.requested` and `tool.completed`; dedupe them by `call_id` so one call
    // counts once. Without a call_id (older rows), count `tool.requested` only.
    let mut tool_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut seen_calls: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let mut non_tool = 0usize;
    let mut reasoning_count = 0usize;
    // Last non-empty reasoning snippet in the run (chronological) — same shape
    // as the unsquashed `agent.reasoning` headline.
    let mut reasoning_snippet: Option<String> = None;
    for e in run {
        match e.event_type.as_str() {
            "agent.reasoning" => {
                reasoning_count += 1;
                if let Some(p) = parse_entry_payload(e) {
                    if let Some(text) = p.get("reasoning").and_then(|v| v.as_str()) {
                        let snippet = one_line(text, 160);
                        if !snippet.is_empty() {
                            reasoning_snippet = Some(snippet);
                        }
                    }
                }
            }
            "tool.requested" | "tool.completed" => {
                let Some(p) = parse_entry_payload(e) else {
                    non_tool += 1;
                    continue;
                };
                let tool = payload_field_str(&p, "tool_name").unwrap_or_else(|| "tool".into());
                let call_id = payload_field_str(&p, "call_id");
                let count_it = match (&call_id, e.event_type.as_str()) {
                    // With a call_id, count the first of the request/completion pair.
                    (Some(id), _) => seen_calls.insert((tool.clone(), id.clone())),
                    // Without one, count the request (the completion would double it).
                    (None, "tool.requested") => true,
                    (None, _) => false,
                };
                if count_it {
                    *tool_counts.entry(tool).or_insert(0) += 1;
                }
            }
            _ => non_tool += 1,
        }
    }

    let reasoning_part = match (reasoning_count, reasoning_snippet.as_deref()) {
        (0, _) => None,
        (1, Some(s)) => Some(format!("💭 {s}")),
        (n, Some(s)) => Some(format!("💭×{n} {s}")),
        (n, None) => Some(format!("💭×{n}")),
    };

    let rest = if tool_counts.is_empty() {
        // No tool activity — event-type breakdown, excluding reasoning (already
        // surfaced above) so we don't double-count as `agent.reasoning×N`.
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for e in run {
            if e.event_type == "agent.reasoning" {
                continue;
            }
            *counts.entry(e.event_type.as_str()).or_insert(0) += 1;
        }
        if counts.is_empty() {
            None
        } else {
            let mut ordered: Vec<(&str, usize)> = counts.into_iter().collect();
            ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            let parts: Vec<String> = ordered
                .iter()
                .take(3)
                .map(|(k, c)| format!("{k}×{c}"))
                .collect();
            let more = if ordered.len() > 3 { ", …" } else { "" };
            Some(format!("routine events ({}{})", parts.join(", "), more))
        }
    } else {
        let mut ordered: Vec<(String, usize)> = tool_counts.into_iter().collect();
        ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let shown = ordered.len().min(3);
        let mut parts: Vec<String> = ordered
            .iter()
            .take(shown)
            .map(|(k, c)| if *c > 1 { format!("{k}×{c}") } else { k.clone() })
            .collect();
        if ordered.len() > shown {
            parts.push(format!("+{}", ordered.len() - shown));
        }
        // `non_tool` excludes reasoning (counted separately); keep the · +N
        // for other plumbing so the fold still signals volume.
        let extra = if non_tool > 0 {
            format!(" · +{non_tool}")
        } else {
            String::new()
        };
        Some(format!("calls: {}{}", parts.join(", "), extra))
    };

    match (reasoning_part, rest) {
        (Some(r), Some(rest)) => format!("{r} · {rest}"),
        (Some(r), None) => r,
        (None, Some(rest)) => rest,
        (None, None) => "routine events".to_string(),
    }
}

/// Multi-line detail view of a single event for the drill-down pane: metadata,
/// refs, and the pretty-printed payload. Pure (no I/O) and channel-neutral.
/// Render a `turn.end` detail by aggregating `llm.round` events from the same
/// turn. `end_index` is the entry's index in `all` — only events from the
/// matching `turn.start` through `end_index` are scanned (not the full session).
pub fn turn_summary(
    entry: &SessionTimelineEntry,
    all: &[SessionTimelineEntry],
    end_index: usize,
) -> Option<Vec<String>> {
    if entry.event_type != "turn.end" {
        return None;
    }
    let turn_id = entry.turn_id.as_deref()?;
    let mut start_index = end_index;
    for j in (0..=end_index).rev() {
        if all[j].turn_id.as_deref() == Some(turn_id) && all[j].event_type == "turn.start" {
            start_index = j;
            break;
        }
    }
    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut calls: u64 = 0;
    let mut models: Vec<String> = Vec::new();
    let mut in_turn = false;
    for e in &all[start_index..=end_index] {
        if e.turn_id.as_deref() != Some(turn_id) {
            continue;
        }
        match e.event_type.as_str() {
            "turn.start" => in_turn = true,
            "turn.end" => break,
            "llm.round" if in_turn => {
                if let Some(p) = e.payload.as_deref() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(p) {
                        let inp = v.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        let out = v.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                        total_in += inp;
                        total_out += out;
                        calls += 1;
                        if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
                            let short = m.split('/').last().unwrap_or(m).to_string();
                            if !models.contains(&short) {
                                models.push(short);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if calls == 0 {
        return None;
    }
    let mut lines = vec![
        format!("turn:      {}", turn_id),
        format!("llm calls: {}", calls),
        format!("tokens in: {} ({})", total_in, format_tokens_compact(total_in)),
        format!("tokens out: {} ({})", total_out, format_tokens_compact(total_out)),
        format!("models:    {}", models.join(", ")),
    ];
    let ratio = if total_in > 0 {
        format!("{:.1}%", total_out as f64 / total_in as f64 * 100.0)
    } else {
        "n/a".to_string()
    };
    lines.push(format!("out/in:    {}", ratio));
    Some(lines)
}

fn format_tokens_compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// One-line per-turn aggregate for the turn divider — the "chapter header".
///
/// Turns are the natural reading unit of a session: the divider now carries
/// what the turn *did* (tool calls), what it *cost* (LLM tokens), and how much
/// thinking it involved, so the operator can scan the process arc from the
/// dividers alone without opening any row. Pure and channel-neutral.
///
/// `wanted` is the set of turn_ids that actually have a divider on the current
/// page; the aggregation is a single pass over `entries`. Open turns aggregate
/// what is visible so far — the label refreshes as the turn progresses.
///
/// Returns a map turn_id → label fragment (e.g.
/// `4 calls: read×2, grep · 45.2k↓/3.1k↑ tok · 💭×3`). Turns with nothing to
/// report are absent — their dividers stay plain.
pub fn turn_divider_labels(
    entries: &[SessionTimelineEntry],
    wanted: &HashSet<String>,
) -> std::collections::HashMap<String, String> {
    let mut out: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if wanted.is_empty() {
        return out;
    }
    struct TurnAgg {
        calls: usize,
        tools: std::collections::HashMap<String, usize>,
        seen_calls: HashSet<(String, String)>,
        tokens_in: u64,
        tokens_out: u64,
        reasoning: usize,
    }
    let mut aggs: std::collections::HashMap<String, TurnAgg> = std::collections::HashMap::new();
    for e in entries {
        let Some(turn_id) = e.turn_id.as_deref().filter(|t| wanted.contains(*t)) else {
            continue;
        };
        let agg = aggs.entry(turn_id.to_string()).or_insert(TurnAgg {
            calls: 0,
            tools: std::collections::HashMap::new(),
            seen_calls: HashSet::new(),
            tokens_in: 0,
            tokens_out: 0,
            reasoning: 0,
        });
        match e.event_type.as_str() {
            "tool.requested" | "tool.completed" => {
                let p = parse_entry_payload(e);
                let tool = p
                    .as_ref()
                    .and_then(|p| payload_field_str(p, "tool_name"))
                    .unwrap_or_else(|| "tool".into());
                let call_id = p.as_ref().and_then(|p| payload_field_str(p, "call_id"));
                // Dedupe the request/completion pair of one call (first wins).
                let count_it = match &call_id {
                    Some(id) => agg.seen_calls.insert((tool.clone(), id.clone())),
                    None => e.event_type == "tool.requested",
                };
                if count_it {
                    agg.calls += 1;
                    *agg.tools.entry(tool).or_insert(0) += 1;
                }
            }
            "llm.round" => {
                if let Some(p) = parse_entry_payload(e) {
                    agg.tokens_in += p.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                    agg.tokens_out += p.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
                }
            }
            "agent.reasoning" => agg.reasoning += 1,
            _ => {}
        }
    }
    for (turn_id, agg) in aggs {
        let mut parts: Vec<String> = Vec::new();
        if agg.calls > 0 {
            let mut ordered: Vec<(String, usize)> = agg.tools.into_iter().collect();
            ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let shown = ordered.len().min(3);
            let mut names: Vec<String> = ordered
                .iter()
                .take(shown)
                .map(|(k, c)| if *c > 1 { format!("{k}×{c}") } else { k.clone() })
                .collect();
            if ordered.len() > shown {
                names.push(format!("+{}", ordered.len() - shown));
            }
            parts.push(format!("{} calls: {}", agg.calls, names.join(", ")));
        }
        if agg.tokens_in > 0 || agg.tokens_out > 0 {
            parts.push(format!(
                "{}/{} tok",
                format_tokens_compact(agg.tokens_in),
                format_tokens_compact(agg.tokens_out)
            ));
        }
        if agg.reasoning > 0 {
            parts.push(format!("💭×{}", agg.reasoning));
        }
        if !parts.is_empty() {
            out.insert(turn_id, parts.join(" · "));
        }
    }
    out
}

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
    if !refs.enforced_rules.is_empty() {
        let glossary = autonoetic_gateway::constitution_glossary::format_enforced_rules(&refs.enforced_rules);
        if glossary.is_empty() {
            lines.push(format!("enforces:  {}", refs.enforced_rules.join(", ")));
        } else {
            lines.push(glossary);
        }
    }

    if let Some(payload) = &entry.payload {
        lines.push(String::new());
        lines.push("payload:".to_string());
        match serde_json::from_str::<serde_json::Value>(payload).ok() {
            Some(v) => {
                let unfolded = if entry.event_type == "agent.message" || entry.event_type == "operator.message" {
                    unfold_stringified_json(&expand_agent_message_payload(&v))
                } else {
                    unfold_stringified_json(&v)
                };
                render_payload_lines(&unfolded, &mut lines);
            }
            None => lines.push(format!("  {payload}")),
        }
    }
    lines
}

/// Render a JSON payload into human-readable lines. Unlike
/// `serde_json::to_string_pretty`, this splits string values that contain
/// `\n` into actual separate lines so they display properly in the detail
/// pane instead of as a single wrapped JSON string.
/// Payload string keys whose values are operator-facing prose — rendered as
/// markdown in the client detail pane (gateway stays format-agnostic).
const NARRATIVE_PAYLOAD_KEYS: &[&str] = &[
    "summary",
    "prose",
    "message",
    "text",
    "content",
    "body",
    "question",
    "reason",
    "explanation",
    "description",
    "detail",
    "answer",
    "output",
    "result",
    "synthesis",
    "guidance",
    "context",
    "risk_summary",
    "diagnosis",
    "recommendation",
];

fn is_narrative_payload_key(key: &str) -> bool {
    NARRATIVE_PAYLOAD_KEYS.contains(&key)
}

fn should_render_payload_as_narrative(key: &str, value: &str) -> bool {
    // Skip values that are valid structured JSON — those should be rendered
    // as pretty-printed JSON, not markdown. But a value that merely *starts*
    // with `{` but contains unescaped newlines (common when the agent emits
    // an io.returns envelope with literal newlines inside string values)
    // is NOT valid JSON and should be treated as narrative prose.
    if (value.starts_with('{') || value.starts_with('['))
        && serde_json::from_str::<serde_json::Value>(value).is_ok()
    {
        return false;
    }
    if is_narrative_payload_key(key) {
        return value.contains('\n')
            || value.chars().count() > 80
            || super::markdown::looks_like_narrative_content(value);
    }
    // For unknown keys, still render as narrative when the value clearly
    // contains markdown formatting — the agent may put rich text in
    // arbitrary fields. Single-line markdown signals (e.g. `**bold**`,
    // `# heading`, links) are enough; plain multiline prose without any
    // markdown marker is left as-is to avoid over-rendering data values.
    super::markdown::looks_like_markdown(value)
        || (value.contains('\n')
            && super::markdown::looks_like_narrative_content(value))
}

fn push_narrative_payload_lines(
    key: &str,
    value: &str,
    lines: &mut Vec<String>,
    inner: &str,
    comma: &str,
) {
    lines.push(format!("{inner}\"{key}\":"));
    lines.push(format!("{inner}  {}", super::markdown::NARRATIVE_MD_START));
    for sub in value.split('\n') {
        lines.push(format!("{inner}  {sub}"));
    }
    lines.push(format!("{inner}  {}{comma}", super::markdown::NARRATIVE_MD_END));
}

/// Variant of [`push_narrative_payload_lines`] for array elements — no `key:`
/// header, just the narrative markers so the TUI runs the value through the
/// markdown pipeline.
fn push_narrative_array_elem(
    value: &str,
    lines: &mut Vec<String>,
    inner: &str,
    comma: &str,
) {
    lines.push(format!("{inner}  {}", super::markdown::NARRATIVE_MD_START));
    for sub in value.split('\n') {
        lines.push(format!("{inner}  {sub}"));
    }
    lines.push(format!("{inner}  {}{comma}", super::markdown::NARRATIVE_MD_END));
}

/// Spaces added per JSON nesting level in the detail-pane payload renderer.
const PAYLOAD_INDENT: usize = 2;

fn render_payload_lines(v: &serde_json::Value, lines: &mut Vec<String>) {
    render_payload_lines_indent(v, lines, 1);
}

fn render_payload_lines_indent(v: &serde_json::Value, lines: &mut Vec<String>, depth: usize) {
    let pad = " ".repeat(depth * PAYLOAD_INDENT);
    let inner = " ".repeat((depth + 1) * PAYLOAD_INDENT);
    match v {
        serde_json::Value::Object(map) => {
            lines.push(format!("{pad}{{"));
            let last_idx = map.len().saturating_sub(1);
            for (i, (k, child)) in map.iter().enumerate() {
                let comma = if i < last_idx { "," } else { "" };
                match child {
                    serde_json::Value::String(s)
                        if should_render_payload_as_narrative(k, s) =>
                    {
                        push_narrative_payload_lines(k, s, lines, &inner, comma);
                    }
                    serde_json::Value::String(s) if s.contains('\n') => {
                        lines.push(format!("{inner}\"{k}\":"));
                        for sub in s.split('\n') {
                            lines.push(format!("{inner}  {sub}"));
                        }
                        if !comma.is_empty() {
                            lines.push(format!("{inner}{comma}"));
                        }
                    }
                    serde_json::Value::String(s)
                        if (s.starts_with('{') || s.starts_with('[')) && s.len() > 10 =>
                    {
                        lines.push(format!("{inner}\"{k}\":"));
                        render_jsonish_string(s, lines, depth + 2);
                        if !comma.is_empty() {
                            lines.push(format!("{inner}{comma}"));
                        }
                    }
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        lines.push(format!("{inner}\"{k}\":"));
                        render_payload_lines_indent(child, lines, depth + 1);
                        if !comma.is_empty() {
                            lines.push(format!("{inner}{comma}"));
                        }
                    }
                    other => {
                        let formatted = serde_json::to_string(other).unwrap_or_default();
                        if let serde_json::Value::String(s) = other {
                            if should_render_payload_as_narrative(k, s) {
                                push_narrative_payload_lines(k, s, lines, &inner, comma);
                                continue;
                            }
                        }
                        lines.push(format!("{inner}\"{k}\": {formatted}{comma}"));
                    }
                }
            }
            lines.push(format!("{pad}}}"));
        }
        serde_json::Value::Array(arr) => {
            lines.push(format!("{pad}["));
            let last_idx = arr.len().saturating_sub(1);
            for (i, elem) in arr.iter().enumerate() {
                let comma = if i < last_idx { "," } else { "" };
                match elem {
                    serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                        render_payload_lines_indent(elem, lines, depth + 1);
                        if !comma.is_empty() {
                            if let Some(last) = lines.last_mut() {
                                last.push_str(comma);
                            }
                        }
                    }
                    serde_json::Value::String(s)
                        if should_render_payload_as_narrative("", s) =>
                    {
                        push_narrative_array_elem(s, lines, &inner, comma);
                    }
                    serde_json::Value::String(s) if s.contains('\n') => {
                        for sub in s.split('\n') {
                            lines.push(format!("{inner}{sub}"));
                        }
                        if !comma.is_empty() {
                            lines.push(format!("{inner}{comma}"));
                        }
                    }
                    serde_json::Value::String(s)
                        if (s.starts_with('{') || s.starts_with('[')) && s.len() > 10 =>
                    {
                        render_jsonish_string(s, lines, depth + 1);
                        if !comma.is_empty() {
                            if let Some(last) = lines.last_mut() {
                                last.push_str(comma);
                            }
                        }
                    }
                    other => {
                        let formatted = serde_json::to_string(other).unwrap_or_default();
                        lines.push(format!("{inner}{formatted}{comma}"));
                    }
                }
            }
            lines.push(format!("{pad}]"));
        }
        other => {
            lines.push(format!("{pad}{}", serde_json::to_string(other).unwrap_or_default()));
        }
    }
}

/// Render a string that looks like JSON (possibly truncated). Tries to parse it
/// as structured JSON first, then falls back to bracket-repair for truncated
/// payloads, and finally displays the raw text if nothing works.
fn render_jsonish_string(s: &str, lines: &mut Vec<String>, depth: usize) {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
        render_payload_lines_indent(&parsed, lines, depth);
        return;
    }
    if let Some(repaired) = repair_truncated_json(s) {
        render_payload_lines_indent(&repaired, lines, depth);
        lines.push(format!("{}… (truncated)", " ".repeat(depth * PAYLOAD_INDENT)));
        return;
    }
    let pad = " ".repeat(depth * PAYLOAD_INDENT);
    for sub in s.split(", ") {
        lines.push(format!("{pad}{sub}"));
    }
}

/// Try to parse truncated JSON by counting unclosed brackets and appending
/// the needed closing characters. Returns None if repair doesn't yield valid JSON.
fn repair_truncated_json(s: &str) -> Option<serde_json::Value> {
    let mut open_braces: i32 = 0;
    let mut open_brackets: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for ch in s.chars() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' {
            escape = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '{' => open_braces += 1,
            '}' => open_braces -= 1,
            '[' => open_brackets += 1,
            ']' => open_brackets -= 1,
            _ => {}
        }
    }
    let close_brackets = open_brackets.max(0) as usize;
    let close_braces = open_braces.max(0) as usize;
    if close_brackets == 0 && close_braces == 0 {
        return None;
    }
    let mut repaired = s.to_string();
    // If we're inside a string, close it
    if in_string {
        repaired.push('"');
    }
    // Remove trailing incomplete token (partial key or value)
    let trimmed = repaired.trim_end_matches(|c: char| c != '{' && c != '}' && c != '[' && c != ']' && c != '"' && c != ',' && c != ':');
    let suffix = format!(
        "{}{}",
        "]".repeat(close_brackets),
        "}".repeat(close_braces),
    );
    serde_json::from_str(&format!("{trimmed}{suffix}")).ok()
}

/// Recursively walk a JSON value and replace any string field that parses as
/// valid JSON with the parsed value. Handles double-encoded payloads and
/// attempts bracket-repair for truncated JSON strings.
fn unfold_stringified_json(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let unfolded: serde_json::Map<String, serde_json::Value> = map
                .into_iter()
                .map(|(k, child)| {
                    let unrolled = match child.as_str() {
                        Some(s) if s.len() < 1_048_576 => {
                            if k == "message" {
                                coerce_message_string(s)
                            } else if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                                unfold_stringified_json(&parsed)
                            } else if let Some(repaired) = repair_truncated_json(s) {
                                unfold_stringified_json(&repaired)
                            } else {
                                child.clone()
                            }
                        }
                        _ => unfold_stringified_json(child),
                    };
                    (k.clone(), unrolled)
                })
                .collect();
            serde_json::Value::Object(unfolded)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(
                arr.iter()
                    .map(|elem| match elem.as_str() {
                        Some(s) if s.len() < 1_048_576 => {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                                unfold_stringified_json(&parsed)
                            } else if let Some(repaired) = repair_truncated_json(s) {
                                unfold_stringified_json(&repaired)
                            } else {
                                elem.clone()
                            }
                        }
                        _ => unfold_stringified_json(elem),
                    })
                    .collect(),
            )
        }
        other => other.clone(),
    }
}
/// Non-interactive rendering of a row. Always allocates: the `Line` variant
/// stores a structured `RowSpec`, not a pre-rendered string, so there is no
/// borrowed path. Multi-line rows (those with a `detail` preview) keep their
/// embedded `\n` — the CLI viewer prints them as multiple terminal lines via
/// `println!`. Collapsed runs render as `⟨N summary⟩`.
pub fn row_text(row: &RenderedRow) -> std::borrow::Cow<'_, str> {
    match row {
        // Single-line fast path: `<glyph> [<label>] <headline>`. No borrow
        // because the components live in separate fields of the spec.
        RenderedRow::Line(spec) if spec.detail.is_none() => {
            std::borrow::Cow::Owned(format!(
                "{} [{}] {}",
                altitude_glyph(spec.altitude),
                spec.actor_label,
                spec.headline
            ))
        }
        // Multi-line path: keep the `headline\ndetail` boundary intact so the
        // CLI viewer can present the preview on its own line.
        RenderedRow::Line(spec) => std::borrow::Cow::Owned(format!(
            "{} [{}] {}",
            altitude_glyph(spec.altitude),
            spec.actor_label,
            spec.to_plain_text()
        )),
        RenderedRow::Collapsed { count, summary, .. } => std::borrow::Cow::Owned(format!(
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
    fn row_copy_text_prefers_tool_token_else_visible_text() {
        // Tool row → the actionable token (args_preview), not the headline chrome.
        let exec = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::agent("coder.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "sandbox_exec",
                "args_preview": "pytest -k integration",
                "result": r#"{"ok":true,"stdout":"3 passed"}"#
            }),
        );
        assert_eq!(row_copy_text(&exec), "pytest -k integration");

        // Non-tool row → visible headline + detail.
        let msg = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": "delegating to the coder now" }),
        );
        assert!(row_copy_text(&msg).contains("delegating to the coder"));
    }

    #[test]
    fn collapsed_summary_names_the_tools_that_ran() {
        // A folded run should still tell the operator which tools were called,
        // deduping request+completion of the same call by call_id.
        let mk = |et: &str, tool: &str, call_id: &str| {
            entry(
                SessionRole::Specialist { kind: "coder".into() },
                Principal::agent("coder.default"),
                et,
                Altitude::Detail,
                serde_json::json!({ "tool_name": tool, "call_id": call_id }),
            )
        };
        let run = vec![
            mk("tool.requested", "read", "c1"),
            mk("tool.completed", "read", "c1"),
            mk("tool.requested", "read", "c2"),
            mk("tool.completed", "read", "c2"),
            mk("tool.requested", "grep", "c3"),
            mk("tool.completed", "grep", "c3"),
        ];
        let refs: Vec<&SessionTimelineEntry> = run.iter().collect();
        let summary = collapsed_summary(&refs);
        assert!(summary.starts_with("calls:"), "got: {summary}");
        assert!(summary.contains("read×2"), "got: {summary}");
        assert!(summary.contains("grep"), "got: {summary}");
        assert!(!summary.contains("grep×"), "grep ran once, no count: {summary}");
    }

    #[test]
    fn collapsed_summary_falls_back_to_event_types_without_tools() {
        let mk = |et: &str| {
            entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                et,
                Altitude::Detail,
                serde_json::json!({}),
            )
        };
        let run = vec![mk("llm.round"), mk("llm.round"), mk("turn.start")];
        let refs: Vec<&SessionTimelineEntry> = run.iter().collect();
        let summary = collapsed_summary(&refs);
        assert!(summary.starts_with("routine events"), "got: {summary}");
    }

    #[test]
    fn collapsed_summary_surfaces_reasoning_snippet() {
        let reasoning = |text: &str| {
            entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                "agent.reasoning",
                Altitude::Detail,
                serde_json::json!({ "reasoning": text }),
            )
        };
        let plumbing = |et: &str| {
            entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                et,
                Altitude::Detail,
                serde_json::json!({}),
            )
        };
        let run = vec![
            plumbing("turn.start"),
            reasoning("Checking whether the host is covered by remote_access.targets."),
            plumbing("llm.round"),
            reasoning("Targets look correct; proceeding to sandbox_exec."),
        ];
        let refs: Vec<&SessionTimelineEntry> = run.iter().collect();
        let summary = collapsed_summary(&refs);
        // Count + last snippet (same shape as an unsquashed reasoning row).
        assert!(
            summary.starts_with("💭×2 Targets look correct; proceeding to sandbox_exec."),
            "got: {summary}"
        );
        // Reasoning is not double-counted in the event-type fallback.
        assert!(
            !summary.contains("agent.reasoning"),
            "reasoning should not reappear as an event-type count: {summary}"
        );
        assert!(
            summary.contains("routine events"),
            "remaining plumbing still listed: {summary}"
        );
    }

    #[test]
    fn collapsed_summary_reasoning_alongside_tools() {
        let reasoning = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::agent("coder.default"),
            "agent.reasoning",
            Altitude::Detail,
            serde_json::json!({ "reasoning": "Need to resolve the artifact before exec." }),
        );
        let mk = |et: &str, tool: &str, call_id: &str| {
            entry(
                SessionRole::Specialist { kind: "coder".into() },
                Principal::agent("coder.default"),
                et,
                Altitude::Detail,
                serde_json::json!({ "tool_name": tool, "call_id": call_id }),
            )
        };
        let run = vec![
            reasoning,
            mk("tool.requested", "resolve", "c1"),
            mk("tool.completed", "resolve", "c1"),
        ];
        let refs: Vec<&SessionTimelineEntry> = run.iter().collect();
        let summary = collapsed_summary(&refs);
        assert!(
            summary.starts_with("💭 Need to resolve the artifact before exec."),
            "got: {summary}"
        );
        assert!(summary.contains("calls:"), "got: {summary}");
        assert!(summary.contains("resolve"), "got: {summary}");
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
    fn renders_guard_tripped_from_first_class_refs() {
        // The rule rides on the first-class `refs.enforced_rules`; the payload
        // carries no `rule_id`, proving the renderer reads refs, not payload.
        let mut e = entry(
            SessionRole::Runtime,
            Principal::agent("planner.default"),
            "guard.tripped",
            Altitude::Error,
            serde_json::json!({ "reason": "no_progress" }),
        );
        e.refs.enforced_rules = vec!["P-7.19".into()];
        let line = render_line(&e);
        assert!(line.starts_with("✗"), "guard trip should render at Error: {line}");
        assert!(line.contains("loop guard tripped: no_progress [P-7.19]"), "got: {line}");
    }

    #[test]
    fn renders_guard_tripped_payload_rule_id_fallback() {
        // Older events have no `refs.enforced_rules`; fall back to payload.
        let e = entry(
            SessionRole::Runtime,
            Principal::agent("planner.default"),
            "guard.tripped",
            Altitude::Error,
            serde_json::json!({ "reason": "no_progress", "rule_id": "P-7.19" }),
        );
        let line = render_line(&e);
        assert!(line.contains("loop guard tripped: no_progress [P-7.19]"), "got: {line}");
    }

    #[test]
    fn detail_view_shows_enforced_rules() {
        let mut e = entry(
            SessionRole::Operator,
            Principal::human("operator"),
            "approval.rejected",
            Altitude::Attention,
            serde_json::json!({ "request_id": "apr-9" }),
        );
        e.refs.enforced_rules = vec!["Ri-0.9".into(), "P-2.25".into()];
        let detail = format_detail(&e).join("\n");
        assert!(detail.contains("Ri-0.9"), "got: {detail}");
        assert!(detail.contains("P-2.25"), "got: {detail}");
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
    fn coalesce_folds_routine_runs_but_keeps_checkpoints_and_significant() {
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
            mk("llm.round", Altitude::Detail),
            mk("turn.start", Altitude::Detail),
            mk("approval.pending", Altitude::Attention), // checkpoint — breaks the run
            mk("turn.end", Altitude::Detail),            // lone routine ⇒ normal line
        ];
        let rows = coalesce(&entries);
        assert_eq!(rows.len(), 3, "run + checkpoint + lone routine");
        match &rows[0] {
            RenderedRow::Collapsed { count, summary, .. } => {
                assert_eq!(*count, 3);
                assert!(summary.contains("turn.start×2"));
                assert!(summary.contains("llm.round"));
            }
            other => panic!("expected collapsed run, got {other:?}"),
        }
        assert!(matches!(&rows[1], RenderedRow::Line(spec) if spec.headline.contains("APPROVAL REQUIRED")));
        // The trailing lone routine event is a normal line, not collapsed.
        assert!(matches!(&rows[2], RenderedRow::Line { .. }));
        assert!(row_text(&rows[0]).contains("⟨3 routine events"));
    }

    #[test]
    fn coalesce_folds_routine_tool_completed_but_keeps_significant() {
        // The clutter fix: a read-only tool.completed (e.g. workflow_wait)
        // folds as Routine, while a state-changing tool.completed
        // (e.g. agent_spawn) renders individually as Significant.
        let mk_tool = |et: &str, tool: &str| {
            entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                et,
                Altitude::Normal,
                serde_json::json!({ "tool_name": tool }),
            )
        };
        let entries = vec![
            mk_tool("tool.completed", "workflow_wait"), // routine
            mk_tool("tool.completed", "workflow_wait"), // routine  (run of 2)
            mk_tool("tool.completed", "agent_spawn"),   // significant — breaks run
            mk_tool("tool.completed", "resolve"),       // routine (lone ⇒ individual)
        ];
        let rows = coalesce(&entries);
        assert_eq!(rows.len(), 3, "run(2) + agent_spawn + lone resolve");
        assert!(
            matches!(&rows[0], RenderedRow::Collapsed { count: 2, .. }),
            "two read-only completions should fold; got {:?}",
            rows[0]
        );
        assert!(matches!(&rows[1], RenderedRow::Line { .. }));
        assert!(matches!(&rows[2], RenderedRow::Line { .. }));
    }

    #[test]
    fn paired_tool_request_is_dropped_completion_renders_once() {
        // One call, one row: a tool.requested whose tool.completed (same
        // call_id) is on the page is dropped — the completion headline already
        // carries the same key param. This kills the double row a significant
        // completion used to produce (lone requested flush + completion row).
        let mk = |et: &str, call_id: &str| {
            entry(
                SessionRole::Specialist { kind: "coder".into() },
                Principal::agent("coder.default"),
                et,
                Altitude::Normal,
                serde_json::json!({
                    "tool_name": "content_write",
                    "call_id": call_id,
                    "args_preview": "/tmp/out.rs"
                }),
            )
        };
        let entries = vec![mk("tool.requested", "c1"), mk("tool.completed", "c1")];
        let rows = coalesce(&entries);
        assert_eq!(rows.len(), 1, "only the completion renders; got {rows:?}");
        match &rows[0] {
            RenderedRow::Line(spec) => {
                assert!(spec.headline.contains("wrote"), "got: {}", spec.headline);
            }
            other => panic!("expected the completion line, got {other:?}"),
        }
    }

    #[test]
    fn in_flight_tool_request_still_renders() {
        // A request without its completion yet (call still open, or completion
        // on a later page) must stay visible — it is the in-flight signal.
        let mk = |et: &str, call_id: &str| {
            entry(
                SessionRole::Specialist { kind: "coder".into() },
                Principal::agent("coder.default"),
                et,
                Altitude::Normal,
                serde_json::json!({ "tool_name": "sandbox_exec", "call_id": call_id }),
            )
        };
        let entries = vec![mk("tool.requested", "c1"), mk("tool.requested", "c2")];
        let rows = coalesce(&entries);
        match &rows[..] {
            [RenderedRow::Collapsed { count: 2, .. }] => {}
            other => panic!("both requests survive (folded), got {other:?}"),
        }
    }

    #[test]
    fn request_without_call_id_is_never_paired_away() {
        // Older rows may lack call_id; without one we cannot prove the pair,
        // so the request always renders (previous behavior) — here as its own
        // row next to the significant completion.
        let requested = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.requested",
            Altitude::Normal,
            serde_json::json!({ "tool_name": "content_write", "args_preview": "/tmp/a.rs" }),
        );
        let completed = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({ "tool_name": "content_write", "args_preview": "/tmp/a.rs" }),
        );
        let rows = coalesce(&[requested, completed]);
        assert_eq!(rows.len(), 2, "got {rows:?}");
        assert!(matches!(&rows[0], RenderedRow::Line { .. }));
        assert!(matches!(&rows[1], RenderedRow::Line { .. }));
    }

    #[test]
    fn preserve_lines_cut_names_amount_and_escape_hatch() {
        // Truncation vocabulary: content cuts carry `…(+N chars · Enter)`.
        let long = "x".repeat(NARRATIVE_BODY_MAX + 500);
        let cut = preserve_lines(&long, NARRATIVE_BODY_MAX);
        assert!(cut.ends_with("…(+501 chars · Enter)"), "got: {cut}");
        let marker = "…(+501 chars · Enter)";
        assert_eq!(
            cut.chars().count(),
            (NARRATIVE_BODY_MAX - 1) + marker.chars().count()
        );
        // Under the cap: untouched.
        assert_eq!(preserve_lines("short", NARRATIVE_BODY_MAX), "short");
    }

    #[test]
    fn compact_count_formats_thousands() {
        assert_eq!(compact_count(950), "950");
        assert_eq!(compact_count(9_999), "9999");
        assert_eq!(compact_count(10_000), "10.0k");
        assert_eq!(compact_count(12_340), "12.3k");
    }

    #[test]
    fn turn_divider_labels_aggregate_calls_tokens_reasoning() {
        let mk_turn = |turn: &str, et: &str, payload: serde_json::Value| {
            let mut e = entry(
                SessionRole::Specialist { kind: "coder".into() },
                Principal::agent("coder.default"),
                et,
                Altitude::Detail,
                payload,
            );
            e.turn_id = Some(turn.into());
            e
        };
        let entries = vec![
            // Turn 3: two read calls (request/completion pairs dedupe), one
            // LLM round with tokens, two reasoning events.
            mk_turn("turn-000003", "tool.requested", serde_json::json!({"tool_name": "read", "call_id": "c1"})),
            mk_turn("turn-000003", "tool.completed", serde_json::json!({"tool_name": "read", "call_id": "c1"})),
            mk_turn("turn-000003", "tool.requested", serde_json::json!({"tool_name": "read", "call_id": "c2"})),
            mk_turn("turn-000003", "tool.completed", serde_json::json!({"tool_name": "read", "call_id": "c2"})),
            mk_turn("turn-000003", "tool.requested", serde_json::json!({"tool_name": "grep", "call_id": "c3"})),
            mk_turn("turn-000003", "llm.round", serde_json::json!({"input_tokens": 1200, "output_tokens": 340})),
            mk_turn("turn-000003", "agent.reasoning", serde_json::json!({"reasoning": "a"})),
            mk_turn("turn-000003", "agent.reasoning", serde_json::json!({"reasoning": "b"})),
            // Turn 4: narrative only — nothing to report, no label.
            mk_turn("turn-000004", "agent.message", serde_json::json!({"message": "done"})),
            // Turn 5 belongs to a different wanted-set run; excluded below.
            mk_turn("turn-000005", "llm.round", serde_json::json!({"input_tokens": 5})),
        ];
        let wanted: HashSet<String> = ["turn-000003", "turn-000004"]
            .into_iter()
            .map(String::from)
            .collect();
        let labels = turn_divider_labels(&entries, &wanted);
        assert_eq!(labels.len(), 1, "only turn 3 has something to report");
        let t3 = &labels["turn-000003"];
        assert!(t3.contains("3 calls: read×2, grep"), "got: {t3}");
        assert!(t3.contains("1.2k/340 tok"), "got: {t3}");
        assert!(t3.contains("💭×2"), "got: {t3}");
        // A narrative-only turn has no plumbing/cost/thought → no entry,
        // so its divider stays plain.
        assert!(!labels.contains_key("turn-000004"), "narrative-only turn: no stats");
        // Empty wanted set → no work.
        assert!(turn_divider_labels(&entries, &HashSet::new()).is_empty());
    }

    #[test]
    fn event_tier_classification() {
        let mk = |et: &str| {
            entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                et,
                Altitude::Normal,
                serde_json::json!({}),
            )
        };
        // Checkpoints
        for et in [
            "plan.pending", "plan.approved", "approval.pending", "approval.approved",
            "escalation.pending", "operator.message", "session.start", "workbench.created",
            "runtime.lock_drift", "user.ask.pending", "divergence.intervention",
            "security.escape_threshold",
        ] {
            assert_eq!(
                event_tier(&mk(et)),
                EventTier::Checkpoint,
                "{et} should be a checkpoint"
            );
        }
        // Significant — agent output, audits, AND failures (never folded).
        for et in [
            "agent.message", "digest_annotate",
            "llm.request_failed", "llm.empty_response", "guard.tripped",
            "tool.failed", "session.emergency_stop", "security.sandbox_escape",
            "egress.envelope_withheld", "egress.assertion_violation",
            "egress.boundary_refused", "egress.declassified",
        ] {
            assert_eq!(
                event_tier(&mk(et)),
                EventTier::Significant,
                "{et} should be significant (never folded)"
            );
        }
        // Unknown event type defaults to Significant (never folded).
        assert_eq!(
            event_tier(&mk("some.future.event_type")),
            EventTier::Significant,
            "unknown event type must not default to Routine (would hide it)"
        );
        // Routine — known plumbing only.
        for et in [
            "turn.start", "turn.end", "llm.round", "agent.reasoning",
            "tool.requested", "workflow.child_state", "workflow.join_satisfied",
            "egress.envelope_labeled", "egress.request_filtered",
            "egress.request_forwarded", "egress.provider_selected", "egress.relabel",
        ] {
            assert_eq!(event_tier(&mk(et)), EventTier::Routine, "{et} should be routine");
        }
    }

    #[test]
    fn checkpoint_and_significant_types_have_a_real_summary() {
        // Anti-drift guard: every event type classified as Checkpoint or
        // Significant in `event_tier` (i.e. consciously made non-foldable —
        // meant to always be seen) must also have a real `summarize()` arm.
        // Otherwise it silently falls through to the `other => other.to_string()`
        // fallback and the operator sees the raw event_type string instead of
        // a sentence — this happened for `operator.comment`, `session.start`,
        // `security.escape_threshold`, and others before this test existed.
        for et in CHECKPOINT_EVENT_TYPES.iter().chain(SIGNIFICANT_EVENT_TYPES.iter()) {
            let e = entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                et,
                Altitude::Normal,
                serde_json::json!({}),
            );
            let summary = summarize(&e);
            assert_ne!(
                &summary, et,
                "{et} falls back to the raw event_type string in summarize() — add a match arm"
            );
        }
    }

    #[test]
    fn egress_policy_declaration_summarizes_operator_readable() {
        // `/private` (provider constraint) and `/taint` (rule) both land here.
        let e = entry(
            SessionRole::Operator,
            Principal::human("operator"),
            "egress.session_policy",
            Altitude::Normal,
            serde_json::json!({
                "operation": "set",
                "set_by": "operator:tui",
                "detail": {
                    "rule_count": 1,
                    "rule_sources": ["email.*"],
                    "default_label": "local_only",
                },
            }),
        );
        let summary = summarize(&e);
        assert!(summary.contains("egress policy set"), "{summary}");
        assert!(summary.contains("1 rule"), "{summary}");
        assert!(summary.contains("email.*"), "{summary}");
        assert!(summary.contains("default local_only"), "{summary}");
        assert!(summary.contains("operator:tui"), "{summary}");

        // Clear has no detail — must not render a stale rule line.
        let cleared = entry(
            SessionRole::Operator,
            Principal::human("operator"),
            "egress.session_policy",
            Altitude::Normal,
            serde_json::json!({ "operation": "clear", "set_by": "operator:tui", "detail": null }),
        );
        let s2 = summarize(&cleared);
        assert!(s2.contains("cleared"), "{s2}");
        assert!(!s2.contains("email.*"), "{s2}");
    }

    #[test]
    fn egress_enforcement_rows_summarize_operator_readable() {
        // #972: each of the ten egress.* actions must read as a sentence in
        // the room — metadata only, no content.
        let mk = |et: &str, payload: serde_json::Value| {
            entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                et,
                Altitude::Normal,
                payload,
            )
        };
        let withheld = summarize(&mk(
            "egress.envelope_withheld",
            serde_json::json!({
                "tool_call_id": "tc_1",
                "target_sink": "remote_model",
                "label": ["local_model", "local_agent", "user_reply", "memory_persist"],
                "indication": "[withheld local_only content]",
            }),
        ));
        assert!(withheld.contains("withheld from remote_model"), "{withheld}");
        assert!(withheld.contains("tc_1"), "{withheld}");
        assert!(withheld.contains("local_only"), "{withheld}");

        let assertion = summarize(&mk(
            "egress.assertion_violation",
            serde_json::json!({
                "tool_call_id": "tc_2",
                "target_sink": "remote_model",
                "payload_digest": "deadbeef",
            }),
        ));
        assert!(assertion.contains("assertion violation"), "{assertion}");
        assert!(assertion.contains("tc_2"), "{assertion}");

        let refused = summarize(&mk(
            "egress.boundary_refused",
            serde_json::json!({
                "surface": "sandbox.share_net",
                "label_name": "local_only",
                "reason": "network sink not cleared",
            }),
        ));
        assert!(refused.contains("boundary refused: sandbox.share_net"), "{refused}");
        assert!(refused.contains("local_only"), "{refused}");

        // Payload built through the real serialization path (tagged enum) so
        // the test can't drift from the emitter's shape again.
        let declass_target = serde_json::to_value(
            autonoetic_types::egress::EgressDeclassificationTarget::SourcePattern(
                "session:root-1".to_string(),
            ),
        )
        .expect("declass target serializes");
        let mut declass_payload = serde_json::json!({
            "allowed_sink": "network",
            "source_approval_id": "apr-1",
        });
        declass_payload["target"] = declass_target;
        let declassified = summarize(&mk("egress.declassified", declass_payload));
        assert!(
            declassified.contains("egress widened: source_pattern:session:root-1 → network"),
            "{declassified}"
        );

        // High-volume metadata rows read sanely too (dial-down view).
        let labeled = summarize(&mk(
            "egress.envelope_labeled",
            serde_json::json!({
                "tool_name": "email.send",
                "label": ["local_model", "local_agent", "user_reply", "memory_persist"],
                "matched_rules": ["email.*"],
            }),
        ));
        assert!(labeled.contains("email.send labeled → local_only"), "{labeled}");
        assert!(labeled.contains("email.*"), "{labeled}");

        let filtered = summarize(&mk(
            "egress.request_filtered",
            serde_json::json!({
                "target_sink": "remote_model",
                "withheld_count": 2,
                "included_count": 3,
            }),
        ));
        assert!(filtered.contains("2 withheld, 3 included"), "{filtered}");

        let forwarded = summarize(&mk(
            "egress.request_forwarded",
            serde_json::json!({
                "model": "claude",
                "target_sink": "remote_model",
                "withheld_count": 1,
                "included_count": 4,
            }),
        ));
        assert!(forwarded.contains("1 withheld, 4 included (remote_model)"), "{forwarded}");
        assert!(forwarded.contains("claude"), "{forwarded}");

        let routed = summarize(&mk(
            "egress.provider_selected",
            serde_json::json!({
                "batch_label_name": "local_only",
                "chosen_preset": "ollama",
                "no_eligible_provider": false,
            }),
        ));
        assert!(routed.contains("local_only → ollama"), "{routed}");

        let refused_route = summarize(&mk(
            "egress.provider_selected",
            serde_json::json!({
                "batch_label_name": "local_only",
                "chosen_preset": null,
                "no_eligible_provider": true,
            }),
        ));
        assert!(refused_route.contains("no provider cleared for local_only"), "{refused_route}");

        let relabeled = summarize(&mk(
            "egress.relabel",
            serde_json::json!({
                "kind": "memory",
                "count": 1,
                "new_label_name": "local_only",
            }),
        ));
        assert!(relabeled.contains("memory → local_only"), "{relabeled}");
    }

    #[test]
    fn egress_label_display_stays_honest_on_empty_or_missing() {
        // A missing/empty label must never render as fully cleared — "?" or
        // "blocked", matching the labels panel's sinks_label_name.
        let mk = |et: &str, payload: serde_json::Value| {
            entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                et,
                Altitude::Normal,
                payload,
            )
        };
        let empty = summarize(&mk(
            "egress.envelope_withheld",
            serde_json::json!({
                "tool_call_id": "tc_1",
                "target_sink": "remote_model",
                "label": [],
                "indication": "[withheld]",
            }),
        ));
        assert!(empty.contains("blocked"), "{empty}");

        let missing = summarize(&mk(
            "egress.envelope_labeled",
            serde_json::json!({ "tool_name": "email.send", "matched_rules": [] }),
        ));
        assert!(missing.contains("→ ?"), "{missing}");
    }

    #[test]
    fn coalesce_never_folds_attention_or_error_rows() {
        // Defense-in-depth: even if a high-importance event were mis-classified
        // as Routine, the altitude guard in coalesce_indexed must keep it
        // individual. Attention/Error rows never collapse into a run.
        let mk_at = |et: &str, alt: Altitude| {
            entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                et,
                alt,
                serde_json::json!({}),
            )
        };
        let entries = vec![
            mk_at("turn.start", Altitude::Detail),                 // routine, folds
            mk_at("user.ask.pending", Altitude::Attention),        // checkpoint — never folds
            mk_at("turn.end", Altitude::Detail),                   // routine, folds (lone ⇒ line)
            mk_at("llm.request_failed", Altitude::Error),          // significant — never folds
        ];
        let rows = coalesce(&entries);
        // No row may be a Collapsed run containing an Attention/Error event.
        for r in &rows {
            match r {
                RenderedRow::Collapsed { summary, .. } => {
                    assert!(
                        !summary.contains("user.ask.pending") && !summary.contains("llm.request_failed"),
                        "high-importance event leaked into a collapsed run: {summary}"
                    );
                }
                _ => {}
            }
        }
        // The Attention and Error rows must be present as individual lines.
        assert!(
            rows.iter().any(|r| matches!(r, RenderedRow::Line(s) if s.headline.contains("CLARIFICATION") || s.headline.contains("user.ask"))),
            "user.ask.pending must render individually"
        );
    }

    #[test]
    fn event_tier_tool_completed_splits_by_tool_name() {
        let mk = |tool: &str| {
            entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                "tool.completed",
                Altitude::Normal,
                serde_json::json!({ "tool_name": tool }),
            )
        };
        // Significant tools
        for tool in ["agent_spawn", "content_write", "artifact_build", "sandbox_exec"] {
            assert_eq!(
                event_tier(&mk(tool)),
                EventTier::Significant,
                "tool.completed({tool}) should be significant"
            );
        }
        // Routine tools
        for tool in ["resolve", "artifact_inspect", "workflow_wait", "agent_list"] {
            assert_eq!(
                event_tier(&mk(tool)),
                EventTier::Routine,
                "tool.completed({tool}) should be routine"
            );
        }
    }

    #[test]
    fn coalesce_indexed_maps_rows_to_sources() {
        let mk = |et: &str, alt: Altitude| {
            entry(SessionRole::Planner, Principal::agent("planner.default"), et, alt, serde_json::json!({}))
        };
        let entries = vec![
            mk("turn.start", Altitude::Detail),   // 0 ┐ run
            mk("turn.end", Altitude::Detail),     // 1 ┘
            mk("approval.pending", Altitude::Attention), // 2 single (checkpoint)
            mk("turn.start", Altitude::Detail),   // 3 lone routine → single
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
    fn format_detail_indents_nested_payload_keys() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({
                "message": {
                    "artifact_ref": "ar.cb049a325507",
                    "reason": "Artifact ready.",
                    "status": "ok"
                }
            }),
        );
        let lines = format_detail(&e);
        let artifact_line = lines
            .iter()
            .find(|l| l.contains("artifact_ref"))
            .expect("nested key present");
        assert!(
            artifact_line.starts_with("      \"artifact_ref\""),
            "expected 6-space indent for nested key, got: {artifact_line:?}"
        );
        let outer = lines.iter().find(|l| l.trim_start() == "{").expect("outer brace");
        assert_eq!(outer, "  {", "top-level object starts at 2 spaces");
    }

    #[test]
    fn format_detail_marks_narrative_summary_for_markdown_rendering() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({
                "message": {
                    "status": "ok",
                    "summary": "## Diagnosis\n\n```python\nimport autonoetic_sdk\n```\n\nUse file-based state instead."
                }
            }),
        );
        let detail = format_detail(&e).join("\n");
        assert!(detail.contains("@@NARRATIVE@@"));
        assert!(detail.contains("import autonoetic_sdk"));
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
    fn user_ask_detail_shows_context_for_divergence_sentinel() {
        let e = entry(
            SessionRole::Specialist {
                kind: "researcher".to_string(),
            },
            Principal::agent("researcher.default"),
            "user.ask.pending",
            Altitude::Attention,
            serde_json::json!({
                "interaction_id": "ui-abc123",
                "kind": "divergence_sentinel",
                "question": "Critical divergence in 'researcher.default' turn 1: 10 consecutive cycles without meaningful progress (limit 10)",
                "context": "Signals:\n- loop_pressure (critical): 10 consecutive cycles without meaningful progress (limit 10)",
                "options": [
                    {"id": "ack", "label": "Acknowledge"},
                    {"id": "stop", "label": "Stop"},
                ],
                "allow_freeform": true,
            }),
        );
        let spec = render_spec(&e);
        assert!(spec.headline.contains("10 consecutive cycles"));
        let detail = spec.detail.expect("should have detail");
        assert!(detail.contains("context:"));
        assert!(detail.contains("loop_pressure"));
    }

    #[test]
    fn user_ask_detail_shows_choices_multiline() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "user.ask.pending",
            Altitude::Attention,
            serde_json::json!({
                "question": "Which approach?",
                "options": [
                    {"id": "o1", "label": "Option A"},
                    {"id": "o2", "label": "Option B"},
                    {"id": "o3", "label": "Option C"},
                ],
                "allow_freeform": true,
            }),
        );
        let spec = render_spec(&e);
        assert!(spec.headline.contains("CLARIFICATION"));
        assert!(spec.headline.contains("Which approach?"));
        assert_eq!(spec.tone, RowTone::OperatorGate);
        let detail = spec.detail.expect("should have detail");
        assert!(detail.contains("[1] Option A"));
        assert!(detail.contains("[2] Option B"));
        assert!(detail.contains("[3] Option C"));
        assert!(detail.contains("or type your own"));
        assert!(detail.contains("Enter/i/r"));
    }

    #[test]
    fn user_ask_detail_no_freeform() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "user.ask.pending",
            Altitude::Attention,
            serde_json::json!({
                "question": "Confirm?",
                "options": [
                    {"id": "o1", "label": "Yes"},
                    {"id": "o2", "label": "No"},
                ],
                "allow_freeform": false,
            }),
        );
        let spec = render_spec(&e);
        assert_eq!(spec.tone, RowTone::OperatorGate);
        let detail = spec.detail.expect("should have detail");
        assert!(!detail.contains("type your own"));
        assert!(detail.contains("[1] Yes"));
        assert!(detail.contains("[2] No"));
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
    fn wrap_display_lines_preserves_newlines() {
        let text = "What: run a test\nWhy gated: artifact exec\nAnalysis:\n- import urllib\n- line 2";
        let out = wrap_display_lines(text, 76);
        assert_eq!(out.len(), 5, "should preserve 5 source lines: {out:?}");
        assert_eq!(out[0], "What: run a test");
        assert_eq!(out[1], "Why gated: artifact exec");
        assert_eq!(out[2], "Analysis:");
        assert_eq!(out[3], "- import urllib");
        assert_eq!(out[4], "- line 2");
    }

    #[test]
    fn wrap_display_lines_preserves_blank_lines_and_wraps_long_lines() {
        let text = "paragraph one\n\nvery long word that exceeds the width limit and must wrap\nshort";
        let out = wrap_display_lines(text, 20);
        // Blank line preserved
        assert_eq!(out[1], "", "blank line should be preserved: {out:?}");
        // Last line is short
        assert_eq!(out[out.len() - 1], "short");
        // No line exceeds the width
        for line in &out {
            assert!(
                line.chars().count() <= 20 || !line.contains(' '),
                "line should be wrapped: {line:?}"
            );
        }
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
    fn tool_requested_sandbox_exec_shows_the_command_not_just_the_tool_name() {
        let e = entry(
            SessionRole::Specialist { kind: "researcher".into() },
            Principal::agent("researcher.default"),
            "tool.requested",
            Altitude::Detail,
            serde_json::json!({
                "tool_name": "sandbox_exec",
                "arguments": r#"{"command":"pytest -k foo"}"#,
            }),
        );
        // Exec rows lead with `▶ <command>` so the operator sees what ran.
        assert!(
            render_line(&e).contains("▶ pytest -k foo"),
            "got: {}",
            render_line(&e)
        );
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
    fn llm_empty_response_renders_model_and_tokens() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "llm.empty_response",
            Altitude::Error,
            serde_json::json!({
                "model": "nvidia/nemotron-3-ultra-550b-a55b:free",
                "stop_reason": "EndTurn",
                "input_tokens": 33332,
                "output_tokens": 0,
                "preceding": ["read_file"],
            }),
        );
        let line = render_line(&e);
        assert!(line.starts_with("✗"));
        assert!(line.contains("LLM empty response"));
        assert!(line.contains("nemotron"));
        assert!(line.contains("tokens=33332/0"));
        assert!(line.contains("after: read_file"));
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
    fn scheduled_job_completed_renders_result_summary() {
        let e = entry(
            SessionRole::Specialist { kind: "fibonacci".into() },
            Principal::agent("fibonacci-next"),
            "scheduled_job.completed",
            Altitude::Normal,
            serde_json::json!({
                "agent_id": "fibonacci-next",
                "result_summary": "next=21 a=8 b=13",
            }),
        );
        let spec = render_spec(&e);
        assert!(spec.headline.contains("scheduled result"));
        assert!(spec.headline.contains("fibonacci-next"));
        assert_eq!(
            spec.detail.as_deref(),
            Some("next=21 a=8 b=13")
        );
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
    fn coalesce_indexed_hides_session_escalate_mirror_when_escalation_pending_exists() {
        let approval_id = "apr-esc-esc_ed2ebff710e4";
        let mut esc = entry(
            SessionRole::Specialist {
                kind: "weather-lo".into(),
            },
            Principal::agent("weather-lookup"),
            "escalation.pending",
            Altitude::Attention,
            serde_json::json!({
                "escalation_id": "esc_ed2ebff710e44b27bf4cb82d2e6f4547",
                "agent_id": "weather-lookup",
                "revision_id": "1",
                "synthesis": "All federation roles passed.",
                "escalation_type": "promotion_review",
            }),
        );
        esc.refs.approval_request_id = Some(approval_id.into());
        let appr = entry(
            SessionRole::Specialist {
                kind: "weather-lo".into(),
            },
            Principal::agent("weather-lookup"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": approval_id,
                "action": "session_escalate",
                "reason": "Promotion review for agent 'weather-lookup'",
                "context": "All federation roles passed.",
            }),
        );
        let rows = coalesce_indexed(&[esc, appr]);
        assert_eq!(rows.len(), 1);
        let RenderedRow::Line(spec) = &rows[0].0 else {
            panic!("expected line row");
        };
        assert!(spec.headline.contains("PROMOTION ESCALATION"));
        assert!(!spec.headline.contains("SESSION ESCALATION"));
    }

    #[test]
    fn session_escalate_without_escalation_pending_still_renders() {
        let appr = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": "esc-human-1",
                "action": "session_escalate",
                "reason": "stuck on integration test",
                "context": "needs operator guidance",
            }),
        );
        let rows = coalesce_indexed(&[appr]);
        assert_eq!(rows.len(), 1);
        let RenderedRow::Line(spec) = &rows[0].0 else {
            panic!("expected line row");
        };
        assert!(spec.headline.contains("SESSION ESCALATION"));
    }

    #[test]
    fn session_escalate_mirror_with_apr_esc_prefix_always_suppressed() {
        // Even without the escalation.pending card in the same batch, an
        // approval.pending whose request_id starts with apr-esc- is a mirror
        // and should produce no visible row.
        let appr = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": "apr-esc-abc123",
                "action": "session_escalate",
                "reason": "Promotion review for agent 'weather-agent'",
                "context": "All federation roles passed.",
            }),
        );
        let rows = coalesce_indexed(&[appr]);
        assert!(rows.is_empty());
    }

    #[test]
    fn non_escalate_approval_pending_still_renders() {
        let appr = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": "apr-normal-1",
                "action": "revision_promote",
                "agent_id": "weather-agent",
                "reason": "acknowledge capabilities",
            }),
        );
        let rows = coalesce_indexed(&[appr]);
        assert_eq!(rows.len(), 1);
        let RenderedRow::Line(spec) = &rows[0].0 else {
            panic!("expected line row");
        };
        assert!(spec.headline.contains("PROMOTION APPROVAL"));
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

    #[test]
    fn row_tone_classifies_agent_vs_tool_rows() {
        assert_eq!(row_tone("agent.message"), RowTone::AgentNarrative);
        assert_eq!(row_tone("operator.message"), RowTone::AgentNarrative);
        assert_eq!(row_tone("tool.completed"), RowTone::ToolCall);
        assert_eq!(row_tone("agent.reasoning"), RowTone::Reasoning);
        assert_eq!(row_tone("approval.pending"), RowTone::Default);
        let appr = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": "apr-1",
                "action": "sandbox_exec",
                "command": "curl example.com",
            }),
        );
        assert_eq!(tone_for_entry(&appr), RowTone::OperatorGate);
    }

    #[test]
    fn render_spec_agent_message_keeps_full_body_for_wrapped_display() {
        let greeting = "Hello! I'm your planner agent. I can help you research topics, build agents, execute code, set up credentials, and coordinate complex workflows. What would you like to work on?";
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": greeting }),
        );
        let spec = render_spec(&e);
        assert!(spec.headline.is_empty(), "body lives in detail for wrapping");
        assert_eq!(spec.detail.as_deref(), Some(greeting));
        assert_eq!(spec.tone, RowTone::AgentNarrative);
    }

    /// `agent.peer_message` rows render a readable headline (they used to fall
    /// through to the raw event type), expose their `message_id` for the egress
    /// label marker, and read as narrative rows (#971 proposal 2).
    #[test]
    fn render_spec_peer_message_renders_body_and_exposes_message_id() {
        let e = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::agent("email.reader"),
            "agent.peer_message",
            Altitude::Normal,
            serde_json::json!({
                "message_id": "msg-peer-1",
                "sender_agent_id": "email.reader",
                "message": "Summarized 3 unread threads — all local_only"
            }),
        );
        assert_eq!(
            agent_message_id(&e).as_deref(),
            Some("msg-peer-1"),
            "the payload message_id keys the label lookup"
        );
        let spec = render_spec(&e);
        assert!(spec.headline.contains("Summarized 3 unread threads"));
        assert!(spec.headline.contains("⇄"), "peer traffic is visually distinct");
        assert_eq!(spec.tone, RowTone::AgentNarrative);
    }

    #[test]
    fn agent_message_id_is_none_for_non_peer_message_events() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message_id": "msg-1", "message": "hi" }),
        );
        assert_eq!(agent_message_id(&e), None);
    }

    #[test]
    fn render_spec_structured_agent_message_uses_summary_title_and_result_sketch() {
        let msg = serde_json::json!({
            "status": "ok",
            "summary": "Here is a walkthrough of the fibonacci-next agent's code and its state management model.",
            "result": {
                "agent_id": "fibonacci-next",
                "explanation": { "overview": "pure script", "files": { "main.py": "entry" } },
                "state_management": { "model": "stateless" }
            }
        });
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": msg }),
        );
        let spec = render_spec(&e);
        assert!(
            spec.headline.is_empty(),
            "plain summary sentences render as formatted body, not a flat headline"
        );
        assert!(!spec.headline.starts_with('{'), "title must not be raw JSON");
        let detail = spec.detail.expect("result subline");
        assert!(detail.contains("walkthrough"));
        assert!(detail.contains("[ok]"));
        assert!(detail.contains("agent: fibonacci-next"));
        assert!(detail.contains("explanation (+2 fields)"));
        assert!(detail.contains("state_management (+1 fields)"));
    }

    #[test]
    fn render_spec_structured_agent_message_flattens_string_result_facts() {
        let msg = serde_json::json!({
            "status": "ok",
            "summary": "fibonacci-next is a stateless stdin/stdout script.",
            "result": {
                "agent_id": "fibonacci-next",
                "entrypoint": "main.py",
                "tests": "12 passing in test_main.py",
                "state": "none — pure computation"
            }
        });
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": msg }),
        );
        let spec = render_spec(&e);
        let detail = spec.detail.expect("flat result prose");
        assert!(detail.contains("[ok]"));
        assert!(detail.contains("entrypoint: main.py"));
        assert!(!detail.contains("(+"), "flat strings must not become field-count sketches");
    }

    #[test]
    fn render_spec_multi_line_planner_intro_keeps_full_summary_in_body() {
        let msg = serde_json::json!({
            "status": "ok",
            "summary": "I'm the Collaborative Planner — your lead agent.\n\nWhat I do:\n\nPlan & coordinate.\n\nMy capabilities:\n\nPlanFrame management.",
        });
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": msg }),
        );
        let spec = render_spec(&e);
        assert!(spec.headline.is_empty(), "multi-line intros render as formatted body");
        let detail = spec.detail.expect("full summary in detail");
        assert!(detail.contains("[ok]"));
        assert!(detail.contains("What I do:"));
        assert!(detail.contains("My capabilities:"));
    }

    #[test]
    fn render_spec_markdown_summary_uses_heading_title_and_prose_body() {
        let msg = serde_json::json!({
            "status": "ok",
            "summary": "## fibonacci-next Agent\n\nWhat it does:\n\nfibonacci-next is a stateless script agent.",
            "result": { "agent_id": "fibonacci-next", "entrypoint": "main.py" }
        });
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": msg }),
        );
        let spec = render_spec(&e);
        assert_eq!(spec.headline, "fibonacci-next Agent");
        let detail = spec.detail.expect("subline + body");
        assert!(detail.contains("[ok]"));
        assert!(detail.contains("entrypoint: main.py"));
        assert!(detail.contains("What it does:"));
        assert!(detail.contains("stateless script agent"));
        assert!(!detail.starts_with('{'), "must not show raw JSON");
    }

    #[test]
    fn render_spec_repairs_truncated_json_message_string() {
        let msg = serde_json::json!({
            "status": "ok",
            "summary": "## fibonacci-next Agent\n\nWhat it does:\n\nfibonacci-next is a stateless script agent that computes the next Fibonacci number.",
            "result": { "agent_id": "fibonacci-next", "entrypoint": "main.py" }
        });
        let mut raw = msg.to_string();
        if raw.chars().count() > 180 {
            raw = raw.chars().take(179).collect();
            raw.push('…');
        }
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": raw }),
        );
        let spec = render_spec(&e);
        assert_eq!(spec.headline, "fibonacci-next Agent");
        assert!(!spec.headline.starts_with('{'));
        let detail = spec.detail.expect("repaired structured detail");
        assert!(detail.contains("[ok]"));
    }

    #[test]
    fn render_spec_structured_agent_message_surfaces_failed_status_and_error() {
        let msg = serde_json::json!({
            "status": "failed",
            "summary": "Could not install the agent.",
            "error": "artifact_ref kind mismatch",
            "result": { "agent_id": "fibonacci-next" }
        });
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": msg }),
        );
        let spec = render_spec(&e);
        let detail = spec.detail.expect("failed subline");
        assert!(detail.contains("[failed]"));
        assert!(detail.contains("error: artifact_ref kind mismatch"));
        assert!(detail.contains("agent: fibonacci-next"));
    }

    #[test]
    fn render_spec_splits_prose_prefix_from_embedded_json_message() {
        let structured = serde_json::json!({
            "status": "ok",
            "summary": "## Installation Status\n\n**All federation gates passed.**",
            "result": {
                "agent_id": "fibonacci-next",
                "approval_id": "apr-esc-esc_eba476c5dfe3",
                "blocker": "Operator approval pending"
            }
        });
        let message = format!(
            "All federation gates passed and the builder is ready.\n\n{}",
            structured
        );
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": message }),
        );
        let spec = render_spec(&e);
        assert!(spec.headline.contains("federation gates passed"));
        assert!(!spec.headline.contains('"'), "headline must not contain JSON");
        let detail = spec.detail.expect("structured detail");
        assert!(detail.contains("[ok]"));
        assert!(detail.contains("approval_id"));
        assert!(detail.contains("Installation Status") || detail.contains("federation gates passed"));

        let lines = format_detail(&e);
        let payload = lines.join("\n");
        assert!(payload.contains("\"prose\":"));
        assert!(payload.contains("\"summary\":"));
        assert!(
            !payload.contains("\"message\":\n      All federation"),
            "raw glued message"
        );
    }

    #[test]
    fn render_spec_sandbox_approval_shows_summary_and_content_ref_hint() {
        let appr = entry(
            SessionRole::Planner,
            Principal::agent("researcher.default"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": "apr-59f8",
                "approval_level": "operator",
                "action": "sandbox_exec",
                "command": "cnt_f57014c6",
                "command_kind": "content_ref",
                "command_hint": "Content handle — not a shell command.",
                "host_patterns": ["api.open-meteo.com"],
                "intent": "Run weather fetch script against Open-Meteo",
                "summary": "What will run:\ncnt_f57014c6\n\nWhy approval is required:\nRemote URL detected in script",
            }),
        );
        let spec = render_spec(&appr);
        let detail = spec.detail.expect("approval card body");
        assert!(detail.contains("content ref"));
        assert!(detail.contains("purpose:"));
        assert!(detail.contains("Open-Meteo"));
        assert!(detail.contains("details:"));
        assert!(detail.contains("Why approval is required"));
        assert!(detail.contains("api.open-meteo.com"));
    }

    #[test]
    fn render_spec_operator_gate_cards_are_high_visibility() {
        let appr = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": "apr-99",
                "action": "sandbox_exec",
                "command": "cargo test",
                "risk_summary": "runs tests in sandbox",
            }),
        );
        let spec = render_spec(&appr);
        assert_eq!(spec.tone, RowTone::OperatorGate);
        assert!(spec.headline.contains("APPROVAL REQUIRED"));
        let detail = spec.detail.expect("approval card body");
        assert!(detail.contains("apr-99"));
        assert!(detail.contains("y approve"));

        let ask = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "user.ask.pending",
            Altitude::Attention,
            serde_json::json!({
                "interaction_id": "int-42",
                "question": "Which database should we use?",
                "options": [{"id": "a", "label": "Postgres"}, {"id": "b", "label": "SQLite"}],
                "allow_freeform": false,
            }),
        );
        let ask_spec = render_spec(&ask);
        assert_eq!(ask_spec.tone, RowTone::OperatorGate);
        assert!(ask_spec.headline.contains("CLARIFICATION"));
        let ask_detail = ask_spec.detail.expect("ask card body");
        assert!(ask_detail.contains("[1] Postgres"));
        assert!(ask_detail.contains("Enter/i/r"));
    }

    #[test]
    fn render_spec_credential_prompt_card_surfaces_egress_scope() {
        // #1105: the secret-entry card approves the egress scope too — the
        // hosts must be on the card, never buried in the payload.
        let e = entry(
            SessionRole::Planner,
            Principal::agent("credential_onboarding.default"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": "apr-cred1",
                "action": "credential_prompt",
                "service": "mockweather",
                "secret_fields": ["api_key"],
                "inject_as": "header:X-Api-Key",
                "allowed_hosts": ["api.mockweather.local", "*.weather.example"],
            }),
        );
        let spec = render_spec(&e);
        let detail = spec.detail.expect("approval card body");
        assert!(detail.contains("egress scope (allowed_hosts):"));
        assert!(detail.contains("api.mockweather.local"));
        assert!(detail.contains("*.weather.example"));
        assert!(detail.contains("egress scope for the credential's lifetime"));
        assert!(
            !detail.contains("pre-approved for this session"),
            "credential scope must not read as a session grant: {detail}"
        );

        // Wildcard egress must be unmistakable.
        let e2 = entry(
            SessionRole::Planner,
            Principal::agent("credential_onboarding.default"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": "apr-cred2",
                "action": "credential_prompt",
                "service": "anyapi",
                "secret_fields": ["token"],
                "allowed_hosts": ["*"],
            }),
        );
        let detail2 = render_spec(&e2).detail.expect("approval card body");
        assert!(detail2.contains("WILDCARD: the secret can be sent to ANY host"));
    }

    #[test]
    fn render_spec_session_escalate_approval_shows_guidance_context() {
        let appr = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": "esc-abc123",
                "action": "session_escalate",
                "approval_level": "operator",
                "requested_by_agent_id": "planner.default",
                "urgency": "normal",
                "reason": "Unit tests require live network and cannot be evaluated",
                "context": "Federation blocked on environment.",
                "suggested_actions": ["accept without dynamic evidence", "retry with mocks"],
            }),
        );
        let spec = render_spec(&appr);
        assert!(spec.headline.contains("SESSION ESCALATION"));
        let detail = spec.detail.expect("session escalate detail");
        assert!(detail.contains("planner.default"));
        assert!(detail.contains("live network"));
        assert!(detail.contains("Federation blocked"));
        assert!(detail.contains("retry with mocks"));
    }

    #[test]
    fn render_spec_revision_promote_approval_shows_agent_context() {
        let appr = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "approval.pending",
            Altitude::Attention,
            serde_json::json!({
                "request_id": "apr-promo",
                "action": "revision_promote",
                "approval_level": "elevated",
                "agent_id": "weather-lookup",
                "revision_id": "rev_sha256:abc123",
                "summary": "Promote after federation pass",
                "added_capabilities": ["NetworkAccess(hosts=[api.open-meteo.com])"],
                "output_label": "local_only",
                "confirm_phrase": "promote weather-lookup rev_sha256:abc123",
            }),
        );
        let spec = render_spec(&appr);
        assert!(spec.headline.contains("PROMOTION APPROVAL"));
        assert!(spec.headline.contains("weather-lookup"));
        let detail = spec.detail.expect("promotion detail");
        assert!(detail.contains("weather-lookup"));
        assert!(detail.contains("rev_sha256"));
        assert!(detail.contains("federation pass"));
        assert!(detail.contains("NetworkAccess"));
        // The candidate's declared output floor surfaces so an operator can see
        // they are admitting a local-only bundle (#971).
        assert!(detail.contains("output floor"));
        assert!(detail.contains("local_only"));
        assert!(detail.contains("Esc peek timeline"));
    }

    /// A spawn whose completed result carries the target's declared output floor
    /// marks the spawn row headline with it (#971) — an operator sees that a
    /// delegation went to a local-only bundle. Distinct from the runtime taint
    /// marker (■), which is the spawn *result's* live label.
    #[test]
    fn render_spec_spawn_row_marks_the_target_output_floor() {
        let spawn = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "agent_spawn",
                "result": {
                    "ok": true,
                    "status": "queued",
                    "agent_id": "email.reader",
                    "target_egress_output_label": "local_only"
                }
            }),
        );
        assert_eq!(
            agent_spawn_output_label(&spawn).as_deref(),
            Some("local_only"),
            "the floor is extracted from the completed result"
        );
        let spec = render_spec(&spawn);
        assert!(
            spec.headline.contains("spawn → email.reader"),
            "headline names the target: {}",
            spec.headline
        );
        assert!(
            spec.headline.contains("local_only"),
            "headline marks the declared floor: {}",
            spec.headline
        );
    }

    /// A spawn to an unrestricted bundle (no floor) shows no floor tag — absence
    /// is the unrestricted state, not a missing marker.
    #[test]
    fn render_spec_spawn_row_without_a_floor_has_no_tag() {
        let spawn = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "agent_spawn",
                "result": {"ok": true, "status": "queued", "agent_id": "planner.default"}
            }),
        );
        assert_eq!(agent_spawn_output_label(&spawn), None);
        let spec = render_spec(&spawn);
        assert!(spec.headline.contains("spawn → planner.default"));
        assert!(!spec.headline.contains("unrestricted"));
        assert!(!spec.headline.contains("· "));
    }

    #[test]
    fn render_spec_plan_proposal_card_hides_raw_json_envelope() {
        let msg = serde_json::json!({
            "status": "awaiting_approval",
            "plan_id": "plan-af1e431bc8a7",
            "summary": "Proposed a 4-step plan to fix the fibonacci-next agent.",
            "result": {
                "fix": "main.py line 14: sdk.init() → sdk = autonoetic_sdk.init()",
                "next_step": "Operator approval via /plan approve",
                "plan_id": "plan-af1e431bc8a7",
                "steps": 4
            }
        });
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.collaborative"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": msg }),
        );
        let spec = render_spec(&e);
        assert_eq!(spec.tone, RowTone::OperatorGate);
        assert!(spec.headline.contains("PLAN AWAITING APPROVAL"));
        assert!(spec.headline.contains("4-step plan") || spec.headline.contains("fibonacci"));
        let detail = spec.detail.expect("plan card body");
        assert!(detail.contains("plan-af1e431bc8a7"));
        assert!(detail.contains("4 steps"));
        assert!(detail.contains("y approve"));
        assert!(!detail.contains("\"status\":"), "raw JSON must not leak");
    }

    #[test]
    fn extract_plan_proposal_id_from_embedded_agent_message() {
        let msg = serde_json::json!({
            "status": "awaiting_approval",
            "plan_id": "plan-abc",
            "summary": "A plan."
        });
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.collaborative"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": msg }),
        );
        assert_eq!(
            extract_plan_proposal_id(&e).as_deref(),
            Some("plan-abc")
        );
    }

    #[test]
    fn render_spec_glued_diagnosis_summary_keeps_body_not_flat_headline() {
        let summary = "I found the problem. Here's the diagnosis:Root CauseThe fibonacci-next agent fails with AttributeError.What's HappeningThe agent was built to use sdk.state.Evidence| Date | Status | |---|---|---| | Jun 6 | ok |Fix OptionsRewrite to use file-based state.";
        let msg = serde_json::json!({
            "status": "ok",
            "summary": summary,
            "result": { "agent_id": "fibonacci-next" }
        });
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": msg }),
        );
        let spec = render_spec(&e);
        assert!(
            spec.headline.is_empty(),
            "glued single-line diagnosis must not become a flat headline: {:?}",
            spec.headline
        );
        let detail = spec.detail.expect("full summary in detail");
        assert!(detail.contains("Root Cause") || detail.contains("diagnosis"));
        assert!(detail.contains("[ok]"));
        assert!(detail.contains("agent: fibonacci-next"));
    }

    #[test]
    fn render_spec_splits_pretty_printed_embedded_json_after_prose() {
        let structured = serde_json::json!({
            "status": "ok",
            "summary": "## Fibonacci Agent Failure Analysis\n\n### Agent Overview\n- **Agent ID**: `fibonacci-next`\n- **Type**: Script agent",
            "result": {
                "agent_id": "fibonacci-next",
                "revision_status": "Archived",
                "failure_count": 8,
                "primary_cause": "Archived revision + SDK sandbox initialization failure"
            }
        });
        // Model often emits pretty-printed JSON after a single newline, not `\n\n{`.
        let message = format!(
            "Now I have a comprehensive picture. Let me summarize my findings.\n{}",
            serde_json::to_string_pretty(&structured).unwrap()
        );
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.collaborative"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": message }),
        );
        let spec = render_spec(&e);
        let detail = spec.detail.expect("structured detail");
        assert!(detail.contains("comprehensive picture"));
        assert!(detail.contains("[ok]"));
        assert!(detail.contains("### Agent Overview"));
        assert!(detail.contains("fibonacci-next"));
        assert!(!detail.contains("\"status\": \"ok\""), "raw JSON must not leak");

        let lines = format_detail(&e);
        let payload = lines.join("\n");
        assert!(payload.contains("\"prose\":"));
        assert!(payload.contains("\"summary\":"));
        assert!(payload.contains("Fibonacci Agent Failure Analysis"));
        assert!(
            !payload.contains("Let me summarize my findings.\n{"),
            "raw glued message in detail pane"
        );
    }

    #[test]
    fn render_spec_structured_install_intent_uses_reason_title() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({
                "message": {
                    "artifact_ref": "ar.cb049a325507",
                    "reason": "Artifact ready with semantic install intent.",
                    "status": "ok"
                }
            }),
        );
        let spec = render_spec(&e);
        assert!(spec.headline.contains("Artifact ready"));
        assert_eq!(spec.detail.as_deref(), Some("artifact: ar.cb049a325507"));
    }

    #[test]
    fn render_spec_sets_tone_from_event_type() {
        let tool = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({ "tool_name": "agent_list" }),
        );
        let msg = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": "I'll list agents first." }),
        );
        assert_eq!(render_spec(&tool).tone, RowTone::ToolCall);
        assert_eq!(render_spec(&msg).tone, RowTone::AgentNarrative);
    }

    #[test]
    fn render_spec_carries_actor_kind_and_label() {
        let e = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::agent("coder.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({ "tool_name": "Edit" }),
        );
        let spec = render_spec(&e);
        assert_eq!(spec.altitude, Altitude::Normal);
        assert_eq!(spec.actor, ActorKind::Specialist);
        assert_eq!(spec.actor_label, "coder");
        assert!(spec.headline.contains("tool Edit"));
        // No second line for an unknown tool with no payload detail.
        assert!(spec.detail.is_none());
        // No turn set by default.
        assert!(spec.turn_id.is_none());
    }

    #[test]
    fn render_spec_extracts_sandbox_exec_result_preview() {
        let e = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::agent("coder.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "sandbox_exec",
                "result": { "ok": true, "stdout": "hello world from the script" }
            }),
        );
        let spec = render_spec(&e);
        assert!(spec.detail.is_some());
        assert!(spec.detail.as_ref().unwrap().contains("hello world"));
    }

    #[test]
    fn render_spec_sandbox_exec_result_as_encoded_string_shows_real_preview_not_raw_json() {
        // The real gateway emits `result` as a JSON-*encoded string* (see
        // `log_tool_completed_with_approval`), not a nested object. Before the
        // fix, the `stdout` lookup silently missed this shape and fell back to
        // dumping the raw, truncated JSON string on-screen.
        let raw_result = serde_json::json!({
            "ok": false,
            "command_succeeded": false,
            "exit_code": 1,
            "stdout": "",
            "stderr": "ModuleNotFoundError: no module named 'requests'",
            "execution_trace_id": "80bc0ef4-c20f-4e70-940a-84ffcb000000"
        })
        .to_string();
        let e = entry(
            SessionRole::Specialist { kind: "researcher".into() },
            Principal::agent("researcher.default"),
            "tool.completed",
            Altitude::Attention,
            serde_json::json!({ "tool_name": "sandbox_exec", "result": raw_result }),
        );
        let spec = render_spec(&e);
        let detail = spec.detail.expect("expected a detail preview");
        assert!(
            !detail.starts_with('{'),
            "detail must not be the raw JSON payload: {detail}"
        );
        assert!(detail.contains("ModuleNotFoundError"), "got: {detail}");
        assert!(detail.contains("exit 1"), "got: {detail}");
    }

    #[test]
    fn render_spec_artifact_exec_shows_command_in_headline_and_output_in_detail() {
        // The tracer packs `<entrypoint> <args> · <artifact_ref>` into args_preview;
        // the headline leads with `▶` so the operator sees what ran, and the detail
        // carries the captured stdout.
        let e = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::agent("coder.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "artifact_exec",
                "args_preview": "main.py --fast · ar.abc123",
                "result": r#"{"ok":true,"command_succeeded":true,"stdout":"42 passed"}"#
            }),
        );
        let spec = render_spec(&e);
        assert!(spec.headline.starts_with('▶'), "headline: {}", spec.headline);
        assert!(spec.headline.contains("main.py --fast"), "headline: {}", spec.headline);
        assert!(spec.headline.contains("ar.abc123"), "headline: {}", spec.headline);
        assert_eq!(spec.detail.as_deref(), Some("42 passed"));
    }

    #[test]
    fn render_spec_content_write_detail_shows_ref_not_redundant_path() {
        let e = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::agent("coder.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "content_write",
                "args_preview": "skills/weather/SKILL.md",
                "result": r#"{"ok":true,"sandbox_path":"/tmp/skills/weather/SKILL.md"}"#
            }),
        );
        let spec = render_spec(&e);
        assert!(spec.headline.contains("skills/weather/SKILL.md"));
        // Detail surfaces the sandbox path (useful, non-redundant), not the name again.
        assert_eq!(spec.detail.as_deref(), Some("/tmp/skills/weather/SKILL.md"));
    }

    #[test]
    fn render_spec_extracts_agent_spawn_message_preview() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "agent_spawn",
                "message": "build the weather skill end-to-end"
            }),
        );
        let spec = render_spec(&e);
        assert!(spec
            .detail
            .as_ref()
            .unwrap()
            .contains("build the weather skill"));
    }

    #[test]
    fn render_spec_agent_spawn_splits_target_and_task() {
        // With both an agent id and a task message, the headline names the child
        // (`⑂ spawn → coder.default`) and the detail carries the task.
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "agent_spawn",
                "result": { "ok": true, "agent_id": "coder.default", "task_id": "t-1" },
                "message": "implement the retry backoff"
            }),
        );
        let spec = render_spec(&e);
        assert!(spec.headline.starts_with('⑂'), "headline: {}", spec.headline);
        assert!(spec.headline.contains("coder.default"), "headline: {}", spec.headline);
        assert!(!spec.headline.contains("tool agent_spawn"), "headline: {}", spec.headline);
        assert_eq!(spec.detail.as_deref(), Some("implement the retry backoff"));
    }

    #[test]
    fn render_spec_promotion_record_pass_is_green_verdict_with_detail() {
        let raw_result = serde_json::json!({
            "ok": true,
            "pass": true,
            "execution_trace_id": "51204daf-4efb-4c19-9cf8-d8b337c13289",
            "promotion_record": {
                "artifact_ref": "ar.386f5b222421",
                "unit_test_runner_pass": true,
                "unit_test_runner_id": "unit_test_runner.default",
                "unit_test_runner_findings": [
                    {"severity": "info", "description": "47 tests passed"},
                ],
                "unit_test_runner_timestamp": "2026-08-04T12:00:00Z",
                "evaluator_pass": false,
                "auditor_pass": false,
            }
        });
        let e = entry(
            SessionRole::Specialist {
                kind: "unit_test_runner".into(),
            },
            Principal::agent("unit_test_runner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "promotion_record",
                "result": raw_result.to_string(),
            }),
        );
        let spec = render_spec(&e);
        assert_eq!(spec.tone, RowTone::VerdictPass);
        assert!(
            spec.headline.contains("✓ promotion PASS"),
            "headline: {}",
            spec.headline
        );
        assert!(
            spec.headline.contains("unit_test_runner"),
            "headline: {}",
            spec.headline
        );
        let detail = spec.detail.expect("promotion detail");
        assert!(detail.contains("execution trace"), "{detail}");
        assert!(detail.contains("findings"), "{detail}");
        assert!(detail.contains("unit_test_runner ✓"), "{detail}");
        assert!(
            !detail.contains("promotion_record"),
            "must not dump raw JSON: {detail}"
        );
    }

    #[test]
    fn render_spec_promotion_record_fail_is_red_verdict_with_blocking_findings() {
        let raw_result = serde_json::json!({
            "ok": true,
            "pass": false,
            "execution_trace_id": "aabbccdd-1111-2222-3333-444455556666",
            "promotion_record": {
                "artifact_ref": "ar.deadbeef",
                "unit_test_runner_pass": false,
                "unit_test_runner_id": "unit_test_runner.default",
                "unit_test_runner_findings": [
                    {"severity": "critical", "description": "test_auth_login FAILED"},
                    {"severity": "warning", "description": "coverage below target"},
                ],
                "unit_test_runner_timestamp": "2026-08-04T12:01:00Z",
            }
        });
        let e = entry(
            SessionRole::Specialist {
                kind: "unit_test_runner".into(),
            },
            Principal::agent("unit_test_runner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "promotion_record",
                "result": raw_result.to_string(),
            }),
        );
        let spec = render_spec(&e);
        assert_eq!(spec.tone, RowTone::VerdictFail);
        assert!(
            spec.headline.contains("✗ promotion FAIL"),
            "headline: {}",
            spec.headline
        );
        let detail = spec.detail.expect("promotion detail");
        assert!(detail.contains("[critical]"), "{detail}");
        assert!(detail.contains("test_auth_login"), "{detail}");
    }

    #[test]
    fn render_spec_generic_tool_completed_prefers_summary_over_raw_json() {
        // Any tool without an explicit detail_preview arm (the vast majority —
        // ~50+ tools) falls to the generic `Some(_)` branch. `result` is a
        // JSON-encoded string here, matching real gateway emission
        // (`log_tool_completed_with_approval`). Before the fix, the raw
        // string always won the race against `extract_tool_summary` and the
        // operator saw `{"summary":"found 3 matching skills","ok":true,...`
        // instead of the plain-English summary.
        let raw_result =
            serde_json::json!({ "ok": true, "summary": "found 3 matching skills" }).to_string();
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({ "tool_name": "knowledge_search", "result": raw_result }),
        );
        let spec = render_spec(&e);
        let detail = spec.detail.expect("expected a detail preview");
        assert_eq!(detail, "found 3 matching skills", "got: {detail}");
    }

    #[test]
    fn render_spec_tool_completed_shows_args_preview_from_timeline_payload() {
        let spawn = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "agent_spawn",
                "result": r#"{"accepted":true}"#,
                "args_preview": "coder.default"
            }),
        );
        let spawn_spec = render_spec(&spawn);
        // Distinctive spawn headline names the target with the fork glyph.
        assert!(
            spawn_spec.headline.contains("spawn") && spawn_spec.headline.contains("coder.default"),
            "headline: {}",
            spawn_spec.headline
        );
        // The agent id is already in the headline, so the redundant detail line
        // (whose only content here is that same id) is suppressed.
        assert_eq!(spawn_spec.detail.as_deref(), None);

        let write = entry(
            SessionRole::Specialist { kind: "coder".into() },
            Principal::agent("coder.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "content_write",
                "result": r#"{"ok":true}"#,
                "args_preview": "skills/weather/SKILL.md"
            }),
        );
        let write_spec = render_spec(&write);
        // File writes read as `✎ wrote <path>`; the path is in the headline, so
        // the detail line (which would only repeat it) is suppressed.
        assert!(
            write_spec.headline.contains("wrote") && write_spec.headline.contains("skills/weather/SKILL.md"),
            "headline: {}",
            write_spec.headline
        );
        assert_eq!(write_spec.detail.as_deref(), None);
    }

    #[test]
    fn agent_spawn_agent_id_from_completed_result_json() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "agent_spawn",
                "result": {
                    "ok": true,
                    "agent_id": "coder.default",
                    "task_id": "t-1"
                }
            }),
        );
        assert_eq!(
            agent_spawn_agent_id(&e).as_deref(),
            Some("coder.default")
        );
    }

    #[test]
    fn agent_spawn_agent_id_from_completed_summary_text() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "agent_spawn",
                "summary": "spawned architect.default for s1"
            }),
        );
        assert_eq!(
            agent_spawn_agent_id(&e).as_deref(),
            Some("architect.default")
        );
    }

    #[test]
    fn agent_spawn_agent_id_from_requested_arguments() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.requested",
            Altitude::Detail,
            serde_json::json!({
                "tool_name": "agent_spawn",
                "arguments": {
                    "agent_id": "specialized_builder",
                    "message": "build the skill"
                }
            }),
        );
        assert_eq!(
            agent_spawn_agent_id(&e).as_deref(),
            Some("specialized_builder")
        );
    }

    #[test]
    fn render_spec_extracts_workflow_wait_message_from_result() {
        // Real gateway shape: `result` is a JSON-encoded string (see
        // `log_tool_completed_with_approval`) containing a human-readable
        // `message` (from `workflow_wait_join_message`) — never a top-level
        // `task_ids` field, which this branch previously (and always, in
        // production) looked for.
        let raw_result = serde_json::json!({
            "ok": true,
            "join_satisfied": true,
            "message": "Join satisfied: 3/3 tasks done"
        })
        .to_string();
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({ "tool_name": "workflow_wait", "result": raw_result }),
        );
        let spec = render_spec(&e);
        assert_eq!(spec.detail.as_deref(), Some("Join satisfied: 3/3 tasks done"));
    }

    #[test]
    fn render_spec_extracts_workflow_state_resume_hint_from_result() {
        let raw_result = serde_json::json!({
            "workflow_status": "running",
            "resume_hint": "coder_done — proceed to evaluator or federation"
        })
        .to_string();
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({ "tool_name": "workflow_state", "result": raw_result }),
        );
        let spec = render_spec(&e);
        assert_eq!(
            spec.detail.as_deref(),
            Some("coder_done — proceed to evaluator or federation")
        );
    }

    #[test]
    fn render_spec_shows_llm_failure_preceding_chain() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "llm.request_failed",
            Altitude::Error,
            serde_json::json!({
                "error": "rate limited",
                "preceding": ["read_file", "edit", "run"]
            }),
        );
        let spec = render_spec(&e);
        assert!(spec.detail.is_some());
        assert!(spec.detail.as_ref().unwrap().contains("after:"));
        assert!(spec.detail.as_ref().unwrap().contains("read_file"));
    }

    #[test]
    fn row_spec_plain_text_joins_headline_and_detail() {
        let spec = RowSpec {
            altitude: Altitude::Normal,
            actor: ActorKind::Planner,
            tone: RowTone::Default,
            actor_label: "planner".into(),
            headline: "headline here".into(),
            detail: Some("and a detail".into()),
            turn_id: None,
            source_session_id: None,
            turn_index: None,
            turn_label: None,
            in_flight: false,
            show_reasoning: true,
            egress_label: None,
        };
        let s = spec.to_plain_text();
        assert!(s.contains("headline here"));
        assert!(s.contains("and a detail"));
        // Two lines, separated by \n.
        assert_eq!(s.lines().count(), 2);
    }

    #[test]
    fn cap_preview_truncates_at_max_with_ellipsis() {
        let long = "a".repeat(100);
        let capped = cap_preview(&long, 12);
        assert_eq!(capped.chars().count(), 12);
        assert!(capped.ends_with('\u{2026}'));
        // Within-budget strings pass through unchanged.
        assert_eq!(cap_preview("short", 12), "short");
    }

    #[test]
    fn detail_preview_omitted_for_unknown_event_types() {
        // The room is intentionally permissive: an unknown event type yields
        // a single-line row with no preview, not an error.
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "some.future.event",
            Altitude::Normal,
            serde_json::json!({ "anything": "goes" }),
        );
        let spec = render_spec(&e);
        assert!(spec.detail.is_none());
    }

    #[test]
    fn render_spec_propagates_turn_id() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "message": "hi" }),
        );
        let mut e = e;
        e.turn_id = Some("turn-42".into());
        let spec = render_spec(&e);
        assert_eq!(spec.turn_id.as_deref(), Some("turn-42"));
    }

    #[test]
    fn render_spec_decorates_human_operator_with_emoji() {
        // A human principal gets the 🧑 prefix; an autonoetic agent does not.
        // Same role, different label — this is the decoration the old
        // `role_label(role)` path was missing.
        let human = entry(
            SessionRole::Operator,
            Principal::human("operator-1"),
            "operator.message",
            Altitude::Normal,
            serde_json::json!({ "message": "hi" }),
        );
        let agent = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "operator.message",
            Altitude::Normal,
            serde_json::json!({ "message": "hi" }),
        );
        let h = render_spec(&human);
        let a = render_spec(&agent);
        assert!(h.actor_label.contains('\u{1F9D1}'), "human gets \u{1F9D1} prefix: {}", h.actor_label);
        assert!(!a.actor_label.contains('\u{1F9D1}'), "agent has no human prefix: {}", a.actor_label);
    }

    #[test]
    fn render_spec_decorates_foreign_agent_with_provider() {
        // Foreign agent principal carries the upstream provider name in the
        // label so the operator can distinguish local vs remote origins.
        let e = entry(
            SessionRole::Planner,
            Principal::foreign("openrouter", "agent-9"),
            "operator.message",
            Altitude::Normal,
            serde_json::json!({ "message": "hi" }),
        );
        let spec = render_spec(&e);
        assert!(spec.actor_label.contains('\u{1F310}'), "foreign agent gets \u{1F310} prefix");
        assert!(spec.actor_label.contains("openrouter"), "provider name preserved");
    }

    #[test]
    fn row_text_preserves_multi_line_detail_for_cli_viewer() {
        // CLI viewer relies on the embedded \n to render detail on its own
        // line via println!. Flattening it would defeat the whole point.
        let spec = RowSpec {
            altitude: Altitude::Normal,
            actor: ActorKind::Planner,
            tone: RowTone::ToolCall,
            actor_label: "planner".into(),
            headline: "tool sandbox_exec".into(),
            detail: Some("hello world".into()),
            turn_id: None,
            source_session_id: None,
            turn_index: None,
            turn_label: None,
            in_flight: false,
            show_reasoning: true,
            egress_label: None,
        };
        let row = RenderedRow::Line(spec);
        let s = row_text(&row);
        assert!(s.contains('\n'), "detail boundary must be preserved: {s:?}");
        assert!(s.contains("hello world"));
    }

    #[test]
    fn detail_narrative_markers_for_expanded_keys() {
        for key in ["reason", "explanation", "diagnosis", "output", "synthesis"] {
            let e = entry(
                SessionRole::Planner,
                Principal::agent("planner.default"),
                "agent.message",
                Altitude::Normal,
                serde_json::json!({ key: "## Heading\n\nSome text here." }),
            );
            let lines = format_detail(&e);
            let joined = lines.join("\n");
            assert!(
                joined.contains("@@NARRATIVE@@"),
                "key `{key}` should produce narrative markers, got:\n{joined}"
            );
        }
    }

    #[test]
    fn detail_narrative_markers_for_unknown_key_with_markdown() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "custom_field": "### Title\n\n- item one\n- item two" }),
        );
        let lines = format_detail(&e);
        let joined = lines.join("\n");
        assert!(
            joined.contains("@@NARRATIVE@@"),
            "unknown key with markdown should produce narrative markers, got:\n{joined}"
        );
    }

    #[test]
    fn detail_no_narrative_for_plain_short_unknown_key() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            serde_json::json!({ "custom_field": "just a short string" }),
        );
        let lines = format_detail(&e);
        let joined = lines.join("\n");
        assert!(
            !joined.contains("@@NARRATIVE@@"),
            "short plain unknown key should NOT produce narrative markers, got:\n{joined}"
        );
    }

    #[test]
    fn detail_narrative_for_summary_inside_nested_message_object() {
        // Payload where `message` is a JSON object (not a stringified JSON
        // envelope), with `summary` as a prose field inside it.
        let payload_json = r#"{"message":{"result":{"conditions":"Mainly clear","current_temp":"24.6°C"},"status":"ok","summary":"Current weather in Paris, France\n\n- Temperature: 24.6°C (76°F)\n- Conditions: Mainly clear skies\n- Humidity: 51%\n- Wind: 13.4 km/h from the West\n\n| Time | Temp | Conditions |\n|------|------|------------|\n| 21:00 | 24.4°C | Mainly clear |\n| 22:00 | 23.1°C | Mainly clear |"}}"#;
        let e = SessionTimelineEntry {
            event_id: "ev2".into(),
            root_session_id: "root".into(),
            source_session_id: "src".into(),
            turn_id: None,
            principal: Principal::agent("researcher.default"),
            role: SessionRole::Specialist { kind: "researcher".into() },
            event_type: "agent.message".into(),
            altitude: Altitude::Normal,
            occurred_at: "2026-06-12T20:45:00Z".into(),
            payload: Some(payload_json.to_string()),
            refs: Default::default(),
        };
        let lines = format_detail(&e);
        let joined = lines.join("\n");
        assert!(
            joined.contains("@@NARRATIVE@@"),
            "nested-message summary should have narrative markers, got:\n{joined}"
        );
        assert!(
            joined.contains("Current weather in Paris"),
            "should contain the summary content"
        );
    }

    #[test]
    fn diag_realistic_weather_message() {
        let payload_json = r#"{"message":"{\"status\":\"ok\",\"summary\":\"**Current Weather in Paris, France** (as of 4:15 PM CEST, June 12, 2026)\n\n- **Temperature:** 24.7°C (76.5°F)\n- **Conditions:** Partly cloudy\n- **Humidity:** 50%\n- **Wind Speed:** 18.5 km/h (11.5 mph)\n- **Precipitation:** 0.0 mm — no rain\n\n---\n\n**Hourly Forecast — Next 8 Hours**\n\n| Time (CEST) | Temp (°C / °F) | Conditions | Humidity | Wind (km/h) | Precip Chance |\n|---|---|---|---|---|---|\n| 5:00 PM | 24.9°C / 76.8°F | Partly cloudy | 49% | 18.2 | 0% |\"}"}"#;
        let e = SessionTimelineEntry {
            event_id: "ev1".into(),
            root_session_id: "root".into(),
            source_session_id: "src".into(),
            turn_id: None,
            principal: Principal::agent("researcher.default"),
            role: SessionRole::Specialist { kind: "researcher".into() },
            event_type: "agent.message".into(),
            altitude: Altitude::Normal,
            occurred_at: "2026-06-12T14:20:00Z".into(),
            payload: Some(payload_json.to_string()),
            refs: Default::default(),
        };
        let lines = format_detail(&e);
        let joined = lines.join("\n");
        assert!(
            joined.contains("@@NARRATIVE@@"),
            "should have narrative markers"
        );
        assert!(
            joined.contains("**Temperature:**"),
            "should contain the summary markdown content"
        );
    }

    // ── Markdown-coverage probes: these exercise the gaps that historically
    // caused agent prose to render as raw text in the detail pane. Each
    // payload below MUST produce @@NARRATIVE@@ markers so the TUI runs it
    // through the markdown pipeline.

    fn narrative_entries(payload: serde_json::Value) -> bool {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "agent.message",
            Altitude::Normal,
            payload,
        );
        format_detail(&e).join("\n").contains("@@NARRATIVE@@")
    }

    #[test]
    fn narrative_unknown_key_single_line_bold() {
        assert!(
            narrative_entries(serde_json::json!({ "note": "**important** single line" })),
            "single-line bold in unknown key should render as markdown"
        );
    }

    #[test]
    fn narrative_unknown_key_single_line_heading() {
        assert!(
            narrative_entries(serde_json::json!({ "note": "## Heading only" })),
            "single-line heading in unknown key should render as markdown"
        );
    }

    #[test]
    fn narrative_unknown_key_single_line_underscore_bold() {
        assert!(
            narrative_entries(serde_json::json!({ "note": "__bold__ via underscores" })),
            "__bold__ in unknown key should render as markdown"
        );
    }

    #[test]
    fn narrative_unknown_key_multiline_code_block() {
        let val = "Here is code:\n\ndef fib(n):\n    return n\n\nDone.";
        assert!(
            narrative_entries(serde_json::json!({ "note": val })),
            "multiline python-ish block in unknown key should render as markdown"
        );
    }

    #[test]
    fn narrative_known_key_summary_with_link() {
        let val = "See [the docs](https://example.com) for details.";
        assert!(
            narrative_entries(serde_json::json!({ "summary": val })),
            "markdown link in known key should render as markdown"
        );
    }

    #[test]
    fn narrative_known_key_ascii_thematic_break() {
        let val = "Intro paragraph.\n\n---\n\nAfter the break.";
        assert!(
            narrative_entries(serde_json::json!({ "summary": val })),
            "ASCII --- thematic break should render as markdown"
        );
    }

    #[test]
    fn narrative_array_of_markdown_strings() {
        assert!(
            narrative_entries(serde_json::json!({ "findings": ["## Critical\n\nbad thing", "## Warning\n\nmeh"] })),
            "markdown strings inside an array should render as markdown"
        );
    }

    #[test]
    fn narrative_nested_array_of_objects_with_prose() {
        assert!(
            narrative_entries(serde_json::json!({ "results": [{ "answer": "## Yes\n\nHere is why…" }] })),
            "prose inside array-of-objects should render as markdown"
        );
    }
}
