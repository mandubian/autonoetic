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

/// Hard ceiling for narrative body text shown inline in the room list. The
/// detail pane (⏎) still shows the full payload; beyond this we add `…`.
const NARRATIVE_BODY_MAX: usize = 8_000;

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
    serde_json::Value::String(s.to_string())
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

/// Plaintext projection of a summary field (markdown stripped when detected).
fn summary_plaintext(s: &str) -> String {
    if super::markdown::looks_like_markdown(s) {
        super::markdown::strip_markdown(s)
    } else {
        s.to_string()
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

/// High-visibility approval gate card (`approval.pending`).
fn approval_gate_card(entry: &SessionTimelineEntry) -> (String, Option<String>) {
    let p = parse_entry_payload(entry);
    let field = |key: &str| p.as_ref().and_then(|v| payload_field_str(v, key));
    let request_id = field("request_id")
        .or_else(|| entry.refs.approval_request_id.clone())
        .unwrap_or_default();
    let action = field("action").unwrap_or_else(|| "approval".into());
    let level = field("approval_level");
    let headline = format!("⏸ APPROVAL REQUIRED — {}", one_line(&action, 72));
    let mut lines = vec![format!("  request: {request_id}")];
    if let Some(lvl) = level {
        lines.push(format!("  level: {lvl}"));
    }
    if let Some(cmd) = field("command") {
        lines.push(format!("  command: {}", one_line(&cmd, 140)));
    }
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
        }
    }
    if let Some(risk) = field("risk_summary") {
        lines.push(format!("  risk: {}", one_line(&risk, 120)));
    }
    lines.push("  ↳ y approve · n reject".to_string());
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

/// High-visibility escalation gate card (`escalation.pending`).
fn escalation_gate_card(entry: &SessionTimelineEntry) -> (String, Option<String>) {
    let p = parse_entry_payload(entry);
    let field = |key: &str| p.as_ref().and_then(|v| payload_field_str(v, key));
    let synthesis = field("synthesis").unwrap_or_else(|| "operator decision requested".into());
    let rev = field("revision_id");
    let headline = format!("⏸ ESCALATION — {}", one_line(&synthesis, 88));
    let mut lines = Vec::new();
    if let Some(r) = rev.filter(|r| !r.is_empty()) {
        lines.push(format!("  revision: {r}"));
    }
    lines.push("  ↳ review in detail pane · resolve via gateway approvals".to_string());
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

/// Full agent/operator message text for multiline row display. Strips markdown
/// to plain text and preserves intentional newlines.
pub(crate) fn narrative_body(entry: &SessionTimelineEntry, key: &str) -> Option<String> {
    let (_, detail) = message_list_rows(entry, key);
    detail
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
        "plan.pending" => format!("plan proposed: {}", field("title").unwrap_or_default()),
        "plan.approved" => format!("plan approved ({})", field("plan_id").unwrap_or_default()),
        "wiki.proposed" => format!("wiki proposed: {} ({})", field("title").unwrap_or_default(), field("page_id").unwrap_or_default()),
        "wiki.promoted" => format!("wiki promoted: {} ({})", field("title").unwrap_or_default(), field("page_id").unwrap_or_default()),
        "wiki.rejected" => format!("wiki rejected: {} — {}", field("title").unwrap_or_default(), field("reason").unwrap_or_else(|| "no reason".into())),
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
        "agent.reasoning" => format!("💭 {}", one_line(&field("reasoning").unwrap_or_default(), 160)),
        "user.ask.pending" => format!(
            "asks: {}",
            one_line(&field("question").unwrap_or_default(), 200),
        ),
        // Payload key is `tool_name`; keep `tool` as a fallback for older rows.
        // Show just the result summary — status is conveyed by altitude color.
        "tool.completed" => {
            let summary = extract_tool_summary(p.as_ref());
            match summary {
                Some(s) => {
                    let plain = if super::markdown::looks_like_markdown(&s) {
                        super::markdown::strip_markdown(&s)
                    } else {
                        s
                    };
                    one_line(&plain, 160)
                }
                None => format!("tool {}", field("tool_name")
                    .or_else(|| field("tool"))
                    .unwrap_or_else(|| "completed".into())),
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
        other => other.to_string(),
    }
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
/// suitable for the `detail` field. Caps total character count at `max`.
fn preserve_lines(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}…")
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
        // Tool calls: a one-line hint at the args or the result preview.
        "tool.completed" => {
            let tool = s("tool_name").or_else(|| s("tool"));
            // For sandbox_exec-like tools, surface the result.stdout (or first
            // 80 chars of any string result). For content_write, surface path.
            match tool.as_deref() {
                Some("content_write") => s("path").map(|p| cap_preview(&p, 80)),
                Some("sandbox_exec") | Some("artifact_exec") => p
                    .as_ref()
                    .and_then(|v| v.get("result"))
                    .and_then(|r| r.get("stdout"))
                    .and_then(|x| x.as_str())
                    .map(|o| cap_preview(o, 80))
                    .or_else(|| {
                        // The result may be a JSON string (not an object).
                        s("result").map(|r| cap_preview(&r, 80))
                    }),
                Some("agent_spawn") => s("message").map(|m| cap_preview(&m, 80)),
                Some("workflow_wait") | Some("workflow_state") => p
                    .as_ref()
                    .and_then(|v| v.get("task_ids"))
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        let ids: Vec<String> = a
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect();
                        cap_preview(&ids.join(", "), 80)
                    }),
                Some(_) => {
                    // Show result content as preview, not full payload
                    p.as_ref()
                        .and_then(|v| v.get("result"))
                        .and_then(|r| {
                            // Structured result: try to extract a text preview
                            r.get("stdout").and_then(|x| x.as_str()).map(|o| cap_preview(o, 120))
                                .or_else(|| r.as_str().map(|s| cap_preview(s, 120)))
                        })
                        .or_else(|| extract_tool_summary(p.as_ref()).map(|s| cap_preview(&s, 120)))
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
        // [`narrative_body`] — not duplicated here.
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
            let is_wiki = entry
                .payload
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .is_some_and(|v| v.get("action").and_then(|a| a.as_str()) == Some("wiki_propose"));
            if is_wiki {
                wiki_proposal_gate_card(entry)
            } else {
                approval_gate_card(entry)
            }
        }
        "user.ask.pending" => interaction_gate_card(entry),
        "plan.pending" => plan_gate_card(entry),
        "escalation.pending" => escalation_gate_card(entry),
        "wiki.proposed" => wiki_lifecycle_card(entry, "📝 WIKI PROPOSED"),
        "wiki.promoted" => wiki_lifecycle_card(entry, "✅ WIKI PROMOTED"),
        "wiki.rejected" => wiki_lifecycle_card(entry, "❌ WIKI REJECTED"),
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
        turn_index: None, // The TUI fills in a 1-based ordinal once turns are scanned.
        in_flight: false, // The TUI fills this in once it knows turn lifecycle.
        show_reasoning,
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
    Collapsed { count: usize, summary: String },
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
    /// LLM errors and everything else.
    Default,
}

/// Map a gateway event type to the row's visual tone.
pub fn row_tone(event_type: &str) -> RowTone {
    match event_type {
        "agent.message" | "operator.message" => RowTone::AgentNarrative,
        "tool.completed" => RowTone::ToolCall,
        "agent.reasoning" => RowTone::Reasoning,
        _ => RowTone::Default,
    }
}

/// Tone for a full timeline entry — includes embedded plan proposals in messages.
pub fn tone_for_entry(entry: &SessionTimelineEntry) -> RowTone {
    match entry.event_type.as_str() {
        "plan.pending" | "approval.pending" | "user.ask.pending" | "escalation.pending"
        | "wiki.proposed" | "wiki.promoted" | "wiki.rejected" => {
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
    /// 1-based ordinal of this turn in the session (first `turn_id` seen = 1).
    /// Filled by the TUI after scanning the timeline; `None` when untagged.
    pub turn_index: Option<u32>,
    /// True when the turn containing this row is still in flight (no matching
    /// `turn.end` has been seen yet). The TUI uses this to show a spinner.
    pub in_flight: bool,
    /// Show the 💭 reasoning prefix — false when reasoning is hidden by toggle.
    pub show_reasoning: bool,
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
                RenderedRow::Line(render_spec(e)),
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
            RenderedRow::Line(render_spec(&entries[start])),
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
/// Render a `turn.end` detail by aggregating `llm.round` events from the same
/// turn. Returns `None` if the entry is not `turn.end` or no rounds were found.
pub fn turn_summary(entry: &SessionTimelineEntry, all: &[SessionTimelineEntry]) -> Option<Vec<String>> {
    if entry.event_type != "turn.end" {
        return None;
    }
    let turn_id = entry.turn_id.as_deref()?;
    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut calls: u64 = 0;
    let mut models: Vec<String> = Vec::new();
    let mut in_turn = false;
    for e in all {
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
    "summary", "prose", "message", "text", "content", "body", "question",
];

fn is_narrative_payload_key(key: &str) -> bool {
    NARRATIVE_PAYLOAD_KEYS.contains(&key)
}

fn should_render_payload_as_narrative(key: &str, value: &str) -> bool {
    if !is_narrative_payload_key(key) {
        return false;
    }
    if value.starts_with('{') || value.starts_with('[') {
        return false;
    }
    value.contains('\n')
        || value.chars().count() > 80
        || super::markdown::looks_like_narrative_content(value)
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
        assert!(matches!(&rows[1], RenderedRow::Line(spec) if spec.headline.contains("APPROVAL REQUIRED")));
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
    fn render_spec_extracts_workflow_wait_task_ids() {
        let e = entry(
            SessionRole::Planner,
            Principal::agent("planner.default"),
            "tool.completed",
            Altitude::Normal,
            serde_json::json!({
                "tool_name": "workflow_wait",
                "task_ids": ["t-1", "t-2", "t-3"]
            }),
        );
        let spec = render_spec(&e);
        assert_eq!(spec.detail.as_deref(), Some("t-1, t-2, t-3"));
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
            turn_index: None,
            in_flight: false,
            show_reasoning: true,
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
            turn_index: None,
            in_flight: false,
            show_reasoning: true,
        };
        let row = RenderedRow::Line(spec);
        let s = row_text(&row);
        assert!(s.contains('\n'), "detail boundary must be preserved: {s:?}");
        assert!(s.contains("hello world"));
    }
}
