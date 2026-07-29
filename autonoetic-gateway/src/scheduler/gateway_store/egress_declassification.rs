//! Operator-approved egress declassification grants (RFC §8 / #909).

use anyhow::Result;
use autonoetic_types::background::GrantScope;
use autonoetic_types::egress::{
    EgressDeclassificationGrant, EgressDeclassificationTarget, Sink,
};
use rusqlite::{params, Connection};

fn parse_target(kind: &str, value: &str) -> Option<EgressDeclassificationTarget> {
    match kind {
        "envelope_id" => Some(EgressDeclassificationTarget::EnvelopeId(value.to_string())),
        "source_pattern" => Some(EgressDeclassificationTarget::SourcePattern(value.to_string())),
        "memory_id" => Some(EgressDeclassificationTarget::MemoryId(value.to_string())),
        _ => None,
    }
}

/// Reject source patterns that would silently blanket-declassify everything.
fn source_pattern_is_bound(pattern: &str) -> bool {
    let p = pattern.trim();
    if p.is_empty() {
        return false;
    }
    !matches!(p, "*" | "**" | "**/*" | "*/*")
}

pub(super) fn insert_grant(
    conn: &Connection,
    root_session_id: &str,
    session_id: &str,
    agent_id: &str,
    target: &EgressDeclassificationTarget,
    allowed_sink: Sink,
    scope: &GrantScope,
    granted_by: &str,
    granted_at: &str,
    source_approval_id: Option<&str>,
    expires_at: Option<&str>,
) -> Result<i64> {
    if let EgressDeclassificationTarget::SourcePattern(pattern) = target {
        anyhow::ensure!(
            source_pattern_is_bound(pattern),
            "egress declassification source_pattern must be bound (not a bare wildcard); got {pattern:?}"
        );
    }
    conn.execute(
        "INSERT INTO egress_declassification_grants
            (root_session_id, session_id, agent_id, target_kind, target_value,
             allowed_sink, scope, granted_by, granted_at, source_approval_id, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(root_session_id, session_id, agent_id, scope, target_kind, target_value, allowed_sink)
         DO UPDATE SET
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
            target.kind_str(),
            target.value(),
            serde_json::to_string(&allowed_sink)?,
            scope.as_str(),
            granted_by,
            granted_at,
            source_approval_id,
            expires_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn grant_active(expires_at: Option<&str>, revoked_at: Option<&str>, now: &str) -> bool {
    if revoked_at.is_some() {
        return false;
    }
    if let Some(exp) = expires_at {
        if !exp.is_empty() {
            let exp_dt = match chrono::DateTime::parse_from_rfc3339(exp) {
                Ok(dt) => dt,
                Err(_) => return false, // malformed expiry ⇒ fail-closed
            };
            let now_dt = match chrono::DateTime::parse_from_rfc3339(now) {
                Ok(dt) => dt,
                Err(_) => return false,
            };
            if exp_dt < now_dt {
                return false;
            }
        }
    }
    true
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

pub(super) fn declassification_allows(
    conn: &Connection,
    target: &EgressDeclassificationTarget,
    allowed_sink: Sink,
    session_id: &str,
    root_session_id: &str,
    now: &str,
) -> Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT session_id, scope, expires_at, revoked_at
         FROM egress_declassification_grants
         WHERE root_session_id = ?1
           AND target_kind = ?2
           AND target_value = ?3
           AND allowed_sink = ?4",
    )?;
    let rows = stmt.query_map(
        params![
            root_session_id,
            target.kind_str(),
            target.value(),
            serde_json::to_string(&allowed_sink)?,
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        },
    )?;
    for row in rows {
        let (grant_session_id, scope_str, expires_at, revoked_at) = row?;
        let scope = GrantScope::from_str_lossy(&scope_str);
        if !scope_matches(&scope, &grant_session_id, session_id, root_session_id) {
            continue;
        }
        if grant_active(expires_at.as_deref(), revoked_at.as_deref(), now) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn list_grants_for_root(
    conn: &Connection,
    root_session_id: &str,
) -> Result<Vec<EgressDeclassificationGrant>> {
    let mut stmt = conn.prepare(
        "SELECT id, root_session_id, session_id, agent_id, target_kind, target_value,
                allowed_sink, scope, granted_by, granted_at, source_approval_id, expires_at
         FROM egress_declassification_grants
         WHERE root_session_id = ?1 AND revoked_at IS NULL
         ORDER BY granted_at DESC",
    )?;
    let rows = stmt.query_map(params![root_session_id], |row| {
        let target_kind: String = row.get(4)?;
        let target_value: String = row.get(5)?;
        let allowed_sink_raw: String = row.get(6)?;
        let scope_str: String = row.get(7)?;
        let target = parse_target(&target_kind, &target_value).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(4, target_kind, rusqlite::types::Type::Text)
        })?;
        let allowed_sink: Sink = serde_json::from_str(&allowed_sink_raw).map_err(|_| {
            rusqlite::Error::InvalidColumnType(6, allowed_sink_raw, rusqlite::types::Type::Text)
        })?;
        Ok(EgressDeclassificationGrant {
            id: row.get(0)?,
            root_session_id: row.get(1)?,
            session_id: row.get(2)?,
            agent_id: row.get(3)?,
            target,
            allowed_sink,
            scope: GrantScope::from_str_lossy(&scope_str),
            granted_by: row.get(8)?,
            granted_at: row.get(9)?,
            source_approval_id: row.get(10)?,
            expires_at: row.get(11)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub(super) fn delete_grants_for_root(conn: &Connection, root_session_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM egress_declassification_grants WHERE root_session_id = ?1",
        params![root_session_id],
    )?;
    Ok(())
}

impl super::GatewayStore {
    pub fn insert_egress_declassification_grant(
        &self,
        root_session_id: &str,
        session_id: &str,
        agent_id: &str,
        target: &EgressDeclassificationTarget,
        allowed_sink: Sink,
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
            target,
            allowed_sink,
            scope,
            granted_by,
            granted_at,
            source_approval_id,
            expires_at,
        )?;
        Ok(())
    }

    pub fn egress_declassification_allows(
        &self,
        target: &EgressDeclassificationTarget,
        allowed_sink: Sink,
        session_id: &str,
        root_session_id: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        declassification_allows(
            &conn,
            target,
            allowed_sink,
            session_id,
            root_session_id,
            &now,
        )
    }

    pub fn delete_egress_declassification_grants(&self, root_session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        delete_grants_for_root(&conn, root_session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::background::GrantScope;
    use rusqlite::Connection;

    fn open_test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE egress_declassification_grants (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                root_session_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                target_kind TEXT NOT NULL,
                target_value TEXT NOT NULL,
                allowed_sink TEXT NOT NULL,
                scope TEXT NOT NULL,
                granted_by TEXT NOT NULL,
                granted_at TEXT NOT NULL,
                source_approval_id TEXT,
                expires_at TEXT,
                revoked_at TEXT,
                revoked_reason TEXT,
                UNIQUE(root_session_id, session_id, agent_id, scope, target_kind, target_value, allowed_sink)
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn malformed_expires_at_is_fail_closed() {
        let conn = open_test_conn();
        let target = EgressDeclassificationTarget::SourcePattern("session:root".into());
        insert_grant(
            &conn,
            "root",
            "root/sess",
            "agent",
            &target,
            Sink::Network,
            &GrantScope::RootSession,
            "op",
            "2026-01-01T00:00:00Z",
            None,
            Some("not-rfc3339"),
        )
        .unwrap();
        assert!(
            !declassification_allows(
                &conn,
                &target,
                Sink::Network,
                "root/sess",
                "root",
                "2026-01-01T00:00:00Z",
            )
            .unwrap()
        );
    }

    #[test]
    fn root_scope_does_not_match_prefix_collision() {
        let conn = open_test_conn();
        let target = EgressDeclassificationTarget::SourcePattern("session:root".into());
        insert_grant(
            &conn,
            "root",
            "root/sess",
            "agent",
            &target,
            Sink::Network,
            &GrantScope::RootSession,
            "op",
            "2026-01-01T00:00:00Z",
            None,
            None,
        )
        .unwrap();
        assert!(
            !declassification_allows(
                &conn,
                &target,
                Sink::Network,
                "root2/sess",
                "root",
                "2026-01-01T00:00:00Z",
            )
            .unwrap()
        );
        assert!(
            declassification_allows(
                &conn,
                &target,
                Sink::Network,
                "root/sess",
                "root",
                "2026-01-01T00:00:00Z",
            )
            .unwrap()
        );
    }
}
