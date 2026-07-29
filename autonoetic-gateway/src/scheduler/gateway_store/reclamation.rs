use super::GatewayStore;
use anyhow::Result;
use rusqlite::params;

impl GatewayStore {
    /// Delete memories whose `expires_at` has passed.
    /// Also cleans up orphaned `memory_tags` rows.
    /// Returns the number of deleted memories.
    pub fn delete_expired_memories(&self, now: &chrono::DateTime<chrono::Utc>) -> Result<u64> {
        let now_rfc = now.to_rfc3339();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // Clean up memory_tags first (no FK cascade on memories DELETE)
        tx.execute(
            "DELETE FROM memory_tags WHERE memory_id IN (
               SELECT memory_id FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1
             )",
            params![now_rfc],
        )?;
        let n = tx.execute(
            "DELETE FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
            params![now_rfc],
        )?;
        tx.commit()?;
        Ok(n as u64)
    }

    /// Delete agent revisions in `Archived` status older than `max_age_days`.
    /// Skips revisions still referenced by `session_agent_bindings`.
    /// 0 = skip. Returns the number of deleted rows.
    pub fn delete_archived_revisions(
        &self,
        max_age_days: u64,
        now: &chrono::DateTime<chrono::Utc>,
    ) -> Result<u64> {
        if max_age_days == 0 {
            return Ok(0);
        }
        let cutoff = (*now - chrono::Duration::days(max_age_days as i64)).to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let sql = "DELETE FROM agent_revisions
                   WHERE status = 'Archived'
                     AND created_at < ?1
                     AND revision_id NOT IN (
                       SELECT revision_id FROM session_agent_bindings
                     )";
        let n = conn.execute(sql, params![cutoff])?;
        Ok(n as u64)
    }

    /// Mark sessions as closed if they are still `active` and their `started_at`
    /// is older than `max_age_days`. Sets `ended_at` to `now`.
    /// 0 = skip. Returns the number of updated rows.
    pub fn close_orphaned_sessions(
        &self,
        max_age_days: u64,
        now: &chrono::DateTime<chrono::Utc>,
    ) -> Result<u64> {
        if max_age_days == 0 {
            return Ok(0);
        }
        let cutoff = (*now - chrono::Duration::days(max_age_days as i64)).to_rfc3339();
        let now_rfc = now.to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let sql = "UPDATE session_transcripts SET status = 'closed', ended_at = ?1
                   WHERE status = 'active' AND started_at < ?2";
        let n = conn.execute(sql, params![now_rfc, cutoff])?;
        Ok(n as u64)
    }

    /// Cancel `active` scheduled jobs whose root session has been closed for
    /// more than `max_age_days`. Only closes transcripts representing root
    /// sessions (where `session_id = root_session_id`).
    /// 0 = skip. Returns the number of cancelled jobs.
    pub fn cancel_stale_jobs(
        &self,
        max_age_days: u64,
        now: &chrono::DateTime<chrono::Utc>,
    ) -> Result<u64> {
        if max_age_days == 0 {
            return Ok(0);
        }
        let cutoff = (*now - chrono::Duration::days(max_age_days as i64)).to_rfc3339();
        let now_rfc = now.to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let sql = "UPDATE scheduled_jobs SET status = 'cancelled', updated_at = ?1
                   WHERE status = 'active'
                     AND root_session_id IN (
                       SELECT root_session_id FROM session_transcripts
                       WHERE session_id = root_session_id
                         AND ended_at IS NOT NULL AND ended_at < ?2
                     )";
        let n = conn.execute(sql, params![now_rfc, cutoff])?;
        Ok(n as u64)
    }

    /// Collect all content handles referenced by `artifact_refs.artifact_digest`
    /// in the database. Used together with manifest scanning to compute the
    /// full set of referenced blobs.
    pub fn referenced_artifact_digests(
        &self,
    ) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT artifact_digest FROM artifact_refs WHERE revoked_at IS NULL
               AND (expires_at IS NULL OR expires_at > ?1)",
        )?;
        let now_rfc = chrono::Utc::now().to_rfc3339();
        let rows = stmt.query_map(params![now_rfc], |row| row.get::<_, String>(0))?;
        let mut digests = std::collections::HashSet::new();
        for r in rows {
            if let Ok(d) = r {
                digests.insert(d);
            }
        }
        Ok(digests)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::gateway_store::GatewayStore;
    use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};

    fn make_store() -> (GatewayStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn test_delete_expired_memories() {
        let (store, _dir) = make_store();
        let now = chrono::Utc::now();

        let past = (now - chrono::Duration::hours(1)).to_rfc3339();
        let future = (now + chrono::Duration::hours(1)).to_rfc3339();

        let expired = MemoryObject {
            memory_id: "mem-expired".into(),
            scope: "agent".into(),
            owner_agent_id: "agent-1".into(),
            writer_agent_id: "agent-1".into(),
            source_type: MemorySourceType::AgentWrite,
            source_ref: "ref-1".into(),
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
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
            egress_label: None,
        };
        store.memory_upsert(&expired).unwrap();

        let active = MemoryObject {
            memory_id: "mem-active".into(),
            scope: "agent".into(),
            owner_agent_id: "agent-1".into(),
            writer_agent_id: "agent-1".into(),
            source_type: MemorySourceType::AgentWrite,
            source_ref: "ref-2".into(),
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
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
            egress_label: None,
        };
        store.memory_upsert(&active).unwrap();

        let no_expiry = MemoryObject {
            memory_id: "mem-no-expiry".into(),
            scope: "agent".into(),
            owner_agent_id: "agent-1".into(),
            writer_agent_id: "agent-1".into(),
            source_type: MemorySourceType::AgentWrite,
            source_ref: "ref-3".into(),
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            content: "no-expiry".into(),
            content_hash: "hash3".into(),
            confidence: None,
            tags: vec![],
            lineage: vec![],
            visibility: MemoryVisibility::Private,
            expires_at: None,
            revision_id: None,
            binding_session_id: None,
            alias_ref: None,
            quarantine_reason: None,
            egress_label: None,
        };
        store.memory_upsert(&no_expiry).unwrap();

        let deleted = store.delete_expired_memories(&now).unwrap();
        assert_eq!(deleted, 1, "only the expired memory should be deleted");

        assert!(store.memory_get("mem-expired").unwrap().is_none());
        assert!(store.memory_get("mem-active").unwrap().is_some());
        assert!(store.memory_get("mem-no-expiry").unwrap().is_some());
    }

    #[test]
    fn test_delete_archived_revisions_zero_skips() {
        let (store, _dir) = make_store();
        let now = chrono::Utc::now();
        let deleted = store.delete_archived_revisions(0, &now).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn test_close_orphaned_sessions_zero_skips() {
        let (store, _dir) = make_store();
        let now = chrono::Utc::now();
        let closed = store.close_orphaned_sessions(0, &now).unwrap();
        assert_eq!(closed, 0);
    }

    #[test]
    fn test_cancel_stale_jobs_zero_skips() {
        let (store, _dir) = make_store();
        let now = chrono::Utc::now();
        let cancelled = store.cancel_stale_jobs(0, &now).unwrap();
        assert_eq!(cancelled, 0);
    }

    #[test]
    fn test_referenced_artifact_digests_empty() {
        let (store, _dir) = make_store();
        let digests = store.referenced_artifact_digests().unwrap();
        assert!(digests.is_empty());
    }
}
