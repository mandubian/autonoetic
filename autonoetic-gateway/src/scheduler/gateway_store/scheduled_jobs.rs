use anyhow::Result;
use autonoetic_types::scheduled_job::{ScheduledJob, ScheduledJobStatus};
use rusqlite::{params, Connection};

pub fn create_scheduled_job(conn: &Connection, job: &ScheduledJob) -> Result<()> {
    conn.execute(
        "INSERT INTO scheduled_jobs (
            job_id, owner_agent_id, root_session_id, target_agent_id,
            target_revision_id, message, metadata_json, cron_expr, timezone, next_run_at,
            last_run_at, status, created_at, updated_at, last_error, generation
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            job.job_id,
            job.owner_agent_id,
            job.root_session_id,
            job.target_agent_id,
            job.target_revision_id,
            job.message,
            job.metadata_json,
            job.cron_expr,
            job.timezone,
            job.next_run_at,
            job.last_run_at,
            job.status.to_string(),
            job.created_at,
            job.updated_at,
            job.last_error,
            job.generation,
        ],
    )?;
    Ok(())
}

pub fn get_scheduled_job(conn: &Connection, job_id: &str) -> Result<Option<ScheduledJob>> {
    let mut stmt = conn.prepare(
        "SELECT job_id, owner_agent_id, root_session_id, target_agent_id,
                target_revision_id, message, metadata_json, cron_expr, timezone, next_run_at,
                last_run_at, status, created_at, updated_at, last_error, generation
         FROM scheduled_jobs WHERE job_id = ?1",
    )?;
    let mut rows = stmt.query(params![job_id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row_to_job(row)?))
    } else {
        Ok(None)
    }
}

pub fn list_scheduled_jobs_for_owner(
    conn: &Connection,
    owner_agent_id: &str,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Vec<ScheduledJob>> {
    let limit = limit.unwrap_or(100) as i64;
    let offset = offset.unwrap_or(0) as i64;
    let mut stmt = conn.prepare(
        "SELECT job_id, owner_agent_id, root_session_id, target_agent_id,
                target_revision_id, message, metadata_json, cron_expr, timezone, next_run_at,
                last_run_at, status, created_at, updated_at, last_error, generation
         FROM scheduled_jobs
         WHERE owner_agent_id = ?1
         ORDER BY created_at DESC
         LIMIT ?2 OFFSET ?3",
    )?;
    let mut rows = stmt.query(params![owner_agent_id, limit, offset])?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next()? {
        jobs.push(row_to_job(row)?);
    }
    Ok(jobs)
}

pub fn list_scheduled_jobs_for_root(
    conn: &Connection,
    root_session_id: &str,
) -> Result<Vec<ScheduledJob>> {
    let mut stmt = conn.prepare(
        "SELECT job_id, owner_agent_id, root_session_id, target_agent_id,
                target_revision_id, message, metadata_json, cron_expr, timezone, next_run_at,
                last_run_at, status, created_at, updated_at, last_error, generation
         FROM scheduled_jobs
         WHERE root_session_id = ?1
         ORDER BY created_at DESC",
    )?;
    let mut rows = stmt.query(params![root_session_id])?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next()? {
        jobs.push(row_to_job(row)?);
    }
    Ok(jobs)
}

pub fn claim_due_scheduled_job(
    conn: &Connection,
    job_id: &str,
    now_rfc3339: &str,
) -> Result<Option<ScheduledJob>> {
    let updated = conn.execute(
        "UPDATE scheduled_jobs
         SET generation = generation + 1, updated_at = ?1
         WHERE job_id = ?2 AND status = 'active' AND next_run_at <= ?1",
        params![now_rfc3339, job_id],
    )?;
    if updated == 0 {
        return Ok(None);
    }
    get_scheduled_job(conn, job_id)
}

pub fn claim_and_advance_due_job(
    conn: &Connection,
    job_id: &str,
    now_rfc3339: &str,
    new_next_run_at: &str,
) -> Result<Option<ScheduledJob>> {
    let current_gen: i64 = match conn.query_row(
        "SELECT generation FROM scheduled_jobs WHERE job_id = ?1 AND status = 'active' AND next_run_at <= ?2",
        params![job_id, now_rfc3339],
        |row| row.get(0),
    ) {
        Ok(g) => g,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let wall = chrono::Utc::now().to_rfc3339();
    let updated = conn.execute(
        "UPDATE scheduled_jobs
         SET next_run_at = ?1,
             last_run_at = ?2,
             updated_at = ?3,
             generation = generation + 1
         WHERE job_id = ?4 AND status = 'active' AND generation = ?5",
        params![new_next_run_at, now_rfc3339, wall, job_id, current_gen],
    )?;
    if updated == 0 {
        return Ok(None);
    }
    get_scheduled_job(conn, job_id)
}

pub fn load_due_scheduled_jobs(
    conn: &Connection,
    now_rfc3339: &str,
    limit: usize,
) -> Result<Vec<ScheduledJob>> {
    let limit = limit as i64;
    let mut stmt = conn.prepare(
        "SELECT job_id, owner_agent_id, root_session_id, target_agent_id,
                target_revision_id, message, metadata_json, cron_expr, timezone, next_run_at,
                last_run_at, status, created_at, updated_at, last_error, generation
         FROM scheduled_jobs
         WHERE status = 'active' AND next_run_at <= ?1
         ORDER BY next_run_at ASC
         LIMIT ?2",
    )?;
    let mut rows = stmt.query(params![now_rfc3339, limit])?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next()? {
        jobs.push(row_to_job(row)?);
    }
    Ok(jobs)
}

/// Load due scheduled jobs whose `owner_agent_id` matches the given owner.
///
/// Unlike [`load_due_scheduled_jobs`] (which loads globally), this query
/// filters by owner before the LIMIT is applied, preventing sentinel-owned
/// jobs from being starved by a backlog of non-sentinel due jobs.
pub fn load_due_scheduled_jobs_for_owner(
    conn: &Connection,
    owner_agent_id: &str,
    now_rfc3339: &str,
    limit: usize,
) -> Result<Vec<ScheduledJob>> {
    let limit = limit as i64;
    let mut stmt = conn.prepare(
        "SELECT job_id, owner_agent_id, root_session_id, target_agent_id,
                target_revision_id, message, metadata_json, cron_expr, timezone, next_run_at,
                last_run_at, status, created_at, updated_at, last_error, generation
         FROM scheduled_jobs
         WHERE owner_agent_id = ?1 AND status = 'active' AND next_run_at <= ?2
         ORDER BY next_run_at ASC
         LIMIT ?3",
    )?;
    let mut rows = stmt.query(params![owner_agent_id, now_rfc3339, limit])?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next()? {
        jobs.push(row_to_job(row)?);
    }
    Ok(jobs)
}

pub fn advance_next_run(
    conn: &Connection,
    job_id: &str,
    next_run_at: &str,
    last_run_at: Option<&str>,
    last_error: Option<&str>,
) -> Result<()> {
    let now_rfc3339 = chrono::Utc::now().to_rfc3339();
    if let Some(la) = last_run_at {
        conn.execute(
            "UPDATE scheduled_jobs
             SET next_run_at = ?1, last_run_at = ?2, updated_at = ?3, last_error = ?4, generation = generation + 1
             WHERE job_id = ?5",
            params![next_run_at, la, now_rfc3339, last_error.unwrap_or(""), job_id],
        )?;
    } else {
        conn.execute(
            "UPDATE scheduled_jobs
             SET next_run_at = ?1, updated_at = ?2, last_error = ?3, generation = generation + 1
             WHERE job_id = ?4",
            params![next_run_at, now_rfc3339, last_error.unwrap_or(""), job_id],
        )?;
    }
    Ok(())
}

pub fn pause_scheduled_job(conn: &Connection, job_id: &str) -> Result<bool> {
    let now_rfc3339 = chrono::Utc::now().to_rfc3339();
    let updated = conn.execute(
        "UPDATE scheduled_jobs
         SET status = 'paused', updated_at = ?1, generation = generation + 1
         WHERE job_id = ?2 AND status = 'active'",
        params![now_rfc3339, job_id],
    )?;
    Ok(updated > 0)
}

pub fn resume_scheduled_job(conn: &Connection, job_id: &str) -> Result<bool> {
    let now_rfc3339 = chrono::Utc::now().to_rfc3339();
    let updated = conn.execute(
        "UPDATE scheduled_jobs
         SET status = 'active', updated_at = ?1, generation = generation + 1
         WHERE job_id = ?2 AND status = 'paused'",
        params![now_rfc3339, job_id],
    )?;
    Ok(updated > 0)
}

pub fn cancel_scheduled_job(conn: &Connection, job_id: &str) -> Result<bool> {
    let now_rfc3339 = chrono::Utc::now().to_rfc3339();
    let updated = conn.execute(
        "UPDATE scheduled_jobs
         SET status = 'cancelled', updated_at = ?1, generation = generation + 1
         WHERE job_id = ?2 AND status != 'cancelled'",
        params![now_rfc3339, job_id],
    )?;
    Ok(updated > 0)
}

pub fn cancel_scheduled_jobs_for_root(conn: &Connection, root_session_id: &str) -> Result<usize> {
    let now_rfc3339 = chrono::Utc::now().to_rfc3339();
    let updated = conn.execute(
        "UPDATE scheduled_jobs
         SET status = 'cancelled', updated_at = ?1, generation = generation + 1
         WHERE root_session_id = ?2 AND status = 'active'",
        params![now_rfc3339, root_session_id],
    )?;
    Ok(updated)
}

pub fn delete_scheduled_job(conn: &Connection, job_id: &str) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM scheduled_jobs WHERE job_id = ?1",
        params![job_id],
    )?;
    Ok(deleted > 0)
}

const JOB_SELECT_COLUMNS: &str = "job_id, owner_agent_id, root_session_id, target_agent_id, \
    target_revision_id, message, metadata_json, cron_expr, timezone, next_run_at, \
    last_run_at, status, created_at, updated_at, last_error, generation";

pub fn list_scheduled_jobs(
    conn: &Connection,
    owner_agent_id: Option<&str>,
    root_session_id: Option<&str>,
    status: Option<ScheduledJobStatus>,
    limit: usize,
) -> Result<Vec<ScheduledJob>> {
    let mut sql = format!("SELECT {JOB_SELECT_COLUMNS} FROM scheduled_jobs WHERE 1=1");
    let mut param_vals: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(owner) = owner_agent_id {
        sql.push_str(" AND owner_agent_id = ?");
        param_vals.push(Box::new(owner.to_string()));
    }
    if let Some(root) = root_session_id {
        sql.push_str(" AND root_session_id = ?");
        param_vals.push(Box::new(root.to_string()));
    }
    if let Some(st) = status {
        sql.push_str(" AND status = ?");
        param_vals.push(Box::new(st.to_string()));
    }
    sql.push_str(" ORDER BY next_run_at ASC LIMIT ?");
    param_vals.push(Box::new(limit as i64));

    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_vals.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(param_refs.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_job(row)?);
    }
    Ok(out)
}

fn row_to_job(row: &rusqlite::Row<'_>) -> Result<ScheduledJob> {
    let status_str: String = row.get(11)?;
    let status = match status_str.as_str() {
        "active" => ScheduledJobStatus::Active,
        "paused" => ScheduledJobStatus::Paused,
        "cancelled" => ScheduledJobStatus::Cancelled,
        _ => ScheduledJobStatus::Active,
    };
    let last_error: Option<String> = row.get(14)?;
    let last_error = last_error.filter(|s| !s.is_empty());
    Ok(ScheduledJob {
        job_id: row.get(0)?,
        owner_agent_id: row.get(1)?,
        root_session_id: row.get(2)?,
        target_agent_id: row.get(3)?,
        target_revision_id: row.get(4)?,
        message: row.get(5)?,
        metadata_json: row.get(6)?,
        cron_expr: row.get(7)?,
        timezone: row.get(8)?,
        next_run_at: row.get(9)?,
        last_run_at: row.get(10)?,
        status,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        last_error,
        generation: row.get(15)?,
    })
}
