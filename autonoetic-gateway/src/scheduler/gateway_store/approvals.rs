use anyhow::Result;
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest};
use rusqlite::{params, Connection, OptionalExtension};

use super::GatewayStore;

impl GatewayStore {
    pub fn create_approval(&self, request: &ApprovalRequest) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let action_payload = serde_json::to_string(&request.action)?;
        conn.execute(
            "INSERT INTO approvals (
                request_id, agent_id, session_id, root_session_id, workflow_id, task_id,
                action_type, action_payload, reason, evidence_ref, status, created_at,
                approval_level
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                request.request_id,
                request.agent_id,
                request.session_id,
                request.root_session_id,
                request.workflow_id,
                request.task_id,
                request.action.kind(),
                action_payload,
                request.reason,
                request.evidence_ref,
                "pending",
                request.created_at,
                serde_json::to_string(&request.approval_level)?
            ],
        )?;
        Ok(())
    }

    fn get_approval_with_conn(
        conn: &Connection,
        request_id: &str,
    ) -> Result<Option<ApprovalRequest>> {
        conn.query_row(
            "SELECT request_id, agent_id, session_id, action_payload, created_at, workflow_id, task_id, root_session_id, status, decided_at, decided_by, reason, evidence_ref, approval_level FROM approvals WHERE request_id = ?1",
            params![request_id],
            |row| {
                let action_payload: String = row.get(3)?;
                let status_str: Option<String> = row.get(8)?;
                let status = status_str.and_then(|s| match s.as_str() {
                    "approved" => Some(autonoetic_types::background::ApprovalStatus::Approved),
                    "rejected" => Some(autonoetic_types::background::ApprovalStatus::Rejected),
                    "cancelled" => Some(autonoetic_types::background::ApprovalStatus::Cancelled),
                    _ => None,
                });
                let action = serde_json::from_str(&action_payload).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
                })?;
                let level_str: String = row.get(13)?;
                let approval_level: ApprovalLevel = serde_json::from_str(&level_str).unwrap_or(ApprovalLevel::Operator);
                Ok(ApprovalRequest {
                    request_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    session_id: row.get(2)?,
                    action,
                    created_at: row.get(4)?,
                    workflow_id: row.get(5)?,
                    task_id: row.get(6)?,
                    root_session_id: row.get(7)?,
                    status,
                    decided_at: row.get(9)?,
                    decided_by: row.get(10)?,
                    reason: row.get(11)?,
                    evidence_ref: row.get(12)?,
                    approval_level,
                })
            },
        ).optional().map_err(Into::into)
    }

    pub fn get_approval(&self, request_id: &str) -> Result<Option<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        Self::get_approval_with_conn(&conn, request_id)
    }

    pub fn record_decision(
        &self,
        request_id: &str,
        status: &str,
        decided_by: &str,
        decided_at: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE approvals SET status = ?1, decided_by = ?2, decided_at = ?3 WHERE request_id = ?4 AND status = 'pending'",
            params![status, decided_by, decided_at, request_id],
        )?;
        if rows == 0 {
            anyhow::bail!(
                "Approval {} is no longer pending (already decided or not found)",
                request_id
            );
        }
        Ok(())
    }

    pub fn cancel_approval(
        &self,
        request_id: &str,
        cancelled_by: &str,
        cancelled_at: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE approvals SET status = 'cancelled', decided_by = ?1, decided_at = ?2 WHERE request_id = ?3 AND status = 'pending'",
            params![cancelled_by, cancelled_at, request_id],
        )?;
        if rows == 0 {
            anyhow::bail!(
                "Approval {} is no longer pending (already decided or not found)",
                request_id
            );
        }
        Ok(())
    }

    pub fn get_pending_approvals(&self) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT request_id FROM approvals WHERE status = 'pending'")?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }

    pub fn get_pending_approvals_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals WHERE root_session_id = ?1 AND status = 'pending'",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }

    pub fn get_approved_approvals_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals WHERE root_session_id = ?1 AND status = 'approved'",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }

    pub fn get_approved_approvals_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals WHERE session_id = ?1 AND status = 'approved'",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }
}
