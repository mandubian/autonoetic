mod agent_registry;
mod approvals;
mod artifacts;
mod credentials;
mod evaluations;
mod memory;
mod migrate;
mod notifications;
mod observability;
mod row_decode;
mod runtime_control;
mod user_interactions;
mod util;
mod workflow;

use anyhow::Result;
use autonoetic_types::notification::NotificationStatus;
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::path::Path;

pub(crate) use row_decode::memory_object_from_row;
pub(crate) use util::escape_sqlite_like_fragment;

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowIndexFile {
    pub workflow_id: String,
    pub root_session_id: String,
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

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;

        let store = Self {
            conn: std::sync::Mutex::new(conn),
        };
        {
            let mut conn = store.conn.lock().unwrap();
            migrate::migrate(&mut conn)?;
        }
        {
            let conn = store.conn.lock().unwrap();
            if let Err(e) = migrate::reconcile_stale_active_executions(&conn) {
                tracing::warn!(
                    target: "gateway_store",
                    error = %e,
                    "Failed to reconcile stale active_executions"
                );
            }
        }
        {
            let conn = store.conn.lock().unwrap();
            migrate::backfill_workflow_index(&conn, gateway_dir)?;
        }
        Ok(store)
    }

    pub fn migrate(&self) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        migrate::migrate(&mut conn)
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
    use autonoetic_types::causal_chain::SessionTranscriptRecord;

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

        let large_stdout = "A".repeat(10000);
        let large_stderr = "B".repeat(10000);

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
    fn search_session_transcripts_fts_query() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let now = chrono::Utc::now().to_rfc3339();
        store.upsert_session_transcript(&SessionTranscriptRecord {
            transcript_id: "stx-sess1".to_string(),
            session_id: "sess1".to_string(),
            root_session_id: "root1".to_string(),
            agent_id: "agent1".to_string(),
            revision_id: None,
            user_id: None,
            started_at: now.clone(),
            ended_at: None,
            status: "active".to_string(),
            turn_count: 2,
            transcript_handle: Some("h1".to_string()),
            excerpt: Some("search for api docs api docs api".to_string()),
            origin_node_id: None,
        })?;

        let results = store.search_session_transcripts(Some("api"), None, None, None, None, 10)?;
        assert!(
            results.len() >= 1,
            "expected >= 1 match for 'api', got {}",
            results.len()
        );
        assert_eq!(
            results[0].session_id, "sess1",
            "sess1 has more 'api' occurrences so should rank first via bm25"
        );

        let all = store.search_session_transcripts(None, None, None, None, None, 10)?;
        assert_eq!(all.len(), 1);

        Ok(())
    }

    #[test]
    fn finalize_session_transcript_updates_status() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let now = chrono::Utc::now().to_rfc3339();
        store.upsert_session_transcript(&SessionTranscriptRecord {
            transcript_id: "stx-sess1".to_string(),
            session_id: "sess1".to_string(),
            root_session_id: "root1".to_string(),
            agent_id: "agent1".to_string(),
            revision_id: None,
            user_id: None,
            started_at: now.clone(),
            ended_at: None,
            status: "active".to_string(),
            turn_count: 1,
            transcript_handle: Some("h1".to_string()),
            excerpt: Some("test".to_string()),
            origin_node_id: None,
        })?;

        store.finalize_session_transcript("sess1", &now, "completed")?;

        let results =
            store.search_session_transcripts(None, None, None, Some("completed"), None, 10)?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "completed");
        assert!(results[0].ended_at.is_some());

        Ok(())
    }
}
