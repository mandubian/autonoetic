//! Store methods for `security_sentinel_disagreements`.
//!
//! Disagreements are recorded when the frozen baseline sentinel and the current
//! sentinel diverge: the baseline finds something the current missed, or the
//! current (Phase-1 only) finds something the baseline missed. Phase-2 findings
//! are expected to diverge from the deterministic baseline and are excluded from
//! disagreement comparison.

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::GatewayStore;

/// Direction of a sentinel disagreement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisagreementDirection {
    /// The frozen baseline flagged this anchor; the current sentinel did not.
    BaselineOnly,
    /// The current sentinel flagged this anchor (Phase 1); the baseline did not.
    CurrentOnly,
}

impl std::fmt::Display for DisagreementDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisagreementDirection::BaselineOnly => write!(f, "baseline_only"),
            DisagreementDirection::CurrentOnly => write!(f, "current_only"),
        }
    }
}

/// A record of sentinel divergence between the frozen baseline and the current sentinel.
#[derive(Debug, Clone)]
pub struct SentinelDisagreementRecord {
    pub disagreement_id: String,
    pub sweep_at: String,
    pub direction: DisagreementDirection,
    /// JSON of the `EvidenceAnchor` that the diverging sentinel flagged.
    pub anchor_json: String,
    /// The `finding_id` from the baseline sweep (None when `direction = current_only`).
    pub baseline_finding_id: Option<String>,
    /// The `finding_id` from the current sweep (None when `direction = baseline_only`).
    pub current_finding_id: Option<String>,
    pub baseline_sentinel_rev: String,
    pub current_sentinel_rev: String,
}

impl GatewayStore {
    /// Persist a new sentinel disagreement record.
    pub fn insert_sentinel_disagreement(&self, rec: &SentinelDisagreementRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO security_sentinel_disagreements (
                disagreement_id, sweep_at, direction, anchor_json,
                baseline_finding_id, current_finding_id,
                baseline_sentinel_rev, current_sentinel_rev, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                rec.disagreement_id,
                rec.sweep_at,
                rec.direction.to_string(),
                rec.anchor_json,
                rec.baseline_finding_id,
                rec.current_finding_id,
                rec.baseline_sentinel_rev,
                rec.current_sentinel_rev,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// List disagreements since the given RFC-3339 timestamp, newest first.
    pub fn list_sentinel_disagreements(
        &self,
        since: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SentinelDisagreementRecord>> {
        let conn = self.conn.lock().unwrap();
        let lim = limit as i64;

        let rows = if let Some(since) = since {
            let mut stmt = conn.prepare(
                "SELECT disagreement_id, sweep_at, direction, anchor_json,
                        baseline_finding_id, current_finding_id,
                        baseline_sentinel_rev, current_sentinel_rev
                 FROM security_sentinel_disagreements
                 WHERE sweep_at >= ?1
                 ORDER BY created_at DESC LIMIT ?2",
            )?;
            let result = stmt.query_map(params![since, lim], decode_disagreement_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            result
        } else {
            let mut stmt = conn.prepare(
                "SELECT disagreement_id, sweep_at, direction, anchor_json,
                        baseline_finding_id, current_finding_id,
                        baseline_sentinel_rev, current_sentinel_rev
                 FROM security_sentinel_disagreements
                 ORDER BY created_at DESC LIMIT ?1",
            )?;
            let result = stmt.query_map(params![lim], decode_disagreement_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            result
        };
        Ok(rows)
    }

    /// Count disagreements grouped by direction for monitoring.
    pub fn count_sentinel_disagreements_by_direction(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT direction, COUNT(*) FROM security_sentinel_disagreements
             GROUP BY direction ORDER BY direction",
        )?;
        let result = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }
}

fn decode_disagreement_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<SentinelDisagreementRecord> {
    let direction_str: String = row.get(2)?;
    let direction = match direction_str.as_str() {
        "baseline_only" => DisagreementDirection::BaselineOnly,
        _ => DisagreementDirection::CurrentOnly,
    };
    Ok(SentinelDisagreementRecord {
        disagreement_id: row.get(0)?,
        sweep_at: row.get(1)?,
        direction,
        anchor_json: row.get(3)?,
        baseline_finding_id: row.get(4)?,
        current_finding_id: row.get(5)?,
        baseline_sentinel_rev: row.get(6)?,
        current_sentinel_rev: row.get(7)?,
    })
}
