//! Persistence for run-scoped decider appointments (#1195, umbrella #1191).
//!
//! Validation lives in `crate::decider_appointment`; this module is storage
//! only. Rows are never hard-deleted — revocation sets `revoked_at`, so the
//! record of who held the seat and when survives the seat being vacated.

use anyhow::Result;
use rusqlite::{params, Connection};

use autonoetic_types::background::ApprovalRisk;
use autonoetic_types::decider_appointment::{DeciderAppointment, DeciderGateRouting};

pub(crate) fn insert_appointment(conn: &Connection, a: &DeciderAppointment) -> Result<()> {
    conn.execute(
        "INSERT INTO decider_appointments
            (appointment_id, decider_agent, decider_revision, decider_provider, decider_model,
             kinds, scope_root_session, decider_session, risk_ceiling, advice_only, expires_at,
             max_gates, gates_decided, appointed_by, appointed_at, revoked_at, revoked_by,
             revoked_reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            a.appointment_id,
            a.decider_agent,
            a.decider_revision,
            a.decider_provider,
            a.decider_model,
            a.kinds.join(","),
            a.scope_root_session,
            a.decider_session,
            a.risk_ceiling.as_str(),
            a.advice_only as i64,
            a.expires_at,
            a.max_gates.map(|m| m as i64),
            a.gates_decided as i64,
            a.appointed_by,
            a.appointed_at,
            a.revoked_at,
            a.revoked_by,
            a.revoked_reason,
        ],
    )?;
    Ok(())
}

pub(crate) fn get_appointment(
    conn: &Connection,
    appointment_id: &str,
) -> Result<Option<DeciderAppointment>> {
    let mut stmt = conn.prepare(&format!("{SELECT_COLS} WHERE appointment_id = ?1"))?;
    let mut rows = stmt.query_map(params![appointment_id], |row| row_to_appointment(row))?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// All appointments for a scope, newest first. `active_only` filters out
/// revoked rows; wall-clock and gate-count expiry are evaluated by the caller
/// against its own clock (`DeciderAppointment::is_expired`) so this stays a
/// plain query.
pub(crate) fn list_appointments_for_scope(
    conn: &Connection,
    scope_root_session: &str,
    active_only: bool,
) -> Result<Vec<DeciderAppointment>> {
    let sql = if active_only {
        format!("{SELECT_COLS} WHERE scope_root_session = ?1 AND revoked_at IS NULL ORDER BY appointed_at DESC")
    } else {
        format!("{SELECT_COLS} WHERE scope_root_session = ?1 ORDER BY appointed_at DESC")
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![scope_root_session], |row| row_to_appointment(row))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Every un-revoked appointment across all scopes, newest first. Used by the
/// operator's standing view — an appointment pointing at a finished run is
/// exactly the thing #1199's reaper needs to see.
pub(crate) fn list_active_appointments(conn: &Connection) -> Result<Vec<DeciderAppointment>> {
    let mut stmt =
        conn.prepare(&format!("{SELECT_COLS} WHERE revoked_at IS NULL ORDER BY appointed_at DESC"))?;
    let rows = stmt.query_map([], |row| row_to_appointment(row))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Revoke, returning false when the appointment does not exist or was already
/// revoked. Idempotent by construction: the `revoked_at IS NULL` predicate
/// means a second revoke changes nothing rather than rewriting the first one's
/// attribution.
pub(crate) fn revoke_appointment(
    conn: &Connection,
    appointment_id: &str,
    revoked_by: &str,
    revoked_at: &str,
    reason: Option<&str>,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE decider_appointments
            SET revoked_at = ?1, revoked_by = ?2, revoked_reason = ?3
          WHERE appointment_id = ?4 AND revoked_at IS NULL",
        params![revoked_at, revoked_by, reason, appointment_id],
    )?;
    Ok(n > 0)
}

/// Bind the gateway-created peer-root session to the appointment (#1196).
pub(crate) fn set_decider_session(
    conn: &Connection,
    appointment_id: &str,
    decider_session: &str,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE decider_appointments SET decider_session = ?1
          WHERE appointment_id = ?2 AND revoked_at IS NULL",
        params![decider_session, appointment_id],
    )?;
    Ok(n > 0)
}

/// Increment the decided-gate tally that `max_gates` bounds.
pub(crate) fn record_gate_decided(conn: &Connection, appointment_id: &str) -> Result<bool> {
    let n = conn.execute(
        "UPDATE decider_appointments SET gates_decided = gates_decided + 1
          WHERE appointment_id = ?1 AND revoked_at IS NULL",
        params![appointment_id],
    )?;
    Ok(n > 0)
}

const SELECT_COLS: &str = "SELECT appointment_id, decider_agent, decider_revision, \
     decider_provider, decider_model, kinds, scope_root_session, decider_session, risk_ceiling, \
     advice_only, expires_at, max_gates, gates_decided, appointed_by, appointed_at, revoked_at, \
     revoked_by, revoked_reason FROM decider_appointments";

fn row_to_appointment(
    row: &rusqlite::Row<'_>,
) -> Result<DeciderAppointment, rusqlite::Error> {
    let kinds_raw: String = row.get(5)?;
    let risk_raw: String = row.get(8)?;
    let risk_ceiling = ApprovalRisk::parse(&risk_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            8,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown risk_ceiling '{risk_raw}' in stored appointment"),
            )),
        )
    })?;
    // Saturating rather than `as`: a wrapping cast on a hand-edited or
    // future-widened column would silently produce a *smaller* ceiling or
    // tally, which for `gates_decided` means a spent appointment reading as
    // live. Saturation errs toward expired in both directions.
    let max_gates: Option<i64> = row.get(11)?;
    let gates_decided: i64 = row.get(12)?;
    Ok(DeciderAppointment {
        appointment_id: row.get(0)?,
        decider_agent: row.get(1)?,
        decider_revision: row.get(2)?,
        decider_provider: row.get(3)?,
        decider_model: row.get(4)?,
        kinds: kinds_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        scope_root_session: row.get(6)?,
        decider_session: row.get(7)?,
        risk_ceiling,
        advice_only: row.get::<_, i64>(9)? != 0,
        expires_at: row.get(10)?,
        max_gates: max_gates.map(saturating_u32),
        gates_decided: saturating_u32(gates_decided),
        appointed_by: row.get(13)?,
        appointed_at: row.get(14)?,
        revoked_at: row.get(15)?,
        revoked_by: row.get(16)?,
        revoked_reason: row.get(17)?,
    })
}

/// Clamp a stored SQLite integer into `u32` without wrapping.
fn saturating_u32(v: i64) -> u32 {
    v.clamp(0, u32::MAX as i64) as u32
}


// ── Gate routings (#1197) ───────────────────────────────────────────────────

/// `INSERT OR IGNORE` on `(gate_id, appointment_id)`: gate creation is the
/// trigger, and a retried creation must not produce a second referral.
pub(crate) fn insert_gate_routing(conn: &Connection, r: &DeciderGateRouting) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO decider_gate_routings
            (routing_id, gate_id, appointment_id, decider_agent, decider_session,
             gate_kind, gate_risk, advice_only, routed_at, verdict, verdict_reason, verdict_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            r.routing_id, r.gate_id, r.appointment_id, r.decider_agent, r.decider_session,
            r.gate_kind, r.gate_risk, r.advice_only as i64, r.routed_at,
            r.verdict, r.verdict_reason, r.verdict_at,
        ],
    )?;
    Ok(n > 0)
}

pub(crate) fn list_gate_routings(conn: &Connection, gate_id: &str) -> Result<Vec<DeciderGateRouting>> {
    let mut stmt = conn.prepare(&format!("{ROUTING_COLS} WHERE gate_id = ?1 ORDER BY routed_at"))?;
    let rows = stmt.query_map(params![gate_id], |row| row_to_routing(row))?;
    let mut out = Vec::new();
    for r in rows { out.push(r?); }
    Ok(out)
}

/// Routed gates the seat has not answered — the reaper's input (#1199), and
/// the set a verdict is missing from.
pub(crate) fn list_routings_awaiting_verdict(
    conn: &Connection,
    appointment_id: &str,
) -> Result<Vec<DeciderGateRouting>> {
    let mut stmt = conn.prepare(&format!(
        "{ROUTING_COLS} WHERE appointment_id = ?1 AND verdict IS NULL ORDER BY routed_at"
    ))?;
    let rows = stmt.query_map(params![appointment_id], |row| row_to_routing(row))?;
    let mut out = Vec::new();
    for r in rows { out.push(r?); }
    Ok(out)
}

const ROUTING_COLS: &str = "SELECT routing_id, gate_id, appointment_id, decider_agent, \
     decider_session, gate_kind, gate_risk, advice_only, routed_at, verdict, verdict_reason, \
     verdict_at FROM decider_gate_routings";

fn row_to_routing(row: &rusqlite::Row<'_>) -> Result<DeciderGateRouting, rusqlite::Error> {
    Ok(DeciderGateRouting {
        routing_id: row.get(0)?,
        gate_id: row.get(1)?,
        appointment_id: row.get(2)?,
        decider_agent: row.get(3)?,
        decider_session: row.get(4)?,
        gate_kind: row.get(5)?,
        gate_risk: row.get(6)?,
        advice_only: row.get::<_, i64>(7)? != 0,
        routed_at: row.get(8)?,
        verdict: row.get(9)?,
        verdict_reason: row.get(10)?,
        verdict_at: row.get(11)?,
    })
}
