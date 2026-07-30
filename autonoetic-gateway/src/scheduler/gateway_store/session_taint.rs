//! Cross-agent egress taint — RFC data-envelopes §5.5.
//!
//! A session's **accumulated taint** is the intersection of the labels of
//! everything it touched (see `egress_labeler::session_accumulated_taint`). It
//! is recorded here, keyed by session id, at session finalize. When that
//! session's content crosses to another session, the recipient reads this taint
//! and labels the transferred content, so a tainted child can't hand content to
//! a remote-pinned sibling — closing the `LocalAgent` hole.
//!
//! **Consumed so far (slice 4a):** spawn-return — `workflow_wait` intersects a
//! surfaced child's taint into the parent's tool-result label. The
//! `agent_message` payload path (sender→recipient) reads this table in slice 4b
//! and is not yet wired.
//!
//! Only *restrictive* taint is stored: an `unrestricted` session touched
//! nothing private, so there is nothing to carry (absence ⇒ unrestricted).

use anyhow::Result;
use autonoetic_types::egress::EgressLabel;
use rusqlite::{params, Connection, OptionalExtension};

/// Record (or replace) a session's accumulated egress taint. The label is
/// stored as its serde-transparent sink-set JSON — the same wire shape the
/// chokepoint and label map use.
///
/// **Upholds "absence ⇒ unrestricted" structurally**: an `unrestricted` label
/// *deletes* the row rather than storing it. The module invariant is that only
/// restrictive taint is stored, and leaving that to caller-side
/// `if !taint.is_unrestricted()` guards means one unguarded caller silently
/// creates rows that say "everything is allowed" — indistinguishable at a
/// glance from a real taint, and wrong for any reader treating `None` as the
/// only clean state. Enforcing it here means no caller can violate it.
pub(super) fn set_taint(conn: &Connection, session_id: &str, label: &EgressLabel) -> Result<()> {
    if label.is_unrestricted() {
        conn.execute(
            "DELETE FROM session_egress_taint WHERE session_id = ?1",
            params![session_id],
        )?;
        return Ok(());
    }
    let label_json = serde_json::to_string(label)?;
    conn.execute(
        "INSERT INTO session_egress_taint (session_id, label_json, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO UPDATE SET label_json = ?2, updated_at = ?3",
        params![session_id, label_json, chrono::Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// Intersect `label` into a session's stored taint, never widening it, and
/// return the resulting taint.
///
/// [`set_taint`] replaces the row, which is correct for the finalize path (it
/// recomputes from the whole label sidecar) but wrong for an incremental
/// contribution: writing a less-restrictive label over an accumulated one would
/// *widen* the taint and break monotonicity (RFC §2.4). Ingest-side
/// contributions — a peer's inbound label, an operator's marked message — use
/// this instead.
///
/// Read and write happen under the single `conn` the caller already holds, so
/// there is no window where a concurrent contribution could be lost to a
/// read-modify-write race.
pub(super) fn restrict_taint(
    conn: &Connection,
    session_id: &str,
    label: &EgressLabel,
) -> Result<EgressLabel> {
    let current = get_taint(conn, session_id)?;
    let merged = match current {
        Some(existing) => existing.restrict(label),
        None => label.clone(),
    };
    // Unconditional: `set_taint` normalizes an unrestricted result to absence,
    // so this both stores a real taint and clears a legacy unrestricted row.
    // Intersection only shrinks, so an unrestricted merge cannot arise from a
    // restrictive existing taint — the delete can never drop a live taint.
    set_taint(conn, session_id, &merged)?;
    Ok(merged)
}

/// Read a session's accumulated taint. `None` when the session recorded no
/// restrictive taint (⇒ treat as `unrestricted`).
pub(super) fn get_taint(conn: &Connection, session_id: &str) -> Result<Option<EgressLabel>> {
    let row: Option<String> = conn
        .query_row(
            "SELECT label_json FROM session_egress_taint WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .optional()?;
    match row {
        Some(json) => Ok(Some(serde_json::from_str(&json)?)),
        None => Ok(None),
    }
}
