//! Parent-turn lineage for spawned child sessions — used by the Session Room
//! to label parallel sub-agent rows (e.g. `T3→coder` instead of child-local `T1`).

use anyhow::Result;
use autonoetic_types::session_timeline::SessionSpawnLineageEntry;
use rusqlite::params;

use super::GatewayStore;

impl GatewayStore {
    pub fn upsert_session_spawn_lineage(
        &self,
        child_session_id: &str,
        parent_session_id: &str,
        root_session_id: &str,
        spawned_at_turn: u64,
        target_agent_id: &str,
        created_at: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO session_spawn_lineage (
                child_session_id, parent_session_id, root_session_id,
                spawned_at_turn, target_agent_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                child_session_id,
                parent_session_id,
                root_session_id,
                spawned_at_turn as i64,
                target_agent_id,
                created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_session_spawn_lineage(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<SessionSpawnLineageEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT child_session_id, parent_session_id, spawned_at_turn, target_agent_id
             FROM session_spawn_lineage
             WHERE root_session_id = ?1
             ORDER BY spawned_at_turn ASC, child_session_id ASC",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            Ok(SessionSpawnLineageEntry {
                child_session_id: row.get(0)?,
                parent_session_id: row.get(1)?,
                spawned_at_turn: row.get::<_, i64>(2)? as u64,
                target_agent_id: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::gateway_store::GatewayStore;
    use tempfile::tempdir;

    #[test]
    fn upsert_and_list_spawn_lineage_by_root() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        store
            .upsert_session_spawn_lineage(
                "root/coder-abc",
                "root",
                "root",
                3,
                "coder.default",
                "2026-06-01T00:00:00Z",
            )
            .unwrap();
        store
            .upsert_session_spawn_lineage(
                "root/researcher-xyz",
                "root",
                "root",
                3,
                "researcher.default",
                "2026-06-01T00:00:01Z",
            )
            .unwrap();
        let listed = store.list_session_spawn_lineage("root").unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].spawned_at_turn, 3);
        assert_eq!(listed[0].target_agent_id, "coder.default");
        assert!(store.list_session_spawn_lineage("other").unwrap().is_empty());
    }
}
