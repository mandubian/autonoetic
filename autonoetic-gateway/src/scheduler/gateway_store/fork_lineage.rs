//! Session fork lineage (#814): `session_fork_lineage` records that a forked
//! session branched from a source session, enriched with the turn it branched
//! from, the branch message's digest, and the acting agent.
//!
//! Beyond the raw table, this module is the single choke point
//! (`record_session_fork`) for every side effect a session fork must perform —
//! timeline mirroring, the lineage row, and the two `session.forked` /
//! `session.fork_created` causal events — so `session.fork` (RPC) and
//! `trace fork` (CLI) can't drift from each other again.

use super::GatewayStore;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;

/// A row of `session_fork_lineage`. The enrichment columns (`fork_turn`,
/// `branch_message_sha256`, `agent_id`) are `None` for rows written before
/// #814 (migration v70) or via the causal-event backfill, which doesn't have
/// this information available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkLineageRecord {
    pub forked_session_id: String,
    pub source_session_id: String,
    pub fork_turn: Option<u64>,
    pub branch_message_sha256: Option<String>,
    pub agent_id: Option<String>,
    pub created_at: String,
}

fn fork_lineage_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ForkLineageRecord> {
    Ok(ForkLineageRecord {
        forked_session_id: row.get(0)?,
        source_session_id: row.get(1)?,
        fork_turn: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
        branch_message_sha256: row.get(3)?,
        agent_id: row.get(4)?,
        created_at: row.get(5)?,
    })
}

const FORK_LINEAGE_COLUMNS: &str =
    "forked_session_id, source_session_id, fork_turn, branch_message_sha256, agent_id, created_at";

impl GatewayStore {
    // --- Fork lineage ---

    /// Record that `forked_session_id` was forked from `source_session_id` at
    /// `fork_turn`, optionally with a branch message (stored as a SHA-256 hex
    /// digest, not the raw text) and the agent id that performed the fork.
    /// Enables artifact-ref resolution across fork boundaries: a fork inherits
    /// its parent's artifact refs even though it has a different root session id.
    pub fn record_fork_lineage(
        &self,
        forked_session_id: &str,
        source_session_id: &str,
        fork_turn: u64,
        branch_message_sha256: Option<&str>,
        agent_id: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO session_fork_lineage
                (forked_session_id, source_session_id, created_at, fork_turn, branch_message_sha256, agent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                forked_session_id,
                source_session_id,
                chrono::Utc::now().to_rfc3339(),
                fork_turn as i64,
                branch_message_sha256,
                agent_id,
            ],
        )?;
        Ok(())
    }

    /// Look up the immediate source session for a forked session.
    /// Returns `None` if the session was not forked.
    pub fn get_fork_source(&self, forked_session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let source = conn
            .query_row(
                "SELECT source_session_id FROM session_fork_lineage
                 WHERE forked_session_id = ?1",
                params![forked_session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(source)
    }

    /// Full lineage row for a forked session (enrichment columns included),
    /// or `None` if the session was not forked.
    pub fn get_fork_lineage(&self, forked_session_id: &str) -> Result<Option<ForkLineageRecord>> {
        let conn = self.conn.lock().unwrap();
        let record = conn
            .query_row(
                &format!(
                    "SELECT {FORK_LINEAGE_COLUMNS} FROM session_fork_lineage
                     WHERE forked_session_id = ?1"
                ),
                params![forked_session_id],
                fork_lineage_record_from_row,
            )
            .optional()?;
        Ok(record)
    }

    /// All sessions forked directly FROM `source_session_id`, oldest first.
    pub fn list_fork_children(&self, source_session_id: &str) -> Result<Vec<ForkLineageRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {FORK_LINEAGE_COLUMNS} FROM session_fork_lineage
             WHERE source_session_id = ?1
             ORDER BY created_at ASC"
        ))?;
        let rows = stmt.query_map(params![source_session_id], fork_lineage_record_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Backfill `session_fork_lineage` from existing `session.forked` causal
    /// events. Used by migration v54 to repair forks created before the
    /// lineage table existed. Returns the number of rows inserted.
    ///
    /// The enrichment columns (`fork_turn`, `branch_message_sha256`,
    /// `agent_id`) added by v70 are left `NULL`: that information isn't
    /// reliably recoverable from the causal event alone (payload shape
    /// varies across gateway versions).
    pub fn backfill_fork_lineage_from_causal_events(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "INSERT OR IGNORE INTO session_fork_lineage (forked_session_id, source_session_id, created_at)
             SELECT
                 ce.session_id,
                 json_extract(ce.payload, '$.source_session_id'),
                 ce.timestamp
             FROM causal_events ce
             WHERE ce.action = 'session.forked'
               AND ce.session_id IS NOT NULL
               AND json_extract(ce.payload, '$.source_session_id') IS NOT NULL",
            [],
        )?;
        Ok(n)
    }

    /// Walk the fork chain starting from `session_id`'s root, yielding each
    /// ancestor source session's root. Stops at cycles or depth 16.
    pub(super) fn fork_ancestor_roots(&self, conn: &Connection, session_id: &str) -> Vec<String> {
        let mut ancestors = Vec::new();
        let mut visited = HashSet::new();
        // Start from the ROOT of the session — fork lineage is recorded under
        // the fork's root id, so a child ("fork-abc/T5") must look up its
        // root ("fork-abc") to find the lineage entry.
        let mut cursor = crate::runtime::content_store::root_session_id(session_id).to_string();
        for _ in 0..16 {
            let Ok(source) = conn
                .query_row(
                    "SELECT source_session_id FROM session_fork_lineage
                     WHERE forked_session_id = ?1",
                    params![&cursor],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            else {
                break;
            };
            let Some(source) = source else { break };
            let source_root = crate::runtime::content_store::root_session_id(&source).to_string();
            if !visited.insert(source_root.clone()) {
                break; // cycle guard
            }
            ancestors.push(source_root.clone());
            // Advance by the ROOT, not the raw source: the table is keyed by
            // root ids, so a legacy row whose source was recorded as a nested
            // id ("root/T5") would otherwise dead-end the walk one hop early.
            cursor = source_root;
        }
        ancestors
    }

    /// Single choke point for every side effect a session fork must perform,
    /// shared by `session.fork` (RPC) and `trace fork` (CLI) so they can't
    /// drift from each other (before #814, the CLI path recorded no lineage
    /// row and no causal event at all).
    ///
    /// Steps, in order:
    /// 1. Best-effort timeline mirror (`clone_timeline_for_fork`) — a missing
    ///    or stale timeline is a cosmetic UI gap, not a correctness issue, so
    ///    failure is logged and treated as zero mirrored events.
    /// 2. The lineage row. This one is NOT best-effort: it's the row that
    ///    artifact-ref resolution across fork boundaries depends on
    ///    (`fork_ancestor_roots`), so a failure here is propagated as `Err`
    ///    instead of swallowed — callers decide how to react, but they can no
    ///    longer stay silently unaware of it.
    /// 3. The `session.forked` causal event on the NEW session (unchanged
    ///    shape from the pre-#814 router implementation, so existing
    ///    backfills/consumers keep working).
    /// 4. A new `session.fork_created` causal event on the SOURCE session, so
    ///    the source's own causal chain records that it was forked from.
    ///
    /// Causal-event write failures (steps 3 and 4) are logged and swallowed —
    /// they're an observability nicety, not load-bearing state.
    ///
    /// Returns the number of timeline events mirrored (step 1).
    pub fn record_session_fork(
        &self,
        fork: &crate::runtime::checkpoint::SessionFork,
        branch_message: Option<&str>,
        agent_id: &str,
    ) -> Result<usize> {
        // Lineage rows, causal events, and children queries are all keyed by
        // ROOT session ids (that's what `fork_ancestor_roots` and
        // `list_fork_children` walk), so normalize a nested source
        // ("root/T5") to its root here. The exact source id is preserved in
        // the causal payloads as `source_session_id_exact` when it differs.
        let source_root =
            crate::runtime::content_store::root_session_id(&fork.source_session_id).to_string();
        let source_exact = (fork.source_session_id != source_root)
            .then_some(fork.source_session_id.as_str());

        let mirrored_events = match self.clone_timeline_for_fork(
            &fork.source_session_id,
            &fork.new_session_id,
            fork.fork_turn as u64,
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    target: "session.fork",
                    source = %fork.source_session_id,
                    new = %fork.new_session_id,
                    error = %e,
                    "Failed to mirror source timeline into fork"
                );
                0
            }
        };

        let branch_message_sha256 = branch_message.map(|m| {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(m.as_bytes());
            format!("{:x}", hasher.finalize())
        });

        // Load-bearing: artifact-ref resolution across fork boundaries walks
        // this table, so a failure here must not be silently swallowed.
        self.record_fork_lineage(
            &fork.new_session_id,
            &source_root,
            fork.fork_turn as u64,
            branch_message_sha256.as_deref(),
            agent_id,
        )?;

        let forked_payload = serde_json::json!({
            "source_session_id": source_root,
            "source_session_id_exact": source_exact,
            "fork_turn": fork.fork_turn,
            "history_handle": fork.history_handle,
            "branch_message_sha256": branch_message_sha256,
        });
        let forked_event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session_id: fork.new_session_id.clone(),
            turn_id: Some("turn-000001".to_string()),
            event_seq: 1,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "session".to_string(),
            action: "session.forked".to_string(),
            status: "success".to_string(),
            enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
            target: None,
            payload: Some(forked_payload.to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        };
        if let Err(e) = self.create_causal_event(&forked_event) {
            tracing::warn!(
                target: "session.fork",
                error = %e,
                "Failed to write session.forked causal event"
            );
        }

        let fork_created_payload = serde_json::json!({
            "forked_session_id": fork.new_session_id,
            "source_session_id_exact": source_exact,
            "fork_turn": fork.fork_turn,
            "branch_message_sha256": branch_message_sha256,
        });
        // Written under the source's ROOT so the event is visible when
        // querying the root session's chain, even for forks taken from a
        // nested child session.
        let fork_created_event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session_id: source_root.clone(),
            turn_id: None,
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "session".to_string(),
            action: "session.fork_created".to_string(),
            status: "success".to_string(),
            enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
            target: None,
            payload: Some(fork_created_payload.to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        };
        if let Err(e) = self.create_causal_event(&fork_created_event) {
            tracing::warn!(
                target: "session.fork",
                error = %e,
                "Failed to write session.fork_created causal event"
            );
        }

        Ok(mirrored_events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Message;
    use crate::runtime::checkpoint::{save_checkpoint, SessionCheckpoint, SessionFork, YieldReason};
    use crate::runtime::guard::LoopGuard;
    use autonoetic_types::config::GatewayConfig;
    use tempfile::tempdir;

    fn test_config(temp: &tempfile::TempDir) -> GatewayConfig {
        GatewayConfig {
            agents_dir: temp.path().to_path_buf(),
            ..Default::default()
        }
    }

    fn test_checkpoint(
        session_id: &str,
        turn_id: &str,
        history: Vec<Message>,
        turn_counter: u64,
    ) -> SessionCheckpoint {
        SessionCheckpoint {
            history,
            turn_counter,
            session_state: Default::default(),
            tool_tier_escalated: false,
            discovered_tools: Default::default(),
            blocked_state_event_emitted: false,
            loop_guard_state: LoopGuard {
                max_loops_without_progress: 10,
                max_tool_failures: 5,
                max_consecutive_same_progress: 2,
                max_child_failures: 3,
                current_loops: 0,
                tool_failure_counts: std::collections::HashMap::new(),
                last_progress_fingerprint: None,
                consecutive_progress_count: 0,
                child_failure_count: 0,
                ..Default::default()
            },
            agent_id: "test-agent".to_string(),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::Hibernation,
            content_store_refs: vec![],
            created_at: "2024-01-01T00:00:00Z".to_string(),
            pending_tool_state: None,
            llm_rounds_consumed: 1,
            tool_invocations_consumed: 0,
            tokens_consumed: 100,
            estimated_cost_usd: 0.001,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        }
    }

    /// #814 (a): `record_session_fork` writes the enriched lineage row, the
    /// `session.forked` event on the child, and `session.fork_created` on the
    /// source — with the expected payload fields.
    #[test]
    fn record_session_fork_writes_lineage_and_both_causal_events() -> Result<()> {
        let temp = tempdir()?;
        let config = test_config(&temp);
        let gw_dir = temp.path().join(".gateway");
        let store = GatewayStore::open(&gw_dir)?;

        let cp = test_checkpoint(
            "parent-session",
            "turn-0001",
            vec![Message::user("Start")],
            1,
        );
        save_checkpoint(&config, &cp)?;

        let fork = SessionFork::fork(
            &config,
            "parent-session",
            Some("child-session"),
            Some("Branch point"),
        )?;

        let mirrored = store.record_session_fork(&fork, Some("Branch point"), "coder.default")?;
        assert_eq!(mirrored, 0, "no live_digest_events exist to mirror in this test");

        // Lineage row.
        let lineage = store
            .get_fork_lineage("child-session")?
            .expect("lineage row must exist");
        assert_eq!(lineage.source_session_id, "parent-session");
        assert_eq!(lineage.fork_turn, Some(1));
        assert_eq!(lineage.agent_id.as_deref(), Some("coder.default"));
        let expected_hash = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(b"Branch point"))
        };
        assert_eq!(lineage.branch_message_sha256.as_deref(), Some(expected_hash.as_str()));

        // session.forked on the child.
        let child_events = store.search_causal_events(Some("child-session"), None, 10)?;
        assert_eq!(child_events.len(), 1);
        let event = &child_events[0];
        assert_eq!(event.action, "session.forked");
        assert_eq!(event.session_id, "child-session");
        assert_eq!(event.turn_id.as_deref(), Some("turn-000001"));
        assert_eq!(event.event_seq, 1);
        let payload: serde_json::Value =
            serde_json::from_str(event.payload.as_ref().unwrap())?;
        assert_eq!(payload["source_session_id"], "parent-session");
        assert_eq!(payload["fork_turn"], 1);
        assert_eq!(payload["branch_message_sha256"], expected_hash);

        // session.fork_created on the source.
        let source_events = store.search_causal_events(Some("parent-session"), None, 10)?;
        assert_eq!(source_events.len(), 1);
        let event = &source_events[0];
        assert_eq!(event.action, "session.fork_created");
        assert_eq!(event.session_id, "parent-session");
        assert!(event.turn_id.is_none());
        assert_eq!(event.event_seq, 0);
        let payload: serde_json::Value =
            serde_json::from_str(event.payload.as_ref().unwrap())?;
        assert_eq!(payload["forked_session_id"], "child-session");
        assert_eq!(payload["fork_turn"], 1);
        assert_eq!(payload["branch_message_sha256"], expected_hash);

        Ok(())
    }

    /// #814 (b): multi-level fork A -> B -> C. Ancestor walk from C finds A;
    /// `list_fork_children(A)` contains B.
    #[test]
    fn multi_level_fork_ancestor_and_children_walk() -> Result<()> {
        let temp = tempdir()?;
        let config = test_config(&temp);
        let gw_dir = temp.path().join(".gateway");
        let store = GatewayStore::open(&gw_dir)?;

        let cp_a = test_checkpoint("session-a", "turn-0001", vec![Message::user("A")], 1);
        save_checkpoint(&config, &cp_a)?;
        let fork_b = SessionFork::fork(&config, "session-a", Some("session-b"), None)?;
        store.record_session_fork(&fork_b, None, "planner.default")?;

        let cp_b = test_checkpoint(
            "session-b",
            "turn-0002",
            fork_b.initial_history.clone(),
            2,
        );
        save_checkpoint(&config, &cp_b)?;
        let fork_c = SessionFork::fork(&config, "session-b", Some("session-c"), None)?;
        store.record_session_fork(&fork_c, None, "planner.default")?;

        // Ancestor walk: C -> B -> A.
        let ancestors = {
            let conn = store.conn.lock().unwrap();
            store.fork_ancestor_roots(&conn, "session-c")
        };
        assert_eq!(ancestors, vec!["session-b".to_string(), "session-a".to_string()]);

        // list_fork_children(A) contains B.
        let children_of_a = store.list_fork_children("session-a")?;
        assert_eq!(children_of_a.len(), 1);
        assert_eq!(children_of_a[0].forked_session_id, "session-b");

        let children_of_b = store.list_fork_children("session-b")?;
        assert_eq!(children_of_b.len(), 1);
        assert_eq!(children_of_b[0].forked_session_id, "session-c");

        Ok(())
    }

    /// #814 review (PR #826): a fork taken from a NESTED source session id is
    /// normalized to the root for the lineage row and the source-side causal
    /// event — keeping `list_fork_children`, `fork_ancestor_roots`, and
    /// `trace fork-tree` root-keyed — while the exact source id is preserved
    /// in the payload. The attribution fallback is the checkpoint's agent.
    #[test]
    fn record_session_fork_normalizes_nested_source_to_root() -> Result<()> {
        let temp = tempdir()?;
        let config = test_config(&temp);
        let store = GatewayStore::open(&temp.path().join(".gateway"))?;

        let cp = test_checkpoint("root-x/child-y", "turn-0003", vec![Message::user("hi")], 3);
        save_checkpoint(&config, &cp)?;
        let fork = SessionFork::fork(&config, "root-x/child-y", Some("fork-nested"), None)?;
        assert_eq!(fork.agent_id, "test-agent", "fork carries the checkpoint's agent");
        store.record_session_fork(&fork, None, "coder.default")?;

        // Lineage row keyed by the ROOT source.
        let lineage = store.get_fork_lineage("fork-nested")?.expect("lineage row");
        assert_eq!(lineage.source_session_id, "root-x");
        let children = store.list_fork_children("root-x")?;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].forked_session_id, "fork-nested");

        // fork_created lands on the ROOT chain; exact source kept in payload.
        let root_events = store.search_causal_events(Some("root-x"), None, 10)?;
        assert_eq!(root_events.len(), 1);
        assert_eq!(root_events[0].action, "session.fork_created");
        let payload: serde_json::Value =
            serde_json::from_str(root_events[0].payload.as_ref().unwrap())?;
        assert_eq!(payload["source_session_id_exact"], "root-x/child-y");

        // Child event's source_session_id is the root too (v54-backfill shape).
        let child_events = store.search_causal_events(Some("fork-nested"), None, 10)?;
        let payload: serde_json::Value =
            serde_json::from_str(child_events[0].payload.as_ref().unwrap())?;
        assert_eq!(payload["source_session_id"], "root-x");
        assert_eq!(payload["source_session_id_exact"], "root-x/child-y");

        Ok(())
    }

    /// #814 (c): a legacy-shape row (written before v70, enrichment columns
    /// NULL) is read back correctly as `None`s.
    #[test]
    fn get_fork_lineage_reads_legacy_row_as_none() -> Result<()> {
        let temp = tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO session_fork_lineage (forked_session_id, source_session_id, created_at)
                 VALUES (?1, ?2, ?3)",
                params!["legacy-fork", "legacy-source", "2020-01-01T00:00:00Z"],
            )?;
        }

        let record = store
            .get_fork_lineage("legacy-fork")?
            .expect("legacy row should still be readable");
        assert_eq!(record.source_session_id, "legacy-source");
        assert!(record.fork_turn.is_none());
        assert!(record.branch_message_sha256.is_none());
        assert!(record.agent_id.is_none());

        Ok(())
    }
}
