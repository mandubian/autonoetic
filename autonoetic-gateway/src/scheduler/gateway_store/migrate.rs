use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::Path;

use super::WorkflowIndexFile;

const SCHEMA_VERSION_LATEST: i64 = 21;

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
            enforced_rules TEXT NOT NULL DEFAULT '[\"R+++3\"]',
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
            expires_at TEXT,
            revision_id TEXT,
            binding_session_id TEXT,
            alias_ref TEXT,
            quarantine_reason TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_memories_scope ON memories(scope);
        CREATE INDEX IF NOT EXISTS idx_memories_owner ON memories(owner_agent_id);
        CREATE INDEX IF NOT EXISTS idx_memories_visibility ON memories(visibility);
        CREATE INDEX IF NOT EXISTS idx_memories_tags ON memories(tags);
        CREATE INDEX IF NOT EXISTS idx_memories_revision_id ON memories(revision_id);

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
    apply_user_profiles_v5(conn)?;
    apply_approvals_decision_reason_v6(conn)?;
    apply_published_reports_and_hooks_v7(conn)?;
    apply_scheduled_jobs_v8(conn)?;
    apply_scheduled_jobs_v9(conn)?;
    apply_credential_setup_state_v10(conn)?;
    apply_agent_messages_v11(conn)?;
    apply_credential_refresh_fields_v12(conn)?;
    apply_admin_proposals_v13(conn)?;
    apply_memories_revision_provenance_v14(conn)?;
    apply_session_grant_revocation_v15(conn)?;
    apply_grant_scope_and_session_v16(conn)?;
    apply_grant_targets_table_v17(conn)?;
    apply_grant_expiry_v18(conn)?;
    apply_approval_similarity_v19(conn)?;
    apply_artifact_canonical_digest_v20(conn)?;
    apply_causal_event_enforced_rules_v21(conn)?;

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

fn apply_user_profiles_v5(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 5 {
        return Ok(());
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS user_profiles (
            user_id         TEXT PRIMARY KEY,
            display_name    TEXT,
            trust_domain    TEXT NOT NULL DEFAULT 'local',
            origin_node_id  TEXT,
            profile_json    TEXT,
            profile_version INTEGER NOT NULL DEFAULT 1,
            created_at      TEXT NOT NULL,
            updated_at      TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS user_agent_bindings (
            user_id    TEXT NOT NULL,
            agent_id   TEXT NOT NULL,
            scope      TEXT NOT NULL DEFAULT 'restricted',
            granted_at TEXT NOT NULL,
            granted_by TEXT,
            PRIMARY KEY (user_id, agent_id)
        );
        CREATE INDEX IF NOT EXISTS idx_user_agent_bindings_agent
          ON user_agent_bindings(agent_id);
        CREATE INDEX IF NOT EXISTS idx_user_profiles_trust
          ON user_profiles(trust_domain);",
    )?;
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![5_i64, "user_profiles", chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn apply_approvals_decision_reason_v6(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 6 {
        return Ok(());
    }
    let col_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('approvals') WHERE name = 'decision_reason'",
        [],
        |row| row.get(0),
    )?;
    if col_count == 0 {
        conn.execute("ALTER TABLE approvals ADD COLUMN decision_reason TEXT", [])?;
    }
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            6_i64,
            "approvals_decision_reason",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_published_reports_and_hooks_v7(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 7 {
        return Ok(());
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS published_session_reports (
            root_session_id TEXT PRIMARY KEY,
            report_handle TEXT NOT NULL,
            overview_handle TEXT,
            html_handle TEXT,
            narrative_handle TEXT,
            title TEXT NOT NULL,
            status TEXT NOT NULL,
            started_at TEXT,
            ended_at TEXT,
            agent_count INTEGER NOT NULL DEFAULT 0,
            error_count INTEGER NOT NULL DEFAULT 0,
            approval_count INTEGER NOT NULL DEFAULT 0,
            search_text TEXT NOT NULL,
            generated_at TEXT NOT NULL,
            report_version INTEGER NOT NULL DEFAULT 1
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS published_session_reports_fts
            USING fts5(root_session_id, title, search_text, status);

        CREATE TABLE IF NOT EXISTS hook_deliveries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_id TEXT NOT NULL,
            hook_event TEXT NOT NULL,
            hook_action TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            attempt_count INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_hook_deliveries_event
            ON hook_deliveries(event_id, hook_event, hook_action);",
    )?;

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            7_i64,
            "published_reports_and_hooks",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_scheduled_jobs_v8(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 8 {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS scheduled_jobs (
            job_id TEXT PRIMARY KEY,
            owner_agent_id TEXT NOT NULL,
            root_session_id TEXT NOT NULL,
            target_agent_id TEXT NOT NULL,
            target_revision_id TEXT NOT NULL DEFAULT '',
            message TEXT NOT NULL,
            metadata_json TEXT,
            cron_expr TEXT NOT NULL,
            timezone TEXT NOT NULL DEFAULT 'UTC',
            next_run_at TEXT NOT NULL,
            last_run_at TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_error TEXT,
            generation INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_status_next_run
          ON scheduled_jobs(status, next_run_at);
        CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_root_session
          ON scheduled_jobs(root_session_id);
        CREATE INDEX IF NOT EXISTS idx_scheduled_jobs_owner
          ON scheduled_jobs(owner_agent_id);",
    )?;
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![8_i64, "scheduled_jobs", chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn apply_credential_setup_state_v10(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 10 {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS credential_setup_state (
            credential_id TEXT PRIMARY KEY,
            state_json    TEXT NOT NULL,
            created_at    TEXT NOT NULL,
            updated_at    TEXT NOT NULL
        );",
    )?;
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            10_i64,
            "credential_setup_state",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_scheduled_jobs_v9(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 9 {
        return Ok(());
    }
    // Column may already exist if the table was created with v8 schema (fresh DB)
    let has_col: bool = conn
        .prepare("SELECT target_revision_id FROM scheduled_jobs LIMIT 0")
        .is_ok();
    if !has_col {
        conn.execute(
            "ALTER TABLE scheduled_jobs ADD COLUMN target_revision_id TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            9_i64,
            "scheduled_jobs_target_revision",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_agent_messages_v11(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 11 {
        return Ok(());
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_messages (
            message_id TEXT PRIMARY KEY,
            sender_session_id TEXT NOT NULL,
            sender_agent_id TEXT NOT NULL,
            target_pattern TEXT NOT NULL,
            message TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS agent_message_deliveries (
            message_id TEXT NOT NULL,
            target_session_id TEXT NOT NULL,
            delivered_at TEXT,
            PRIMARY KEY (message_id, target_session_id)
        );
        CREATE INDEX IF NOT EXISTS idx_agent_msg_deliveries_target ON agent_message_deliveries(target_session_id, delivered_at);"
    )?;

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![11_i64, "agent_messages", chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn apply_credential_refresh_fields_v12(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 12 {
        return Ok(());
    }

    let cols = [
        ("refresh_token_secret_name", "TEXT"),
        ("refresh_url", "TEXT"),
        ("refresh_method", "TEXT"),
        ("refresh_headers", "TEXT"),
        ("refresh_extract_access_token", "TEXT"),
        ("refresh_extract_refresh_token", "TEXT"),
        ("refresh_extract_expires_in", "TEXT"),
    ];
    for (col, ty) in &cols {
        let has_col: bool = conn
            .prepare(&format!("SELECT {col} FROM credentials LIMIT 0"))
            .is_ok();
        if !has_col {
            conn.execute(
                &format!("ALTER TABLE credentials ADD COLUMN {col} {ty}"),
                [],
            )?;
        }
    }

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![12_i64, "credential_refresh_fields", chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn apply_admin_proposals_v13(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 13 {
        return Ok(());
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS admin_proposals (
            proposal_id     TEXT PRIMARY KEY,
            title           TEXT NOT NULL,
            category        TEXT NOT NULL,
            evidence_json   TEXT NOT NULL,
            remediation     TEXT NOT NULL,
            blast_radius    TEXT NOT NULL,
            priority        TEXT NOT NULL DEFAULT 'medium',
            created_by      TEXT NOT NULL,
            created_at      TEXT NOT NULL,
            status          TEXT NOT NULL DEFAULT 'open',
            triaged_by      TEXT,
            triaged_at      TEXT,
            decision_reason TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_admin_proposals_status ON admin_proposals(status);
        CREATE INDEX IF NOT EXISTS idx_admin_proposals_category ON admin_proposals(category);
        CREATE INDEX IF NOT EXISTS idx_admin_proposals_created_at ON admin_proposals(created_at);",
    )?;

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![13_i64, "admin_proposals", chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn apply_memories_revision_provenance_v14(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 14 {
        return Ok(());
    }

    let cols: Vec<&str> = ["revision_id", "binding_session_id", "alias_ref", "quarantine_reason"]
        .to_vec();
    for col in &cols {
        let col_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = ?1",
            [col],
            |row| row.get(0),
        )?;
        if col_count == 0 {
            conn.execute(&format!("ALTER TABLE memories ADD COLUMN {col} TEXT"), [])?;
        }
    }

    let idx_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_index_list('memories') WHERE name = 'idx_memories_revision_id'",
        [],
        |row| row.get(0),
    )?;
    if idx_count == 0 {
        conn.execute(
            "CREATE INDEX idx_memories_revision_id ON memories(revision_id)",
            [],
        )?;
    }

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            14_i64,
            "memories_revision_provenance",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_session_grant_revocation_v15(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 15 {
        return Ok(());
    }

    for col in &["revoked_at", "revoked_reason"] {
        let col_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('session_approval_grants') WHERE name = ?1",
            [col],
            |row| row.get(0),
        )?;
        if col_count == 0 {
            conn.execute(
                &format!("ALTER TABLE session_approval_grants ADD COLUMN {col} TEXT"),
                [],
            )?;
        }
    }

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            15_i64,
            "session_grant_revocation",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_grant_scope_and_session_v16(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 16 {
        return Ok(());
    }

    conn.execute_batch(
        "CREATE TABLE session_approval_grants_new (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            root_session_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            host TEXT NOT NULL,
            granted_by TEXT NOT NULL,
            granted_at TEXT NOT NULL,
            source_approval_id TEXT,
            revoked_at TEXT,
            revoked_reason TEXT,
            session_id TEXT NOT NULL DEFAULT '',
            scope TEXT NOT NULL DEFAULT 'root_session',
            UNIQUE(root_session_id, session_id, agent_id, scope, host)
        );
        INSERT INTO session_approval_grants_new
            (id, root_session_id, agent_id, host, granted_by, granted_at, source_approval_id, revoked_at, revoked_reason)
            SELECT id, root_session_id, agent_id, host, granted_by, granted_at, source_approval_id, revoked_at, revoked_reason
            FROM session_approval_grants;
        DROP TABLE session_approval_grants;
        ALTER TABLE session_approval_grants_new RENAME TO session_approval_grants;",
    )?;

    conn.execute(
        "UPDATE session_approval_grants SET session_id = root_session_id WHERE session_id = ''",
        [],
    )?;

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_session_grants_root_agent
          ON session_approval_grants(root_session_id, agent_id);
         CREATE INDEX IF NOT EXISTS idx_session_grants_root
          ON session_approval_grants(root_session_id);
         CREATE INDEX IF NOT EXISTS idx_session_grants_session_agent
          ON session_approval_grants(session_id, agent_id);",
    )?;

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            16_i64,
            "grant_scope_and_session",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_grant_targets_table_v17(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 17 {
        return Ok(());
    }

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_approval_grant_targets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            grant_id INTEGER NOT NULL REFERENCES session_approval_grants(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            value TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_grant_targets_grant_id
          ON session_approval_grant_targets(grant_id);",
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, host FROM session_approval_grants",
    )?;
    let rows: Vec<(i64, String)> = stmt.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?.filter_map(|r| r.ok()).collect();
    drop(stmt);

    for (grant_id, host) in &rows {
        let value = serde_json::json!({"kind": "exact_host", "value": host}).to_string();
        conn.execute(
            "INSERT OR IGNORE INTO session_approval_grant_targets (grant_id, kind, value) VALUES (?1, 'exact_host', ?2)",
            params![grant_id, value],
        )?;
    }

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            17_i64,
            "grant_targets_table",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_grant_expiry_v18(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 18 {
        return Ok(());
    }

    let col_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('session_approval_grants') WHERE name = 'expires_at'",
        [],
        |row| row.get(0),
    )?;
    if col_count == 0 {
        conn.execute(
            "ALTER TABLE session_approval_grants ADD COLUMN expires_at TEXT",
            [],
        )?;
    }

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            18_i64,
            "grant_expiry",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_approval_similarity_v19(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 19 {
        return Ok(());
    }

    for (col, ty) in &[
        ("similar_to_request_id", "TEXT"),
        ("similarity_score", "REAL"),
    ] {
        let col_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('approvals') WHERE name = ?1",
            [col],
            |row| row.get(0),
        )?;
        if col_count == 0 {
            conn.execute(
                &format!("ALTER TABLE approvals ADD COLUMN {col} {ty}"),
                [],
            )?;
        }
    }

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            19_i64,
            "approval_similarity",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_artifact_canonical_digest_v20(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 20 {
        return Ok(());
    }

    let col_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('artifact_refs') WHERE name = 'artifact_canonical_digest'",
        [],
        |row| row.get(0),
    )?;
    if col_count == 0 {
        conn.execute(
            "ALTER TABLE artifact_refs ADD COLUMN artifact_canonical_digest TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![
            20_i64,
            "artifact_canonical_digest",
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(())
}

fn apply_causal_event_enforced_rules_v21(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current >= 21 {
        return Ok(());
    }

    let col_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('causal_events') WHERE name = 'enforced_rules'",
        [],
        |row| row.get(0),
    )?;
    if col_count == 0 {
        conn.execute(
            "ALTER TABLE causal_events ADD COLUMN enforced_rules TEXT NOT NULL DEFAULT '[\"R+++3\"]'",
            [],
        )?;
    }

    conn.execute(
        "UPDATE causal_events SET enforced_rules = '[\"R+++3\"]' WHERE enforced_rules IS NULL OR trim(enforced_rules) = ''",
        [],
    )?;

    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        params![21_i64, "causal_event_enforced_rules", chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}
