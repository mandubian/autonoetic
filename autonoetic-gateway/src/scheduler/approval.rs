//! Approval resolution for the background scheduler.
//! Handles loading, approving, and rejecting approval requests.
//!
//! The gateway follows a "Dumb Gate / Agent Retry" model: on approval it merely
//! unblocks the workflow and notifies the agent, which retries the tool call
//! with an approval_ref. The gateway never auto-executes tool calls on behalf
//! of the agent.

use crate::execution::{gateway_actor_id, init_gateway_causal_logger};
use crate::tracing::{EventScope, SessionId, TraceSession};
use autonoetic_types::background::{
    ApprovalDecision, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;

/// Load approval requests from the gateway store for a specific session.
///
/// Fetches pending approval requests stored directly in the SQLite `GatewayStore`.
/// Returns an empty list if the gateway store is unavailable.
pub fn load_approval_requests(
    _config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
) -> anyhow::Result<Vec<ApprovalRequest>> {
    if let Some(store) = gateway_store {
        store.get_pending_approvals()
    } else {
        // GatewayStore not available - return empty list instead of error
        Ok(Vec::new())
    }
}

/// Pending approvals whose [`ApprovalRequest::session_id`] shares the same root session as
/// `root_session_id` (see [`crate::runtime::content_store::root_session_id`]).
pub fn pending_approval_requests_for_root(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    root_session_id: &str,
) -> anyhow::Result<Vec<ApprovalRequest>> {
    let all = load_approval_requests(config, gateway_store)?;
    Ok(all
        .into_iter()
        .filter(|r| {
            crate::runtime::content_store::root_session_id(&r.session_id) == root_session_id
        })
        .collect())
}

/// Pending [`ScheduledAction::SandboxExec`] approvals for an exact `session_id` (e.g. child
/// delegation path), oldest first. Used to stop repeated `sandbox.exec` calls from minting many
/// `apr-*` rows while an approval is still open.
pub fn pending_sandbox_exec_requests_for_session(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
) -> anyhow::Result<Vec<ApprovalRequest>> {
    if session_id.is_empty() {
        return Ok(Vec::new());
    }
    let mut v: Vec<ApprovalRequest> = load_approval_requests(config, gateway_store)?
        .into_iter()
        .filter(|r| r.session_id == session_id)
        .filter(|r| matches!(r.action, ScheduledAction::SandboxExec { .. }))
        .collect();
    v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(v)
}

pub fn approve_request(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    decided_by: &str,
    reason: Option<String>,
) -> anyhow::Result<ApprovalDecision> {
    let decision = decide_request(
        config,
        gateway_store,
        request_id,
        decided_by,
        reason,
        ApprovalStatus::Approved,
    )?;

    // Dumb Gate model: notify the waiting session, do not auto-execute.
    if should_resume_waiting_session(&decision) {
        if let Err(e) = resume_session_after_approval(config, gateway_store, &decision) {
            tracing::warn!(
                target: "approval",
                request_id = %decision.request_id,
                error = %e,
                "Failed to send session resume notification"
            );
        }
    } else {
        tracing::info!(
            target: "approval",
            request_id = %decision.request_id,
            workflow_id = ?decision.workflow_id,
            task_id = ?decision.task_id,
            "Skipping direct session resume; workflow-bound task will continue via durable re-queue"
        );
    }

    // Unblock the task in the workflow (if bound to one)
    unblock_task_on_approval(config, gateway_store, &decision);

    Ok(decision)
}

pub fn reject_request(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    decided_by: &str,
    reason: Option<String>,
) -> anyhow::Result<ApprovalDecision> {
    let decision = decide_request(
        config,
        gateway_store,
        request_id,
        decided_by,
        reason,
        ApprovalStatus::Rejected,
    )?;

    // Workflow-bound tasks surface rejection through task failure + workflow
    // resume. Non-workflow callers still need a direct notification.
    if should_resume_waiting_session(&decision) {
        resume_session_after_approval(config, gateway_store, &decision)?;
    } else {
        tracing::info!(
            target: "approval",
            request_id = %decision.request_id,
            workflow_id = ?decision.workflow_id,
            task_id = ?decision.task_id,
            "Skipping direct rejection resume; workflow-bound task will continue via workflow failure"
        );
    }

    // Unblock the task in the workflow (marks as Failed)
    unblock_task_on_approval(config, gateway_store, &decision);

    Ok(decision)
}

/// Queue durable session notifications after approval/rejection.
///
/// This function persists approval-resolution signals that are consumed by
/// gateway-owned delivery loops and/or channel clients (for example, the TUI
/// chat client resuming on its own connection).
///
/// Under the "Dumb Gate" model, the gateway never auto-executes tool calls.
/// It merely notifies the agent that approval was granted, and the agent
/// retries the tool call with an approval_ref.
fn resume_session_after_approval(
    _config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    decision: &ApprovalDecision,
) -> anyhow::Result<()> {
    // Resume for agent_install and sandbox_exec actions - both have a caller waiting
    let is_supported_action = matches!(
        &decision.action,
        autonoetic_types::background::ScheduledAction::AgentInstall { .. }
            | autonoetic_types::background::ScheduledAction::SandboxExec { .. }
    );

    if !is_supported_action {
        tracing::warn!(
            target: "approval",
            request_id = %decision.request_id,
            action = ?std::mem::discriminant(&decision.action),
            "Unsupported action type for auto-execute"
        );
        return Ok(());
    }

    let session_id = &decision.session_id;
    if session_id.is_empty() {
        return Ok(());
    }

    tracing::info!(
        target: "approval",
        request_id = %decision.request_id,
        session_id = %session_id,
        status = ?decision.status,
        "Resuming session after approval resolution"
    );

    // Build a synthetic message that the gateway will route to the waiting agent
    let status_str = match decision.status {
        ApprovalStatus::Approved => "approved",
        ApprovalStatus::Rejected => "rejected",
    };

    // Extract agent_id and build status message based on action type
    let (agent_id, status_message) = match &decision.action {
        autonoetic_types::background::ScheduledAction::AgentInstall { agent_id, .. } => {
            let msg = if decision.status == ApprovalStatus::Approved {
                format!(
                    "Approval {} granted for installing agent '{}'. You must now RE-RUN agent.install with install_approval_ref='{}'.",
                    decision.request_id, agent_id, decision.request_id
                )
            } else {
                format!(
                    "The approval for installing agent '{}' was {}.",
                    agent_id, status_str
                )
            };
            (agent_id.clone(), msg)
        }
        autonoetic_types::background::ScheduledAction::SandboxExec { command, .. } => {
            let msg = if decision.status == ApprovalStatus::Approved {
                format!(
                    "Approval {} granted for your pending sandbox.exec. You must now RE-RUN your sandbox.exec command exactly as before, adding 'approval_ref': '{}' to the arguments.\nCommand: {}",
                    decision.request_id, decision.request_id, command
                )
            } else {
                format!(
                    "Sandbox execution was rejected. Request: {}",
                    decision.request_id
                )
            };
            // Use the decision-level requester id to avoid brittle parsing of
            // nested session names.
            (decision.agent_id.clone(), msg)
        }
        _ => (
            "unknown".to_string(),
            format!(
                "Approval {} for request {}",
                status_str, decision.request_id
            ),
        ),
    };

    // Write approval resolution signal to GatewayStore for scheduler delivery (enables auto-resume).
    let signal = super::signal::Signal::ApprovalResolved {
        request_id: decision.request_id.clone(),
        agent_id: agent_id.clone(),
        status: status_str.to_string(),
        install_completed: false,
        message: status_message.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };

    // Write signal to the child session (the original waiting runtime).
    // write_signal persists the record to GatewayStore for durable scheduler delivery.
    if let Err(e) = super::signal::write_signal(
        gateway_store.as_deref(),
        session_id,
        &decision.request_id,
        &signal,
    ) {
        tracing::warn!(
            target: "approval",
            request_id = %decision.request_id,
            error = %e,
            "Failed to write approval signal to store"
        );
    }

    // Use root_session_id from the task graph to determine the parent,
    // rather than string-parsing the session ID.
    let notify_parent = should_notify_parent_session(decision);
    let parent_session_id = decision.root_session_id.as_deref().unwrap_or(session_id);

    if notify_parent {
        tracing::info!(
            target: "approval",
            parent_session = %parent_session_id,
            "Also notifying parent session of approval resolution"
        );

        // Write signal to parent session too
        let parent_signal = super::signal::Signal::ApprovalResolved {
            request_id: decision.request_id.clone(),
            agent_id: agent_id.clone(),
            status: status_str.to_string(),
            install_completed: false,
            message: status_message.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        // write_signal persists the record to GatewayStore for durable scheduler delivery.
        if let Err(e) = super::signal::write_signal(
            gateway_store.as_deref(),
            parent_session_id,
            &decision.request_id,
            &parent_signal,
        ) {
            tracing::warn!(
                target: "approval",
                request_id = %decision.request_id,
                parent_session = %parent_session_id,
                error = %e,
                "Failed to write approval signal to parent session store"
            );
        }
    }

    // Delivery ownership is gateway-side and durable:
    // this function only persists signals. Gateway pollers and channel-specific
    // consumers (such as the chat TUI on its own socket) perform delivery + ack.
    let target_session = if notify_parent {
        format!("{},{}", session_id, parent_session_id)
    } else {
        session_id.to_string()
    };
    tracing::info!(
        target: "approval",
        request_id = %decision.request_id,
        target_session = %target_session,
        "Approval notification queued for gateway-owned delivery"
    );

    Ok(())
}

/// Determines whether the parent (root) session should be notified of an
/// approval resolution. Uses the task graph (`root_session_id`) rather than
/// string-parsing the session ID.
fn should_notify_parent_session(decision: &ApprovalDecision) -> bool {
    // If the decision has a root_session_id that differs from the session_id,
    // this is a child session and the root should be notified.
    match &decision.root_session_id {
        Some(root) if root != &decision.session_id => true,
        _ => false,
    }
}

fn should_resume_waiting_session(decision: &ApprovalDecision) -> bool {
    !(decision.workflow_id.is_some() && decision.task_id.is_some())
}

/// On approval resolution, update the blocked task's status and emit workflow events.
fn unblock_task_on_approval(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    decision: &ApprovalDecision,
) {
    let (Some(wf_id), Some(t_id)) = (&decision.workflow_id, &decision.task_id) else {
        return;
    };
    let (new_status, approval_event_type) = match decision.status {
        ApprovalStatus::Approved => (
            autonoetic_types::workflow::TaskRunStatus::Runnable,
            "task.approved",
        ),
        ApprovalStatus::Rejected => (
            autonoetic_types::workflow::TaskRunStatus::Failed,
            "task.rejected",
        ),
    };

    // Emit the approval decision event before updating status so chat CLI sees it.
    let _ = super::workflow_store::append_workflow_event(
        config,
        gateway_store,
        &autonoetic_types::workflow::WorkflowEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            workflow_id: wf_id.to_string(),
            task_id: Some(t_id.to_string()),
            event_type: approval_event_type.to_string(),
            agent_id: Some(decision.agent_id.clone()),
            payload: serde_json::json!({
                "request_id": decision.request_id,
                "status": match decision.status {
                    ApprovalStatus::Approved => "approved",
                    ApprovalStatus::Rejected => "rejected",
                },
            }),
            occurred_at: decision.decided_at.clone(),
        },
    );

    if let Err(e) = super::workflow_store::update_task_run_status(
        config,
        gateway_store,
        wf_id,
        t_id,
        new_status,
        None,
    ) {
        tracing::warn!(
            target: "approval",
            workflow_id = %wf_id,
            task_id = %t_id,
            error = %e,
            "Failed to unblock task on approval resolution"
        );
        return;
    }

    tracing::info!(
        target: "approval",
        workflow_id = %wf_id,
        task_id = %t_id,
        status = ?decision.status,
        "Task unblocked after approval resolution"
    );

    // Clear BlockedApproval if no tasks remain in AwaitingApproval.
    if let Ok(tasks) =
        super::workflow_store::list_task_runs_for_workflow(config, gateway_store, wf_id)
    {
        let any_awaiting = tasks
            .iter()
            .any(|t| t.status == autonoetic_types::workflow::TaskRunStatus::AwaitingApproval);
        if !any_awaiting {
            if let Ok(Some(mut wf)) =
                super::workflow_store::load_workflow_run(config, gateway_store, wf_id)
            {
                if wf.status == autonoetic_types::workflow::WorkflowRunStatus::BlockedApproval {
                    wf.status = autonoetic_types::workflow::WorkflowRunStatus::WaitingChildren;
                    wf.updated_at = chrono::Utc::now().to_rfc3339();
                    if let Err(e) =
                        super::workflow_store::save_workflow_run(config, gateway_store, &wf)
                    {
                        tracing::warn!(
                            target: "approval",
                            workflow_id = %wf_id,
                            error = %e,
                            "Failed to clear BlockedApproval status"
                        );
                    }
                }
            }
        }
    }
}

fn decide_request(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    decided_by: &str,
    reason: Option<String>,
    status: ApprovalStatus,
) -> anyhow::Result<ApprovalDecision> {
    let request = if let Some(store) = gateway_store {
        store
            .get_approval(request_id)?
            .ok_or_else(|| anyhow::anyhow!("Approval request not found in store: {}", request_id))?
    } else {
        anyhow::bail!("GatewayStore is required to decide approvals");
    };

    let decision = ApprovalDecision {
        request_id: request.request_id,
        agent_id: request.agent_id,
        session_id: request.session_id,
        action: request.action,
        status: status.clone(),
        decided_at: chrono::Utc::now().to_rfc3339(),
        decided_by: decided_by.to_string(),
        reason,
        root_session_id: request.root_session_id.clone(),
        workflow_id: request.workflow_id.clone(),
        task_id: request.task_id.clone(),
    };
    // Persist decision in GatewayStore
    if let Some(store) = gateway_store {
        if let Err(e) = store.record_decision(
            &decision.request_id,
            match decision.status {
                ApprovalStatus::Approved => "approved",
                ApprovalStatus::Rejected => "rejected",
            },
            &decision.decided_by,
            &decision.decided_at,
        ) {
            tracing::warn!(
                target: "approval",
                request_id = %decision.request_id,
                error = %e,
                "Failed to record decision in store"
            );
        }
    }

    let background_session_id = super::decision::background_session_id;
    let load_background_state = super::store::load_background_state;
    let save_background_state = super::store::save_background_state;

    if matches!(status, ApprovalStatus::Rejected) {
        let agent_dir = config.agents_dir.join(&decision.agent_id);
        crate::runtime::reevaluation_state::persist_reevaluation_state(&agent_dir, |state| {
            state
                .open_approval_request_ids
                .retain(|existing| existing != &decision.request_id);
            state.pending_scheduled_action = None;
            state.last_outcome = Some("approval_rejected".to_string());
        })?;
        let state_path = super::store::background_state_path(config, &decision.agent_id);
        let mut background_state = load_background_state(
            &state_path,
            &decision.agent_id,
            &background_session_id(&decision.agent_id),
        )?;
        background_state.approval_blocked = false;
        background_state
            .pending_approval_request_ids
            .retain(|existing| existing != &decision.request_id);
        background_state
            .processed_approval_request_ids
            .push(decision.request_id.clone());
        save_background_state(&state_path, &background_state)?;
    }

    let causal_logger = init_gateway_causal_logger(config)?;
    let mut trace_session = TraceSession::create_with_session_id(
        SessionId::from_string(decision.session_id.clone()),
        Arc::new(causal_logger),
        gateway_actor_id(),
        EventScope::Session,
    );
    let action = match status {
        ApprovalStatus::Approved => "background.approval",
        ApprovalStatus::Rejected => "background.approval",
    };
    let status_str = match status {
        ApprovalStatus::Approved => "approved",
        ApprovalStatus::Rejected => "rejected",
    };
    let _ = trace_session.log_completed(
        action,
        Some(status_str),
        Some(serde_json::json!({
            "agent_id": decision.agent_id,
            "request_id": decision.request_id,
            "decided_by": decision.decided_by,
            "action_kind": decision.action.kind()
        })),
    );
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::{should_notify_parent_session, should_resume_waiting_session};
    use crate::scheduler::workflow_store::{ensure_workflow_for_root_session, save_task_run};
    use autonoetic_types::background::{
        ApprovalDecision, ApprovalRequest, ApprovalStatus, ScheduledAction,
    };
    use autonoetic_types::config::GatewayConfig;
    use autonoetic_types::workflow::{TaskRun, TaskRunStatus};
    use tempfile::tempdir;

    #[test]
    fn load_approval_requests_skips_payload_companion_files() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let req = ApprovalRequest {
            request_id: "apr-test1234".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root-session/coder-abc".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 x".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            created_at: "2020-01-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
        };
        store.create_approval(&req).unwrap();

        let loaded = super::load_approval_requests(&cfg, Some(&store)).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].request_id, "apr-test1234");
    }

    #[test]
    fn pending_approval_requests_for_root_filters_by_session() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let req = |id: &str, sess: &str| ApprovalRequest {
            request_id: id.to_string(),
            agent_id: "a".to_string(),
            session_id: sess.to_string(),
            action: ScheduledAction::SandboxExec {
                command: "c".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            created_at: "2020-01-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
        };
        store
            .create_approval(&req("apr-a", "root-a/coder-1"))
            .unwrap();
        store
            .create_approval(&req("apr-b", "root-b/coder-1"))
            .unwrap();

        let for_a =
            super::pending_approval_requests_for_root(&cfg, Some(&store), "root-a").unwrap();
        assert_eq!(for_a.len(), 1);
        assert_eq!(for_a[0].request_id, "apr-a");
    }

    #[test]
    fn pending_sandbox_exec_requests_for_session_filters_and_sorts() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let req = |id: &str, created: &str| ApprovalRequest {
            request_id: id.to_string(),
            agent_id: "evaluator.default".to_string(),
            session_id: "sess/evaluator-1".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 x".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            created_at: created.to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
        };
        store
            .create_approval(&req("apr-second", "2020-01-02T00:00:00Z"))
            .unwrap();
        store
            .create_approval(&req("apr-first", "2020-01-01T00:00:00Z"))
            .unwrap();
        // Install-style request same session — must not appear in sandbox-only list
        let install = ApprovalRequest {
            request_id: "apr-install".to_string(),
            agent_id: "b".to_string(),
            session_id: "sess/evaluator-1".to_string(),
            action: ScheduledAction::AgentInstall {
                agent_id: "x".to_string(),
                summary: "s".to_string(),
                requested_by_agent_id: "y".to_string(),
                install_fingerprint: "fp".to_string(),
                payload: None,
            },
            created_at: "2019-01-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
        };
        store.create_approval(&install).unwrap();

        let list = super::pending_sandbox_exec_requests_for_session(
            &cfg,
            Some(&store),
            "sess/evaluator-1",
        )
        .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].request_id, "apr-first");
        assert_eq!(list[1].request_id, "apr-second");
    }

    #[test]
    fn test_should_notify_parent_session_when_root_differs_from_session() {
        let decision = ApprovalDecision {
            request_id: "apr-1".to_string(),
            agent_id: "specialized_builder.default".to_string(),
            session_id: "demo-session/specialized_builder.default-abcd1234".to_string(),
            action: ScheduledAction::AgentInstall {
                agent_id: "specialist.weather".to_string(),
                summary: "install specialist.weather".to_string(),
                requested_by_agent_id: "specialized_builder.default".to_string(),
                install_fingerprint: "sha256:abc123".to_string(),
                payload: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: Some("demo-session".to_string()),
        };
        assert!(should_notify_parent_session(&decision));
    }

    #[test]
    fn test_should_notify_parent_session_for_sandbox_exec_in_child() {
        let decision = ApprovalDecision {
            request_id: "apr-2".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: Some("demo-session".to_string()),
        };
        assert!(should_notify_parent_session(&decision));
    }

    #[test]
    fn test_should_not_notify_parent_session_when_root_is_same_as_session() {
        let decision = ApprovalDecision {
            request_id: "apr-3".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: Some("demo-session".to_string()),
        };
        assert!(!should_notify_parent_session(&decision));
    }

    #[test]
    fn test_should_not_notify_parent_session_when_no_root() {
        let decision = ApprovalDecision {
            request_id: "apr-4".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
        };
        assert!(!should_notify_parent_session(&decision));
    }

    #[test]
    fn test_should_not_resume_waiting_session_for_workflow_bound_approval() {
        let decision = ApprovalDecision {
            request_id: "apr-workflow1".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: Some("wf-demo".to_string()),
            task_id: Some("task-demo".to_string()),
            root_session_id: Some("demo-session".to_string()),
        };

        assert!(!should_resume_waiting_session(&decision));
    }

    #[test]
    fn test_should_resume_waiting_session_for_non_workflow_approval() {
        let decision = ApprovalDecision {
            request_id: "apr-direct1".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
        };

        assert!(should_resume_waiting_session(&decision));
    }

    #[test]
    fn workflow_bound_approval_skips_direct_session_notification() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        let agent_dir = agents_dir.join("coder.default");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf =
            ensure_workflow_for_root_session(&cfg, Some(&store), "demo-session", None).unwrap();

        let task = TaskRun {
            task_id: "task-approval".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            parent_session_id: "demo-session".to_string(),
            status: TaskRunStatus::AwaitingApproval,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: Some("Continue after approval".to_string()),
            metadata: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        let request = ApprovalRequest {
            request_id: "apr-write123".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: task.session_id.clone(),
            action: ScheduledAction::WriteFile {
                path: "approved.txt".to_string(),
                content: "approved".to_string(),
                requires_approval: true,
                evidence_ref: None,
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            reason: None,
            evidence_ref: None,
            workflow_id: Some(wf.workflow_id.clone()),
            task_id: Some(task.task_id.clone()),
            root_session_id: Some("demo-session".to_string()),
            status: None,
            decided_at: None,
            decided_by: None,
        };
        store.create_approval(&request).unwrap();

        super::approve_request(&cfg, Some(&store), &request.request_id, "operator", None).unwrap();

        let pending = store.list_pending_notifications().unwrap();
        assert!(
            pending.is_empty(),
            "workflow-bound approvals should continue through workflow re-queue only"
        );
    }

    #[test]
    fn sandbox_approval_signal_prompts_agent_retry() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir,
            ..Default::default()
        };
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );

        let decision = ApprovalDecision {
            request_id: "apr-out1234".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
        };

        // SandboxExec is no longer auto-executed; agent retries with approval_ref.
        super::resume_session_after_approval(&cfg, Some(store.as_ref()), &decision).unwrap();

        let pending = store.list_pending_notifications().unwrap();
        assert!(!pending.is_empty(), "should have created a notification");
    }
}
