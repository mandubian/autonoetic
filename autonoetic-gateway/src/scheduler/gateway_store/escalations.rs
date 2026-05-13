use anyhow::Result;
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus};
use rusqlite::{params, OptionalExtension};

use super::GatewayStore;

impl GatewayStore {
    pub fn create_escalation(&self, escalation: &mut EscalationMessage) -> Result<()> {
        let conn = self.conn.lock().unwrap();
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

    pub fn get_escalation(&self, escalation_id: &str) -> Result<Option<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status
             FROM escalations WHERE escalation_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![escalation_id], |row| {
            let status_str: String = row.get(10)?;
            let status =
                EscalationStatus::from_str(&status_str).unwrap_or(EscalationStatus::Pending);
            Ok(EscalationMessage {
                escalation_id: row.get(0)?,
                artifact_id: row.get(1)?,
                artifact_digest: row.get(2)?,
                agent_id: row.get(3)?,
                revision_id: row.get(4)?,
                role_verdicts: serde_json::from_str(&row.get::<_, String>(5)?)
                    .unwrap_or_default(),
                planner_synthesis: row.get(6)?,
                created_at: row.get(7)?,
                resolved_at: row.get(8)?,
                root_session_id: row.get(9)?,
                status,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_pending_escalations(&self) -> Result<Vec<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status
             FROM escalations WHERE status = 'pending' ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let status_str: String = row.get(10)?;
            let status =
                EscalationStatus::from_str(&status_str).unwrap_or(EscalationStatus::Pending);
            Ok(EscalationMessage {
                escalation_id: row.get(0)?,
                artifact_id: row.get(1)?,
                artifact_digest: row.get(2)?,
                agent_id: row.get(3)?,
                revision_id: row.get(4)?,
                role_verdicts: serde_json::from_str(&row.get::<_, String>(5)?)
                    .unwrap_or_default(),
                planner_synthesis: row.get(6)?,
                created_at: row.get(7)?,
                resolved_at: row.get(8)?,
                root_session_id: row.get(9)?,
                status,
            })
        })?;
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
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE escalations SET status = ?1, resolved_at = ?2 WHERE escalation_id = ?3",
            params![
                status.as_str(),
                chrono::Utc::now().to_rfc3339(),
                escalation_id
            ],
        )?;
        Ok(())
    }
}
