use super::GatewayStore;
use anyhow::Result;
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus, PromotionKind, PromotionRecord,
    SessionAgentBinding,
};
use rusqlite::{params, OptionalExtension};

impl GatewayStore {
    pub fn insert_agent_revision(&self, rev: &AgentRevisionRecord) -> Result<()> {
        let _ = self.insert_agent_revision_transactional(rev)?;
        Ok(())
    }

    pub fn insert_agent_revision_transactional(&self, rev: &AgentRevisionRecord) -> Result<String> {
        let metadata_json = serde_json::to_string(&rev.metadata_json)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let short = if rev.short_id.trim().is_empty() {
            let mut stmt =
                tx.prepare("SELECT revision_id FROM agent_revisions WHERE revision_id != ?1")?;
            let rows = stmt.query_map(params![&rev.revision_id], |row| row.get::<_, String>(0))?;
            let mut existing = Vec::new();
            for row in rows {
                existing.push(row?);
            }
            autonoetic_types::agent_revision::short_id_unique(
                &rev.revision_id,
                existing.iter().map(|s| s.as_str()),
                None,
            )
        } else {
            rev.short_id.clone()
        };

        tx.execute(
                "INSERT INTO agent_revisions (
                    revision_id, agent_id, base_revision_id, artifact_id, content_digest,
                    runtime_lock_hash, manifest_hash, created_at, created_by_type, created_by_id,
                    source_kind, source_ref, origin_node_id, trust_domain, status, metadata_json,
                    short_id, signature, signer_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                params![
                    &rev.revision_id,
                    &rev.agent_id,
                    rev.base_revision_id,
                    rev.artifact_id,
                    &rev.content_digest,
                    &rev.runtime_lock_hash,
                    &rev.manifest_hash,
                    &rev.created_at,
                    &rev.created_by_type,
                    &rev.created_by_id,
                    &rev.source_kind,
                    rev.source_ref,
                    &rev.origin_node_id,
                    &rev.trust_domain,
                    &format!("{:?}", rev.status),
                    metadata_json,
                    &short,
                    rev.signature,
                    rev.signer_id,
                ],
            )?;
        let now = chrono::Utc::now().to_rfc3339();
        tx.execute(
            "INSERT OR IGNORE INTO short_id_index (short_id, revision_id, created_at)
             VALUES (?1, ?2, ?3)",
            params![&short, &rev.revision_id, now],
        )?;
        tx.commit()?;

        Ok(short)
    }

    pub fn get_agent_revision(&self, revision_id: &str) -> Result<Option<AgentRevisionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT revision_id, agent_id, base_revision_id, artifact_id, content_digest,
                    runtime_lock_hash, manifest_hash, created_at, created_by_type, created_by_id,
                    source_kind, source_ref, origin_node_id, trust_domain, status, metadata_json,
                    short_id, signature, signer_id
             FROM agent_revisions WHERE revision_id = ?1",
        )?;
        let rows = stmt.query_map(params![revision_id], |row| {
            let status_str: String = row.get(14)?;
            let status = match status_str.as_str() {
                "Candidate" => AgentRevisionStatus::Candidate,
                "Ready" => AgentRevisionStatus::Ready,
                "Archived" => AgentRevisionStatus::Archived,
                "Rejected" => AgentRevisionStatus::Rejected,
                _ => AgentRevisionStatus::Candidate,
            };
            let metadata_json: String = row.get(15)?;
            let metadata_json =
                serde_json::from_str(&metadata_json).unwrap_or(serde_json::Value::Null);
            let short_id: Option<String> = row.get(16).ok();
            Ok(AgentRevisionRecord {
                revision_id: row.get(0)?,
                agent_id: row.get(1)?,
                base_revision_id: row.get(2)?,
                artifact_id: row.get(3)?,
                content_digest: row.get(4)?,
                runtime_lock_hash: row.get(5)?,
                manifest_hash: row.get(6)?,
                created_at: row.get(7)?,
                created_by_type: row.get(8)?,
                created_by_id: row.get(9)?,
                source_kind: row.get(10)?,
                source_ref: row.get(11)?,
                origin_node_id: row.get(12)?,
                trust_domain: row.get(13)?,
                status,
                metadata_json,
                short_id: short_id.unwrap_or_default(),
                signature: row.get(17).ok().flatten(),
                signer_id: row.get(18).ok().flatten(),
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results.pop())
    }

    pub fn list_agent_revisions(&self, agent_id: &str) -> Result<Vec<AgentRevisionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT revision_id, agent_id, base_revision_id, artifact_id, content_digest,
                    runtime_lock_hash, manifest_hash, created_at, created_by_type, created_by_id,
                    source_kind, source_ref, origin_node_id, trust_domain, status, metadata_json,
                    short_id, signature, signer_id
             FROM agent_revisions WHERE agent_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![agent_id], |row| {
            let status_str: String = row.get(14)?;
            let status = match status_str.as_str() {
                "Candidate" => AgentRevisionStatus::Candidate,
                "Ready" => AgentRevisionStatus::Ready,
                "Archived" => AgentRevisionStatus::Archived,
                "Rejected" => AgentRevisionStatus::Rejected,
                _ => AgentRevisionStatus::Candidate,
            };
            let metadata_json: String = row.get(15)?;
            let short_id: Option<String> = row.get(16).ok();
            let metadata_json =
                serde_json::from_str(&metadata_json).unwrap_or(serde_json::Value::Null);
            Ok(AgentRevisionRecord {
                revision_id: row.get(0)?,
                agent_id: row.get(1)?,
                base_revision_id: row.get(2)?,
                artifact_id: row.get(3)?,
                content_digest: row.get(4)?,
                runtime_lock_hash: row.get(5)?,
                manifest_hash: row.get(6)?,
                created_at: row.get(7)?,
                created_by_type: row.get(8)?,
                created_by_id: row.get(9)?,
                source_kind: row.get(10)?,
                source_ref: row.get(11)?,
                origin_node_id: row.get(12)?,
                trust_domain: row.get(13)?,
                status,
                metadata_json,
                short_id: short_id.unwrap_or_default(),
                signature: row.get(17).ok().flatten(),
                signer_id: row.get(18).ok().flatten(),
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn list_all_agent_revisions(&self) -> Result<Vec<AgentRevisionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT revision_id, agent_id, base_revision_id, artifact_id, content_digest,
                    runtime_lock_hash, manifest_hash, created_at, created_by_type, created_by_id,
                    source_kind, source_ref, origin_node_id, trust_domain, status, metadata_json,
                    short_id, signature, signer_id
             FROM agent_revisions ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![], |row| {
            let status_str: String = row.get(14)?;
            let status = match status_str.as_str() {
                "Candidate" => AgentRevisionStatus::Candidate,
                "Ready" => AgentRevisionStatus::Ready,
                "Archived" => AgentRevisionStatus::Archived,
                "Rejected" => AgentRevisionStatus::Rejected,
                _ => AgentRevisionStatus::Candidate,
            };
            let metadata_json: String = row.get(15)?;
            let short_id: Option<String> = row.get(16).ok();
            let metadata_json =
                serde_json::from_str(&metadata_json).unwrap_or(serde_json::Value::Null);
            Ok(AgentRevisionRecord {
                revision_id: row.get(0)?,
                agent_id: row.get(1)?,
                base_revision_id: row.get(2)?,
                artifact_id: row.get(3)?,
                content_digest: row.get(4)?,
                runtime_lock_hash: row.get(5)?,
                manifest_hash: row.get(6)?,
                created_at: row.get(7)?,
                created_by_type: row.get(8)?,
                created_by_id: row.get(9)?,
                source_kind: row.get(10)?,
                source_ref: row.get(11)?,
                origin_node_id: row.get(12)?,
                trust_domain: row.get(13)?,
                status,
                metadata_json,
                short_id: short_id.unwrap_or_default(),
                signature: row.get(17).ok().flatten(),
                signer_id: row.get(18).ok().flatten(),
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn update_agent_revision_status(
        &self,
        revision_id: &str,
        status: AgentRevisionStatus,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE agent_revisions SET status = ?1 WHERE revision_id = ?2",
            params![&format!("{:?}", status), revision_id],
        )?;
        Ok(())
    }

    pub fn upsert_agent_alias(&self, alias: &AgentAliasRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO agent_aliases (
                alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &alias.alias_id,
                &alias.agent_id,
                &alias.revision_id,
                &alias.updated_at,
                &alias.updated_by_type,
                &alias.updated_by_id,
                alias.reason,
            ],
        )?;
        Ok(())
    }

    pub fn get_agent_alias(&self, alias_id: &str) -> Result<Option<AgentAliasRecord>> {
        self.resolve_alias(alias_id)
    }

    pub fn resolve_alias(&self, alias_id: &str) -> Result<Option<AgentAliasRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason
             FROM agent_aliases WHERE alias_id = ?1",
        )?;
        let rows = stmt.query_map(params![alias_id], |row| {
            Ok(AgentAliasRecord {
                alias_id: row.get(0)?,
                agent_id: row.get(1)?,
                revision_id: row.get(2)?,
                updated_at: row.get(3)?,
                updated_by_type: row.get(4)?,
                updated_by_id: row.get(5)?,
                reason: row.get(6)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results.pop())
    }

    pub fn list_agent_aliases(&self, filter: Option<&str>) -> Result<Vec<AgentAliasRecord>> {
        let conn = self.conn.lock().unwrap();
        if let Some(value) = filter {
            let mut stmt = conn.prepare(
                "SELECT alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason
                 FROM agent_aliases
                 WHERE agent_id = ?1 OR alias_id = ?1
                 ORDER BY agent_id ASC, alias_id ASC",
            )?;
            let rows = stmt.query_map(params![value], |row| {
                Ok(AgentAliasRecord {
                    alias_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    revision_id: row.get(2)?,
                    updated_at: row.get(3)?,
                    updated_by_type: row.get(4)?,
                    updated_by_id: row.get(5)?,
                    reason: row.get(6)?,
                })
            })?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row?);
            }
            return Ok(results);
        } else {
            let mut stmt = conn.prepare(
                "SELECT alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason
                 FROM agent_aliases
                 ORDER BY agent_id ASC, alias_id ASC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(AgentAliasRecord {
                    alias_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    revision_id: row.get(2)?,
                    updated_at: row.get(3)?,
                    updated_by_type: row.get(4)?,
                    updated_by_id: row.get(5)?,
                    reason: row.get(6)?,
                })
            })?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row?);
            }
            return Ok(results);
        }
    }

    pub fn upsert_session_agent_binding(&self, binding: &SessionAgentBinding) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO session_agent_bindings (
                session_id, root_session_id, alias_id, agent_id, revision_id,
                runtime_lock_hash, home_node_id, created_at, requested_target
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &binding.session_id,
                &binding.root_session_id,
                &binding.alias_id,
                &binding.agent_id,
                &binding.revision_id,
                &binding.runtime_lock_hash,
                &binding.home_node_id,
                &binding.created_at,
                &binding.requested_target,
            ],
        )?;
        Ok(())
    }

    pub fn get_session_agent_binding(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionAgentBinding>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, root_session_id, alias_id, agent_id, revision_id,
                    runtime_lock_hash, home_node_id, created_at, requested_target
             FROM session_agent_bindings WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(SessionAgentBinding {
                session_id: row.get(0)?,
                root_session_id: row.get(1)?,
                alias_id: row.get(2)?,
                agent_id: row.get(3)?,
                revision_id: row.get(4)?,
                runtime_lock_hash: row.get(5)?,
                home_node_id: row.get(6)?,
                created_at: row.get(7)?,
                requested_target: row.get(8)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results.pop())
    }

    pub fn list_sessions_for_agent(&self, agent_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT session_id FROM session_agent_bindings WHERE agent_id = ?1")?;
        let rows = stmt.query_map(params![agent_id], |row| row.get(0))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn insert_promotion_record(&self, record: &PromotionRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO promotion_history (
                promotion_id, kind, alias_id, agent_id, previous_revision_id,
                new_revision_id, source_eval_run_id, reason, created_at,
                created_by_type, created_by_id, origin_node_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                &record.promotion_id,
                &format!("{:?}", record.kind),
                &record.alias_id,
                &record.agent_id,
                record.previous_revision_id,
                &record.new_revision_id,
                record.source_eval_run_id,
                record.reason,
                &record.created_at,
                &record.created_by_type,
                &record.created_by_id,
                &record.origin_node_id,
            ],
        )?;
        Ok(())
    }

    pub fn atomic_promote(
        &self,
        agent_id: &str,
        revision_id: &str,
        promotion_id: &str,
        created_by_type: &str,
        created_by_id: &str,
        reason: Option<&str>,
        source_eval_run_id: Option<&str>,
    ) -> Result<Option<String>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let previous_revision_id: Option<String> = tx
            .query_row(
                "SELECT revision_id FROM agent_aliases WHERE alias_id = ?1 AND agent_id = ?2",
                params![agent_id, agent_id],
                |row| row.get(0),
            )
            .optional()?;

        let (rev_agent_id, rev_status): (String, String) = tx.query_row(
            "SELECT agent_id, status FROM agent_revisions WHERE revision_id = ?1",
            params![revision_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        anyhow::ensure!(
            rev_agent_id == agent_id,
            "Revision '{}' belongs to '{}', not '{}'",
            revision_id,
            rev_agent_id,
            agent_id
        );
        anyhow::ensure!(
            rev_status == "Candidate" || rev_status == "Ready",
            "Revision '{}' is in status '{}', must be Candidate or Ready",
            revision_id,
            rev_status
        );

        let now = chrono::Utc::now().to_rfc3339();
        tx.execute(
            "INSERT OR REPLACE INTO agent_aliases (
                alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                agent_id,
                agent_id,
                revision_id,
                now,
                created_by_type,
                created_by_id,
                reason
            ],
        )?;

        tx.execute(
            "UPDATE agent_revisions SET status = 'Ready' WHERE revision_id = ?1",
            params![revision_id],
        )?;

        if let Some(ref prev) = previous_revision_id {
            if prev != revision_id {
                tx.execute(
                    "UPDATE agent_revisions SET status = 'Archived' WHERE revision_id = ?1",
                    params![prev],
                )?;
            }
        }

        tx.execute(
            "INSERT INTO promotion_history (
                promotion_id, kind, alias_id, agent_id, previous_revision_id,
                new_revision_id, source_eval_run_id, reason, created_at,
                created_by_type, created_by_id, origin_node_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                promotion_id,
                "Promote",
                agent_id,
                agent_id,
                previous_revision_id,
                revision_id,
                source_eval_run_id,
                reason,
                chrono::Utc::now().to_rfc3339(),
                created_by_type,
                created_by_id,
                "gateway",
            ],
        )?;

        tx.commit()?;
        Ok(previous_revision_id)
    }

    pub fn atomic_rollback(
        &self,
        agent_id: &str,
        revision_id: &str,
        promotion_id: &str,
        created_by_type: &str,
        created_by_id: &str,
        reason: Option<&str>,
    ) -> Result<Option<String>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let current_revision_id: Option<String> = tx
            .query_row(
                "SELECT revision_id FROM agent_aliases WHERE alias_id = ?1 AND agent_id = ?2",
                params![agent_id, agent_id],
                |row| row.get(0),
            )
            .optional()?;

        let (rev_agent_id, _rev_status): (String, String) = tx.query_row(
            "SELECT agent_id, status FROM agent_revisions WHERE revision_id = ?1",
            params![revision_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        anyhow::ensure!(
            rev_agent_id == agent_id,
            "Revision '{}' belongs to '{}', not '{}'",
            revision_id,
            rev_agent_id,
            agent_id
        );

        let now = chrono::Utc::now().to_rfc3339();
        tx.execute(
            "INSERT OR REPLACE INTO agent_aliases (
                alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                agent_id,
                agent_id,
                revision_id,
                now,
                created_by_type,
                created_by_id,
                reason
            ],
        )?;

        tx.execute(
            "UPDATE agent_revisions SET status = 'Ready' WHERE revision_id = ?1",
            params![revision_id],
        )?;

        if let Some(ref curr) = current_revision_id {
            if curr != revision_id {
                tx.execute(
                    "UPDATE agent_revisions SET status = 'Archived' WHERE revision_id = ?1",
                    params![curr],
                )?;

                let quarantine_reason = format!("revision_rollback:{}", curr);
                tx.execute(
                    "UPDATE memories SET quarantine_reason = ?1 WHERE revision_id = ?2 AND quarantine_reason IS NULL",
                    params![&quarantine_reason, curr],
                )?;
            }
        }

        tx.execute(
            "INSERT INTO promotion_history (
                promotion_id, kind, alias_id, agent_id, previous_revision_id,
                new_revision_id, source_eval_run_id, reason, created_at,
                created_by_type, created_by_id, origin_node_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                promotion_id,
                "Rollback",
                agent_id,
                agent_id,
                current_revision_id,
                revision_id,
                None as Option<&str>,
                reason,
                chrono::Utc::now().to_rfc3339(),
                created_by_type,
                created_by_id,
                "gateway",
            ],
        )?;

        tx.commit()?;
        Ok(current_revision_id)
    }

    /// Count `Promote` (not `Rollback`) entries for an alias whose
    /// `created_at >= since_rfc3339`. Used by the promotion safety governor
    /// (issue #25) for the per-alias velocity check.
    pub fn count_promotions_since(
        &self,
        agent_id: &str,
        since_rfc3339: &str,
    ) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM promotion_history
             WHERE agent_id = ?1 AND kind = 'Promote' AND created_at >= ?2",
            params![agent_id, since_rfc3339],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    pub fn list_promotion_history(&self, agent_id: &str) -> Result<Vec<PromotionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT promotion_id, kind, alias_id, agent_id, previous_revision_id,
                    new_revision_id, source_eval_run_id, reason, created_at,
                    created_by_type, created_by_id, origin_node_id
             FROM promotion_history WHERE agent_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![agent_id], |row| {
            let kind_str: String = row.get(1)?;
            let kind = match kind_str.as_str() {
                "Promote" => PromotionKind::Promote,
                "Rollback" => PromotionKind::Rollback,
                _ => PromotionKind::Promote,
            };
            Ok(PromotionRecord {
                promotion_id: row.get(0)?,
                kind,
                alias_id: row.get(2)?,
                agent_id: row.get(3)?,
                previous_revision_id: row.get(4)?,
                new_revision_id: row.get(5)?,
                source_eval_run_id: row.get(6)?,
                reason: row.get(7)?,
                created_at: row.get(8)?,
                created_by_type: row.get(9)?,
                created_by_id: row.get(10)?,
                origin_node_id: row.get(11)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn register_short_id(&self, revision_id: &str, short_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR IGNORE INTO short_id_index (short_id, revision_id, created_at)
             VALUES (?1, ?2, ?3)",
            params![short_id, revision_id, now],
        )?;
        Ok(())
    }

    pub fn lookup_short_id(&self, short_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT revision_id FROM short_id_index WHERE short_id = ?1")?;
        let result = stmt
            .query_row(params![short_id], |row| row.get(0))
            .optional()?;
        Ok(result)
    }

    pub fn list_short_ids_for_agent(&self, agent_id: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT si.short_id, si.revision_id
             FROM short_id_index si
             JOIN agent_revisions ar ON si.revision_id = ar.revision_id
             WHERE ar.agent_id = ?1
             ORDER BY ar.created_at DESC",
        )?;
        let rows = stmt.query_map(params![agent_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }
}
