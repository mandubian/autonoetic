use super::GatewayStore;
use super::WorkflowIndexFile;
use anyhow::Result;
use autonoetic_types::workflow::WorkflowEventRecord;
use rusqlite::{params, OptionalExtension};

impl GatewayStore {
    pub fn append_workflow_event(&self, event: &WorkflowEventRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let payload = serde_json::to_string(&event.payload)?;
        conn.execute(
            "INSERT INTO workflow_events (
                event_id, workflow_id, event_type, task_id, agent_id, payload, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                event.event_id,
                event.workflow_id,
                event.event_type,
                event.task_id,
                event.agent_id,
                payload,
                event.occurred_at
            ],
        )?;
        Ok(())
    }

    pub fn list_workflow_events(&self, workflow_id: &str) -> Result<Vec<WorkflowEventRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM workflow_events WHERE workflow_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![workflow_id], |row| {
            let payload_str: String = row.get(5)?;
            let payload = serde_json::from_str(&payload_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(WorkflowEventRecord {
                event_id: row.get(0)?,
                workflow_id: row.get(1)?,
                event_type: row.get(2)?,
                task_id: row.get(3)?,
                agent_id: row.get(4)?,
                payload,
                occurred_at: row.get(6)?,
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn list_workflow_events_since(
        &self,
        workflow_id: &str,
        since: &str,
    ) -> Result<Vec<WorkflowEventRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM workflow_events WHERE workflow_id = ?1 AND created_at > ?2 ORDER BY created_at ASC")?;
        let rows = stmt.query_map(params![workflow_id, since], |row| {
            let payload_str: String = row.get(5)?;
            let payload = serde_json::from_str(&payload_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(WorkflowEventRecord {
                event_id: row.get(0)?,
                workflow_id: row.get(1)?,
                event_type: row.get(2)?,
                task_id: row.get(3)?,
                agent_id: row.get(4)?,
                payload,
                occurred_at: row.get(6)?,
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn set_workflow_index(&self, root_session_id: &str, workflow_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO workflow_index (root_session_id, workflow_id, created_at) VALUES (?1, ?2, ?3)",
            params![root_session_id, workflow_id, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn resolve_workflow_id(&self, root_session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result: Option<String> = conn
            .query_row(
                "SELECT workflow_id FROM workflow_index WHERE root_session_id = ?1",
                params![root_session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
    }

    pub fn resolve_root_session_id(&self, workflow_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result: Option<String> = conn
            .query_row(
                "SELECT root_session_id FROM workflow_index WHERE workflow_id = ?1",
                params![workflow_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
    }

    pub fn list_workflow_index(&self) -> Result<Vec<WorkflowIndexFile>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT workflow_id, root_session_id, created_at FROM workflow_index")?;
        let rows = stmt.query_map(params![], |row| {
            Ok(WorkflowIndexFile {
                workflow_id: row.get(0)?,
                root_session_id: row.get(1)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }
}
