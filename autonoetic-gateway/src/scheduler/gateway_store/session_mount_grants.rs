//! Session-scoped mount grants (#1002 slice 5, issue #1296).
//!
//! The filesystem analog of the host grants in `approvals.rs`: when an
//! operator approves a sandbox exec whose manifest declared host mounts that
//! neither `sandbox.allowed_mount_roots` nor an existing grant covers, each
//! request materializes a row here, and the declared-mount resolver cures the
//! matching denial on the agent's retry. Same lifecycle contract as host
//! grants: session-scoped, expiring on `default_grant_ttl_secs`, soft-revoked
//! (the row is the audit trail), deleted on emergency stop and root-session
//! close. Coverage is by canonical host path with prefix semantics and a
//! per-row read-only ceiling — an `ro` row never cures an `rw` request.

use anyhow::Result;
use autonoetic_types::background::{GrantScope, ROOT_WIDE_GRANT_AGENT};
use rusqlite::{params, Connection};

/// One active session mount grant, as the declared-mount resolver sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMountGrant {
    pub id: i64,
    pub root_session_id: String,
    pub session_id: String,
    pub agent_id: String,
    /// Canonicalized host path the grant covers (prefix semantics: also
    /// everything under it).
    pub canonical_path: String,
    /// `true` = read-only ceiling; an ro grant never cures an rw declaration.
    pub readonly: bool,
    pub scope: GrantScope,
    pub granted_by: String,
    pub granted_at: String,
    pub source_approval_id: Option<String>,
    pub expires_at: Option<String>,
}

pub(super) fn insert_grant(
    conn: &Connection,
    root_session_id: &str,
    session_id: &str,
    agent_id: &str,
    canonical_path: &str,
    readonly: bool,
    scope: &GrantScope,
    granted_by: &str,
    granted_at: &str,
    source_approval_id: Option<&str>,
    expires_at: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO session_mount_grants
            (root_session_id, session_id, agent_id, canonical_path, readonly,
             scope, granted_by, granted_at, source_approval_id, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(root_session_id, session_id, agent_id, scope, canonical_path)
         DO UPDATE SET
            -- A re-approval can widen ro → rw but never narrows an existing
            -- rw ceiling back to ro (stored 0 = rw, 1 = ro; MIN = widest).
            readonly = MIN(session_mount_grants.readonly, excluded.readonly),
            granted_by = excluded.granted_by,
            granted_at = excluded.granted_at,
            source_approval_id = excluded.source_approval_id,
            expires_at = excluded.expires_at,
            revoked_at = NULL,
            revoked_reason = NULL",
        params![
            root_session_id,
            session_id,
            agent_id,
            canonical_path,
            readonly,
            scope.as_str(),
            granted_by,
            granted_at,
            source_approval_id,
            expires_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Whether `query_session_id` belongs to `root_session_id` (exact or `root/child`).
fn session_under_root_session(query_session_id: &str, root_session_id: &str) -> bool {
    query_session_id == root_session_id
        || query_session_id
            .strip_prefix(root_session_id)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn scope_matches(
    grant_scope: &GrantScope,
    grant_session_id: &str,
    query_session_id: &str,
    root_session_id: &str,
) -> bool {
    match grant_scope {
        GrantScope::RootSession => session_under_root_session(query_session_id, root_session_id),
        GrantScope::Session => grant_session_id == query_session_id,
    }
}

/// Active (non-revoked, non-expired) mount grants under a root that cover the
/// requesting session and agent — the input the declared-mount resolver
/// applies its path-prefix and ro/rw semantics to. Fail-closed on malformed
/// expiry stamps, like every other grant flavor.
pub(super) fn active_grants_for(
    conn: &Connection,
    root_session_id: &str,
    session_id: &str,
    agent_id: &str,
    now: &str,
) -> Result<Vec<SessionMountGrant>> {
    let mut stmt = conn.prepare(
        "SELECT id, root_session_id, session_id, agent_id, canonical_path, readonly,
                scope, granted_by, granted_at, source_approval_id, expires_at
         FROM session_mount_grants
         WHERE root_session_id = ?1 AND revoked_at IS NULL
         ORDER BY id",
    )?;
    let rows = stmt.query_map(params![root_session_id], |row| {
        let scope_str: String = row.get(6)?;
        Ok(SessionMountGrant {
            id: row.get(0)?,
            root_session_id: row.get(1)?,
            session_id: row.get(2)?,
            agent_id: row.get(3)?,
            canonical_path: row.get(4)?,
            readonly: row.get::<_, i64>(5)? != 0,
            scope: GrantScope::from_str_lossy(&scope_str),
            granted_by: row.get(7)?,
            granted_at: row.get(8)?,
            source_approval_id: row.get(9)?,
            expires_at: row.get(10)?,
        })
    })?;
    let mut active = Vec::new();
    for grant in rows {
        let grant = grant?;
        if let Some(exp) = &grant.expires_at {
            let exp_dt = match chrono::DateTime::parse_from_rfc3339(exp) {
                Ok(dt) => dt,
                Err(_) => continue, // malformed expiry ⇒ fail-closed
            };
            let now_dt = match chrono::DateTime::parse_from_rfc3339(now) {
                Ok(dt) => dt,
                Err(_) => continue,
            };
            if exp_dt < now_dt {
                continue;
            }
        }
        let agent_covers = grant.agent_id == agent_id || grant.agent_id == ROOT_WIDE_GRANT_AGENT;
        if !agent_covers {
            continue;
        }
        if !scope_matches(&grant.scope, &grant.session_id, session_id, root_session_id) {
            continue;
        }
        active.push(grant);
    }
    Ok(active)
}

pub(super) fn delete_grants_for_root(conn: &Connection, root_session_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM session_mount_grants WHERE root_session_id = ?1",
        params![root_session_id],
    )?;
    Ok(())
}

/// Operator revocation. Marks grants revoked (UPDATE, never DELETE — the row
/// is the audit trail). With `path`, revokes every grant at or **above** that
/// path — revoking `/data` kills both the `/data` grant and a narrower
/// `/data/mail` grant whose coverage contains the revoked path; without
/// `path`, every active grant under the root.
pub(super) fn revoke_grants_for_root(
    conn: &Connection,
    root_session_id: &str,
    path: Option<&str>,
    reason: &str,
    now: &str,
) -> Result<usize> {
    let candidates: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, canonical_path FROM session_mount_grants
             WHERE root_session_id = ?1 AND revoked_at IS NULL",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let query = path.map(std::path::PathBuf::from);
    let mut count = 0usize;
    for (id, canonical) in candidates {
        let matches = match &query {
            None => true,
            Some(q) => {
                let grant_path = std::path::Path::new(&canonical);
                q.starts_with(grant_path)
            }
        };
        if !matches {
            continue;
        }
        count += conn.execute(
            "UPDATE session_mount_grants
             SET revoked_at = ?1, revoked_reason = ?2
             WHERE id = ?3 AND revoked_at IS NULL",
            params![now, reason, id],
        )?;
    }
    Ok(count)
}

/// Physical reaper for lapsed rows — mirrors `prune_expired_grants` for host
/// grants. Returns the number of rows deleted.
pub(super) fn prune_expired(conn: &Connection, now: &str) -> Result<usize> {
    let count = conn.execute(
        "DELETE FROM session_mount_grants
         WHERE expires_at IS NOT NULL AND expires_at != '' AND expires_at < ?1",
        params![now],
    )?;
    Ok(count)
}

/// Soft-revoke a single ACTIVE mount grant by row id, scoped to the root
/// session that owns it (row ids are enumerable, not capabilities).
/// Idempotent: absent, already-revoked, or foreign-owned ids report `false`.
pub(super) fn revoke_grant_by_id(
    conn: &Connection,
    root_session_id: &str,
    grant_id: i64,
    now: &str,
    reason: &str,
) -> Result<bool> {
    let count = conn.execute(
        "UPDATE session_mount_grants
         SET revoked_at = ?1, revoked_reason = ?2
         WHERE id = ?3 AND root_session_id = ?4 AND revoked_at IS NULL",
        params![now, reason, grant_id, root_session_id],
    )?;
    Ok(count > 0)
}

impl super::GatewayStore {
    pub fn insert_session_mount_grant(
        &self,
        root_session_id: &str,
        session_id: &str,
        agent_id: &str,
        canonical_path: &str,
        readonly: bool,
        scope: &GrantScope,
        granted_by: &str,
        granted_at: &str,
        source_approval_id: Option<&str>,
        expires_at: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        insert_grant(
            &conn,
            root_session_id,
            session_id,
            agent_id,
            canonical_path,
            readonly,
            scope,
            granted_by,
            granted_at,
            source_approval_id,
            expires_at,
        )?;
        Ok(())
    }

    /// Active mount grants the requesting session/agent can rely on, resolved
    /// against `root_session_id`. The declared-mount resolver applies path and
    /// ro/rw semantics; this only does row lifecycle (revocation, expiry,
    /// scope, agent).
    pub fn active_session_mount_grants(
        &self,
        root_session_id: &str,
        session_id: &str,
        agent_id: &str,
    ) -> Result<Vec<SessionMountGrant>> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        active_grants_for(&conn, root_session_id, session_id, agent_id, &now)
    }

    pub fn delete_session_mount_grants(&self, root_session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        delete_grants_for_root(&conn, root_session_id)
    }

    /// Revoke (not delete) mount grants for a root session — at-or-above
    /// `path` when given, all active grants otherwise. Returns the number of
    /// rows transitioned.
    pub fn revoke_session_mount_grants(
        &self,
        root_session_id: &str,
        path: Option<&str>,
        reason: &str,
    ) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        revoke_grants_for_root(&conn, root_session_id, path, reason, &now)
    }

    pub fn prune_expired_mount_grants(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        prune_expired(&conn, &now)
    }

    /// Soft-revoke a single mount grant by row id, scoped to the root session
    /// that owns it. Returns `true` when a row transitioned active → revoked.
    pub fn revoke_session_mount_grant_by_id(
        &self,
        root_session_id: &str,
        grant_id: i64,
        reason: &str,
    ) -> Result<bool> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        revoke_grant_by_id(&conn, root_session_id, grant_id, &now, reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_mount_grants (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                root_session_id TEXT NOT NULL,
                session_id TEXT NOT NULL DEFAULT '',
                agent_id TEXT NOT NULL,
                canonical_path TEXT NOT NULL,
                readonly INTEGER NOT NULL DEFAULT 1,
                scope TEXT NOT NULL DEFAULT 'root_session',
                granted_by TEXT NOT NULL,
                granted_at TEXT NOT NULL,
                source_approval_id TEXT,
                expires_at TEXT,
                revoked_at TEXT,
                revoked_reason TEXT,
                UNIQUE(root_session_id, session_id, agent_id, scope, canonical_path)
            );",
        )
        .unwrap();
        conn
    }

    fn insert(
        conn: &Connection,
        canonical: &str,
        readonly: bool,
        expires_at: Option<&str>,
    ) -> i64 {
        insert_grant(
            conn,
            "root",
            "root",
            "agent-a",
            canonical,
            readonly,
            &GrantScope::RootSession,
            "op",
            "2026-01-01T00:00:00Z",
            None,
            expires_at,
        )
        .unwrap()
    }

    #[test]
    fn rw_grant_covers_ro_request_but_not_vice_versa_at_row_level() {
        let conn = open_test_conn();
        insert(&conn, "/data", true, None);
        let active =
            active_grants_for(&conn, "root", "root", "agent-a", "2026-01-02T00:00:00Z").unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].readonly, "ro row stored as read-only");

        // Re-approval with rw widens the ceiling; a later ro re-approval must
        // not narrow it back.
        insert(&conn, "/data", false, None);
        let active =
            active_grants_for(&conn, "root", "root", "agent-a", "2026-01-02T00:00:01Z").unwrap();
        assert!(!active[0].readonly, "rw re-approval must widen the row");
        insert(&conn, "/data", true, None);
        let active =
            active_grants_for(&conn, "root", "root", "agent-a", "2026-01-02T00:00:02Z").unwrap();
        assert!(!active[0].readonly, "ro re-approval must not narrow rw");
    }

    #[test]
    fn expired_and_malformed_expiry_fail_closed() {
        let conn = open_test_conn();
        insert(&conn, "/data", true, Some("2026-01-01T00:00:01Z"));
        insert(&conn, "/other", true, Some("not-rfc3339"));
        let active =
            active_grants_for(&conn, "root", "root", "agent-a", "2026-01-02T00:00:00Z").unwrap();
        assert!(active.is_empty(), "lapsed and malformed rows must not surface");
    }

    #[test]
    fn agent_and_scope_filters_apply() {
        let conn = open_test_conn();
        insert(&conn, "/data", true, None);
        // Different agent under the same root: no coverage.
        let active =
            active_grants_for(&conn, "root", "root", "agent-b", "2026-01-02T00:00:00Z").unwrap();
        assert!(active.is_empty());
        // Child session of the root under RootSession scope: covered.
        let active =
            active_grants_for(&conn, "root", "root/child", "agent-a", "2026-01-02T00:00:00Z")
                .unwrap();
        assert_eq!(active.len(), 1);
        // The `*` sentinel covers any agent.
        insert_grant(
            &conn,
            "root",
            "root",
            ROOT_WIDE_GRANT_AGENT,
            "/wide",
            true,
            &GrantScope::RootSession,
            "op",
            "2026-01-01T00:00:00Z",
            None,
            None,
        )
        .unwrap();
        let active =
            active_grants_for(&conn, "root", "root", "agent-b", "2026-01-02T00:00:00Z").unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].canonical_path, "/wide");
    }

    #[test]
    fn revoke_by_path_kills_grants_at_or_above_the_path() {
        let conn = open_test_conn();
        insert(&conn, "/data", true, None);
        insert(&conn, "/data/mail", true, None);
        insert(&conn, "/etc", true, None);
        // Revoking /data/mail/deep must kill both the /data/mail grant and the
        // /data grant that contains it, but leave /etc alone.
        let n = revoke_grants_for_root(
            &conn,
            "root",
            Some("/data/mail/deep"),
            "operator revoke",
            "2026-01-02T00:00:00Z",
        )
        .unwrap();
        assert_eq!(n, 2);
        let active =
            active_grants_for(&conn, "root", "root", "agent-a", "2026-01-02T00:00:01Z").unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].canonical_path, "/etc");
    }

    #[test]
    fn prune_deletes_only_lapsed_rows() {
        let conn = open_test_conn();
        insert(&conn, "/data", true, Some("2026-01-01T00:00:01Z"));
        insert(&conn, "/etc", true, None);
        let n = prune_expired(&conn, "2026-01-02T00:00:00Z").unwrap();
        assert_eq!(n, 1);
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_mount_grants", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1, "non-expiring row must survive the reaper");
    }
}
