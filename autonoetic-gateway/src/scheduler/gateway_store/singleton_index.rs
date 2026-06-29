use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use super::GatewayStore;

impl GatewayStore {
    /// Attempt to acquire a singleton slot for `(workflow_id, agent_id, revision_id)`.
    ///
    /// Returns `Ok(None)` if the slot was acquired for `task_id`. Returns
    /// `Ok(Some(existing_task_id))` if an active (pending or running) singleton
    /// task already exists for the dedup key.
    pub fn acquire_singleton_slot(
        &self,
        workflow_id: &str,
        agent_id: &str,
        revision_id: Option<&str>,
        task_id: &str,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let revision = revision_id.unwrap_or("");
        let now = chrono::Utc::now().to_rfc3339();

        // Upsert: only take over a terminal row. Active rows are left untouched
        // so the SELECT below returns the existing task_id.
        conn.execute(
            "INSERT INTO workflow_singleton_index (workflow_id, agent_id, revision_id, task_id, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?5)
             ON CONFLICT(workflow_id, agent_id, revision_id) DO UPDATE SET
                 task_id = excluded.task_id,
                 status = 'pending',
                 updated_at = excluded.updated_at
             WHERE status = 'terminal'",
            params![workflow_id, agent_id, revision, task_id, now],
        )?;

        let existing: Option<String> = conn
            .query_row(
                "SELECT task_id FROM workflow_singleton_index
                 WHERE workflow_id = ?1 AND agent_id = ?2 AND revision_id = ?3 AND status IN ('pending', 'running')",
                params![workflow_id, agent_id, revision],
                |row| row.get(0),
            )
            .optional()?;

        Ok(existing.filter(|id: &String| id != task_id))
    }

    /// Mark a singleton slot as running.
    pub fn activate_singleton_task(
        &self,
        workflow_id: &str,
        agent_id: &str,
        revision_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let revision = revision_id.unwrap_or("");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE workflow_singleton_index SET status = 'running', updated_at = ?4
             WHERE workflow_id = ?1 AND agent_id = ?2 AND revision_id = ?3",
            params![workflow_id, agent_id, revision, now],
        )?;
        Ok(())
    }

    /// Mark a singleton slot terminal by its task_id. This is idempotent and
    /// only affects rows that are currently pending or running.
    pub fn release_singleton_slot_by_task_id(
        &self,
        workflow_id: &str,
        task_id: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE workflow_singleton_index SET status = 'terminal', updated_at = ?3
             WHERE workflow_id = ?1 AND task_id = ?2 AND status IN ('pending', 'running')",
            params![workflow_id, task_id, now],
        )?;
        Ok(())
    }

    /// Delete all singleton index rows for a workflow. Used on emergency stop
    /// and workflow cleanup.
    pub fn delete_singleton_slots_for_workflow(&self, workflow_id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "DELETE FROM workflow_singleton_index WHERE workflow_id = ?1",
            params![workflow_id],
        )?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_memory_store() -> (GatewayStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn singleton_slot_acquire_and_dedup() {
        let (store, _dir) = open_memory_store();
        let wf = "wf-singleton-1";
        let agent = "architect.default";

        let first = store
            .acquire_singleton_slot(wf, agent, None, "task-first")
            .unwrap();
        assert!(first.is_none());

        let second = store
            .acquire_singleton_slot(wf, agent, None, "task-second")
            .unwrap();
        assert_eq!(second, Some("task-first".to_string()));
    }

    #[test]
    fn singleton_slot_revision_isolation() {
        let (store, _dir) = open_memory_store();
        let wf = "wf-singleton-2";
        let agent = "architect.default";

        let r1 = store
            .acquire_singleton_slot(wf, agent, Some("rev-a"), "task-a")
            .unwrap();
        assert!(r1.is_none());

        let r2 = store
            .acquire_singleton_slot(wf, agent, Some("rev-b"), "task-b")
            .unwrap();
        assert!(r2.is_none());

        let r3 = store
            .acquire_singleton_slot(wf, agent, Some("rev-a"), "task-c")
            .unwrap();
        assert_eq!(r3, Some("task-a".to_string()));
    }

    #[test]
    fn singleton_slot_release_allows_reacquire() {
        let (store, _dir) = open_memory_store();
        let wf = "wf-singleton-3";
        let agent = "architect.default";

        assert!(store
            .acquire_singleton_slot(wf, agent, None, "task-first")
            .unwrap()
            .is_none());
        store.release_singleton_slot_by_task_id(wf, "task-first").unwrap();

        let again = store
            .acquire_singleton_slot(wf, agent, None, "task-again")
            .unwrap();
        assert!(again.is_none());
    }

    #[test]
    fn singleton_slot_agent_isolation() {
        let (store, _dir) = open_memory_store();
        let wf = "wf-singleton-4";

        assert!(store
            .acquire_singleton_slot(wf, "architect.default", None, "task-arch")
            .unwrap()
            .is_none());
        assert!(store
            .acquire_singleton_slot(wf, "coder.default", None, "task-coder")
            .unwrap()
            .is_none());
    }
}
