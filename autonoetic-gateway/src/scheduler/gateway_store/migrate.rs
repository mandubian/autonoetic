use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::Path;

use super::WorkflowIndexFile;

const SCHEMA_VERSION_LATEST: i64 = 4;

pub(super) fn migrate(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at TEXT NOT NULL
        );",
    )?;

    let current_version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;

    anyhow::ensure!(
        current_version <= SCHEMA_VERSION_LATEST,
        "gateway.db schema version ({}) is newer than this binary supports ({})",
        current_version,
        SCHEMA_VERSION_LATEST
    );

    if current_version >= SCHEMA_VERSION_LATEST {
        return Ok(());
    }

    if current_version < 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(
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
            decided_by TEXT,
            approval_level TEXT NOT NULL DEFAULT 'operator'
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
            expires_at TEXT
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

        CREATE TABLE IF NOT EXISTS agent_revisions (
            revision_id TEXT PRIMARY KEY,
            agent_id TEXT NOT NULL,
            base_revision_id TEXT,
            artifact_id TEXT,
            content_digest TEXT NOT NULL,
            runtime_lock_hash TEXT NOT NULL,
            manifest_hash TEXT NOT NULL,
            created_at TEXT NOT NULL,
            created_by_type TEXT NOT NULL,
            created_by_id TEXT NOT NULL,
            source_kind TEXT NOT NULL,
            source_ref TEXT,
            origin_node_id TEXT NOT NULL,
            trust_domain TEXT NOT NULL,
            status TEXT NOT NULL,
            metadata_json TEXT,
            short_id TEXT
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_revisions_agent_content
          ON agent_revisions(agent_id, content_digest);
        CREATE INDEX IF NOT EXISTS idx_agent_revisions_agent ON agent_revisions(agent_id);
        CREATE INDEX IF NOT EXISTS idx_agent_revisions_status ON agent_revisions(status);

        CREATE TABLE IF NOT EXISTS agent_aliases (
            alias_id TEXT PRIMARY KEY,
            agent_id TEXT NOT NULL,
            revision_id TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            updated_by_type TEXT NOT NULL,
            updated_by_id TEXT NOT NULL,
            reason TEXT
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_aliases_agent ON agent_aliases(agent_id);
        CREATE INDEX IF NOT EXISTS idx_agent_aliases_revision ON agent_aliases(revision_id);

        CREATE TABLE IF NOT EXISTS session_agent_bindings (
            session_id TEXT PRIMARY KEY,
            root_session_id TEXT NOT NULL,
            alias_id TEXT,
            agent_id TEXT NOT NULL,
            revision_id TEXT NOT NULL,
            runtime_lock_hash TEXT NOT NULL,
            home_node_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            requested_target TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_session_agent_bindings_root ON session_agent_bindings(root_session_id);
        CREATE INDEX IF NOT EXISTS idx_session_agent_bindings_revision ON session_agent_bindings(revision_id);

        CREATE TABLE IF NOT EXISTS promotion_history (
            promotion_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            alias_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            previous_revision_id TEXT,
            new_revision_id TEXT NOT NULL,
            source_eval_run_id TEXT,
            reason TEXT,
            created_at TEXT NOT NULL,
            created_by_type TEXT NOT NULL,
            created_by_id TEXT NOT NULL,
            origin_node_id TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_promotion_history_agent ON promotion_history(agent_id);
        CREATE INDEX IF NOT EXISTS idx_promotion_history_revision ON promotion_history(new_revision_id);
        CREATE INDEX IF NOT EXISTS idx_promotion_history_alias ON promotion_history(alias_id);

        CREATE TABLE IF NOT EXISTS eval_suites (
            suite_id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            description TEXT NOT NULL,
            spec_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            created_by_type TEXT NOT NULL,
            created_by_id TEXT NOT NULL,
            origin_node_id TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS eval_runs (
            eval_run_id TEXT PRIMARY KEY,
            suite_id TEXT NOT NULL,
            subject_agent_id TEXT NOT NULL,
            subject_revision_id TEXT NOT NULL,
            baseline_revision_id TEXT,
            status TEXT NOT NULL,
            queued_at TEXT NOT NULL,
            started_at TEXT,
            completed_at TEXT,
            summary_json TEXT NOT NULL,
            report_handle TEXT,
            origin_node_id TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_eval_runs_subject ON eval_runs(subject_agent_id, subject_revision_id);
        CREATE INDEX IF NOT EXISTS idx_eval_runs_suite ON eval_runs(suite_id, status);

        CREATE TABLE IF NOT EXISTS eval_case_results (
            eval_run_id TEXT NOT NULL,
            case_id TEXT NOT NULL,
            status TEXT NOT NULL,
            score REAL,
            session_id TEXT,
            notes TEXT,
            output_json TEXT NOT NULL,
            PRIMARY KEY (eval_run_id, case_id)
        );

        CREATE INDEX IF NOT EXISTS idx_eval_case_results_run ON eval_case_results(eval_run_id);
        ",
    )?;

        tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS short_id_index (
            short_id TEXT PRIMARY KEY,
            revision_id TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_short_id_index_revision ON short_id_index(revision_id);

        CREATE TABLE IF NOT EXISTS credentials (
            credential_id TEXT PRIMARY KEY,
            service TEXT NOT NULL,
            secret_name TEXT NOT NULL,
            inject_as TEXT,
            created_by_agent TEXT,
            expires_at TEXT,
            shared_with TEXT,
            allowed_hosts TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_credentials_service ON credentials(service);
        CREATE INDEX IF NOT EXISTS idx_credentials_agent ON credentials(created_by_agent);

        CREATE TABLE IF NOT EXISTS session_transcripts (
            transcript_id  TEXT PRIMARY KEY,
            session_id     TEXT NOT NULL UNIQUE,
            root_session_id TEXT NOT NULL,
            agent_id       TEXT NOT NULL,
            revision_id    TEXT,
            user_id        TEXT,
            started_at     TEXT NOT NULL,
            ended_at       TEXT,
            status         TEXT NOT NULL DEFAULT 'active',
            turn_count     INTEGER NOT NULL DEFAULT 0,
            transcript_handle TEXT,
            excerpt        TEXT,
            origin_node_id TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_session_transcripts_agent ON session_transcripts(agent_id);
        CREATE INDEX IF NOT EXISTS idx_session_transcripts_root ON session_transcripts(root_session_id);
        CREATE INDEX IF NOT EXISTS idx_session_transcripts_started ON session_transcripts(started_at);
        CREATE INDEX IF NOT EXISTS idx_session_transcripts_status ON session_transcripts(status);

        CREATE VIRTUAL TABLE IF NOT EXISTS session_transcripts_fts USING fts5(
            excerpt,
            content='session_transcripts',
            content_rowid='rowid'
        );

        CREATE TRIGGER IF NOT EXISTS session_transcripts_ai AFTER INSERT ON session_transcripts BEGIN
            INSERT INTO session_transcripts_fts(rowid, excerpt) VALUES (new.rowid, new.excerpt);
        END;

        CREATE TRIGGER IF NOT EXISTS session_transcripts_ad AFTER DELETE ON session_transcripts BEGIN
            INSERT INTO session_transcripts_fts(session_transcripts_fts, rowid, excerpt) VALUES('delete', old.rowid, old.excerpt);
        END;

        CREATE TRIGGER IF NOT EXISTS session_transcripts_au AFTER UPDATE ON session_transcripts BEGIN
            INSERT INTO session_transcripts_fts(session_transcripts_fts, rowid, excerpt) VALUES('delete', old.rowid, old.excerpt);
            INSERT INTO session_transcripts_fts(rowid, excerpt) VALUES (new.rowid, new.excerpt);
        END;
        ",
    )?;

        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            params![1_i64, "initial_schema", chrono::Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
    }

    apply_memories_expires_at_v2(conn)?;
    apply_memories_drop_allowed_agents_v3(conn)?;
    apply_session_approval_grants_v4(conn)?;

    Ok(())
}

fn apply_memories_expires_at_v2(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 2 {
        return Ok(());
    }
    let col_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'expires_at'",
        [],
        |row| row.get(0),
    )?;
    if col_count == 0 {
        conn.execute("ALTER TABLE memories ADD COLUMN expires_at TEXT", [])?;
    }
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            2_i64,
            "memories_expires_at",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_memories_drop_allowed_agents_v3(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 3 {
        return Ok(());
    }
    let col_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'allowed_agents'",
        [],
        |row| row.get(0),
    )?;
    if col_count > 0 {
        conn.execute("ALTER TABLE memories DROP COLUMN allowed_agents", [])?;
    }
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            3_i64,
            "memories_drop_allowed_agents",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

pub(super) fn backfill_workflow_index(conn: &Connection, gateway_dir: &Path) -> Result<()> {
    let index_dir = gateway_dir
        .join("scheduler")
        .join("workflows")
        .join("index")
        .join("by_root");
    if !index_dir.exists() {
        return Ok(());
    }

    let count: i64 = conn.query_row("SELECT COUNT(*) FROM workflow_index", [], |row| row.get(0))?;

    if count > 0 {
        return Ok(());
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

pub(super) fn reconcile_stale_active_executions(conn: &Connection) -> Result<()> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE active_executions SET status = 'lost', stopped_at = ?1 WHERE status IN ('running', 'stop_requested') AND heartbeat_at < ?2",
        params![now, cutoff],
    )?;
    Ok(())
}

fn apply_session_approval_grants_v4(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 4 {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_approval_grants (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            root_session_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            host TEXT NOT NULL,
            granted_by TEXT NOT NULL,
            granted_at TEXT NOT NULL,
            source_approval_id TEXT,
            UNIQUE(root_session_id, agent_id, host)
        );
        CREATE INDEX IF NOT EXISTS idx_session_grants_root_agent
          ON session_approval_grants(root_session_id, agent_id);
        CREATE INDEX IF NOT EXISTS idx_session_grants_root
          ON session_approval_grants(root_session_id);",
    )?;
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            4_i64,
            "session_approval_grants",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}
