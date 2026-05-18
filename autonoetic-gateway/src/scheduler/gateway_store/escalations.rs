use anyhow::{bail, Result};
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus, EscalationType};
use rusqlite::{params, Connection, OptionalExtension};

use super::GatewayStore;

fn row_to_escalation(row: &rusqlite::Row) -> rusqlite::Result<EscalationMessage> {
    let status_str: String = row.get(10)?;
    let status = EscalationStatus::parse(&status_str).unwrap_or(EscalationStatus::Pending);
    let code_excerpts_json: Option<String> = row.get(13)?;
    let code_excerpts = code_excerpts_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let escalation_type_str: String = row.get(14)?;
    let escalation_type =
        EscalationType::parse(&escalation_type_str).unwrap_or(EscalationType::PromotionReview);
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
        code_excerpts,
        escalation_type,
    })
}

impl GatewayStore {
    pub fn set_escalation_flood_cap(&self, cap: usize) {
        self.escalation_flood_cap
            .store(cap, std::sync::atomic::Ordering::Relaxed);
    }

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

        let root_sid = &escalation.root_session_id;
        if !root_sid.is_empty() {
            let cap = self
                .escalation_flood_cap
                .load(std::sync::atomic::Ordering::Relaxed);
            if cap > 0 {
                let pending_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM escalations WHERE root_session_id = ?1 AND status = 'pending'",
                    params![root_sid],
                    |row| row.get(0),
                )?;
                if (pending_count as usize) >= cap {
                    bail!(
                        "escalation_flood: root session '{}' already has {} pending escalations (cap {})",
                        root_sid,
                        pending_count,
                        cap
                    );
                }
            }
        }

        let role_verdicts = serde_json::to_string(&escalation.role_verdicts)?;
        let code_excerpts_json = escalation
            .code_excerpts
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        conn.execute(
            "INSERT INTO escalations (escalation_id, artifact_id, artifact_digest, agent_id,
             revision_id, role_verdicts, planner_synthesis, created_at, resolved_at,
             root_session_id, status, code_excerpts, escalation_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                code_excerpts_json,
                escalation.escalation_type.as_str(),
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
             decided_by, decision_reason, code_excerpts, escalation_type
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
             decided_by, decision_reason, code_excerpts, escalation_type
             FROM escalations WHERE status = 'pending' ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], row_to_escalation)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Find the latest escalation for an artifact+revision with a matching
    /// status (used by the FullJury gate to check for operator approval).
    pub fn find_escalation(
        &self,
        artifact_id: &str,
        revision_id: &str,
        status: EscalationStatus,
    ) -> Result<Option<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status,
             decided_by, decision_reason, code_excerpts, escalation_type
             FROM escalations
             WHERE artifact_id = ?1 AND revision_id = ?2 AND status = ?3
             ORDER BY created_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(
            params![artifact_id, revision_id, status.as_str()],
            row_to_escalation,
        )?;
        Ok(rows.next().transpose()?)
    }

    pub fn find_approved_escalation_for_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<Option<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status,
             decided_by, decision_reason, code_excerpts, escalation_type
             FROM escalations
             WHERE artifact_id = ?1 AND status = 'approved'
             ORDER BY created_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![artifact_id], row_to_escalation)?;
        Ok(rows.next().transpose()?)
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
