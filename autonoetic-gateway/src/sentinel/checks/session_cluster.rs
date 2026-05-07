//! Session-cluster anomaly detection.
//!
//! Queries `causal_events` to find sessions with unusual event shapes that may
//! indicate compromise, adversarial prompting, or runaway execution. These are
//! statistical/heuristic checks — the findings land at `warning` severity with
//! `llm_judgment` reproducibility because the shape alone does not confirm
//! malicious intent. A human or LLM reasoning pass is required to characterise
//! whether the cluster indicates a real problem.
//!
//! **Anomaly types detected:**
//!
//! 1. **Rapid tool-failure burst**: a session accumulates more than
//!    `failure_burst_threshold` tool-category error-status causal events within
//!    the past `window_minutes` rolling window. Indicates repeated failed tool
//!    invocations — possible adversarial prompt, broken agent, or runaway retry.
//!
//! 2. **Repeated sandbox.exec attempts**: a session issues more than
//!    `exec_repeat_threshold` `sandbox_exec` events with the same target within
//!    the past `window_minutes` rolling window. Indicates a possible sandbox
//!    escape probing pattern or stuck retry loop.
//!
//! Both checks always scan the full rolling window (computed as `now - window_minutes`).
//! They do not take a `since_rfc3339` parameter because incremental sweep cadence
//! must not undercount or over-count events in a fixed rolling window.

use anyhow::Result;
use autonoetic_types::security::{
    AffectedEntities, EvidenceAnchor, FindingSeverity, FindingType, Reproducibility,
    SecurityFinding,
};
use rusqlite::Connection;

// ── Anomaly 1: Rapid tool-failure burst ──────────────────────────────────────

struct FailureBurstCandidate {
    session_id: String,
    agent_id: String,
    error_count: i64,
    /// `event_id` of the earliest tool-error event in the burst for this
    /// specific `(session_id, agent_id)` group.
    first_error_event_id: String,
}

/// Flag sessions with an unusually high number of tool-layer error-status events.
///
/// The cutoff is always `now - window_minutes` — not the sweep's `since` timestamp —
/// so the rolling window is independent of incremental sweep cadence.
/// Returns one finding per flagged `(session, agent)` pair.
pub fn scan_failure_bursts(
    conn: &Connection,
    sentinel_revision_id: &str,
    window_minutes: u32,
    threshold: u32,
    scan_limit: u32,
) -> Result<Vec<SecurityFinding>> {
    let cutoff = {
        let d = chrono::Utc::now() - chrono::Duration::minutes(window_minutes as i64);
        d.to_rfc3339()
    };
    let threshold_i = threshold as i64;
    let limit_i = scan_limit as i64;

    let mut stmt = conn.prepare(
        "SELECT ce.session_id,
                ce.agent_id,
                COUNT(*) AS error_count,
                (SELECT e2.event_id
                 FROM causal_events e2
                 WHERE e2.session_id = ce.session_id
                   AND e2.agent_id   = ce.agent_id
                   AND e2.category   = 'tool'
                   AND e2.status     = 'error'
                   AND e2.timestamp  > ?1
                 ORDER BY e2.timestamp ASC
                 LIMIT 1) AS first_error_event_id
         FROM causal_events ce
         WHERE ce.category = 'tool'
           AND ce.status   = 'error'
           AND ce.timestamp > ?1
         GROUP BY ce.session_id, ce.agent_id
         HAVING error_count > ?2
         ORDER BY error_count DESC
         LIMIT ?3",
    )?;

    let candidates = stmt
        .query_map(
            rusqlite::params![cutoff, threshold_i, limit_i],
            |row| {
                Ok(FailureBurstCandidate {
                    session_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    error_count: row.get(2)?,
                    first_error_event_id: row.get::<_, Option<String>>(3)?
                        .unwrap_or_default(),
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    for c in candidates {
        let mut anchors = Vec::new();
        if !c.first_error_event_id.is_empty() {
            anchors.push(EvidenceAnchor::CausalEvent {
                id: c.first_error_event_id.clone(),
            });
        }

        let finding = SecurityFinding::new(
            FindingType::BehavioralAnomaly,
            FindingSeverity::Warning,
            0.55,
            Reproducibility::LlmJudgment,
            format!(
                "Session '{}' (agent '{}') produced {} tool-layer error events in the past \
                 {} minutes (threshold: {}). Review the causal chain for runaway retries, \
                 adversarial prompting, or repeated forbidden-tool attempts.",
                c.session_id, c.agent_id, c.error_count, window_minutes, threshold
            ),
            sentinel_revision_id,
        )
        .with_affected(AffectedEntities {
            agent_alias: Some(c.agent_id.clone()),
            session_id: Some(c.session_id.clone()),
            ..Default::default()
        })
        .with_anchors(anchors);

        findings.push(finding);
    }
    Ok(findings)
}

// ── Anomaly 2: Repeated sandbox.exec with same target ────────────────────────

struct ExecRepeatCandidate {
    session_id: String,
    agent_id: String,
    target: String,
    exec_count: i64,
    /// `event_id` of the first occurrence for this `(session, agent, target)` group.
    first_event_id: String,
}

/// Flag sessions issuing the same `sandbox_exec` target more than `threshold` times.
///
/// The cutoff is always `now - window_minutes` — not the sweep's `since` timestamp —
/// so the rolling window is independent of incremental sweep cadence.
/// Returns one finding per flagged `(session, agent, target)` triple.
pub fn scan_exec_repeats(
    conn: &Connection,
    sentinel_revision_id: &str,
    window_minutes: u32,
    threshold: u32,
    scan_limit: u32,
) -> Result<Vec<SecurityFinding>> {
    let cutoff = {
        let d = chrono::Utc::now() - chrono::Duration::minutes(window_minutes as i64);
        d.to_rfc3339()
    };
    let threshold_i = threshold as i64;
    let limit_i = scan_limit as i64;

    let mut stmt = conn.prepare(
        "SELECT ce.session_id,
                ce.agent_id,
                ce.target,
                COUNT(*) AS exec_count,
                (SELECT e2.event_id
                 FROM causal_events e2
                 WHERE e2.session_id = ce.session_id
                   AND e2.agent_id   = ce.agent_id
                   AND e2.action     = 'sandbox_exec'
                   AND e2.target     = ce.target
                   AND e2.timestamp  > ?1
                 ORDER BY e2.timestamp ASC
                 LIMIT 1) AS first_event_id
         FROM causal_events ce
         WHERE ce.action    = 'sandbox_exec'
           AND ce.timestamp > ?1
           AND ce.target IS NOT NULL
         GROUP BY ce.session_id, ce.agent_id, ce.target
         HAVING exec_count > ?2
         ORDER BY exec_count DESC
         LIMIT ?3",
    )?;

    let candidates = stmt
        .query_map(
            rusqlite::params![cutoff, threshold_i, limit_i],
            |row| {
                Ok(ExecRepeatCandidate {
                    session_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    target: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    exec_count: row.get(3)?,
                    first_event_id: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    for c in candidates {
        let mut anchors = Vec::new();
        if !c.first_event_id.is_empty() {
            anchors.push(EvidenceAnchor::CausalEvent {
                id: c.first_event_id.clone(),
            });
        }

        let finding = SecurityFinding::new(
            FindingType::BehavioralAnomaly,
            FindingSeverity::Warning,
            0.60,
            Reproducibility::LlmJudgment,
            format!(
                "Session '{}' (agent '{}') issued {} identical sandbox_exec calls to target \
                 '{}' in the past {} minutes (threshold: {}). Review the causal chain for \
                 sandbox-escape probing or stuck retry loops. \
                 Use 'autonoetic trace show {}' to inspect the session.",
                c.session_id, c.agent_id, c.exec_count, c.target,
                window_minutes, threshold, c.session_id
            ),
            sentinel_revision_id,
        )
        .with_affected(AffectedEntities {
            agent_alias: Some(c.agent_id.clone()),
            session_id: Some(c.session_id.clone()),
            ..Default::default()
        })
        .with_anchors(anchors);

        findings.push(finding);
    }
    Ok(findings)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS causal_events (
                event_id     TEXT PRIMARY KEY,
                agent_id     TEXT NOT NULL,
                session_id   TEXT NOT NULL,
                turn_id      TEXT,
                event_seq    INTEGER NOT NULL,
                timestamp    TEXT NOT NULL,
                category     TEXT NOT NULL,
                action       TEXT NOT NULL,
                status       TEXT NOT NULL,
                enforced_rules TEXT NOT NULL DEFAULT '[\"R+++3\"]',
                target       TEXT,
                payload      TEXT,
                payload_ref  TEXT,
                evidence_ref TEXT,
                reason       TEXT
            );",
        )
        .unwrap();
        conn
    }

    fn insert_event(
        conn: &Connection,
        event_id: &str,
        session_id: &str,
        agent_id: &str,
        category: &str,
        action: &str,
        status: &str,
        target: Option<&str>,
        timestamp: &str,
    ) {
        conn.execute(
            "INSERT INTO causal_events
                (event_id, agent_id, session_id, event_seq, timestamp, category, action, status, target)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![event_id, agent_id, session_id, timestamp, category, action, status, target],
        )
        .unwrap();
    }

    #[test]
    fn flags_session_with_failure_burst() {
        let conn = setup_db();
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..8u32 {
            insert_event(
                &conn,
                &format!("evt_{}", i),
                "sess_burst",
                "coder.default",
                "tool",
                "sandbox_exec",
                "error",
                None,
                &now,
            );
        }

        let findings = scan_failure_bursts(&conn, "sentinel-rev-1", 60, 5, 100).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].finding_type, FindingType::BehavioralAnomaly);
        assert_eq!(findings[0].severity, FindingSeverity::Warning);
        assert_eq!(findings[0].reproducibility, Reproducibility::LlmJudgment);
        assert_eq!(
            findings[0].affected.session_id.as_deref(),
            Some("sess_burst")
        );
        // Anchor must point to the first event in the burst.
        assert!(
            findings[0]
                .evidence_anchors
                .iter()
                .any(|a| matches!(a, EvidenceAnchor::CausalEvent { .. })),
            "must anchor to a causal event"
        );
    }

    #[test]
    fn does_not_flag_non_tool_errors() {
        let conn = setup_db();
        let now = chrono::Utc::now().to_rfc3339();
        // Insert errors but in category 'lifecycle', not 'tool'.
        for i in 0..10u32 {
            insert_event(
                &conn,
                &format!("evt_{}", i),
                "sess_lifecycle",
                "coder.default",
                "lifecycle",
                "session_start",
                "error",
                None,
                &now,
            );
        }
        // These should not be counted by scan_failure_bursts (tool only).
        let findings = scan_failure_bursts(&conn, "sentinel-rev-1", 60, 5, 100).unwrap();
        assert!(findings.is_empty(), "non-tool errors must not trigger failure burst");
    }

    #[test]
    fn does_not_flag_session_under_failure_threshold() {
        let conn = setup_db();
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..3u32 {
            insert_event(
                &conn,
                &format!("evt_{}", i),
                "sess_ok",
                "coder.default",
                "tool",
                "tool_call",
                "error",
                None,
                &now,
            );
        }
        let findings = scan_failure_bursts(&conn, "sentinel-rev-1", 60, 5, 100).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn flags_session_with_exec_repeat() {
        let conn = setup_db();
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..6u32 {
            insert_event(
                &conn,
                &format!("exec_{}", i),
                "sess_repeat",
                "coder.default",
                "tool",
                "sandbox_exec",
                "success",
                Some("/bin/sh -c id"),
                &now,
            );
        }

        let findings = scan_exec_repeats(&conn, "sentinel-rev-1", 60, 4, 100).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].finding_type, FindingType::BehavioralAnomaly);
        assert!(findings[0]
            .proposed_remediation
            .contains("/bin/sh -c id"));
    }

    #[test]
    fn does_not_flag_exec_repeat_under_threshold() {
        let conn = setup_db();
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..2u32 {
            insert_event(
                &conn,
                &format!("exec_{}", i),
                "sess_ok",
                "coder.default",
                "tool",
                "sandbox_exec",
                "success",
                Some("ls /"),
                &now,
            );
        }
        let findings = scan_exec_repeats(&conn, "sentinel-rev-1", 60, 4, 100).unwrap();
        assert!(findings.is_empty());
    }

    #[test]
    fn anchor_agent_id_matches_group() {
        // Two different agents in the same session each produce a burst.
        // Each finding must be associated with the correct agent and include
        // a CausalEvent anchor (the `agent_id` filter in the subquery ensures
        // the anchor event belongs to the same agent as the finding).
        let conn = setup_db();
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..8u32 {
            insert_event(
                &conn,
                &format!("aaa_evt_{}", i),
                "sess_shared",
                "agent.a",
                "tool",
                "tool_call",
                "error",
                None,
                &now,
            );
            insert_event(
                &conn,
                &format!("bbb_evt_{}", i),
                "sess_shared",
                "agent.b",
                "tool",
                "tool_call",
                "error",
                None,
                &now,
            );
        }
        let findings = scan_failure_bursts(&conn, "sentinel-rev-1", 60, 5, 100).unwrap();
        assert_eq!(findings.len(), 2, "one finding per agent");
        for f in &findings {
            // Every finding must have a CausalEvent anchor.
            let anchor_id = f.evidence_anchors.iter().find_map(|a| {
                if let EvidenceAnchor::CausalEvent { id } = a {
                    Some(id.as_str())
                } else {
                    None
                }
            });
            assert!(anchor_id.is_some(), "finding must have a CausalEvent anchor");

            // The anchor event ID prefix must correspond to the agent in the finding.
            let agent = f.affected.agent_alias.as_deref().unwrap();
            let expected_prefix = if agent == "agent.a" { "aaa_" } else { "bbb_" };
            assert!(
                anchor_id.unwrap().starts_with(expected_prefix),
                "anchor event must belong to the flagged agent (agent={}, anchor={:?}, expected prefix={})",
                agent, anchor_id, expected_prefix
            );
        }
    }

    #[test]
    fn empty_db_produces_no_cluster_findings() {
        let conn = setup_db();
        let f1 = scan_failure_bursts(&conn, "rev", 60, 5, 100).unwrap();
        let f2 = scan_exec_repeats(&conn, "rev", 60, 4, 100).unwrap();
        assert!(f1.is_empty());
        assert!(f2.is_empty());
    }
}
