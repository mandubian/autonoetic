//! History persistence helpers — extract, merge, and persist conversation
//! transcripts to the content-addressed store.

use crate::llm::Message;
use crate::runtime::context::safe_prefix_by_bytes;
use crate::runtime::disclosure::DisclosureState;
use crate::runtime::session_tracer::SessionTracer;
use std::path::Path;

pub(crate) fn normalize_message_for_persist_snapshot(
    msg: &Message,
    disclosure_state: &DisclosureState,
) -> Message {
    let mut m = msg.clone();
    m.content =
        crate::log_redaction::redact_text_for_logs(&disclosure_state.filter_reply(&m.content));
    for tc in &mut m.tool_calls {
        tc.arguments = crate::log_redaction::redact_text_for_logs(
            &disclosure_state.filter_reply(&tc.arguments),
        );
    }
    m
}

/// Longest `k` such that `merged[len-k..]` matches `incoming[0..k]` after normalizing
/// incoming messages to the persisted snapshot form. Avoids re-appending the full in-memory
/// transcript on every hibernate (which duplicated older turns in the content store).
pub(crate) fn longest_history_suffix_prefix_overlap(
    merged: &[Message],
    incoming: &[Message],
    disclosure_state: &DisclosureState,
) -> usize {
    let max_k = merged.len().min(incoming.len());
    for k in (1..=max_k).rev() {
        let suf = &merged[merged.len() - k..];
        let pre = &incoming[..k];
        if suf.iter().zip(pre.iter()).all(|(persisted, fresh)| {
            *persisted == normalize_message_for_persist_snapshot(fresh, disclosure_state)
        }) {
            return k;
        }
    }
    0
}

/// Persists conversation history to content store at diagnostic checkpoints.
pub(crate) fn persist_history_to_content_store(
    _agent_dir: &Path,
    session_id: &str,
    history: &[Message],
    gateway_dir: &Path,
    tracer: &mut SessionTracer,
    disclosure_state: &DisclosureState,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    agent_id: Option<&str>,
    session_started_at: Option<&str>,
) -> anyhow::Result<()> {
    use crate::runtime::content_store::ContentStore;
    const MAX_PERSISTED_MESSAGES: usize = 400;

    let store = ContentStore::new(gateway_dir)?;

    // Merge with previously persisted history so reconnecting to the same
    // session can restore prior turns instead of only the latest run window.
    let mut merged_history: Vec<Message> = match store.read_by_name(session_id, "session_history") {
        Ok(existing) => serde_json::from_slice(&existing).unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    if merged_history.is_empty() {
        merged_history.extend_from_slice(history);
    } else {
        // Skip the live system prompt when merging: persisted history already keeps the
        // first system block; the in-memory `history` repeats it every run.
        let incoming_tail: Vec<Message> = history
            .iter()
            .filter(|m| !matches!(m.role, crate::llm::Role::System))
            .cloned()
            .collect();

        let overlap = longest_history_suffix_prefix_overlap(
            &merged_history,
            &incoming_tail,
            disclosure_state,
        );
        merged_history.extend(incoming_tail[overlap..].iter().cloned());
    }

    // Bound persisted history size while preserving the first system message if present.
    if merged_history.len() > MAX_PERSISTED_MESSAGES {
        let keep = MAX_PERSISTED_MESSAGES;
        let mut trimmed = Vec::with_capacity(keep);
        if let Some(first) = merged_history.first().cloned() {
            if matches!(first.role, crate::llm::Role::System) {
                trimmed.push(first);
                let tail_keep = keep.saturating_sub(1);
                let tail_start = merged_history.len().saturating_sub(tail_keep);
                trimmed.extend(merged_history[tail_start..].iter().cloned());
                merged_history = trimmed;
            } else {
                let tail_start = merged_history.len().saturating_sub(keep);
                merged_history = merged_history[tail_start..].to_vec();
            }
        }
    }

    // Serialize history
    for msg in &mut merged_history {
        // Persist a redacted view of message content.
        msg.content = crate::log_redaction::redact_text_for_logs(
            &disclosure_state.filter_reply(&msg.content),
        );
        for tc in &mut msg.tool_calls {
            tc.arguments = crate::log_redaction::redact_text_for_logs(
                &disclosure_state.filter_reply(&tc.arguments),
            );
        }
    }

    let history_json = serde_json::to_string(&merged_history)?;
    let history_handle = store.write(history_json.as_bytes())?;

    // Register in session
    store.register_name(session_id, "session_history", &history_handle)?;

    // Extract searchable excerpt for FTS
    let excerpt = extract_searchable_excerpt(&merged_history);

    // Upsert session transcript to database for FTS
    if let Some(gs) = gateway_store {
        let root_session_id = crate::runtime::content_store::root_session_id(session_id);
        let transcript_id = format!("stx-{}", session_id);
        let record = autonoetic_types::causal_chain::SessionTranscriptRecord {
            transcript_id,
            session_id: session_id.to_string(),
            root_session_id: root_session_id.to_string(),
            agent_id: agent_id.unwrap_or("unknown").to_string(),
            revision_id: None,
            user_id: None,
            started_at: session_started_at
                .map(|s| s.to_string())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            ended_at: None,
            status: "active".to_string(),
            turn_count: merged_history.len() as i64,
            transcript_handle: Some(history_handle.to_string()),
            excerpt: Some(excerpt),
            origin_node_id: None,
        };
        if let Err(e) = gs.upsert_session_transcript(&record) {
            tracing::warn!(
                target: "lifecycle",
                session_id = %session_id,
                error = %e,
                "Failed to upsert session transcript"
            );
        }
    }

    // Log causal chain entry
    tracer.log_history_persisted(history.len(), &history_handle);

    tracing::debug!(
        target: "lifecycle",
        session_id = %session_id,
        handle = %history_handle,
        message_count = merged_history.len(),
        "Persisted session history to content store"
    );

    Ok(())
}

pub fn extract_searchable_excerpt(messages: &[Message]) -> String {
    const MAX_CHARS: usize = 8000;
    let mut parts = Vec::new();
    let mut total = 0;
    for msg in messages {
        if !msg.content.is_empty() {
            let role_label = match msg.role {
                crate::llm::Role::System => "[system]",
                crate::llm::Role::User => "[user]",
                crate::llm::Role::Assistant => "[assistant]",
                crate::llm::Role::Tool => "[tool]",
            };
            let line = format!("{}: {}", role_label, msg.content);
            let line_len = line.len();
            if total + line_len > MAX_CHARS {
                let remaining = MAX_CHARS.saturating_sub(total);
                if remaining > 0 {
                    let prefix = safe_prefix_by_bytes(&line, remaining);
                    if !prefix.is_empty() {
                        parts.push(prefix.to_string());
                    }
                }
                break;
            }
            parts.push(line);
            total += line_len;
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod history_persistence_tests {
    use super::*;
    use crate::llm::ToolCall;
    use crate::runtime::content_store::ContentStore;
    use crate::runtime::disclosure::DisclosureState;
    use tempfile::tempdir;

    #[test]
    fn persisted_history_redacts_secret_like_text_and_tool_args() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir)?;

        let mut assistant = Message::assistant("Will use Authorization: Bearer very-secret-value");
        assistant.tool_calls = vec![ToolCall {
            id: "tc-1".to_string(),
            name: "web_fetch".to_string(),
            arguments: r#"{"headers":{"authorization":"Bearer very-secret-value"}}"#.to_string(),
        }];

        let history = vec![Message::system("sys"), assistant];
        let mut tracer = SessionTracer::test_tracer();
        let disclosure = DisclosureState::default();

        persist_history_to_content_store(
            temp.path(),
            "sess-redact",
            &history,
            &gateway_dir,
            &mut tracer,
            &disclosure,
            None,
            None,
            None,
        )?;

        let store = ContentStore::new(&gateway_dir)?;
        let bytes = store.read_by_name("sess-redact", "session_history")?;
        let persisted: Vec<Message> = serde_json::from_slice(&bytes)?;

        let raw = serde_json::to_string(&persisted)?;
        assert!(raw.contains("***REDACTED***"));
        assert!(!raw.contains("very-secret-value"));
        Ok(())
    }

    #[test]
    fn extract_searchable_excerpt_handles_unicode_boundary_without_panic() {
        let msg = Message::user(format!("{}{}", "x".repeat(7992), "─"));
        let excerpt = extract_searchable_excerpt(&[msg]);
        assert!(excerpt.len() <= 8000);
    }

    #[test]
    fn sequential_persist_appends_only_new_tail() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir)?;

        let mut tracer = SessionTracer::test_tracer();
        let disclosure = DisclosureState::default();

        let h1 = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi there"),
        ];
        persist_history_to_content_store(
            temp.path(),
            "sess-merge",
            &h1,
            &gateway_dir,
            &mut tracer,
            &disclosure,
            None,
            None,
            None,
        )?;

        // Second hibernate passes the *full* transcript again (simulates executor state).
        let h2 = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::user("next"),
            Message::assistant("done"),
        ];
        persist_history_to_content_store(
            temp.path(),
            "sess-merge",
            &h2,
            &gateway_dir,
            &mut tracer,
            &disclosure,
            None,
            None,
            None,
        )?;

        let store = ContentStore::new(&gateway_dir)?;
        let bytes = store.read_by_name("sess-merge", "session_history")?;
        let persisted: Vec<Message> = serde_json::from_slice(&bytes)?;

        assert_eq!(
            persisted.len(),
            5,
            "expected sys + 4 non-system, no duplicated hello turn"
        );
        assert_eq!(persisted[1].content, "hello");
        assert_eq!(persisted[2].content, "hi there");
        assert_eq!(persisted[3].content, "next");
        assert_eq!(persisted[4].content, "done");
        Ok(())
    }
}
