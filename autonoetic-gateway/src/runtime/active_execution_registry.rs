//! In-memory registry of live workflow task executions (tokio abort handles) and sandbox
//! child PIDs for emergency stop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::task::AbortHandle;

/// Scope passed into native tools (e.g. `sandbox.exec`) for PID registration.
#[derive(Clone)]
pub struct NativeToolRunContext {
    pub registry: Arc<ActiveExecutionRegistry>,
    pub root_session_id: String,
    pub workflow_id: Option<String>,
    pub task_id: Option<String>,
    pub session_id: String,
    pub agent_id: String,
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
}

impl ActiveExecutionRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            workflow_task_abort: Mutex::new(HashMap::new()),
            sandbox_child_pids: Mutex::new(HashMap::new()),
        })
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
    pub fn register_sandbox_child_pid(self: &Arc<Self>, root_session_id: &str, pid: u32) -> SandboxPidGuard {
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
