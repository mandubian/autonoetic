use anyhow::Result;
use rusqlite::params;

use super::{ActiveExecutionRecord, EmergencyStopRecord, GatewayStore};

impl GatewayStore {
    pub fn insert_emergency_stop(&self, row: &EmergencyStopRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO emergency_stops (
                stop_id, scope_type, scope_id, root_session_id, workflow_id,
                requested_by_type, requested_by_id, reason, trigger_kind, mode,
                status, requested_at, completed_at, details_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                &row.stop_id,
                &row.scope_type,
                &row.scope_id,
                &row.root_session_id,
                row.workflow_id.as_deref(),
                &row.requested_by_type,
                &row.requested_by_id,
                row.reason.as_deref(),
                &row.trigger_kind,
                &row.mode,
                &row.status,
                &row.requested_at,
                row.completed_at.as_deref(),
                row.details_json.as_deref(),
            ],
        )?;
        Ok(())
    }

    pub fn update_emergency_stop_status(
        &self,
        stop_id: &str,
        status: &str,
        completed_at: Option<&str>,
        details_json: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE emergency_stops SET status = ?1, completed_at = ?2, details_json = ?3 WHERE stop_id = ?4",
            params![status, completed_at, details_json, stop_id],
        )?;
        anyhow::ensure!(
            changed == 1,
            "emergency stop '{}' not found or not updated",
            stop_id
        );
        Ok(())
    }

    pub fn list_emergency_stops_for_root_session(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<EmergencyStopRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT stop_id, scope_type, scope_id, root_session_id, workflow_id, requested_by_type,
                    requested_by_id, reason, trigger_kind, mode, status, requested_at, completed_at, details_json
             FROM emergency_stops WHERE root_session_id = ?1 ORDER BY requested_at DESC",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            Ok(EmergencyStopRecord {
                stop_id: row.get(0)?,
                scope_type: row.get(1)?,
                scope_id: row.get(2)?,
                root_session_id: row.get(3)?,
                workflow_id: row.get(4)?,
                requested_by_type: row.get(5)?,
                requested_by_id: row.get(6)?,
                reason: row.get(7)?,
                trigger_kind: row.get(8)?,
                mode: row.get(9)?,
                status: row.get(10)?,
                requested_at: row.get(11)?,
                completed_at: row.get(12)?,
                details_json: row.get(13)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn upsert_active_execution(&self, row: &ActiveExecutionRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO active_executions (
                execution_id, root_session_id, workflow_id, task_id, session_id, agent_id,
                execution_kind, driver, pid, host_id, status, started_at, heartbeat_at,
                stop_requested_at, stopped_at, stop_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                &row.execution_id,
                &row.root_session_id,
                row.workflow_id.as_deref(),
                row.task_id.as_deref(),
                &row.session_id,
                &row.agent_id,
                &row.execution_kind,
                row.driver.as_deref(),
                row.pid,
                &row.host_id,
                &row.status,
                &row.started_at,
                &row.heartbeat_at,
                row.stop_requested_at.as_deref(),
                row.stopped_at.as_deref(),
                row.stop_id.as_deref(),
            ],
        )?;
        Ok(())
    }

    pub fn touch_active_execution_heartbeat(&self, execution_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let n = conn.execute(
            "UPDATE active_executions SET heartbeat_at = ?1 WHERE execution_id = ?2",
            params![now, execution_id],
        )?;
        anyhow::ensure!(
            n == 1,
            "active execution '{}' not found for heartbeat",
            execution_id
        );
        Ok(())
    }

    pub fn complete_active_execution(
        &self,
        execution_id: &str,
        status: &str,
        stop_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let n = conn.execute(
            "UPDATE active_executions SET status = ?1, stopped_at = ?2, stop_id = ?3 WHERE execution_id = ?4",
            params![status, now, stop_id, execution_id],
        )?;
        anyhow::ensure!(
            n == 1,
            "active execution '{}' not found for completion",
            execution_id
        );
        Ok(())
    }

    pub fn list_active_executions_for_root_sqlite(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<ActiveExecutionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT execution_id, root_session_id, workflow_id, task_id, session_id, agent_id,
                    execution_kind, driver, pid, host_id, status, started_at, heartbeat_at,
                    stop_requested_at, stopped_at, stop_id
             FROM active_executions WHERE root_session_id = ?1 ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            Ok(ActiveExecutionRecord {
                execution_id: row.get(0)?,
                root_session_id: row.get(1)?,
                workflow_id: row.get(2)?,
                task_id: row.get(3)?,
                session_id: row.get(4)?,
                agent_id: row.get(5)?,
                execution_kind: row.get(6)?,
                driver: row.get(7)?,
                pid: row.get(8)?,
                host_id: row.get(9)?,
                status: row.get(10)?,
                started_at: row.get(11)?,
                heartbeat_at: row.get(12)?,
                stop_requested_at: row.get(13)?,
                stopped_at: row.get(14)?,
                stop_id: row.get(15)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}
