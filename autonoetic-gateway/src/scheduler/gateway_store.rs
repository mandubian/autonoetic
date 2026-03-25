use anyhow::Result;
use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
use autonoetic_types::memory::MemoryObject;
use autonoetic_types::background::{
    ApprovalRequest, UserInteraction, UserInteractionAnswer, UserInteractionKind,
    UserInteractionOption, UserInteractionStatus,
};
use autonoetic_types::notification::{NotificationRecord, NotificationStatus, NotificationType};
use autonoetic_types::workflow::WorkflowEventRecord;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
struct WorkflowIndexFile {
    workflow_id: String,
    root_session_id: String,
}

#[derive(Debug, Clone)]
pub struct EmergencyStopRecord {
    pub stop_id: String,
    pub scope_type: String,
    pub scope_id: String,
    pub root_session_id: String,
    pub workflow_id: Option<String>,
    pub requested_by_type: String,
    pub requested_by_id: String,
    pub reason: Option<String>,
    pub trigger_kind: String,
    pub mode: String,
    pub status: String,
    pub requested_at: String,
    pub completed_at: Option<String>,
    pub details_json: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActiveExecutionRecord {
    pub execution_id: String,
    pub root_session_id: String,
    pub workflow_id: Option<String>,
    pub task_id: Option<String>,
    pub session_id: String,
    pub agent_id: String,
    pub execution_kind: String,
    pub driver: Option<String>,
    pub pid: Option<i64>,
    pub host_id: String,
    pub status: String,
    pub started_at: String,
    pub heartbeat_at: String,
    pub stop_requested_at: Option<String>,
    pub stopped_at: Option<String>,
    pub stop_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LiveDigestEventRecord {
    pub event_id: String,
    pub root_session_id: String,
    pub source_session_id: String,
    pub turn_id: Option<String>,
    pub source_agent_id: Option<String>,
    pub source_node_id: String,
    pub event_type: String,
    pub payload: Option<String>,
    pub created_at: String,
}

/// Stable host/process identity for `active_executions.host_id` (override with `AUTONOETIC_HOST_ID`).
pub fn default_gateway_host_id() -> String {
    match std::env::var("AUTONOETIC_HOST_ID") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => format!("pid:{}", std::process::id()),
    }
}

fn memory_object_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryObject> {
    let source_type_str: String = row.get(4)?;
    let tags_str: String = row.get(11)?;
    let lineage_str: String = row.get(12)?;
    let visibility_str: String = row.get(13)?;
    let allowed_agents_str: String = row.get(14)?;

    Ok(MemoryObject {
        memory_id: row.get(0)?,
        scope: row.get(1)?,
        owner_agent_id: row.get(2)?,
        writer_agent_id: row.get(3)?,
        source_type: serde_json::from_str(&source_type_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                e.to_string().into(),
            )
        })?,
        source_ref: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        content: row.get(8)?,
        content_hash: row.get(9)?,
        confidence: row.get(10)?,
        tags: serde_json::from_str(&tags_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                e.to_string().into(),
            )
        })?,
        lineage: serde_json::from_str(&lineage_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                e.to_string().into(),
            )
        })?,
        visibility: serde_json::from_str(&visibility_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                e.to_string().into(),
            )
        })?,
        allowed_agents: serde_json::from_str(&allowed_agents_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                e.to_string().into(),
            )
        })?,
    })
}

/// Escape `\`, `%`, and `_` for embedding a literal prefix inside an SQLite `LIKE` pattern when using `ESCAPE '\\'`.
fn escape_sqlite_like_fragment(s: &str) -> String {
    let mut out = String::with_capacity(s.len().saturating_add(8));
    for ch in s.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

pub struct GatewayStore {
    conn: std::sync::Mutex<Connection>,
}

impl GatewayStore {
    pub fn open(gateway_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(gateway_dir).map_err(|e| {
            anyhow::anyhow!(
                "Failed to create gateway directory {:?}: {}",
                gateway_dir,
                e
            )
        })?;
        let db_path = gateway_dir.join("gateway.db");
        let conn = Connection::open(&db_path)?;

        // Optimizations
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;

        let store = Self {
            conn: std::sync::Mutex::new(conn),
        };
        store.migrate()?;
        if let Err(e) = store.reconcile_stale_active_executions() {
            tracing::warn!(
                target: "gateway_store",
                error = %e,
                "Failed to reconcile stale active_executions"
            );
        }
        store.backfill_workflow_index(gateway_dir)?;
        Ok(store)
    }

    fn backfill_workflow_index(&self, gateway_dir: &Path) -> Result<()> {
        let index_dir = gateway_dir
            .join("scheduler")
            .join("workflows")
            .join("index")
            .join("by_root");
        if !index_dir.exists() {
            return Ok(());
        }

        let conn = self.conn.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM workflow_index", [], |row| row.get(0))?;

        if count > 0 {
            return Ok(()); // Already backfilled
        }

        tracing::info!(target: "gateway_store", "Backfilling workflow_index from file-based index");

        for entry in std::fs::read_dir(&index_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<WorkflowIndexFile>(&content) {
                    Ok(idx) => {
                        let now = chrono::Utc::now().to_rfc3339();
                        if let Err(e) = conn.execute(
                                "INSERT OR IGNORE INTO workflow_index (root_session_id, workflow_id, created_at) VALUES (?1, ?2, ?3)",
                                rusqlite::params![idx.root_session_id, idx.workflow_id, now],
                            ) {
                                tracing::warn!(
                                    target: "gateway_store",
                                    path = %path.display(),
                                    error = %e,
                                    "Failed to backfill workflow index entry"
                                );
                            }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "gateway_store",
                            path = %path.display(),
                            error = %e,
                            "Failed to parse workflow index file"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        target: "gateway_store",
                        path = %path.display(),
                        error = %e,
                        "Failed to read workflow index file"
                    );
                }
            }
        }

        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS approvals (
                request_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                root_session_id TEXT,
                workflow_id TEXT,
                task_id TEXT,
                action_type TEXT NOT NULL,
                action_payload TEXT NOT NULL,
                reason TEXT,
                evidence_ref TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL,
                decided_at TEXT,
                decided_by TEXT
            );

            CREATE TABLE IF NOT EXISTS notifications (
                notification_id TEXT PRIMARY KEY,
                notification_type TEXT NOT NULL,
                request_id TEXT,
                target_session_id TEXT NOT NULL,
                target_agent_id TEXT,
                workflow_id TEXT,
                task_id TEXT,
                payload TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL,
                action_completed_at TEXT,
                delivered_at TEXT,
                consumed_at TEXT,
                attempt_count INTEGER NOT NULL DEFAULT 0,
                last_attempt_at TEXT,
                error_message TEXT
            );

            CREATE TABLE IF NOT EXISTS workflow_events (
                event_id TEXT PRIMARY KEY,
                workflow_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                task_id TEXT,
                agent_id TEXT,
                payload TEXT,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS artifact_refs (
                ref_id TEXT PRIMARY KEY,
                scope_type TEXT NOT NULL,
                scope_id TEXT NOT NULL,
                artifact_id TEXT NOT NULL,
                artifact_digest TEXT NOT NULL,
                created_by_agent_id TEXT NOT NULL,
                created_at TEXT NOT NULL,
                expires_at TEXT,
                revoked_at TEXT
            );

            CREATE TABLE IF NOT EXISTS causal_events (
                event_id     TEXT PRIMARY KEY,
                agent_id     TEXT NOT NULL,
                session_id   TEXT NOT NULL,
                turn_id      TEXT,
                event_seq    INTEGER NOT NULL,
                timestamp    TEXT NOT NULL,
                category     TEXT NOT NULL,
                action       TEXT NOT NULL,
                status       TEXT NOT NULL,
                target       TEXT,
                payload      TEXT,
                payload_ref  TEXT,
                evidence_ref TEXT,
                reason       TEXT
            );

            CREATE TABLE IF NOT EXISTS execution_traces (
                trace_id     TEXT PRIMARY KEY,
                event_id     TEXT,
                agent_id     TEXT NOT NULL,
                session_id   TEXT NOT NULL,
                turn_id      TEXT,
                timestamp    TEXT NOT NULL,
                tool_name    TEXT NOT NULL,
                command      TEXT,
                exit_code    INTEGER,
                stdout       TEXT,
                stderr       TEXT,
                duration_ms  INTEGER,
                success      INTEGER NOT NULL,
                error_type   TEXT,
                error_summary TEXT,
                approval_required INTEGER DEFAULT 0,
                approval_request_id TEXT,
                arguments    TEXT,
                result       TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_approvals_status ON approvals(status);
            CREATE INDEX IF NOT EXISTS idx_approvals_session ON approvals(session_id);
            CREATE INDEX IF NOT EXISTS idx_approvals_root_session ON approvals(root_session_id);
            CREATE INDEX IF NOT EXISTS idx_approvals_workflow ON approvals(workflow_id);
            CREATE INDEX IF NOT EXISTS idx_notifications_status ON notifications(status);
            CREATE INDEX IF NOT EXISTS idx_notifications_target ON notifications(target_session_id);
            CREATE INDEX IF NOT EXISTS idx_workflow_events_workflow ON workflow_events(workflow_id);
            CREATE INDEX IF NOT EXISTS idx_workflow_events_created ON workflow_events(created_at);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_artifact_refs_scope_ref
              ON artifact_refs(scope_type, scope_id, ref_id);
            CREATE INDEX IF NOT EXISTS idx_artifact_refs_artifact ON artifact_refs(artifact_id);
            CREATE INDEX IF NOT EXISTS idx_artifact_refs_digest ON artifact_refs(artifact_digest);

            CREATE INDEX IF NOT EXISTS idx_causal_agent_session ON causal_events(agent_id, session_id);
            CREATE INDEX IF NOT EXISTS idx_causal_category_action ON causal_events(category, action);
            CREATE INDEX IF NOT EXISTS idx_causal_status ON causal_events(status);
            CREATE INDEX IF NOT EXISTS idx_causal_target ON causal_events(target);
            CREATE INDEX IF NOT EXISTS idx_causal_timestamp ON causal_events(timestamp);

            CREATE INDEX IF NOT EXISTS idx_exec_agent_session ON execution_traces(agent_id, session_id);
            CREATE INDEX IF NOT EXISTS idx_exec_tool ON execution_traces(tool_name);
            CREATE INDEX IF NOT EXISTS idx_exec_success ON execution_traces(success);
            CREATE INDEX IF NOT EXISTS idx_exec_error_type ON execution_traces(error_type);
            CREATE INDEX IF NOT EXISTS idx_exec_command ON execution_traces(command);

            CREATE TABLE IF NOT EXISTS user_interactions (
                interaction_id   TEXT PRIMARY KEY,
                session_id       TEXT NOT NULL,
                root_session_id  TEXT NOT NULL,
                workflow_id      TEXT,
                task_id          TEXT,
                agent_id         TEXT NOT NULL,
                turn_id          TEXT,
                kind             TEXT NOT NULL,
                question         TEXT NOT NULL,
                context          TEXT,
                options_json     TEXT,
                allow_freeform   INTEGER NOT NULL DEFAULT 1,
                status           TEXT NOT NULL DEFAULT 'pending',
                answer_option_id TEXT,
                answer_text      TEXT,
                answered_by      TEXT,
                created_at       TEXT NOT NULL,
                answered_at      TEXT,
                expires_at       TEXT,
                checkpoint_turn_id TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_user_interactions_session ON user_interactions(session_id);
            CREATE INDEX IF NOT EXISTS idx_user_interactions_root_session ON user_interactions(root_session_id);
            CREATE INDEX IF NOT EXISTS idx_user_interactions_workflow ON user_interactions(workflow_id);
            CREATE INDEX IF NOT EXISTS idx_user_interactions_status ON user_interactions(status);
            CREATE INDEX IF NOT EXISTS idx_user_interactions_agent ON user_interactions(agent_id, created_at);

            CREATE TABLE IF NOT EXISTS workflow_index (
                root_session_id TEXT PRIMARY KEY,
                workflow_id TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS emergency_stops (
                stop_id TEXT PRIMARY KEY,
                scope_type TEXT NOT NULL,
                scope_id TEXT NOT NULL,
                root_session_id TEXT NOT NULL,
                workflow_id TEXT,
                requested_by_type TEXT NOT NULL,
                requested_by_id TEXT NOT NULL,
                reason TEXT,
                trigger_kind TEXT NOT NULL,
                mode TEXT NOT NULL,
                status TEXT NOT NULL,
                requested_at TEXT NOT NULL,
                completed_at TEXT,
                details_json TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_emergency_stops_root ON emergency_stops(root_session_id, requested_at);
            CREATE INDEX IF NOT EXISTS idx_emergency_stops_workflow ON emergency_stops(workflow_id, requested_at);
            CREATE INDEX IF NOT EXISTS idx_emergency_stops_status ON emergency_stops(status);
            CREATE INDEX IF NOT EXISTS idx_emergency_stops_requester ON emergency_stops(requested_by_type, requested_by_id, requested_at);

            CREATE TABLE IF NOT EXISTS active_executions (
                execution_id TEXT PRIMARY KEY,
                root_session_id TEXT NOT NULL,
                workflow_id TEXT,
                task_id TEXT,
                session_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                execution_kind TEXT NOT NULL,
                driver TEXT,
                pid INTEGER,
                host_id TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at TEXT NOT NULL,
                heartbeat_at TEXT NOT NULL,
                stop_requested_at TEXT,
                stopped_at TEXT,
                stop_id TEXT
            );

            CREATE TABLE IF NOT EXISTS live_digest_events (
                event_id TEXT PRIMARY KEY,
                root_session_id TEXT NOT NULL,
                source_session_id TEXT NOT NULL,
                turn_id TEXT,
                source_agent_id TEXT,
                source_node_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_active_executions_root ON active_executions(root_session_id, status);
            CREATE INDEX IF NOT EXISTS idx_active_executions_workflow ON active_executions(workflow_id, status);
            CREATE INDEX IF NOT EXISTS idx_active_executions_task ON active_executions(task_id, status);
            CREATE INDEX IF NOT EXISTS idx_active_executions_session ON active_executions(session_id, status);
            CREATE INDEX IF NOT EXISTS idx_live_digest_root_created ON live_digest_events(root_session_id, created_at);
            CREATE INDEX IF NOT EXISTS idx_live_digest_event_type ON live_digest_events(event_type, created_at);
            CREATE INDEX IF NOT EXISTS idx_live_digest_source_session ON live_digest_events(source_session_id, created_at);

            CREATE TABLE IF NOT EXISTS memories (
                memory_id TEXT PRIMARY KEY,
                scope TEXT NOT NULL,
                owner_agent_id TEXT NOT NULL,
                writer_agent_id TEXT NOT NULL,
                source_type TEXT NOT NULL DEFAULT 'agent_write',
                source_ref TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                confidence REAL,
                tags TEXT,
                lineage TEXT,
                visibility TEXT NOT NULL DEFAULT 'private',
                allowed_agents TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_memories_scope ON memories(scope);
            CREATE INDEX IF NOT EXISTS idx_memories_owner ON memories(owner_agent_id);
            CREATE INDEX IF NOT EXISTS idx_memories_visibility ON memories(visibility);
            CREATE INDEX IF NOT EXISTS idx_memories_tags ON memories(tags);

            CREATE TABLE IF NOT EXISTS memory_tags (
                memory_id TEXT NOT NULL,
                scope TEXT NOT NULL,
                tag TEXT NOT NULL,
                PRIMARY KEY (memory_id, tag)
            );

            CREATE INDEX IF NOT EXISTS idx_memory_tags_scope_tag ON memory_tags(scope, tag);
            CREATE INDEX IF NOT EXISTS idx_memory_tags_tag ON memory_tags(tag);

            INSERT OR IGNORE INTO memory_tags (memory_id, scope, tag)
            SELECT m.memory_id, m.scope, j.value
            FROM memories m, json_each(m.tags) AS j
            WHERE m.tags IS NOT NULL AND json_valid(m.tags);
            ",
        )?;
        Ok(())
    }

    fn reconcile_stale_active_executions(&self) -> Result<()> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE active_executions SET status = 'lost', stopped_at = ?1 WHERE status IN ('running', 'stop_requested') AND heartbeat_at < ?2",
            params![now, cutoff],
        )?;
        Ok(())
    }

    // --- Tier 2 memories (gateway.db) ---

    pub fn memory_upsert(&self, memory: &MemoryObject) -> Result<()> {
        let tags_json = serde_json::to_string(&memory.tags)?;
        let lineage_json = serde_json::to_string(&memory.lineage)?;
        let allowed_agents_json = serde_json::to_string(&memory.allowed_agents)?;

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO memories (
                memory_id, scope, owner_agent_id, writer_agent_id, source_type, source_ref,
                created_at, updated_at, content, content_hash, confidence, tags, lineage,
                visibility, allowed_agents
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                &memory.memory_id,
                &memory.scope,
                &memory.owner_agent_id,
                &memory.writer_agent_id,
                serde_json::to_string(&memory.source_type)?,
                &memory.source_ref,
                &memory.created_at,
                &memory.updated_at,
                &memory.content,
                &memory.content_hash,
                memory.confidence,
                tags_json,
                lineage_json,
                serde_json::to_string(&memory.visibility)?,
                allowed_agents_json,
            ],
        )?;
        tx.execute(
            "DELETE FROM memory_tags WHERE memory_id = ?1",
            params![&memory.memory_id],
        )?;
        for raw in &memory.tags {
            let t = raw.trim();
            if t.is_empty() {
                continue;
            }
            tx.execute(
                "INSERT OR IGNORE INTO memory_tags (memory_id, scope, tag) VALUES (?1, ?2, ?3)",
                params![&memory.memory_id, &memory.scope, t],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn memory_get_unrestricted(&self, memory_id: &str) -> Result<Option<MemoryObject>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM memories WHERE memory_id = ?1")?;
        let mut rows = stmt.query(params![memory_id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(memory_object_from_row(&row)?))
    }

    pub fn memory_list_ids_for_scope(
        &self,
        scope: &str,
        content_substr: Option<&str>,
    ) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from("SELECT memory_id FROM memories WHERE scope = ?1");
        if content_substr.is_some() {
            sql.push_str(" AND content LIKE ?2");
        }
        sql.push_str(" ORDER BY updated_at DESC");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = match content_substr {
            Some(q) => {
                let term = format!("%{}%", q);
                stmt.query(params![scope, term])?
            }
            None => stmt.query(params![scope])?,
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get(0)?);
        }
        Ok(out)
    }

    /// Returns IDs of memories in `scope` that are readable by `agent_id` and match all `tags`.
    /// Optional `content_substr` applies `LIKE %substr%` on content. Results are sorted by
    /// recency and capped by `limit`.
    pub fn memory_list_ids_matching_tags(
        &self,
        scope: &str,
        agent_id: &str,
        tags: &[String],
        content_substr: Option<&str>,
        limit: i64,
    ) -> Result<Vec<String>> {
        use rusqlite::types::Value;
        use std::collections::BTreeSet;

        let mut norm: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for t in tags {
            let s = t.trim();
            if s.is_empty() {
                continue;
            }
            if seen.insert(s.to_string()) {
                norm.push(s.to_string());
            }
        }
        if norm.is_empty() {
            anyhow::bail!("tags must contain at least one non-empty tag after trimming");
        }
        if limit <= 0 {
            anyhow::bail!("limit must be positive");
        }

        let mut sql = String::from("SELECT m.memory_id FROM memories m WHERE m.scope = ?1 ");
        // Keep ACL in SQL so LIMIT applies to final readable rows.
        sql.push_str(
            "AND (
                json_extract(m.visibility, '$') = 'global'
                OR (
                    json_extract(m.visibility, '$') = 'private'
                    AND (m.owner_agent_id = ?2 OR m.writer_agent_id = ?2)
                )
                OR (
                    json_extract(m.visibility, '$') = 'shared'
                    AND (
                        m.owner_agent_id = ?2
                        OR m.writer_agent_id = ?2
                        OR (
                            m.allowed_agents IS NOT NULL
                            AND json_valid(m.allowed_agents)
                            AND ?2 IN (SELECT value FROM json_each(m.allowed_agents))
                        )
                    )
                )
            ) ",
        );

        let mut next_param: i32 = 3;
        if content_substr.is_some() {
            sql.push_str(&format!("AND m.content LIKE ?{} ", next_param));
            next_param += 1;
        }
        for _ in &norm {
            sql.push_str(&format!(
                "AND EXISTS (
                    SELECT 1 FROM memory_tags mt
                    WHERE mt.memory_id = m.memory_id
                      AND mt.scope = ?1
                      AND mt.tag = ?{}
                ) ",
                next_param
            ));
            next_param += 1;
        }
        sql.push_str(&format!(
            "ORDER BY m.updated_at DESC LIMIT ?{}",
            next_param
        ));

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let mut bind: Vec<Value> = vec![
            Value::Text(scope.to_string()),
            Value::Text(agent_id.to_string()),
        ];
        if let Some(q) = content_substr {
            bind.push(Value::Text(format!("%{}%", q)));
        }
        for t in norm {
            bind.push(Value::Text(t));
        }
        bind.push(Value::Integer(limit));

        let mut rows = stmt.query(rusqlite::params_from_iter(bind.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get(0)?);
        }
        Ok(out)
    }

    pub fn memory_list_ids_owned_by(&self, owner_agent_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT memory_id FROM memories WHERE owner_agent_id = ?1 ORDER BY created_at DESC",
        )?;
        let mut rows = stmt.query(params![owner_agent_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get(0)?);
        }
        Ok(out)
    }

    pub fn memory_list_scopes_for_agent(&self, agent_id: &str) -> Result<Vec<String>> {
        const LIST_SCOPES_SQL: &str = r#"
            SELECT DISTINCT scope FROM memories
            WHERE json_extract(visibility, '$') = 'global'
               OR (json_extract(visibility, '$') = 'private'
                   AND (owner_agent_id = ?1 OR writer_agent_id = ?1))
               OR (json_extract(visibility, '$') = 'shared'
                   AND (owner_agent_id = ?1 OR writer_agent_id = ?1
                        OR (allowed_agents IS NOT NULL
                            AND json_valid(allowed_agents)
                            AND ?1 IN (SELECT value FROM json_each(allowed_agents)))))
            ORDER BY scope
        "#;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(LIST_SCOPES_SQL)?;
        let mut rows = stmt.query(params![agent_id])?;
        let mut scopes = Vec::new();
        while let Some(row) = rows.next()? {
            scopes.push(row.get(0)?);
        }
        Ok(scopes)
    }

    // --- Emergency stop & active executions ---

    pub fn insert_emergency_stop(&self, row: &EmergencyStopRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO emergency_stops (
                stop_id, scope_type, scope_id, root_session_id, workflow_id,
                requested_by_type, requested_by_id, reason, trigger_kind, mode,
                status, requested_at, completed_at, details_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                &row.stop_id,
                &row.scope_type,
                &row.scope_id,
                &row.root_session_id,
                row.workflow_id.as_deref(),
                &row.requested_by_type,
                &row.requested_by_id,
                row.reason.as_deref(),
                &row.trigger_kind,
                &row.mode,
                &row.status,
                &row.requested_at,
                row.completed_at.as_deref(),
                row.details_json.as_deref(),
            ],
        )?;
        Ok(())
    }

    pub fn update_emergency_stop_status(
        &self,
        stop_id: &str,
        status: &str,
        completed_at: Option<&str>,
        details_json: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE emergency_stops SET status = ?1, completed_at = ?2, details_json = ?3 WHERE stop_id = ?4",
            params![status, completed_at, details_json, stop_id],
        )?;
        anyhow::ensure!(
            changed == 1,
            "emergency stop '{}' not found or not updated",
            stop_id
        );
        Ok(())
    }

    pub fn list_emergency_stops_for_root_session(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<EmergencyStopRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT stop_id, scope_type, scope_id, root_session_id, workflow_id, requested_by_type,
                    requested_by_id, reason, trigger_kind, mode, status, requested_at, completed_at, details_json
             FROM emergency_stops WHERE root_session_id = ?1 ORDER BY requested_at DESC",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            Ok(EmergencyStopRecord {
                stop_id: row.get(0)?,
                scope_type: row.get(1)?,
                scope_id: row.get(2)?,
                root_session_id: row.get(3)?,
                workflow_id: row.get(4)?,
                requested_by_type: row.get(5)?,
                requested_by_id: row.get(6)?,
                reason: row.get(7)?,
                trigger_kind: row.get(8)?,
                mode: row.get(9)?,
                status: row.get(10)?,
                requested_at: row.get(11)?,
                completed_at: row.get(12)?,
                details_json: row.get(13)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn upsert_active_execution(&self, row: &ActiveExecutionRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO active_executions (
                execution_id, root_session_id, workflow_id, task_id, session_id, agent_id,
                execution_kind, driver, pid, host_id, status, started_at, heartbeat_at,
                stop_requested_at, stopped_at, stop_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                &row.execution_id,
                &row.root_session_id,
                row.workflow_id.as_deref(),
                row.task_id.as_deref(),
                &row.session_id,
                &row.agent_id,
                &row.execution_kind,
                row.driver.as_deref(),
                row.pid,
                &row.host_id,
                &row.status,
                &row.started_at,
                &row.heartbeat_at,
                row.stop_requested_at.as_deref(),
                row.stopped_at.as_deref(),
                row.stop_id.as_deref(),
            ],
        )?;
        Ok(())
    }

    pub fn touch_active_execution_heartbeat(&self, execution_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let n = conn.execute(
            "UPDATE active_executions SET heartbeat_at = ?1 WHERE execution_id = ?2",
            params![now, execution_id],
        )?;
        anyhow::ensure!(
            n == 1,
            "active execution '{}' not found for heartbeat",
            execution_id
        );
        Ok(())
    }

    pub fn complete_active_execution(
        &self,
        execution_id: &str,
        status: &str,
        stop_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let n = conn.execute(
            "UPDATE active_executions SET status = ?1, stopped_at = ?2, stop_id = ?3 WHERE execution_id = ?4",
            params![status, now, stop_id, execution_id],
        )?;
        anyhow::ensure!(
            n == 1,
            "active execution '{}' not found for completion",
            execution_id
        );
        Ok(())
    }

    pub fn list_active_executions_for_root_sqlite(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<ActiveExecutionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT execution_id, root_session_id, workflow_id, task_id, session_id, agent_id,
                    execution_kind, driver, pid, host_id, status, started_at, heartbeat_at,
                    stop_requested_at, stopped_at, stop_id
             FROM active_executions WHERE root_session_id = ?1 ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            Ok(ActiveExecutionRecord {
                execution_id: row.get(0)?,
                root_session_id: row.get(1)?,
                workflow_id: row.get(2)?,
                task_id: row.get(3)?,
                session_id: row.get(4)?,
                agent_id: row.get(5)?,
                execution_kind: row.get(6)?,
                driver: row.get(7)?,
                pid: row.get(8)?,
                host_id: row.get(9)?,
                status: row.get(10)?,
                started_at: row.get(11)?,
                heartbeat_at: row.get(12)?,
                stop_requested_at: row.get(13)?,
                stopped_at: row.get(14)?,
                stop_id: row.get(15)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // --- Approvals ---

    pub fn create_approval(&self, request: &ApprovalRequest) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let action_payload = serde_json::to_string(&request.action)?;
        conn.execute(
            "INSERT INTO approvals (
                request_id, agent_id, session_id, root_session_id, workflow_id, task_id,
                action_type, action_payload, reason, evidence_ref, status, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                request.request_id,
                request.agent_id,
                request.session_id,
                request.root_session_id,
                request.workflow_id,
                request.task_id,
                request.action.kind(),
                action_payload,
                request.reason,
                request.evidence_ref,
                "pending",
                request.created_at
            ],
        )?;
        Ok(())
    }

    fn get_approval_with_conn(
        conn: &Connection,
        request_id: &str,
    ) -> Result<Option<ApprovalRequest>> {
        conn.query_row(
            "SELECT request_id, agent_id, session_id, action_payload, created_at, workflow_id, task_id, root_session_id, status, decided_at, decided_by, reason, evidence_ref FROM approvals WHERE request_id = ?1",
            params![request_id],
            |row| {
                let action_payload: String = row.get(3)?;
                let status_str: Option<String> = row.get(8)?;
                let status = status_str.and_then(|s| match s.as_str() {
                    "approved" => Some(autonoetic_types::background::ApprovalStatus::Approved),
                    "rejected" => Some(autonoetic_types::background::ApprovalStatus::Rejected),
                    _ => None,
                });
                let action = serde_json::from_str(&action_payload).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
                })?;
                Ok(ApprovalRequest {
                    request_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    session_id: row.get(2)?,
                    action,
                    created_at: row.get(4)?,
                    workflow_id: row.get(5)?,
                    task_id: row.get(6)?,
                    root_session_id: row.get(7)?,
                    status,
                    decided_at: row.get(9)?,
                    decided_by: row.get(10)?,
                    reason: row.get(11)?,
                    evidence_ref: row.get(12)?,
                })
            },
        ).optional().map_err(Into::into)
    }

    pub fn get_approval(&self, request_id: &str) -> Result<Option<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        Self::get_approval_with_conn(&conn, request_id)
    }

    pub fn record_decision(
        &self,
        request_id: &str,
        status: &str,
        decided_by: &str,
        decided_at: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE approvals SET status = ?1, decided_by = ?2, decided_at = ?3 WHERE request_id = ?4",
            params![status, decided_by, decided_at, request_id],
        )?;
        Ok(())
    }

    pub fn get_pending_approvals(&self) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT request_id FROM approvals WHERE status = 'pending'")?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            if let Ok(id) = id_result {
                if let Ok(Some(app)) = Self::get_approval_with_conn(&conn, &id) {
                    results.push(app);
                }
            }
        }
        Ok(results)
    }

    pub fn get_pending_approvals_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals WHERE root_session_id = ?1 AND status = 'pending'",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            if let Ok(id) = id_result {
                if let Ok(Some(app)) = Self::get_approval_with_conn(&conn, &id) {
                    results.push(app);
                }
            }
        }
        Ok(results)
    }

    // --- Notifications ---

    pub fn create_notification_record(&self, n: &NotificationRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let payload = serde_json::to_string(&n.payload)?;
        conn.execute(
            "INSERT INTO notifications (
                notification_id, notification_type, request_id, target_session_id, target_agent_id,
                workflow_id, task_id, payload, status, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                n.notification_id,
                serde_json::to_string(&n.notification_type)?,
                n.request_id,
                n.target_session_id,
                n.target_agent_id,
                n.workflow_id,
                n.task_id,
                payload,
                serde_json::to_string(&n.status)?,
                n.created_at
            ],
        )?;
        Ok(())
    }

    pub fn create_notification(&self, session_id: &str, payload: &serde_json::Value) -> Result<()> {
        let n = NotificationRecord::new(
            format!("ntf-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            NotificationType::ApprovalResolved,
            session_id.to_string(),
            payload.clone(),
        );
        self.create_notification_record(&n)
    }

    pub fn list_pending_notifications(&self) -> Result<Vec<NotificationRecord>> {
        let conn = self.conn.lock().unwrap();
        let status = serde_json::to_string(&NotificationStatus::Pending)?;
        let mut stmt = conn.prepare(
            "SELECT notification_id FROM notifications WHERE status = ?1 ORDER BY created_at ASC, notification_id ASC",
        )?;
        let rows = stmt.query_map(params![status], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            if let Ok(id) = id_result {
                if let Ok(Some(n)) = Self::get_notification_with_conn(&conn, &id) {
                    results.push(n);
                }
            }
        }
        Ok(results)
    }

    fn get_notification_with_conn(
        conn: &Connection,
        id: &str,
    ) -> Result<Option<NotificationRecord>> {
        conn.query_row(
            "SELECT * FROM notifications WHERE notification_id = ?1",
            params![id],
            |row| {
                let n_type_str: String = row.get(1)?;
                let status_str: String = row.get(8)?;
                let payload_str: String = row.get(7)?;

                let notification_type = serde_json::from_str(&n_type_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                let payload = serde_json::from_str(&payload_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                let status = serde_json::from_str(&status_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        8,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

                Ok(NotificationRecord {
                    notification_id: row.get(0)?,
                    notification_type,
                    request_id: row.get(2)?,
                    target_session_id: row.get(3)?,
                    target_agent_id: row.get(4)?,
                    workflow_id: row.get(5)?,
                    task_id: row.get(6)?,
                    payload,
                    status,
                    created_at: row.get(9)?,
                    action_completed_at: row.get(10)?,
                    delivered_at: row.get(11)?,
                    consumed_at: row.get(12)?,
                    attempt_count: row.get(13)?,
                    last_attempt_at: row.get(14)?,
                    error_message: row.get(15)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_notification(&self, id: &str) -> Result<Option<NotificationRecord>> {
        let conn = self.conn.lock().unwrap();
        Self::get_notification_with_conn(&conn, id)
    }

    pub fn update_notification_status(&self, id: &str, status: NotificationStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let status_str = serde_json::to_string(&status)?;
        let now = chrono::Utc::now().to_rfc3339();

        match status {
            NotificationStatus::ActionExecuted => {
                conn.execute(
                    "UPDATE notifications SET status = ?1, action_completed_at = ?2 WHERE notification_id = ?3",
                    params![status_str, now, id],
                )?;
            }
            NotificationStatus::Delivered => {
                conn.execute(
                    "UPDATE notifications SET status = ?1, delivered_at = ?2 WHERE notification_id = ?3",
                    params![status_str, now, id],
                )?;
            }
            NotificationStatus::Consumed => {
                conn.execute(
                    "UPDATE notifications SET status = ?1, consumed_at = ?2 WHERE notification_id = ?3",
                    params![status_str, now, id],
                )?;
            }
            _ => {
                conn.execute(
                    "UPDATE notifications SET status = ?1 WHERE notification_id = ?2",
                    params![status_str, id],
                )?;
            }
        }
        Ok(())
    }

    pub fn mark_consumed(&self, id: &str) -> Result<()> {
        self.update_notification_status(id, NotificationStatus::Consumed)
    }

    pub fn list_notifications_for_session(
        &self,
        session_id: &str,
        status: NotificationStatus,
    ) -> Result<Vec<NotificationRecord>> {
        let conn = self.conn.lock().unwrap();
        let status_str = serde_json::to_string(&status)?;
        let mut stmt = conn.prepare("SELECT notification_id FROM notifications WHERE target_session_id = ?1 AND status = ?2")?;
        let rows = stmt.query_map(params![session_id, status_str], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            if let Ok(id) = id_result {
                if let Ok(Some(n)) = Self::get_notification_with_conn(&conn, &id) {
                    results.push(n);
                }
            }
        }
        Ok(results)
    }

    pub fn increment_attempt(&self, id: &str, error: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE notifications SET attempt_count = attempt_count + 1, last_attempt_at = ?1, error_message = ?2 WHERE notification_id = ?3",
            params![now, error, id],
        )?;
        Ok(())
    }

    // --- User Interactions ---

    /// Create a new user interaction (agent asked a question).
    pub fn create_user_interaction(&self, interaction: &UserInteraction) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let options_json = if interaction.options.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&interaction.options)?)
        };
        conn.execute(
            "INSERT INTO user_interactions (
                interaction_id, session_id, root_session_id, workflow_id, task_id,
                agent_id, turn_id, kind, question, context, options_json, allow_freeform,
                status, created_at, expires_at, checkpoint_turn_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                interaction.interaction_id,
                interaction.session_id,
                interaction.root_session_id,
                interaction.workflow_id,
                interaction.task_id,
                interaction.agent_id,
                interaction.turn_id,
                interaction.kind.as_str(),
                interaction.question,
                interaction.context,
                options_json,
                if interaction.allow_freeform {
                    1i32
                } else {
                    0i32
                },
                "pending",
                interaction.created_at,
                interaction.expires_at,
                interaction.checkpoint_turn_id,
            ],
        )?;
        Ok(())
    }

    /// Get a user interaction by ID.
    pub fn get_user_interaction(&self, interaction_id: &str) -> Result<Option<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        Self::get_user_interaction_with_conn(&conn, interaction_id)
    }

    fn get_user_interaction_with_conn(
        conn: &Connection,
        interaction_id: &str,
    ) -> Result<Option<UserInteraction>> {
        conn.query_row(
            "SELECT interaction_id, session_id, root_session_id, workflow_id, task_id,
                    agent_id, turn_id, kind, question, context, options_json, allow_freeform,
                    status, answer_option_id, answer_text, answered_by, created_at, answered_at,
                    expires_at, checkpoint_turn_id
             FROM user_interactions WHERE interaction_id = ?1",
            params![interaction_id],
            |row| {
                let kind_str: String = row.get(7)?;
                let status_str: String = row.get(12)?;
                let options_json_str: Option<String> = row.get(10)?;

                let kind = match kind_str.as_str() {
                    "clarification" => UserInteractionKind::Clarification,
                    "decision" => UserInteractionKind::Decision,
                    "proposal" => UserInteractionKind::Proposal,
                    "confirmation" => UserInteractionKind::Confirmation,
                    _ => UserInteractionKind::Clarification,
                };
                let status = match status_str.as_str() {
                    "answered" => UserInteractionStatus::Answered,
                    "cancelled" => UserInteractionStatus::Cancelled,
                    "expired" => UserInteractionStatus::Expired,
                    _ => UserInteractionStatus::Pending,
                };
                let options: Vec<UserInteractionOption> = options_json_str
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();

                Ok(UserInteraction {
                    interaction_id: row.get(0)?,
                    session_id: row.get(1)?,
                    root_session_id: row.get(2)?,
                    workflow_id: row.get(3)?,
                    task_id: row.get(4)?,
                    agent_id: row.get(5)?,
                    turn_id: row.get(6)?,
                    kind,
                    question: row.get(8)?,
                    context: row.get(9)?,
                    options,
                    allow_freeform: row.get::<_, i32>(11)? != 0,
                    status,
                    answer_option_id: row.get(13)?,
                    answer_text: row.get(14)?,
                    answered_by: row.get(15)?,
                    created_at: row.get(16)?,
                    answered_at: row.get(17)?,
                    expires_at: row.get(18)?,
                    checkpoint_turn_id: row.get(19)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Answer a user interaction (user provides an answer).
    pub fn answer_user_interaction(&self, answer: &UserInteractionAnswer) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        anyhow::ensure!(
            !answer.interaction_id.trim().is_empty(),
            "interaction_id must not be empty"
        );
        let interaction = Self::get_user_interaction_with_conn(&conn, &answer.interaction_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("User interaction '{}' not found", answer.interaction_id)
            })?;
        anyhow::ensure!(
            interaction.status == UserInteractionStatus::Pending,
            "User interaction '{}' is {:?}; only pending interactions can be answered",
            answer.interaction_id,
            interaction.status
        );

        let answer_option_id = answer
            .answer_option_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        let answer_text = answer
            .answer_text
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned();
        anyhow::ensure!(
            answer_option_id.is_some() || answer_text.is_some(),
            "Must provide either answer_option_id or non-empty answer_text"
        );
        anyhow::ensure!(
            !(answer_option_id.is_some() && answer_text.is_some()),
            "Provide exactly one of answer_option_id or answer_text"
        );

        if let Some(ref oid) = answer_option_id {
            let valid = interaction.options.iter().any(|opt| opt.id == *oid);
            anyhow::ensure!(
                valid,
                "Invalid answer_option_id '{}' for interaction '{}'",
                oid,
                answer.interaction_id
            );
        }
        if answer_text.is_some() {
            anyhow::ensure!(
                interaction.allow_freeform,
                "Interaction '{}' does not allow freeform answers",
                answer.interaction_id
            );
        }

        let now = chrono::Utc::now().to_rfc3339();
        let changed = conn.execute(
            "UPDATE user_interactions SET
                status = 'answered', answer_option_id = ?1, answer_text = ?2,
                answered_by = ?3, answered_at = ?4
             WHERE interaction_id = ?5 AND status = 'pending'",
            params![
                answer_option_id,
                answer_text,
                answer.answered_by,
                now,
                answer.interaction_id,
            ],
        )?;
        anyhow::ensure!(
            changed == 1,
            "User interaction '{}' was not updated (status changed concurrently)",
            answer.interaction_id
        );
        Ok(())
    }

    /// Cancel a user interaction (e.g., when workflow is cancelled or timed out).
    pub fn cancel_user_interaction(&self, interaction_id: &str, reason: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        anyhow::ensure!(
            !interaction_id.trim().is_empty(),
            "interaction_id must not be empty"
        );
        anyhow::ensure!(!reason.trim().is_empty(), "reason must not be empty");

        let interaction = Self::get_user_interaction_with_conn(&conn, interaction_id)?
            .ok_or_else(|| anyhow::anyhow!("User interaction '{}' not found", interaction_id))?;
        anyhow::ensure!(
            interaction.status == UserInteractionStatus::Pending,
            "User interaction '{}' is {:?}; only pending interactions can be cancelled",
            interaction_id,
            interaction.status
        );

        let changed = conn.execute(
            "UPDATE user_interactions SET status = 'cancelled', answer_text = ?1 WHERE interaction_id = ?2 AND status = 'pending'",
            params![reason, interaction_id],
        )?;
        anyhow::ensure!(
            changed == 1,
            "User interaction '{}' was not cancelled (status changed concurrently)",
            interaction_id
        );
        Ok(())
    }

    /// Expire timed-out user interactions.
    pub fn expire_timed_out_interactions(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions
             WHERE status = 'pending' AND expires_at IS NOT NULL AND expires_at < ?1",
        )?;
        let rows = stmt.query_map(params![now], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut expired_ids = Vec::new();
        for row in rows {
            if let Ok(id) = row {
                conn.execute(
                    "UPDATE user_interactions SET status = 'expired' WHERE interaction_id = ?1",
                    params![id],
                )?;
                expired_ids.push(id);
            }
        }
        Ok(expired_ids)
    }

    /// List pending user interactions for a session.
    pub fn get_pending_interactions_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions WHERE session_id = ?1 AND status = 'pending'",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for row in rows {
            if let Ok(id) = row {
                if let Ok(Some(interaction)) = Self::get_user_interaction_with_conn(&conn, &id) {
                    results.push(interaction);
                }
            }
        }
        Ok(results)
    }

    /// List pending user interactions for a root session.
    pub fn get_pending_interactions_for_root_session(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions WHERE root_session_id = ?1 AND status = 'pending'",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for row in rows {
            if let Ok(id) = row {
                if let Ok(Some(interaction)) = Self::get_user_interaction_with_conn(&conn, &id) {
                    results.push(interaction);
                }
            }
        }
        Ok(results)
    }

    /// All `user_interactions` rows for this session line or the same root session (`session_id`
    /// appears either as `session_id` or `root_session_id`).
    pub fn list_user_interactions_for_session_trace(
        &self,
        session_id: &str,
    ) -> Result<Vec<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions \
             WHERE session_id = ?1 OR root_session_id = ?1 \
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for row in rows {
            if let Ok(id) = row {
                if let Ok(Some(interaction)) = Self::get_user_interaction_with_conn(&conn, &id) {
                    results.push(interaction);
                }
            }
        }
        Ok(results)
    }

    /// `user_interactions` rows bound to a workflow (may be empty).
    pub fn list_user_interactions_for_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions \
             WHERE workflow_id = ?1 \
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![workflow_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for row in rows {
            if let Ok(id) = row {
                if let Ok(Some(interaction)) = Self::get_user_interaction_with_conn(&conn, &id) {
                    results.push(interaction);
                }
            }
        }
        Ok(results)
    }

    // --- Artifact refs ---

    pub fn create_artifact_ref(&self, record: &ArtifactRefRecord) -> Result<()> {
        if record.ref_id.is_empty() {
            return Err(anyhow::anyhow!("artifact ref_id must not be empty"));
        }
        if record.scope_id.is_empty() {
            return Err(anyhow::anyhow!("artifact scope_id must not be empty"));
        }
        if record.artifact_id.is_empty() {
            return Err(anyhow::anyhow!("artifact_id must not be empty"));
        }
        if record.artifact_digest.is_empty() {
            return Err(anyhow::anyhow!("artifact_digest must not be empty"));
        }
        if record.created_by_agent_id.is_empty() {
            return Err(anyhow::anyhow!("created_by_agent_id must not be empty"));
        }

        Self::parse_rfc3339_utc(&record.created_at, "created_at")?;
        if let Some(expires_at) = record.expires_at.as_deref() {
            Self::parse_rfc3339_utc(expires_at, "expires_at")?;
        }
        if let Some(revoked_at) = record.revoked_at.as_deref() {
            Self::parse_rfc3339_utc(revoked_at, "revoked_at")?;
        }

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO artifact_refs (
                ref_id, scope_type, scope_id, artifact_id, artifact_digest, created_by_agent_id,
                created_at, expires_at, revoked_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.ref_id,
                record.scope_type.as_str(),
                record.scope_id,
                record.artifact_id,
                record.artifact_digest,
                record.created_by_agent_id,
                record.created_at,
                record.expires_at,
                record.revoked_at
            ],
        )?;
        Ok(())
    }

    pub fn resolve_artifact_ref(
        &self,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
        ref_id: &str,
    ) -> Result<Option<ArtifactRefRecord>> {
        let conn = self.conn.lock().unwrap();
        Self::resolve_artifact_ref_with_conn(&conn, scope_type, scope_id, ref_id)
    }

    pub fn list_artifact_refs_for_scope(
        &self,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
    ) -> Result<Vec<ArtifactRefRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT
                ref_id, scope_type, scope_id, artifact_id, artifact_digest, created_by_agent_id,
                created_at, expires_at, revoked_at
             FROM artifact_refs
             WHERE scope_type = ?1 AND scope_id = ?2
             ORDER BY created_at ASC, ref_id ASC",
        )?;
        let rows = stmt.query_map(
            params![scope_type.as_str(), scope_id],
            Self::artifact_ref_from_row,
        )?;

        let now = chrono::Utc::now();
        let mut refs = Vec::new();
        for row in rows {
            let record = row?;
            if Self::artifact_ref_is_active(&record, now)? {
                refs.push(record);
            }
        }
        Ok(refs)
    }

    pub fn revoke_artifact_ref(
        &self,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
        ref_id: &str,
        revoked_at: Option<&str>,
    ) -> Result<bool> {
        let revoked_at = revoked_at
            .map(str::to_string)
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        Self::parse_rfc3339_utc(&revoked_at, "revoked_at")?;

        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE artifact_refs
             SET revoked_at = ?1
             WHERE scope_type = ?2
               AND scope_id = ?3
               AND ref_id = ?4
               AND revoked_at IS NULL",
            params![revoked_at, scope_type.as_str(), scope_id, ref_id],
        )?;
        Ok(updated > 0)
    }

    fn resolve_artifact_ref_with_conn(
        conn: &Connection,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
        ref_id: &str,
    ) -> Result<Option<ArtifactRefRecord>> {
        let record = conn
            .query_row(
                "SELECT
                    ref_id, scope_type, scope_id, artifact_id, artifact_digest, created_by_agent_id,
                    created_at, expires_at, revoked_at
                 FROM artifact_refs
                 WHERE scope_type = ?1 AND scope_id = ?2 AND ref_id = ?3",
                params![scope_type.as_str(), scope_id, ref_id],
                Self::artifact_ref_from_row,
            )
            .optional()?;

        let Some(record) = record else {
            return Ok(None);
        };

        if Self::artifact_ref_is_active(&record, chrono::Utc::now())? {
            Ok(Some(record))
        } else {
            Ok(None)
        }
    }

    fn artifact_ref_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRefRecord> {
        let scope_type_raw: String = row.get(1)?;
        let scope_type = ArtifactRefScopeType::from_str(&scope_type_raw).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid artifact ref scope_type: {scope_type_raw}"),
                )),
            )
        })?;

        Ok(ArtifactRefRecord {
            ref_id: row.get(0)?,
            scope_type,
            scope_id: row.get(2)?,
            artifact_id: row.get(3)?,
            artifact_digest: row.get(4)?,
            created_by_agent_id: row.get(5)?,
            created_at: row.get(6)?,
            expires_at: row.get(7)?,
            revoked_at: row.get(8)?,
        })
    }

    fn artifact_ref_is_active(
        record: &ArtifactRefRecord,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        if let Some(revoked_at) = record.revoked_at.as_deref() {
            Self::parse_rfc3339_utc(revoked_at, "revoked_at")?;
            return Ok(false);
        }
        if let Some(expires_at) = record.expires_at.as_deref() {
            let expires_at = Self::parse_rfc3339_utc(expires_at, "expires_at")?;
            if now >= expires_at {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn parse_rfc3339_utc(
        value: &str,
        field_name: &'static str,
    ) -> Result<chrono::DateTime<chrono::Utc>> {
        let dt = chrono::DateTime::parse_from_rfc3339(value).map_err(|e| {
            anyhow::anyhow!(
                "invalid RFC3339 timestamp for artifact_refs.{}: {}",
                field_name,
                e
            )
        })?;
        Ok(dt.with_timezone(&chrono::Utc))
    }

    // --- Workflow events ---

    pub fn append_workflow_event(&self, event: &WorkflowEventRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let payload = serde_json::to_string(&event.payload)?;
        conn.execute(
            "INSERT INTO workflow_events (
                event_id, workflow_id, event_type, task_id, agent_id, payload, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                event.event_id,
                event.workflow_id,
                event.event_type,
                event.task_id,
                event.agent_id,
                payload,
                event.occurred_at
            ],
        )?;
        Ok(())
    }

    pub fn list_workflow_events(&self, workflow_id: &str) -> Result<Vec<WorkflowEventRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM workflow_events WHERE workflow_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![workflow_id], |row| {
            let payload_str: String = row.get(5)?;
            let payload = serde_json::from_str(&payload_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(WorkflowEventRecord {
                event_id: row.get(0)?,
                workflow_id: row.get(1)?,
                event_type: row.get(2)?,
                task_id: row.get(3)?,
                agent_id: row.get(4)?,
                payload,
                occurred_at: row.get(6)?,
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
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
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        let mut param_idx = 1;

        if let Some(name) = tool_name {
            conditions.push("tool_name = ?");
            params.push(rusqlite::types::Value::Text(name.to_string()));
            param_idx += 1;
        }

        if let Some(s) = success {
            conditions.push("success = ?");
            params.push(rusqlite::types::Value::Integer(if s { 1 } else { 0 }));
            param_idx += 1;
        }

        if let Some(et) = error_type {
            conditions.push("error_type = ?");
            params.push(rusqlite::types::Value::Text(et.to_string()));
            param_idx += 1;
        }

        if let Some(pattern) = command_pattern {
            conditions.push("command LIKE ?");
            params.push(rusqlite::types::Value::Text(format!("%{}%", pattern)));
            param_idx += 1;
        }

        if let Some(aid) = agent_id {
            conditions.push("agent_id = ?");
            params.push(rusqlite::types::Value::Text(aid.to_string()));
            param_idx += 1;
        }

        if let Some(sid) = session_branch {
            conditions.push("(session_id = ? OR session_id LIKE ? ESCAPE '\\')");
            params.push(rusqlite::types::Value::Text(sid.to_string()));
            let escaped = escape_sqlite_like_fragment(sid);
            params.push(rusqlite::types::Value::Text(format!("{}/%", escaped)));
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
        let mut params_with_limit = params.clone();
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

    pub fn list_workflow_events_since(
        &self,
        workflow_id: &str,
        since: &str,
    ) -> Result<Vec<WorkflowEventRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM workflow_events WHERE workflow_id = ?1 AND created_at > ?2 ORDER BY created_at ASC")?;
        let rows = stmt.query_map(params![workflow_id, since], |row| {
            let payload_str: String = row.get(5)?;
            let payload = serde_json::from_str(&payload_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(WorkflowEventRecord {
                event_id: row.get(0)?,
                workflow_id: row.get(1)?,
                event_type: row.get(2)?,
                task_id: row.get(3)?,
                agent_id: row.get(4)?,
                payload,
                occurred_at: row.get(6)?,
            })
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn cleanup_stale_notifications(&self, max_age_hours: u64) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let cutoff =
            (chrono::Utc::now() - chrono::Duration::hours(max_age_hours as i64)).to_rfc3339();
        let rows = conn.execute(
            "DELETE FROM notifications WHERE consumed_at < ?1 OR (status = ?2 AND created_at < ?3)",
            params![
                cutoff,
                serde_json::to_string(&NotificationStatus::Failed)?,
                cutoff
            ],
        )?;
        Ok(rows as u64)
    }

    // --- Workflow Index ---

    pub fn set_workflow_index(&self, root_session_id: &str, workflow_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO workflow_index (root_session_id, workflow_id, created_at) VALUES (?1, ?2, ?3)",
            params![root_session_id, workflow_id, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn resolve_workflow_id(&self, root_session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result: Option<String> = conn
            .query_row(
                "SELECT workflow_id FROM workflow_index WHERE root_session_id = ?1",
                params![root_session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::GatewayStore;
    use anyhow::Result;
    use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
    use autonoetic_types::background::{
        UserInteraction, UserInteractionAnswer, UserInteractionKind, UserInteractionOption,
    };

    fn artifact_ref(
        ref_id: &str,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
        expires_at: Option<String>,
    ) -> ArtifactRefRecord {
        ArtifactRefRecord {
            ref_id: ref_id.to_string(),
            scope_type,
            scope_id: scope_id.to_string(),
            artifact_id: "art_abcd1234".to_string(),
            artifact_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_string(),
            created_by_agent_id: "planner.default".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at,
            revoked_at: None,
        }
    }

    fn pending_interaction(
        interaction_id: &str,
        allow_freeform: bool,
        options: Vec<UserInteractionOption>,
    ) -> UserInteraction {
        UserInteraction {
            interaction_id: interaction_id.to_string(),
            session_id: "sess-1".to_string(),
            root_session_id: "sess-1".to_string(),
            workflow_id: None,
            task_id: None,
            agent_id: "planner.default".to_string(),
            turn_id: "turn-1".to_string(),
            kind: UserInteractionKind::Decision,
            question: "Choose one".to_string(),
            context: None,
            options,
            allow_freeform,
            status: autonoetic_types::background::UserInteractionStatus::Pending,
            answer_option_id: None,
            answer_text: None,
            answered_by: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            answered_at: None,
            expires_at: None,
            checkpoint_turn_id: Some("turn-1".to_string()),
        }
    }

    #[test]
    fn test_artifact_ref_migration_idempotent_and_roundtrip() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        // Ensure migration can be safely run multiple times.
        store.migrate()?;
        store.migrate()?;

        let record = artifact_ref(
            "ar.wf9f3.001.k7p2",
            ArtifactRefScopeType::Workflow,
            "wf-123",
            None,
        );
        store.create_artifact_ref(&record)?;

        let resolved = store.resolve_artifact_ref(
            ArtifactRefScopeType::Workflow,
            "wf-123",
            "ar.wf9f3.001.k7p2",
        )?;
        assert_eq!(resolved, Some(record));

        Ok(())
    }

    #[test]
    fn test_artifact_ref_resolution_is_scope_strict() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let record = artifact_ref(
            "ar.sess8f1.004.0x9c",
            ArtifactRefScopeType::Session,
            "sess-1",
            None,
        );
        store.create_artifact_ref(&record)?;

        let correct = store.resolve_artifact_ref(
            ArtifactRefScopeType::Session,
            "sess-1",
            "ar.sess8f1.004.0x9c",
        )?;
        assert!(correct.is_some());

        let wrong_scope_id = store.resolve_artifact_ref(
            ArtifactRefScopeType::Session,
            "sess-2",
            "ar.sess8f1.004.0x9c",
        )?;
        assert!(wrong_scope_id.is_none());

        let wrong_scope_type = store.resolve_artifact_ref(
            ArtifactRefScopeType::Workflow,
            "sess-1",
            "ar.sess8f1.004.0x9c",
        )?;
        assert!(wrong_scope_type.is_none());

        Ok(())
    }

    #[test]
    fn test_artifact_ref_revocation_and_expiry_filter_resolution_and_list() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let active = artifact_ref(
            "ar.wf9f3.010.a1a1",
            ArtifactRefScopeType::Workflow,
            "wf-456",
            None,
        );
        store.create_artifact_ref(&active)?;

        let expired = artifact_ref(
            "ar.wf9f3.011.b2b2",
            ArtifactRefScopeType::Workflow,
            "wf-456",
            Some((chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339()),
        );
        store.create_artifact_ref(&expired)?;

        let revoked = artifact_ref(
            "ar.wf9f3.012.c3c3",
            ArtifactRefScopeType::Workflow,
            "wf-456",
            Some((chrono::Utc::now() + chrono::Duration::seconds(600)).to_rfc3339()),
        );
        store.create_artifact_ref(&revoked)?;
        let first_revoke = store.revoke_artifact_ref(
            ArtifactRefScopeType::Workflow,
            "wf-456",
            "ar.wf9f3.012.c3c3",
            None,
        )?;
        assert!(first_revoke);
        let second_revoke = store.revoke_artifact_ref(
            ArtifactRefScopeType::Workflow,
            "wf-456",
            "ar.wf9f3.012.c3c3",
            None,
        )?;
        assert!(!second_revoke);

        let active_resolved = store.resolve_artifact_ref(
            ArtifactRefScopeType::Workflow,
            "wf-456",
            "ar.wf9f3.010.a1a1",
        )?;
        assert!(active_resolved.is_some());

        let expired_resolved = store.resolve_artifact_ref(
            ArtifactRefScopeType::Workflow,
            "wf-456",
            "ar.wf9f3.011.b2b2",
        )?;
        assert!(expired_resolved.is_none());

        let revoked_resolved = store.resolve_artifact_ref(
            ArtifactRefScopeType::Workflow,
            "wf-456",
            "ar.wf9f3.012.c3c3",
        )?;
        assert!(revoked_resolved.is_none());

        let refs = store.list_artifact_refs_for_scope(ArtifactRefScopeType::Workflow, "wf-456")?;
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].ref_id, "ar.wf9f3.010.a1a1");

        Ok(())
    }

    #[test]
    fn test_execution_traces_captures_full_stdout_stderr() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let large_stdout = "A".repeat(10000); // Large stdout
        let large_stderr = "B".repeat(10000); // Large stderr

        // Test successful execution
        let success_trace = autonoetic_types::causal_chain::ExecutionTraceRecord {
            trace_id: "trace-success".to_string(),
            event_id: None,
            agent_id: "coder.default".to_string(),
            session_id: "sess-123".to_string(),
            turn_id: Some("turn-001".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            tool_name: "sandbox.exec".to_string(),
            command: Some("pytest tests/".to_string()),
            exit_code: Some(0),
            stdout: Some(large_stdout.clone()),
            stderr: Some("".to_string()),
            duration_ms: 1500,
            success: 1,
            error_type: None,
            error_summary: None,
            approval_required: None,
            approval_request_id: None,
            arguments: Some(r#"{"command": "pytest tests/"}"#.to_string()),
            result: Some(r#"{"ok": true, "exit_code": 0}"#.to_string()),
        };
        store.create_execution_trace(&success_trace)?;

        // Test failed execution
        let fail_trace = autonoetic_types::causal_chain::ExecutionTraceRecord {
            trace_id: "trace-fail".to_string(),
            event_id: None,
            agent_id: "coder.default".to_string(),
            session_id: "sess-123".to_string(),
            turn_id: Some("turn-002".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            tool_name: "sandbox.exec".to_string(),
            command: Some("python script.py".to_string()),
            exit_code: Some(1),
            stdout: Some("Some output".to_string()),
            stderr: Some(large_stderr.clone()),
            duration_ms: 500,
            success: 0,
            error_type: Some("compilation".to_string()),
            error_summary: Some("SyntaxError: invalid syntax".to_string()),
            approval_required: None,
            approval_request_id: None,
            arguments: Some(r#"{"command": "python script.py"}"#.to_string()),
            result: Some(r#"{"ok": false, "exit_code": 1}"#.to_string()),
        };
        store.create_execution_trace(&fail_trace)?;

        // Verify successful execution
        let traces = store.search_execution_traces(
            Some("sandbox.exec"),
            Some(true),
            None,
            None,
            None,
            None,
            100,
        )?;
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].trace_id, "trace-success");
        assert_eq!(traces[0].stdout.as_ref().unwrap().len(), 10000);
        assert_eq!(traces[0].exit_code, Some(0));

        // Verify failed execution
        let fail_traces = store.search_execution_traces(
            Some("sandbox.exec"),
            Some(false),
            Some("compilation"),
            None,
            None,
            None,
            100,
        )?;
        assert_eq!(fail_traces.len(), 1);
        assert_eq!(fail_traces[0].trace_id, "trace-fail");
        assert_eq!(fail_traces[0].stderr.as_ref().unwrap().len(), 10000);
        assert_eq!(fail_traces[0].exit_code, Some(1));
        assert_eq!(fail_traces[0].error_type.as_deref(), Some("compilation"));

        Ok(())
    }

    #[test]
    fn answer_user_interaction_validates_inputs_and_status() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let interaction = pending_interaction(
            "ui-answer-1",
            false,
            vec![UserInteractionOption {
                id: "opt-a".to_string(),
                label: "Option A".to_string(),
                value: "A".to_string(),
            }],
        );
        store.create_user_interaction(&interaction)?;

        let invalid_option = store.answer_user_interaction(&UserInteractionAnswer {
            interaction_id: "ui-answer-1".to_string(),
            answer_option_id: Some("missing".to_string()),
            answer_text: None,
            answered_by: "test".to_string(),
        });
        assert!(invalid_option.is_err());

        let disallowed_freeform = store.answer_user_interaction(&UserInteractionAnswer {
            interaction_id: "ui-answer-1".to_string(),
            answer_option_id: None,
            answer_text: Some("freeform".to_string()),
            answered_by: "test".to_string(),
        });
        assert!(disallowed_freeform.is_err());

        store.answer_user_interaction(&UserInteractionAnswer {
            interaction_id: "ui-answer-1".to_string(),
            answer_option_id: Some("opt-a".to_string()),
            answer_text: None,
            answered_by: "test".to_string(),
        })?;

        let second_answer = store.answer_user_interaction(&UserInteractionAnswer {
            interaction_id: "ui-answer-1".to_string(),
            answer_option_id: Some("opt-a".to_string()),
            answer_text: None,
            answered_by: "test".to_string(),
        });
        assert!(second_answer.is_err());

        let unknown = store.answer_user_interaction(&UserInteractionAnswer {
            interaction_id: "ui-missing".to_string(),
            answer_option_id: Some("opt-a".to_string()),
            answer_text: None,
            answered_by: "test".to_string(),
        });
        assert!(unknown.is_err());

        Ok(())
    }

    #[test]
    fn cancel_user_interaction_requires_pending_interaction() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let interaction = pending_interaction("ui-cancel-1", true, vec![]);
        store.create_user_interaction(&interaction)?;

        store.cancel_user_interaction("ui-cancel-1", "cancelled by test")?;

        let second_cancel = store.cancel_user_interaction("ui-cancel-1", "again");
        assert!(second_cancel.is_err());

        let unknown = store.cancel_user_interaction("ui-missing", "x");
        assert!(unknown.is_err());

        Ok(())
    }
}
