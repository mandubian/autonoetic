use anyhow::Result;
use rusqlite::params;

use autonoetic_types::improvement_cycle::{
    CycleOutcome, ImprovementCycleRecord, ImprovementLevel,
};

pub(crate) const CYCLE_COLUMNS: &str = "\
    cycle_id, agent_id, level, outcome, regression_detected, \
    operator_decision, session_id, revision_before, revision_after, \
    blast_radius_score, created_at, closed_at";

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImprovementCycleRecord> {
    let level_str: String = row.get(2)?;
    let outcome_str: String = row.get(3)?;
    let level = level_str.parse::<ImprovementLevel>().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::<dyn std::error::Error + Send + Sync>::from(format!("invalid ImprovementLevel: {}", level_str)))
    })?;
    let outcome = outcome_str.parse::<CycleOutcome>().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::<dyn std::error::Error + Send + Sync>::from(format!("invalid CycleOutcome: {}", outcome_str)))
    })?;
    Ok(ImprovementCycleRecord {
        cycle_id: row.get(0)?,
        agent_id: row.get(1)?,
        level,
        outcome,
        regression_detected: row.get::<_, i64>(4)? != 0,
        operator_decision: row.get(5)?,
        session_id: row.get(6)?,
        revision_before: row.get(7)?,
        revision_after: row.get(8)?,
        blast_radius_score: row.get(9)?,
        created_at: row.get(10)?,
        closed_at: row.get(11)?,
    })
}

impl super::GatewayStore {
    pub fn insert_improvement_cycle(&self, record: &ImprovementCycleRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!("INSERT INTO improvement_cycles ({}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)", CYCLE_COLUMNS),
            params![
                record.cycle_id,
                record.agent_id,
                record.level.to_string(),
                record.outcome.to_string(),
                record.regression_detected as i64,
                record.operator_decision,
                record.session_id,
                record.revision_before,
                record.revision_after,
                record.blast_radius_score,
                record.created_at,
                record.closed_at,
            ],
        )?;
        Ok(())
    }

    pub fn close_improvement_cycle(
        &self,
        cycle_id: &str,
        outcome: &CycleOutcome,
        regression_detected: bool,
        operator_decision: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE improvement_cycles SET outcome = ?1, regression_detected = ?2, operator_decision = ?3, closed_at = ?4 WHERE cycle_id = ?5",
            params![
                outcome.to_string(),
                regression_detected as i64,
                operator_decision,
                chrono::Utc::now().to_rfc3339(),
                cycle_id,
            ],
        )?;
        anyhow::ensure!(rows == 1, "close_improvement_cycle: cycle '{}' not found or already closed", cycle_id);
        Ok(())
    }

    pub fn get_improvement_cycle(&self, cycle_id: &str) -> Result<Option<ImprovementCycleRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("SELECT {} FROM improvement_cycles WHERE cycle_id = ?1", CYCLE_COLUMNS))?;
        let mut rows = stmt.query(params![cycle_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_record(row)?)),
            None => Ok(None),
        }
    }

    pub fn list_improvement_cycles_for_agent(
        &self,
        agent_id: &str,
        level: Option<&ImprovementLevel>,
        limit: i64,
    ) -> Result<Vec<ImprovementCycleRecord>> {
        let conn = self.conn.lock().unwrap();
        let query = if level.is_some() {
            format!(
                "SELECT {} FROM improvement_cycles WHERE agent_id = ?1 AND level = ?2 ORDER BY created_at DESC LIMIT ?3",
                CYCLE_COLUMNS
            )
        } else {
            format!(
                "SELECT {} FROM improvement_cycles WHERE agent_id = ?1 ORDER BY created_at DESC LIMIT ?2",
                CYCLE_COLUMNS
            )
        };
        let mut stmt = conn.prepare(&query)?;
        let mut rows = if let Some(lvl) = level {
            stmt.query(params![agent_id, lvl.to_string(), limit])?
        } else {
            stmt.query(params![agent_id, limit])?
        };
        let mut results = Vec::new();
        while let Some(row) = rows.next()? {
            results.push(row_to_record(row)?);
        }
        Ok(results)
    }

    pub fn count_successful_cycles(
        &self,
        agent_id: &str,
        level: &ImprovementLevel,
    ) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM improvement_cycles \
             WHERE agent_id = ?1 AND level = ?2 AND outcome = 'success' AND regression_detected = 0 AND closed_at IS NOT NULL",
            params![agent_id, level.to_string()],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn check_automation_level_eligibility(
        &self,
        agent_id: &str,
        target_level: &ImprovementLevel,
        l2_threshold: u64,
        l3_threshold: u64,
    ) -> Result<bool> {
        match target_level {
            ImprovementLevel::L1 => Ok(true),
            ImprovementLevel::L2 => {
                let count = self.count_successful_cycles(agent_id, &ImprovementLevel::L1)?;
                Ok(count >= l2_threshold)
            }
            ImprovementLevel::L3 => {
                let l2_count = self.count_successful_cycles(agent_id, &ImprovementLevel::L2)?;
                Ok(l2_count >= l3_threshold)
            }
        }
    }
}
