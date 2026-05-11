use super::GatewayStore;
use anyhow::Result;
use rusqlite::params;

impl GatewayStore {
    /// Delete memories whose `expires_at` has passed.
    /// Returns the number of deleted rows.
    pub fn delete_expired_memories(&self) -> Result<u64> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let sql = "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1";
        let n = conn.execute(sql, params![now])?;
        Ok(n as u64)
    }

    /// Delete agent revisions in `Archived` status older than `max_age_days`.
    /// 0 = skip. Returns the number of deleted rows.
    pub fn delete_archived_revisions(&self, max_age_days: u64) -> Result<u64> {
        if max_age_days == 0 {
            return Ok(0);
        }
        let cutoff =
            (chrono::Utc::now() - chrono::Duration::days(max_age_days as i64)).to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let sql = "DELETE FROM agent_revisions WHERE status = 'Archived' AND created_at < ?1";
        let n = conn.execute(sql, params![cutoff])?;
        Ok(n as u64)
    }

    /// Mark sessions as closed if they are still `active` and their last activity
    /// (started_at) is older than `max_age_days`. 0 = skip.
    /// Returns the number of updated rows.
    pub fn close_orphaned_sessions(&self, max_age_days: u64) -> Result<u64> {
        if max_age_days == 0 {
            return Ok(0);
        }
        let cutoff =
            (chrono::Utc::now() - chrono::Duration::days(max_age_days as i64)).to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let sql = "UPDATE session_transcripts SET status = 'closed', ended_at = ?1
                   WHERE status = 'active' AND started_at < ?2";
        let n = conn.execute(sql, params![cutoff, cutoff])?;
        Ok(n as u64)
    }

    /// Cancel `active` scheduled jobs whose root session has been closed for
    /// more than `max_age_days`. 0 = skip. Returns the number of cancelled jobs.
    pub fn cancel_stale_jobs(&self, max_age_days: u64) -> Result<u64> {
        if max_age_days == 0 {
            return Ok(0);
        }
        let cutoff =
            (chrono::Utc::now() - chrono::Duration::days(max_age_days as i64)).to_rfc3339();
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let sql = "UPDATE scheduled_jobs SET status = 'cancelled', updated_at = ?1
                   WHERE status = 'active'
                     AND root_session_id IN (
                       SELECT root_session_id FROM session_transcripts
                       WHERE ended_at IS NOT NULL AND ended_at < ?2
                     )";
        let n = conn.execute(sql, params![now, cutoff])?;
        Ok(n as u64)
    }

    /// Count content blob references across all session manifests.
    /// Returns a set of content handles that are referenced by at least one manifest.
    pub fn referenced_content_handles(
        &self,
        sessions_dir: &std::path::Path,
    ) -> Result<std::collections::HashSet<String>> {
        let mut referenced = std::collections::HashSet::new();
        let sessions_dir = sessions_dir.to_path_buf();
        if !sessions_dir.exists() {
            return Ok(referenced);
        }
        for entry in std::fs::read_dir(&sessions_dir)? {
            let entry = entry?;
            let manifest_path = entry.path().join("manifest.json");
            if manifest_path.exists() {
                let content = std::fs::read_to_string(&manifest_path)?;
                if let Ok(manifest) =
                    serde_json::from_str::<serde_json::Value>(&content)
                {
                    if let Some(names) = manifest.get("names").and_then(|v| v.as_object()) {
                        for (_name, handle) in names {
                            if let Some(h) = handle.as_str() {
                                referenced.insert(h.to_string());
                            }
                        }
                    }
                }
            }
        }
        Ok(referenced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::gateway_store::GatewayStore;
    use autonoetic_types::agent_revision::AgentRevisionStatus;
    use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};

    fn make_store() -> (GatewayStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn test_delete_expired_memories() {
        let (store, _dir) = make_store();

        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let no_expiry: Option<String> = None;

        let expired = MemoryObject {
            memory_id: "mem-expired".into(),
            scope: "agent".into(),
            owner_agent_id: "agent-1".into(),
            writer_agent_id: "agent-1".into(),
            source_type: MemorySourceType::AgentWrite,
            source_ref: "ref-1".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            content: "expired".into(),
            content_hash: "hash1".into(),
            confidence: None,
            tags: vec![],
            lineage: vec![],
            visibility: MemoryVisibility::Private,
            expires_at: Some(past),
            revision_id: None,
            binding_session_id: None,
            alias_ref: None,
            quarantine_reason: None,
        };
        store.memory_upsert(&expired).unwrap();

        let active = MemoryObject {
            memory_id: "mem-active".into(),
            scope: "agent".into(),
            owner_agent_id: "agent-1".into(),
            writer_agent_id: "agent-1".into(),
            source_type: MemorySourceType::AgentWrite,
            source_ref: "ref-2".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            content: "active".into(),
            content_hash: "hash2".into(),
            confidence: None,
            tags: vec![],
            lineage: vec![],
            visibility: MemoryVisibility::Private,
            expires_at: Some(future),
            revision_id: None,
            binding_session_id: None,
            alias_ref: None,
            quarantine_reason: None,
        };
        store.memory_upsert(&active).unwrap();

        let no_expiry_mem = MemoryObject {
            memory_id: "mem-no-expiry".into(),
            scope: "agent".into(),
            owner_agent_id: "agent-1".into(),
            writer_agent_id: "agent-1".into(),
            source_type: MemorySourceType::AgentWrite,
            source_ref: "ref-3".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            content: "no-expiry".into(),
            content_hash: "hash3".into(),
            confidence: None,
            tags: vec![],
            lineage: vec![],
            visibility: MemoryVisibility::Private,
            expires_at: no_expiry,
            revision_id: None,
            binding_session_id: None,
            alias_ref: None,
            quarantine_reason: None,
        };
        store.memory_upsert(&no_expiry_mem).unwrap();

        let deleted = store.delete_expired_memories().unwrap();
        assert_eq!(deleted, 1, "only the expired memory should be deleted");

        assert!(store.memory_get("mem-expired").unwrap().is_none());
        assert!(store.memory_get("mem-active").unwrap().is_some());
        assert!(store.memory_get("mem-no-expiry").unwrap().is_some());
    }

    #[test]
    fn test_delete_archived_revisions_zero_skips() {
        let (store, _dir) = make_store();
        let deleted = store.delete_archived_revisions(0).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn test_close_orphaned_sessions_zero_skips() {
        let (store, _dir) = make_store();
        let closed = store.close_orphaned_sessions(0).unwrap();
        assert_eq!(closed, 0);
    }

    #[test]
    fn test_cancel_stale_jobs_zero_skips() {
        let (store, _dir) = make_store();
        let cancelled = store.cancel_stale_jobs(0).unwrap();
        assert_eq!(cancelled, 0);
    }

    #[test]
    fn test_referenced_content_handles_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _tmp) = make_store();
        let handles = store
            .referenced_content_handles(dir.path())
            .unwrap();
        assert!(handles.is_empty());
    }
}
