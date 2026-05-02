use anyhow::Result;
use rusqlite::{params, Connection};

use super::GatewayStore;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdminProposal {
    pub proposal_id: String,
    pub title: String,
    pub category: String,
    pub evidence_json: serde_json::Value,
    pub remediation: String,
    pub blast_radius: String,
    pub priority: String,
    pub created_by: String,
    pub created_at: String,
    pub status: String,
    pub triaged_by: Option<String>,
    pub triaged_at: Option<String>,
    pub decision_reason: Option<String>,
}

fn row_to_proposal(row: &rusqlite::Row<'_>) -> rusqlite::Result<AdminProposal> {
    let evidence_str: String = row.get(3)?;
    let evidence_json = serde_json::from_str(&evidence_str).unwrap_or(serde_json::Value::Null);
    Ok(AdminProposal {
        proposal_id: row.get(0)?,
        title: row.get(1)?,
        category: row.get(2)?,
        evidence_json,
        remediation: row.get(4)?,
        blast_radius: row.get(5)?,
        priority: row.get(6)?,
        created_by: row.get(7)?,
        created_at: row.get(8)?,
        status: row.get(9)?,
        triaged_by: row.get(10)?,
        triaged_at: row.get(11)?,
        decision_reason: row.get(12)?,
    })
}

const PROPOSAL_COLUMNS: &str = "proposal_id, title, category, evidence_json, remediation, blast_radius, priority, created_by, created_at, status, triaged_by, triaged_at, decision_reason";

impl GatewayStore {
    pub fn upsert_admin_proposal(&self, p: &AdminProposal) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let evidence_str = serde_json::to_string(&p.evidence_json)?;
        conn.execute(
            &format!("INSERT OR REPLACE INTO admin_proposals ({PROPOSAL_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"),
            params![
                p.proposal_id,
                p.title,
                p.category,
                evidence_str,
                p.remediation,
                p.blast_radius,
                p.priority,
                p.created_by,
                p.created_at,
                p.status,
                p.triaged_by,
                p.triaged_at,
                p.decision_reason,
            ],
        )?;
        Ok(())
    }

    pub fn insert_admin_proposal(&self, p: &AdminProposal) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let evidence_str = serde_json::to_string(&p.evidence_json)?;
        conn.execute(
            &format!("INSERT INTO admin_proposals ({PROPOSAL_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"),
            params![
                p.proposal_id,
                p.title,
                p.category,
                evidence_str,
                p.remediation,
                p.blast_radius,
                p.priority,
                p.created_by,
                p.created_at,
                p.status,
                p.triaged_by,
                p.triaged_at,
                p.decision_reason,
            ],
        )?;
        Ok(())
    }

    pub fn list_admin_proposals(
        &self,
        status_filter: Option<&str>,
        category_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AdminProposal>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = format!("SELECT {PROPOSAL_COLUMNS} FROM admin_proposals WHERE 1=1");
        let mut param_vals: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(sf) = status_filter {
            sql.push_str(" AND status = ?");
            param_vals.push(Box::new(sf.to_string()));
        }
        if let Some(cf) = category_filter {
            sql.push_str(" AND category = ?");
            param_vals.push(Box::new(cf.to_string()));
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        param_vals.push(Box::new(limit as i64));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_vals.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| row_to_proposal(row))?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn get_admin_proposal(&self, proposal_id: &str) -> Result<Option<AdminProposal>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            &format!("SELECT {PROPOSAL_COLUMNS} FROM admin_proposals WHERE proposal_id = ?1"),
            params![proposal_id],
            |row| row_to_proposal(row),
        );
        match result {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn update_admin_proposal_status(
        &self,
        proposal_id: &str,
        new_status: &str,
        triaged_by: &str,
        decision_reason: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let rows = conn.execute(
            "UPDATE admin_proposals SET status = ?1, triaged_by = ?2, triaged_at = ?3, decision_reason = ?4 WHERE proposal_id = ?5",
            params![new_status, triaged_by, now, decision_reason, proposal_id],
        )?;
        Ok(rows > 0)
    }

    pub fn find_open_proposals_by_title_category(
        &self,
        title: &str,
        category: &str,
    ) -> Result<Vec<AdminProposal>> {
        let conn = self.conn.lock().unwrap();
        let pattern = format!("%{}%", title.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(
            &format!("SELECT {PROPOSAL_COLUMNS} FROM admin_proposals WHERE status = 'open' AND category = ?1 AND title LIKE ?2 ESCAPE '\\' LIMIT 10"),
        )?;
        let rows = stmt.query_map(params![category, pattern], |row| row_to_proposal(row))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }
}
