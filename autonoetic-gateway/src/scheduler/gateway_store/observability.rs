use super::GatewayStore;
use super::LiveDigestEventRecord;
use anyhow::Result;
use autonoetic_types::config::RetentionConfig;
use rusqlite::params;

impl GatewayStore {
    /// Prune execution_traces older than `days`. 0 = no pruning.
    pub fn prune_execution_traces(&self, days: u32) -> Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
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
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM causal_events WHERE timestamp < ?1",
            params![cutoff],
        )?;
        Ok(n as u64)
    }

    /// Apply retention policy from config. Call once on gateway startup.
    pub fn apply_retention_policy(&self, retention: &RetentionConfig) -> Result<()> {
        if let Err(e) = self.prune_execution_traces(retention.execution_traces_days) {
            tracing::warn!(
                target: "gateway_store",
                error = %e,
                "Failed to prune execution_traces"
            );
        }
        if let Err(e) = self.prune_causal_events(retention.causal_events_days) {
            tracing::warn!(
                target: "gateway_store",
                error = %e,
                "Failed to prune causal_events"
            );
        }
        Ok(())
    }

    pub fn create_causal_event(
        &self,
        event: &autonoetic_types::causal_chain::CausalEventRecord,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO causal_events (
                event_id, agent_id, session_id, turn_id, event_seq, timestamp,
                category, action, status, target, payload, payload_ref, evidence_ref, reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
                event.target.as_deref(),
                event.payload.as_deref(),
                event.payload_ref.as_deref(),
                event.evidence_ref.as_deref(),
                event.reason.as_deref(),
            ],
        )?;
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
            "SELECT * FROM causal_events WHERE {} ORDER BY timestamp DESC LIMIT ?{}",
            where_clause, param_idx
        );

        let mut stmt = conn.prepare(&query)?;
        let mut params_with_limit = params.clone();
        params_with_limit.push(rusqlite::types::Value::Integer(limit));

        let rows = stmt.query_map(rusqlite::params_from_iter(params_with_limit), |row| {
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
                target: row.get(9)?,
                payload: row.get(10)?,
                payload_ref: row.get(11)?,
                evidence_ref: row.get(12)?,
                reason: row.get(13)?,
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
                ended_at = excluded.ended_at,
                status = excluded.status,
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

        let mut conditions = Vec::new();
        let mut sql_params: Vec<rusqlite::types::Value> = Vec::new();

        let has_fts = if let Some(q) = query {
            sql_params.push(rusqlite::types::Value::Text(q.to_string()));
            true
        } else {
            false
        };

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

        let sql = if has_fts {
            format!(
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
            )
        } else {
            format!(
                "SELECT st.transcript_id, st.session_id, st.root_session_id, st.agent_id,
                        st.revision_id, st.user_id, st.started_at, st.ended_at, st.status,
                        st.turn_count, st.transcript_handle, st.excerpt, st.origin_node_id
                 FROM session_transcripts st
                 WHERE {where_clause}
                 ORDER BY st.started_at DESC
                 LIMIT ?",
                where_clause = where_clause,
            )
        };

        let mut stmt = conn.prepare(&sql)?;
        sql_params.push(rusqlite::types::Value::Integer(limit));

        let rows = stmt.query_map(rusqlite::params_from_iter(sql_params), |row| {
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
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }
}
