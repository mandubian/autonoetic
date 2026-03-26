//! Progressive Markdown **live digest** writer (`digest.md`).
//!
//! Replaces the flat `timeline.md` table with a turn-oriented narrative: actions,
//! structured tool results, errors, and optional agent annotations via `digest.annotate`.

use crate::log_redaction::redact_text_for_logs;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Root session id — portion before the first `/`.
pub fn base_session_id(session_id: &str) -> &str {
    session_id.split('/').next().unwrap_or(session_id)
}

/// Compute nesting depth from session_id (number of slashes).
pub fn session_depth(session_id: &str) -> usize {
    session_id.matches('/').count()
}

/// Writes `{gateway_dir}/sessions/{base_session_id}/digest.md`.
///
/// Multiple agents writing to the same digest file is expected — each agent opens its
/// own writer with its `agent_id` and `session_id`. Turn headers and summaries are
/// labelled with the agent. Header level follows nesting depth (## root, ### depth-1 …).
///
/// **Group Chat Model**: Headers are only written on agent context switches (when a
/// different agent's turn appears). Consecutive turns by the same agent are bundled
/// under one header to reduce noise.
pub struct LiveDigestWriter {
    path: PathBuf,
    /// Agent that owns this writer instance.
    agent_id: String,
    /// Nesting depth (0 = root, 1 = child …).
    depth: usize,
    digest_turn_seq: u32,
    tools_in_open_turn: u32,
    session_tool_total: u32,
    session_error_total: u32,
    /// True if this is a resumed session (from checkpoint/hibernation).
    is_resumed: bool,
    /// Buffer for the current turn. Flushed atomically at end_turn().
    turn_buffer: Option<String>,
}

/// Get the last agent that was logged to the digest file.
fn get_last_logged_agent(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines().rev() {
            if let Some(agent) = extract_agent_from_header(line) {
                return Some(agent);
            }
        }
    }
    None
}

/// Extract agent_id from a digest header line (e.g., "### 👑 planner.default" or "### planner.default")
fn extract_agent_from_header(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("### ") || trimmed.starts_with("## ") {
        let rest = trimmed.splitn(2, ' ').nth(1)?;
        let without_emoji =
            rest.trim_start_matches(|c: char| c.is_whitespace() || !c.is_alphabetic());
        Some(without_emoji.to_string())
    } else {
        None
    }
}

/// Get an emoji for an agent type (planner, specialist, auditor, etc.)
fn get_agent_emoji(agent_id: &str) -> &'static str {
    if agent_id.contains("planner") {
        "👑"
    } else if agent_id.contains("coder") {
        "👨‍💻"
    } else if agent_id.contains("researcher") {
        "🔍"
    } else if agent_id.contains("architect") {
        "🏗️"
    } else if agent_id.contains("debugger") {
        "🐛"
    } else if agent_id.contains("auditor") {
        "🕵️"
    } else if agent_id.contains("builder") || agent_id.contains("evolution") {
        "🤖"
    } else if agent_id.contains("evaluator") {
        "📊"
    } else if agent_id.contains("memory") {
        "🧠"
    } else {
        "🤖"
    }
}

/// Format timestamp as HH:MM:SS only.
fn format_time_hhmmss(timestamp: &str) -> String {
    timestamp
        .split('T')
        .nth(1)
        .and_then(|t| t.split('.').next())
        .unwrap_or(timestamp)
        .to_string()
}

impl LiveDigestWriter {
    pub fn open(gateway_dir: &Path, session_id: &str, agent_id: &str) -> anyhow::Result<Self> {
        let base = base_session_id(session_id);
        let depth = session_depth(session_id);
        let dir = gateway_dir.join("sessions").join(base);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("digest.md");

        let (digest_turn_seq, is_resumed) = if path.exists() {
            let existing_turns = count_turn_headers(&path)?;
            (existing_turns, false)
        } else {
            let mut f = std::fs::File::create(&path)?;
            writeln!(f, "# Live session digest: `{}`", base)?;
            writeln!(f)?;
            writeln!(
                f,
                "Structured narrative (actions, tool results, errors, annotations). Workflow structure: see `workflow_graph.md` in this folder."
            )?;
            writeln!(f)?;
            (0, false)
        };

        Ok(Self {
            path,
            agent_id: agent_id.to_string(),
            depth,
            digest_turn_seq,
            tools_in_open_turn: 0,
            session_tool_total: 0,
            session_error_total: 0,
            is_resumed,
            turn_buffer: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the markdown header prefix for this agent's depth.
    fn header(&self) -> String {
        match self.depth {
            0 => "##".to_string(),
            1 => "###".to_string(),
            2 => "####".to_string(),
            d => "#".repeat(d + 2),
        }
    }

    /// Write a string into the current turn buffer (not yet flushed).
    fn buf_write(&mut self, s: &str) {
        if let Some(buf) = &mut self.turn_buffer {
            buf.push_str(s);
        }
    }

    /// Flush the turn buffer to disk as a single atomic write.
    fn flush_buffer(&mut self) -> anyhow::Result<()> {
        if let Some(buf) = self.turn_buffer.take() {
            if !buf.is_empty() {
                let mut f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?;
                f.write_all(buf.as_bytes())?;
            }
        }
        self.turn_buffer = None;
        Ok(())
    }

    /// Mark this session as resumed from hibernation/checkpoint.
    /// Call before start_turn() when resuming.
    pub fn mark_resumed(&mut self) {
        self.is_resumed = true;
    }

    /// Session preamble: agent, start time, task preview. Idempotent per file.
    pub fn start_session(&mut self, agent_id: &str, task_preview: &str) -> anyhow::Result<()> {
        let marker = format!("<!-- autonoetic-session-started:{} -->", agent_id);
        let existing = std::fs::read_to_string(&self.path)?;
        if existing.contains(&marker) {
            return Ok(());
        }
        use std::fmt::Write;
        let ts = format_time_hhmmss(&chrono::Utc::now().to_rfc3339());
        let mut buf = String::new();
        let _ = writeln!(buf);
        let _ = writeln!(buf, "{marker}");
        let _ = writeln!(buf, "**Agent:** `{}` | **Started:** [{ts}]", cell(agent_id));
        let _ = writeln!(
            buf,
            "**Task:** {}",
            cell(&truncate_chars(task_preview, 500))
        );
        let _ = writeln!(buf);
        let _ = writeln!(buf, "---");
        let _ = writeln!(buf);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(buf.as_bytes())?;
        Ok(())
    }

    pub fn start_turn(&mut self) -> anyhow::Result<()> {
        self.digest_turn_seq += 1;
        self.tools_in_open_turn = 0;
        let n = self.digest_turn_seq;
        let ts = format_time_hhmmss(&chrono::Utc::now().to_rfc3339());
        let agent = &self.agent_id;

        // Start new turn buffer
        let mut turn = String::new();

        // Resume marker
        if self.is_resumed {
            use std::fmt::Write;
            let _ = writeln!(turn, "---");
            let _ = writeln!(turn, "### ↻ `{agent}` — Resumed from hibernation [{ts}]");
            let _ = writeln!(turn);
            self.is_resumed = false;
            self.turn_buffer = Some(turn);
            return Ok(());
        }

        // Group chat model: only write header on context switch
        let last_agent = get_last_logged_agent(&self.path);
        let agent_with_emoji = format!("{} {}", get_agent_emoji(agent), agent);

        if last_agent.as_deref() != Some(agent) {
            use std::fmt::Write;
            let _ = writeln!(turn, "\n---");
            let _ = writeln!(turn, "### {}", agent_with_emoji);
            let _ = writeln!(turn);
        }

        // Turn header with number and timestamp
        use std::fmt::Write;
        let _ = writeln!(turn, "**Turn {}** [{}]", n, ts);

        self.turn_buffer = Some(turn);
        Ok(())
    }

    pub fn record_llm_round(
        &mut self,
        _model_short: &str,
        _stop_reason: &str,
        _tool_calls: usize,
        _in_tok: u64,
        _out_tok: u64,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn record_llm_retry_note(&mut self, attempt: usize, max: usize) -> anyhow::Result<()> {
        use std::fmt::Write;
        let mut line = String::new();
        let _ = writeln!(
            line,
            "*Note: empty LLM response (stop reason Other); retry {attempt}/{max}.*"
        );
        let _ = writeln!(line);
        self.buf_write(&line);
        Ok(())
    }

    pub fn record_action(&mut self, line: &str) -> anyhow::Result<()> {
        self.tools_in_open_turn += 1;
        self.session_tool_total += 1;
        use std::fmt::Write;
        let mut buf = String::new();
        let _ = writeln!(buf, "* 🛠️ **Tool:** {}", redact_text_for_logs(line));
        self.buf_write(&buf);
        Ok(())
    }

    pub fn record_result(&mut self, line: &str) -> anyhow::Result<()> {
        use std::fmt::Write;
        let mut buf = String::new();
        let _ = writeln!(buf, "* 📄 **Result:** {}", redact_text_for_logs(line));
        let _ = writeln!(buf);
        self.buf_write(&buf);
        Ok(())
    }

    pub fn record_error(&mut self, line: &str) -> anyhow::Result<()> {
        self.session_error_total += 1;
        use std::fmt::Write;
        let mut buf = String::new();
        let _ = writeln!(buf, "* ❌ **Error:** {}", redact_text_for_logs(line));
        let _ = writeln!(buf);
        self.buf_write(&buf);
        Ok(())
    }

    pub fn record_annotation(&mut self, kind: &str, content: &str) -> anyhow::Result<()> {
        let (emoji, label) = match kind {
            "reasoning" => ("🧠", "Reasoning"),
            "decision" => ("🧠", "Decision"),
            "observation" => ("👀", "Observation"),
            "lesson" => ("💡", "Lesson"),
            other => ("📝", other),
        };
        use std::fmt::Write;
        let mut buf = String::new();
        let _ = writeln!(
            buf,
            "* {} **{}:** {}",
            emoji,
            label,
            cell(&truncate_chars(&redact_text_for_logs(content.trim()), 2000))
        );
        self.buf_write(&buf);
        Ok(())
    }

    pub fn record_user_ask_pending(
        &mut self,
        question: &str,
        options_summary: Option<&str>,
    ) -> anyhow::Result<()> {
        use std::fmt::Write;
        let mut buf = String::new();
        let _ = writeln!(
            buf,
            "**User question (`user.ask`):** {}",
            cell(&truncate_chars(&redact_text_for_logs(question), 800))
        );
        if let Some(o) = options_summary.filter(|s| !s.is_empty()) {
            let _ = writeln!(
                buf,
                "**Options:** {}",
                cell(&truncate_chars(&redact_text_for_logs(o), 600))
            );
        }
        let _ = writeln!(buf);
        self.buf_write(&buf);
        Ok(())
    }

    pub fn record_delegation_start(&mut self, agent_id: &str, task_preview: &str) -> anyhow::Result<()> {
        use std::fmt::Write;
        let mut buf = String::new();
        let _ = writeln!(
            buf,
            "* 🚀 **Delegating to:** `{}`",
            redact_text_for_logs(agent_id)
        );
        let _ = writeln!(
            buf,
            "  *Task:* {}",
            cell(&truncate_chars(&redact_text_for_logs(task_preview), 300))
        );
        let _ = writeln!(buf);
        self.buf_write(&buf);
        // Flush immediately so it appears before the child's session logs
        self.flush_buffer()
    }

    pub fn end_turn(&mut self) -> anyhow::Result<()> {
        self.buf_write("\n");
        self.flush_buffer()
    }

    pub fn write_session_summary(&mut self, outcome_reason: &str) -> anyhow::Result<()> {
        let hdr = self.header();
        let agent = &self.agent_id;
        use std::fmt::Write;
        let ts = chrono::Utc::now().to_rfc3339();
        let mut buf = String::new();
        let _ = writeln!(buf, "{hdr} `{agent}` — Session summary — {ts}");
        let _ = writeln!(buf);
        let _ = writeln!(
            buf,
            "**Outcome:** {}",
            cell(&truncate_chars(&redact_text_for_logs(outcome_reason), 200))
        );
        let _ = writeln!(
            buf,
            "**Digest turns:** {} | **Tool invocations (session):** {} | **Errors (session):** {}",
            self.digest_turn_seq, self.session_tool_total, self.session_error_total
        );
        let _ = writeln!(buf);
        let _ = writeln!(buf, "---");
        let _ = writeln!(buf);
        // Flush directly (not through turn_buffer since this may run after end_turn)
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(buf.as_bytes())?;
        Ok(())
    }
}

/// Best-effort: append a resolved `user.ask` answer when resuming from checkpoint (no writer handle).
pub fn append_user_ask_answer_best_effort(
    gateway_dir: &Path,
    base_session_id: &str,
    interaction_id: &str,
    answer_summary: &str,
) {
    let path = gateway_dir
        .join("sessions")
        .join(base_session_id)
        .join("digest.md");
    if !path.exists() {
        return;
    }
    let Ok(mut f) = OpenOptions::new().append(true).open(&path) else {
        return;
    };
    let _ = writeln!(
        f,
        "* 🙋 **User answer (`user.ask` / `{}`):** {}",
        cell(interaction_id),
        cell(&truncate_chars(&redact_text_for_logs(answer_summary), 1200))
    );
}

fn count_turn_headers(path: &Path) -> anyhow::Result<u32> {
    let s = std::fs::read_to_string(path)?;
    let n = s.lines().filter(|l| l.contains("**Turn ")).count();
    Ok(n as u32)
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|")
}

fn truncate_chars(s: &str, max: usize) -> String {
    let mut iter = s.chars();
    let chunk: String = iter.by_ref().take(max).collect();
    if iter.next().is_some() {
        format!("{}…", chunk)
    } else {
        chunk
    }
}

fn as_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// Format a tool result JSON for the digest (full `result` string, not log preview).
pub fn format_tool_digest_result(tool_name: &str, result_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(result_json) else {
        return format!(
            "`{}` — (non-JSON result, {} chars)",
            cell(tool_name),
            result_json.len()
        );
    };

    match tool_name {
        "sandbox.exec" => {
            let exit = v.get("exit_code").and_then(|x| x.as_i64());
            let stdout = as_str(&v, "stdout").unwrap_or("");
            let stderr = as_str(&v, "stderr").unwrap_or("");
            let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true);
            let mut s = format!(
                "`sandbox.exec` exit={} ok={}",
                exit.map(|e| e.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                ok
            );
            if !stdout.is_empty() {
                s.push_str(&format!(
                    " | stdout: `{}`",
                    cell(&truncate_chars(&redact_text_for_logs(stdout), 240))
                ));
            }
            if !stderr.is_empty() {
                s.push_str(&format!(
                    " | stderr: `{}`",
                    cell(&truncate_chars(&redact_text_for_logs(stderr), 240))
                ));
            }
            if let Some(et) = as_str(&v, "error_type") {
                s.push_str(&format!(" | error_type: `{}`", cell(et)));
            }
            if let Some(msg) = as_str(&v, "message") {
                if v.get("ok") == Some(&Value::Bool(false)) {
                    s.push_str(&format!(
                        " | message: `{}`",
                        cell(&truncate_chars(&redact_text_for_logs(msg), 200))
                    ));
                }
            }
            s
        }
        "user.ask" => {
            let q = as_str(&v, "question").unwrap_or("");
            let mut s = format!(
                "`user.ask` — `{}`",
                cell(&truncate_chars(&redact_text_for_logs(q), 200))
            );
            if let Some(opts) = v.get("options").and_then(|x| x.as_array()) {
                if !opts.is_empty() {
                    let labels: Vec<String> = opts
                        .iter()
                        .filter_map(|o| o.get("label").and_then(|l| l.as_str()).map(String::from))
                        .collect();
                    if !labels.is_empty() {
                        s.push_str(&format!(
                            " | options: {}",
                            cell(&truncate_chars(
                                &redact_text_for_logs(&labels.join("; ")),
                                300
                            ))
                        ));
                    }
                }
            }
            if v.get("interaction_required").and_then(|x| x.as_bool()) == Some(true) {
                if let Some(iid) = as_str(&v, "interaction_id") {
                    s.push_str(&format!(" | awaiting interaction `{}`", cell(iid)));
                }
            }
            s
        }
        "content.write" | "content.read" => {
            let name = as_str(&v, "name")
                .or_else(|| as_str(&v, "handle"))
                .unwrap_or("");
            format!(
                "`{}` — `{}`",
                cell(tool_name),
                cell(&truncate_chars(name, 120))
            )
        }
        "artifact.build" | "artifact.inspect" => {
            let id = as_str(&v, "artifact_id")
                .or_else(|| as_str(&v, "id"))
                .unwrap_or("");
            let files = v
                .get("files")
                .and_then(|x| x.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if files > 0 {
                format!(
                    "`{}` id=`{}` files≈{}",
                    cell(tool_name),
                    cell(&truncate_chars(id, 40)),
                    files
                )
            } else {
                format!(
                    "`{}` id=`{}`",
                    cell(tool_name),
                    cell(&truncate_chars(id, 40))
                )
            }
        }
        _ => {
            if v.get("ok") == Some(&Value::Bool(false)) {
                let et = as_str(&v, "error_type").unwrap_or("error");
                let msg = as_str(&v, "message").unwrap_or("");
                format!(
                    "`{}` — {} — {}",
                    cell(tool_name),
                    cell(et),
                    cell(&truncate_chars(&redact_text_for_logs(msg), 280))
                )
            } else {
                let preview = serde_json::to_string(&v).unwrap_or_default();
                format!(
                    "`{}` — `{}`",
                    cell(tool_name),
                    cell(&truncate_chars(&redact_text_for_logs(&preview), 320))
                )
            }
        }
    }
}

/// Build the action line for a tool invocation (name + short args).
pub fn format_tool_action_line(tool_name: &str, arguments_redacted: &str) -> String {
    format!(
        "Called `{}` with `{}`",
        cell(tool_name),
        cell(&truncate_chars(arguments_redacted, 220))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn base_session_splits_subsession() {
        assert_eq!(base_session_id("demo-session"), "demo-session");
        assert_eq!(
            base_session_id("demo-session/coder.default-abc"),
            "demo-session"
        );
    }

    #[test]
    fn digest_creates_file_and_turns() {
        let tmp = tempdir().unwrap();
        let gw = tmp.path().join(".gateway");
        let mut w = LiveDigestWriter::open(&gw, "s1", "agent.a").unwrap();
        w.start_session("agent.a", "hello task").unwrap();
        w.start_turn().unwrap();
        w.record_action("Called `echo`").unwrap();
        w.record_result("`echo` ok").unwrap();
        w.record_annotation("lesson", "note").unwrap();
        w.end_turn().unwrap();
        let body = std::fs::read_to_string(w.path()).unwrap();
assert!(body.contains("# Live session digest"));
        assert!(body.contains("### 🤖 agent.a"));
        assert!(body.contains("**Turn 1**"));
        assert!(body.contains("💡 **Lesson:** note"));
    }

    #[test]
    fn reopen_continues_turn_numbers() {
        let tmp = tempdir().unwrap();
        let gw = tmp.path().join(".gateway");
        {
            let mut w = LiveDigestWriter::open(&gw, "s2", "agent.a").unwrap();
            w.start_turn().unwrap();
            w.end_turn().unwrap();
        }
        let mut w2 = LiveDigestWriter::open(&gw, "s2", "agent.a").unwrap();
        w2.start_turn().unwrap();
        w2.end_turn().unwrap();
        let body = std::fs::read_to_string(w2.path()).unwrap();
assert!(body.contains("### 🤖 agent.a"));
        assert!(body.contains("**Turn 1**"));
        assert!(body.contains("**Turn 2**"));
    }

    #[test]
    fn format_sandbox_exec() {
        let j = r#"{"ok":true,"exit_code":1,"stdout":"a","stderr":"boom"}"#;
        let s = format_tool_digest_result("sandbox.exec", j);
        assert!(s.contains("exit=1"));
        assert!(s.contains("stdout"));
        assert!(s.contains("stderr"));
    }

    #[test]
    fn digest_redacts_secret_like_annotation() {
        let tmp = tempdir().unwrap();
        let gw = tmp.path().join(".gateway");
        let mut w = LiveDigestWriter::open(&gw, "s3", "agent.a").unwrap();
        w.start_turn().unwrap();
        w.record_annotation("observation", "Authorization: Bearer top-secret-value")
            .unwrap();
        w.end_turn().unwrap();
        let body = std::fs::read_to_string(w.path()).unwrap();
        assert!(body.contains("***REDACTED***"));
        assert!(!body.contains("top-secret-value"));
    }
}
