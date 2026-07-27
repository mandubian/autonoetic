//! Session-scoped egress policy — RFC data-envelopes §5.4.
//!
//! The operator-global `egress.rules` are standing policy. A *session* policy
//! is the second rung of the granularity ladder: "for this room, these named
//! sources are private." It is keyed by **root** session so every child agent
//! spawned under it inherits the same restriction, and it is deleted when the
//! root session closes — the RFC's "die with the root session".
//!
//! Session rules only ever *add* to the global set. Because label resolution is
//! an intersection (§4.1), an added rule can restrict and never widen, so a
//! session cannot be used to loosen what the operator's config already
//! tightened.

use anyhow::Result;
use autonoetic_types::egress::EgressSessionPolicy;
use rusqlite::{params, Connection};

/// A stored session policy plus its attribution (I-6: who declared it, when).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEgressSessionPolicy {
    pub root_session_id: String,
    pub policy: EgressSessionPolicy,
    pub set_by: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Declare (or replace) the policy for a root session.
///
/// Replacing rather than merging is deliberate: the operator sees one policy
/// document per session, so `set` is idempotent and the stored state always
/// matches what was last declared.
pub(super) fn set_policy(
    conn: &Connection,
    root_session_id: &str,
    policy: &EgressSessionPolicy,
    set_by: &str,
) -> Result<StoredEgressSessionPolicy> {
    policy
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid egress session policy: {e}"))?;
    let policy_json = serde_json::to_string(policy)?;
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO egress_session_policies
            (root_session_id, policy_json, set_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(root_session_id) DO UPDATE SET
            policy_json = excluded.policy_json,
            set_by = excluded.set_by,
            updated_at = excluded.updated_at",
        params![root_session_id, policy_json, set_by, now],
    )?;
    get_policy(conn, root_session_id)?
        .ok_or_else(|| anyhow::anyhow!("egress session policy vanished immediately after write"))
}

pub(super) fn get_policy(
    conn: &Connection,
    root_session_id: &str,
) -> Result<Option<StoredEgressSessionPolicy>> {
    let mut stmt = conn.prepare(
        "SELECT root_session_id, policy_json, set_by, created_at, updated_at
         FROM egress_session_policies WHERE root_session_id = ?1",
    )?;
    let mut rows = stmt.query_map(params![root_session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let Some(row) = rows.next() else {
        return Ok(None);
    };
    let (root_session_id, policy_json, set_by, created_at, updated_at) = row?;
    // A policy row we cannot parse is a restriction we cannot enforce. Surface
    // it as an error rather than as an empty policy: silently degrading to "no
    // rules" is exactly the fail-open the RFC forbids (§2.2).
    let policy: EgressSessionPolicy = serde_json::from_str(&policy_json).map_err(|e| {
        anyhow::anyhow!("corrupt egress session policy for {root_session_id}: {e}")
    })?;
    Ok(Some(StoredEgressSessionPolicy {
        root_session_id,
        policy,
        set_by,
        created_at,
        updated_at,
    }))
}

/// Drop the policy — the root session is closing, being emergency-stopped, or
/// the operator cleared it. Returns whether a row was removed.
pub(super) fn delete_policy(conn: &Connection, root_session_id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM egress_session_policies WHERE root_session_id = ?1",
        params![root_session_id],
    )?;
    Ok(n > 0)
}
