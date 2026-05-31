use anyhow::Result;
use rusqlite::{params, Connection};

use autonoetic_types::plan_frame::{
    PlanFrame, PlanStatus, PlanStep, StepStatus, StepOwner, ValidationClass, ValidationEntry,
    ValidationPolicy, ValidationRequirement,
};

pub(crate) fn save_plan_frame(conn: &Connection, plan: &PlanFrame) -> Result<()> {
    let steps_json = serde_json::to_string(&plan.steps)?;
    let validation_policy_json = serde_json::to_string(&plan.validation_policy)?;

    conn.execute(
        "INSERT OR REPLACE INTO plan_frames
            (plan_id, workflow_id, root_session_id, title, objective, status,
             version, steps_json, validation_policy_json, approved_by, approved_at,
             created_by_agent_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            plan.plan_id,
            plan.workflow_id,
            plan.root_session_id,
            plan.title,
            plan.objective,
            plan.status.as_str(),
            plan.version as i64,
            steps_json,
            validation_policy_json,
            plan.approved_by,
            plan.approved_at,
            plan.created_by_agent_id,
            plan.created_at,
            plan.updated_at,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_plan_frame(conn: &Connection, plan_id: &str) -> Result<Option<PlanFrame>> {
    let mut stmt = conn.prepare(
        "SELECT plan_id, workflow_id, root_session_id, title, objective, status,
                version, steps_json, validation_policy_json, approved_by, approved_at,
                created_by_agent_id, created_at, updated_at
         FROM plan_frames WHERE plan_id = ?1",
    )?;

    let result = stmt.query_row(params![plan_id], |row| {
        Ok(row_to_plan_frame(row))
    });

    match result {
        Ok(plan) => Ok(Some(plan?)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn load_active_plan_for_workflow(
    conn: &Connection,
    workflow_id: &str,
) -> Result<Option<PlanFrame>> {
    let mut stmt = conn.prepare(
        "SELECT plan_id, workflow_id, root_session_id, title, objective, status,
                version, steps_json, validation_policy_json, approved_by, approved_at,
                created_by_agent_id, created_at, updated_at
         FROM plan_frames
         WHERE workflow_id = ?1 AND status IN ('draft', 'awaiting_approval', 'approved')
         ORDER BY updated_at DESC LIMIT 1",
    )?;

    let result = stmt.query_row(params![workflow_id], |row| {
        Ok(row_to_plan_frame(row))
    });

    match result {
        Ok(plan) => Ok(Some(plan?)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn list_plan_frames_for_workflow(
    conn: &Connection,
    workflow_id: &str,
) -> Result<Vec<PlanFrame>> {
    let mut stmt = conn.prepare(
        "SELECT plan_id, workflow_id, root_session_id, title, objective, status,
                version, steps_json, validation_policy_json, approved_by, approved_at,
                created_by_agent_id, created_at, updated_at
         FROM plan_frames WHERE workflow_id = ?1
         ORDER BY updated_at DESC",
    )?;

    let rows = stmt.query_map(params![workflow_id], |row| {
        Ok(row_to_plan_frame(row))
    })?;

    let mut plans = Vec::new();
    for row in rows {
        plans.push(row??);
    }
    Ok(plans)
}

fn row_to_plan_frame(row: &rusqlite::Row<'_>) -> Result<PlanFrame, rusqlite::Error> {
    let status_str: String = row.get(5)?;
    let steps_json: String = row.get(7)?;
    let vp_json: String = row.get(8)?;

    let steps: Vec<PlanStep> =
        serde_json::from_str(&steps_json).unwrap_or_default();
    let validation_policy: ValidationPolicy =
        serde_json::from_str(&vp_json).unwrap_or_default();

    Ok(PlanFrame {
        plan_id: row.get(0)?,
        workflow_id: row.get(1)?,
        root_session_id: row.get(2)?,
        title: row.get(3)?,
        objective: row.get(4)?,
        status: parse_plan_status(&status_str),
        version: row.get::<_, i64>(6)? as u32,
        steps,
        validation_policy,
        approved_by: row.get(9)?,
        approved_at: row.get(10)?,
        created_by_agent_id: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn parse_plan_status(s: &str) -> PlanStatus {
    match s {
        "draft" => PlanStatus::Draft,
        "awaiting_approval" => PlanStatus::AwaitingApproval,
        "approved" => PlanStatus::Approved,
        "superseded" => PlanStatus::Superseded,
        "completed" => PlanStatus::Completed,
        "cancelled" => PlanStatus::Cancelled,
        _ => PlanStatus::Draft,
    }
}
