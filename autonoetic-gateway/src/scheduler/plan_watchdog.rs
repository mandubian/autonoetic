//! Plan watchdog — detects the "stuck plan" deadlock at turn end.
//!
//! The failure shape (observed live, session-58342961): an agent with an
//! approved plan frame ends its turn believing a child task is still running,
//! but the child already went terminal. With no non-terminal children and no
//! pending operator decisions, the turn ends as `Completed` and **no future
//! event will ever wake the session** — the plan stalls with steps incomplete.
//!
//! The turn-end loop (`lifecycle.rs` EndTurn arm) consults
//! [`plan_incomplete_nudge`] as a sibling predicate to
//! `waiting_for_child_yield_reason`: when it fires, the agent is nudged with
//! a synthetic user message and the turn continues instead of ending, bounded
//! by `plan_incomplete_nudge_budget` per resume.
//!
//! The predicate is deliberately the conjunction of three conditions — a
//! nudge is only correct when *all* hold:
//!
//! 1. an **approved** plan has actionable (non-operator-owned) steps still
//!    `Pending`/`InProgress` (`AwaitingApproval` means the operator owes a
//!    decision; waiting is correct);
//! 2. **no non-terminal child tasks** for this session (otherwise the
//!    child-wait machinery owns the wake);
//! 3. **no pending operator decisions** for the root session (approvals,
//!    interactions, escalations, pending plans — otherwise the session is
//!    legitimately waiting on the human).

use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::{PlanStatus, StepOwner, StepStatus};

use crate::scheduler::gateway_store::GatewayStore;

/// A plan that would deadlock if the agent ends its turn now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanIncompleteNudge {
    pub plan_id: String,
    pub plan_version: u32,
    pub workflow_id: String,
    /// `(step_id, title)` of actionable incomplete steps, in plan order.
    pub incomplete_steps: Vec<(String, String)>,
}

/// Evaluate the stuck-plan predicate for `session_id` at turn end.
///
/// Returns `Some` only when ending the turn would strand an approved plan
/// with actionable steps and no outstanding work or decisions. Any lookup
/// failure fails closed (`None`) — a missed nudge is a stall the operator
/// can still kick; a false nudge is a turn that should have ended.
pub(crate) fn plan_incomplete_nudge(
    config: &GatewayConfig,
    store: &GatewayStore,
    session_id: &str,
) -> Option<PlanIncompleteNudge> {
    let root_session_id = crate::runtime::content_store::root_session_id(session_id);
    let workflow_id =
        crate::scheduler::resolve_workflow_id_for_root_session(config, &root_session_id).ok()??;

    let plan = store.load_active_plan_for_workflow(&workflow_id).ok()??;
    if plan.status != PlanStatus::Approved {
        return None;
    }

    let incomplete_steps: Vec<(String, String)> = plan
        .steps
        .iter()
        .filter(|s| matches!(s.status, StepStatus::Pending | StepStatus::InProgress))
        .filter(|s| s.owner != StepOwner::Operator)
        .map(|s| (s.step_id.clone(), s.title.clone()))
        .collect();
    if incomplete_steps.is_empty() {
        return None;
    }

    let has_children = crate::scheduler::workflow_store::session_has_non_terminal_children(
        config,
        Some(store),
        &workflow_id,
        session_id,
    )
    .ok()?;
    if has_children {
        return None;
    }

    let pending = crate::runtime::operator_pending::collect_pending_for_root(
        store,
        &root_session_id,
        chrono::Utc::now(),
    )
    .ok()?;
    if !pending.is_empty() {
        return None;
    }

    Some(PlanIncompleteNudge {
        plan_id: plan.plan_id,
        plan_version: plan.version,
        workflow_id,
        incomplete_steps,
    })
}

/// Render the synthetic nudge message injected into history when the
/// predicate fires. Style mirrors the child-wait wake message in
/// `scheduler::process_runnable_workflow_tasks`.
pub(crate) fn render_nudge_message(nudge: &PlanIncompleteNudge, attempt: u32, budget: u32) -> String {
    let steps = nudge
        .incomplete_steps
        .iter()
        .map(|(id, title)| format!("{id} ({title})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[gateway plan watchdog] Approved plan {} v{} still has incomplete steps: {}. \
         No child tasks are running and nothing awaits the operator — ending your turn now \
         deadlocks the plan: no future event will wake you. Re-check state with workflow_state, \
         then take the next action NOW with tool calls (spawn the next step's owner, or record \
         progress with planframe_amend). If the plan is genuinely blocked, say so explicitly \
         (escalate or ask the operator) instead of ending your turn. Nudge {attempt}/{budget}.",
        nudge.plan_id, nudge.plan_version, steps
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::plan_frame::{PlanFrame, PlanStep, StepOwner, StepStatus, ValidationPolicy};
    use autonoetic_types::workflow::{TaskRun, TaskRunStatus};
    use std::path::Path;
    use tempfile::tempdir;

    fn test_config(agents_dir: &Path) -> GatewayConfig {
        GatewayConfig {
            runtime_dir: agents_dir.to_path_buf().join(".gateway"),
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        }
    }

    fn step(step_id: &str, owner: StepOwner, status: StepStatus) -> PlanStep {
        PlanStep {
            step_id: step_id.to_string(),
            title: format!("title-{step_id}"),
            owner,
            depends_on: vec![],
            agent_id: Some("coder.default".to_string()),
            notes: None,
            status,
            required_capabilities: vec![],
        }
    }

    fn plan(workflow_id: &str, root: &str, status: PlanStatus, steps: Vec<PlanStep>) -> PlanFrame {
        PlanFrame {
            plan_id: "plan-test".to_string(),
            version: 1,
            parent_version: None,
            workflow_id: workflow_id.to_string(),
            root_session_id: root.to_string(),
            title: "t".to_string(),
            objective: "o".to_string(),
            status,
            steps,
            validation_policy: ValidationPolicy::default(),
            capability_envelope: vec![],
            approved_by: None,
            approved_at: None,
            created_by_agent_id: "planner.collaborative".to_string(),
            reason: None,
            created_at: "2026-09-10T00:00:00Z".to_string(),
            expires_at: None,
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        cfg: GatewayConfig,
        store: GatewayStore,
        workflow_id: String,
        root: String,
    }

    fn fixture() -> Fixture {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = GatewayStore::open(&gateway_dir).unwrap();
        let root = "root-session".to_string();
        let wf = crate::scheduler::workflow_store::ensure_workflow_for_root_session(
            &cfg,
            Some(&store),
            &root,
            None,
        )
        .unwrap();
        Fixture {
            _dir: dir,
            cfg,
            store,
            workflow_id: wf.workflow_id,
            root,
        }
    }

    #[test]
    fn no_plan_no_nudge() {
        let f = fixture();
        assert!(plan_incomplete_nudge(&f.cfg, &f.store, &f.root).is_none());
    }

    #[test]
    fn awaiting_approval_plan_no_nudge() {
        let f = fixture();
        f.store
            .save_plan_frame(&plan(
                &f.workflow_id,
                &f.root,
                PlanStatus::AwaitingApproval,
                vec![step("s1", StepOwner::Agent, StepStatus::Pending)],
            ))
            .unwrap();
        // Awaiting operator decision — waiting is the correct state.
        assert!(plan_incomplete_nudge(&f.cfg, &f.store, &f.root).is_none());
    }

    #[test]
    fn approved_plan_all_completed_no_nudge() {
        let f = fixture();
        f.store
            .save_plan_frame(&plan(
                &f.workflow_id,
                &f.root,
                PlanStatus::Approved,
                vec![step("s1", StepOwner::Agent, StepStatus::Completed)],
            ))
            .unwrap();
        assert!(plan_incomplete_nudge(&f.cfg, &f.store, &f.root).is_none());
    }

    #[test]
    fn approved_plan_pending_agent_step_nudges() {
        let f = fixture();
        f.store
            .save_plan_frame(&plan(
                &f.workflow_id,
                &f.root,
                PlanStatus::Approved,
                vec![
                    step("s1", StepOwner::Agent, StepStatus::Completed),
                    step("s2", StepOwner::Agent, StepStatus::Pending),
                ],
            ))
            .unwrap();
        let nudge = plan_incomplete_nudge(&f.cfg, &f.store, &f.root)
            .expect("approved plan with pending agent step and nothing outstanding must nudge");
        assert_eq!(nudge.plan_id, "plan-test");
        assert_eq!(nudge.incomplete_steps.len(), 1);
        assert_eq!(nudge.incomplete_steps[0].0, "s2");
        let msg = render_nudge_message(&nudge, 1, 3);
        assert!(msg.contains("plan-test"));
        assert!(msg.contains("s2"));
        assert!(msg.contains("Nudge 1/3"));
    }

    #[test]
    fn in_progress_step_also_nudges() {
        let f = fixture();
        f.store
            .save_plan_frame(&plan(
                &f.workflow_id,
                &f.root,
                PlanStatus::Approved,
                vec![step("s1", StepOwner::Agent, StepStatus::InProgress)],
            ))
            .unwrap();
        assert!(plan_incomplete_nudge(&f.cfg, &f.store, &f.root).is_some());
    }

    #[test]
    fn operator_owned_pending_step_no_nudge() {
        let f = fixture();
        f.store
            .save_plan_frame(&plan(
                &f.workflow_id,
                &f.root,
                PlanStatus::Approved,
                vec![step("s1", StepOwner::Operator, StepStatus::Pending)],
            ))
            .unwrap();
        assert!(plan_incomplete_nudge(&f.cfg, &f.store, &f.root).is_none());
    }

    #[test]
    fn non_terminal_child_suppresses_nudge() {
        let f = fixture();
        f.store
            .save_plan_frame(&plan(
                &f.workflow_id,
                &f.root,
                PlanStatus::Approved,
                vec![step("s1", StepOwner::Agent, StepStatus::Pending)],
            ))
            .unwrap();
        let now = || chrono::Utc::now().to_rfc3339();
        let child = TaskRun {
            task_id: "task-child".to_string(),
            workflow_id: f.workflow_id.clone(),
            agent_id: "researcher.default".to_string(),
            session_id: format!("{}/researcher.default-x", f.root),
            parent_session_id: f.root.clone(),
            status: TaskRunStatus::Running,
            created_at: now(),
            updated_at: now(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        crate::scheduler::workflow_store::save_task_run(&f.cfg, Some(&f.store), &child).unwrap();
        // Child-wait machinery owns the wake — the watchdog stays silent.
        assert!(plan_incomplete_nudge(&f.cfg, &f.store, &f.root).is_none());
    }
}
