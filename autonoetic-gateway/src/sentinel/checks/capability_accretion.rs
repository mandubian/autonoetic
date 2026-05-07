//! Capability-accretion detection.
//!
//! Queries `promotion_history` to find agents that have accumulated a high rate
//! of promotions, which may indicate gradual capability scope expansion.
//! Each flagged agent generates a `warning`-severity finding (deterministic
//! finding from SQL, but requires human review to confirm accretion intent).
//!
//! Severity is `warning`, not `critical`, because the check cannot inspect
//! the actual capability sets without loading each SKILL.md artifact. That
//! deeper comparison belongs to the LLM-judgment layer (Phase 2).

use anyhow::Result;
use autonoetic_types::security::{
    AffectedEntities, EvidenceAnchor, FindingSeverity, FindingType, Reproducibility,
    SecurityFinding,
};
use rusqlite::Connection;

/// An agent with a suspiciously high promotion rate.
struct AccretionCandidate {
    agent_id: String,
    promotion_count: i64,
    latest_promotion_id: String,
}

/// Flag agents that have received more than `threshold` promotions in the
/// past `window_days` days. Returns one finding per flagged agent.
pub fn scan_capability_accretion(
    conn: &Connection,
    sentinel_revision_id: &str,
    window_days: u32,
    threshold: u32,
) -> Result<Vec<SecurityFinding>> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(window_days as i64);
    let cutoff_str = cutoff.to_rfc3339();
    let threshold_i = threshold as i64;

    let mut stmt = conn.prepare(
        "SELECT agent_id, COUNT(*) AS cnt,
                MAX(promotion_id) AS latest_promotion_id
         FROM promotion_history
         WHERE created_at > ?1
         GROUP BY agent_id
         HAVING cnt > ?2
         ORDER BY cnt DESC",
    )?;

    let candidates = stmt
        .query_map(rusqlite::params![cutoff_str, threshold_i], |row| {
            Ok(AccretionCandidate {
                agent_id: row.get(0)?,
                promotion_count: row.get(1)?,
                latest_promotion_id: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    for c in candidates {
        let finding = SecurityFinding::new(
            FindingType::CapabilityAccretion,
            FindingSeverity::Warning,
            0.7,
            Reproducibility::Deterministic,
            format!(
                "Agent '{}' received {} promotions in the last {} days (threshold: {}). \
                 Review promotion history for gradual capability scope expansion. \
                 Use 'autonoetic agent revision list {}' to inspect each revision.",
                c.agent_id, c.promotion_count, window_days, threshold, c.agent_id
            ),
            sentinel_revision_id,
        )
        .with_affected(AffectedEntities {
            agent_alias: Some(c.agent_id.clone()),
            ..Default::default()
        })
        .with_anchors(vec![EvidenceAnchor::RevisionId {
            id: c.latest_promotion_id.clone(),
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
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS promotion_history (
                promotion_id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                alias_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                previous_revision_id TEXT,
                new_revision_id TEXT NOT NULL,
                source_eval_run_id TEXT,
                reason TEXT,
                created_at TEXT NOT NULL,
                created_by_type TEXT NOT NULL,
                created_by_id TEXT NOT NULL,
                origin_node_id TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn flags_agent_with_high_promotion_rate() {
        let conn = setup_db();
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..6u32 {
            conn.execute(
                "INSERT INTO promotion_history VALUES (?1,'promote','alias','coder.default',NULL,'rev','eval',NULL,?2,'agent','eval-runner','local')",
                rusqlite::params![format!("promo_{}", i), now],
            )
            .unwrap();
        }

        let findings = scan_capability_accretion(&conn, "sentinel-rev-1", 7, 5).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].finding_type, FindingType::CapabilityAccretion);
        assert_eq!(findings[0].severity, FindingSeverity::Warning);
    }

    #[test]
    fn does_not_flag_under_threshold() {
        let conn = setup_db();
        let now = chrono::Utc::now().to_rfc3339();
        for i in 0..3u32 {
            conn.execute(
                "INSERT INTO promotion_history VALUES (?1,'promote','alias','coder.default',NULL,'rev','eval',NULL,?2,'agent','eval-runner','local')",
                rusqlite::params![format!("promo_{}", i), now],
            )
            .unwrap();
        }

        let findings = scan_capability_accretion(&conn, "sentinel-rev-1", 7, 5).unwrap();
        assert!(findings.is_empty());
    }
}
