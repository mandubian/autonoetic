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
            (existing_turns, existing_turns > 0)
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
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        let ts = chrono::Utc::now().to_rfc3339();
        writeln!(f)?;
        writeln!(f, "{marker}")?;
        writeln!(f, "**Agent:** `{}` | **Started:** {ts}", cell(agent_id))?;
        writeln!(f, "**Task:** {}", cell(&truncate_chars(task_preview, 500)))?;
        writeln!(f)?;
        writeln!(f, "---")?;
        writeln!(f)?;
        Ok(())
    }

    pub fn start_turn(&mut self) -> anyhow::Result<()> {
        self.digest_turn_seq += 1;
        self.tools_in_open_turn = 0;
        let n = self.digest_turn_seq;
        let ts = chrono::Utc::now().to_rfc3339();
        let hdr = self.header();
        let agent = &self.agent_id;
        let mut f = OpenOptions::new().append(true).open(&self.path)?;

        // Resume marker
        if self.is_resumed {
            writeln!(f, "---")?;
            writeln!(f, "*↻ `{agent}` resumed from hibernation*")?;
            writeln!(f)?;
            self.is_resumed = false;
        }

        writeln!(f, "{hdr} `{agent}` — Turn {n} — {ts}")?;
        writeln!(f)?;
        Ok(())
    }

    pub fn record_llm_round(
        &mut self,
        model_short: &str,
        stop_reason: &str,
        tool_calls: usize,
        in_tok: u64,
        out_tok: u64,
    ) -> anyhow::Result<()> {
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(
            f,
            "*LLM:* `{}` | stop: `{}` | tool calls: {} | tokens in/out: {}/{}*",
            cell(model_short),
            cell(stop_reason),
            tool_calls,
            in_tok,
            out_tok
        )?;
        writeln!(f)?;
        Ok(())
    }

    pub fn record_llm_retry_note(&mut self, attempt: usize, max: usize) -> anyhow::Result<()> {
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(
            f,
            "*Note: empty LLM response (stop reason Other); retry {attempt}/{max}.*"
        )?;
        writeln!(f)?;
        Ok(())
    }

    pub fn record_action(&mut self, line: &str) -> anyhow::Result<()> {
        self.tools_in_open_turn += 1;
        self.session_tool_total += 1;
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(f, "**Action:** {}", redact_text_for_logs(line))?;
        Ok(())
    }

    pub fn record_result(&mut self, line: &str) -> anyhow::Result<()> {
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(f, "**Result:** {}", redact_text_for_logs(line))?;
        writeln!(f)?;
        Ok(())
    }

    pub fn record_error(&mut self, line: &str) -> anyhow::Result<()> {
        self.session_error_total += 1;
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(f, "**Error:** {}", redact_text_for_logs(line))?;
        writeln!(f)?;
        Ok(())
    }

    pub fn record_annotation(&mut self, kind: &str, content: &str) -> anyhow::Result<()> {
        let label = match kind {
            "reasoning" => "Reasoning",
            "decision" => "Decision",
            "observation" => "Observation",
            "lesson" => "Lesson",
            other => other,
        };
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(
            f,
            "**{}:** {}",
            label,
            cell(&truncate_chars(&redact_text_for_logs(content.trim()), 2000))
        )?;
        writeln!(f)?;
        Ok(())
    }

    pub fn record_user_ask_pending(
        &mut self,
        question: &str,
        options_summary: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(
            f,
            "**User question (`user.ask`):** {}",
            cell(&truncate_chars(&redact_text_for_logs(question), 800))
        )?;
        if let Some(o) = options_summary.filter(|s| !s.is_empty()) {
            writeln!(
                f,
                "**Options:** {}",
                cell(&truncate_chars(&redact_text_for_logs(o), 600))
            )?;
        }
        writeln!(f)?;
        Ok(())
    }

    pub fn end_turn(&mut self) -> anyhow::Result<()> {
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        if self.tools_in_open_turn > 0 {
            writeln!(
                f,
                "*Turn wrap-up: {} tool call(s) in this block.*",
                self.tools_in_open_turn
            )?;
            writeln!(f)?;
        }
        writeln!(f, "---")?;
        writeln!(f)?;
        Ok(())
    }

    pub fn write_session_summary(&mut self, outcome_reason: &str) -> anyhow::Result<()> {
        let hdr = self.header();
        let agent = &self.agent_id;
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        let ts = chrono::Utc::now().to_rfc3339();
        writeln!(f, "{hdr} `{agent}` — Session summary — {ts}")?;
        writeln!(f)?;
        writeln!(
            f,
            "**Outcome:** {}",
            cell(&truncate_chars(&redact_text_for_logs(outcome_reason), 200))
        )?;
        writeln!(
            f,
            "**Digest turns:** {} | **Tool invocations (session):** {} | **Errors (session):** {}",
            self.digest_turn_seq, self.session_tool_total, self.session_error_total
        )?;
        writeln!(f)?;
        writeln!(f, "---")?;
        writeln!(f)?;
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
        "**User answer (`user.ask` / `{}`):** {}",
        cell(interaction_id),
        cell(&truncate_chars(&redact_text_for_logs(answer_summary), 1200))
    );
    let _ = writeln!(f);
}

fn count_turn_headers(path: &Path) -> anyhow::Result<u32> {
    let s = std::fs::read_to_string(path)?;
    let n = s.lines().filter(|l| l.contains(" — Turn ")).count();
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
        assert!(body.contains("## `agent.a` — Turn 1"));
        assert!(body.contains("**Lesson:** note"));
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
        let body = std::fs::read_to_string(w2.path()).unwrap();
        assert!(body.contains("## `agent.a` — Turn 1"));
        assert!(body.contains("## `agent.a` — Turn 2"));
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
        w.record_annotation("observation", "Authorization: Bearer top-secret-value")
            .unwrap();
        let body = std::fs::read_to_string(w.path()).unwrap();
        assert!(body.contains("***REDACTED***"));
        assert!(!body.contains("top-secret-value"));
    }
}
