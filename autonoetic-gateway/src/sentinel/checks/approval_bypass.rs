//! Approval-bypass pattern detection.
//!
//! Checks for:
//! 1. Sessions with an unusually high count of denied approvals — may indicate
//!    an agent repeatedly attempting disallowed operations.
//! 2. `sandbox.exec` causal events where no corresponding approval grant was
//!    present — suggests the approval gate was bypassed or mis-configured.

use anyhow::Result;
use autonoetic_types::security::{
    AffectedEntities, EvidenceAnchor, FindingSeverity, FindingType, Reproducibility,
    SecurityFinding,
};
use rusqlite::Connection;

/// Flag root sessions that have more than `denial_threshold` denied approvals
/// in the past `window_days` days. One finding per offending session.
pub fn scan_approval_denials(
    conn: &Connection,
    sentinel_revision_id: &str,
    window_days: u32,
    denial_threshold: u32,
) -> Result<Vec<SecurityFinding>> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(window_days as i64);
    let cutoff_str = cutoff.to_rfc3339();
    let threshold_i = denial_threshold as i64;

    let mut stmt = conn.prepare(
        "SELECT root_session_id, session_id, agent_id, COUNT(*) AS cnt
         FROM approvals
         WHERE status = 'denied'
           AND created_at > ?1
         GROUP BY root_session_id
         HAVING cnt > ?2
         ORDER BY cnt DESC",
    )?;

    struct DenialRow {
        root_session_id: String,
        session_id: String,
        agent_id: String,
        count: i64,
    }

    let rows = stmt
        .query_map(rusqlite::params![cutoff_str, threshold_i], |row| {
            Ok(DenialRow {
                root_session_id: row.get(0)?,
                session_id: row.get(1)?,
                agent_id: row.get(2)?,
                count: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    for r in rows {
        let finding = SecurityFinding::new(
            FindingType::ApprovalBypass,
            FindingSeverity::Warning,
            0.8,
            Reproducibility::Deterministic,
            format!(
                "Session '{}' (agent '{}') accumulated {} denied approval requests \
                 in the past {} days. Review the session's causal chain for repeated \
                 attempts to perform disallowed operations.",
                r.root_session_id, r.agent_id, r.count, window_days
            ),
            sentinel_revision_id,
        )
        .with_affected(AffectedEntities {
            agent_alias: Some(r.agent_id.clone()),
            session_id: Some(r.session_id.clone()),
            ..Default::default()
        })
        .with_anchors(vec![EvidenceAnchor::CausalEvent {
            id: format!("approval_denial_root:{}", r.root_session_id),
        }]);
        findings.push(finding);
    }
    Ok(findings)
}

/// Flag root sessions that have approved network-access operations but no
/// corresponding `session_approval_grants` row — a structural gap that suggests
/// the approval grant may not have been recorded properly.
///
/// This checks approved approvals in the `approvals` table against the
/// `session_approval_grants` table using `root_session_id`.
pub fn scan_exec_without_grant(
    conn: &Connection,
    sentinel_revision_id: &str,
    since_rfc3339: Option<&str>,
    limit: u32,
) -> Result<Vec<SecurityFinding>> {
    let since = since_rfc3339.unwrap_or("1970-01-01T00:00:00Z");
    let lim = limit as i64;

    // Find approved approvals in root sessions that have NO grant recorded.
    // This can happen if an approval was granted but the grant write failed.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT ap.root_session_id, ap.session_id, ap.agent_id, ap.action_type
         FROM approvals ap
         WHERE ap.status = 'approved'
           AND ap.created_at > ?1
           AND ap.root_session_id IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM session_approval_grants sg
               WHERE sg.root_session_id = ap.root_session_id
           )
         ORDER BY ap.created_at ASC
         LIMIT ?2",
    )?;

    struct ApprovalRow {
        root_session_id: String,
        session_id: String,
        agent_id: String,
        action_type: String,
    }

    let rows = stmt
        .query_map(rusqlite::params![since, lim], |row| {
            Ok(ApprovalRow {
                root_session_id: row.get(0)?,
                session_id: row.get(1)?,
                agent_id: row.get(2)?,
                action_type: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    for r in rows {
        let finding = SecurityFinding::new(
            FindingType::ApprovalBypass,
            FindingSeverity::Warning,
            0.75,
            Reproducibility::Deterministic,
            format!(
                "Root session '{}' (agent '{}') has an approved '{}' action but no \
                 corresponding session_approval_grant record. \
                 This may indicate a grant-recording failure or a bypass. \
                 Review the approvals table and session_approval_grants for this root session.",
                r.root_session_id, r.agent_id, r.action_type
            ),
            sentinel_revision_id,
        )
        .with_affected(AffectedEntities {
            agent_alias: Some(r.agent_id.clone()),
            session_id: Some(r.session_id.clone()),
            ..Default::default()
        })
        .with_anchors(vec![EvidenceAnchor::CausalEvent {
            id: format!("approval_grant_gap:{}", r.root_session_id),
        }]);
        findings.push(finding);
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS approvals (
                request_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                root_session_id TEXT,
                workflow_id TEXT,
                task_id TEXT,
                action_type TEXT NOT NULL,
                action_payload TEXT NOT NULL,
                reason TEXT,
                evidence_ref TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL,
                decided_at TEXT,
                decided_by TEXT,
                approval_level TEXT NOT NULL DEFAULT 'operator'
            );
            CREATE TABLE IF NOT EXISTS causal_events (
                event_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                root_session_id TEXT,
                turn_id TEXT,
                event_seq INTEGER NOT NULL,
                timestamp TEXT NOT NULL,
                category TEXT NOT NULL,
                action TEXT NOT NULL,
                status TEXT NOT NULL,
                enforced_rules TEXT NOT NULL DEFAULT '[\"R+++3\"]',
                target TEXT,
                payload TEXT,
                payload_ref TEXT,
                evidence_ref TEXT,
                reason TEXT
            );
            CREATE TABLE IF NOT EXISTS session_approval_grants (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                root_session_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                host TEXT NOT NULL,
                granted_by TEXT NOT NULL,
                granted_at TEXT NOT NULL,
                source_approval_id TEXT,
                UNIQUE(root_session_id, agent_id, host)
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn flags_session_with_many_denials() {
        let conn = setup_db();
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..6u32 {
            conn.execute(
                "INSERT INTO approvals (request_id, agent_id, session_id, root_session_id,
                          action_type, action_payload, status, created_at, approval_level)
                 VALUES (?1, 'coder.default', 'sess_1', 'root_1',
                         'network', '{}', 'denied', ?2, 'operator')",
                rusqlite::params![format!("req_{}", i), now],
            )
            .unwrap();
        }

        let findings = scan_approval_denials(&conn, "sentinel-rev-1", 7, 5).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, FindingSeverity::Warning);
        assert_eq!(findings[0].finding_type, FindingType::ApprovalBypass);
    }

    #[test]
    fn does_not_flag_under_denial_threshold() {
        let conn = setup_db();
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..3u32 {
            conn.execute(
                "INSERT INTO approvals (request_id, agent_id, session_id, root_session_id,
                          action_type, action_payload, status, created_at, approval_level)
                 VALUES (?1, 'coder.default', 'sess_1', 'root_1',
                         'network', '{}', 'denied', ?2, 'operator')",
                rusqlite::params![format!("req_{}", i), now],
            )
            .unwrap();
        }

        let findings = scan_approval_denials(&conn, "sentinel-rev-1", 7, 5).unwrap();
        assert!(findings.is_empty());
    }
}
