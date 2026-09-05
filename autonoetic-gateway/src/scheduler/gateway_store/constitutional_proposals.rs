//! Constitutional amendment proposal persistence — Ri-0.8 (issue #92).
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

/// Every status a decider may move a proposal to via
/// [`GatewayStore::decide_constitutional_proposal`]. The single source of
/// truth shared by the JSON-RPC (`constitution.resolve_proposal`) and the CLI
/// (`gateway constitution proposal …`) so the two can never drift. Mirrors
/// the state machine in the module docs and the constitution's O-6 vocabulary
/// (`approved`/`rejected`/`deferred`/`under_review`).
pub const PROPOSAL_DECISION_STATUSES: &[&str] =
    &["approved", "rejected", "deferred", "under_review"];

/// Terminal proposal decisions — the ones that stamp `decided_at` and the
/// decision fields. `under_review` is excluded: it is a non-terminal
/// review-start transition that only updates `status` (see
/// [`GatewayStore::decide_constitutional_proposal`]). The CLI exposes only
/// these as subcommands.
pub const PROPOSAL_TERMINAL_DECISION_STATUSES: &[&str] = &["approved", "rejected", "deferred"];

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
    /// Stamped once by [`GatewayStore::flag_proposal_sla_breaches`] when the
    /// proposal sits un-adjudicated past the configured SLA (O-6). `None`
    /// means either not yet overdue or the SLA check hasn't run.
    pub sla_breached_at: Option<String>,
    /// The candidate constitution version this proposal was drafted into by
    /// the amendment materializer (#810). `None` until
    /// [`GatewayStore::mark_proposals_materialized`] stamps it. Separate from
    /// `published_in_release`: drafting a candidate directory and labelling a
    /// release batch are different acts in the ceremony.
    pub materialized_in_version: Option<String>,
}

const PROPOSAL_COLUMNS: &str = "proposal_id, proposer_agent_id, proposer_session_id, kind, target_id, proposed_text, justification, evidence_json, status, operator_decision, decision_reason, decided_by, decided_at, published_in_release, created_at, sla_breached_at, materialized_in_version";

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
        sla_breached_at: row.get(15)?,
        materialized_in_version: row.get(16)?,
    })
}

impl GatewayStore {
    pub fn insert_constitutional_proposal(&self, p: &ConstitutionalProposal) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let evidence_str = serde_json::to_string(&p.evidence_json)?;
        conn.execute(
            &format!(
                "INSERT INTO constitutional_proposals ({PROPOSAL_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"
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
                // sla_breached_at: always NULL at insert; stamped later by
                // flag_proposal_sla_breaches, never at filing time.
                None::<String>,
                // materialized_in_version: always NULL at insert; stamped by
                // mark_proposals_materialized when the materializer drafts the
                // candidate version (#810).
                None::<String>,
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
        let mut sql = format!("SELECT {PROPOSAL_COLUMNS} FROM constitutional_proposals WHERE 1=1");
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

    /// List only **non-terminal** proposals (`pending`, `under_review`) — the
    /// ones still awaiting a decision. Used by the signed per-turn state
    /// attestation (#772 A.2): the status filter must happen in SQL, before
    /// the `LIMIT`, so newer terminal decisions can never displace older
    /// still-pending proposals from the bounded query window.
    pub fn list_pending_constitutional_proposals(
        &self,
        proposer_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ConstitutionalProposal>> {
        let conn = self.conn.lock().unwrap();
        let placeholders = PROPOSAL_TERMINAL_DECISION_STATUSES
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!(
            "SELECT {PROPOSAL_COLUMNS} FROM constitutional_proposals \
             WHERE status NOT IN ({placeholders})"
        );
        let mut param_vals: Vec<Box<dyn rusqlite::types::ToSql>> =
            PROPOSAL_TERMINAL_DECISION_STATUSES
                .iter()
                .map(|s| Box::new(s.to_string()) as Box<dyn rusqlite::types::ToSql>)
                .collect();
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

    /// Apply a status transition to a proposal.
    ///
    /// Terminal decisions (`approved`, `rejected`, `deferred`) stamp
    /// `operator_decision`, `decision_reason`, `decided_by`, and
    /// `decided_at`. `under_review` is a non-terminal review-start
    /// transition and only updates `status` — it does not record a
    /// "decision" timestamp.
    pub fn decide_constitutional_proposal(
        &self,
        proposal_id: &str,
        new_status: &str,
        decided_by: &str,
        reason: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let rows = if new_status == "under_review" {
            conn.execute(
                "UPDATE constitutional_proposals \
                 SET status = ?1 \
                 WHERE proposal_id = ?2",
                params![new_status, proposal_id],
            )?
        } else {
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE constitutional_proposals \
                 SET status = ?1, operator_decision = ?1, decision_reason = ?2, decided_by = ?3, decided_at = ?4 \
                 WHERE proposal_id = ?5",
                params![new_status, reason, decided_by, now, proposal_id],
            )?
        };
        Ok(rows > 0)
    }

    /// Mark every approved-but-unpublished proposal with the given release tag.
    /// Returns the proposal IDs that were marked (caller can record them in a
    /// release note). This stamps a label only — the mechanical drafting of
    /// the markdown is the amendment materializer's job (#810,
    /// `constitution_materializer.rs` / `gateway constitution materialize`).
    ///
    /// Atomic via `UPDATE … RETURNING` so the returned list is exactly the
    /// rows this call mutated, even under concurrent operators.
    pub fn publish_approved_proposals(&self, release_tag: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut update = conn.prepare(
            "UPDATE constitutional_proposals \
             SET published_in_release = ?1 \
             WHERE status = 'approved' AND published_in_release IS NULL \
             RETURNING proposal_id",
        )?;
        let ids: Vec<String> = update
            .query_map(params![release_tag], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// Every **approved but not yet materialized** proposal, oldest first —
    /// the work queue for the amendment materializer (#810). Oldest-first so
    /// a candidate version applies amendments in the order they were approved.
    pub fn list_approved_unmaterialized_proposals(&self) -> Result<Vec<ConstitutionalProposal>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {PROPOSAL_COLUMNS} FROM constitutional_proposals \
             WHERE status = 'approved' AND materialized_in_version IS NULL \
             ORDER BY created_at ASC, proposal_id ASC"
        ))?;
        let rows = stmt.query_map([], row_to_proposal)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Stamp the candidate constitution version a proposal was materialized
    /// into (#810). Returns the IDs actually stamped. Atomic via
    /// `UPDATE … RETURNING` with a `materialized_in_version IS NULL` guard,
    /// so a proposal is stamped exactly once even if two operators race —
    /// mirrors [`GatewayStore::publish_approved_proposals`].
    pub fn mark_proposals_materialized(
        &self,
        proposal_ids: &[String],
        version: &str,
    ) -> Result<Vec<String>> {
        if proposal_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let placeholders = proposal_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "UPDATE constitutional_proposals \
             SET materialized_in_version = ?1 \
             WHERE materialized_in_version IS NULL AND proposal_id IN ({placeholders}) \
             RETURNING proposal_id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut param_vals: Vec<&dyn rusqlite::types::ToSql> = vec![&version];
        for id in proposal_ids {
            param_vals.push(id);
        }
        let stamped = stmt
            .query_map(param_vals.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(stamped)
    }

    /// Stamp `sla_breached_at` on proposals overdue for adjudication, returning
    /// the rows first breached by THIS call (so the caller emits one event +
    /// notification per breach, never repeating on later ticks). A breach does
    /// NOT change status — the decision is still owed (O-6).
    pub fn flag_proposal_sla_breaches(
        &self,
        sla_secs: u64,
        now_rfc3339: &str,
    ) -> Result<Vec<ConstitutionalProposal>> {
        let now = chrono::DateTime::parse_from_rfc3339(now_rfc3339)
            .map_err(|e| anyhow::anyhow!("invalid `now_rfc3339` {now_rfc3339:?}: {e}"))?
            .with_timezone(&chrono::Utc);
        let cutoff = (now - chrono::Duration::seconds(sla_secs as i64)).to_rfc3339();

        let terminal_placeholders = PROPOSAL_TERMINAL_DECISION_STATUSES
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");

        // Single atomic UPDATE … RETURNING: `sla_breached_at IS NULL` in the
        // WHERE clause guarantees stamp-once even under concurrent schedulers —
        // only the caller whose UPDATE actually flips the row from NULL gets it
        // back, so a breach fires exactly once (mirrors publish_approved_proposals).
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "UPDATE constitutional_proposals SET sla_breached_at = ? \
             WHERE sla_breached_at IS NULL \
               AND status NOT IN ({terminal_placeholders}) \
               AND created_at < ? \
             RETURNING {PROPOSAL_COLUMNS}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut param_vals: Vec<&dyn rusqlite::types::ToSql> = vec![&now_rfc3339];
        for s in PROPOSAL_TERMINAL_DECISION_STATUSES {
            param_vals.push(s as &dyn rusqlite::types::ToSql);
        }
        param_vals.push(&cutoff);
        let breached = stmt
            .query_map(param_vals.as_slice(), row_to_proposal)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(breached)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_proposal(proposal_id: &str, created_at: &str, status: &str) -> ConstitutionalProposal {
        ConstitutionalProposal {
            proposal_id: proposal_id.to_string(),
            proposer_agent_id: "auditor.default".to_string(),
            proposer_session_id: Some("sess-1".to_string()),
            kind: "add_right".to_string(),
            target_id: None,
            proposed_text: Some("Agents may do X".to_string()),
            justification: "closes a gap".to_string(),
            evidence_json: serde_json::json!([]),
            status: status.to_string(),
            operator_decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            published_in_release: None,
            created_at: created_at.to_string(),
            sla_breached_at: None,
            materialized_in_version: None,
        }
    }

    fn old_rfc3339(secs_ago: u64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs_ago as i64)).to_rfc3339()
    }

    #[test]
    fn breach_is_flagged_once_and_idempotent() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_constitutional_proposal(&sample_proposal(
            "cprop-old",
            &old_rfc3339(1_000),
            "pending",
        ))?;

        let now = chrono::Utc::now().to_rfc3339();
        let first = store.flag_proposal_sla_breaches(100, &now)?;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].proposal_id, "cprop-old");
        assert!(first[0].sla_breached_at.is_some());
        assert_eq!(first[0].status, "pending", "breach must not change status");

        // Second tick: already flagged, must not repeat.
        let second = store.flag_proposal_sla_breaches(100, &now)?;
        assert!(second.is_empty());

        Ok(())
    }

    #[test]
    fn within_sla_is_not_flagged() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_constitutional_proposal(&sample_proposal(
            "cprop-fresh",
            &old_rfc3339(10),
            "pending",
        ))?;

        let now = chrono::Utc::now().to_rfc3339();
        let breached = store.flag_proposal_sla_breaches(1_000, &now)?;
        assert!(breached.is_empty());

        Ok(())
    }

    #[test]
    fn terminal_status_is_never_flagged() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_constitutional_proposal(&sample_proposal(
            "cprop-approved",
            &old_rfc3339(1_000),
            "approved",
        ))?;

        let now = chrono::Utc::now().to_rfc3339();
        let breached = store.flag_proposal_sla_breaches(100, &now)?;
        assert!(breached.is_empty());

        Ok(())
    }

    #[test]
    fn under_review_old_proposal_is_flagged() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_constitutional_proposal(&sample_proposal(
            "cprop-review",
            &old_rfc3339(1_000),
            "under_review",
        ))?;

        let now = chrono::Utc::now().to_rfc3339();
        let breached = store.flag_proposal_sla_breaches(100, &now)?;
        assert_eq!(breached.len(), 1);
        assert_eq!(breached[0].proposal_id, "cprop-review");

        Ok(())
    }

    #[test]
    fn list_pending_excludes_terminal_and_resists_displacement() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        store.insert_constitutional_proposal(&sample_proposal(
            "prop-pending",
            "2025-01-01T00:00:00Z",
            "pending",
        ))?;
        store.insert_constitutional_proposal(&sample_proposal(
            "prop-review",
            "2025-01-02T00:00:00Z",
            "under_review",
        ))?;
        // Newest row is a terminal decision: an all-statuses query with a
        // tight LIMIT would crowd the older pending rows out of the window.
        store.insert_constitutional_proposal(&sample_proposal(
            "prop-approved",
            "2025-01-03T00:00:00Z",
            "approved",
        ))?;

        let listed = store.list_pending_constitutional_proposals(Some("auditor.default"), 64)?;
        let ids: Vec<&str> = listed.iter().map(|p| p.proposal_id.as_str()).collect();
        assert_eq!(ids, vec!["prop-review", "prop-pending"]);

        // Even with a limit below the total row count, terminal decisions
        // never displace still-pending proposals (SQL-level status filter).
        let tight = store.list_pending_constitutional_proposals(Some("auditor.default"), 1)?;
        assert_eq!(tight.len(), 1);
        assert_eq!(tight[0].proposal_id, "prop-review");

        Ok(())
    }

    #[test]
    fn materialized_queue_is_approved_only_and_oldest_first() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        // Approved oldest → newest, one still-pending, one already materialized.
        store.insert_constitutional_proposal(&sample_proposal(
            "cprop-a",
            "2025-01-01T00:00:00Z",
            "approved",
        ))?;
        store.insert_constitutional_proposal(&sample_proposal(
            "cprop-pending",
            "2025-01-02T00:00:00Z",
            "pending",
        ))?;
        store.insert_constitutional_proposal(&sample_proposal(
            "cprop-b",
            "2025-01-03T00:00:00Z",
            "approved",
        ))?;
        store.insert_constitutional_proposal(&sample_proposal(
            "cprop-done",
            "2025-01-04T00:00:00Z",
            "approved",
        ))?;
        store.mark_proposals_materialized(&["cprop-done".to_string()], "2026.01.01")?;

        let queue = store.list_approved_unmaterialized_proposals()?;
        let ids: Vec<&str> = queue.iter().map(|p| p.proposal_id.as_str()).collect();
        assert_eq!(ids, vec!["cprop-a", "cprop-b"], "pending and already-materialized proposals must be excluded; order is oldest-first");

        Ok(())
    }

    #[test]
    fn mark_materialized_stamps_once_and_reports_skips() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_constitutional_proposal(&sample_proposal(
            "cprop-x",
            "2025-01-01T00:00:00Z",
            "approved",
        ))?;

        let first = store.mark_proposals_materialized(&["cprop-x".to_string()], "2026.02.02")?;
        assert_eq!(first, vec!["cprop-x"]);

        // Second call for the same proposal: the NULL guard skips it — the
        // proposal keeps its FIRST materialization version.
        let second = store.mark_proposals_materialized(&["cprop-x".to_string()], "2026.03.03")?;
        assert!(second.is_empty());
        let row = store
            .get_constitutional_proposal("cprop-x")?
            .expect("row exists");
        assert_eq!(row.materialized_in_version.as_deref(), Some("2026.02.02"));

        // Unknown IDs are simply absent from the returned list, and an empty
        // input is a no-op rather than a SQL error.
        let unknown = store.mark_proposals_materialized(&["cprop-nope".to_string()], "2026.02.02")?;
        assert!(unknown.is_empty());
        let none = store.mark_proposals_materialized(&[], "2026.02.02")?;
        assert!(none.is_empty());

        Ok(())
    }
}
