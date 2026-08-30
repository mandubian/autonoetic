pub mod admin_proposals;
mod agent_registry;
pub mod amendment_invitations;
pub mod anomaly_flags;
mod approvals;
mod artifact_taint;
mod artifacts;
pub mod attack_patterns;
mod channel_bindings;
pub mod constitutional_proposals;
mod decider_appointments;
mod carry_lineage;
pub use carry_lineage::CarryLineageRecord;
mod credentials;
pub mod egress_policy;
mod workspace_taint;
pub use egress_policy::StoredEgressSessionPolicy;
mod egress_declassification;
mod escalations;
mod evaluations;
mod fork_lineage;
pub use fork_lineage::ForkLineageRecord;
mod gate_messages;
mod hook_deliveries;
mod improvement_cycles;
mod label_listing;
mod memory;
mod messages;
mod migrate;
mod notifications;
mod operator_activity;
pub use operator_activity::OperatorActivityInsert;
pub use agent_registry::PromotedAgent;
pub use workflow_tasks::TaskExecutionClaim;
mod observability;
pub use observability::{CausalSessionSummary, CivicHealth, CivicHealthEntry};
pub mod plan_frames;
pub mod post_promotion_reviews;
mod reclamation;
mod residency;
mod recordings;
mod row_decode;
mod runtime_control;
mod scheduled_jobs;
pub mod security_findings;
pub mod sentinel_disagreements;
pub mod session_envelopes;
pub mod session_inference;
mod session_outcomes;
mod session_spawn_lineage;
mod session_taint;
mod session_timeline;
pub mod singleton_index;
mod user_interactions;
mod user_profiles;
mod util;
mod validation_waivers;
mod workbenches;
pub mod workflow_tasks;
mod workflow;

use anyhow::Result;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::notification::NotificationStatus;
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

/// Maximum buffered timeline events before an automatic flush.
const LIVE_DIGEST_BUFFER_CAPACITY: usize = 32;

/// Ceiling on timeline events held in memory while flushes keep failing
/// (#1238).
///
/// Retaining a failed batch is what stops a transient `SQLITE_BUSY` from
/// erasing the timeline, but retention without a bound is an unbounded leak: a
/// database that stays unwritable would grow this buffer until the process
/// dies, taking every other subsystem with it. Past this many events the oldest
/// are dropped and the loss is logged with a count — bounded and visible,
/// rather than unbounded and silent. Sixteen full batches is far more headroom
/// than any transient fault needs.
const LIVE_DIGEST_RETRY_CAPACITY: usize = LIVE_DIGEST_BUFFER_CAPACITY * 16;

pub use messages::AgentMessageRecord;
pub use residency::SessionResidency;
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
    anomaly_flag_flood_cap: std::sync::atomic::AtomicUsize,
    /// Weak ref avoids an `Arc` cycle with [`crate::scheduler::hooks::HookExecutor`], which may
    /// hold an `Arc<GatewayStore>` for other hooks.
    policy_hook_executor: Mutex<Option<Weak<crate::scheduler::hooks::HookExecutor>>>,
    pub task_notify: crate::scheduler::task_notify::TaskNotifyRegistry,
    /// Session-scoped result cache for pure read tools (issue #289).
    pub session_read_cache: crate::runtime::session_read_cache::SessionReadCacheRegistry,
    /// Per-session, per-host `sandbox_exec` probe budget (issue #853).
    pub host_probe_budget: crate::runtime::host_probe_budget::HostProbeBudgetRegistry,
    /// Buffered live-digest timeline inserts. Flushed when the buffer is full,
    /// before reads of the timeline, or on drop. This batches high-frequency
    /// observability writes (turn/tool/agent events) into fewer transactions.
    live_digest_buffer: Mutex<Vec<LiveDigestEventRecord>>,
    /// Runtime config used to compute approval/interaction TTLs. Set once at
    /// daemon startup; tests that open the store directly use default values.
    config: Mutex<Option<Arc<GatewayConfig>>>,
    /// Root sessions currently at the approval-flood cap that have already had
    /// an `operator_alert` emitted (#723). Prevents re-emitting the alert on
    /// every rejected create while a root stays flooded; a root is removed when
    /// a create for it next succeeds (the window reset).
    flood_alerted_roots: Mutex<std::collections::HashSet<String>>,
    /// Reporters currently at the anomaly-flag flood cap that have already had
    /// an operator notification emitted (#770). Mirrors `flood_alerted_roots`:
    /// the alert fires once per flood window, and a reporter is removed when
    /// one of its filings next succeeds (capacity freed by adjudication).
    flood_alerted_flag_reporters: Mutex<std::collections::HashSet<String>>,
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
            anomaly_flag_flood_cap: std::sync::atomic::AtomicUsize::new(0),
            policy_hook_executor: Mutex::new(None),
            task_notify: crate::scheduler::task_notify::TaskNotifyRegistry::new(),
            session_read_cache:
                crate::runtime::session_read_cache::SessionReadCacheRegistry::default(),
            host_probe_budget:
                crate::runtime::host_probe_budget::HostProbeBudgetRegistry::default(),
            live_digest_buffer: Mutex::new(Vec::with_capacity(LIVE_DIGEST_BUFFER_CAPACITY)),
            config: Mutex::new(None),
            flood_alerted_roots: Mutex::new(std::collections::HashSet::new()),
            flood_alerted_flag_reporters: Mutex::new(std::collections::HashSet::new()),
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

    /// Runtime config used to compute approval/interaction TTLs. Set once at
    /// daemon startup; tests that open the store directly use default values.
    pub fn set_config(&self, config: Arc<GatewayConfig>) {
        let mut g = self.config.lock().expect("config mutex poisoned");
        *g = Some(config);
    }

    pub fn config(&self) -> Option<Arc<GatewayConfig>> {
        self.config.lock().expect("config mutex poisoned").clone()
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
        self.flush_live_digest_events()?;
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

    pub fn list_scheduled_jobs(
        &self,
        owner_agent_id: Option<&str>,
        root_session_id: Option<&str>,
        status: Option<autonoetic_types::scheduled_job::ScheduledJobStatus>,
        limit: usize,
    ) -> Result<Vec<autonoetic_types::scheduled_job::ScheduledJob>> {
        let conn = self.conn.lock().unwrap();
        scheduled_jobs::list_scheduled_jobs(&conn, owner_agent_id, root_session_id, status, limit)
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

    // ---- Session-scoped egress policy (RFC data-envelopes §5.4) ----

    /// Declare (or replace) the root session's egress policy. Session rules are
    /// added to the operator-global set and can only restrict.
    pub fn set_egress_session_policy(
        &self,
        root_session_id: &str,
        policy: &autonoetic_types::egress::EgressSessionPolicy,
        set_by: &str,
    ) -> Result<StoredEgressSessionPolicy> {
        let conn = self.conn.lock().unwrap();
        egress_policy::set_policy(&conn, root_session_id, policy, set_by)
    }

    pub fn get_egress_session_policy(
        &self,
        root_session_id: &str,
    ) -> Result<Option<StoredEgressSessionPolicy>> {
        let conn = self.conn.lock().unwrap();
        egress_policy::get_policy(&conn, root_session_id)
    }

    /// Drop the policy — the root session closed, was emergency-stopped, or the
    /// operator cleared it. Returns whether a row was removed.
    pub fn delete_egress_session_policy(&self, root_session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        egress_policy::delete_policy(&conn, root_session_id)
    }

    // ---- Cross-agent egress taint (RFC data-envelopes §5.5) ----

    /// Record a session's accumulated egress taint at finalize, so a sibling or
    /// parent that later surfaces its return value / message labels the
    /// transferred content (closes the `LocalAgent` hole).
    pub fn set_session_egress_taint(
        &self,
        session_id: &str,
        label: &autonoetic_types::egress::EgressLabel,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        session_taint::set_taint(&conn, session_id, label)
    }

    /// Intersect a label into a session's taint without ever widening it, and
    /// return the resulting taint. Use this for **incremental** ingest-side
    /// contributions (a peer's inbound label, an operator's marked message);
    /// [`Self::set_session_egress_taint`] replaces the row and is for the
    /// finalize path that recomputes from the whole label sidecar.
    pub fn restrict_session_egress_taint(
        &self,
        session_id: &str,
        label: &autonoetic_types::egress::EgressLabel,
    ) -> Result<autonoetic_types::egress::EgressLabel> {
        let conn = self.conn.lock().unwrap();
        session_taint::restrict_taint(&conn, session_id, label)
    }

    /// Read a session's accumulated egress taint (`None` ⇒ unrestricted).
    pub fn get_session_egress_taint(
        &self,
        session_id: &str,
    ) -> Result<Option<autonoetic_types::egress::EgressLabel>> {
        let conn = self.conn.lock().unwrap();
        session_taint::get_taint(&conn, session_id)
    }

    // ---- Artifact egress labels (RFC data-envelopes §4.5, #980) ----

    /// Intersect a label into an artifact's stored label, never widening it, and
    /// return the result. Called at build time with the builder session's taint;
    /// on content-addressed reuse this tightens the existing label rather than
    /// replacing it.
    pub fn restrict_artifact_egress_label(
        &self,
        artifact_id: &str,
        label: &autonoetic_types::egress::EgressLabel,
    ) -> Result<autonoetic_types::egress::EgressLabel> {
        let conn = self.conn.lock().unwrap();
        artifact_taint::restrict_label(&conn, artifact_id, label)
    }

    /// Read an artifact's egress label (`None` ⇒ unrestricted).
    pub fn get_artifact_egress_label(
        &self,
        artifact_id: &str,
    ) -> Result<Option<autonoetic_types::egress::EgressLabel>> {
        let conn = self.conn.lock().unwrap();
        artifact_taint::get_label(&conn, artifact_id)
    }

    // ---- Agent workspace egress labels (RFC data-envelopes §11, #1001) ----

    /// Read an agent workspace's egress label (`None` ⇒ unrestricted).
    pub fn get_workspace_egress_label(
        &self,
        agent_id: &str,
    ) -> Result<Option<autonoetic_types::egress::EgressLabel>> {
        let conn = self.conn.lock().unwrap();
        workspace_taint::get_label(&conn, agent_id)
    }

    /// Intersect a label into an agent workspace's stored label, never widening
    /// it, and return the result. Called when an exec in the workspace resolves
    /// to a restricted label — content movement is not path-followable, so the
    /// workspace as a whole tightens.
    pub fn restrict_workspace_egress_label(
        &self,
        agent_id: &str,
        label: &autonoetic_types::egress::EgressLabel,
    ) -> Result<autonoetic_types::egress::EgressLabel> {
        let conn = self.conn.lock().unwrap();
        workspace_taint::restrict_label(&conn, agent_id, label)
    }

    /// Delete an agent workspace's label — the operator-approved clearing path
    /// (`EgressDeclassificationTarget::Workspace` grant materialization).
    /// Returns whether a row was removed.
    pub fn delete_workspace_egress_label(&self, agent_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        workspace_taint::delete_label(&conn, agent_id)
    }

    // ---- `labels.list` read queries (#974, RFC §9.3) ----
    //
    // Operator-facing, read-only, metadata-only views over the label plane.
    // Each is root-tree scoped. The router surfaces store errors rather than
    // masquerading them as `unrestricted` (same fail-visible contract as
    // `grants.list`).

    pub fn list_envelope_events_for_root(
        &self,
        root_session_id: &str,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::egress::LabeledEnvelopeRow>> {
        let conn = self.conn.lock().unwrap();
        label_listing::list_envelope_events_for_root(&conn, root_session_id, limit)
    }

    pub fn list_session_taints_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<autonoetic_types::egress::SessionTaintRow>> {
        let conn = self.conn.lock().unwrap();
        label_listing::list_session_taints_for_root(&conn, root_session_id)
    }

    pub fn list_labeled_memories_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<autonoetic_types::egress::LabeledMemoryRow>> {
        let conn = self.conn.lock().unwrap();
        label_listing::list_labeled_memories_for_root(&conn, root_session_id)
    }

    pub fn list_labeled_artifacts_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<autonoetic_types::egress::LabeledArtifactRow>> {
        let conn = self.conn.lock().unwrap();
        label_listing::list_labeled_artifacts_for_root(&conn, root_session_id)
    }

    pub fn list_labeled_traces_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<autonoetic_types::egress::LabeledTraceRow>> {
        let conn = self.conn.lock().unwrap();
        label_listing::list_labeled_traces_for_root(&conn, root_session_id)
    }

    pub fn list_labeled_messages_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<autonoetic_types::egress::LabeledMessageRow>> {
        let conn = self.conn.lock().unwrap();
        label_listing::list_labeled_messages_for_root(&conn, root_session_id)
    }

    // ---- Session residency (parked, addressable sessions) ----

    /// Park a resident session, or refresh its TTL after it handles a message.
    pub fn upsert_session_residency(&self, r: &SessionResidency) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        residency::upsert_residency(&conn, r)
    }

    /// Drop the park — the session is resuming, closing, or being reaped.
    pub fn clear_session_residency(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        residency::clear_residency(&conn, session_id)
    }

    pub fn get_session_residency(&self, session_id: &str) -> Result<Option<SessionResidency>> {
        let conn = self.conn.lock().unwrap();
        residency::get_residency(&conn, session_id)
    }

    /// Re-point a parked session's agent after a session handoff (#1088).
    /// No-op when nothing is parked.
    pub fn update_residency_agent(&self, session_id: &str, agent_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        residency::update_residency_agent(&conn, session_id, agent_id)
    }

    /// Parked, unexpired sessions of `agent_id` — the recipients an
    /// `agent_message` broadcast can actually reach.
    pub fn list_resident_sessions_for_agent(&self, agent_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        residency::list_resident_sessions_for_agent(&conn, agent_id, &chrono::Utc::now().to_rfc3339())
    }

    /// Test/deterministic variant: evaluate expiry against `now`.
    pub fn list_resident_sessions_for_agent_at(
        &self,
        agent_id: &str,
        now: &str,
    ) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        residency::list_resident_sessions_for_agent(&conn, agent_id, now)
    }

    /// Parks whose TTL has elapsed; the caller closes each session properly
    /// before clearing its row.
    pub fn list_expired_session_residencies(&self) -> Result<Vec<SessionResidency>> {
        let conn = self.conn.lock().unwrap();
        residency::list_expired_residencies(&conn, &chrono::Utc::now().to_rfc3339())
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
        plan_frames::update_plan_frame_status(
            &conn,
            plan_id,
            version,
            status,
            approved_by,
            approved_at,
        )
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

    pub fn expire_timed_out_plan_frames(&self) -> Result<Vec<(String, u32)>> {
        let conn = self.conn.lock().unwrap();
        plan_frames::expire_timed_out_plan_frames(&conn)
    }

    pub fn get_stale_plan_frames_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<autonoetic_types::plan_frame::PlanFrame>> {
        let conn = self.conn.lock().unwrap();
        plan_frames::get_stale_plan_frames_for_root(&conn, root_session_id)
    }

    /// List the latest **approved** revision of each plan for a root session.
    /// Used by the agent_spawn depends_on enforcement to find the active plan.
    pub fn list_latest_plan_frames_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<autonoetic_types::plan_frame::PlanFrame>> {
        let conn = self.conn.lock().unwrap();
        plan_frames::list_latest_plan_frames_for_root(&conn, root_session_id)
    }

    // -------------------------------------------------------------------------
    // Workbenches
    // -------------------------------------------------------------------------

    pub fn save_workbench(
        &self,
        wb: &autonoetic_types::workbench::WorkbenchProjection,
    ) -> Result<()> {
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

    pub fn save_checkpoint(
        &self,
        cp: &autonoetic_types::workbench::WorkbenchCheckpoint,
    ) -> Result<()> {
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

    // ── Decider appointments (#1195) ─────────────────────────────────────
    //
    // Storage only: validation (capability containment, Critical refusal,
    // advisory-only) lives in `crate::decider_appointment` so it is enforced
    // once, at the appointing act, rather than re-derived by each caller.

    pub fn insert_decider_appointment(
        &self,
        appointment: &autonoetic_types::decider_appointment::DeciderAppointment,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        decider_appointments::insert_appointment(&conn, appointment)
    }

    pub fn get_decider_appointment(
        &self,
        appointment_id: &str,
    ) -> Result<Option<autonoetic_types::decider_appointment::DeciderAppointment>> {
        let conn = self.conn.lock().unwrap();
        decider_appointments::get_appointment(&conn, appointment_id)
    }

    pub fn list_decider_appointments_for_scope(
        &self,
        scope_root_session: &str,
        active_only: bool,
    ) -> Result<Vec<autonoetic_types::decider_appointment::DeciderAppointment>> {
        let conn = self.conn.lock().unwrap();
        decider_appointments::list_appointments_for_scope(&conn, scope_root_session, active_only)
    }

    pub fn list_active_decider_appointments(
        &self,
    ) -> Result<Vec<autonoetic_types::decider_appointment::DeciderAppointment>> {
        let conn = self.conn.lock().unwrap();
        decider_appointments::list_active_appointments(&conn)
    }

    /// Returns false when the appointment does not exist or was already
    /// revoked — idempotent, and a second revoke never rewrites the first
    /// one's attribution.
    pub fn revoke_decider_appointment(
        &self,
        appointment_id: &str,
        revoked_by: &str,
        revoked_at: &str,
        reason: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        decider_appointments::revoke_appointment(
            &conn,
            appointment_id,
            revoked_by,
            revoked_at,
            reason,
        )
    }

    /// Bind the gateway-created peer-root decider session (#1196).
    pub fn set_decider_appointment_session(
        &self,
        appointment_id: &str,
        decider_session: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        decider_appointments::set_decider_session(&conn, appointment_id, decider_session)
    }

    /// Increment the decided-gate tally that `max_gates` bounds.
    pub fn record_decider_gate_decided(&self, appointment_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        decider_appointments::record_gate_decided(&conn, appointment_id)
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

    /// Persist all buffered timeline events in a single transaction.
    ///
    /// Two properties this owes callers, neither of which it used to have
    /// (#1238):
    ///
    /// **The buffer is only emptied by a write that succeeded.** The drain used
    /// to happen before the transaction, so any error after it — transaction
    /// start, one failing insert, commit — dropped every drained event on the
    /// floor. A single `SQLITE_BUSY` under contention silently erased up to
    /// `LIVE_DIGEST_BUFFER_CAPACITY` rows of the operator timeline, with no gap
    /// marker to distinguish "lost" from "never happened". Events are now
    /// restored to the front of the buffer on failure, in order, so the next
    /// flush retries them.
    ///
    /// **The drain is atomic with respect to other flushers.** The buffer lock
    /// used to be released before the connection lock was taken, so a reader —
    /// every read path flushes first — could drain a *different* thread's
    /// events into its own transaction. That thread would then flush, find the
    /// buffer empty, return `Ok(())`, and query before the other transaction
    /// had committed: it could not see its own writes. Holding the buffer lock
    /// across the write serialises flushes and closes both halves.
    ///
    /// Lock order is buffer → connection. That is the order every emit site
    /// already follows (they release the connection lock before emitting,
    /// precisely because `create_live_digest_event` may flush), so no inversion
    /// is introduced.
    ///
    /// Producers block for the length of one batched insert. That is normally
    /// at most `LIVE_DIGEST_BUFFER_CAPACITY` rows, since a healthy buffer
    /// flushes at capacity — but under sustained write failure the retained
    /// batch grows, so the worst case is `LIVE_DIGEST_RETRY_CAPACITY`. A
    /// database failing every write is already degraded; bounding the retry
    /// buffer is what bounds this hold time too.
    pub fn flush_live_digest_events(&self) -> Result<()> {
        let mut buf = self.live_digest_buffer.lock().unwrap();
        if buf.is_empty() {
            return Ok(());
        }
        // Take a copy rather than draining: the buffer is only cleared once the
        // transaction has committed.
        let pending: Vec<LiveDigestEventRecord> = buf.clone();

        match self.write_live_digest_events(&pending) {
            Ok(()) => {
                // Anything appended while we held the lock is impossible (we
                // hold it), so the buffer is exactly what we just wrote.
                buf.clear();
                Ok(())
            }
            Err(e) => {
                // The buffer still holds `pending` — nothing to restore. Left
                // in place so the next flush retries rather than losing them.
                //
                // Bounded, so a database that never becomes writable degrades
                // to visible loss instead of unbounded memory growth. Oldest
                // first: newer timeline events are the ones an operator is
                // most likely to still be looking at.
                let dropped = buf.len().saturating_sub(LIVE_DIGEST_RETRY_CAPACITY);
                if dropped > 0 {
                    buf.drain(..dropped);
                    tracing::error!(
                        target: "live_digest",
                        dropped,
                        retained = buf.len(),
                        "Live-digest retry buffer is full after repeated flush \
                         failures; dropped the oldest timeline events"
                    );
                }
                tracing::warn!(
                    target: "live_digest",
                    error = %e,
                    buffered = buf.len(),
                    "Failed to flush live-digest events; retained in the buffer for retry"
                );
                Err(e)
            }
        }
    }

    /// How many timeline events are buffered and not yet committed.
    ///
    /// A non-zero value after a flush means the write failed and the events are
    /// held for retry rather than lost — the property that distinguishes the
    /// current behaviour from the one that silently dropped them.
    pub fn pending_live_digest_events(&self) -> usize {
        self.live_digest_buffer
            .lock()
            .map(|b| b.len())
            .unwrap_or_default()
    }

    /// Write `events` in one transaction. Split out so the buffer lock can be
    /// held across the call without the SQL obscuring the ownership rules above.
    fn write_live_digest_events(&self, events: &[LiveDigestEventRecord]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            // Ignore a colliding `event_id`, and *only* that.
            //
            // Retention makes a permanent per-row error poisonous in a way it
            // never was before: a duplicate id would abort the batch, the batch
            // would be retained, and every later flush would fail on the same
            // row — the buffer would grow and no timeline event would ever be
            // written again. Skipping the duplicate is also the right semantics
            // for an append-only log keyed by a generated id: a colliding id
            // means this event is already recorded.
            //
            // `ON CONFLICT(event_id) DO NOTHING` rather than `INSERT OR
            // IGNORE`, which suppresses *every* constraint violation. Under
            // `OR IGNORE` a data-shape bug — a NOT NULL or CHECK failure —
            // would be skipped, the transaction would commit, and the buffer
            // would clear as though the row had been written. That is the same
            // silent loss this function exists to remove, just relocated.
            // Anything that is not a primary-key conflict must still fail the
            // flush so the batch is retained and the error is logged.
            let mut stmt = tx.prepare(
                "INSERT INTO live_digest_events (
                    event_id, root_session_id, source_session_id, turn_id, source_agent_id,
                    source_node_id, event_type, payload, created_at,
                    principal_kind, principal_id, role, altitude, refs_json
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                ON CONFLICT(event_id) DO NOTHING",
            )?;
            for event in events {
                stmt.execute(params![
                    &event.event_id,
                    &event.root_session_id,
                    &event.source_session_id,
                    event.turn_id.as_deref(),
                    event.source_agent_id.as_deref(),
                    &event.source_node_id,
                    &event.event_type,
                    event.payload.as_deref(),
                    &event.created_at,
                    event.principal_kind.as_deref(),
                    event.principal_id.as_deref(),
                    event.role.as_deref(),
                    event.altitude.as_deref(),
                    event.refs_json.as_deref(),
                ])?;
            }
            stmt.finalize()?;
        }
        tx.commit()?;
        Ok(())
    }
}

impl Drop for GatewayStore {
    fn drop(&mut self) {
        // The one caller that cannot propagate. Discarding the error silently
        // meant the tail of the buffer vanished at process exit — the moment it
        // matters most — with nothing in the log to say so (#1238). Retry is not
        // available here, so the least this owes an operator is a record that
        // the timeline is short.
        if let Err(e) = self.flush_live_digest_events() {
            let buffered = self
                .live_digest_buffer
                .lock()
                .map(|b| b.len())
                .unwrap_or_default();
            tracing::error!(
                target: "live_digest",
                error = %e,
                buffered,
                "Final live-digest flush failed on store shutdown; these timeline \
                 events are lost"
            );
        }
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

    // ── #1238: live-digest flush durability ─────────────────────────────
    //
    // The flush used to `drain(..)` before opening its transaction, so any
    // error after the drain discarded the whole batch: out of the buffer, and
    // never written. These pin the ownership rule — the buffer is emptied only
    // by a write that committed.

    fn live_event(id: &str, root: &str) -> super::LiveDigestEventRecord {
        super::LiveDigestEventRecord {
            event_id: id.to_string(),
            root_session_id: root.to_string(),
            source_session_id: root.to_string(),
            turn_id: None,
            source_agent_id: Some("planner.default".to_string()),
            source_node_id: "node".to_string(),
            event_type: "turn.start".to_string(),
            payload: None,
            created_at: "2026-06-01T10:00:00+00:00".to_string(),
            principal_kind: None,
            principal_id: None,
            role: None,
            altitude: None,
            refs_json: None,
        }
    }

    /// Drop the table to make the next flush fail the way a transient fault
    /// would, without needing to provoke real lock contention.
    fn break_live_digest_table(store: &GatewayStore) {
        store
            .conn
            .lock()
            .unwrap()
            .execute("DROP TABLE live_digest_events", [])
            .unwrap();
    }

    fn restore_live_digest_table(store: &GatewayStore) {
        store
            .conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS live_digest_events (
                    event_id TEXT PRIMARY KEY,
                    root_session_id TEXT NOT NULL,
                    source_session_id TEXT NOT NULL,
                    turn_id TEXT,
                    source_agent_id TEXT,
                    source_node_id TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    payload TEXT,
                    created_at TEXT NOT NULL,
                    principal_kind TEXT,
                    principal_id TEXT,
                    role TEXT,
                    altitude TEXT,
                    refs_json TEXT
                );",
            )
            .unwrap();
    }

    fn live_event_count(store: &GatewayStore) -> i64 {
        store
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM live_digest_events", [], |r| r.get(0))
            .unwrap()
    }

    /// Only a primary-key conflict is skipped. Any other constraint violation
    /// must still fail the flush, so the batch is retained and logged.
    ///
    /// `INSERT OR IGNORE` would suppress all of them: the transaction would
    /// commit, the buffer would clear, and a malformed row would vanish exactly
    /// as silently as the loss this function exists to remove.
    #[test]
    fn a_non_conflict_constraint_violation_still_fails_the_flush() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = GatewayStore::open(dir.path())?;

        // Re-create the table with a CHECK the writer can violate. NOT NULL is
        // unreachable through the API (those columns are non-Option `String`),
        // so a CHECK stands in for the general data-shape bug.
        store
            .conn
            .lock()
            .unwrap()
            .execute_batch(
                "DROP TABLE live_digest_events;
                 CREATE TABLE live_digest_events (
                    event_id TEXT PRIMARY KEY,
                    root_session_id TEXT NOT NULL,
                    source_session_id TEXT NOT NULL,
                    turn_id TEXT,
                    source_agent_id TEXT,
                    source_node_id TEXT NOT NULL,
                    event_type TEXT NOT NULL CHECK (event_type <> 'poison'),
                    payload TEXT,
                    created_at TEXT NOT NULL,
                    principal_kind TEXT,
                    principal_id TEXT,
                    role TEXT,
                    altitude TEXT,
                    refs_json TEXT
                 );",
            )
            .unwrap();

        let mut bad = live_event("evt-bad", "root-5");
        bad.event_type = "poison".to_string();
        store.create_live_digest_event(&bad)?;
        store.create_live_digest_event(&live_event("evt-ok", "root-5"))?;

        assert!(
            store.flush_live_digest_events().is_err(),
            "a CHECK violation must surface, not be silently skipped"
        );
        assert_eq!(
            store.pending_live_digest_events(),
            2,
            "the batch must be retained so the failure is retryable and visible"
        );
        assert_eq!(live_event_count(&store), 0, "the transaction rolled back");
        Ok(())
    }

    #[test]
    fn failed_flush_retains_events_instead_of_discarding_them() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = GatewayStore::open(dir.path())?;

        store.create_live_digest_event(&live_event("evt-a", "root-1"))?;
        store.create_live_digest_event(&live_event("evt-b", "root-1"))?;

        break_live_digest_table(&store);
        assert!(
            store.flush_live_digest_events().is_err(),
            "flush must surface the write failure"
        );

        // Before the fix this was 0: both events had been drained out of the
        // buffer and were never written anywhere.
        assert_eq!(
            store.pending_live_digest_events(),
            2,
            "a failed flush must retain its batch for retry"
        );
        Ok(())
    }

    #[test]
    fn retained_events_land_once_the_write_path_recovers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = GatewayStore::open(dir.path())?;

        store.create_live_digest_event(&live_event("evt-a", "root-2"))?;
        break_live_digest_table(&store);
        assert!(store.flush_live_digest_events().is_err());
        assert_eq!(store.pending_live_digest_events(), 1);

        // Retention is only meaningful if the retry actually commits.
        restore_live_digest_table(&store);
        store.flush_live_digest_events()?;
        assert_eq!(store.pending_live_digest_events(), 0);
        assert_eq!(live_event_count(&store), 1);
        Ok(())
    }

    #[test]
    fn successful_flush_clears_the_buffer_and_does_not_rewrite() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = GatewayStore::open(dir.path())?;

        for i in 0..3 {
            store.create_live_digest_event(&live_event(&format!("evt-{i}"), "root-3"))?;
        }
        assert_eq!(store.pending_live_digest_events(), 3);

        store.flush_live_digest_events()?;
        assert_eq!(store.pending_live_digest_events(), 0);
        assert_eq!(live_event_count(&store), 3);

        // A second flush is a no-op, not a duplicate write.
        store.flush_live_digest_events()?;
        assert_eq!(live_event_count(&store), 3);
        Ok(())
    }

    /// Retention must not let one permanently-bad row block the timeline
    /// forever. A colliding `event_id` is ignored rather than aborting the
    /// batch, so its companions still land.
    #[test]
    fn a_duplicate_event_id_does_not_poison_the_batch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = GatewayStore::open(dir.path())?;

        store.create_live_digest_event(&live_event("evt-dup", "root-4"))?;
        store.flush_live_digest_events()?;

        store.create_live_digest_event(&live_event("evt-dup", "root-4"))?;
        store.create_live_digest_event(&live_event("evt-new", "root-4"))?;
        store.flush_live_digest_events()?;

        assert_eq!(store.pending_live_digest_events(), 0);
        assert_eq!(
            live_event_count(&store),
            2,
            "the duplicate is ignored and its batch-mate is written"
        );
        Ok(())
    }

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
    fn test_list_artifact_refs_for_session_respects_scope_visibility() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        store.create_artifact_ref(&artifact_ref(
            "ar.global.001.aaaa",
            ArtifactRefScopeType::Global,
            "__global__",
            None,
        ))?;
        store.create_artifact_ref(&artifact_ref(
            "ar.root.002.bbbb",
            ArtifactRefScopeType::Session,
            "sess-root",
            None,
        ))?;
        store.create_artifact_ref(&artifact_ref(
            "ar.child.003.cccc",
            ArtifactRefScopeType::Session,
            "sess-root/coder.default-x",
            None,
        ))?;
        store.create_artifact_ref(&artifact_ref(
            "ar.other.004.dddd",
            ArtifactRefScopeType::Session,
            "sess-unrelated",
            None,
        ))?;

        let visible: Vec<String> = store
            .list_artifact_refs_for_session("sess-root/coder.default-x")?
            .into_iter()
            .map(|r| r.ref_id)
            .collect();

        assert!(visible.contains(&"ar.global.001.aaaa".to_string()));
        assert!(visible.contains(&"ar.root.002.bbbb".to_string()));
        assert!(visible.contains(&"ar.child.003.cccc".to_string()));
        assert!(
            !visible.contains(&"ar.other.004.dddd".to_string()),
            "unrelated session refs must not be visible: {visible:?}"
        );

        // From the root session, the child-scoped ref is not visible.
        let root_visible: Vec<String> = store
            .list_artifact_refs_for_session("sess-root")?
            .into_iter()
            .map(|r| r.ref_id)
            .collect();
        assert!(root_visible.contains(&"ar.global.001.aaaa".to_string()));
        assert!(root_visible.contains(&"ar.root.002.bbbb".to_string()));
        assert!(!root_visible.contains(&"ar.child.003.cccc".to_string()));

        Ok(())
    }

    #[test]
    fn test_artifact_ref_revocation_and_expiry_filter_resolution_and_list() -> Result<()> {        let temp_dir = tempfile::tempdir()?;
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
            egress_label: None,
            mount_set: None,
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
            egress_label: None,
            mount_set: None,
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

    /// Seed one `active` transcript and park it at `lifecycle`.
    fn seed_parked_transcript(
        store: &GatewayStore,
        session_id: &str,
        lifecycle: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        store.upsert_session_transcript(&SessionTranscriptRecord {
            transcript_id: format!("stx-{session_id}"),
            session_id: session_id.to_string(),
            root_session_id: "root1".to_string(),
            agent_id: "agent1".to_string(),
            revision_id: None,
            user_id: None,
            started_at: now,
            ended_at: None,
            status: "active".to_string(),
            turn_count: 1,
            transcript_handle: None,
            excerpt: None,
            origin_node_id: None,
        })?;
        store.set_session_lifecycle_state(session_id, lifecycle)?;
        Ok(())
    }

    /// The polite path must keep preserving a resumable lifecycle: `close_session`
    /// reports `completed` at the end of every turn, and the session resumes on
    /// the next operator message. Since #1057 this covers every state
    /// `SessionLifecycleState::is_resumable` owns — `hibernated` (between-turn
    /// yield), `awaiting_gate` (gate-suspended), `idle` (parked resident), and
    /// `paused` (operator pause). A `completed` finalize on any of these must
    /// leave the resumable state in place.
    #[test]
    fn finalize_completed_preserves_resumable_lifecycle() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;
        let now = chrono::Utc::now().to_rfc3339();

        for state in ["hibernated", "awaiting_gate", "idle", "paused"] {
            let sid = format!("sess-{state}");
            seed_parked_transcript(&store, &sid, state)?;
            store.finalize_session_transcript(&sid, &now, "completed")?;
            assert_eq!(
                store.get_session_lifecycle_state(&sid)?.as_deref(),
                Some(state),
                "finalize(completed) must not terminate a {state} session"
            );
        }

        // A non-resumable lifecycle (`active`) *is* overwritten to terminal —
        // that is the path that finalizes a genuinely-ending session.
        seed_parked_transcript(&store, "sess-active", "active")?;
        store.finalize_session_transcript("sess-active", &now, "completed")?;
        assert_eq!(
            store.get_session_lifecycle_state("sess-active")?.as_deref(),
            Some("terminated:completed"),
            "finalize(completed) must terminate an active session"
        );

        Ok(())
    }

    /// A session the caller knows is unreachable must reach `terminated:*` even
    /// from a resumable lifecycle — otherwise `find_orphaned_sessions` keeps
    /// selecting it and the reaper spins forever.
    #[test]
    fn terminate_clears_resumable_lifecycle() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;
        let now = chrono::Utc::now().to_rfc3339();

        for state in ["hibernated", "awaiting_gate", "idle", "paused", "active"] {
            let sid = format!("sess-{state}");
            seed_parked_transcript(&store, &sid, state)?;
            store.terminate_session_transcript(&sid, &now, "failed")?;
            assert_eq!(
                store.get_session_lifecycle_state(&sid)?.as_deref(),
                Some("terminated:failed"),
                "terminate must clear a {state} lifecycle"
            );
        }

        Ok(())
    }

    /// The status→lifecycle mapping must match what the read paths already use
    /// (`find_orphaned_sessions`, `search_session_transcripts`, migration v64):
    /// `closed` is a terminal *success*, not a failure. The reclamation sweep
    /// writes `closed`, so getting this wrong would mis-record those sessions.
    /// An unrecognised status must still land terminal — never leaving the row
    /// resumable is the property that prevents the reap livelock.
    #[test]
    fn terminate_maps_status_to_lifecycle_like_the_read_paths() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;
        let now = chrono::Utc::now().to_rfc3339();

        for (status, expected) in [
            ("completed", "terminated:completed"),
            ("closed", "terminated:completed"),
            ("failed", "terminated:failed"),
            // Not a terminal status: still terminated, recorded as a failure.
            ("suspended", "terminated:failed"),
        ] {
            let sid = format!("sess-status-{status}");
            seed_parked_transcript(&store, &sid, "hibernated")?;
            store.terminate_session_transcript(&sid, &now, status)?;
            assert_eq!(
                store.get_session_lifecycle_state(&sid)?.as_deref(),
                Some(expected),
                "status {status:?} must map to {expected}"
            );
        }

        Ok(())
    }

    /// The first terminal verdict wins, so repeated sweeps are idempotent and
    /// `ended_at` keeps recording when the session actually died.
    #[test]
    fn terminate_preserves_first_terminal_verdict() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        seed_parked_transcript(&store, "sess1", "hibernated")?;
        let died_at = chrono::Utc::now().to_rfc3339();
        store.terminate_session_transcript("sess1", &died_at, "failed")?;

        let later = (chrono::Utc::now() + chrono::Duration::minutes(20)).to_rfc3339();
        store.terminate_session_transcript("sess1", &later, "completed")?;

        let row = store
            .find_transcript_by_session_id("sess1")?
            .expect("transcript should exist");
        assert_eq!(row.status, "failed", "later verdict must not overwrite");
        assert_eq!(
            row.ended_at.as_deref(),
            Some(died_at.as_str()),
            "ended_at must keep the real time of death, not the last sweep"
        );
        assert_eq!(
            store.get_session_lifecycle_state("sess1")?.as_deref(),
            Some("terminated:failed")
        );

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

        let results =
            store.search_session_transcripts(None, None, None, Some("completed"), None, 10)?;
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
                carried_from: None,
            },
            RoleVerdictSummary {
                role: PromotionRole::UnitTestRunner,
                agent_id: "unit_test_runner.default".to_string(),
                passed: true,
                findings_summary: "All 12 tests pass".to_string(),
                evidence_ref: Some("art_artifact123".to_string()),
                recorded_at: chrono::Utc::now().to_rfc3339(),
                carried_from: None,
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
        assert_eq!(fetched.role_verdicts[0].role.as_str(), "static_evaluator");
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
        assert_eq!(
            resolved.decision_reason.as_deref(),
            Some("Looks good, promote")
        );

        Ok(())
    }
}
