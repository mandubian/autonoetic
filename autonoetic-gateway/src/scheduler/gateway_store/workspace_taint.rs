//! Agent workspace egress labels — RFC data-envelopes §11, #1001.
//!
//! The agent workspace (`agents_dir.join(agent_id)`, bind-mounted into the
//! sandbox) is a durable object, and it carries a durable label. Content that
//! enters it by *movement* (unzip, cp, tar, git clone …) cannot be followed by
//! path-scoped source rules, which are a firewall over named paths — so the
//! workspace as a whole is the labeled unit instead. Recorded here, keyed by
//! `agent_id`; read back when any exec runs in the workspace, so a laundered
//! file read in a *later* session (or by a later agent-agnostic pass) is still
//! labeled.
//!
//! **Why the workspace and not write-path tracking.** A control shaped like a
//! guarantee that isn't one is worse than a coarse one (the #987 lesson):
//! write-path static analysis can be evaded the same way read-side analysis
//! can, while implying the precision it lacks. Coarseness is the mechanism
//! here — nothing moves, so there is no evasion surface.
//!
//! Only *restrictive* labels are stored (absence ⇒ unrestricted), and the write
//! path only ever intersects, so a workspace label can tighten but never widen
//! (RFC §2.4). Widening happens exclusively through the operator-approval path:
//! a materialized `EgressDeclassificationTarget::Workspace` grant deletes the
//! row (`delete_label`).

use anyhow::Result;
use autonoetic_types::egress::EgressLabel;
use rusqlite::{params, Connection, OptionalExtension};

/// Read an agent workspace's label. `None` ⇒ unrestricted.
pub(super) fn get_label(conn: &Connection, agent_id: &str) -> Result<Option<EgressLabel>> {
    let row: Option<String> = conn
        .query_row(
            "SELECT label_json FROM agent_workspace_egress_labels WHERE agent_id = ?1",
            params![agent_id],
            |row| row.get(0),
        )
        .optional()?;
    match row {
        Some(json) => Ok(Some(serde_json::from_str(&json)?)),
        None => Ok(None),
    }
}

/// Intersect `label` into an agent workspace's stored label and return the
/// result.
///
/// Never widens: once the workspace has seen restricted content, everything
/// produced there stays restricted until an operator-approved declassification
/// deletes the row — there is no other un-restriction path, by design (a
/// "clear" a later session could trigger would be a laundering lever itself).
///
/// An unrestricted result stores nothing (and clears any existing row), keeping
/// "absence ⇒ unrestricted" true in the table rather than by convention.
pub(super) fn restrict_label(
    conn: &Connection,
    agent_id: &str,
    label: &EgressLabel,
) -> Result<EgressLabel> {
    let merged = match get_label(conn, agent_id)? {
        Some(existing) => existing.restrict(label),
        None => label.clone(),
    };
    if merged.is_unrestricted() {
        delete_label(conn, agent_id)?;
        return Ok(merged);
    }
    conn.execute(
        "INSERT INTO agent_workspace_egress_labels (agent_id, label_json, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(agent_id) DO UPDATE SET label_json = ?2, updated_at = ?3",
        params![
            agent_id,
            serde_json::to_string(&merged)?,
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(merged)
}

/// Delete an agent workspace's label — the operator-approved clearing path
/// (`EgressDeclassificationTarget::Workspace` grant materialization). Returns
/// whether a row was removed.
pub(super) fn delete_label(conn: &Connection, agent_id: &str) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM agent_workspace_egress_labels WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(changed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory store with just the workspace table (same DDL as the v79
    /// migration) — these tests exercise the row semantics, not migrations.
    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE agent_workspace_egress_labels (
                agent_id TEXT PRIMARY KEY,
                label_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )
        .unwrap();
        c
    }

    #[test]
    fn restrict_intersects_and_never_widens() {
        let c = conn();
        let l1 = restrict_label(&c, "coder.abc", &EgressLabel::local_only()).unwrap();
        assert_eq!(l1, EgressLabel::local_only());
        // A weaker intersection is a no-op — the stored label never widens.
        let l2 = restrict_label(&c, "coder.abc", &EgressLabel::no_remote_model()).unwrap();
        assert_eq!(l2, EgressLabel::local_only());
        assert_eq!(
            get_label(&c, "coder.abc").unwrap(),
            Some(EgressLabel::local_only())
        );
        // A stronger one tightens.
        let narrower = EgressLabel::from_sinks([autonoetic_types::egress::Sink::LocalModel]);
        let l3 = restrict_label(&c, "coder.abc", &narrower).unwrap();
        assert_eq!(l3, narrower);
        assert_ne!(l3, EgressLabel::local_only());
    }

    #[test]
    fn unrestricted_intersection_stores_nothing() {
        let c = conn();
        // A fresh agent restricted with the unrestricted label keeps the
        // "absence ⇒ unrestricted" table invariant.
        let merged = restrict_label(&c, "coder.abc", &EgressLabel::unrestricted()).unwrap();
        assert!(merged.is_unrestricted());
        assert_eq!(get_label(&c, "coder.abc").unwrap(), None);
        // Restricting the unrestricted label is a no-op on an existing row.
        restrict_label(&c, "coder.abc", &EgressLabel::local_only()).unwrap();
        let merged = restrict_label(&c, "coder.abc", &EgressLabel::unrestricted()).unwrap();
        assert_eq!(merged, EgressLabel::local_only());
        assert_eq!(
            get_label(&c, "coder.abc").unwrap(),
            Some(EgressLabel::local_only()),
            "an unrestricted intersection never widens a stored label"
        );
    }

    #[test]
    fn delete_removes_and_reports() {
        let c = conn();
        assert_eq!(delete_label(&c, "coder.abc").unwrap(), false);
        restrict_label(&c, "coder.abc", &EgressLabel::local_only()).unwrap();
        assert_eq!(delete_label(&c, "coder.abc").unwrap(), true);
        assert_eq!(get_label(&c, "coder.abc").unwrap(), None);
    }

    #[test]
    fn labels_are_per_agent() {
        let c = conn();
        restrict_label(&c, "coder.abc", &EgressLabel::local_only()).unwrap();
        assert_eq!(get_label(&c, "coder.xyz").unwrap(), None);
    }
}
