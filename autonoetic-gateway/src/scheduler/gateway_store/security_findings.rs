//! Append-only store for `SecurityFinding` records.
//!
//! The `security_findings` table is append-only by contract: once a row is
//! inserted, only the `triage_state` and `triage_reason` columns may be
//! updated by operators. The finding body itself is never modified.

use anyhow::Result;
use autonoetic_types::security::{SecurityFinding, TriageState};
use rusqlite::params;

use super::GatewayStore;

const SELECT_FINDING_COLUMNS: &str =
    "SELECT finding_id, severity, confidence, finding_type,
            affected_json, evidence_json, reproducibility,
            proposed_remediation, sentinel_revision_id,
            baseline_agreed, ensemble_agreed,
            triage_state, triage_reason, created_at
     FROM security_findings";

impl GatewayStore {
    /// Persist a new `SecurityFinding`. Returns an error if the finding_id
    /// already exists (the table is append-only).
    pub fn insert_security_finding(&self, finding: &SecurityFinding) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        let affected_json = serde_json::to_string(&finding.affected)?;
        let evidence_json = serde_json::to_string(&finding.evidence_anchors)?;
        let severity = finding.severity.to_string();
        let finding_type = finding.finding_type.to_string();
        let reproducibility = serde_json::to_value(&finding.reproducibility)?
            .as_str()
            .unwrap_or("deterministic")
            .to_string();
        let ensemble_agreed: Option<i64> = finding.ensemble_agreed.map(|b| if b { 1 } else { 0 });

        conn.execute(
            "INSERT INTO security_findings (
                finding_id, severity, confidence, finding_type,
                affected_json, evidence_json, reproducibility,
                proposed_remediation, sentinel_revision_id,
                baseline_agreed, ensemble_agreed,
                triage_state, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                finding.finding_id,
                severity,
                finding.confidence,
                finding_type,
                affected_json,
                evidence_json,
                reproducibility,
                finding.proposed_remediation,
                finding.sentinel_revision_id,
                finding.baseline_agreed as i64,
                ensemble_agreed,
                TriageState::Pending.to_string(),
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Update the triage state for a finding. This is the *only* mutation
    /// allowed on a persisted finding.
    pub fn update_security_finding_triage(
        &self,
        finding_id: &str,
        state: TriageState,
        reason: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE security_findings SET triage_state = ?1, triage_reason = ?2
             WHERE finding_id = ?3",
            params![state.to_string(), reason, finding_id],
        )?;
        anyhow::ensure!(updated == 1, "finding not found: {}", finding_id);
        Ok(())
    }

    /// List all pending findings (most recent first, up to `limit`).
    pub fn list_pending_security_findings(&self, limit: u32) -> Result<Vec<SecurityFindingRow>> {
        self.list_security_findings_inner(None, Some("pending"), limit)
    }

    /// List findings filtered by optional severity and/or triage state.
    pub fn list_security_findings(
        &self,
        severity: Option<&str>,
        triage_state: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SecurityFindingRow>> {
        self.list_security_findings_inner(severity, triage_state, limit)
    }

    fn list_security_findings_inner(
        &self,
        severity: Option<&str>,
        triage_state: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SecurityFindingRow>> {
        let conn = self.conn.lock().unwrap();
        let lim = limit as i64;

        let rows = match (severity, triage_state) {
            (None, None) => {
                let mut stmt = conn.prepare(&format!(
                    "{} ORDER BY created_at DESC LIMIT ?1",
                    SELECT_FINDING_COLUMNS
                ))?;
                let result = stmt
                    .query_map(params![lim], decode_finding_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                result
            }
            (Some(sev), None) => {
                let mut stmt = conn.prepare(&format!(
                    "{} WHERE severity = ?1 ORDER BY created_at DESC LIMIT ?2",
                    SELECT_FINDING_COLUMNS
                ))?;
                let result = stmt
                    .query_map(params![sev, lim], decode_finding_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                result
            }
            (None, Some(ts)) => {
                let mut stmt = conn.prepare(&format!(
                    "{} WHERE triage_state = ?1 ORDER BY created_at DESC LIMIT ?2",
                    SELECT_FINDING_COLUMNS
                ))?;
                let result = stmt
                    .query_map(params![ts, lim], decode_finding_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                result
            }
            (Some(sev), Some(ts)) => {
                let mut stmt = conn.prepare(&format!(
                    "{} WHERE severity = ?1 AND triage_state = ?2 ORDER BY created_at DESC LIMIT ?3",
                    SELECT_FINDING_COLUMNS
                ))?;
                let result = stmt
                    .query_map(params![sev, ts, lim], decode_finding_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                result
            }
        };
        Ok(rows)
    }

    /// Count pending findings grouped by severity.
    pub fn count_pending_security_findings_by_severity(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT severity, COUNT(*) FROM security_findings
             WHERE triage_state = 'pending'
             GROUP BY severity ORDER BY severity",
        )?;
        let result = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }

    /// Count all findings grouped by triage state.
    pub fn count_security_findings_by_triage_state(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT triage_state, COUNT(*) FROM security_findings
             GROUP BY triage_state ORDER BY triage_state",
        )?;
        let result = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }

    /// List findings with optional severity, finding_type, and triage_state filters.
    ///
    /// Uses the `(col = ? OR ? IS NULL)` pattern so all three filters share a
    /// single prepared statement with fixed parameter arity.
    pub fn list_security_findings_filtered(
        &self,
        severity: Option<&str>,
        finding_type: Option<&str>,
        triage_state: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SecurityFindingRow>> {
        let conn = self.conn.lock().unwrap();
        let lim = limit as i64;
        let mut stmt = conn.prepare(&format!(
            "{} WHERE (severity = ?1 OR ?1 IS NULL)
               AND (finding_type = ?2 OR ?2 IS NULL)
               AND (triage_state = ?3 OR ?3 IS NULL)
             ORDER BY created_at DESC LIMIT ?4",
            SELECT_FINDING_COLUMNS
        ))?;
        let result = stmt
            .query_map(
                rusqlite::params![severity, finding_type, triage_state, lim],
                decode_finding_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }
}

fn decode_finding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SecurityFindingRow> {
    Ok(SecurityFindingRow {
        finding_id: row.get(0)?,
        severity: row.get(1)?,
        confidence: row.get(2)?,
        finding_type: row.get(3)?,
        affected_json: row.get(4)?,
        evidence_json: row.get(5)?,
        reproducibility: row.get(6)?,
        proposed_remediation: row.get(7)?,
        sentinel_revision_id: row.get(8)?,
        baseline_agreed: row.get::<_, i64>(9)? != 0,
        ensemble_agreed: row.get::<_, Option<i64>>(10)?.map(|v| v != 0),
        triage_state: row.get(11)?,
        triage_reason: row.get(12)?,
        created_at: row.get(13)?,
    })
}

/// A raw row from the `security_findings` table, with JSON columns still as strings.
#[derive(Debug, Clone)]
pub struct SecurityFindingRow {
    pub finding_id: String,
    pub severity: String,
    pub confidence: f64,
    pub finding_type: String,
    pub affected_json: String,
    pub evidence_json: String,
    pub reproducibility: String,
    pub proposed_remediation: String,
    pub sentinel_revision_id: String,
    pub baseline_agreed: bool,
    pub ensemble_agreed: Option<bool>,
    pub triage_state: String,
    pub triage_reason: Option<String>,
    pub created_at: String,
}
