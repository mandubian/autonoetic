//! Approval-bypass pattern detection — **FROZEN BASELINE**.
//!
//! ## DO NOT EDIT WITHOUT EXPLICIT OPERATOR ACTION.
//!
//! Frozen snapshot of `super::checks::approval_bypass` (issue #153).
//! See `super::baseline::credential` for the full editing-rules rationale.
//!
//! Last frozen at `BASELINE_VERSION = 1.0.0` (issue #153, initial freeze).
//! See `super::BASELINE_VERSION` for the version pin and bump policy.

#![allow(dead_code)]

use anyhow::Result;
use autonoetic_types::security::{
    AffectedEntities, EvidenceAnchor, FindingSeverity, FindingType, Reproducibility,
    SecurityFinding,
};
use rusqlite::Connection;

/// Flag root sessions that have more than `denial_threshold` denied approvals
/// in the past `window_days` days. One finding per offending (root_session_id,
/// agent_id) pair, using the most-recently-denied session_id for attribution.
///
/// `scope_agent_id` filters to a single agent (used by the pre-promotion gate).
pub fn scan_approval_denials(
    conn: &Connection,
    sentinel_revision_id: &str,
    window_days: u32,
    denial_threshold: u32,
    scope_agent_id: Option<&str>,
) -> Result<Vec<SecurityFinding>> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(window_days as i64);
    let cutoff_str = cutoff.to_rfc3339();
    let threshold_i = denial_threshold as i64;

    // Group by (root_session_id, agent_id) so every selected column is
    // part of the group key — avoids SQLite returning arbitrary values for
    // non-aggregated columns that were previously projected without grouping.
    // The most-recent session_id is fetched via a correlated subquery.
    let mut stmt = conn.prepare(
        "SELECT ap.root_session_id,
                ap.agent_id,
                COUNT(*) AS cnt,
                (SELECT session_id FROM approvals a2
                 WHERE a2.root_session_id = ap.root_session_id
                   AND a2.agent_id = ap.agent_id
                   AND a2.status = 'denied'
                   AND a2.created_at > ?1
                 ORDER BY a2.created_at DESC LIMIT 1) AS latest_session_id
         FROM approvals ap
         WHERE ap.status = 'denied'
           AND ap.created_at > ?1
           AND (?3 IS NULL OR ap.agent_id = ?3)
         GROUP BY ap.root_session_id, ap.agent_id
         HAVING cnt > ?2
         ORDER BY cnt DESC",
    )?;

    struct DenialRow {
        root_session_id: String,
        agent_id: String,
        count: i64,
        latest_session_id: Option<String>,
    }

    let rows = stmt
        .query_map(
            rusqlite::params![cutoff_str, threshold_i, scope_agent_id],
            |row| {
                Ok(DenialRow {
                    root_session_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    count: row.get(2)?,
                    latest_session_id: row.get(3)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    for r in rows {
        let session_id = r.latest_session_id.unwrap_or_else(|| r.root_session_id.clone());
        let finding = SecurityFinding::new(
            FindingType::ApprovalBypass,
            FindingSeverity::Warning,
            0.8,
            Reproducibility::Deterministic,
            format!(
                "Root session '{}' (agent '{}') accumulated {} denied approval requests \
                 in the past {} days. Review the session's causal chain for repeated \
                 attempts to perform disallowed operations.",
                r.root_session_id, r.agent_id, r.count, window_days
            ),
            sentinel_revision_id,
        )
        .with_affected(AffectedEntities {
            agent_alias: Some(r.agent_id.clone()),
            session_id: Some(session_id),
            ..Default::default()
        })
        .with_anchors(vec![EvidenceAnchor::CausalEvent {
            id: format!("approval_denial_root:{}", r.root_session_id),
        }]);
        findings.push(finding);
    }
    Ok(findings)
}

/// Flag root sessions that have approved `sandbox_exec` actions but no
/// matching `session_approval_grants` row. This is a structural check:
/// every approved sandbox_exec should produce a grant; its absence may
/// indicate a grant-recording failure or a bypass.
///
/// The check is scoped to `action_type = 'sandbox_exec'` and requires that
/// the NOT EXISTS matches on both `root_session_id` and `agent_id` to avoid
/// false positives from grants belonging to a different agent in the same
/// root session.
pub fn scan_exec_without_grant(
    conn: &Connection,
    sentinel_revision_id: &str,
    since_rfc3339: Option<&str>,
    limit: u32,
    scope_agent_id: Option<&str>,
) -> Result<Vec<SecurityFinding>> {
    let since = since_rfc3339.unwrap_or("1970-01-01T00:00:00Z");
    let lim = limit as i64;

    let mut stmt = conn.prepare(
        "SELECT DISTINCT ap.root_session_id, ap.session_id, ap.agent_id
         FROM approvals ap
         WHERE ap.status = 'approved'
           AND ap.action_type = 'sandbox_exec'
           AND ap.created_at > ?1
           AND ap.root_session_id IS NOT NULL
           AND (?3 IS NULL OR ap.agent_id = ?3)
           AND NOT EXISTS (
               SELECT 1 FROM session_approval_grants sg
               WHERE sg.root_session_id = ap.root_session_id
                 AND sg.agent_id = ap.agent_id
           )
         ORDER BY ap.created_at ASC
         LIMIT ?2",
    )?;

    struct ApprovalRow {
        root_session_id: String,
        session_id: String,
        agent_id: String,
    }

    let rows = stmt
        .query_map(
            rusqlite::params![since, lim, scope_agent_id],
            |row| {
                Ok(ApprovalRow {
                    root_session_id: row.get(0)?,
                    session_id: row.get(1)?,
                    agent_id: row.get(2)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    for r in rows {
        let finding = SecurityFinding::new(
            FindingType::ApprovalBypass,
            FindingSeverity::Warning,
            0.75,
            Reproducibility::Deterministic,
            format!(
                "Root session '{}' (agent '{}') has an approved sandbox_exec action \
                 but no corresponding session_approval_grant for that agent. \
                 This may indicate a grant-recording failure or a bypass. \
                 Review approvals and session_approval_grants for this root session.",
                r.root_session_id, r.agent_id
            ),
            sentinel_revision_id,
        )
        .with_affected(AffectedEntities {
            agent_alias: Some(r.agent_id.clone()),
            session_id: Some(r.session_id.clone()),
            ..Default::default()
        })
        .with_anchors(vec![EvidenceAnchor::CausalEvent {
            id: format!("approval_grant_gap:{}:{}", r.root_session_id, r.agent_id),
        }]);
        findings.push(finding);
    }
    Ok(findings)
}

