//! Label-listing read queries — `labels.list` operator RPC (#974), RFC §9.3.
//!
//! Every function here is a root-tree-scoped read returning **metadata only**:
//! ids, scopes, labels, timestamps. No content column (`memories.content`,
//! `execution_traces.stdout`/`stderr`, `agent_messages.message`) is ever
//! selected — that is the defining invariant of this plane (I-14: the label
//! plane is gateway-only; the operator view is label-shaped, never a content
//! dump).
//!
//! Root-tree scoping reuses the established pattern
//! `session_id = ? OR session_id LIKE '<root>/%' ESCAPE '\'`. Memories are not
//! keyed by session id but by a `session:<sid>` scope convention, so they are
//! scoped the same way against `scope`.
//!
//! Absence ⇒ unrestricted everywhere (only restrictive labels are stored), and
//! a present-but-malformed label row fails the conversion (fail-closed) rather
//! than degrading to `None` — surfaced by the router as `-32000`, never
//! silently "unrestricted".

use anyhow::Result;
use autonoetic_types::egress::{
    EgressLabel, LabeledArtifactRow, LabeledEnvelopeRow, LabeledMemoryRow, LabeledMessageRow,
    LabeledTraceRow, SessionTaintRow,
};
use rusqlite::{params, Connection};

use super::util::{decode_egress_label_json, escape_sqlite_like_fragment};

/// Build the `(session_id = ? OR session_id LIKE ? ESCAPE '\')` fragment and
/// its bind values for a root tree, returning `(exact, like_pattern)` so the
/// caller can repeat the pair across multiple bound parameters.
fn root_tree_patterns(root_session_id: &str) -> (String, String) {
    (
        root_session_id.to_string(),
        format!("{}/%", escape_sqlite_like_fragment(root_session_id)),
    )
}

// -----------------------------------------------------------------------
// Envelopes — reconstructed from `egress.envelope_labeled` causal events.
// -----------------------------------------------------------------------

/// List every `egress.envelope_labeled` event under the root tree, newest
/// first. The durable record of an envelope *is* this causal event (envelopes
/// are otherwise ephemeral, held in the in-turn sidecar), so the row is a
/// typed view onto the event payload. `truncated` is set when the cap is hit.
///
/// Returns events in descending timestamp order so a panel renders the most
/// recent labelings first; the caller reorders for display as needed.
pub(super) fn list_envelope_events_for_root(
    conn: &Connection,
    root_session_id: &str,
    limit: i64,
) -> Result<Vec<LabeledEnvelopeRow>> {
    let (exact, like) = root_tree_patterns(root_session_id);
    let mut stmt = conn.prepare(
        "SELECT session_id, turn_id, timestamp, target, payload
         FROM causal_events
         WHERE category = 'egress' AND action = 'egress.envelope_labeled'
           AND (session_id = ?1 OR session_id LIKE ?2 ESCAPE '\\')
         ORDER BY timestamp DESC, event_seq DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![exact, like, limit], |row| {
        let session_id: String = row.get(0)?;
        let turn_id: Option<String> = row.get(1)?;
        let timestamp: String = row.get(2)?;
        // `target` carries the envelope id for envelope_labeled events.
        let target_envelope_id: Option<String> = row.get(3)?;
        let payload_json: Option<String> = row.get(4)?;
        Ok((session_id, turn_id, timestamp, target_envelope_id, payload_json))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (session_id, turn_id, timestamp, target_envelope_id, payload_json) = r?;
        let payload: serde_json::Value = payload_json
            .as_deref()
            .and_then(|p| serde_json::from_str(p).ok())
            .unwrap_or(serde_json::Value::Null);
        out.push(envelope_row_from_payload(
            &payload,
            target_envelope_id.as_deref(),
            session_id,
            turn_id,
            timestamp,
        )?);
    }
    Ok(out)
}

/// Parse one `egress.envelope_labeled` payload (built at
/// `egress_labeler.rs::emit_envelope_labeled_event`) into a row.
fn envelope_row_from_payload(
    payload: &serde_json::Value,
    fallback_envelope_id: Option<&str>,
    session_id: String,
    turn_id: Option<String>,
    timestamp: String,
) -> Result<LabeledEnvelopeRow> {
    let s = |k: &str| payload.get(k).and_then(|v| v.as_str()).map(String::from);
    let arr = |k: &str| -> Vec<String> {
        payload
            .get(k)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    // `matched_rule_scopes` is emitted as an array of `{rule, scope}` objects
    // (see egress_labeler::emit_envelope_labeled_event). Flatten to the scope
    // strings in order so the row mirrors `matched_rules`.
    let rule_scopes = |k: &str| -> Vec<String> {
        payload
            .get(k)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("scope").and_then(|s| s.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let bool_or = |k: &str, default: bool| {
        payload.get(k).and_then(|v| v.as_bool()).unwrap_or(default)
    };
    // `artifact_labels_applied` is an array of artifact ids in the payload.
    let artifact_labels_applied = arr("artifact_labels_applied");
    // `workspace_labels_applied` is an array of agent ids in the payload
    // (RFC §11, #1001).
    let workspace_labels_applied = arr("workspace_labels_applied");
    let envelope_id = s("envelope_id")
        .or_else(|| fallback_envelope_id.map(String::from))
        .unwrap_or_default();
    // `label` serializes the transparent sink-set array.
    let label: EgressLabel = serde_json::from_value(
        payload
            .get("label")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![])),
    )?;
    Ok(LabeledEnvelopeRow {
        envelope_id,
        session_id,
        turn_id,
        timestamp,
        tool_name: s("tool_name").or_else(|| s("tool")),
        tool_call_id: s("tool_call_id"),
        label,
        resolution: s("resolution"),
        matched_rules: arr("matched_rules"),
        matched_rule_scopes: rule_scopes("matched_rule_scopes"),
        parent_envelope_ids: arr("parent_envelope_ids"),
        taint_applied: bool_or("taint_applied", false),
        artifact_labels_applied,
        workspace_labels_applied,
        bundle_floor_applied: bool_or("bundle_floor_applied", false),
    })
}

// -----------------------------------------------------------------------
// Session taints — every restrictive row under the root tree (children too).
// -----------------------------------------------------------------------

pub(super) fn list_session_taints_for_root(
    conn: &Connection,
    root_session_id: &str,
) -> Result<Vec<SessionTaintRow>> {
    let (exact, like) = root_tree_patterns(root_session_id);
    let mut stmt = conn.prepare(
        "SELECT session_id, label_json, updated_at
         FROM session_egress_taint
         WHERE session_id = ?1 OR session_id LIKE ?2 ESCAPE '\\'
         ORDER BY updated_at DESC",
    )?;
    let rows = stmt.query_map(params![exact, like], |row| {
        let session_id: String = row.get(0)?;
        let label_json: Option<String> = row.get(1)?;
        let updated_at: String = row.get(2)?;
        Ok((session_id, label_json, updated_at))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (session_id, label_json, updated_at) = r?;
        let label = decode_egress_label_json(label_json)?
            // A present row with no decodable label cannot stand — fail closed.
            .unwrap_or_else(EgressLabel::empty);
        out.push(SessionTaintRow {
            session_id,
            label,
            updated_at,
        });
    }
    Ok(out)
}

// -----------------------------------------------------------------------
// Memories — `session:<root>` scope tree, metadata only (never content).
// -----------------------------------------------------------------------

pub(super) fn list_labeled_memories_for_root(
    conn: &Connection,
    root_session_id: &str,
) -> Result<Vec<LabeledMemoryRow>> {
    // Memory scopes follow `session:<sid>` / `session:<sid>:turn:<t>` (and a
    // few `:suffix` forms). A session-id `root/child` maps to a scope
    // `session:root/child[:...]`, so the same `session:<root>/%` prefix tree
    // scopes it. Escape both the literal exact form and the prefix form.
    let escaped_root = escape_sqlite_like_fragment(root_session_id);
    let exact = format!("session:{}", root_session_id);
    let child_prefix = format!("session:{}/%", escaped_root);
    let sub_scope_prefix = format!("session:{}:%", escaped_root);
    let mut stmt = conn.prepare(
        "SELECT memory_id, scope, owner_agent_id, egress_label_json, updated_at
         FROM memories
         WHERE egress_label_json IS NOT NULL AND egress_label_json != ''
           AND (scope = ?1 OR scope LIKE ?2 ESCAPE '\\' OR scope LIKE ?3 ESCAPE '\\')
         ORDER BY updated_at DESC",
    )?;
    let rows = stmt.query_map(params![exact, child_prefix, sub_scope_prefix], |row| {
        let memory_id: String = row.get(0)?;
        let scope: String = row.get(1)?;
        let owner_agent_id: String = row.get(2)?;
        let label_json: Option<String> = row.get(3)?;
        let updated_at: String = row.get(4)?;
        Ok((memory_id, scope, owner_agent_id, label_json, updated_at))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (memory_id, scope, owner_agent_id, label_json, updated_at) = r?;
        let label = decode_egress_label_json(label_json)?
            .unwrap_or_else(EgressLabel::empty);
        out.push(LabeledMemoryRow {
            memory_id,
            scope,
            owner_agent_id,
            label,
            updated_at,
        });
    }
    Ok(out)
}

// -----------------------------------------------------------------------
// Artifacts — `artifact_egress_labels` joined to session-scoped refs.
// -----------------------------------------------------------------------

pub(super) fn list_labeled_artifacts_for_root(
    conn: &Connection,
    root_session_id: &str,
) -> Result<Vec<LabeledArtifactRow>> {
    let (exact, like) = root_tree_patterns(root_session_id);
    // The label table is session-agnostic; `artifact_refs` (scope_type=session)
    // is what ties an artifact to the queried root tree. LEFT JOIN so an
    // artifact with a label but no surviving ref still surfaces (the ref may
    // have expired/been revoked) — `ref_id` is then None. DISTINCT because one
    // artifact can have several refs across the tree.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT l.artifact_id, r.ref_id, r.scope_id, l.label_json, l.updated_at
         FROM artifact_egress_labels l
         LEFT JOIN artifact_refs r
           ON r.artifact_id = l.artifact_id
          AND r.scope_type = 'session'
          AND (r.scope_id = ?1 OR r.scope_id LIKE ?2 ESCAPE '\\')
         WHERE r.scope_id IS NOT NULL
         ORDER BY l.updated_at DESC",
    )?;
    let rows = stmt.query_map(params![exact, like], |row| {
        let artifact_id: String = row.get(0)?;
        let ref_id: Option<String> = row.get(1)?;
        let scope_id: String = row.get(2)?;
        let label_json: Option<String> = row.get(3)?;
        let updated_at: String = row.get(4)?;
        Ok((artifact_id, ref_id, scope_id, label_json, updated_at))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (artifact_id, ref_id, scope_id, label_json, updated_at) = r?;
        let label = decode_egress_label_json(label_json)?
            .unwrap_or_else(EgressLabel::empty);
        out.push(LabeledArtifactRow {
            artifact_id,
            ref_id,
            scope_id,
            label,
            updated_at,
        });
    }
    Ok(out)
}

// -----------------------------------------------------------------------
// Execution traces — metadata only, never stdout/stderr/command.
// -----------------------------------------------------------------------

pub(super) fn list_labeled_traces_for_root(
    conn: &Connection,
    root_session_id: &str,
) -> Result<Vec<LabeledTraceRow>> {
    let (exact, like) = root_tree_patterns(root_session_id);
    let mut stmt = conn.prepare(
        "SELECT trace_id, session_id, turn_id, tool_name, egress_label_json, timestamp
         FROM execution_traces
         WHERE egress_label_json IS NOT NULL AND egress_label_json != ''
           AND (session_id = ?1 OR session_id LIKE ?2 ESCAPE '\\')
         ORDER BY timestamp DESC",
    )?;
    let rows = stmt.query_map(params![exact, like], |row| {
        let trace_id: String = row.get(0)?;
        let session_id: String = row.get(1)?;
        let turn_id: Option<String> = row.get(2)?;
        let tool_name: String = row.get(3)?;
        let label_json: Option<String> = row.get(4)?;
        let timestamp: String = row.get(5)?;
        Ok((trace_id, session_id, turn_id, tool_name, label_json, timestamp))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (trace_id, session_id, turn_id, tool_name, label_json, timestamp) = r?;
        let label = decode_egress_label_json(label_json)?
            .unwrap_or_else(EgressLabel::empty);
        out.push(LabeledTraceRow {
            trace_id,
            session_id,
            turn_id,
            tool_name: Some(tool_name),
            label,
            timestamp,
        });
    }
    Ok(out)
}

// -----------------------------------------------------------------------
// Agent messages — metadata only, never the message body.
// -----------------------------------------------------------------------

pub(super) fn list_labeled_messages_for_root(
    conn: &Connection,
    root_session_id: &str,
) -> Result<Vec<LabeledMessageRow>> {
    let (exact, like) = root_tree_patterns(root_session_id);
    let mut stmt = conn.prepare(
        "SELECT message_id, sender_session_id, sender_agent_id, target_pattern,
                egress_label_json, created_at
         FROM agent_messages
         WHERE egress_label_json IS NOT NULL AND egress_label_json != ''
           AND (sender_session_id = ?1 OR sender_session_id LIKE ?2 ESCAPE '\\')
         ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map(params![exact, like], |row| {
        let message_id: String = row.get(0)?;
        let session_id: String = row.get(1)?;
        let from_agent: Option<String> = row.get(2)?;
        let target_pattern: Option<String> = row.get(3)?;
        let label_json: Option<String> = row.get(4)?;
        let timestamp: String = row.get(5)?;
        Ok((
            message_id,
            session_id,
            from_agent,
            target_pattern,
            label_json,
            timestamp,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (message_id, session_id, from_agent, target_pattern, label_json, timestamp) = r?;
        let label = decode_egress_label_json(label_json)?
            .unwrap_or_else(EgressLabel::empty);
        out.push(LabeledMessageRow {
            message_id,
            session_id,
            from_agent,
            to_agent: target_pattern,
            label,
            timestamp,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::gateway_store::GatewayStore;
    use autonoetic_types::egress::EgressLabel;

    /// Open a fresh migrated store in a temp dir.
    fn fresh_store() -> (tempfile::TempDir, std::sync::Arc<GatewayStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let store = std::sync::Arc::new(store);
        (dir, store)
    }

    #[test]
    fn root_tree_patterns_escapes_and_suffixes() {
        let (exact, like) = root_tree_patterns("demo/x_y%");
        assert_eq!(exact, "demo/x_y%");
        // `_` and `%` escaped; `/` literal.
        assert_eq!(like, "demo/x\\_y\\%/%");
    }

    #[test]
    fn session_taints_root_scope_includes_children() {
        let (_dir, store) = fresh_store();
        store
            .set_session_egress_taint("root", &EgressLabel::local_only())
            .unwrap();
        store
            .set_session_egress_taint("root/coder", &EgressLabel::no_remote_model())
            .unwrap();
        store
            .set_session_egress_taint("other", &EgressLabel::local_only())
            .unwrap();
        let conn = store.conn.lock().unwrap();
        let rows = list_session_taints_for_root(&conn, "root").unwrap();
        let ids: Vec<_> = rows.iter().map(|r| r.session_id.as_str()).collect();
        assert!(ids.contains(&"root"));
        assert!(ids.contains(&"root/coder"));
        assert!(!ids.contains(&"other"));
        let coder = rows.iter().find(|r| r.session_id == "root/coder").unwrap();
        assert_eq!(coder.label, EgressLabel::no_remote_model());
    }

    #[test]
    fn memories_scope_tree_filters_to_root() {
        let (_dir, store) = fresh_store();
        let conn = store.conn.lock().unwrap();
        let insert = |scope: &str| {
            conn.execute(
                "INSERT INTO memories (memory_id, scope, owner_agent_id, writer_agent_id, \
                 source_type, source_ref, created_at, updated_at, content, content_hash, visibility)
                 VALUES (?1, ?2, 'a', 'a', 'agent_write', 'r', 't', 't', 'c', 'h', 'private')",
                params![format!("m-{scope}"), scope],
            )
            .unwrap();
            conn.execute(
                "UPDATE memories SET egress_label_json = ?1 WHERE scope = ?2",
                params![serde_json::to_string(&EgressLabel::local_only()).unwrap(), scope],
            )
            .unwrap();
        };
        insert("session:root");
        insert("session:root/coder");
        insert("session:root:turn:1");
        insert("session:other");
        let rows = list_labeled_memories_for_root(&conn, "root").unwrap();
        let scopes: Vec<_> = rows.iter().map(|r| r.scope.as_str()).collect();
        assert!(scopes.contains(&"session:root"));
        assert!(scopes.contains(&"session:root/coder"));
        assert!(scopes.contains(&"session:root:turn:1"));
        assert!(!scopes.contains(&"session:other"));
        // Metadata only — no content column is selected.
        assert!(rows.iter().all(|r| r.memory_id.starts_with("m-")));
    }

    #[test]
    fn artifacts_join_filters_to_root_tree() {
        let (_dir, store) = fresh_store();
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO artifact_egress_labels (artifact_id, label_json, updated_at)
             VALUES ('art_aaaa1111', ?1, 't1')",
            params![serde_json::to_string(&EgressLabel::local_only()).unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artifact_egress_labels (artifact_id, label_json, updated_at)
             VALUES ('art_bbbb2222', ?1, 't2')",
            params![serde_json::to_string(&EgressLabel::no_remote_model()).unwrap()],
        )
        .unwrap();
        // art_a belongs to root, art_b belongs to another root.
        conn.execute(
            "INSERT INTO artifact_refs (ref_id, scope_type, scope_id, artifact_id, \
             artifact_digest, created_by_agent_id, created_at)
             VALUES ('ar.aaa', 'session', 'root', 'art_aaaa1111', 'd', 'a', 't')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artifact_refs (ref_id, scope_type, scope_id, artifact_id, \
             artifact_digest, created_by_agent_id, created_at)
             VALUES ('ar.bbb', 'session', 'other', 'art_bbbb2222', 'd', 'a', 't')",
            [],
        )
        .unwrap();
        let rows = list_labeled_artifacts_for_root(&conn, "root").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].artifact_id, "art_aaaa1111");
        assert_eq!(rows[0].ref_id.as_deref(), Some("ar.aaa"));
        assert_eq!(rows[0].scope_id, "root");
    }

    #[test]
    fn traces_and_messages_root_scoped() {
        let (_dir, store) = fresh_store();
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO execution_traces (trace_id, agent_id, session_id, timestamp, tool_name, \
             success) VALUES ('t1', 'a', 'root/coder', 't', 'sandbox_exec', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE execution_traces SET egress_label_json = ?1 WHERE trace_id = 't1'",
            params![serde_json::to_string(&EgressLabel::local_only()).unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_messages (message_id, sender_session_id, sender_agent_id, \
             target_pattern, message, created_at, egress_label_json) VALUES \
             ('m1', 'root', 'lead', '*', 'body', 't', ?1)",
            params![serde_json::to_string(&EgressLabel::local_only()).unwrap()],
        )
        .unwrap();
        let traces = list_labeled_traces_for_root(&conn, "root").unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].session_id, "root/coder");
        let msgs = list_labeled_messages_for_root(&conn, "root").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from_agent.as_deref(), Some("lead"));
    }
}
