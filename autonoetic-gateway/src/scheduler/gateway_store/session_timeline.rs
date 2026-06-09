//! Reader for the canonical Session Room timeline (#363 P1).
//!
//! `live_digest_events` is the merged spine; this reads it back cursor-paginated
//! and altitude-filtered, mapping rows to [`SessionTimelineEntry`]. Mirrors the
//! `list_operator_activity` reader. NULL attribution columns (rows written before
//! v46, or by producers not yet enriched) fall back to sensible defaults:
//! altitude→`Normal`, principal→`AutonoeticAgent` on `source_agent_id`, role
//! derived from the agent id.

use anyhow::Result;
use autonoetic_types::principal::{Principal, PrincipalKind};
use autonoetic_types::session_timeline::{
    Altitude, SessionRole, SessionTimelineEntry, SessionTimelineListResult, TimelineRefs,
};
use rusqlite::types::Value;

use super::GatewayStore;

const SELECT_COLS: &str = "event_id, root_session_id, source_session_id, turn_id,
    source_agent_id, event_type, payload, created_at,
    principal_kind, principal_id, role, altitude, refs_json";

/// NULL altitude is treated as `normal` (rank 1).
const ALTITUDE_RANK_SQL: &str = "CASE altitude
    WHEN 'detail' THEN 0
    WHEN 'normal' THEN 1
    WHEN 'attention' THEN 2
    WHEN 'error' THEN 3
    ELSE 1
END";

impl GatewayStore {
    /// Read the canonical timeline for a root session, oldest-first, after an
    /// optional cursor event, floored at `min_altitude`, optionally restricted to
    /// one principal. Returns one extra row internally to compute `has_more`.
    pub fn list_session_timeline(
        &self,
        root_session_id: &str,
        after_event_id: Option<&str>,
        limit: u32,
        min_altitude: Option<Altitude>,
        principal_id: Option<&str>,
    ) -> Result<SessionTimelineListResult> {
        let conn = self.conn.lock().unwrap();
        let fetch_limit = (limit as i64).saturating_add(1);

        let mut conditions = vec!["root_session_id = ?".to_string()];
        let mut p: Vec<Value> = vec![Value::Text(root_session_id.to_string())];

        conditions.push(format!("{ALTITUDE_RANK_SQL} >= ?"));
        p.push(Value::Integer(min_altitude_rank(min_altitude)));

        if let Some(pid) = principal_id {
            // Match the canonical principal_id, falling back to source_agent_id
            // for rows not yet carrying explicit principal columns.
            conditions.push("COALESCE(principal_id, source_agent_id) = ?".to_string());
            p.push(Value::Text(pid.to_string()));
        }

        if let Some(after_id) = resolve_cursor(&conn, root_session_id, after_event_id)? {
            conditions.push(
                "(created_at > (SELECT created_at FROM live_digest_events
                                WHERE event_id = ? AND root_session_id = ?)
                  OR (created_at = (SELECT created_at FROM live_digest_events
                                    WHERE event_id = ? AND root_session_id = ?)
                      AND event_id > ?))"
                    .to_string(),
            );
            p.push(Value::Text(after_id.clone()));
            p.push(Value::Text(root_session_id.to_string()));
            p.push(Value::Text(after_id.clone()));
            p.push(Value::Text(root_session_id.to_string()));
            p.push(Value::Text(after_id));
        }

        let sql = format!(
            "SELECT {SELECT_COLS} FROM live_digest_events
             WHERE {}
             ORDER BY created_at ASC, event_id ASC
             LIMIT ?",
            conditions.join(" AND ")
        );
        p.push(Value::Integer(fetch_limit));

        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<SessionTimelineEntry> = stmt
            .query_map(rusqlite::params_from_iter(p), map_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let has_more = rows.len() > limit as usize;
        let entries: Vec<SessionTimelineEntry> = rows.into_iter().take(limit as usize).collect();
        let next_cursor = if has_more {
            entries.last().map(|e| e.event_id.clone())
        } else {
            None
        };

        Ok(SessionTimelineListResult {
            entries,
            next_cursor,
            has_more,
        })
    }

    /// Copy a source session's timeline rows into a forked session's root id, up
    /// to and including the fork turn. A fork writes a checkpoint (LLM history)
    /// but no `live_digest_events` of its own, so without this the Session Room
    /// shows an empty timeline until the branch first runs. This mirrors the
    /// parent's narrative spine up to the branch point so the operator sees the
    /// inherited history immediately.
    ///
    /// Rows get fresh `event_id`s (the column is the primary key); their
    /// `created_at`, ordering, and attribution columns are preserved verbatim,
    /// so the copied timeline renders identically to the source. The cutoff is
    /// the end-of-fork-turn timestamp (the latest event carrying a turn id `<=`
    /// the fork turn), which also pulls in interleaved turn-less rows (operator
    /// messages, session start) that belong before the branch point. Returns the
    /// number of rows copied.
    pub fn clone_timeline_for_fork(
        &self,
        source_root_session_id: &str,
        new_root_session_id: &str,
        up_to_turn: u64,
    ) -> Result<usize> {
        use rusqlite::OptionalExtension;

        let conn = self.conn.lock().unwrap();
        // Turn ids are zero-padded to a fixed width (`turn-000003`), so a plain
        // lexicographic `<=` comparison is also a numeric one.
        let turn_ceiling = crate::runtime::checkpoint::turn_id_for(up_to_turn);
        let cutoff: Option<String> = conn
            .query_row(
                "SELECT MAX(created_at) FROM live_digest_events
                 WHERE root_session_id = ?1 AND turn_id IS NOT NULL AND turn_id <= ?2",
                rusqlite::params![source_root_session_id, &turn_ceiling],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let cutoff = match cutoff {
            Some(c) => c,
            None => return Ok(0), // no in-range turn events → nothing to mirror
        };

        let mut stmt = conn.prepare(
            "SELECT event_id, source_session_id, turn_id, source_agent_id, source_node_id,
                    event_type, payload, created_at, principal_kind, principal_id,
                    role, altitude, refs_json
             FROM live_digest_events
             WHERE root_session_id = ?1 AND created_at <= ?2
             ORDER BY created_at ASC, event_id ASC",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![source_root_session_id, &cutoff],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,         // original event_id
                        row.get::<_, String>(1)?,         // source_session_id
                        row.get::<_, Option<String>>(2)?, // turn_id
                        row.get::<_, Option<String>>(3)?, // source_agent_id
                        row.get::<_, String>(4)?,         // source_node_id
                        row.get::<_, String>(5)?,         // event_type
                        row.get::<_, Option<String>>(6)?, // payload
                        row.get::<_, String>(7)?,         // created_at
                        row.get::<_, Option<String>>(8)?, // principal_kind
                        row.get::<_, Option<String>>(9)?, // principal_id
                        row.get::<_, Option<String>>(10)?, // role
                        row.get::<_, Option<String>>(11)?, // altitude
                        row.get::<_, Option<String>>(12)?, // refs_json
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for (i, r) in rows.iter().enumerate() {
            // Fresh, collision-free event id scoped to the forked session.
            let new_event_id = format!("{new_root_session_id}-evt-{i:06}");
            conn.execute(
                "INSERT INTO live_digest_events (
                    event_id, root_session_id, source_session_id, turn_id, source_agent_id,
                    source_node_id, event_type, payload, created_at,
                    principal_kind, principal_id, role, altitude, refs_json
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                rusqlite::params![
                    new_event_id,
                    new_root_session_id,
                    r.1,
                    r.2,
                    r.3,
                    r.4,
                    r.5,
                    r.6,
                    r.7,
                    r.8,
                    r.9,
                    r.10,
                    r.11,
                    r.12,
                ],
            )?;
        }
        Ok(rows.len())
    }
}

fn min_altitude_rank(min: Option<Altitude>) -> i64 {
    match min {
        None | Some(Altitude::Detail) => 0,
        Some(Altitude::Normal) => 1,
        Some(Altitude::Attention) => 2,
        Some(Altitude::Error) => 3,
    }
}

/// An unknown cursor falls back to the first page (no error), matching the
/// operator-activity reader.
fn resolve_cursor(
    conn: &rusqlite::Connection,
    root_session_id: &str,
    after_event_id: Option<&str>,
) -> Result<Option<String>> {
    let Some(after_id) = after_event_id else {
        return Ok(None);
    };
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM live_digest_events
         WHERE event_id = ?1 AND root_session_id = ?2",
        rusqlite::params![after_id, root_session_id],
        |row| row.get(0),
    )?;
    Ok((exists > 0).then(|| after_id.to_string()))
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionTimelineEntry> {
    let source_agent_id: Option<String> = row.get(4)?;
    let principal_kind: Option<String> = row.get(8)?;
    let principal_id: Option<String> = row.get(9)?;
    let role_str: Option<String> = row.get(10)?;
    let altitude_str: Option<String> = row.get(11)?;
    let refs_json: Option<String> = row.get(12)?;

    let principal = Principal {
        kind: principal_kind
            .as_deref()
            .map(Principal::kind_from_storage)
            .unwrap_or(PrincipalKind::AutonoeticAgent),
        id: principal_id
            .or_else(|| source_agent_id.clone())
            .unwrap_or_default(),
    };

    let role = match role_str {
        Some(s) => SessionRole::from_storage(&s),
        None => crate::runtime::session_timeline::derive_role(
            source_agent_id.as_deref().unwrap_or(""),
        ),
    };

    let event_type: String = row.get(5)?;

    // Pre-v46 rows (or producers not yet writing altitude) have NULL here.
    // Recompute deterministically from (event_type, role) rather than defaulting
    // to Normal, so historical failures/gates still surface and aren't dropped by
    // a `min_altitude` filter.
    let altitude = altitude_str
        .as_deref()
        .and_then(Altitude::parse_str)
        .unwrap_or_else(|| crate::runtime::session_timeline::altitude_for(&event_type, &role));

    let refs: TimelineRefs = refs_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(12, rusqlite::types::Type::Text, Box::new(e))
        })?
        .unwrap_or_default();

    Ok(SessionTimelineEntry {
        event_id: row.get(0)?,
        root_session_id: row.get(1)?,
        source_session_id: row.get(2)?,
        turn_id: row.get(3)?,
        principal,
        role,
        event_type,
        altitude,
        occurred_at: row.get(7)?,
        payload: row.get(6)?,
        refs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::gateway_store::LiveDigestEventRecord;
    use tempfile::tempdir;

    fn record(root: &str, agent: &str, event_type: &str, at: &str) -> LiveDigestEventRecord {
        let role = crate::runtime::session_timeline::derive_role(agent);
        let altitude = crate::runtime::session_timeline::altitude_for(event_type, &role);
        let principal = Principal::agent(agent.to_string());
        LiveDigestEventRecord {
            event_id: format!("ev-{}", uuid::Uuid::new_v4()),
            root_session_id: root.to_string(),
            source_session_id: root.to_string(),
            turn_id: Some("turn-1".to_string()),
            source_agent_id: Some(agent.to_string()),
            source_node_id: "gateway".to_string(),
            event_type: event_type.to_string(),
            payload: None,
            created_at: at.to_string(),
            principal_kind: Some(principal.kind_to_storage()),
            principal_id: Some(principal.id.clone()),
            role: Some(role.to_storage()),
            altitude: Some(altitude.as_str().to_string()),
            refs_json: None,
        }
    }

    #[test]
    fn lists_oldest_first_with_cursor_paging() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "root-tl";

        store
            .create_live_digest_event(&record(root, "planner.default", "session.start", "2026-06-01T10:00:00+00:00"))
            .unwrap();
        store
            .create_live_digest_event(&record(root, "coder.default", "tool.completed", "2026-06-01T10:00:01+00:00"))
            .unwrap();

        let page1 = store
            .list_session_timeline(root, None, 1, None, None)
            .unwrap();
        assert_eq!(page1.entries.len(), 1);
        assert!(page1.has_more);
        assert_eq!(page1.entries[0].event_type, "session.start");

        let page2 = store
            .list_session_timeline(root, page1.next_cursor.as_deref(), 10, None, None)
            .unwrap();
        assert_eq!(page2.entries.len(), 1);
        assert_eq!(page2.entries[0].event_type, "tool.completed");
        assert!(!page2.has_more);
    }

    #[test]
    fn min_altitude_filters_below_floor() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "root-alt";

        // planner turn.start = Detail; a failure = Error.
        store
            .create_live_digest_event(&record(root, "planner.default", "turn.start", "2026-06-01T10:00:00+00:00"))
            .unwrap();
        store
            .create_live_digest_event(&record(root, "coder.default", "llm.request_failed", "2026-06-01T10:00:01+00:00"))
            .unwrap();

        let attention = store
            .list_session_timeline(root, None, 50, Some(Altitude::Attention), None)
            .unwrap();
        assert_eq!(attention.entries.len(), 1);
        assert_eq!(attention.entries[0].altitude, Altitude::Error);
    }

    #[test]
    fn sentinel_floor_surfaces_mild_event_at_attention() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "root-sentinel";

        // Same mild event type for both seats; only the Sentinel's is raised.
        store
            .create_live_digest_event(&record(root, "planner.default", "turn.start", "2026-06-01T10:00:00+00:00"))
            .unwrap();
        store
            .create_live_digest_event(&record(root, "sentinel.divergence", "turn.start", "2026-06-01T10:00:01+00:00"))
            .unwrap();

        let attention = store
            .list_session_timeline(root, None, 50, Some(Altitude::Attention), None)
            .unwrap();
        assert_eq!(attention.entries.len(), 1);
        assert_eq!(attention.entries[0].role, SessionRole::Sentinel);
        assert_eq!(attention.entries[0].principal.id, "sentinel.divergence");
    }

    #[test]
    fn null_altitude_is_recomputed_on_read() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "root-null-alt";

        // A row written without attribution columns (pre-v46 shape / unenriched).
        let mut row = record(root, "coder.default", "llm.request_failed", "2026-06-01T10:00:00+00:00");
        row.altitude = None;
        row.role = None;
        row.principal_kind = None;
        store.create_live_digest_event(&row).unwrap();

        let listed = store.list_session_timeline(root, None, 10, None, None).unwrap();
        assert_eq!(listed.entries.len(), 1);
        // Recomputed from (event_type, role) rather than defaulting to Normal.
        assert_eq!(listed.entries[0].altitude, Altitude::Error);
        // Principal/role fall back from source_agent_id.
        assert_eq!(listed.entries[0].principal.id, "coder.default");
    }

    #[test]
    fn principal_filter_restricts_to_one_actor() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "root-princ";

        store
            .create_live_digest_event(&record(root, "planner.default", "tool.completed", "2026-06-01T10:00:00+00:00"))
            .unwrap();
        store
            .create_live_digest_event(&record(root, "coder.default", "tool.completed", "2026-06-01T10:00:01+00:00"))
            .unwrap();

        let only_coder = store
            .list_session_timeline(root, None, 50, None, Some("coder.default"))
            .unwrap();
        assert_eq!(only_coder.entries.len(), 1);
        assert_eq!(only_coder.entries[0].principal.id, "coder.default");
    }
}
