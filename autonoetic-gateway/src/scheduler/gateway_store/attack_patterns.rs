use anyhow::Result;
use autonoetic_types::security::{AttackPatternStatus, ProposedAttackPattern};
use rusqlite::params;

use super::GatewayStore;

impl GatewayStore {
    pub fn insert_attack_pattern(&self, p: &ProposedAttackPattern) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO proposed_attack_patterns (
                pattern_id, proposed_by_agent_id, category, description,
                how_sentinel_should_catch, evidence_anchors_json, synthetic_test_case_json,
                status, accepted_check_type, operator_notes, created_at, reviewed_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                p.pattern_id,
                p.proposed_by_agent_id,
                p.category,
                p.description,
                p.how_sentinel_should_catch,
                p.evidence_anchors_json,
                p.synthetic_test_case_json,
                p.status.to_string(),
                p.accepted_check_type,
                p.operator_notes,
                p.created_at,
                p.reviewed_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_attack_pattern(&self, pattern_id: &str) -> Result<Option<ProposedAttackPattern>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT pattern_id, proposed_by_agent_id, category, description,
                    how_sentinel_should_catch, evidence_anchors_json, synthetic_test_case_json,
                    status, accepted_check_type, operator_notes, created_at, reviewed_at
             FROM proposed_attack_patterns WHERE pattern_id = ?1",
        )?;
        let rows = stmt.query_map(params![pattern_id], decode_attack_pattern_row)?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results.pop())
    }

    pub fn list_attack_patterns(
        &self,
        status: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ProposedAttackPattern>> {
        let conn = self.conn.lock().unwrap();
        let lim = limit as i64;
        let mut stmt = conn.prepare(
            "SELECT pattern_id, proposed_by_agent_id, category, description,
                    how_sentinel_should_catch, evidence_anchors_json, synthetic_test_case_json,
                    status, accepted_check_type, operator_notes, created_at, reviewed_at
             FROM proposed_attack_patterns
             WHERE (status = ?1 OR ?1 IS NULL)
             ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![status, lim], decode_attack_pattern_row)?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn review_attack_pattern(
        &self,
        pattern_id: &str,
        status: AttackPatternStatus,
        accepted_check_type: Option<&str>,
        operator_notes: Option<&str>,
    ) -> Result<()> {
        const VALID_CHECK_TYPES: &[&str] = &["phase1", "phase2"];

        let effective_check_type = match status {
            AttackPatternStatus::Accepted => {
                let ct = accepted_check_type.ok_or_else(|| {
                    anyhow::anyhow!(
                        "accepted_check_type is required when accepting a pattern (phase1 or phase2)"
                    )
                })?;
                anyhow::ensure!(
                    VALID_CHECK_TYPES.contains(&ct),
                    "invalid accepted_check_type '{}'; must be one of: {}",
                    ct,
                    VALID_CHECK_TYPES.join(", ")
                );
                Some(ct)
            }
            AttackPatternStatus::Rejected => None,
            AttackPatternStatus::Pending => {
                return Err(anyhow::anyhow!("cannot review a pattern back to Pending status"));
            }
        };

        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let updated = conn.execute(
            "UPDATE proposed_attack_patterns
             SET status = ?1, accepted_check_type = ?2, operator_notes = ?3, reviewed_at = ?4
             WHERE pattern_id = ?5",
            params![
                status.to_string(),
                effective_check_type,
                operator_notes,
                now,
                pattern_id,
            ],
        )?;
        anyhow::ensure!(updated == 1, "attack pattern not found: {}", pattern_id);
        Ok(())
    }
}

#[derive(Debug)]
struct UnknownStatusError(String);
impl std::fmt::Display for UnknownStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown attack pattern status: '{}'", self.0)
    }
}
impl std::error::Error for UnknownStatusError {}

fn decode_attack_pattern_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ProposedAttackPattern> {
    let status_str: String = row.get(7)?;
    let status = match status_str.as_str() {
        "pending" => AttackPatternStatus::Pending,
        "accepted" => AttackPatternStatus::Accepted,
        "rejected" => AttackPatternStatus::Rejected,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(UnknownStatusError(other.to_string())),
            ))
        }
    };
    Ok(ProposedAttackPattern {
        pattern_id: row.get(0)?,
        proposed_by_agent_id: row.get(1)?,
        category: row.get(2)?,
        description: row.get(3)?,
        how_sentinel_should_catch: row.get(4)?,
        evidence_anchors_json: row.get(5)?,
        synthetic_test_case_json: row.get(6)?,
        status,
        accepted_check_type: row.get(8)?,
        operator_notes: row.get(9)?,
        created_at: row.get(10)?,
        reviewed_at: row.get(11)?,
    })
}
