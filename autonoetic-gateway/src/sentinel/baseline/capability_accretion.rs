//! Capability-accretion detection — **FROZEN BASELINE**.
//!
//! ## DO NOT EDIT WITHOUT EXPLICIT OPERATOR ACTION.
//!
//! Frozen snapshot of `super::checks::capability_accretion` (issue #153).
//! See `super::baseline::credential` for the full editing-rules rationale —
//! summary: changes to detection logic go to `super::checks`, not here, so the
//! dual-sweep can detect a regression in the canonical sentinel.
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

/// An agent with a suspiciously high promotion rate.
struct AccretionCandidate {
    agent_id: String,
    promotion_count: i64,
    /// The `promotion_id` of the most recent promotion (by `created_at`).
    latest_promotion_id: String,
    /// The `new_revision_id` corresponding to the most recent promotion.
    latest_revision_id: String,
}

/// Flag agents that have received more than `threshold` promotions in the
/// past `window_days` days. Returns one finding per flagged agent.
///
/// `scope_agent_id` restricts to a single agent (used by the pre-promotion
/// gate so a sibling agent's accretion doesn't block this promotion).
pub fn scan_capability_accretion(
    conn: &Connection,
    sentinel_revision_id: &str,
    window_days: u32,
    threshold: u32,
    scope_agent_id: Option<&str>,
) -> Result<Vec<SecurityFinding>> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(window_days as i64);
    let cutoff_str = cutoff.to_rfc3339();
    let threshold_i = threshold as i64;

    // Two-step approach: first count promotions per agent in the window, then
    // fetch the most recent promotion separately to anchor the finding.
    // Using a scalar subquery for the anchor avoids JOIN multiplication when
    // multiple rows share the same MAX(created_at) timestamp.
    let mut stmt = conn.prepare(
        "SELECT ph.agent_id,
                COUNT(*) AS cnt,
                (SELECT p2.promotion_id
                 FROM promotion_history p2
                 WHERE p2.agent_id = ph.agent_id
                   AND p2.created_at > ?1
                 ORDER BY p2.created_at DESC, p2.promotion_id DESC
                 LIMIT 1) AS latest_promotion_id,
                (SELECT p2.new_revision_id
                 FROM promotion_history p2
                 WHERE p2.agent_id = ph.agent_id
                   AND p2.created_at > ?1
                 ORDER BY p2.created_at DESC, p2.promotion_id DESC
                 LIMIT 1) AS latest_revision_id
         FROM promotion_history ph
         WHERE ph.created_at > ?1
           AND (?3 IS NULL OR ph.agent_id = ?3)
         GROUP BY ph.agent_id
         HAVING cnt > ?2
         ORDER BY cnt DESC",
    )?;

    let candidates = stmt
        .query_map(
            rusqlite::params![cutoff_str, threshold_i, scope_agent_id],
            |row| {
                Ok(AccretionCandidate {
                    agent_id: row.get(0)?,
                    promotion_count: row.get(1)?,
                    latest_promotion_id: row.get(2)?,
                    latest_revision_id: row.get(3)?,
                })
            },
        )?
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
            revision_id: Some(c.latest_revision_id.clone()),
            ..Default::default()
        })
        .with_anchors(vec![
            EvidenceAnchor::PromotionRecord {
                promotion_id: c.latest_promotion_id.clone(),
            },
            EvidenceAnchor::RevisionId {
                id: c.latest_revision_id.clone(),
            },
        ]);
        findings.push(finding);
    }
    Ok(findings)
}

