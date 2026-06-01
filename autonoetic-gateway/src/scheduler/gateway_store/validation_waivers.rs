use anyhow::Result;
use rusqlite::{params, Connection};

use autonoetic_types::plan_frame::{ValidationClass, ValidationWaiver};

pub(crate) fn save_waiver(conn: &Connection, w: &ValidationWaiver) -> Result<()> {
    conn.execute(
        "INSERT INTO validation_waivers
            (waiver_id, workflow_id, artifact_id, validation_id,
             validation_class, waived_by, reason, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            w.waiver_id,
            w.workflow_id,
            w.artifact_id,
            w.validation_id,
            w.validation_class.as_str(),
            w.waived_by,
            w.reason,
            w.created_at,
        ],
    )?;
    Ok(())
}

pub(crate) fn list_waivers_for_artifact(
    conn: &Connection,
    artifact_id: &str,
) -> Result<Vec<ValidationWaiver>> {
    let mut stmt = conn.prepare(
        "SELECT waiver_id, workflow_id, artifact_id, validation_id,
                validation_class, waived_by, reason, created_at
         FROM validation_waivers WHERE artifact_id = ?1
         ORDER BY created_at DESC",
    )?;

    let rows = stmt.query_map(params![artifact_id], |row| Ok(row_to_waiver(row)))?;

    let mut waivers = Vec::new();
    for row in rows {
        waivers.push(row??);
    }
    Ok(waivers)
}

pub(crate) fn list_waivers_for_workflow(
    conn: &Connection,
    workflow_id: &str,
) -> Result<Vec<ValidationWaiver>> {
    let mut stmt = conn.prepare(
        "SELECT waiver_id, workflow_id, artifact_id, validation_id,
                validation_class, waived_by, reason, created_at
         FROM validation_waivers WHERE workflow_id = ?1
         ORDER BY created_at DESC",
    )?;

    let rows = stmt.query_map(params![workflow_id], |row| Ok(row_to_waiver(row)))?;

    let mut waivers = Vec::new();
    for row in rows {
        waivers.push(row??);
    }
    Ok(waivers)
}

fn row_to_waiver(row: &rusqlite::Row<'_>) -> Result<ValidationWaiver, rusqlite::Error> {
    let class_str: String = row.get(4)?;
    Ok(ValidationWaiver {
        waiver_id: row.get(0)?,
        workflow_id: row.get(1)?,
        artifact_id: row.get(2)?,
        validation_id: row.get(3)?,
        validation_class: parse_validation_class(&class_str),
        waived_by: row.get(5)?,
        reason: row.get(6)?,
        created_at: row.get(7)?,
    })
}

fn parse_validation_class(s: &str) -> ValidationClass {
    match s {
        "mechanical_safety" => ValidationClass::MechanicalSafety,
        "security_review" => ValidationClass::SecurityReview,
        "quality_check" => ValidationClass::QualityCheck,
        "packaging_check" => ValidationClass::PackagingCheck,
        _ => ValidationClass::CorrectnessCheck,
    }
}
