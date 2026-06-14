use anyhow::Result;
use rusqlite::{params, Connection};

use autonoetic_types::capability::Capability;
use autonoetic_types::plan_frame::{
    PlanFrame, PlanStatus, PlanStep, StepOwner, ValidationClass, ValidationEntry,
    ValidationPolicy, ValidationRequirement,
};

const SELECT_COLS: &str = "\
    plan_id, version, parent_version, workflow_id, root_session_id, \
    title, objective, status, steps_json, validation_policy_json, \
    capability_envelope_json, approved_by, approved_at, created_by_agent_id, reason, created_at";

pub(crate) fn save_plan_frame(conn: &Connection, plan: &PlanFrame) -> Result<()> {
    let steps_json = serde_json::to_string(&plan.steps)?;
    let validation_policy_json = serde_json::to_string(&plan.validation_policy)?;
    let capability_envelope_json = serde_json::to_string(&plan.capability_envelope)?;

    conn.execute(
        "INSERT INTO plan_frames
            (plan_id, version, parent_version, workflow_id, root_session_id,
             title, objective, status, steps_json, validation_policy_json,
             capability_envelope_json, approved_by, approved_at, created_by_agent_id, reason, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            plan.plan_id,
            plan.version as i64,
            plan.parent_version.map(|v| v as i64),
            plan.workflow_id,
            plan.root_session_id,
            plan.title,
            plan.objective,
            plan.status.as_str(),
            steps_json,
            validation_policy_json,
            capability_envelope_json,
            plan.approved_by,
            plan.approved_at,
            plan.created_by_agent_id,
            plan.reason,
            plan.created_at,
        ],
    )?;
    Ok(())
}

pub(crate) fn update_plan_frame_status(
    conn: &Connection,
    plan_id: &str,
    version: u32,
    status: PlanStatus,
    approved_by: Option<&str>,
    approved_at: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE plan_frames SET status = ?1, approved_by = ?2, approved_at = ?3
         WHERE plan_id = ?4 AND version = ?5",
        params![
            status.as_str(),
            approved_by,
            approved_at,
            plan_id,
            version as i64,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_plan_frame(conn: &Connection, plan_id: &str) -> Result<Option<PlanFrame>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM plan_frames WHERE plan_id = ?1 ORDER BY version DESC LIMIT 1"
    ))?;

    let result = stmt.query_row(params![plan_id], |row| {
        Ok(row_to_plan_frame(row))
    });

    match result {
        Ok(plan) => Ok(Some(plan?)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn load_plan_frame_revision(
    conn: &Connection,
    plan_id: &str,
    version: u32,
) -> Result<Option<PlanFrame>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM plan_frames WHERE plan_id = ?1 AND version = ?2"
    ))?;

    let result = stmt.query_row(params![plan_id, version as i64], |row| {
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
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM plan_frames
         WHERE workflow_id = ?1 AND status IN ('awaiting_approval', 'approved')
         ORDER BY version DESC LIMIT 1"
    ))?;

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
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM plan_frames
         WHERE workflow_id = ?1
           AND version = (SELECT MAX(p2.version) FROM plan_frames p2 WHERE p2.plan_id = plan_frames.plan_id)
         ORDER BY created_at DESC"
    ))?;

    let rows = stmt.query_map(params![workflow_id], |row| {
        Ok(row_to_plan_frame(row))
    })?;

    let mut plans = Vec::new();
    for row in rows {
        plans.push(row??);
    }
    Ok(plans)
}

/// Latest revision of each plan for `root_session_id` that is still awaiting operator approval.
pub(crate) fn list_pending_plan_frames_for_root(
    conn: &Connection,
    root_session_id: &str,
) -> Result<Vec<PlanFrame>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM plan_frames p
         WHERE p.root_session_id = ?1 AND p.status = 'awaiting_approval'
           AND p.version = (
             SELECT MAX(p2.version) FROM plan_frames p2 WHERE p2.plan_id = p.plan_id
           )
         ORDER BY p.created_at ASC"
    ))?;

    let rows = stmt.query_map(params![root_session_id], |row| {
        Ok(row_to_plan_frame(row))
    })?;

    let mut plans = Vec::new();
    for row in rows {
        plans.push(row??);
    }
    Ok(plans)
}

pub(crate) fn list_plan_revisions(
    conn: &Connection,
    plan_id: &str,
) -> Result<Vec<PlanFrame>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM plan_frames WHERE plan_id = ?1 ORDER BY version ASC"
    ))?;

    let rows = stmt.query_map(params![plan_id], |row| {
        Ok(row_to_plan_frame(row))
    })?;

    let mut revisions = Vec::new();
    for row in rows {
        revisions.push(row??);
    }
    Ok(revisions)
}

fn row_to_plan_frame(row: &rusqlite::Row<'_>) -> Result<PlanFrame, rusqlite::Error> {
    let parent_version: Option<i64> = row.get(2)?;
    let status_str: String = row.get(7)?;
    let steps_json: String = row.get(8)?;
    let vp_json: String = row.get(9)?;
    let capability_envelope_json: String = row.get(10)?;

    let steps: Vec<PlanStep> =
        serde_json::from_str(&steps_json).unwrap_or_default();
    let validation_policy: ValidationPolicy =
        serde_json::from_str(&vp_json).unwrap_or_default();
    let capability_envelope: Vec<Capability> =
        serde_json::from_str(&capability_envelope_json).unwrap_or_default();

    Ok(PlanFrame {
        plan_id: row.get(0)?,
        version: row.get::<_, i64>(1)? as u32,
        parent_version: parent_version.map(|v| v as u32),
        workflow_id: row.get(3)?,
        root_session_id: row.get(4)?,
        title: row.get(5)?,
        objective: row.get(6)?,
        status: parse_plan_status(&status_str),
        steps,
        validation_policy,
        capability_envelope,
        approved_by: row.get(11)?,
        approved_at: row.get(12)?,
        created_by_agent_id: row.get(13)?,
        reason: row.get(14)?,
        created_at: row.get(15)?,
    })
}

fn parse_plan_status(s: &str) -> PlanStatus {
    match s {
        "awaiting_approval" => PlanStatus::AwaitingApproval,
        "approved" => PlanStatus::Approved,
        "completed" => PlanStatus::Completed,
        "cancelled" => PlanStatus::Cancelled,
        _ => PlanStatus::AwaitingApproval,
    }
}
