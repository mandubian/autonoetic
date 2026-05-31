use anyhow::Result;
use rusqlite::{params, Connection};

use autonoetic_types::workbench::{
    WorkbenchCheckpoint, WorkbenchProjection, WorkbenchStatus,
};

pub(crate) fn save_workbench(conn: &Connection, wb: &WorkbenchProjection) -> Result<()> {
    conn.execute(
        "INSERT INTO workbenches
            (workbench_id, workflow_id, root_session_id, plan_id,
             base_artifact_id, base_artifact_canonical_digest, workspace_path,
             status, created_by_agent_id, created_at,
             last_checkpoint_at, reconciled_at, discarded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            wb.workbench_id,
            wb.workflow_id,
            wb.root_session_id,
            wb.plan_id,
            wb.base_artifact_id,
            wb.base_artifact_canonical_digest,
            wb.workspace_path,
            wb.status.as_str(),
            wb.created_by_agent_id,
            wb.created_at,
            wb.last_checkpoint_at,
            wb.reconciled_at,
            wb.discarded_at,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_workbench(conn: &Connection, workbench_id: &str) -> Result<Option<WorkbenchProjection>> {
    let mut stmt = conn.prepare(
        "SELECT workbench_id, workflow_id, root_session_id, plan_id,
                base_artifact_id, base_artifact_canonical_digest, workspace_path,
                status, created_by_agent_id, created_at,
                last_checkpoint_at, reconciled_at, discarded_at
         FROM workbenches WHERE workbench_id = ?1",
    )?;

    let result = stmt.query_row(params![workbench_id], |row| Ok(row_to_workbench(row)));

    match result {
        Ok(wb) => Ok(Some(wb?)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn load_active_workbench_for_workflow(
    conn: &Connection,
    workflow_id: &str,
) -> Result<Option<WorkbenchProjection>> {
    let mut stmt = conn.prepare(
        "SELECT workbench_id, workflow_id, root_session_id, plan_id,
                base_artifact_id, base_artifact_canonical_digest, workspace_path,
                status, created_by_agent_id, created_at,
                last_checkpoint_at, reconciled_at, discarded_at
         FROM workbenches
         WHERE workflow_id = ?1 AND status = 'active'
         ORDER BY created_at DESC LIMIT 1",
    )?;

    let result = stmt.query_row(params![workflow_id], |row| Ok(row_to_workbench(row)));

    match result {
        Ok(wb) => Ok(Some(wb?)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn list_workbenches_for_workflow(
    conn: &Connection,
    workflow_id: &str,
) -> Result<Vec<WorkbenchProjection>> {
    let mut stmt = conn.prepare(
        "SELECT workbench_id, workflow_id, root_session_id, plan_id,
                base_artifact_id, base_artifact_canonical_digest, workspace_path,
                status, created_by_agent_id, created_at,
                last_checkpoint_at, reconciled_at, discarded_at
         FROM workbenches WHERE workflow_id = ?1
         ORDER BY created_at DESC",
    )?;

    let rows = stmt.query_map(params![workflow_id], |row| Ok(row_to_workbench(row)))?;

    let mut workbenches = Vec::new();
    for row in rows {
        workbenches.push(row??);
    }
    Ok(workbenches)
}

pub(crate) fn update_workbench_status(
    conn: &Connection,
    workbench_id: &str,
    status: WorkbenchStatus,
    timestamp: &str,
) -> Result<()> {
    let ts_col = match status {
        WorkbenchStatus::Reconciled => "reconciled_at",
        WorkbenchStatus::Discarded => "discarded_at",
        WorkbenchStatus::Active => "last_checkpoint_at",
    };
    conn.execute(
        &format!(
            "UPDATE workbenches SET status = ?1, {ts_col} = ?2 WHERE workbench_id = ?3"
        ),
        params![status.as_str(), timestamp, workbench_id],
    )?;
    Ok(())
}

pub(crate) fn update_workbench_last_checkpoint(
    conn: &Connection,
    workbench_id: &str,
    timestamp: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE workbenches SET last_checkpoint_at = ?1 WHERE workbench_id = ?2",
        params![timestamp, workbench_id],
    )?;
    Ok(())
}

pub(crate) fn save_checkpoint(conn: &Connection, cp: &WorkbenchCheckpoint) -> Result<()> {
    conn.execute(
        "INSERT INTO workbench_checkpoints
            (checkpoint_id, workbench_id, label, file_count, total_bytes, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            cp.checkpoint_id,
            cp.workbench_id,
            cp.label,
            cp.file_count as i64,
            cp.total_bytes as i64,
            cp.created_at,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_checkpoint(
    conn: &Connection,
    checkpoint_id: &str,
) -> Result<Option<WorkbenchCheckpoint>> {
    let mut stmt = conn.prepare(
        "SELECT checkpoint_id, workbench_id, label, file_count, total_bytes, created_at
         FROM workbench_checkpoints WHERE checkpoint_id = ?1",
    )?;

    let result = stmt.query_row(params![checkpoint_id], |row| {
        Ok(WorkbenchCheckpoint {
            checkpoint_id: row.get(0)?,
            workbench_id: row.get(1)?,
            label: row.get(2)?,
            file_count: row.get::<_, i64>(3)? as usize,
            total_bytes: row.get::<_, i64>(4)? as u64,
            created_at: row.get(5)?,
        })
    });

    match result {
        Ok(cp) => Ok(Some(cp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn list_checkpoints_for_workbench(
    conn: &Connection,
    workbench_id: &str,
) -> Result<Vec<WorkbenchCheckpoint>> {
    let mut stmt = conn.prepare(
        "SELECT checkpoint_id, workbench_id, label, file_count, total_bytes, created_at
         FROM workbench_checkpoints WHERE workbench_id = ?1
         ORDER BY created_at DESC",
    )?;

    let rows = stmt.query_map(params![workbench_id], |row| {
        Ok(WorkbenchCheckpoint {
            checkpoint_id: row.get(0)?,
            workbench_id: row.get(1)?,
            label: row.get(2)?,
            file_count: row.get::<_, i64>(3)? as usize,
            total_bytes: row.get::<_, i64>(4)? as u64,
            created_at: row.get(5)?,
        })
    })?;

    let mut checkpoints = Vec::new();
    for row in rows {
        checkpoints.push(row?);
    }
    Ok(checkpoints)
}

fn row_to_workbench(row: &rusqlite::Row<'_>) -> Result<WorkbenchProjection, rusqlite::Error> {
    let status_str: String = row.get(7)?;
    Ok(WorkbenchProjection {
        workbench_id: row.get(0)?,
        workflow_id: row.get(1)?,
        root_session_id: row.get(2)?,
        plan_id: row.get(3)?,
        base_artifact_id: row.get(4)?,
        base_artifact_canonical_digest: row.get(5)?,
        workspace_path: row.get(6)?,
        status: parse_workbench_status(&status_str),
        created_by_agent_id: row.get(8)?,
        created_at: row.get(9)?,
        last_checkpoint_at: row.get(10)?,
        reconciled_at: row.get(11)?,
        discarded_at: row.get(12)?,
    })
}

fn parse_workbench_status(s: &str) -> WorkbenchStatus {
    match s {
        "reconciled" => WorkbenchStatus::Reconciled,
        "discarded" => WorkbenchStatus::Discarded,
        _ => WorkbenchStatus::Active,
    }
}
