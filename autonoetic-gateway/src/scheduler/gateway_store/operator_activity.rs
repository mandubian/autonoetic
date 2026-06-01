use anyhow::Result;
use autonoetic_types::operator_activity::{
    OperatorActivityKind, OperatorActivityListResult, OperatorActivityRecord,
    OperatorActivityRefs, OperatorActivitySeverity,
};
use rusqlite::{params, Connection};

use super::GatewayStore;

impl GatewayStore {
    pub fn insert_operator_activity(&self, record: &OperatorActivityRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let kind = record.kind.as_str();
        let severity = record.severity.as_str();
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
                kind,
                severity,
                record.summary,
                record.tool_name,
                record.causal_event_id,
                record.workflow_event_id,
                refs_json,
            ],
        )?;
        Ok(())
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

        let rows: Vec<OperatorActivityRecord> = if let Some(after_id) = after_activity_id {
            let mut stmt = conn.prepare(
                "SELECT activity_id, root_session_id, session_id, agent_id,
                        workflow_id, task_id, turn_id, occurred_at,
                        kind, severity, summary, tool_name,
                        causal_event_id, workflow_event_id, refs_json
                 FROM operator_activity
                 WHERE root_session_id = ?1
                   AND (
                     occurred_at > (SELECT occurred_at FROM operator_activity WHERE activity_id = ?2)
                     OR (
                       occurred_at = (SELECT occurred_at FROM operator_activity WHERE activity_id = ?2)
                       AND activity_id > ?2
                     )
                   )
                 ORDER BY occurred_at ASC, activity_id ASC
                 LIMIT ?3",
            )?;
            let mapped = stmt.query_map(params![root_session_id, after_id, fetch_limit], map_row)?;
            mapped.collect::<Result<Vec<_>, _>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT activity_id, root_session_id, session_id, agent_id,
                        workflow_id, task_id, turn_id, occurred_at,
                        kind, severity, summary, tool_name,
                        causal_event_id, workflow_event_id, refs_json
                 FROM operator_activity
                 WHERE root_session_id = ?1
                 ORDER BY occurred_at ASC, activity_id ASC
                 LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![root_session_id, fetch_limit], map_row)?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };

        let mut filtered: Vec<OperatorActivityRecord> = rows
            .into_iter()
            .filter(|r| severity_meets_min(r.severity, min_severity))
            .collect();

        let has_more = filtered.len() > limit as usize;
        if has_more {
            filtered.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            filtered.last().map(|r| r.activity_id.clone())
        } else {
            None
        };

        Ok(OperatorActivityListResult {
            activities: filtered,
            next_cursor,
            has_more,
        })
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

fn severity_meets_min(
    severity: OperatorActivitySeverity,
    min: Option<OperatorActivitySeverity>,
) -> bool {
    match min {
        None => true,
        Some(min) => severity >= min,
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
        other => {
            return Err(rusqlite::Error::InvalidColumnType(
                8,
                other.to_string(),
                rusqlite::types::Type::Text,
            ))
        }
    };

    let severity = OperatorActivitySeverity::parse_str(&severity_str).ok_or_else(|| {
        rusqlite::Error::InvalidColumnType(
            9,
            severity_str,
            rusqlite::types::Type::Text,
        )
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
    use autonoetic_types::operator_activity::OperatorActivitySeverity;
    use crate::runtime::operator_activity::classify_tool_activity;
    use tempfile::tempdir;

    fn sample_record(root: &str, summary: &str) -> OperatorActivityRecord {
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

        let mut first = sample_record(root, "first");
        first.summary = "wrote a.py".to_string();
        first.occurred_at = "2026-06-01T10:00:00Z".to_string();
        store.insert_operator_activity(&first).unwrap();

        let mut second = sample_record(root, "second");
        second.summary = "wrote b.py".to_string();
        second.occurred_at = "2026-06-01T10:00:01Z".to_string();
        store.insert_operator_activity(&second).unwrap();

        let page1 = store
            .list_operator_activity(root, None, 1, None)
            .unwrap();
        assert_eq!(page1.activities.len(), 1);
        assert!(page1.has_more);
        assert_eq!(page1.activities[0].summary, "wrote a.py");

        let page2 = store
            .list_operator_activity(root, page1.next_cursor.as_deref(), 10, None)
            .unwrap();
        assert_eq!(page2.activities.len(), 1);
        assert_eq!(page2.activities[0].summary, "wrote b.py");
    }
}
