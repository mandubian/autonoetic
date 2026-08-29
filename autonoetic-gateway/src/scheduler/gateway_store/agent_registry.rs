use super::GatewayStore;
use anyhow::Result;
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus, PromotionKind, PromotionRecord,
    SessionAgentBinding,
};
use rusqlite::{params, OptionalExtension};

/// A successfully promoted agent, surfaced for session-context fact injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedAgent {
    pub agent_id: String,
    pub revision_id: String,
    pub created_at: String,
}

const AGENT_REVISION_SELECT: &str = "SELECT revision_id, agent_id, base_revision_id, artifact_id, content_digest,
                    runtime_lock_hash, manifest_hash, created_at, created_by_type, created_by_id,
                    source_kind, source_ref, origin_node_id, trust_domain, status, metadata_json,
                    short_id, signature, signer_id, detected_network_hosts,
                    requested_by_type, requested_by_id
             FROM agent_revisions";

fn map_agent_revision_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentRevisionRecord> {
    let status_str: String = row.get(14)?;
    // Fail loud on anything but the three live states (#649): defaulting an
    // unknown/legacy status to `Candidate` would silently make a rejected or
    // corrupt revision promotable again.
    let status = match status_str.as_str() {
        "Candidate" => AgentRevisionStatus::Candidate,
        "Ready" => AgentRevisionStatus::Ready,
        "Archived" => AgentRevisionStatus::Archived,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                14,
                rusqlite::types::Type::Text,
                format!("unknown agent revision status '{other}' ('Rejected' was removed by #649)")
                    .into(),
            ));
        }
    };
    let metadata_json: String = row.get(15)?;
    let metadata_json = serde_json::from_str(&metadata_json).unwrap_or(serde_json::Value::Null);
    let short_id: Option<String> = row.get(16).ok();
    let detected_network_hosts_raw: Option<String> = row.get(19)?;
    let detected_network_hosts = match detected_network_hosts_raw {
        None => None,
        Some(raw) if raw.trim().is_empty() => None,
        Some(raw) => Some(serde_json::from_str(&raw).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                19,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?),
    };
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
        detected_network_hosts,
        requested_by_type: row.get(20).ok().flatten(),
        requested_by_id: row.get(21).ok().flatten(),
    })
}

impl GatewayStore {
    pub fn insert_agent_revision(&self, rev: &AgentRevisionRecord) -> Result<()> {
        let _ = self.insert_agent_revision_transactional(rev)?;
        Ok(())
    }

    /// Insert a revision, idempotently (#651): `revision_id` is
    /// content-addressed, so a concurrent duplicate create of the same
    /// content is the *same* revision. The insert uses
    /// `ON CONFLICT(revision_id) DO NOTHING`; `Ok(None)` means the row
    /// already won a race and the caller should take its `already_exists`
    /// path (re-reading for the short id) instead of surfacing a
    /// UNIQUE-constraint error and burning a retry.
    pub fn insert_agent_revision_transactional(
        &self,
        rev: &AgentRevisionRecord,
    ) -> Result<Option<String>> {
        let metadata_json = serde_json::to_string(&rev.metadata_json)?;
        let detected_network_hosts = rev
            .detected_network_hosts
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
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

        let inserted = tx.execute(
                "INSERT INTO agent_revisions (
                    revision_id, agent_id, base_revision_id, artifact_id, content_digest,
                    runtime_lock_hash, manifest_hash, created_at, created_by_type, created_by_id,
                    source_kind, source_ref, origin_node_id, trust_domain, status, metadata_json,
                    short_id, signature, signer_id, detected_network_hosts,
                    requested_by_type, requested_by_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
                ON CONFLICT(revision_id) DO NOTHING",
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
                    detected_network_hosts,
                    rev.requested_by_type,
                    rev.requested_by_id,
                ],
            )?;
        if inserted == 0 {
            // Lost a concurrent create of the same content-addressed id: the
            // winner's row is identical by construction, so its
            // short_id_index entry stays authoritative. `None` signals the
            // caller to take its `already_exists` path and re-read the
            // winner's short id itself.
            tx.commit()?;
            return Ok(None);
        }
        let now = chrono::Utc::now().to_rfc3339();
        tx.execute(
            "INSERT OR IGNORE INTO short_id_index (short_id, revision_id, created_at)
             VALUES (?1, ?2, ?3)",
            params![&short, &rev.revision_id, now],
        )?;
        tx.commit()?;

        Ok(Some(short))
    }

    pub fn get_agent_revision(&self, revision_id: &str) -> Result<Option<AgentRevisionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{AGENT_REVISION_SELECT} WHERE revision_id = ?1"))?;
        let rows = stmt.query_map(params![revision_id], map_agent_revision_row)?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results.pop())
    }

    pub fn list_agent_revisions(&self, agent_id: &str) -> Result<Vec<AgentRevisionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare(&format!("{AGENT_REVISION_SELECT} WHERE agent_id = ?1 ORDER BY created_at DESC"))?;
        let rows = stmt.query_map(params![agent_id], map_agent_revision_row)?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn list_all_agent_revisions(&self) -> Result<Vec<AgentRevisionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{AGENT_REVISION_SELECT} ORDER BY created_at DESC"))?;
        let rows = stmt.query_map(params![], map_agent_revision_row)?;
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
                alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason,
                suspended_at, suspended_reason, suspended_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                &alias.alias_id,
                &alias.agent_id,
                &alias.revision_id,
                &alias.updated_at,
                &alias.updated_by_type,
                &alias.updated_by_id,
                alias.reason,
                alias.suspended_at,
                alias.suspended_reason,
                alias.suspended_by,
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
            "SELECT alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason,
                    suspended_at, suspended_reason, suspended_by
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
                suspended_at: row.get(7)?,
                suspended_reason: row.get(8)?,
                suspended_by: row.get(9)?,
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
                "SELECT alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason,
                        suspended_at, suspended_reason, suspended_by
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
                    suspended_at: row.get(7)?,
                    suspended_reason: row.get(8)?,
                    suspended_by: row.get(9)?,
                })
            })?;
            let mut results = Vec::new();
            for row in rows {
                results.push(row?);
            }
            return Ok(results);
        } else {
            let mut stmt = conn.prepare(
                "SELECT alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason,
                        suspended_at, suspended_reason, suspended_by
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
                    suspended_at: row.get(7)?,
                    suspended_reason: row.get(8)?,
                    suspended_by: row.get(9)?,
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
                runtime_lock_hash, home_node_id, created_at, requested_target,
                constitution_version, constitution_digest
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                &binding.constitution_version,
                &binding.constitution_digest,
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
                    runtime_lock_hash, home_node_id, created_at, requested_target,
                    constitution_version, constitution_digest
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
                constitution_version: row.get(9)?,
                constitution_digest: row.get(10)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results.pop())
    }

    /// Every session ever bound to `agent_id`, finished or not.
    ///
    /// `session_agent_bindings` is append-only — one row per session, never
    /// deleted (reclamation treats a binding as a live revision reference). So
    /// this is a historical index, suitable for retrospective analysis
    /// (`autonoetic improve --last-sessions`). It cannot answer "who can
    /// receive a message right now": use
    /// [`Self::list_unfinished_sessions_for_agent`] for that.
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

    /// Sessions of `agent_id` that can actually receive an `agent_message`:
    /// parked resident sessions, plus sessions still mid-execution.
    ///
    /// Residency is the reliable half — it is written when a session parks and
    /// deleted when it resumes or is reaped, so it states reachability rather
    /// than inferring it. [`Self::list_unfinished_sessions_for_agent`] is kept
    /// as the second half for sessions that are currently executing (no outcome
    /// row yet); note it cannot see a *suspended* session, because the outcome
    /// row is written at the first finalize even for suspensions.
    pub fn list_addressable_sessions_for_agent(&self, agent_id: &str) -> Result<Vec<String>> {
        let mut sessions = self.list_resident_sessions_for_agent(agent_id)?;
        // Set-based dedup: a broadcast resolves against this, and an agent with
        // many sessions would otherwise pay a quadratic scan per send.
        let mut seen: std::collections::HashSet<String> = sessions.iter().cloned().collect();
        for sid in self.list_unfinished_sessions_for_agent(agent_id)? {
            if seen.insert(sid.clone()) {
                sessions.push(sid);
            }
        }
        Ok(sessions)
    }

    /// Sessions bound to `agent_id` that have not reached a terminal state —
    /// the plausible recipients of an `agent_message` broadcast.
    ///
    /// A `session_outcomes` row is written unconditionally at session close
    /// (`session_outcome_writer::write_session_outcome_metrics` — "every
    /// session gets a row"), and `updated_at` is refreshed at *each* close.
    ///
    /// Presence alone is **not** terminality (#1231). A room or root session
    /// closes and wakes again on every operator turn, and the row is never
    /// removed, so reading presence as "finished" made every such session
    /// permanently unaddressable while it was still running. The observed case:
    /// a root session whose outcome row was written at 13:40 went on to spawn
    /// five children and send a peer message of its own at 14:25.
    ///
    /// A session is unfinished when `session_liveness` says it has not stopped
    /// for good: never closed, closed *as a suspension* (a yield point it will
    /// resume from), or woken again since it last closed.
    ///
    /// Sessions with no `session_liveness` row predate the ledger, so they fall
    /// back to the original outcome-row test. That keeps historical rows
    /// behaving exactly as before rather than reclassifying them en masse on
    /// migration.
    ///
    /// Residual case: a session killed without a clean close leaves no outcome
    /// row and stays listed here. Messaging it queues a delivery that is never
    /// consumed — the same observable failure as messaging a hung session, and
    /// strictly better than counting every historical session as a live
    /// recipient.
    pub fn list_unfinished_sessions_for_agent(&self, agent_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT b.session_id
             FROM session_agent_bindings b
             LEFT JOIN session_outcomes o ON o.session_id = b.session_id
             LEFT JOIN session_liveness l ON l.session_id = b.session_id
             WHERE b.agent_id = ?1 AND (
                 CASE
                     WHEN l.session_id IS NULL      THEN o.session_id IS NULL
                     WHEN l.last_closed_at IS NULL  THEN 1
                     WHEN l.resumable = 1           THEN 1
                     WHEN l.last_woken_at IS NOT NULL
                          AND l.last_woken_at > l.last_closed_at THEN 1
                     ELSE 0
                 END
             )
             ORDER BY b.created_at ASC",
        )?;
        let rows = stmt.query_map(params![agent_id], |row| row.get(0))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    /// Record that `session_id` has begun executing.
    ///
    /// Called from the lifecycle's wake path rather than from the transcript
    /// upsert on purpose: the transcript is written by observability paths that
    /// can arrive late or duplicated (the very thing 125485f5 hardened
    /// against), whereas this fires where a session genuinely starts a turn.
    pub fn record_session_wake(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_liveness (session_id, last_woken_at) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET last_woken_at = excluded.last_woken_at",
            params![session_id, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Record that `session_id` has stopped, and whether it can resume.
    ///
    /// `resumable` is `SessionCloseOutcome::is_suspended()` — a gate, child
    /// wait or user-input yield parks the session rather than ending it, and a
    /// parked session still consumes its queued deliveries when it resumes.
    pub fn record_session_close(&self, session_id: &str, resumable: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_liveness (session_id, last_closed_at, resumable)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                last_closed_at = excluded.last_closed_at,
                resumable = excluded.resumable",
            params![
                session_id,
                chrono::Utc::now().to_rfc3339(),
                if resumable { 1_i64 } else { 0_i64 }
            ],
        )?;
        Ok(())
    }

    /// Whether `session_id` can still consume a delivery — the single-session
    /// counterpart of [`Self::list_addressable_sessions_for_agent`], and it must
    /// stay in lockstep with it: if the direct check were stricter than the
    /// broadcast filter, a role broadcast would reach sessions a direct send
    /// could not, turning the narrower mode into an evasion of the broader one.
    ///
    /// That lockstep is why residency is the first clause. A parked resident
    /// session took a clean-completion close, so the ledger alone reads
    /// "closed, not resumable" — but parking exists precisely so peers can
    /// still reach an agent that finished its task, and the broadcast filter
    /// unions residency in. Consulting only the ledger here made the same
    /// session reachable by role broadcast and not by its own session id.
    /// Expiry is honoured, so a lapsed residency does not keep a dead session
    /// alive.
    pub fn is_session_addressable(&self, session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let addressable: bool = conn.query_row(
            "SELECT CASE
                 WHEN r.session_id IS NOT NULL  THEN 1
                 WHEN l.session_id IS NULL      THEN o.session_id IS NULL
                 WHEN l.last_closed_at IS NULL  THEN 1
                 WHEN l.resumable = 1           THEN 1
                 WHEN l.last_woken_at IS NOT NULL
                      AND l.last_woken_at > l.last_closed_at THEN 1
                 ELSE 0
             END
             FROM (SELECT ?1 AS session_id) s
             LEFT JOIN session_outcomes o ON o.session_id = s.session_id
             LEFT JOIN session_liveness l ON l.session_id = s.session_id
             LEFT JOIN session_residency r
                 ON r.session_id = s.session_id AND r.expires_at > ?2",
            params![session_id, now],
            |row| row.get::<_, i64>(0).map(|v| v == 1),
        )?;
        Ok(addressable)
    }

    pub fn insert_promotion_record(&self, record: &PromotionRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO promotion_history (
                promotion_id, kind, alias_id, agent_id, previous_revision_id,
                new_revision_id, source_eval_run_id, reason, created_at,
                created_by_type, created_by_id, origin_node_id, pre_authorization
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                record.pre_authorization,
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
        pre_authorization: Option<&str>,
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

        // Operator re-promotion clears suspension — re-promotion = implicit
        // unsuspend. But an envelope-pre-authorized promotion is automatic (no
        // fresh operator decision), so it must NOT silently reactivate a
        // suspended agent: preserve the suspension in that case.
        let (suspended_at, suspended_reason, suspended_by): (
            Option<String>,
            Option<String>,
            Option<String>,
        ) = if pre_authorization.is_some() {
            tx.query_row(
                "SELECT suspended_at, suspended_reason, suspended_by FROM agent_aliases WHERE alias_id = ?1",
                params![agent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .unwrap_or((None, None, None))
        } else {
            (None, None, None)
        };
        tx.execute(
            "INSERT OR REPLACE INTO agent_aliases (
                alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason,
                suspended_at, suspended_reason, suspended_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                agent_id,
                agent_id,
                revision_id,
                now,
                created_by_type,
                created_by_id,
                reason,
                suspended_at,
                suspended_reason,
                suspended_by,
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
                created_by_type, created_by_id, origin_node_id, pre_authorization
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                pre_authorization,
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
        // Preserve existing suspension state during rollback — do not auto-unsuspend.
        let (suspended_at, suspended_reason, suspended_by): (Option<String>, Option<String>, Option<String>) = tx
            .query_row(
                "SELECT suspended_at, suspended_reason, suspended_by FROM agent_aliases WHERE alias_id = ?1",
                params![agent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .unwrap_or((None, None, None));
        tx.execute(
            "INSERT OR REPLACE INTO agent_aliases (
                alias_id, agent_id, revision_id, updated_at, updated_by_type, updated_by_id, reason,
                suspended_at, suspended_reason, suspended_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                agent_id,
                agent_id,
                revision_id,
                now,
                created_by_type,
                created_by_id,
                reason,
                suspended_at,
                suspended_reason,
                suspended_by,
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
                created_by_type, created_by_id, origin_node_id, pre_authorization
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                None::<&str>, // pre_authorization — rollbacks don't use this
            ],
        )?;

        tx.commit()?;
        Ok(current_revision_id)
    }

    /// Suspend an agent by setting its suspension fields. Use `agent.suspend` RPC.
    pub fn suspend_agent(
        &self,
        agent_id: &str,
        suspended_by: &str,
        reason: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let updated = conn.execute(
            "UPDATE agent_aliases SET
                suspended_at = ?1,
                suspended_reason = ?2,
                suspended_by = ?3
             WHERE alias_id = ?4 AND suspended_at IS NULL",
            params![now, reason, suspended_by, agent_id],
        )?;
        Ok(updated > 0)
    }

    /// Clear suspension fields on an agent. Use `agent.unsuspend` RPC.
    pub fn unsuspend_agent(&self, agent_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE agent_aliases SET
                suspended_at = NULL,
                suspended_reason = NULL,
                suspended_by = NULL
             WHERE alias_id = ?1 AND suspended_at IS NOT NULL",
            params![agent_id],
        )?;
        Ok(updated > 0)
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

    /// Bounded variant of [`list_promotion_history`](Self::list_promotion_history).
    /// Returns at most `limit` newest rows for the alias. Used by the promotion
    /// safety governor to avoid loading unbounded history for long-lived agents.
    pub fn list_recent_promotion_history(
        &self,
        agent_id: &str,
        limit: usize,
    ) -> Result<Vec<PromotionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT promotion_id, kind, alias_id, agent_id, previous_revision_id,
                    new_revision_id, source_eval_run_id, reason, created_at,
                    created_by_type, created_by_id, origin_node_id, pre_authorization
             FROM promotion_history WHERE agent_id = ?1 ORDER BY created_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![agent_id, limit as i64], |row| {
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
                pre_authorization: row.get(12)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn list_promotion_history(&self, agent_id: &str) -> Result<Vec<PromotionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT promotion_id, kind, alias_id, agent_id, previous_revision_id,
                    new_revision_id, source_eval_run_id, reason, created_at,
                    created_by_type, created_by_id, origin_node_id, pre_authorization
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
                pre_authorization: row.get(12)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    // ── Promotion attempt ledger (issue #720) ──────────────────────────

    /// Record one terminal outcome of a promotion attempt. `outcome` is either
    /// `'rejected'` (a gate blocked the promote) or `'promoted'` (the alias was
    /// updated). The ledger is keyed by `content_digest` so a rebuilt identical
    /// revision shares the same attempt budget.
    pub fn record_promotion_attempt(
        &self,
        attempt_id: &str,
        alias_id: &str,
        revision_id: &str,
        content_digest: &str,
        outcome: &str,
        gate: Option<&str>,
        error_code: Option<&str>,
        session_id: Option<&str>,
        workflow_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO promotion_attempts (
                attempt_id, alias_id, revision_id, content_digest, outcome,
                gate, error_code, session_id, workflow_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                attempt_id,
                alias_id,
                revision_id,
                content_digest,
                outcome,
                gate,
                error_code,
                session_id,
                workflow_id,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Count rejected promotion attempts for `(alias_id, content_digest)`.
    /// Used by the attempt-exhaustion governor check (issue #720).
    pub fn count_promotion_attempt_rejections(
        &self,
        alias_id: &str,
        content_digest: &str,
    ) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM promotion_attempts
             WHERE alias_id = ?1 AND content_digest = ?2 AND outcome = 'rejected'",
            params![alias_id, content_digest],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Reset the rejected-attempt counter for `(alias_id, content_digest)`.
    /// Called when an operator approves the `RevisionPromote` ack for this
    /// revision, or resolves an exhaustion escalation.
    pub fn reset_promotion_attempts(
        &self,
        alias_id: &str,
        content_digest: &str,
    ) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM promotion_attempts
             WHERE alias_id = ?1 AND content_digest = ?2 AND outcome = 'rejected'",
            params![alias_id, content_digest],
        )?;
        Ok(deleted)
    }

    /// List agents successfully promoted in `root_session_id` or any of its
    /// child sessions (`root_session_id/...`). Returns rows ordered oldest
    /// first so callers can identify the most recent promotion. Used to seed
    /// durable session-context facts so the planner retains conversational
    /// referents (e.g. "it") across session finalization.
    pub fn list_promoted_agents_by_root_session(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<PromotedAgent>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT alias_id, revision_id, created_at
             FROM promotion_attempts
             WHERE outcome = 'promoted'
               AND (session_id = ?1 OR session_id LIKE ?1 || '/%')
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            Ok(PromotedAgent {
                agent_id: row.get(0)?,
                revision_id: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Transactional variant of `record_promotion_attempt`. The caller must
    /// commit `tx` for the row to persist.
    pub fn record_promotion_attempt_in_tx(
        tx: &rusqlite::Transaction,
        attempt_id: &str,
        alias_id: &str,
        revision_id: &str,
        content_digest: &str,
        outcome: &str,
        gate: Option<&str>,
        error_code: Option<&str>,
        session_id: Option<&str>,
        workflow_id: Option<&str>,
    ) -> Result<()> {
        tx.execute(
            "INSERT INTO promotion_attempts (
                attempt_id, alias_id, revision_id, content_digest, outcome,
                gate, error_code, session_id, workflow_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                attempt_id,
                alias_id,
                revision_id,
                content_digest,
                outcome,
                gate,
                error_code,
                session_id,
                workflow_id,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Transactional variant used when the caller already holds a rusqlite
    /// transaction and wants the count read serialized with other writes.
    pub fn count_promotion_attempt_rejections_in_tx(
        tx: &rusqlite::Transaction,
        alias_id: &str,
        content_digest: &str,
    ) -> Result<usize> {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM promotion_attempts
             WHERE alias_id = ?1 AND content_digest = ?2 AND outcome = 'rejected'",
            params![alias_id, content_digest],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Record a rejected promotion attempt transactionally, serializing the
    /// rejection-count read with the insert to close the concurrent-promote
    /// TOCTOU window (issue #720). Returns the current rejection count if the
    /// cap is already reached (the insert is not performed), otherwise `None`.
    pub fn record_rejected_promotion_attempt(
        &self,
        alias_id: &str,
        revision_id: &str,
        content_digest: &str,
        gate: Option<&str>,
        error_code: Option<&str>,
        session_id: Option<&str>,
        workflow_id: Option<&str>,
        max_attempts: usize,
    ) -> Result<Option<usize>> {
        if max_attempts == 0 {
            // Cap disabled: record without checking.
            let attempt_id = format!("patt-{}", uuid::Uuid::new_v4());
            self.record_promotion_attempt(
                &attempt_id,
                alias_id,
                revision_id,
                content_digest,
                "rejected",
                gate,
                error_code,
                session_id,
                workflow_id,
            )?;
            return Ok(None);
        }

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let count = Self::count_promotion_attempt_rejections_in_tx(&tx, alias_id, content_digest)?;
        if count >= max_attempts {
            tx.rollback()?;
            return Ok(Some(count));
        }
        let attempt_id = format!("patt-{}", uuid::Uuid::new_v4());
        Self::record_promotion_attempt_in_tx(
            &tx,
            &attempt_id,
            alias_id,
            revision_id,
            content_digest,
            "rejected",
            gate,
            error_code,
            session_id,
            workflow_id,
        )?;
        tx.commit()?;
        Ok(None)
    }

    /// Find agent IDs that were promoted under a given session envelope
    /// (by matching `pre_authorization` JSON containing the envelope_id).
    /// Returns `(agent_id, promotion_id, created_at)` tuples.
    pub fn find_promotions_by_envelope(
        &self,
        envelope_id: i64,
    ) -> Result<Vec<(String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        // Trailing comma anchors the numeric value: the stored JSON is always
        // `{"method":"envelope","envelope_id":<n>,"rule":...}`, so without it
        // envelope 42 would also match 421, 423, 4242, … (substring).
        let pattern = format!("%\"envelope_id\":{envelope_id},%");
        let mut stmt = conn.prepare(
            "SELECT agent_id, promotion_id, created_at
             FROM promotion_history
             WHERE pre_authorization LIKE ?1
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![pattern], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::gateway_store::GatewayStore;
    use tempfile::tempdir;

    fn revision(revision_id: &str) -> AgentRevisionRecord {
        AgentRevisionRecord {
            revision_id: revision_id.to_string(),
            agent_id: "coder.default".to_string(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: format!("sha256:{}", revision_id),
            runtime_lock_hash: "sha256:lock".to_string(),
            manifest_hash: "sha256:manifest".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by_type: "autonoetic_agent".to_string(),
            created_by_id: "specialized_builder.default".to_string(),
            requested_by_type: None,
            requested_by_id: None,
            source_kind: "test".to_string(),
            source_ref: None,
            origin_node_id: "gateway".to_string(),
            trust_domain: "local".to_string(),
            status: AgentRevisionStatus::Candidate,
            metadata_json: serde_json::json!({}),
            short_id: String::new(),
            detected_network_hosts: None,
            signature: None,
            signer_id: None,
        }
    }

    /// #651 — concurrent duplicate creates of the same content-addressed
    /// revision are idempotent: the second insert returns `Ok(None)` (lost
    /// the race) instead of a UNIQUE-constraint error, and the winner's short
    /// id is what callers read back.
    #[test]
    fn duplicate_revision_insert_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        let winner = store
            .insert_agent_revision_transactional(&revision("rev-dup"))
            .unwrap()
            .expect("first insert must win and mint a short id");

        // Same content-addressed id again — the TOCTOU loser's insert.
        let loser = store
            .insert_agent_revision_transactional(&revision("rev-dup"))
            .unwrap();
        assert!(
            loser.is_none(),
            "duplicate insert must return Ok(None), got {loser:?}"
        );

        let read = store
            .get_agent_revision("rev-dup")
            .unwrap()
            .expect("row must exist exactly once");
        assert_eq!(
            read.short_id, winner,
            "the winner's short id survives; no second row was minted"
        );
    }

    /// #651 — a *different* revision id still inserts normally (the conflict
    /// clause is keyed on revision_id only).
    #[test]
    fn distinct_revision_ids_both_insert() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        let a = store
            .insert_agent_revision_transactional(&revision("rev-a"))
            .unwrap()
            .expect("first insert");
        let b = store
            .insert_agent_revision_transactional(&revision("rev-b"))
            .unwrap()
            .expect("second insert with a different id");
        assert_ne!(a, b, "distinct revisions mint distinct short ids");
    }

    /// #803 — requested_by_* lineage survives an insert/select roundtrip.
    #[test]
    fn agent_revision_requested_by_roundtrip() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        let mut with = revision("rev-with-requester");
        with.requested_by_type = Some("autonoetic_agent".to_string());
        with.requested_by_id = Some("agent-factory.default".to_string());
        store.insert_agent_revision(&with).unwrap();

        let read = store
            .get_agent_revision("rev-with-requester")
            .unwrap()
            .expect("revision should exist");
        assert_eq!(read.requested_by_type.as_deref(), Some("autonoetic_agent"));
        assert_eq!(
            read.requested_by_id.as_deref(),
            Some("agent-factory.default")
        );

        let without = revision("rev-without-requester");
        store.insert_agent_revision(&without).unwrap();
        let read = store
            .get_agent_revision("rev-without-requester")
            .unwrap()
            .expect("revision should exist");
        assert!(read.requested_by_type.is_none());
        assert!(read.requested_by_id.is_none());
    }

    fn session_agent_binding(session_id: &str) -> SessionAgentBinding {
        SessionAgentBinding {
            session_id: session_id.to_string(),
            root_session_id: session_id.to_string(),
            alias_id: None,
            agent_id: "coder.default".to_string(),
            revision_id: "rev-1".to_string(),
            runtime_lock_hash: "sha256:lock".to_string(),
            constitution_version: None,
            constitution_digest: None,
            home_node_id: "gateway-1".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            requested_target: "coder.default".to_string(),
        }
    }

    /// #821 — the constitution pin (version + digest) survives an
    /// upsert/get round-trip, both when set (Some) and when absent (None,
    /// e.g. the constitution runtime was never initialized).
    #[test]
    fn session_agent_binding_constitution_pin_roundtrip() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        let mut with_pin = session_agent_binding("sess-with-pin");
        with_pin.constitution_version = Some("2026.06.05".to_string());
        with_pin.constitution_digest = Some("deadbeef".to_string());
        store.upsert_session_agent_binding(&with_pin).unwrap();

        let read = store
            .get_session_agent_binding("sess-with-pin")
            .unwrap()
            .expect("binding should exist");
        assert_eq!(read.constitution_version.as_deref(), Some("2026.06.05"));
        assert_eq!(read.constitution_digest.as_deref(), Some("deadbeef"));

        let without_pin = session_agent_binding("sess-without-pin");
        store.upsert_session_agent_binding(&without_pin).unwrap();
        let read = store
            .get_session_agent_binding("sess-without-pin")
            .unwrap()
            .expect("binding should exist");
        assert!(read.constitution_version.is_none());
        assert!(read.constitution_digest.is_none());
    }
}
