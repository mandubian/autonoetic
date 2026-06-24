use anyhow::Result;
use autonoetic_types::operator_activity::{
    OperatorActivityKind, OperatorActivityListResult, OperatorActivityRecord,
    OperatorActivityRefs, OperatorActivitySeverity,
};
use rusqlite::params;

use super::GatewayStore;

const SELECT_COLS: &str = "activity_id, root_session_id, session_id, agent_id,
    workflow_id, task_id, turn_id, occurred_at,
    kind, severity, summary, tool_name,
    causal_event_id, workflow_event_id, refs_json";

const SEVERITY_RANK_SQL: &str = "CASE severity
    WHEN 'info' THEN 0
    WHEN 'progress' THEN 1
    WHEN 'attention' THEN 2
    WHEN 'error' THEN 3
    ELSE 0
END";

/// Outcome of a rate-limited operator-activity insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorActivityInsert {
    /// The record was persisted.
    Inserted,
    /// The record was dropped, but a single `rate_limited` notice was emitted
    /// for this window so the suppression is visible.
    ThrottleNoticeEmitted,
    /// The record was dropped; a notice for this window already existed.
    Dropped,
}

impl GatewayStore {
    pub fn insert_operator_activity(&self, record: &OperatorActivityRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        write_operator_activity(&conn, record)
    }

    /// Insert an operator activity subject to a per-root rolling-window rate
    /// limit. At most `rate_limit_per_min` real rows are persisted per root
    /// session per 60 seconds; once the cap is reached a single
    /// `rate_limited` notice is emitted and further rows in the window are
    /// dropped. `rate_limit_per_min == 0` disables the limit. The count and
    /// notice check run under the same connection lock as the insert, so the
    /// cap holds even with concurrent emitters.
    pub fn insert_operator_activity_throttled(
        &self,
        record: &OperatorActivityRecord,
        rate_limit_per_min: u32,
    ) -> Result<OperatorActivityInsert> {
        if rate_limit_per_min == 0 {
            self.insert_operator_activity(record)?;
            return Ok(OperatorActivityInsert::Inserted);
        }

        let window_start = window_start_rfc3339(&record.occurred_at)?;
        let conn = self.conn.lock().unwrap();

        // Notices don't consume budget — only real activity rows count.
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM operator_activity
             WHERE root_session_id = ?1
               AND occurred_at >= ?2 AND occurred_at <= ?3
               AND kind != 'rate_limited'",
            params![record.root_session_id, window_start, record.occurred_at],
            |row| row.get(0),
        )?;

        if (count as u64) < rate_limit_per_min as u64 {
            write_operator_activity(&conn, record)?;
            return Ok(OperatorActivityInsert::Inserted);
        }

        let notice_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM operator_activity
             WHERE root_session_id = ?1
               AND occurred_at >= ?2 AND occurred_at <= ?3
               AND kind = 'rate_limited'",
            params![record.root_session_id, window_start, record.occurred_at],
            |row| row.get(0),
        )?;

        if notice_count == 0 {
            let notice = rate_limit_notice(record, rate_limit_per_min);
            write_operator_activity(&conn, &notice)?;
            return Ok(OperatorActivityInsert::ThrottleNoticeEmitted);
        }

        Ok(OperatorActivityInsert::Dropped)
    }

    pub fn list_operator_activity(
        &self,
        root_session_id: &str,
        after_activity_id: Option<&str>,
        limit: u32,
        min_severity: Option<OperatorActivitySeverity>,
    ) -> Result<OperatorActivityListResult> {
        let conn = self.conn.lock().unwrap();
        let fetch_limit = (limit as i64).saturating_add(1);
        let min_rank = severity_rank(min_severity);
        let effective_after = resolve_effective_cursor(&conn, root_session_id, after_activity_id)?;

        let rows: Vec<OperatorActivityRecord> = if let Some(after_id) = effective_after {
            let sql = format!(
                "SELECT {SELECT_COLS}
                 FROM operator_activity
                 WHERE root_session_id = ?1
                   AND {SEVERITY_RANK_SQL} >= ?2
                   AND (
                     occurred_at > (
                       SELECT occurred_at FROM operator_activity
                       WHERE activity_id = ?3 AND root_session_id = ?1
                     )
                     OR (
                       occurred_at = (
                         SELECT occurred_at FROM operator_activity
                         WHERE activity_id = ?3 AND root_session_id = ?1
                       )
                       AND activity_id > ?3
                     )
                   )
                 ORDER BY occurred_at ASC, activity_id ASC
                 LIMIT ?4"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mapped = stmt.query_map(
                params![root_session_id, min_rank, after_id, fetch_limit],
                map_row,
            )?;
            mapped.collect::<Result<Vec<_>, _>>()?
        } else {
            let sql = format!(
                "SELECT {SELECT_COLS}
                 FROM operator_activity
                 WHERE root_session_id = ?1
                   AND {SEVERITY_RANK_SQL} >= ?2
                 ORDER BY occurred_at ASC, activity_id ASC
                 LIMIT ?3"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mapped = stmt.query_map(params![root_session_id, min_rank, fetch_limit], map_row)?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };

        let has_more = rows.len() > limit as usize;
        let activities: Vec<OperatorActivityRecord> = rows
            .into_iter()
            .take(limit as usize)
            .collect();
        let next_cursor = if has_more {
            activities.last().map(|r| r.activity_id.clone())
        } else {
            None
        };

        Ok(OperatorActivityListResult {
            activities,
            next_cursor,
            has_more,
        })
    }

    pub fn prune_operator_activity(&self, retention_days: i64) -> Result<usize> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let count = conn.execute(
            "DELETE FROM operator_activity WHERE occurred_at < ?1",
            params![cutoff],
        )?;
        Ok(count)
    }

    pub fn count_execution_traces_for_session(&self, session_id: &str) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM execution_traces WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
    }
}

fn write_operator_activity(
    conn: &rusqlite::Connection,
    record: &OperatorActivityRecord,
) -> Result<()> {
    let refs_json = if record.refs == OperatorActivityRefs::default() {
        None
    } else {
        Some(serde_json::to_string(&record.refs)?)
    };

    conn.execute(
        "INSERT OR IGNORE INTO operator_activity (
            activity_id, root_session_id, session_id, agent_id,
            workflow_id, task_id, turn_id, occurred_at,
            kind, severity, summary, tool_name,
            causal_event_id, workflow_event_id, refs_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            record.activity_id,
            record.root_session_id,
            record.session_id,
            record.agent_id,
            record.workflow_id,
            record.task_id,
            record.turn_id,
            record.occurred_at,
            record.kind.as_str(),
            record.severity.as_str(),
            record.summary,
            record.tool_name,
            record.causal_event_id,
            record.workflow_event_id,
            refs_json,
        ],
    )?;
    Ok(())
}

/// Start of the 60-second rate-limit window ending at `occurred_at`, as an
/// RFC3339 string comparable (lexicographically) with stored `occurred_at`.
fn window_start_rfc3339(occurred_at: &str) -> Result<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(occurred_at)
        .map_err(|e| anyhow::anyhow!("invalid occurred_at `{occurred_at}`: {e}"))?;
    Ok((parsed - chrono::Duration::seconds(60)).to_rfc3339())
}

/// Build the synthetic `rate_limited` notice that stands in for dropped rows,
/// inheriting the source record's session/agent context.
fn rate_limit_notice(src: &OperatorActivityRecord, cap: u32) -> OperatorActivityRecord {
    OperatorActivityRecord {
        activity_id: format!("oa-{}", uuid::Uuid::new_v4()),
        root_session_id: src.root_session_id.clone(),
        session_id: src.session_id.clone(),
        agent_id: src.agent_id.clone(),
        workflow_id: src.workflow_id.clone(),
        task_id: src.task_id.clone(),
        turn_id: src.turn_id.clone(),
        occurred_at: src.occurred_at.clone(),
        kind: OperatorActivityKind::RateLimited,
        severity: OperatorActivitySeverity::Attention,
        summary: format!(
            "activity rate limit reached ({cap}/min) — further updates this minute are suppressed"
        ),
        tool_name: None,
        causal_event_id: None,
        workflow_event_id: None,
        refs: OperatorActivityRefs::default(),
    }
}

fn severity_rank(min: Option<OperatorActivitySeverity>) -> i64 {
    match min {
        None => 0,
        Some(OperatorActivitySeverity::Info) => 0,
        Some(OperatorActivitySeverity::Progress) => 1,
        Some(OperatorActivitySeverity::Attention) => 2,
        Some(OperatorActivitySeverity::Error) => 3,
    }
}

fn resolve_effective_cursor(
    conn: &rusqlite::Connection,
    root_session_id: &str,
    after_activity_id: Option<&str>,
) -> Result<Option<String>> {
    let Some(after_id) = after_activity_id else {
        return Ok(None);
    };
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM operator_activity
         WHERE activity_id = ?1 AND root_session_id = ?2",
        params![after_id, root_session_id],
        |row| row.get(0),
    )?;
    if exists > 0 {
        Ok(Some(after_id.to_string()))
    } else {
        Ok(None)
    }
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperatorActivityRecord> {
    let kind_str: String = row.get(8)?;
    let severity_str: String = row.get(9)?;
    let refs_json: Option<String> = row.get(14)?;

    let kind = match kind_str.as_str() {
        "tool_completed" => OperatorActivityKind::ToolCompleted,
        "tool_failed" => OperatorActivityKind::ToolFailed,
        "delegation" => OperatorActivityKind::Delegation,
        "workflow_join" => OperatorActivityKind::WorkflowJoin,
        "approval_required" => OperatorActivityKind::ApprovalRequired,
        "plan_proposal" => OperatorActivityKind::PlanProposal,
        "human_gate" => OperatorActivityKind::HumanGate,
        "session_lifecycle" => OperatorActivityKind::SessionLifecycle,
        "rate_limited" => OperatorActivityKind::RateLimited,
        "sentinel_notice" => OperatorActivityKind::SentinelNotice,
        other => {
            return Err(rusqlite::Error::InvalidColumnType(
                8,
                other.to_string(),
                rusqlite::types::Type::Text,
            ))
        }
    };

    let severity = OperatorActivitySeverity::parse_str(&severity_str).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(9, severity_str, rusqlite::types::Type::Text)
    })?;

    let refs = refs_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(14, rusqlite::types::Type::Text, Box::new(e))
        })?
        .unwrap_or_default();

    Ok(OperatorActivityRecord {
        activity_id: row.get(0)?,
        root_session_id: row.get(1)?,
        session_id: row.get(2)?,
        agent_id: row.get(3)?,
        workflow_id: row.get(4)?,
        task_id: row.get(5)?,
        turn_id: row.get(6)?,
        occurred_at: row.get(7)?,
        kind,
        severity,
        summary: row.get(10)?,
        tool_name: row.get(11)?,
        causal_event_id: row.get(12)?,
        workflow_event_id: row.get(13)?,
        refs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::operator_activity::classify_tool_activity;
    use tempfile::tempdir;

    fn sample_record(root: &str) -> OperatorActivityRecord {
        classify_tool_activity(
            "content_write",
            r#"{"name":"a.py"}"#,
            r#"{"ok":true,"name":"a.py"}"#,
        )
        .unwrap()
        .into_record(
            root.to_string(),
            root.to_string(),
            "planner.default".to_string(),
            None,
            None,
            Some("turn-1".to_string()),
            Some("content_write".to_string()),
            None,
            None,
        )
    }

    #[test]
    fn list_operator_activity_cursor_ordering() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "session-test";

        let mut first = sample_record(root);
        first.summary = "wrote a.py".to_string();
        first.occurred_at = "2026-06-01T10:00:00Z".to_string();
        store.insert_operator_activity(&first).unwrap();

        let mut second = sample_record(root);
        second.summary = "wrote b.py".to_string();
        second.occurred_at = "2026-06-01T10:00:01Z".to_string();
        store.insert_operator_activity(&second).unwrap();

        let page1 = store.list_operator_activity(root, None, 1, None).unwrap();
        assert_eq!(page1.activities.len(), 1);
        assert!(page1.has_more);
        assert_eq!(page1.activities[0].summary, "wrote a.py");

        let page2 = store
            .list_operator_activity(root, page1.next_cursor.as_deref(), 10, None)
            .unwrap();
        assert_eq!(page2.activities.len(), 1);
        assert_eq!(page2.activities[0].summary, "wrote b.py");
    }

    #[test]
    fn invalid_cursor_falls_back_to_first_page() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "session-invalid-cursor";

        let mut row = sample_record(root);
        row.summary = "visible".to_string();
        store.insert_operator_activity(&row).unwrap();

        let listed = store
            .list_operator_activity(root, Some("oa-does-not-exist"), 10, None)
            .unwrap();
        assert_eq!(listed.activities.len(), 1);
        assert_eq!(listed.activities[0].summary, "visible");
    }

    #[test]
    fn rate_limit_emits_single_notice_then_drops() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "session-rate-limit";
        let at = "2026-06-01T10:00:00+00:00";

        // Cap of 2: the first two real rows go through.
        for i in 0..2 {
            let mut r = sample_record(root);
            r.summary = format!("row {i}");
            r.occurred_at = at.to_string();
            assert_eq!(
                store.insert_operator_activity_throttled(&r, 2).unwrap(),
                OperatorActivityInsert::Inserted
            );
        }

        // The third row in the window is dropped but emits one notice.
        let mut third = sample_record(root);
        third.summary = "row 2".to_string();
        third.occurred_at = at.to_string();
        assert_eq!(
            store.insert_operator_activity_throttled(&third, 2).unwrap(),
            OperatorActivityInsert::ThrottleNoticeEmitted
        );

        // Subsequent rows in the same window are dropped with no extra notice.
        let mut fourth = sample_record(root);
        fourth.summary = "row 3".to_string();
        fourth.occurred_at = at.to_string();
        assert_eq!(
            store.insert_operator_activity_throttled(&fourth, 2).unwrap(),
            OperatorActivityInsert::Dropped
        );

        let listed = store.list_operator_activity(root, None, 50, None).unwrap();
        // 2 real rows + exactly 1 notice.
        assert_eq!(listed.activities.len(), 3);
        let notices = listed
            .activities
            .iter()
            .filter(|a| a.kind == OperatorActivityKind::RateLimited)
            .count();
        assert_eq!(notices, 1);
    }

    #[test]
    fn rate_limit_window_resets_for_later_activity() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "session-rate-window";

        let mut first = sample_record(root);
        first.occurred_at = "2026-06-01T10:00:00+00:00".to_string();
        assert_eq!(
            store.insert_operator_activity_throttled(&first, 1).unwrap(),
            OperatorActivityInsert::Inserted
        );

        // Two minutes later the window no longer contains the first row.
        let mut later = sample_record(root);
        later.occurred_at = "2026-06-01T10:02:00+00:00".to_string();
        assert_eq!(
            store.insert_operator_activity_throttled(&later, 1).unwrap(),
            OperatorActivityInsert::Inserted
        );
    }

    #[test]
    fn rate_limit_window_has_upper_bound_for_out_of_order_inserts() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "session-out-of-order";

        // A later-timestamped row lands first (e.g. concurrent emitter).
        let mut later = sample_record(root);
        later.occurred_at = "2026-06-01T10:02:00+00:00".to_string();
        assert_eq!(
            store.insert_operator_activity_throttled(&later, 1).unwrap(),
            OperatorActivityInsert::Inserted
        );

        // An earlier row's window is [09:59:00, 10:00:00] — the later row sits
        // past its upper bound and must NOT count against it, so it inserts.
        let mut earlier = sample_record(root);
        earlier.occurred_at = "2026-06-01T10:00:00+00:00".to_string();
        assert_eq!(
            store.insert_operator_activity_throttled(&earlier, 1).unwrap(),
            OperatorActivityInsert::Inserted
        );
    }

    #[test]
    fn rate_limit_zero_disables_throttle() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "session-rate-unlimited";

        for i in 0..5 {
            let mut r = sample_record(root);
            r.summary = format!("row {i}");
            r.occurred_at = "2026-06-01T10:00:00+00:00".to_string();
            assert_eq!(
                store.insert_operator_activity_throttled(&r, 0).unwrap(),
                OperatorActivityInsert::Inserted
            );
        }
        let listed = store.list_operator_activity(root, None, 50, None).unwrap();
        assert_eq!(listed.activities.len(), 5);
    }

    #[test]
    fn min_severity_filtered_before_limit() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let root = "session-severity";

        let mut info = sample_record(root);
        info.summary = "info row".to_string();
        info.severity = OperatorActivitySeverity::Info;
        info.occurred_at = "2026-06-01T10:00:00Z".to_string();
        store.insert_operator_activity(&info).unwrap();

        let mut progress = sample_record(root);
        progress.summary = "progress row".to_string();
        progress.severity = OperatorActivitySeverity::Progress;
        progress.occurred_at = "2026-06-01T10:00:01Z".to_string();
        store.insert_operator_activity(&progress).unwrap();

        let listed = store
            .list_operator_activity(root, None, 10, Some(OperatorActivitySeverity::Progress))
            .unwrap();
        assert_eq!(listed.activities.len(), 1);
        assert_eq!(listed.activities[0].summary, "progress row");
    }
}
