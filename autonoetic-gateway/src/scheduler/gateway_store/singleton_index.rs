use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use super::GatewayStore;

impl GatewayStore {
    /// Attempt to acquire a singleton slot for `(workflow_id, agent_id, revision_id)`.
    ///
    /// Returns `Ok(None)` if the slot was acquired for `task_id`. Returns
    /// `Ok(Some(existing_task_id))` if an active (pending or running) singleton
    /// task already exists for the dedup key.
    ///
    /// The index is a cache of "who owns this singleton", not the source of
    /// truth for task liveness (`task_runs` is). Any completion path that
    /// writes a terminal task status without releasing the slot — historically
    /// the P-7.16 orphan reaper, which saves the task directly — would
    /// otherwise wedge the singleton forever: every later spawn dedups onto a
    /// dead task that `workflow_cancel_task` and `workflow_force_complete`
    /// both refuse to touch. Acquisition therefore verifies the indexed task
    /// and self-heals a verified-terminal owner before deduping.
    pub fn acquire_singleton_slot(
        &self,
        workflow_id: &str,
        agent_id: &str,
        revision_id: Option<&str>,
        task_id: &str,
    ) -> Result<Option<String>> {
        // Two passes at most: the first may discover a leaked slot and release
        // it; the second then acquires normally.
        for _ in 0..2 {
            let existing =
                self.try_acquire_singleton_slot(workflow_id, agent_id, revision_id, task_id)?;
            let Some(existing_id) = existing else {
                return Ok(None);
            };
            if self.singleton_indexed_task_is_terminal(workflow_id, &existing_id) {
                tracing::warn!(
                    target: "singleton_dedup",
                    workflow_id = %workflow_id,
                    agent_id = %agent_id,
                    stale_task_id = %existing_id,
                    "Singleton slot points at a terminal task; releasing the stale slot and retrying"
                );
                self.release_singleton_slot_by_task_id(workflow_id, &existing_id)?;
                continue;
            }
            return Ok(Some(existing_id));
        }
        // Another pass still found a terminal owner (or a concurrent healer
        // raced us); return the final state rather than looping.
        self.try_acquire_singleton_slot(workflow_id, agent_id, revision_id, task_id)
    }

    /// One upsert+select pass of [`Self::acquire_singleton_slot`], without the
    /// self-heal retry. Split out so the retry can re-run just this part.
    fn try_acquire_singleton_slot(
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

    /// True when the task a singleton slot points at can never run again.
    ///
    /// A missing row or a read error is **not** treated as stale: the task row
    /// is written before the slot is acquired in the spawn path, and a read we
    /// cannot perform must never hand the same singleton slot to two live
    /// tasks. Only a verified terminal status heals the slot.
    fn singleton_indexed_task_is_terminal(&self, workflow_id: &str, task_id: &str) -> bool {
        matches!(
            self.get_task_run(workflow_id, task_id),
            Ok(Some(task)) if task.status.is_terminal()
        )
    }

    /// Mark a singleton slot as running. Only updates the row matching
    /// `(workflow_id, agent_id, revision_id, task_id)` that is currently
    /// `pending`, preventing activation of a stale or replaced slot.
    pub fn activate_singleton_task(
        &self,
        workflow_id: &str,
        agent_id: &str,
        revision_id: Option<&str>,
        task_id: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let revision = revision_id.unwrap_or("");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE workflow_singleton_index SET status = 'running', updated_at = ?4
             WHERE workflow_id = ?1 AND agent_id = ?2 AND revision_id = ?3
               AND task_id = ?5 AND status = 'pending'",
            params![workflow_id, agent_id, revision, now, task_id],
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
    use autonoetic_types::workflow::{TaskRun, TaskRunStatus};

    fn open_memory_store() -> (GatewayStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        (store, dir)
    }

    fn task(workflow_id: &str, task_id: &str, status: TaskRunStatus) -> TaskRun {
        TaskRun {
            task_id: task_id.to_string(),
            workflow_id: workflow_id.to_string(),
            agent_id: "specialized_builder.default".to_string(),
            session_id: format!("root/{task_id}"),
            parent_session_id: "root".to_string(),
            status,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        }
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

    /// Regression for the `wf-bf15bb44` wedged promote: the slot holder reached
    /// a terminal state through a path that saved the task directly without
    /// releasing the slot. Acquisition must not dedup onto the corpse — it
    /// verifies the indexed task and heals the slot, or the whole workflow is
    /// permanently blocked (cancel and force-complete both refuse to help).
    #[test]
    fn singleton_slot_self_heals_when_indexed_task_is_terminal() {
        let (store, _dir) = open_memory_store();
        let wf = "wf-singleton-heal";

        store
            .upsert_task_run(&task(wf, "task-dead", TaskRunStatus::Running))
            .unwrap();
        assert!(store
            .acquire_singleton_slot(wf, "specialized_builder.default", None, "task-dead")
            .unwrap()
            .is_none());

        // While the holder is live the slot dedups as before.
        assert_eq!(
            store
                .acquire_singleton_slot(wf, "specialized_builder.default", None, "task-live")
                .unwrap(),
            Some("task-dead".to_string())
        );

        // A path that bypasses `update_task_run_status` (the P-7.16 orphan
        // reaper) terminally cancels the task without touching the index.
        store
            .upsert_task_run(&task(wf, "task-dead", TaskRunStatus::Cancelled))
            .unwrap();

        // The next acquisition heals the stale slot instead of wedging.
        assert!(
            store
                .acquire_singleton_slot(wf, "specialized_builder.default", None, "task-next")
                .unwrap()
                .is_none(),
            "a terminal singleton owner must be released, not returned"
        );
        assert_eq!(
            store
                .acquire_singleton_slot(wf, "specialized_builder.default", None, "task-other")
                .unwrap(),
            Some("task-next".to_string()),
            "after healing, the new holder owns the slot"
        );
    }

    /// A `Stale` owner is resumable (a late approval can revive it) and must
    /// keep its slot — self-healing is limited to truly terminal tasks.
    #[test]
    fn singleton_slot_keeps_a_stale_owner() {
        let (store, _dir) = open_memory_store();
        let wf = "wf-singleton-stale";

        store
            .upsert_task_run(&task(wf, "task-stale", TaskRunStatus::Stale))
            .unwrap();
        assert!(store
            .acquire_singleton_slot(wf, "specialized_builder.default", None, "task-stale")
            .unwrap()
            .is_none());

        assert_eq!(
            store
                .acquire_singleton_slot(wf, "specialized_builder.default", None, "task-new")
                .unwrap(),
            Some("task-stale".to_string()),
            "a resumable Stale owner must keep the slot"
        );
    }
}
