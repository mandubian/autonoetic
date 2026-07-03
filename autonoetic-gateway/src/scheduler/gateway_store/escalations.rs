use anyhow::{bail, Result};
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus, EscalationType};
use rusqlite::{params, Connection, OptionalExtension};

use super::GatewayStore;

fn row_to_escalation(row: &rusqlite::Row) -> rusqlite::Result<EscalationMessage> {
    let status_str: String = row.get(10)?;
    let status = EscalationStatus::parse(&status_str).unwrap_or(EscalationStatus::Pending);
    let code_excerpts_json: Option<String> = row.get(13)?;
    let code_excerpts = code_excerpts_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let escalation_type_str: String = row.get(14)?;
    let escalation_type =
        EscalationType::parse(&escalation_type_str).unwrap_or(EscalationType::PromotionReview);
    let approval_request_id: Option<String> = row.get(15)?;
    let expires_at: Option<String> = row.get(16)?;
    Ok(EscalationMessage {
        escalation_id: row.get(0)?,
        artifact_id: row.get(1)?,
        artifact_digest: row.get(2)?,
        agent_id: row.get(3)?,
        revision_id: row.get(4)?,
        role_verdicts: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
        planner_synthesis: row.get(6)?,
        created_at: row.get(7)?,
        resolved_at: row.get(8)?,
        root_session_id: row.get(9)?,
        status,
        decided_by: row.get(11)?,
        decision_reason: row.get(12)?,
        code_excerpts,
        escalation_type,
        approval_request_id,
        expires_at,
    })
}

impl GatewayStore {
    pub fn set_escalation_flood_cap(&self, cap: usize) {
        self.escalation_flood_cap
            .store(cap, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn create_escalation(&self, escalation: &EscalationMessage) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        let existing: Option<String> = conn
            .query_row(
                "SELECT escalation_id FROM escalations \
                 WHERE artifact_id = ?1 AND revision_id = ?2 AND status = 'pending'",
                params![escalation.artifact_id, escalation.revision_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing_id) = existing {
            bail!(
                "A pending escalation '{}' already exists for artifact '{}' revision '{}'",
                existing_id,
                escalation.artifact_id,
                escalation.revision_id
            );
        }

        let root_sid = &escalation.root_session_id;
        if !root_sid.is_empty() {
            let cap = self
                .escalation_flood_cap
                .load(std::sync::atomic::Ordering::Relaxed);
            if cap > 0 {
                let pending_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM escalations WHERE root_session_id = ?1 AND status = 'pending'",
                    params![root_sid],
                    |row| row.get(0),
                )?;
                if (pending_count as usize) >= cap {
                    bail!(
                        "escalation_flood: root session '{}' already has {} pending escalations (cap {})",
                        root_sid,
                        pending_count,
                        cap
                    );
                }
            }
        }

        let role_verdicts = serde_json::to_string(&escalation.role_verdicts)?;
        let code_excerpts_json = escalation
            .code_excerpts
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        conn.execute(
            "INSERT INTO escalations (escalation_id, artifact_id, artifact_digest, agent_id,
             revision_id, role_verdicts, planner_synthesis, created_at, resolved_at,
             root_session_id, status, code_excerpts, escalation_type, approval_request_id,
             expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                escalation.escalation_id,
                escalation.artifact_id,
                escalation.artifact_digest,
                escalation.agent_id,
                escalation.revision_id,
                role_verdicts,
                escalation.planner_synthesis,
                escalation.created_at,
                escalation.resolved_at,
                escalation.root_session_id,
                escalation.status.as_str(),
                code_excerpts_json,
                escalation.escalation_type.as_str(),
                escalation.approval_request_id,
                escalation.expires_at,
            ],
        )?;
        // Release the conn lock before emitting — create_live_digest_event re-locks
        // the same Mutex, which would deadlock.
        drop(conn);

        // Surface the operator-facing escalation on the canonical timeline (#413).
        // It was store-only: the `apr-esc-*` approval it spawns is inserted
        // directly (not via the gate service), so no `approval.pending` fires —
        // the escalation was invisible in the room. Attributed to the agent under
        // review; Attention altitude (an operator decision is pending).
        //
        // Only emit for a genuinely pending escalation: a projection created
        // already-resolved (e.g. a promotion review cleared via approval_ref or
        // policy, #724 Part B) must not record a misleading "pending" event.
        if !escalation.root_session_id.is_empty()
            && escalation.status == autonoetic_types::escalation::EscalationStatus::Pending
        {
            let principal = autonoetic_types::principal::Principal::agent(&escalation.agent_id);
            let seat = crate::runtime::session_timeline::derive_role(&escalation.agent_id);
            let event = crate::runtime::session_timeline::build_timeline_event(
                escalation.root_session_id.clone(),
                escalation.root_session_id.clone(),
                None,
                &principal,
                &seat,
                "escalation.pending",
                None, // base_altitude ⇒ Attention
                Some(serde_json::json!({
                    "escalation_id": escalation.escalation_id,
                    "agent_id": escalation.agent_id,
                    "revision_id": escalation.revision_id,
                    "synthesis": escalation.planner_synthesis,
                    "escalation_type": escalation.escalation_type.as_str(),
                })),
                // Attach the artifact under review so detail views can show
                // `refs: artifact=…` and consumers can drill down without parsing
                // the payload.
                autonoetic_types::session_timeline::TimelineRefs {
                    artifact_id: (!escalation.artifact_id.is_empty())
                        .then(|| escalation.artifact_id.clone()),
                    approval_request_id: escalation.approval_request_id.clone(),
                    ..Default::default()
                },
            );
            if let Err(e) = self.create_live_digest_event(&event) {
                tracing::debug!(
                    target: "session_timeline",
                    error = %e,
                    "escalation.pending timeline emit failed"
                );
            }
        }
        Ok(())
    }

    fn escalation_exists_with_conn(
        &self,
        conn: &Connection,
        escalation_id: &str,
    ) -> Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM escalations WHERE escalation_id = ?1",
            params![escalation_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn get_escalation(&self, escalation_id: &str) -> Result<Option<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status,
             decided_by, decision_reason, code_excerpts, escalation_type, approval_request_id,
             expires_at
             FROM escalations WHERE escalation_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![escalation_id], row_to_escalation)?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_pending_escalations(&self) -> Result<Vec<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status,
             decided_by, decision_reason, code_excerpts, escalation_type, approval_request_id,
             expires_at
             FROM escalations WHERE status = 'pending' ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], row_to_escalation)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Mark pending escalations whose `expires_at` timestamp has passed as
    /// `stale`. Returns the IDs of escalations that were transitioned.
    pub fn expire_timed_out_escalations(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT escalation_id FROM escalations
             WHERE status = 'pending' AND expires_at IS NOT NULL AND expires_at < ?1",
        )?;
        let rows = stmt.query_map(params![now], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut expired_ids = Vec::new();
        for row in rows {
            let id = row?;
            conn.execute(
                "UPDATE escalations SET status = 'stale' WHERE escalation_id = ?1",
                params![id],
            )?;
            expired_ids.push(id);
        }
        Ok(expired_ids)
    }

    /// List stale escalations for a root session. Stale escalations are still
    /// resolvable by the operator — they just exceeded the configured TTL.
    pub fn get_stale_escalations_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status,
             decided_by, decision_reason, code_excerpts, escalation_type, approval_request_id,
             expires_at
             FROM escalations WHERE root_session_id = ?1 AND status = 'stale'
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![root_session_id], row_to_escalation)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Check whether any pending (unresolved) escalations exist for the given
    /// root session. Used by `try_complete_workflow` to prevent the workflow
    /// from completing while the planner is waiting for operator input — the
    /// plan may have more steps to run after the operator responds.
    pub fn has_pending_escalations_for_session(&self, root_session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM escalations \
             WHERE root_session_id = ?1 AND status = 'pending'",
            params![root_session_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Find the latest escalation for an artifact+revision with a matching
    /// status (used by the FullJury gate to check for operator approval).
    pub fn find_escalation(
        &self,
        artifact_id: &str,
        revision_id: &str,
        status: EscalationStatus,
    ) -> Result<Option<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status,
             decided_by, decision_reason, code_excerpts, escalation_type, approval_request_id
             FROM escalations
             WHERE artifact_id = ?1 AND revision_id = ?2 AND status = ?3
             ORDER BY created_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(
            params![artifact_id, revision_id, status.as_str()],
            row_to_escalation,
        )?;
        Ok(rows.next().transpose()?)
    }

    pub fn find_approved_escalation_for_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<Option<EscalationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT escalation_id, artifact_id, artifact_digest, agent_id, revision_id,
             role_verdicts, planner_synthesis, created_at, resolved_at, root_session_id, status,
             decided_by, decision_reason, code_excerpts, escalation_type, approval_request_id
             FROM escalations
             WHERE artifact_id = ?1 AND status = 'approved'
             ORDER BY created_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![artifact_id], row_to_escalation)?;
        Ok(rows.next().transpose()?)
    }

    pub fn resolve_escalation(
        &self,
        escalation_id: &str,
        status: EscalationStatus,
        decided_by: &str,
        decision_reason: Option<&str>,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();

        if !self.escalation_exists_with_conn(&conn, escalation_id)? {
            bail!("Escalation '{}' not found", escalation_id);
        }

        let (current_status, approval_request_id): (String, Option<String>) = conn.query_row(
            "SELECT status, approval_request_id FROM escalations WHERE escalation_id = ?1",
            params![escalation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if current_status != "pending" && current_status != "stale" {
            bail!(
                "Escalation '{}' is already '{}'; cannot resolve again",
                escalation_id,
                current_status
            );
        }

        conn.execute(
            "UPDATE escalations SET status = ?1, resolved_at = ?2, decided_by = ?3, \
             decision_reason = ?4 WHERE escalation_id = ?5",
            params![
                status.as_str(),
                chrono::Utc::now().to_rfc3339(),
                decided_by,
                decision_reason,
                escalation_id
            ],
        )?;
        Ok(approval_request_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::session_timeline::{Altitude, SessionRole};
    use tempfile::tempdir;

    #[test]
    fn create_escalation_surfaces_pending_on_timeline() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let esc = EscalationMessage::new(
            "esc-abc12345".to_string(),
            "art-1".to_string(),
            "coder.default".to_string(),
            "rev-9".to_string(),
            vec![],
            "looks good, recommend promote".to_string(),
            "root-xyz".to_string(),
        );
        store.create_escalation(&esc).unwrap();

        let result = store
            .list_session_timeline("root-xyz", None, 50, Some(Altitude::Detail), None)
            .unwrap();
        let ev = result
            .entries
            .iter()
            .find(|e| e.event_type == "escalation.pending")
            .expect("escalation.pending must reach the timeline");
        // Operator-decision-pending ⇒ Attention; attributed to the agent under review.
        assert_eq!(ev.altitude, Altitude::Attention);
        assert!(matches!(ev.role, SessionRole::Specialist { .. }));
        assert_eq!(ev.principal.id, "coder.default");
        assert!(ev.payload.as_deref().unwrap().contains("recommend promote"));
        // The artifact under review is attached as a ref for drill-down.
        assert_eq!(ev.refs.artifact_id.as_deref(), Some("art-1"));
    }
}
