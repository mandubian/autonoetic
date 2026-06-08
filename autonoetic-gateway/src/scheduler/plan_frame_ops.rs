//! Operator-facing PlanFrame approval (chat TUI, CLI) without routing through an agent tool.

use anyhow::{anyhow, Result};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::{PlanFrame, PlanStatus};
use autonoetic_types::workflow::WorkflowEventRecord;
use chrono::Utc;

use crate::scheduler::gateway_store::GatewayStore;
use crate::scheduler::workflow_store;

pub fn pending_plan_frames_for_root(
    store: &GatewayStore,
    root_session_id: &str,
) -> Result<Vec<PlanFrame>> {
    store.list_pending_plan_frames_for_root(root_session_id)
}

/// Approve the latest revision of a plan as the operator (not via `planframe_approve` agent tool).
pub fn approve_plan_frame_operator(
    config: &GatewayConfig,
    store: &GatewayStore,
    plan_id: &str,
    approved_by: &str,
) -> Result<PlanFrame> {
    let Some(plan) = store.load_plan_frame(plan_id)? else {
        return Err(anyhow!("Plan not found: {plan_id}"));
    };

    if plan.status != PlanStatus::AwaitingApproval {
        return Err(anyhow!(
            "Plan {plan_id} is '{}'; only awaiting_approval plans can be approved",
            plan.status.as_str()
        ));
    }

    let now = Utc::now().to_rfc3339();
    store.update_plan_frame_status(
        &plan.plan_id,
        plan.version,
        PlanStatus::Approved,
        Some(approved_by),
        Some(&now),
    )?;

    // Canonical timeline: close the plan gate (same event as `planframe_approve`).
    {
        use autonoetic_types::session_timeline::TimelineRefs;
        let (principal, role) = crate::runtime::session_timeline::decider_seat(approved_by);
        let refs = TimelineRefs {
            plan_id: Some(plan.plan_id.clone()),
            ..Default::default()
        };
        let event = crate::runtime::session_timeline::build_timeline_event(
            plan.root_session_id.clone(),
            plan.root_session_id.clone(),
            None,
            &principal,
            &role,
            "plan.approved",
            None,
            Some(serde_json::json!({
                "plan_id": plan.plan_id,
                "version": plan.version,
                "approved_by": approved_by,
            })),
            refs,
        );
        if let Err(e) = store.create_live_digest_event(&event) {
            tracing::debug!(
                target: "session_timeline",
                error = %e,
                "plan.approved timeline emit failed (operator approve)"
            );
        }
    }

    let event_id = {
        let bytes = uuid::Uuid::new_v4();
        format!("evt-{}", hex::encode(&bytes.as_bytes()[..8]))
    };
    workflow_store::append_workflow_event(
        config,
        Some(store),
        &WorkflowEventRecord {
            event_id,
            workflow_id: plan.workflow_id.clone(),
            task_id: None,
            event_type: "planframe.approved".to_string(),
            agent_id: None,
            payload: serde_json::json!({
                "plan_id": plan.plan_id,
                "version": plan.version,
                "approved_by": approved_by,
            }),
            occurred_at: now,
        },
    )?;

    store
        .load_plan_frame(plan_id)?
        .ok_or_else(|| anyhow!("Plan disappeared after approval: {plan_id}"))
}
