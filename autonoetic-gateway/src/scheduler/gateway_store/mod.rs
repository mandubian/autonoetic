pub mod admin_proposals;
mod agent_registry;
mod approvals;
mod artifacts;
pub mod attack_patterns;
pub mod constitutional_proposals;
mod credentials;
mod escalations;
mod evaluations;
mod gate_messages;
mod hook_deliveries;
mod improvement_cycles;
mod memory;
mod messages;
mod migrate;
mod notifications;
mod operator_activity;
pub use operator_activity::OperatorActivityInsert;
mod observability;
mod session_timeline;
pub mod plan_frames;
pub mod post_promotion_reviews;
mod reclamation;
mod recordings;
mod row_decode;
mod runtime_control;
mod scheduled_jobs;
pub mod security_findings;
pub mod sentinel_disagreements;
pub mod session_outcomes;
mod user_interactions;
mod user_profiles;
mod util;
mod workflow;
mod workbenches;
mod validation_waivers;

use anyhow::Result;
use autonoetic_types::notification::NotificationStatus;
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

pub use messages::AgentMessageRecord;
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
    // Session Room canonical-timeline attribution (#363 P1). Optional so older
    // call sites compile; the digest tracer populates them.
    pub principal_kind: Option<String>,
    pub principal_id: Option<String>,
    pub role: Option<String>,
    pub altitude: Option<String>,
    pub refs_json: Option<String>,
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
    approval_flood_cap: std::sync::atomic::AtomicUsize,
    escalation_flood_cap: std::sync::atomic::AtomicUsize,
    /// Weak ref avoids an `Arc` cycle with [`crate::scheduler::hooks::HookExecutor`], which may
    /// hold an `Arc<GatewayStore>` for other hooks.
    policy_hook_executor: Mutex<Option<Weak<crate::scheduler::hooks::HookExecutor>>>,
    pub task_notify: crate::scheduler::task_notify::TaskNotifyRegistry,
    /// Session-scoped result cache for pure read tools (issue #289).
    pub session_read_cache: crate::runtime::session_read_cache::SessionReadCacheRegistry,
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

        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA synchronous=FULL;",
        )?;

        let store = Self {
            conn: std::sync::Mutex::new(conn),
            approval_flood_cap: std::sync::atomic::AtomicUsize::new(0),
            escalation_flood_cap: std::sync::atomic::AtomicUsize::new(0),
            policy_hook_executor: Mutex::new(None),
            task_notify: crate::scheduler::task_notify::TaskNotifyRegistry::new(),
            session_read_cache:
                crate::runtime::session_read_cache::SessionReadCacheRegistry::default(),
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

    /// Wire the gateway hook executor for [`autonoetic_types::hooks::HookEvent::PolicyDecision`].
    /// Safe to call once at daemon startup; tests that open the store directly leave this unset.
    pub fn set_policy_hook_executor(&self, exec: &Arc<crate::scheduler::hooks::HookExecutor>) {
        let mut g = self
            .policy_hook_executor
            .lock()
            .expect("policy_hook_executor mutex poisoned");
        *g = Some(Arc::downgrade(exec));
    }

    pub fn migrate(&self) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        migrate::migrate(&mut conn)
    }

    /// Execute a closure with a read-only borrow of the underlying connection.
    /// Used by the sentinel runner to pass the connection to deterministic
    /// check functions without exposing the field directly.
    pub(crate) fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }

    pub fn create_scheduled_job(
        &self,
        job: &autonoetic_types::scheduled_job::ScheduledJob,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::create_scheduled_job(&conn, job)
    }

    pub fn get_scheduled_job(
        &self,
        job_id: &str,
    ) -> Result<Option<autonoetic_types::scheduled_job::ScheduledJob>> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::get_scheduled_job(&conn, job_id)
    }

    pub fn list_scheduled_jobs_for_owner(
        &self,
        owner_agent_id: &str,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Vec<autonoetic_types::scheduled_job::ScheduledJob>> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::list_scheduled_jobs_for_owner(&conn, owner_agent_id, limit, offset)
    }

    pub fn list_scheduled_jobs_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<autonoetic_types::scheduled_job::ScheduledJob>> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::list_scheduled_jobs_for_root(&conn, root_session_id)
    }

    pub fn load_due_scheduled_jobs(
        &self,
        now_rfc3339: &str,
        limit: usize,
    ) -> Result<Vec<autonoetic_types::scheduled_job::ScheduledJob>> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::load_due_scheduled_jobs(&conn, now_rfc3339, limit)
    }

    pub fn load_due_scheduled_jobs_for_owner(
        &self,
        owner_agent_id: &str,
        now_rfc3339: &str,
        limit: usize,
    ) -> Result<Vec<autonoetic_types::scheduled_job::ScheduledJob>> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::load_due_scheduled_jobs_for_owner(&conn, owner_agent_id, now_rfc3339, limit)
    }

    pub fn claim_due_scheduled_job(
        &self,
        job_id: &str,
        now_rfc3339: &str,
    ) -> Result<Option<autonoetic_types::scheduled_job::ScheduledJob>> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::claim_due_scheduled_job(&conn, job_id, now_rfc3339)
    }

    pub fn claim_and_advance_due_job(
        &self,
        job_id: &str,
        now_rfc3339: &str,
        new_next_run_at: &str,
    ) -> Result<Option<autonoetic_types::scheduled_job::ScheduledJob>> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::claim_and_advance_due_job(&conn, job_id, now_rfc3339, new_next_run_at)
    }

    pub fn advance_next_run(
        &self,
        job_id: &str,
        next_run_at: &str,
        last_run_at: Option<&str>,
        last_error: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::advance_next_run(&conn, job_id, next_run_at, last_run_at, last_error)
    }

    pub fn pause_scheduled_job(&self, job_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::pause_scheduled_job(&conn, job_id)
    }

    pub fn resume_scheduled_job(&self, job_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::resume_scheduled_job(&conn, job_id)
    }

    pub fn cancel_scheduled_job(&self, job_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::cancel_scheduled_job(&conn, job_id)
    }

    pub fn cancel_scheduled_jobs_for_root(&self, root_session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::cancel_scheduled_jobs_for_root(&conn, root_session_id)
    }

    pub fn delete_scheduled_job(&self, job_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::delete_scheduled_job(&conn, job_id)
    }

    // -------------------------------------------------------------------------
    // Agent Messages
    // -------------------------------------------------------------------------

    pub fn save_agent_message(&self, record: &AgentMessageRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        messages::save_agent_message(&conn, record)
    }

    pub fn insert_message_delivery(&self, message_id: &str, target_session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        messages::insert_message_delivery(&conn, message_id, target_session_id)
    }

    pub fn fetch_undelivered_messages(&self, session_id: &str) -> Result<Vec<AgentMessageRecord>> {
        let conn = self.conn.lock().unwrap();
        messages::fetch_undelivered_messages(&conn, session_id)
    }

    pub fn mark_message_delivered(&self, message_id: &str, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        messages::mark_message_delivered(&conn, message_id, session_id)
    }
    /// Record a stage transition for idempotent builder/install/promotion flow.
    /// Returns `true` if the transition was newly inserted, `false` if it already existed.
    pub fn record_stage_transition(
        &self,
        agent_id: &str,
        revision_id: &str,
        stage: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let result = conn.execute(
            "INSERT OR IGNORE INTO stage_transitions (agent_id, revision_id, stage, created_at)
             VALUES (?1, ?2, ?3, datetime('now'))",
            params![agent_id, revision_id, stage],
        )?;
        Ok(result > 0)
    }

    /// Check whether a given stage transition has already been recorded.
    pub fn has_stage_transition(
        &self,
        agent_id: &str,
        revision_id: &str,
        stage: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM stage_transitions WHERE agent_id = ?1 AND revision_id = ?2 AND stage = ?3",
            params![agent_id, revision_id, stage],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn save_plan_frame(&self, plan: &autonoetic_types::plan_frame::PlanFrame) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        plan_frames::save_plan_frame(&conn, plan)
    }

    pub fn load_plan_frame(
        &self,
        plan_id: &str,
    ) -> Result<Option<autonoetic_types::plan_frame::PlanFrame>> {
        let conn = self.conn.lock().unwrap();
        plan_frames::load_plan_frame(&conn, plan_id)
    }

    pub fn load_plan_frame_revision(
        &self,
        plan_id: &str,
        version: u32,
    ) -> Result<Option<autonoetic_types::plan_frame::PlanFrame>> {
        let conn = self.conn.lock().unwrap();
        plan_frames::load_plan_frame_revision(&conn, plan_id, version)
    }

    pub fn update_plan_frame_status(
        &self,
        plan_id: &str,
        version: u32,
        status: autonoetic_types::plan_frame::PlanStatus,
        approved_by: Option<&str>,
        approved_at: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        plan_frames::update_plan_frame_status(&conn, plan_id, version, status, approved_by, approved_at)
    }

    pub fn load_active_plan_for_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Option<autonoetic_types::plan_frame::PlanFrame>> {
        let conn = self.conn.lock().unwrap();
        plan_frames::load_active_plan_for_workflow(&conn, workflow_id)
    }

    pub fn list_plan_frames_for_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<autonoetic_types::plan_frame::PlanFrame>> {
        let conn = self.conn.lock().unwrap();
        plan_frames::list_plan_frames_for_workflow(&conn, workflow_id)
    }

    pub fn list_plan_revisions(
        &self,
        plan_id: &str,
    ) -> Result<Vec<autonoetic_types::plan_frame::PlanFrame>> {
        let conn = self.conn.lock().unwrap();
        plan_frames::list_plan_revisions(&conn, plan_id)
    }

    pub fn list_pending_plan_frames_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<autonoetic_types::plan_frame::PlanFrame>> {
        let conn = self.conn.lock().unwrap();
        plan_frames::list_pending_plan_frames_for_root(&conn, root_session_id)
    }

    // -------------------------------------------------------------------------
    // Workbenches
    // -------------------------------------------------------------------------

    pub fn save_workbench(&self, wb: &autonoetic_types::workbench::WorkbenchProjection) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        workbenches::save_workbench(&conn, wb)
    }

    pub fn load_workbench(
        &self,
        workbench_id: &str,
    ) -> Result<Option<autonoetic_types::workbench::WorkbenchProjection>> {
        let conn = self.conn.lock().unwrap();
        workbenches::load_workbench(&conn, workbench_id)
    }

    pub fn load_active_workbench_for_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Option<autonoetic_types::workbench::WorkbenchProjection>> {
        let conn = self.conn.lock().unwrap();
        workbenches::load_active_workbench_for_workflow(&conn, workflow_id)
    }

    pub fn list_workbenches_for_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<autonoetic_types::workbench::WorkbenchProjection>> {
        let conn = self.conn.lock().unwrap();
        workbenches::list_workbenches_for_workflow(&conn, workflow_id)
    }

    pub fn update_workbench_status(
        &self,
        workbench_id: &str,
        status: autonoetic_types::workbench::WorkbenchStatus,
        timestamp: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        workbenches::update_workbench_status(&conn, workbench_id, status, timestamp)
    }

    pub fn update_workbench_last_checkpoint(
        &self,
        workbench_id: &str,
        timestamp: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        workbenches::update_workbench_last_checkpoint(&conn, workbench_id, timestamp)
    }

    pub fn save_checkpoint(&self, cp: &autonoetic_types::workbench::WorkbenchCheckpoint) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        workbenches::save_checkpoint(&conn, cp)
    }

    pub fn load_checkpoint(
        &self,
        checkpoint_id: &str,
    ) -> Result<Option<autonoetic_types::workbench::WorkbenchCheckpoint>> {
        let conn = self.conn.lock().unwrap();
        workbenches::load_checkpoint(&conn, checkpoint_id)
    }

    pub fn list_checkpoints_for_workbench(
        &self,
        workbench_id: &str,
    ) -> Result<Vec<autonoetic_types::workbench::WorkbenchCheckpoint>> {
        let conn = self.conn.lock().unwrap();
        workbenches::list_checkpoints_for_workbench(&conn, workbench_id)
    }

    pub fn delete_workbench(&self, workbench_id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        workbenches::delete_workbench(&mut conn, workbench_id)
    }

    pub fn save_validation_waiver(
        &self,
        waiver: &autonoetic_types::plan_frame::ValidationWaiver,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        validation_waivers::save_waiver(&conn, waiver)
    }

    pub fn list_waivers_for_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<Vec<autonoetic_types::plan_frame::ValidationWaiver>> {
        let conn = self.conn.lock().unwrap();
        validation_waivers::list_waivers_for_artifact(&conn, artifact_id)
    }

    pub fn list_waivers_for_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<autonoetic_types::plan_frame::ValidationWaiver>> {
        let conn = self.conn.lock().unwrap();
        validation_waivers::list_waivers_for_workflow(&conn, workflow_id)
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
    use autonoetic_types::escalation::{EscalationMessage, RoleVerdictSummary};
    use autonoetic_types::promotion::PromotionRole;

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
            artifact_manifest_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_string(),
            artifact_canonical_digest:
                "sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
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
            tool_name: "sandbox_exec".to_string(),
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
            tool_name: "sandbox_exec".to_string(),
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
            Some("sandbox_exec"),
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
            Some("sandbox_exec"),
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
    fn answered_standalone_interaction_resume_claim_is_single_use() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let interaction = pending_interaction("ui-claim-1", true, vec![]);
        store.create_user_interaction(&interaction)?;
        store.answer_user_interaction(&UserInteractionAnswer {
            interaction_id: "ui-claim-1".to_string(),
            answer_option_id: None,
            answer_text: Some("yes".to_string()),
            answered_by: "test".to_string(),
        })?;

        let answered = store.get_answered_standalone_interactions()?;
        assert_eq!(answered.len(), 1);
        assert_eq!(answered[0].interaction_id, "ui-claim-1");

        assert!(store.try_claim_answered_standalone_interaction_resume("ui-claim-1")?);
        assert!(!store.try_claim_answered_standalone_interaction_resume("ui-claim-1")?);
        assert!(store.get_answered_standalone_interactions()?.is_empty());

        store.release_answered_standalone_interaction_resume_claim("ui-claim-1")?;
        let after_release = store.get_answered_standalone_interactions()?;
        assert_eq!(after_release.len(), 1);
        assert_eq!(after_release[0].interaction_id, "ui-claim-1");

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

    #[test]
    fn active_upsert_does_not_reopen_terminal_transcript() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let started = chrono::Utc::now().to_rfc3339();
        let ended = chrono::Utc::now().to_rfc3339();
        store.upsert_session_transcript(&SessionTranscriptRecord {
            transcript_id: "stx-sess1".to_string(),
            session_id: "sess1".to_string(),
            root_session_id: "root1".to_string(),
            agent_id: "agent1".to_string(),
            revision_id: None,
            user_id: None,
            started_at: started.clone(),
            ended_at: None,
            status: "active".to_string(),
            turn_count: 1,
            transcript_handle: Some("h1".to_string()),
            excerpt: Some("initial".to_string()),
            origin_node_id: None,
        })?;

        store.finalize_session_transcript("sess1", &ended, "completed")?;

        store.upsert_session_transcript(&SessionTranscriptRecord {
            transcript_id: "stx-sess1".to_string(),
            session_id: "sess1".to_string(),
            root_session_id: "root1".to_string(),
            agent_id: "agent1".to_string(),
            revision_id: None,
            user_id: None,
            started_at: started,
            ended_at: None,
            status: "active".to_string(),
            turn_count: 2,
            transcript_handle: Some("h2".to_string()),
            excerpt: Some("updated".to_string()),
            origin_node_id: None,
        })?;

        let results = store.search_session_transcripts(None, None, None, Some("completed"), None, 10)?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "completed");
        assert_eq!(results[0].ended_at.as_deref(), Some(ended.as_str()));
        assert_eq!(results[0].turn_count, 2);
        assert_eq!(results[0].transcript_handle.as_deref(), Some("h2"));

        Ok(())
    }

    #[test]
    fn escalation_create_get_list_resolve() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let verdicts = vec![
            RoleVerdictSummary {
                role: PromotionRole::StaticEvaluator,
                agent_id: "static_evaluator.default".to_string(),
                passed: true,
                findings_summary: "Code looks clean, no vulnerabilities".to_string(),
                evidence_ref: None,
                recorded_at: chrono::Utc::now().to_rfc3339(),
            },
            RoleVerdictSummary {
                role: PromotionRole::UnitTestRunner,
                agent_id: "unit_test_runner.default".to_string(),
                passed: true,
                findings_summary: "All 12 tests pass".to_string(),
                evidence_ref: Some("art_artifact123".to_string()),
                recorded_at: chrono::Utc::now().to_rfc3339(),
            },
        ];

        let mut escalation = EscalationMessage::new(
            "esc_test_001".to_string(),
            "art_artifact123".to_string(),
            "my.agent".to_string(),
            "rev_sha256:abc123".to_string(),
            verdicts,
            "All roles passed. Recommend promotion.".to_string(),
            "root_session_xyz".to_string(),
        );

        store.create_escalation(&escalation)?;

        let fetched = store
            .get_escalation("esc_test_001")?
            .expect("escalation should exist");
        assert_eq!(fetched.artifact_id, "art_artifact123");
        assert_eq!(fetched.agent_id, "my.agent");
        assert_eq!(fetched.role_verdicts.len(), 2);
        assert_eq!(
            fetched.role_verdicts[0].role.as_str(),
            "static_evaluator"
        );
        assert!(fetched.role_verdicts[0].passed);
        assert_eq!(
            fetched.role_verdicts[1].findings_summary,
            "All 12 tests pass"
        );

        let pending = store.list_pending_escalations()?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].escalation_id, "esc_test_001");

        store.resolve_escalation(
            "esc_test_001",
            autonoetic_types::escalation::EscalationStatus::Approved,
            "cli-operator",
            Some("Looks good, promote"),
        )?;

        let pending_after = store.list_pending_escalations()?;
        assert_eq!(pending_after.len(), 0);

        let resolved = store
            .get_escalation("esc_test_001")?
            .expect("escalation should still exist after resolution");
        assert_eq!(
            resolved.status,
            autonoetic_types::escalation::EscalationStatus::Approved
        );
        assert!(resolved.resolved_at.is_some());
        assert_eq!(resolved.decided_by.as_deref(), Some("cli-operator"));
        assert_eq!(resolved.decision_reason.as_deref(), Some("Looks good, promote"));

        Ok(())
    }
}
