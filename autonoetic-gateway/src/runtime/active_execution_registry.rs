//! In-memory registry of live workflow task executions (tokio abort handles) and sandbox
//! child PIDs for emergency stop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex};
use tokio::task::AbortHandle;

#[derive(Clone, Default)]
pub struct NativeToolDiscoveryCatalog {
    pub registered: std::collections::HashSet<String>,
    pub available: std::collections::HashSet<String>,
}

/// Scope passed into native tools (e.g. `sandbox_exec`) for PID registration.
#[derive(Clone)]
pub struct NativeToolRunContext {
    pub registry: Arc<ActiveExecutionRegistry>,
    pub root_session_id: String,
    pub workflow_id: Option<String>,
    pub task_id: Option<String>,
    pub session_id: String,
    pub agent_id: String,
    pub live_digest: Option<Arc<Mutex<crate::runtime::live_digest::LiveDigestWriter>>>,
    pub live_report: Option<Arc<Mutex<crate::runtime::session_report::SessionReportWriter>>>,
    /// User ID for profile binding resolution (if authenticated).
    pub user_id: Option<String>,
    /// Artifact ID whose layers should be auto-mounted into sandbox_exec calls.
    pub artifact_id: Option<String>,
    /// Shared suppression target for `sentinel.suppress`. The tool writes a
    /// suppress-until turn counter here; the lifecycle reads it after tool
    /// batches to gate divergence messaging.
    pub sentinel_suppress_target: Option<Arc<AtomicU64>>,
    /// Shared discovered-tools set. `tool_discover` writes here; the lifecycle
    /// reads and drains after tool execution to update the session surface.
    pub discovered_tools: Option<Arc<Mutex<std::collections::HashSet<String>>>>,
    /// Shared per-executor annotation counter (#1092). `digest_annotate`
    /// increments it and echoes the running total in its result, so the
    /// model sees redundancy building up in-context instead of discovering
    /// it via a LoopGuard trip. Session-scoped like the guard state.
    pub annotation_counter: Option<Arc<AtomicU32>>,
    /// Capability-filtered native tool catalog used by `tool_discover` to
    /// distinguish available, forbidden, and unmatched requests.
    pub tool_discovery_catalog: Option<Arc<NativeToolDiscoveryCatalog>>,
    /// Wake hint for the post-approval guardrail on `agent_list`.
    /// Set by the TUI after a plan-approval wake message.
    pub wake_hint: Option<crate::execution::WakeHintState>,
    /// Shared map of root-session → wake hint, populated by
    /// `GatewayExecutionService.register_wake_hint` and read by
    /// `agent_list` to check whether the guardrail is active.
    pub wake_hints_map: Option<Arc<Mutex<std::collections::HashMap<String, crate::execution::WakeHintState>>>>,
    /// The sending session's accumulated egress taint at tool-call time (RFC
    /// data-envelopes §5.5). Threaded from the executor's label sidecar so a
    /// tool that hands content to another session — `agent_message` — can stamp
    /// the payload with what the sender touched, closing the `LocalAgent` hole.
    /// `None` ⇒ the sender touched nothing restrictive (unrestricted payload).
    pub egress_taint: Option<autonoetic_types::egress::EgressLabel>,
    /// Target sink for stored-content recall/search filters (RFC §6). `None`
    /// is treated as [`Sink::RemoteModel`] (fail-closed) by
    /// [`crate::runtime::egress_stored::query_sink_or_remote`].
    pub egress_query_sink: Option<autonoetic_types::egress::Sink>,
}

#[derive(Clone)]
struct SandboxChildRecord {
    root_session_id: String,
    pid: u32,
}

/// Unregisters a sandbox PID when dropped (normal completion).
pub struct SandboxPidGuard {
    registry: Arc<ActiveExecutionRegistry>,
    reg_id: String,
}

impl Drop for SandboxPidGuard {
    fn drop(&mut self) {
        self.registry.unregister_sandbox_pid(&self.reg_id);
    }
}

pub struct ActiveExecutionRegistry {
    workflow_task_abort: Mutex<HashMap<String, AbortHandle>>,
    sandbox_child_pids: Mutex<HashMap<String, SandboxChildRecord>>,
    /// Cooperative pause requests keyed by root session id. Set by
    /// `root_session.pause`; consumed (atomically check-and-clear) by the
    /// execute loop at the pre-LLM checkpoint, which yields with
    /// `YieldReason::ManualStop`. Unlike emergency stop (hard abort), a pause
    /// is gentle — the current tool batch completes before the turn parks.
    pause_requests: Mutex<HashMap<String, String>>,
}

impl ActiveExecutionRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            workflow_task_abort: Mutex::new(HashMap::new()),
            sandbox_child_pids: Mutex::new(HashMap::new()),
            pause_requests: Mutex::new(HashMap::new()),
        })
    }

    /// Register an operator-requested pause for the given root session. The
    /// running turn (if any) will yield at the next pre-LLM checkpoint.
    pub fn request_pause(&self, root_session_id: &str, reason: &str) {
        self.pause_requests
            .lock()
            .unwrap()
            .insert(root_session_id.to_string(), reason.to_string());
    }

    /// Clear a pending pause request without consuming it (resume button).
    pub fn clear_pause(&self, root_session_id: &str) {
        self.pause_requests
            .lock()
            .unwrap()
            .remove(root_session_id);
    }

    /// Returns `true` if a pause is pending but not yet consumed by the loop.
    pub fn is_pause_pending(&self, root_session_id: &str) -> bool {
        self.pause_requests
            .lock()
            .unwrap()
            .contains_key(root_session_id)
    }

    /// Atomically take (consume) a pending pause request. Called by the
    /// execute loop at the cooperative checkpoint; returns the reason if a
    /// pause was pending so the caller can yield with `ManualStop`.
    pub fn take_pause_request(&self, root_session_id: &str) -> Option<String> {
        self.pause_requests
            .lock()
            .unwrap()
            .remove(root_session_id)
    }

    fn workflow_task_key(workflow_id: &str, task_id: &str) -> String {
        format!("{workflow_id}:{task_id}")
    }

    pub fn register_workflow_task(&self, workflow_id: &str, task_id: &str, handle: AbortHandle) {
        let mut g = self.workflow_task_abort.lock().unwrap();
        g.insert(Self::workflow_task_key(workflow_id, task_id), handle);
    }

    pub fn unregister_workflow_task(&self, workflow_id: &str, task_id: &str) {
        let mut g = self.workflow_task_abort.lock().unwrap();
        g.remove(&Self::workflow_task_key(workflow_id, task_id));
    }

    /// Best-effort abort of scheduler-spawned workflow tasks for the given workflow.
    pub fn abort_workflow_tasks(&self, workflow_id: &str, task_ids: &[String]) -> usize {
        let mut n = 0usize;
        let mut g = self.workflow_task_abort.lock().unwrap();
        for tid in task_ids {
            let k = Self::workflow_task_key(workflow_id, tid);
            if let Some(h) = g.remove(&k) {
                h.abort();
                n += 1;
            }
        }
        n
    }

    fn unregister_sandbox_pid(&self, reg_id: &str) {
        self.sandbox_child_pids.lock().unwrap().remove(reg_id);
    }

    /// Track a sandbox or script child process until it exits (see [`SandboxPidGuard`]).
    pub fn register_sandbox_child_pid(
        self: &Arc<Self>,
        root_session_id: &str,
        pid: u32,
    ) -> SandboxPidGuard {
        let reg_id = format!("sb-{}", uuid::Uuid::new_v4());
        self.sandbox_child_pids.lock().unwrap().insert(
            reg_id.clone(),
            SandboxChildRecord {
                root_session_id: root_session_id.to_string(),
                pid,
            },
        );
        SandboxPidGuard {
            registry: Arc::clone(self),
            reg_id,
        }
    }

    /// Send SIGKILL to sandbox/script children still attributed to this root session.
    pub fn kill_sandbox_children_for_root(&self, root_session_id: &str) -> Vec<u32> {
        let mut g = self.sandbox_child_pids.lock().unwrap();
        let keys: Vec<String> = g
            .iter()
            .filter(|(_, v)| v.root_session_id == root_session_id)
            .map(|(k, _)| k.clone())
            .collect();
        let mut killed = Vec::new();
        for k in keys {
            if let Some(rec) = g.remove(&k) {
                #[cfg(unix)]
                signal_kill(rec.pid);
                killed.push(rec.pid);
            }
        }
        killed
    }
}

#[cfg(unix)]
fn signal_kill(pid: u32) {
    unsafe {
        let _ = libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}
