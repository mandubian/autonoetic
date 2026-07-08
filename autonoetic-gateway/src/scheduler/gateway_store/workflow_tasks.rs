use super::GatewayStore;
use anyhow::Result;
use autonoetic_types::tool_error::{FailureClass, SideEffectState};
use autonoetic_types::workflow::{QueuedTaskRun, TaskRun, TaskRunStatus};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;

/// Durable execution claim for a queued task. Moved here from workflow_store so
/// the SQLite-backed task store can use it without a crate-module cycle.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TaskExecutionClaim {
    pub workflow_id: String,
    pub task_id: String,
    pub scheduler_instance_id: String,
    pub claimed_at: String,
    pub heartbeat_at: String,
}

impl GatewayStore {
    pub fn upsert_task_run(&self, task: &TaskRun) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let metadata_json = task
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()?;
        let retry_policy_json = task
            .retry_policy
            .as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()?;
        let last_failure_class = task.last_failure_class.map(|fc| {
            serde_json::to_value(fc)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
        });
        let side_effect_state = task.side_effect_state.map(|s| {
            serde_json::to_value(s)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
        });
        conn.execute(
            "INSERT OR REPLACE INTO task_runs (
                task_id, workflow_id, agent_id, session_id, parent_session_id, status,
                created_at, updated_at, source_agent_id, result_summary, join_group,
                message, metadata_json, retry_count, last_failure_class, retry_policy_json,
                side_effect_state, dedupe_key
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                task.task_id,
                task.workflow_id,
                task.agent_id,
                task.session_id,
                task.parent_session_id,
                task.status.as_str(),
                task.created_at,
                task.updated_at,
                task.source_agent_id.as_deref(),
                task.result_summary.as_deref(),
                task.join_group.as_deref(),
                task.message.as_deref(),
                metadata_json,
                task.retry_count as i64,
                last_failure_class.unwrap_or(None),
                retry_policy_json,
                side_effect_state.unwrap_or(None),
                task.dedupe_key.as_deref()
            ],
        )?;
        Ok(())
    }

    pub fn get_task_run(&self, workflow_id: &str, task_id: &str) -> Result<Option<TaskRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM task_runs WHERE workflow_id = ?1 AND task_id = ?2",
        )?;
        let mut rows = stmt.query_map(params![workflow_id, task_id], |row| {
            Ok(task_run_from_row(row)?)
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_task_runs_for_workflow(&self, workflow_id: &str) -> Result<Vec<TaskRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM task_runs WHERE workflow_id = ?1 ORDER BY created_at ASC, task_id ASC",
        )?;
        let rows = stmt.query_map(params![workflow_id], |row| task_run_from_row(row))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_task_run(&self, workflow_id: &str, task_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM task_runs WHERE workflow_id = ?1 AND task_id = ?2",
            params![workflow_id, task_id],
        )?;
        Ok(())
    }

    pub fn enqueue_queued_task(&self, queued: &QueuedTaskRun) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let metadata_json = queued
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()?;
        let credential_bindings_json = serde_json::to_string(&queued.credential_bindings)?;
        conn.execute(
            "INSERT OR REPLACE INTO queued_task_runs (
                task_id, workflow_id, agent_id, message, child_session_id, parent_session_id,
                source_agent_id, metadata_json, join_group, blocks_planner, enqueued_at,
                credential_bindings_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                queued.task_id,
                queued.workflow_id,
                queued.agent_id,
                queued.message,
                queued.child_session_id,
                queued.parent_session_id,
                queued.source_agent_id,
                metadata_json,
                queued.join_group.as_deref(),
                if queued.blocks_planner { 1 } else { 0 },
                queued.enqueued_at,
                credential_bindings_json
            ],
        )?;
        Ok(())
    }

    pub fn dequeue_queued_task(&self, workflow_id: &str, task_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM queued_task_runs WHERE workflow_id = ?1 AND task_id = ?2",
            params![workflow_id, task_id],
        )?;
        Ok(changed > 0)
    }

    pub fn list_queued_tasks_for_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<QueuedTaskRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM queued_task_runs WHERE workflow_id = ?1 ORDER BY enqueued_at ASC",
        )?;
        let rows = stmt.query_map(params![workflow_id], |row| queued_task_run_from_row(row))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_all_queued_tasks(&self) -> Result<Vec<QueuedTaskRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM queued_task_runs ORDER BY enqueued_at ASC")?;
        let rows = stmt.query_map([], |row| queued_task_run_from_row(row))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_queued_task(
        &self,
        workflow_id: &str,
        task_id: &str,
    ) -> Result<Option<QueuedTaskRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM queued_task_runs WHERE workflow_id = ?1 AND task_id = ?2",
        )?;
        let mut rows = stmt.query_map(params![workflow_id, task_id], |row| {
            Ok(queued_task_run_from_row(row)?)
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn upsert_task_claim(&self, claim: &TaskExecutionClaim) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO task_claims (
                task_id, workflow_id, scheduler_instance_id, claimed_at, heartbeat_at
            ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                claim.task_id,
                claim.workflow_id,
                claim.scheduler_instance_id,
                claim.claimed_at,
                claim.heartbeat_at
            ],
        )?;
        Ok(())
    }

    pub fn get_task_claim(
        &self,
        workflow_id: &str,
        task_id: &str,
    ) -> Result<Option<TaskExecutionClaim>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM task_claims WHERE workflow_id = ?1 AND task_id = ?2",
        )?;
        let mut rows = stmt.query_map(params![workflow_id, task_id], |row| {
            Ok(task_claim_from_row(row)?)
        })?;
        Ok(rows.next().transpose()?)
    }

    /// Try to acquire a fresh claim for a task. Returns the new claim if the task
    /// is currently unclaimed or the existing claim is stale. Returns None if an
    /// alive claim exists.
    pub fn acquire_task_claim(
        &self,
        workflow_id: &str,
        task_id: &str,
        stale_after_secs: u64,
    ) -> Result<Option<TaskExecutionClaim>> {
        let now = chrono::Utc::now().to_rfc3339();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let existing: Option<TaskExecutionClaim> = {
                let mut stmt = tx.prepare(
                    "SELECT * FROM task_claims WHERE workflow_id = ?1 AND task_id = ?2",
                )?;
                let mut rows = stmt.query_map(params![workflow_id, task_id], |row| {
                    Ok(task_claim_from_row(row)?)
                })?;
                rows.next().transpose()?
            };
            if let Some(claim) = existing {
                if !claim_is_stale(&claim, stale_after_secs) {
                    return Ok(None);
                }
                tx.execute(
                    "DELETE FROM task_claims WHERE workflow_id = ?1 AND task_id = ?2",
                    params![workflow_id, task_id],
                )?;
            }
        }
        let claim = TaskExecutionClaim {
            workflow_id: workflow_id.to_string(),
            task_id: task_id.to_string(),
            scheduler_instance_id: uuid::Uuid::new_v4().to_string(),
            claimed_at: now.clone(),
            heartbeat_at: now,
        };
        tx.execute(
            "INSERT INTO task_claims (
                task_id, workflow_id, scheduler_instance_id, claimed_at, heartbeat_at
            ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                claim.task_id,
                claim.workflow_id,
                claim.scheduler_instance_id,
                claim.claimed_at,
                claim.heartbeat_at
            ],
        )?;
        tx.commit()?;
        Ok(Some(claim))
    }

    pub fn refresh_task_claim_heartbeat(
        &self,
        workflow_id: &str,
        task_id: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE task_claims SET heartbeat_at = ?1 WHERE workflow_id = ?2 AND task_id = ?3",
            params![now, workflow_id, task_id],
        )?;
        Ok(())
    }

    pub fn delete_task_claim(&self, workflow_id: &str, task_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM task_claims WHERE workflow_id = ?1 AND task_id = ?2",
            params![workflow_id, task_id],
        )?;
        Ok(())
    }

    pub fn delete_task_claims_for_workflow(&self, workflow_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM task_claims WHERE workflow_id = ?1",
            params![workflow_id],
        )?;
        Ok(())
    }

    pub fn delete_queued_tasks_for_workflow(&self, workflow_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM queued_task_runs WHERE workflow_id = ?1",
            params![workflow_id],
        )?;
        Ok(())
    }

    pub fn delete_task_runs_for_workflow(&self, workflow_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM task_runs WHERE workflow_id = ?1",
            params![workflow_id],
        )?;
        Ok(())
    }
}

fn claim_is_stale(claim: &TaskExecutionClaim, stale_after_secs: u64) -> bool {
    let Ok(heartbeat_at) = chrono::DateTime::parse_from_rfc3339(&claim.heartbeat_at) else {
        return true;
    };
    chrono::Utc::now() - heartbeat_at.with_timezone(&chrono::Utc)
        > chrono::Duration::seconds(stale_after_secs as i64)
}

fn task_run_from_row(row: &rusqlite::Row) -> rusqlite::Result<TaskRun> {
    let status: String = row.get("status")?;
    let status = parse_task_status(&status).map_err(to_sqlite_error)?;
    let metadata_json: Option<String> = row.get("metadata_json")?;
    let metadata = metadata_json
        .map(|s| serde_json::from_str(&s).map_err(|e| to_sqlite_error(anyhow::Error::from(e))))
        .transpose()?;
    let retry_policy_json: Option<String> = row.get("retry_policy_json")?;
    let retry_policy = retry_policy_json
        .map(|s| serde_json::from_str(&s).map_err(|e| to_sqlite_error(anyhow::Error::from(e))))
        .transpose()?;
    let last_failure_class: Option<String> = row.get("last_failure_class")?;
    let last_failure_class = last_failure_class
        .map(|s| parse_failure_class(&s).map_err(to_sqlite_error))
        .transpose()?;
    let side_effect_state: Option<String> = row.get("side_effect_state")?;
    let side_effect_state = side_effect_state
        .map(|s| parse_side_effect_state(&s).map_err(to_sqlite_error))
        .transpose()?;
    Ok(TaskRun {
        task_id: row.get("task_id")?,
        workflow_id: row.get("workflow_id")?,
        agent_id: row.get("agent_id")?,
        session_id: row.get("session_id")?,
        parent_session_id: row.get("parent_session_id")?,
        status,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        source_agent_id: row.get("source_agent_id")?,
        result_summary: row.get("result_summary")?,
        join_group: row.get("join_group")?,
        message: row.get("message")?,
        metadata,
        retry_count: row.get::<_, i64>("retry_count")? as u32,
        last_failure_class,
        retry_policy,
        side_effect_state,
        dedupe_key: row.get("dedupe_key")?,
    })
}

fn queued_task_run_from_row(row: &rusqlite::Row) -> rusqlite::Result<QueuedTaskRun> {
    let metadata_json: Option<String> = row.get("metadata_json")?;
    let metadata = metadata_json
        .map(|s| serde_json::from_str(&s).map_err(|e| to_sqlite_error(anyhow::Error::from(e))))
        .transpose()?;
    let credential_bindings_json: String = row.get("credential_bindings_json")?;
    let credential_bindings: Vec<autonoetic_types::runtime_lock::LockedCredentialMount> =
        serde_json::from_str(&credential_bindings_json).map_err(|e| to_sqlite_error(anyhow::Error::from(e)))?;
    Ok(QueuedTaskRun {
        task_id: row.get("task_id")?,
        workflow_id: row.get("workflow_id")?,
        agent_id: row.get("agent_id")?,
        message: row.get("message")?,
        child_session_id: row.get("child_session_id")?,
        parent_session_id: row.get("parent_session_id")?,
        source_agent_id: row.get("source_agent_id")?,
        metadata,
        join_group: row.get("join_group")?,
        blocks_planner: row.get::<_, i64>("blocks_planner")? != 0,
        enqueued_at: row.get("enqueued_at")?,
        credential_bindings,
    })
}

fn task_claim_from_row(row: &rusqlite::Row) -> rusqlite::Result<TaskExecutionClaim> {
    Ok(TaskExecutionClaim {
        task_id: row.get("task_id")?,
        workflow_id: row.get("workflow_id")?,
        scheduler_instance_id: row.get("scheduler_instance_id")?,
        claimed_at: row.get("claimed_at")?,
        heartbeat_at: row.get("heartbeat_at")?,
    })
}

fn to_sqlite_error(e: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())),
    )
}

fn parse_task_status(s: &str) -> Result<TaskRunStatus> {
    let v = serde_json::Value::String(s.to_string());
    TaskRunStatus::deserialize(v).map_err(|e| e.into())
}

fn parse_failure_class(s: &str) -> Result<FailureClass> {
    let v = serde_json::Value::String(s.to_string());
    FailureClass::deserialize(v).map_err(|e| e.into())
}

fn parse_side_effect_state(s: &str) -> Result<SideEffectState> {
    let v = serde_json::Value::String(s.to_string());
    SideEffectState::deserialize(v).map_err(|e| e.into())
}
