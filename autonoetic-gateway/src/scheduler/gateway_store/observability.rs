use super::GatewayStore;
use super::LiveDigestEventRecord;
use anyhow::Result;
use autonoetic_types::config::RetentionConfig;
use rusqlite::params;

fn looks_like_fts_syntax(query: &str) -> bool {
    query
        .chars()
        .any(|c| matches!(c, '.' | '(' | ')' | '"' | '*' | '-' | '+' | '&'))
}

fn should_fallback_to_like(err: &rusqlite::Error, query: &str) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.extended_code == rusqlite::ffi::SQLITE_ERROR && looks_like_fts_syntax(query)
    )
}

fn is_sqlite_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<rusqlite::Error>()
        .map(|e| matches!(e, rusqlite::Error::SqliteFailure(_, _)))
        .unwrap_or(false)
}

impl GatewayStore {
    /// Prune execution_traces older than `days`. 0 = no pruning.
    pub fn prune_execution_traces(&self, days: u32) -> Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        self.prune_execution_traces_with_cutoff(&Some(cutoff))
    }

    /// Prune execution_traces older than cutoff. None = no pruning.
    pub fn prune_execution_traces_with_cutoff(&self, cutoff: &Option<String>) -> Result<u64> {
        let Some(cutoff) = cutoff else {
            return Ok(0);
        };
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM execution_traces WHERE timestamp < ?1",
            params![cutoff],
        )?;
        Ok(n as u64)
    }

    /// Prune causal_events older than `days`. 0 = no pruning.
    pub fn prune_causal_events(&self, days: u32) -> Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        self.prune_causal_events_with_cutoff(&Some(cutoff))
    }

    /// Prune causal_events older than cutoff. None = no pruning.
    pub fn prune_causal_events_with_cutoff(&self, cutoff: &Option<String>) -> Result<u64> {
        let Some(cutoff) = cutoff else {
            return Ok(0);
        };
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM causal_events WHERE timestamp < ?1",
            params![cutoff],
        )?;
        Ok(n as u64)
    }

    /// Apply retention policy from config. Call once on gateway startup.
    /// Emits a `retention.pruned` causal event with counts of pruned rows.
    pub fn apply_retention_policy(&self, retention: &RetentionConfig) -> Result<()> {
        let now = chrono::Utc::now();

        let traces_cutoff = if retention.execution_traces_days > 0 {
            Some(
                (now - chrono::Duration::days(retention.execution_traces_days as i64)).to_rfc3339(),
            )
        } else {
            None
        };
        let events_cutoff = if retention.causal_events_days > 0 {
            Some((now - chrono::Duration::days(retention.causal_events_days as i64)).to_rfc3339())
        } else {
            None
        };

        let traces_pruned = match self.prune_execution_traces_with_cutoff(&traces_cutoff) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    target: "gateway_store",
                    error = %e,
                    "Failed to prune execution_traces"
                );
                0
            }
        };
        let events_pruned = match self.prune_causal_events_with_cutoff(&events_cutoff) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    target: "gateway_store",
                    error = %e,
                    "Failed to prune causal_events"
                );
                0
            }
        };

        if traces_pruned > 0 || events_pruned > 0 {
            let mut rules = autonoetic_types::causal_chain::default_enforced_rules();
            rules.push("P-8.17".to_string());

            let payload = serde_json::json!({
                "execution_traces_pruned": traces_pruned,
                "causal_events_pruned": events_pruned,
                "execution_traces_cutoff": traces_cutoff,
                "causal_events_cutoff": events_cutoff,
                "retention_config": {
                    "execution_traces_days": retention.execution_traces_days,
                    "causal_events_days": retention.causal_events_days,
                },
            });
            if let Err(e) =
                self.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    agent_id: "gateway".to_string(),
                    session_id: "system".to_string(),
                    turn_id: None,
                    event_seq: now.timestamp_millis().max(0) as u64,
                    timestamp: now.to_rfc3339(),
                    category: "retention".to_string(),
                    action: "pruned".to_string(),
                    status: autonoetic_types::causal_chain::EntryStatus::Success.to_string(),
                    enforced_rules: rules,
                    target: None,
                    payload: serde_json::to_string(&payload).ok(),
                    payload_ref: None,
                    evidence_ref: None,
                    reason: None,
                })
            {
                tracing::warn!(
                    target: "gateway_store",
                    error = %e,
                    "Failed to record retention.pruned causal event"
                );
            }
        }

        Ok(())
    }

    pub fn emit_vault_key_probe_event(&self, result: &crate::vault::KeyProbeResult) {
        let now = chrono::Utc::now();
        let mut rules = autonoetic_types::causal_chain::default_enforced_rules();
        rules.push("R+8".to_string());
        let (status, reason, payload) = match result {
            crate::vault::KeyProbeResult::Present { source } => (
                autonoetic_types::causal_chain::EntryStatus::Success.to_string(),
                format!("Vault key available via {}", match source {
                    crate::vault::KeySource::EnvVar => "AUTONOETIC_VAULT_KEY",
                    crate::vault::KeySource::FilePath => "AUTONOETIC_VAULT_KEY_PATH",
                    crate::vault::KeySource::AutoGenerated => "auto-generated vault.key",
                }),
                serde_json::json!({ "source": match source {
                    crate::vault::KeySource::EnvVar => "env_var",
                    crate::vault::KeySource::FilePath => "file_path",
                    crate::vault::KeySource::AutoGenerated => "auto_generated",
                }}),
            ),
            crate::vault::KeyProbeResult::Missing { source, path } => (
                "ERROR".to_string(),
                format!("Vault key missing: {} points to non-existent file {}", 
                    match source {
                        crate::vault::KeySource::EnvVar => "AUTONOETIC_VAULT_KEY",
                        crate::vault::KeySource::FilePath => "AUTONOETIC_VAULT_KEY_PATH",
                        crate::vault::KeySource::AutoGenerated => "auto-generated",
                    },
                    path),
                serde_json::json!({
                    "source": match source {
                        crate::vault::KeySource::EnvVar => "env_var",
                        crate::vault::KeySource::FilePath => "file_path",
                        crate::vault::KeySource::AutoGenerated => "auto_generated",
                    },
                    "path": path,
                }),
            ),
            crate::vault::KeyProbeResult::Invalid { source, reason: detail } => (
                "ERROR".to_string(),
                format!("Vault key invalid ({}): {}", 
                    match source {
                        crate::vault::KeySource::EnvVar => "AUTONOETIC_VAULT_KEY",
                        crate::vault::KeySource::FilePath => "AUTONOETIC_VAULT_KEY_PATH",
                        crate::vault::KeySource::AutoGenerated => "auto-generated",
                    },
                    detail),
                serde_json::json!({
                    "source": match source {
                        crate::vault::KeySource::EnvVar => "env_var",
                        crate::vault::KeySource::FilePath => "file_path",
                        crate::vault::KeySource::AutoGenerated => "auto_generated",
                    },
                    "reason": detail,
                }),
            ),
            crate::vault::KeyProbeResult::NotConfigured => (
                "ERROR".to_string(),
                "Vault key not configured: no AUTONOETIC_VAULT_KEY, AUTONOETIC_VAULT_KEY_PATH, or auto-generated vault.key found".to_string(),
                serde_json::json!({ "source": "none" }),
            ),
        };

        if let Err(e) =
            self.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
                event_id: uuid::Uuid::new_v4().to_string(),
                agent_id: "gateway".to_string(),
                session_id: "system".to_string(),
                turn_id: None,
                event_seq: now.timestamp_millis().max(0) as u64,
                timestamp: now.to_rfc3339(),
                category: "vault".to_string(),
                action: "key_probe".to_string(),
                status,
                enforced_rules: rules,
                target: None,
                payload: serde_json::to_string(&payload).ok(),
                payload_ref: None,
                evidence_ref: None,
                reason: Some(reason),
            })
        {
            tracing::warn!(
                target: "gateway_store",
                error = %e,
                "Failed to record vault.key_probe causal event"
            );
        }
    }

    pub fn create_causal_event(
        &self,
        event: &autonoetic_types::causal_chain::CausalEventRecord,
    ) -> Result<()> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO causal_events (
                event_id, agent_id, session_id, turn_id, event_seq, timestamp,
                category, action, status, enforced_rules, target, payload, payload_ref, evidence_ref, reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    &event.event_id,
                    &event.agent_id,
                    &event.session_id,
                    event.turn_id.as_deref(),
                    event.event_seq as i64,
                    &event.timestamp,
                    &event.category,
                    &event.action,
                    &event.status,
                    serde_json::to_string(&event.enforced_rules)?,
                    event.target.as_deref(),
                    event.payload.as_deref(),
                    event.payload_ref.as_deref(),
                    event.evidence_ref.as_deref(),
                    event.reason.as_deref(),
                ],
            )?;
        }
        if let Ok(guard) = self.policy_hook_executor.lock() {
            if let Some(w) = guard.as_ref() {
                if let Some(exec) = w.upgrade() {
                    exec.maybe_dispatch_policy_decision_hook(event);
                }
            }
        }
        Ok(())
    }

    /// Query causal events with filters.
    pub fn search_causal_events(
        &self,
        session_id: Option<&str>,
        agent_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::causal_chain::CausalEventRecord>> {
        let conn = self.conn.lock().unwrap();

        let mut conditions = Vec::new();
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        let mut param_idx = 1;

        if let Some(sid) = session_id {
            conditions.push("session_id = ?");
            params.push(rusqlite::types::Value::Text(sid.to_string()));
            param_idx += 1;
        }

        if let Some(aid) = agent_id {
            conditions.push("agent_id = ?");
            params.push(rusqlite::types::Value::Text(aid.to_string()));
            param_idx += 1;
        }

        let where_clause = if conditions.is_empty() {
            "1".to_string()
        } else {
            conditions.join(" AND ")
        };

        let query = format!(
            "SELECT event_id, agent_id, session_id, turn_id, event_seq, timestamp, category, action, status, enforced_rules, target, payload, payload_ref, evidence_ref, reason FROM causal_events WHERE {} ORDER BY timestamp DESC LIMIT ?{}",
            where_clause, param_idx
        );

        let mut stmt = conn.prepare(&query)?;
        let mut params_with_limit = params.clone();
        params_with_limit.push(rusqlite::types::Value::Integer(limit));

        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_with_limit.iter()),
            |row| {
                let enforced_rules_json: String = row.get(9)?;
                let enforced_rules = Some(enforced_rules_json.as_str())
                    .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
                    .unwrap_or_else(autonoetic_types::causal_chain::default_enforced_rules);
                Ok(autonoetic_types::causal_chain::CausalEventRecord {
                    event_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    session_id: row.get(2)?,
                    turn_id: row.get(3)?,
                    event_seq: row.get::<_, i64>(4)? as u64,
                    timestamp: row.get(5)?,
                    category: row.get(6)?,
                    action: row.get(7)?,
                    status: row.get(8)?,
                    enforced_rules,
                    target: row.get(10)?,
                    payload: row.get(11)?,
                    payload_ref: row.get(12)?,
                    evidence_ref: row.get(13)?,
                    reason: row.get(14)?,
                })
            },
        )?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    /// Standing **contract-health** view (#302): tally how often each
    /// constitutional clause (principle/right) has been enforced, by reading
    /// the `enforced_rules` carried on causal events and attributing each
    /// rule/right ID to its owning clause via the enforcement register.
    ///
    /// The `R+++3` event-attribution placeholder (every event carries it by
    /// default) is skipped — only events that named a concrete rule/right
    /// contribute. Real rule IDs not yet in the register surface in
    /// `ContractHealth::unattributed`, keeping migration gaps visible rather
    /// than silently dropped.
    ///
    /// `since` is an optional RFC3339 lower bound on `timestamp`; `None` scans
    /// all retained events. The bound is parsed and compared by absolute
    /// instant (not raw text), so offset forms like `Z` vs `+02:00` behave
    /// correctly and malformed operator input fails clearly.
    pub fn contract_health(
        &self,
        since: Option<&str>,
    ) -> Result<crate::enforcement_register::ContractHealth> {
        // Parse + validate the bound up front: raw SQLite text comparison on
        // RFC3339 is wrong across offset forms, and an unparsed string would
        // silently yield empty/partial results instead of a clear error.
        let since_dt = match since {
            Some(ts) => Some(chrono::DateTime::parse_from_rfc3339(ts).map_err(|e| {
                anyhow::anyhow!("invalid `since` timestamp {ts:?}: {e} (expected RFC3339)")
            })?),
            None => None,
        };

        let conn = self.conn.lock().unwrap();
        let placeholder = autonoetic_types::causal_chain::RULE_ID_EVENT_ATTRIBUTION;

        let mut stmt = conn.prepare("SELECT enforced_rules, timestamp FROM causal_events")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;

        let mut rule_ids: Vec<String> = Vec::new();
        for r in rows {
            let (raw, ts) = r?;
            // Filter by absolute time when a bound is set. An event whose own
            // timestamp won't parse is kept rather than dropped — we never
            // hide enforcement activity over a formatting quirk.
            if let Some(bound) = since_dt {
                if let Ok(event_dt) = chrono::DateTime::parse_from_rfc3339(&ts) {
                    if event_dt < bound {
                        continue;
                    }
                }
            }
            // Each cell is a JSON array of rule/right IDs; tolerate malformed
            // rows by skipping rather than failing the whole tally.
            if let Ok(ids) = serde_json::from_str::<Vec<String>>(&raw) {
                rule_ids.extend(ids.into_iter().filter(|id| id != placeholder));
            }
        }

        Ok(crate::enforcement_register::contract_health(rule_ids))
    }

    /// List curator decision events (`category = 'curator'`, `action = 'decision'`)
    /// for a specific target URI/id. Used by operator workflows asking
    /// "why was memory X dropped" (issue #30). The underlying `idx_causal_target`
    /// index makes this an O(log n) point query.
    pub fn list_curator_decisions_by_target(
        &self,
        target: &str,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::causal_chain::CausalEventRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT event_id, agent_id, session_id, turn_id, event_seq, timestamp,
                    category, action, status, enforced_rules, target, payload,
                    payload_ref, evidence_ref, reason
             FROM causal_events
             WHERE category = 'curator' AND action = 'decision' AND target = ?1
             ORDER BY timestamp DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![target, limit], |row| {
            let enforced_rules_json: String = row.get(9)?;
            let enforced_rules = Some(enforced_rules_json.as_str())
                .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
                .unwrap_or_else(autonoetic_types::causal_chain::default_enforced_rules);
            Ok(autonoetic_types::causal_chain::CausalEventRecord {
                event_id: row.get(0)?,
                agent_id: row.get(1)?,
                session_id: row.get(2)?,
                turn_id: row.get(3)?,
                event_seq: row.get::<_, i64>(4)? as u64,
                timestamp: row.get(5)?,
                category: row.get(6)?,
                action: row.get(7)?,
                status: row.get(8)?,
                enforced_rules,
                target: row.get(10)?,
                payload: row.get(11)?,
                payload_ref: row.get(12)?,
                evidence_ref: row.get(13)?,
                reason: row.get(14)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn create_execution_trace(
        &self,
        trace: &autonoetic_types::causal_chain::ExecutionTraceRecord,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO execution_traces (
                trace_id, event_id, agent_id, session_id, turn_id, timestamp,
                tool_name, command, exit_code, stdout, stderr, duration_ms,
                success, error_type, error_summary, approval_required, approval_request_id, arguments, result
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                &trace.trace_id,
                trace.event_id.as_deref(),
                &trace.agent_id,
                &trace.session_id,
                trace.turn_id.as_deref(),
                &trace.timestamp,
                &trace.tool_name,
                trace.command.as_deref(),
                trace.exit_code,
                trace.stdout.as_deref(),
                trace.stderr.as_deref(),
                &trace.duration_ms,
                &trace.success,
                trace.error_type.as_deref(),
                trace.error_summary.as_deref(),
                trace.approval_required,
                trace.approval_request_id.as_deref(),
                trace.arguments.as_deref(),
                trace.result.as_deref(),
            ],
        )?;
        Ok(())
    }

    pub fn create_live_digest_event(&self, event: &LiveDigestEventRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO live_digest_events (
                event_id, root_session_id, source_session_id, turn_id, source_agent_id,
                source_node_id, event_type, payload, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &event.event_id,
                &event.root_session_id,
                &event.source_session_id,
                event.turn_id.as_deref(),
                event.source_agent_id.as_deref(),
                &event.source_node_id,
                &event.event_type,
                event.payload.as_deref(),
                &event.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn search_execution_traces(
        &self,
        tool_name: Option<&str>,
        success: Option<bool>,
        error_type: Option<&str>,
        command_pattern: Option<&str>,
        agent_id: Option<&str>,
        session_branch: Option<&str>,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::causal_chain::ExecutionTraceRecord>> {
        let conn = self.conn.lock().unwrap();

        let mut conditions = Vec::new();
        let mut sql_params: Vec<rusqlite::types::Value> = Vec::new();
        let mut param_idx = 1;

        if let Some(name) = tool_name {
            conditions.push("tool_name = ?");
            sql_params.push(rusqlite::types::Value::Text(name.to_string()));
            param_idx += 1;
        }

        if let Some(s) = success {
            conditions.push("success = ?");
            sql_params.push(rusqlite::types::Value::Integer(if s { 1 } else { 0 }));
            param_idx += 1;
        }

        if let Some(et) = error_type {
            conditions.push("error_type = ?");
            sql_params.push(rusqlite::types::Value::Text(et.to_string()));
            param_idx += 1;
        }

        if let Some(pattern) = command_pattern {
            conditions.push("command LIKE ?");
            sql_params.push(rusqlite::types::Value::Text(format!("%{}%", pattern)));
            param_idx += 1;
        }

        if let Some(aid) = agent_id {
            conditions.push("agent_id = ?");
            sql_params.push(rusqlite::types::Value::Text(aid.to_string()));
            param_idx += 1;
        }

        if let Some(sid) = session_branch {
            conditions.push("(session_id = ? OR session_id LIKE ? ESCAPE '\\')");
            sql_params.push(rusqlite::types::Value::Text(sid.to_string()));
            let escaped = super::escape_sqlite_like_fragment(sid);
            sql_params.push(rusqlite::types::Value::Text(format!("{}/%", escaped)));
            param_idx += 2;
        }

        let where_clause = if conditions.is_empty() {
            "1".to_string()
        } else {
            format!("{}", conditions.join(" AND "))
        };

        let query = format!(
            "SELECT * FROM execution_traces WHERE {} ORDER BY timestamp DESC LIMIT ?{}",
            where_clause, param_idx
        );

        let mut stmt = conn.prepare(&query)?;
        let mut params_with_limit = sql_params.clone();
        params_with_limit.push(rusqlite::types::Value::Integer(limit));

        let rows = stmt.query_map(rusqlite::params_from_iter(params_with_limit), |row| {
            Ok(autonoetic_types::causal_chain::ExecutionTraceRecord {
                trace_id: row.get(0)?,
                event_id: row.get(1)?,
                agent_id: row.get(2)?,
                session_id: row.get(3)?,
                turn_id: row.get(4)?,
                timestamp: row.get(5)?,
                tool_name: row.get(6)?,
                command: row.get(7)?,
                exit_code: row.get(8)?,
                stdout: row.get(9)?,
                stderr: row.get(10)?,
                duration_ms: row.get(11)?,
                success: row.get(12)?,
                error_type: row.get(13)?,
                error_summary: row.get(14)?,
                approval_required: row.get(15)?,
                approval_request_id: row.get(16)?,
                arguments: row.get(17)?,
                result: row.get(18)?,
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn upsert_session_transcript(
        &self,
        record: &autonoetic_types::causal_chain::SessionTranscriptRecord,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_transcripts (
                transcript_id, session_id, root_session_id, agent_id,
                revision_id, user_id, started_at, ended_at, status,
                turn_count, transcript_handle, excerpt, origin_node_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(session_id) DO UPDATE SET
                transcript_id = excluded.transcript_id,
                ended_at = CASE
                    WHEN excluded.status = 'active'
                     AND session_transcripts.status IN ('completed', 'failed', 'suspended', 'closed')
                    THEN session_transcripts.ended_at
                    ELSE excluded.ended_at
                END,
                status = CASE
                    WHEN excluded.status = 'active'
                     AND session_transcripts.status IN ('completed', 'failed', 'suspended', 'closed')
                    THEN session_transcripts.status
                    ELSE excluded.status
                END,
                turn_count = excluded.turn_count,
                transcript_handle = excluded.transcript_handle,
                excerpt = excluded.excerpt",
            params![
                record.transcript_id,
                record.session_id,
                record.root_session_id,
                record.agent_id,
                record.revision_id,
                record.user_id,
                record.started_at,
                record.ended_at,
                record.status,
                record.turn_count,
                record.transcript_handle,
                record.excerpt,
                record.origin_node_id,
            ],
        )?;
        Ok(())
    }

    pub fn finalize_session_transcript(
        &self,
        session_id: &str,
        ended_at: &str,
        status: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE session_transcripts SET ended_at = ?1, status = ?2 WHERE session_id = ?3",
            params![ended_at, status, session_id],
        )?;
        Ok(())
    }

    /// Re-open a previously finalized session for a new turn.
    ///
    /// Between turns the session is `completed` (from `close_session`), but the
    /// orphan-child reaper (R+12) treats any completed/failed parent as "terminated"
    /// and will cancel its children.  Call this at the start of each turn to restore
    /// `active` so that child agents spawned during execution are not immediately
    /// orphaned.
    pub fn reopen_session_transcript(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE session_transcripts SET status = 'active', ended_at = NULL WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    pub fn find_transcript_by_handle(
        &self,
        handle: &str,
    ) -> Result<Option<autonoetic_types::causal_chain::SessionTranscriptRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT transcript_id, session_id, root_session_id, agent_id,
                    revision_id, user_id, started_at, ended_at, status,
                    turn_count, transcript_handle, excerpt, origin_node_id
             FROM session_transcripts
             WHERE transcript_handle = ?1
             LIMIT 1",
        )?;
        let result = stmt.query_row(params![handle], |row| {
            Ok(autonoetic_types::causal_chain::SessionTranscriptRecord {
                transcript_id: row.get(0)?,
                session_id: row.get(1)?,
                root_session_id: row.get(2)?,
                agent_id: row.get(3)?,
                revision_id: row.get(4)?,
                user_id: row.get(5)?,
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                status: row.get(8)?,
                turn_count: row.get(9)?,
                transcript_handle: row.get(10)?,
                excerpt: row.get(11)?,
                origin_node_id: row.get(12)?,
            })
        });

        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn find_transcript_by_session_id(
        &self,
        session_id: &str,
    ) -> Result<Option<autonoetic_types::causal_chain::SessionTranscriptRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT transcript_id, session_id, root_session_id, agent_id,
                    revision_id, user_id, started_at, ended_at, status,
                    turn_count, transcript_handle, excerpt, origin_node_id
             FROM session_transcripts
             WHERE session_id = ?1
             LIMIT 1",
        )?;
        let result = stmt.query_row(params![session_id], |row| {
            Ok(autonoetic_types::causal_chain::SessionTranscriptRecord {
                transcript_id: row.get(0)?,
                session_id: row.get(1)?,
                root_session_id: row.get(2)?,
                agent_id: row.get(3)?,
                revision_id: row.get(4)?,
                user_id: row.get(5)?,
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                status: row.get(8)?,
                turn_count: row.get(9)?,
                transcript_handle: row.get(10)?,
                excerpt: row.get(11)?,
                origin_node_id: row.get(12)?,
            })
        });
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn search_session_transcripts(
        &self,
        query: Option<&str>,
        agent_id: Option<&str>,
        root_session_id: Option<&str>,
        status: Option<&str>,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::causal_chain::SessionTranscriptRecord>> {
        let conn = self.conn.lock().unwrap();

        let mut conditions: Vec<String> = Vec::new();
        let mut sql_params: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(aid) = agent_id {
            conditions.push("st.agent_id = ?".to_string());
            sql_params.push(rusqlite::types::Value::Text(aid.to_string()));
        }

        if let Some(rid) = root_session_id {
            conditions.push("st.root_session_id = ?".to_string());
            sql_params.push(rusqlite::types::Value::Text(rid.to_string()));
        }

        if let Some(s) = status {
            conditions.push("st.status = ?".to_string());
            sql_params.push(rusqlite::types::Value::Text(s.to_string()));
        }

        if let Some(since_time) = since {
            conditions.push("st.started_at >= ?".to_string());
            sql_params.push(rusqlite::types::Value::Text(since_time.to_string()));
        }

        let where_clause = if conditions.is_empty() {
            "1".to_string()
        } else {
            conditions.join(" AND ")
        };

        if let Some(q) = query {
            let fts_sql = format!(
                "SELECT st.transcript_id, st.session_id, st.root_session_id, st.agent_id,
                        st.revision_id, st.user_id, st.started_at, st.ended_at, st.status,
                        st.turn_count, st.transcript_handle, st.excerpt, st.origin_node_id
                 FROM session_transcripts st
                 JOIN session_transcripts_fts ON st.rowid = session_transcripts_fts.rowid
                 WHERE session_transcripts_fts MATCH ?1
                   AND {where_clause}
                 ORDER BY rank ASC
                 LIMIT ?",
                where_clause = where_clause,
            );

            let mut fts_params = vec![rusqlite::types::Value::Text(q.to_string())];
            fts_params.extend(sql_params.clone());
            fts_params.push(rusqlite::types::Value::Integer(limit));

            let mut stmt = conn.prepare(&fts_sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(fts_params), |row| {
                Ok(autonoetic_types::causal_chain::SessionTranscriptRecord {
                    transcript_id: row.get(0)?,
                    session_id: row.get(1)?,
                    root_session_id: row.get(2)?,
                    agent_id: row.get(3)?,
                    revision_id: row.get(4)?,
                    user_id: row.get(5)?,
                    started_at: row.get(6)?,
                    ended_at: row.get(7)?,
                    status: row.get(8)?,
                    turn_count: row.get(9)?,
                    transcript_handle: row.get(10)?,
                    excerpt: row.get(11)?,
                    origin_node_id: row.get(12)?,
                })
            });

            match rows {
                Ok(r) => {
                    let mut results = Vec::new();
                    for res in r {
                        results.push(res?);
                    }
                    return Ok(results);
                }
                Err(ref e) if should_fallback_to_like(e, q) => {
                    // FTS parse error with FTS-like syntax — fall back to LIKE search
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Non-FTS path (no query, or FTS fallback)
        let (sql, like_params) = if let Some(q) = query {
            let mut fb_conditions = conditions.clone();
            fb_conditions.push("st.excerpt LIKE ?".to_string());
            let mut fb_params = sql_params.clone();
            fb_params.push(rusqlite::types::Value::Text(format!("%{}%", q)));
            let fb_where = fb_conditions.join(" AND ");
            (
                format!(
                    "SELECT st.transcript_id, st.session_id, st.root_session_id, st.agent_id,
                        st.revision_id, st.user_id, st.started_at, st.ended_at, st.status,
                        st.turn_count, st.transcript_handle, st.excerpt, st.origin_node_id
                 FROM session_transcripts st
                 WHERE {fb_where}
                 ORDER BY st.started_at DESC
                 LIMIT ?",
                    fb_where = fb_where,
                ),
                fb_params,
            )
        } else {
            (
                format!(
                    "SELECT st.transcript_id, st.session_id, st.root_session_id, st.agent_id,
                        st.revision_id, st.user_id, st.started_at, st.ended_at, st.status,
                        st.turn_count, st.transcript_handle, st.excerpt, st.origin_node_id
                 FROM session_transcripts st
                 WHERE {where_clause}
                 ORDER BY st.started_at DESC
                 LIMIT ?",
                    where_clause = where_clause,
                ),
                sql_params,
            )
        };

        let mut final_params = like_params;
        final_params.push(rusqlite::types::Value::Integer(limit));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(final_params), |row| {
            Ok(autonoetic_types::causal_chain::SessionTranscriptRecord {
                transcript_id: row.get(0)?,
                session_id: row.get(1)?,
                root_session_id: row.get(2)?,
                agent_id: row.get(3)?,
                revision_id: row.get(4)?,
                user_id: row.get(5)?,
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                status: row.get(8)?,
                turn_count: row.get(9)?,
                transcript_handle: row.get(10)?,
                excerpt: row.get(11)?,
                origin_node_id: row.get(12)?,
            })
        })?;

        let mut results = Vec::new();
        for res in rows {
            results.push(res?);
        }
        Ok(results)
    }

    pub fn upsert_published_session_report(
        &self,
        record: &autonoetic_types::causal_chain::PublishedSessionReportRecord,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO published_session_reports (
                root_session_id, report_handle, overview_handle, html_handle,
                narrative_handle, title, status, started_at, ended_at,
                agent_count, error_count, approval_count, search_text,
                generated_at, report_version
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            ON CONFLICT(root_session_id) DO UPDATE SET
                report_handle = excluded.report_handle,
                overview_handle = excluded.overview_handle,
                html_handle = excluded.html_handle,
                narrative_handle = excluded.narrative_handle,
                title = excluded.title,
                status = excluded.status,
                ended_at = COALESCE(excluded.ended_at, published_session_reports.ended_at),
                agent_count = excluded.agent_count,
                error_count = excluded.error_count,
                approval_count = excluded.approval_count,
                search_text = excluded.search_text,
                generated_at = excluded.generated_at,
                report_version = excluded.report_version",
            params![
                &record.root_session_id,
                &record.report_handle,
                record.overview_handle.as_deref(),
                record.html_handle.as_deref(),
                record.narrative_handle.as_deref(),
                &record.title,
                &record.status,
                record.started_at.as_deref(),
                record.ended_at.as_deref(),
                record.agent_count,
                record.error_count,
                record.approval_count,
                &record.search_text,
                &record.generated_at,
                record.report_version,
            ],
        )?;

        conn.execute(
            "DELETE FROM published_session_reports_fts WHERE root_session_id = ?1",
            params![&record.root_session_id],
        )?;
        conn.execute(
            "INSERT INTO published_session_reports_fts (root_session_id, title, search_text, status) VALUES (?1, ?2, ?3, ?4)",
            params![
                &record.root_session_id,
                &record.title,
                &record.search_text,
                &record.status,
            ],
        )?;

        Ok(())
    }

    pub fn find_published_report(
        &self,
        root_session_id: &str,
    ) -> Result<Option<autonoetic_types::causal_chain::PublishedSessionReportRecord>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT root_session_id, report_handle, overview_handle, html_handle,
                    narrative_handle, title, status, started_at, ended_at,
                    agent_count, error_count, approval_count, search_text,
                    generated_at, report_version
             FROM published_session_reports
             WHERE root_session_id = ?1",
            params![root_session_id],
            |row| {
                Ok(
                    autonoetic_types::causal_chain::PublishedSessionReportRecord {
                        root_session_id: row.get(0)?,
                        report_handle: row.get(1)?,
                        overview_handle: row.get(2)?,
                        html_handle: row.get(3)?,
                        narrative_handle: row.get(4)?,
                        title: row.get(5)?,
                        status: row.get(6)?,
                        started_at: row.get(7)?,
                        ended_at: row.get(8)?,
                        agent_count: row.get(9)?,
                        error_count: row.get(10)?,
                        approval_count: row.get(11)?,
                        search_text: row.get(12)?,
                        generated_at: row.get(13)?,
                        report_version: row.get(14)?,
                    },
                )
            },
        );
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn search_published_reports(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::causal_chain::PublishedSessionReportRecord>> {
        let conn = self.conn.lock().unwrap();

        let fts_sql = "\
            SELECT r.root_session_id, r.report_handle, r.overview_handle, r.html_handle,
                    r.narrative_handle, r.title, r.status, r.started_at, r.ended_at,
                    r.agent_count, r.error_count, r.approval_count, r.search_text,
                    r.generated_at, r.report_version
             FROM published_session_reports r
             WHERE r.root_session_id IN (
                 SELECT f.root_session_id FROM published_session_reports_fts f WHERE f MATCH ?1
             )
             ORDER BY r.generated_at DESC
             LIMIT ?2";

        match Self::read_published_rows(&conn, fts_sql, params![query, limit]) {
            Ok(rows) => return Ok(rows),
            Err(e) => {
                tracing::debug!(target: "observability", error = %e, "FTS search failed, falling back to LIKE");
                // Always fall back to LIKE on FTS error
            }
        }

        let like_sql = "\
            SELECT root_session_id, report_handle, overview_handle, html_handle,
                    narrative_handle, title, status, started_at, ended_at,
                    agent_count, error_count, approval_count, search_text,
                    generated_at, report_version
             FROM published_session_reports
             WHERE search_text LIKE ?1
             ORDER BY generated_at DESC
             LIMIT ?2";
        Self::read_published_rows(&conn, like_sql, params![format!("%{}%", query), limit])
    }

    fn read_published_rows(
        conn: &rusqlite::Connection,
        sql: &str,
        p: impl rusqlite::Params,
    ) -> Result<Vec<autonoetic_types::causal_chain::PublishedSessionReportRecord>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(p, |row| {
            Ok(
                autonoetic_types::causal_chain::PublishedSessionReportRecord {
                    root_session_id: row.get(0)?,
                    report_handle: row.get(1)?,
                    overview_handle: row.get(2)?,
                    html_handle: row.get(3)?,
                    narrative_handle: row.get(4)?,
                    title: row.get(5)?,
                    status: row.get(6)?,
                    started_at: row.get(7)?,
                    ended_at: row.get(8)?,
                    agent_count: row.get(9)?,
                    error_count: row.get(10)?,
                    approval_count: row.get(11)?,
                    search_text: row.get(12)?,
                    generated_at: row.get(13)?,
                    report_version: row.get(14)?,
                },
            )
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    /// Find active sessions whose parent session has ended (orphan detection for R+12).
    ///
    /// Returns (child_session_id, parent_session_id, root_session_id, agent_id) tuples
    /// for each orphaned child. A child is "active" if its transcript status is 'active'
    /// and its parent (derived from session_id path) has a terminal transcript
    /// (`completed` or `failed`). `suspended` parents are NOT terminal (resumable).
    pub fn find_orphaned_sessions(&self) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, root_session_id, agent_id
             FROM session_transcripts
             WHERE status = 'active'
               AND session_id LIKE '%/%'",
        )?;
        let active_children: Vec<(String, String, String, String)> = stmt
            .query_map([], |row| {
                let sid: String = row.get(0)?;
                let root: String = row.get(1)?;
                let agent: String = row.get(2)?;
                let parent = sid
                    .rsplit_once('/')
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_default();
                Ok((sid, parent, root, agent))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut orphans = Vec::new();
        for (child_session_id, parent_session_id, root_session_id, agent_id) in active_children {
            let parent_status: Option<String> = conn
                .query_row(
                    "SELECT status FROM session_transcripts WHERE session_id = ?1",
                    params![parent_session_id],
                    |row| row.get(0),
                )
                .ok();
            match parent_status.as_deref() {
                Some("completed") | Some("failed") => {
                    orphans.push((
                        child_session_id,
                        parent_session_id,
                        root_session_id,
                        agent_id,
                    ));
                }
                _ => {}
            }
        }
        Ok(orphans)
    }

    pub fn record_sandbox_escape_attempt(
        &self,
        session_id: &str,
        root_session_id: &str,
        agent_id: &str,
        indicator: &str,
        detail: &str,
        exit_code: Option<i32>,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("gateway_store conn mutex");
        conn.execute(
            "INSERT INTO sandbox_escape_attempts
                (session_id, root_session_id, agent_id, indicator, detail, exit_code, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                root_session_id,
                agent_id,
                indicator,
                detail,
                exit_code,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn count_sandbox_escape_attempts_for_session(&self, session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("gateway_store conn mutex");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sandbox_escape_attempts WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn count_sandbox_escape_attempts_for_root(&self, root_session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("gateway_store conn mutex");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sandbox_escape_attempts WHERE root_session_id = ?1",
            params![root_session_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn emit_escape_threshold_event(
        &self,
        session_id: &str,
        root_session_id: &str,
        count: usize,
        threshold: usize,
        level: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now();
        let mut rules = autonoetic_types::causal_chain::default_enforced_rules();
        rules.push("R++8".to_string());
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("escape-threshold-{}", uuid::Uuid::new_v4()),
            agent_id: "gateway".to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: now.timestamp_millis().max(0) as u64,
            timestamp: now.to_rfc3339(),
            category: "security".to_string(),
            action: format!("sandbox.escape_threshold_{}", level),
            status: autonoetic_types::causal_chain::EntryStatus::Error.to_string(),
            enforced_rules: rules,
            target: Some(root_session_id.to_string()),
            payload: Some(
                serde_json::json!({
                    "session_id": session_id,
                    "root_session_id": root_session_id,
                    "count": count,
                    "threshold": threshold,
                    "level": level,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some(format!(
                "session {} has {} escape attempts (threshold: {})",
                session_id, count, threshold
            )),
        };
        self.create_causal_event(&event)?;
        Ok(())
    }

    pub fn sessions_exceeding_escape_threshold(
        &self,
        threshold: usize,
    ) -> Result<Vec<(String, String, usize)>> {
        if threshold == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().expect("gateway_store conn mutex");
        let mut stmt = conn.prepare(
            "SELECT session_id, root_session_id, COUNT(*) as cnt \
             FROM sandbox_escape_attempts \
             GROUP BY session_id \
             HAVING cnt >= ?1",
        )?;
        let rows = stmt.query_map(params![threshold as i64], |row| {
            let session_id: String = row.get(0)?;
            let root_session_id: String = row.get(1)?;
            let count: i64 = row.get(2)?;
            Ok((session_id, root_session_id, count as usize))
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn list_recent_sessions(
        &self,
        limit: i64,
    ) -> Result<Vec<(String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, agent_id, MAX(timestamp) as last_ts
             FROM causal_events
             GROUP BY session_id
             ORDER BY last_ts DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }
}
mod fts_fallback_tests {
    use super::*;

    #[test]
    fn test_looks_like_fts_syntax_dot() {
        assert!(looks_like_fts_syntax("runtime.lock"));
    }

    #[test]
    fn test_looks_like_fts_syntax_parens() {
        assert!(looks_like_fts_syntax("config)"));
        assert!(looks_like_fts_syntax("(config"));
    }

    #[test]
    fn test_looks_like_fts_syntax_wildcard() {
        assert!(looks_like_fts_syntax("config*"));
    }

    #[test]
    fn test_looks_like_fts_syntax_operators() {
        assert!(looks_like_fts_syntax("a-b"));
        assert!(looks_like_fts_syntax("a+b"));
        assert!(looks_like_fts_syntax("a&b"));
        assert!(looks_like_fts_syntax("a\"b"));
    }

    #[test]
    fn test_looks_like_fts_syntax_plain() {
        assert!(!looks_like_fts_syntax("config"));
        assert!(!looks_like_fts_syntax("runtime lock"));
    }

    #[test]
    fn test_should_fallback_true_on_sqlite_error() {
        let err = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR as i32),
            None,
        );
        assert!(should_fallback_to_like(&err, "runtime.lock"));
    }

    #[test]
    fn test_should_fallback_false_on_non_fts_syntax() {
        let err = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR as i32),
            None,
        );
        assert!(!should_fallback_to_like(&err, "plain query"));
    }

    #[test]
    fn test_should_fallback_false_on_other_error() {
        let err = rusqlite::Error::InvalidParameterName("test".into());
        assert!(!should_fallback_to_like(&err, "runtime.lock"));
    }
}
