use anyhow::{bail, Result};
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus};
use rusqlite::{params, Connection, OptionalExtension};

use super::GatewayStore;

fn row_to_escalation(row: &rusqlite::Row) -> rusqlite::Result<EscalationMessage> {
    let status_str: String = row.get(10)?;
    let status = EscalationStatus::parse(&status_str).unwrap_or(EscalationStatus::Pending);
    Ok(EscalationMessage {
        escalation_id: row.get(0)?,
        artifact_id: row.get(1)?,
        artifact_digest: row.get(2)?,
        agent_id: row.get(3)?,
        revision_id: row.get(4)?,
        role_verdicts: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
        planner_synthesis: row.get(6)?,
        created_at: row.get(7)?,
        resolved_at: row.get(8)?,
        root_session_id: row.get(9)?,
        status,
        decided_by: row.get(11)?,
        decision_reason: row.get(12)?,
    })
}

impl GatewayStore {
    pub fn create_escalation(&self, escalation: &EscalationMessage) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        let existing: Option<String> = conn
            .query_row(
                "SELECT escalation_id FROM escalations \
                 WHERE artifact_id = ?1 AND revision_id = ?2 AND status = 'pending'",
                params![escalation.artifact_id, escalation.revision_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing_id) = existing {
            bail!(
                "A pending escalation '{}' already exists for artifact '{}' revision '{}'",
                existing_id,
                escalation.artifact_id,
                escalation.revision_id
            );
        }

        let role_verdicts = serde_json::to_string(&escalation.role_verdicts)?;
        conn.execute(
            "INSERT INTO escalations (escalation_id, artifact_id, artifact_digest, agent_id,
             revision_id, role_verdicts, planner_synthesis, created_at, resolved_at,
             root_session_id, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                escalation.escalation_id,
                escalation.artifact_id,
                escalation.artifact_digest,
                escalation.agent_id,
                escalation.revision_id,
                role_verdicts,
                escalation.planner_synthesis,
                escalation.created_at,
                escalation.resolved_at,
                escalation.root_session_id,
                escalation.status.as_str(),
            ],
        )?;
        Ok(())
    }

    fn escalation_exists_with_conn(
        &self,
        conn: &Connection,
        escalation_id: &str,
    ) -> Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM escalations WHERE escalation_id = ?1",
            params![escalation_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn get_escalation(&self, escalation_id: &str) -> Result<Option<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status,
             decided_by, decision_reason
             FROM escalations WHERE escalation_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![escalation_id], row_to_escalation)?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_pending_escalations(&self) -> Result<Vec<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status,
             decided_by, decision_reason
             FROM escalations WHERE status = 'pending' ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], row_to_escalation)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub fn resolve_escalation(
        &self,
        escalation_id: &str,
        status: EscalationStatus,
        decided_by: &str,
        decision_reason: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        if !self.escalation_exists_with_conn(&conn, escalation_id)? {
            bail!("Escalation '{}' not found", escalation_id);
        }

        let current_status: String = conn.query_row(
            "SELECT status FROM escalations WHERE escalation_id = ?1",
            params![escalation_id],
            |row| row.get(0),
        )?;
        if current_status != "pending" {
            bail!(
                "Escalation '{}' is already '{}'; cannot resolve again",
                escalation_id,
                current_status
            );
        }

        conn.execute(
            "UPDATE escalations SET status = ?1, resolved_at = ?2, decided_by = ?3, \
             decision_reason = ?4 WHERE escalation_id = ?5",
            params![
                status.as_str(),
                chrono::Utc::now().to_rfc3339(),
                decided_by,
                decision_reason,
                escalation_id
            ],
        )?;
        Ok(())
    }
}
