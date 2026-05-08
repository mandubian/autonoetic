use super::GatewayStore;
use anyhow::Result;
use autonoetic_types::evaluation::{
    EvalCaseResultRecord, EvalRunRecord, EvalRunStatus, EvalSuiteRecord,
};
use rusqlite::{params, OptionalExtension};

fn parse_eval_run_status(status_str: &str) -> EvalRunStatus {
    match status_str {
        "Queued" => EvalRunStatus::Queued,
        "Running" => EvalRunStatus::Running,
        "Passed" => EvalRunStatus::Passed,
        "Failed" => EvalRunStatus::Failed,
        "Cancelled" => EvalRunStatus::Cancelled,
        _ => EvalRunStatus::Queued,
    }
}

impl GatewayStore {
    pub fn insert_eval_suite(&self, suite: &EvalSuiteRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let spec_json = serde_json::to_string(&suite.spec_json)?;
        let evaluated_targets_json = serde_json::to_string(&suite.evaluated_targets)?;
        conn.execute(
            "INSERT INTO eval_suites (
                suite_id, name, description, spec_json, created_at,
                created_by_type, created_by_id, origin_node_id,
                evaluated_targets_json, author_agent_id, based_on_suite_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                &suite.suite_id,
                &suite.name,
                &suite.description,
                spec_json,
                &suite.created_at,
                &suite.created_by_type,
                &suite.created_by_id,
                &suite.origin_node_id,
                evaluated_targets_json,
                suite.author_agent_id,
                suite.based_on_suite_id,
            ],
        )?;
        Ok(())
    }

    pub fn get_eval_suite(&self, suite_id: &str) -> Result<Option<EvalSuiteRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT suite_id, name, description, spec_json, created_at,
                    created_by_type, created_by_id, origin_node_id,
                    COALESCE(evaluated_targets_json, '[]'),
                    author_agent_id, based_on_suite_id
             FROM eval_suites WHERE suite_id = ?1",
        )?;
        let rows = stmt.query_map(params![suite_id], decode_eval_suite_row)?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results.pop())
    }

    /// List eval suites authored by the given agent ID.
    pub fn list_eval_suites_authored_by(&self, author_agent_id: &str) -> Result<Vec<EvalSuiteRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT suite_id, name, description, spec_json, created_at,
                    created_by_type, created_by_id, origin_node_id,
                    COALESCE(evaluated_targets_json, '[]'),
                    author_agent_id, based_on_suite_id
             FROM eval_suites WHERE author_agent_id = ?1
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![author_agent_id], decode_eval_suite_row)?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    /// List eval suites targeting the given agent ID in their evaluated_targets.
    /// Used by the sentinel to audit ownership invariants.
    pub fn list_eval_suites_targeting_agent(&self, agent_id: &str) -> Result<Vec<EvalSuiteRecord>> {
        let conn = self.conn.lock().unwrap();
        // JSON_EACH to check membership in the targets array.
        let mut stmt = conn.prepare(
            "SELECT DISTINCT s.suite_id, s.name, s.description, s.spec_json, s.created_at,
                    s.created_by_type, s.created_by_id, s.origin_node_id,
                    COALESCE(s.evaluated_targets_json, '[]'),
                    s.author_agent_id, s.based_on_suite_id
             FROM eval_suites s,
                  json_each(COALESCE(s.evaluated_targets_json, '[]')) AS t
             WHERE t.value = ?1
             ORDER BY s.created_at DESC",
        )?;
        let rows = stmt.query_map(params![agent_id], decode_eval_suite_row)?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn insert_eval_run(&self, run: &EvalRunRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let summary_json = serde_json::to_string(&run.summary_json)?;
        conn.execute(
            "INSERT INTO eval_runs (
                eval_run_id, suite_id, subject_agent_id, subject_revision_id,
                baseline_revision_id, status, queued_at, started_at, completed_at,
                summary_json, report_handle, origin_node_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                &run.eval_run_id,
                &run.suite_id,
                &run.subject_agent_id,
                &run.subject_revision_id,
                run.baseline_revision_id,
                &format!("{:?}", run.status),
                &run.queued_at,
                run.started_at,
                run.completed_at,
                summary_json,
                run.report_handle,
                &run.origin_node_id,
            ],
        )?;
        Ok(())
    }

    pub fn update_eval_run_status(
        &self,
        eval_run_id: &str,
        status: EvalRunStatus,
        completed_at: Option<&str>,
        summary_json: Option<&serde_json::Value>,
        report_handle: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let summary_json = if let Some(s) = summary_json {
            serde_json::to_string(s)?
        } else {
            "{}".to_string()
        };
        conn.execute(
            "UPDATE eval_runs SET status = ?1, completed_at = ?2, summary_json = ?3, report_handle = ?4
             WHERE eval_run_id = ?5",
            params![
                &format!("{:?}", status),
                completed_at.unwrap_or(&now),
                summary_json,
                report_handle,
                eval_run_id,
            ],
        )?;
        Ok(())
    }

    pub fn get_eval_run(&self, eval_run_id: &str) -> Result<Option<EvalRunRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT eval_run_id, suite_id, subject_agent_id, subject_revision_id,
                    baseline_revision_id, status, queued_at, started_at, completed_at,
                    summary_json, report_handle, origin_node_id
             FROM eval_runs WHERE eval_run_id = ?1",
        )?;
        let rows = stmt.query_map(params![eval_run_id], |row| {
            let status_str: String = row.get(5)?;
            let status = parse_eval_run_status(status_str.as_str());
            let summary_json: String = row.get(9)?;
            let summary_json =
                serde_json::from_str(&summary_json).unwrap_or(serde_json::Value::Null);
            Ok(EvalRunRecord {
                eval_run_id: row.get(0)?,
                suite_id: row.get(1)?,
                subject_agent_id: row.get(2)?,
                subject_revision_id: row.get(3)?,
                baseline_revision_id: row.get(4)?,
                status,
                queued_at: row.get(6)?,
                started_at: row.get(7)?,
                completed_at: row.get(8)?,
                summary_json,
                report_handle: row.get(10)?,
                origin_node_id: row.get(11)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results.pop())
    }

    pub fn find_latest_completed_eval_run(
        &self,
        suite_id: &str,
        subject_revision_id: &str,
    ) -> Result<Option<EvalRunRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT eval_run_id, suite_id, subject_agent_id, subject_revision_id,
                    baseline_revision_id, status, queued_at, started_at, completed_at,
                    summary_json, report_handle, origin_node_id
             FROM eval_runs
             WHERE suite_id = ?1
               AND subject_revision_id = ?2
               AND status IN ('Passed', 'Failed', 'Cancelled')
             ORDER BY COALESCE(completed_at, queued_at) DESC
             LIMIT 1",
        )?;
        let row = stmt
            .query_row(params![suite_id, subject_revision_id], |row| {
                let status_str: String = row.get(5)?;
                let status = parse_eval_run_status(status_str.as_str());
                let summary_json: String = row.get(9)?;
                let summary_json =
                    serde_json::from_str(&summary_json).unwrap_or(serde_json::Value::Null);
                Ok(EvalRunRecord {
                    eval_run_id: row.get(0)?,
                    suite_id: row.get(1)?,
                    subject_agent_id: row.get(2)?,
                    subject_revision_id: row.get(3)?,
                    baseline_revision_id: row.get(4)?,
                    status,
                    queued_at: row.get(6)?,
                    started_at: row.get(7)?,
                    completed_at: row.get(8)?,
                    summary_json,
                    report_handle: row.get(10)?,
                    origin_node_id: row.get(11)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn insert_eval_case_result(&self, result: &EvalCaseResultRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let output_json = serde_json::to_string(&result.output_json)?;
        conn.execute(
            "INSERT INTO eval_case_results (
                eval_run_id, case_id, status, score, session_id, notes, output_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &result.eval_run_id,
                &result.case_id,
                &result.status,
                result.score,
                result.session_id,
                result.notes,
                output_json,
            ],
        )?;
        Ok(())
    }

    pub fn list_eval_case_results(&self, eval_run_id: &str) -> Result<Vec<EvalCaseResultRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT eval_run_id, case_id, status, score, session_id, notes, output_json
             FROM eval_case_results WHERE eval_run_id = ?1",
        )?;
        let rows = stmt.query_map(params![eval_run_id], |row| {
            let output_json: String = row.get(6)?;
            let output_json = serde_json::from_str(&output_json).unwrap_or(serde_json::Value::Null);
            Ok(EvalCaseResultRecord {
                eval_run_id: row.get(0)?,
                case_id: row.get(1)?,
                status: row.get(2)?,
                score: row.get(3)?,
                session_id: row.get(4)?,
                notes: row.get(5)?,
                output_json,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn list_queued_eval_runs(&self) -> Result<Vec<EvalRunRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT eval_run_id, suite_id, subject_agent_id, subject_revision_id,
                    baseline_revision_id, status, queued_at, started_at, completed_at,
                    summary_json, report_handle, origin_node_id
             FROM eval_runs WHERE status = 'Queued' ORDER BY queued_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let status_str: String = row.get(5)?;
            let status = parse_eval_run_status(status_str.as_str());
            let summary_json: String = row.get(9)?;
            let summary_json =
                serde_json::from_str(&summary_json).unwrap_or(serde_json::Value::Null);
            Ok(EvalRunRecord {
                eval_run_id: row.get(0)?,
                suite_id: row.get(1)?,
                subject_agent_id: row.get(2)?,
                subject_revision_id: row.get(3)?,
                baseline_revision_id: row.get(4)?,
                status,
                queued_at: row.get(6)?,
                started_at: row.get(7)?,
                completed_at: row.get(8)?,
                summary_json,
                report_handle: row.get(10)?,
                origin_node_id: row.get(11)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn start_eval_run(&self, eval_run_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE eval_runs SET status = 'Running', started_at = ?1 WHERE eval_run_id = ?2",
            params![now, eval_run_id],
        )?;
        Ok(())
    }
}

fn decode_eval_suite_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EvalSuiteRecord> {
    let spec_json: String = row.get(3)?;
    let spec_json = serde_json::from_str(&spec_json).unwrap_or(serde_json::Value::Null);
    let targets_json: String = row.get(8)?;
    let evaluated_targets: Vec<String> =
        serde_json::from_str(&targets_json).unwrap_or_default();
    Ok(EvalSuiteRecord {
        suite_id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        spec_json,
        created_at: row.get(4)?,
        created_by_type: row.get(5)?,
        created_by_id: row.get(6)?,
        origin_node_id: row.get(7)?,
        evaluated_targets,
        author_agent_id: row.get(9)?,
        based_on_suite_id: row.get(10)?,
    })
}
