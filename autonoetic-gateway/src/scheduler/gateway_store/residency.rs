//! Session residency — the explicit record of sessions the gateway is holding
//! open and addressable by `agent_message`.
//!
//! A resident agent (`agent.resident_idle_ttl_secs`) does not terminate when its
//! task finishes; it parks in `YieldReason::Idle` and a row lands here. The row
//! is deleted when the session resumes or when the reaper closes it, so the
//! table always describes *now*.
//!
//! This exists because addressability was previously inferred, and both
//! available proxies are wrong: `session_agent_bindings` is append-only (every
//! session ever bound), and `session_outcomes` receives a row at the first
//! finalize — suspended sessions included — so its absence means "currently
//! executing", not "reachable".

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResidency {
    pub session_id: String,
    pub root_session_id: String,
    pub agent_id: String,
    /// Checkpoint turn to resume from when a message wakes this session.
    pub turn_id: String,
    pub since: String,
    pub expires_at: String,
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionResidency> {
    Ok(SessionResidency {
        session_id: row.get(0)?,
        root_session_id: row.get(1)?,
        agent_id: row.get(2)?,
        turn_id: row.get(3)?,
        since: row.get(4)?,
        expires_at: row.get(5)?,
    })
}

const SELECT: &str = "SELECT session_id, root_session_id, agent_id, turn_id, since, expires_at
     FROM session_residency";

/// Update the `agent_id` of an existing park. Session handoff (#1088) rebinds
/// a live session to another agent; the park row's agent is otherwise never
/// rewritten by [`upsert_residency`] (which deliberately refreshes only TTL
/// fields), leaving the session addressable under an agent that no longer
/// executes it. No row, no-op — nothing is parked.
pub(super) fn update_residency_agent(conn: &Connection, session_id: &str, agent_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE session_residency SET agent_id = ?2 WHERE session_id = ?1",
        params![session_id, agent_id],
    )?;
    Ok(())
}

/// Park a session (or refresh an existing park after it handles a message).
pub(super) fn upsert_residency(conn: &Connection, r: &SessionResidency) -> Result<()> {
    conn.execute(
        "INSERT INTO session_residency
            (session_id, root_session_id, agent_id, turn_id, since, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(session_id) DO UPDATE SET
            turn_id = excluded.turn_id,
            since = excluded.since,
            expires_at = excluded.expires_at",
        params![
            r.session_id,
            r.root_session_id,
            r.agent_id,
            r.turn_id,
            r.since,
            r.expires_at
        ],
    )?;
    Ok(())
}

/// Drop the park — the session is resuming, closing, or being reaped.
pub(super) fn clear_residency(conn: &Connection, session_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM session_residency WHERE session_id = ?1",
        params![session_id],
    )?;
    Ok(())
}

pub(super) fn get_residency(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<SessionResidency>> {
    let mut stmt = conn.prepare(&format!("{SELECT} WHERE session_id = ?1"))?;
    let mut rows = stmt.query_map(params![session_id], map_row)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Parked, unexpired sessions of `agent_id` — the recipients a broadcast can
/// actually reach. `now` is passed in rather than read so callers can test
/// expiry deterministically.
pub(super) fn list_resident_sessions_for_agent(
    conn: &Connection,
    agent_id: &str,
    now: &str,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} WHERE agent_id = ?1 AND expires_at > ?2 ORDER BY since ASC"
    ))?;
    let rows = stmt.query_map(params![agent_id, now], map_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?.session_id);
    }
    Ok(out)
}

/// Parks whose TTL has elapsed. The caller closes each session properly
/// (outcome row, checkpoint cleanup) before clearing the row.
pub(super) fn list_expired_residencies(
    conn: &Connection,
    now: &str,
) -> Result<Vec<SessionResidency>> {
    let mut stmt = conn.prepare(&format!("{SELECT} WHERE expires_at <= ?1 ORDER BY expires_at ASC"))?;
    let rows = stmt.query_map(params![now], map_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
