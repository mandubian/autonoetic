//! Constitutional amendment proposal persistence — R+++1 / Ri-0.8 (issue #92).
//!
//! Agents holding the `ConstitutionalProposal` capability submit proposals to
//! amend the gateway constitution. Proposals are durable (every proposal gets
//! an ID and cannot be silently dropped — Ri-0.8) and progress through a
//! state machine: `pending → under_review → (approved | rejected | deferred)`.
//! Approved proposals are queued by tag in `published_in_release` once the
//! operator runs a release.

use anyhow::Result;
use rusqlite::params;

use super::GatewayStore;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConstitutionalProposal {
    pub proposal_id: String,
    pub proposer_agent_id: String,
    pub proposer_session_id: Option<String>,
    pub kind: String,
    pub target_id: Option<String>,
    pub proposed_text: Option<String>,
    pub justification: String,
    pub evidence_json: serde_json::Value,
    pub status: String,
    pub operator_decision: Option<String>,
    pub decision_reason: Option<String>,
    pub decided_by: Option<String>,
    pub decided_at: Option<String>,
    pub published_in_release: Option<String>,
    pub created_at: String,
}

const PROPOSAL_COLUMNS: &str = "proposal_id, proposer_agent_id, proposer_session_id, kind, target_id, proposed_text, justification, evidence_json, status, operator_decision, decision_reason, decided_by, decided_at, published_in_release, created_at";

fn row_to_proposal(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConstitutionalProposal> {
    let evidence_str: String = row.get(7)?;
    let evidence_json = serde_json::from_str(&evidence_str).unwrap_or(serde_json::Value::Null);
    Ok(ConstitutionalProposal {
        proposal_id: row.get(0)?,
        proposer_agent_id: row.get(1)?,
        proposer_session_id: row.get(2)?,
        kind: row.get(3)?,
        target_id: row.get(4)?,
        proposed_text: row.get(5)?,
        justification: row.get(6)?,
        evidence_json,
        status: row.get(8)?,
        operator_decision: row.get(9)?,
        decision_reason: row.get(10)?,
        decided_by: row.get(11)?,
        decided_at: row.get(12)?,
        published_in_release: row.get(13)?,
        created_at: row.get(14)?,
    })
}

impl GatewayStore {
    pub fn insert_constitutional_proposal(&self, p: &ConstitutionalProposal) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let evidence_str = serde_json::to_string(&p.evidence_json)?;
        conn.execute(
            &format!(
                "INSERT INTO constitutional_proposals ({PROPOSAL_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"
            ),
            params![
                p.proposal_id,
                p.proposer_agent_id,
                p.proposer_session_id,
                p.kind,
                p.target_id,
                p.proposed_text,
                p.justification,
                evidence_str,
                p.status,
                p.operator_decision,
                p.decision_reason,
                p.decided_by,
                p.decided_at,
                p.published_in_release,
                p.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_constitutional_proposal(
        &self,
        proposal_id: &str,
    ) -> Result<Option<ConstitutionalProposal>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            &format!(
                "SELECT {PROPOSAL_COLUMNS} FROM constitutional_proposals WHERE proposal_id = ?1"
            ),
            params![proposal_id],
            row_to_proposal,
        );
        match result {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_constitutional_proposals(
        &self,
        status_filter: Option<&str>,
        proposer_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConstitutionalProposal>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = format!(
            "SELECT {PROPOSAL_COLUMNS} FROM constitutional_proposals WHERE 1=1"
        );
        let mut param_vals: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(sf) = status_filter {
            sql.push_str(" AND status = ?");
            param_vals.push(Box::new(sf.to_string()));
        }
        if let Some(pf) = proposer_filter {
            sql.push_str(" AND proposer_agent_id = ?");
            param_vals.push(Box::new(pf.to_string()));
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        param_vals.push(Box::new(limit as i64));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_vals.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), row_to_proposal)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Apply an operator decision to a proposal. The new status must be one of
    /// `under_review`, `approved`, `rejected`, `deferred`.
    pub fn decide_constitutional_proposal(
        &self,
        proposal_id: &str,
        new_status: &str,
        decided_by: &str,
        reason: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let rows = conn.execute(
            "UPDATE constitutional_proposals \
             SET status = ?1, operator_decision = ?1, decision_reason = ?2, decided_by = ?3, decided_at = ?4 \
             WHERE proposal_id = ?5",
            params![new_status, reason, decided_by, now, proposal_id],
        )?;
        Ok(rows > 0)
    }

    /// Mark every approved-but-unpublished proposal with the given release tag.
    /// Returns the proposal IDs that were marked (caller can record them in a
    /// release note). The constitution markdown is *not* mutated here — the
    /// operator edits the file by hand and the digest bumps naturally on
    /// rebuild via `include_str!`.
    pub fn publish_approved_proposals(&self, release_tag: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut select = conn.prepare(
            "SELECT proposal_id FROM constitutional_proposals \
             WHERE status = 'approved' AND published_in_release IS NULL",
        )?;
        let ids: Vec<String> = select
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(select);

        if ids.is_empty() {
            return Ok(ids);
        }

        conn.execute(
            "UPDATE constitutional_proposals \
             SET published_in_release = ?1 \
             WHERE status = 'approved' AND published_in_release IS NULL",
            params![release_tag],
        )?;
        Ok(ids)
    }
}
