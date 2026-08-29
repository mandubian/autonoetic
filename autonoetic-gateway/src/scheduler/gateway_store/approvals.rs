use anyhow::Result;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, GrantScope, GrantTarget, ScheduledAction,
    SessionApprovalGrant,
};
use rusqlite::{params, Connection, OptionalExtension};

use super::GatewayStore;

/// A session that joined an existing pending approval instead of minting a
/// duplicate (#723). On resolution the approval fans in to every waiter.
#[derive(Debug, Clone)]
pub struct ApprovalWaiter {
    pub request_id: String,
    pub session_id: String,
    pub workflow_id: Option<String>,
    pub task_id: Option<String>,
}

impl GatewayStore {
    /// Register `session_id` as a waiter that joined pending approval
    /// `request_id` (#723). Idempotent per `(request_id, session_id)`.
    pub fn add_approval_waiter(
        &self,
        request_id: &str,
        session_id: &str,
        workflow_id: Option<&str>,
        task_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO approval_waiters
                (request_id, session_id, workflow_id, task_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                request_id,
                session_id,
                workflow_id,
                task_id,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// List every session that joined pending approval `request_id`.
    pub fn list_approval_waiters(&self, request_id: &str) -> Result<Vec<ApprovalWaiter>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id, session_id, workflow_id, task_id
             FROM approval_waiters WHERE request_id = ?1",
        )?;
        let rows = stmt.query_map(params![request_id], |row| {
            Ok(ApprovalWaiter {
                request_id: row.get(0)?,
                session_id: row.get(1)?,
                workflow_id: row.get(2)?,
                task_id: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Remove all waiter rows for `request_id` (called once it resolves).
    pub fn clear_approval_waiters(&self, request_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM approval_waiters WHERE request_id = ?1",
            params![request_id],
        )?;
        Ok(())
    }

    /// Surface an `approval_flood` cap trip to the operator as an Attention
    /// timeline event on the root session (#723) so it reads as a triage item
    /// rather than only as the agent's tool failure.
    fn emit_approval_flood_alert(&self, root_session_id: &str, pending_count: i64, cap: usize) {
        if root_session_id.is_empty() {
            return;
        }
        // Emit at most once per root per flood window: skip if this root already
        // has an active alert. The flag is cleared when a create for the root
        // next succeeds (see `create_approval`).
        {
            let mut alerted = self.flood_alerted_roots.lock().unwrap();
            if !alerted.insert(root_session_id.to_string()) {
                return;
            }
        }
        let principal = autonoetic_types::principal::Principal::agent("gateway");
        let seat = crate::runtime::session_timeline::derive_role("gateway");
        let event = crate::runtime::session_timeline::build_timeline_event(
            root_session_id.to_string(),
            root_session_id.to_string(),
            None,
            &principal,
            &seat,
            "operator_alert",
            None, // base altitude ⇒ Attention
            Some(serde_json::json!({
                "alert": "approval_flood",
                "pending_count": pending_count,
                "cap": cap,
                "message": format!(
                    "Approval intake suspended: {} pending approvals at cap {}. \
                     Resolve some with `autonoetic gateway pending --root-session {}`.",
                    pending_count, cap, root_session_id
                ),
            })),
            autonoetic_types::session_timeline::TimelineRefs::default(),
        );
        if let Err(e) = self.create_live_digest_event(&event) {
            tracing::debug!(
                target: "session_timeline",
                error = %e,
                "approval_flood alert timeline emit failed"
            );
        }
    }

    pub fn create_approval(&self, request: &mut ApprovalRequest) -> Result<()> {
        crate::scheduler::approval_hardening::enrich_request(request, self.config().as_deref());
        let conn = self.conn.lock().unwrap();

        if let Some(ref root_session_id) = request.root_session_id {
            let cap = self
                .approval_flood_cap
                .load(std::sync::atomic::Ordering::Relaxed);
            if cap > 0 {
                let pending_count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM approvals WHERE root_session_id = ?1 AND status = 'pending'",
                    params![root_session_id],
                    |row| row.get(0),
                )?;
                if (pending_count as usize) >= cap {
                    let root = root_session_id.clone();
                    // Release the connection before emitting the alert (the
                    // timeline write re-acquires it) and before bailing.
                    drop(conn);
                    self.emit_approval_flood_alert(&root, pending_count, cap);
                    anyhow::bail!(
                        "approval_flood: root session '{}' already has {} pending approvals (cap {})",
                        root,
                        pending_count,
                        cap
                    );
                }
            }
        }

        let action_payload = serde_json::to_string(&request.action)?;
        let code_excerpts_json = request
            .code_excerpts
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let risk_summary_json = request
            .risk_summary
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let expires_at = request.expires_at.as_deref();
        conn.execute(
            "INSERT INTO approvals (
                request_id, agent_id, session_id, root_session_id, workflow_id, task_id,
                action_type, action_payload, reason, evidence_ref, status, created_at,
                approval_level,
                min_dwell_ms, confirm_phrase, code_excerpts, risk_summary, expires_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                request.request_id,
                request.agent_id,
                request.session_id,
                request.root_session_id,
                request.workflow_id,
                request.task_id,
                request.action.kind(),
                action_payload,
                request.reason,
                request.evidence_ref,
                "pending",
                request.created_at,
                serde_json::to_string(&request.approval_level)?,
                request.min_dwell_ms,
                request.confirm_phrase,
                code_excerpts_json,
                risk_summary_json,
                expires_at,
            ],
        )?;
        // Release the conn lock before timeline emit — create_live_digest_event
        // re-locks the same Mutex and would deadlock otherwise.
        drop(conn);
        // A create succeeded for this root, so it is no longer at the flood cap:
        // reset the once-per-window alert flag (#723).
        if let Some(ref root) = request.root_session_id {
            self.flood_alerted_roots.lock().unwrap().remove(root);
        }
        crate::runtime::session_timeline::emit_approval_pending_timeline_event(self, request, None);
        Ok(())
    }

    pub fn set_approval_code_excerpts(
        &self,
        request_id: &str,
        code_excerpts: Option<&[autonoetic_types::background::CodeExcerpt]>,
        risk_summary: Option<&autonoetic_types::background::RiskSummary>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let code_json = code_excerpts.map(|v| serde_json::to_string(v).unwrap_or_default());
        let risk_json = risk_summary.map(|v| serde_json::to_string(v).unwrap_or_default());
        conn.execute(
            "UPDATE approvals SET code_excerpts = ?1, risk_summary = ?2 WHERE request_id = ?3",
            params![code_json, risk_json, request_id],
        )?;
        Ok(())
    }

    pub fn set_approval_flood_cap(&self, cap: usize) {
        self.approval_flood_cap
            .store(cap, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn count_pending_for_root(&self, root_session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM approvals WHERE root_session_id = ?1 AND status = 'pending'",
            params![root_session_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    fn get_approval_with_conn(
        conn: &Connection,
        request_id: &str,
    ) -> Result<Option<ApprovalRequest>> {
        conn.query_row(
            "SELECT request_id, agent_id, session_id, action_payload, created_at, workflow_id, task_id, root_session_id, status, decided_at, decided_by, reason, evidence_ref, approval_level, decision_reason, min_dwell_ms, confirm_phrase, code_excerpts, risk_summary, expires_at FROM approvals WHERE request_id = ?1",
            params![request_id],
            |row| {
                let action_payload: String = row.get(3)?;
                let status_str: Option<String> = row.get(8)?;
                let status = status_str.and_then(|s| match s.as_str() {
                    "approved" => Some(autonoetic_types::background::ApprovalStatus::Approved),
                    "rejected" => Some(autonoetic_types::background::ApprovalStatus::Rejected),
                    "cancelled" => Some(autonoetic_types::background::ApprovalStatus::Cancelled),
                    "stale" => Some(autonoetic_types::background::ApprovalStatus::Stale),
                    _ => None,
                });
                let action = serde_json::from_str(&action_payload).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
                })?;
                let level_str: String = row.get(13)?;
                let approval_level: ApprovalLevel = serde_json::from_str(&level_str).unwrap_or(ApprovalLevel::Operator);
                let code_excerpts_json: Option<String> = row.get(17)?;
                let risk_summary_json: Option<String> = row.get(18)?;
                let expires_at: Option<String> = row.get(19)?;
                let code_excerpts = code_excerpts_json
                    .and_then(|s| serde_json::from_str::<Vec<autonoetic_types::background::CodeExcerpt>>(&s).ok());
                let risk_summary = risk_summary_json
                    .and_then(|s| serde_json::from_str::<autonoetic_types::background::RiskSummary>(&s).ok());
                Ok(ApprovalRequest {
                    request_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    session_id: row.get(2)?,
                    action,
                    created_at: row.get(4)?,
                    workflow_id: row.get(5)?,
                    task_id: row.get(6)?,
                    root_session_id: row.get(7)?,
                    status,
                    decided_at: row.get(9)?,
                    decided_by: row.get(10)?,
                    reason: row.get(11)?,
                    evidence_ref: row.get(12)?,
                    decision_reason: row.get(14)?,
                    approval_level,
                    min_dwell_ms: row.get(15)?,
                    confirm_phrase: row.get(16)?,
                    code_excerpts,
                    risk_summary,
                    expires_at,
                })
            },
        ).optional().map_err(Into::into)
    }

    pub fn get_approval(&self, request_id: &str) -> Result<Option<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        Self::get_approval_with_conn(&conn, request_id)
    }

    /// Replace a dead approval's stored `action_payload` with its
    /// operator-class projection (#1213).
    ///
    /// `action_payload` is kept raw while an approval is live because it *is*
    /// the execution input — the scheduler deserializes it and runs it. Once a
    /// gate is **rejected or cancelled**, its turn is dead (the bound checkpoint
    /// is reaped in the same `apply_decision` branch), so the raw command will
    /// never be executed and there is nothing left to keep it raw for.
    ///
    /// `Stale` is deliberately **not** in that set. A stale approval is still
    /// resolvable — scrubbing it would leave an operator able to approve a
    /// command whose credential had been replaced with `***REDACTED***`. Stale
    /// rows are bounded by retention like approved ones.
    ///
    /// The projection is the operator view, so the record keeps everything a
    /// human reviewing history would look at — binary, flags, destination host
    /// — and drops only the credential values. Approved rows are deliberately
    /// left alone: they remain resumable, and a crash between the decision and
    /// its execution would otherwise resume a command with `***REDACTED***`
    /// where a token belongs.
    pub fn scrub_dead_approval_payload(&self, request_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<String> = conn
            .query_row(
                "SELECT action_payload FROM approvals WHERE request_id = ?1",
                params![request_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(false);
        };
        let Ok(action) = serde_json::from_str::<ScheduledAction>(&raw) else {
            // An unparseable payload cannot be projected. Leave it rather than
            // guessing — retention still bounds how long it survives.
            return Ok(false);
        };
        let scrubbed = serde_json::to_string(
            &action.redact_for_viewer(autonoetic_types::disclosure::ViewerClass::Operator),
        )?;
        if scrubbed == raw {
            return Ok(false);
        }
        conn.execute(
            "UPDATE approvals SET action_payload = ?1 WHERE request_id = ?2",
            params![scrubbed, request_id],
        )?;
        Ok(true)
    }

    pub fn record_decision(
        &self,
        request_id: &str,
        status: &str,
        decided_by: &str,
        decided_at: &str,
        decision_reason: Option<&str>,
    ) -> Result<()> {
        // #361: record the decider's principal kind (derived from decided_by) so
        // §O symmetric-obligation checks are mechanically queryable in SQL.
        let decided_by_kind =
            autonoetic_types::principal::decider_principal_kind(decided_by).map(|k| k.tag());
        let ctx = {
            let conn = self.conn.lock().unwrap();
            let rows = conn.execute(
                "UPDATE approvals SET status = ?1, decided_by = ?2, decided_at = ?3, decision_reason = ?4, decided_by_kind = ?5 WHERE request_id = ?6 AND status IN ('pending', 'stale')",
                params![status, decided_by, decided_at, decision_reason, decided_by_kind, request_id],
            )?;
            if rows == 0 {
                anyhow::bail!(
                    "Approval {} is no longer pending or stale (already decided or not found)",
                    request_id
                );
            }
            resolution_context(&conn, request_id)
        };
        // Session Room: the gate *closes* on the canonical timeline (#363 P1).
        self.emit_gate_resolution(request_id, status, decided_by, ctx);
        Ok(())
    }

    pub fn cancel_approval(
        &self,
        request_id: &str,
        cancelled_by: &str,
        cancelled_at: &str,
    ) -> Result<()> {
        let decided_by_kind =
            autonoetic_types::principal::decider_principal_kind(cancelled_by).map(|k| k.tag());
        let ctx = {
            let conn = self.conn.lock().unwrap();
            let rows = conn.execute(
                "UPDATE approvals SET status = 'cancelled', decided_by = ?1, decided_at = ?2, decided_by_kind = ?3 WHERE request_id = ?4 AND status = 'pending'",
                params![cancelled_by, cancelled_at, decided_by_kind, request_id],
            )?;
            if rows == 0 {
                anyhow::bail!(
                    "Approval {} is no longer pending (already decided or not found)",
                    request_id
                );
            }
            resolution_context(&conn, request_id)
        };
        self.emit_gate_resolution(request_id, "cancelled", cancelled_by, ctx);
        Ok(())
    }

    /// Emit an `approval.{approved,rejected,cancelled}` event onto the canonical
    /// timeline, authored by the decider (#363 P1). Best-effort: a failure here
    /// never affects the recorded decision. `ctx` is `(root_session_id,
    /// session_id, agent_id)` of the original request.
    fn emit_gate_resolution(
        &self,
        request_id: &str,
        status: &str,
        decided_by: &str,
        ctx: Option<(Option<String>, String, String)>,
    ) {
        use autonoetic_types::session_timeline::TimelineRefs;

        let Some((root, session, _agent)) = ctx else {
            return;
        };
        // The event type selects the gate-lifecycle arm; the altitude comes
        // from `base_altitude` (the single source of truth for gate altitudes:
        // approved/rejected = Attention, cancelled = Normal). Passing `None`
        // lets `altitude_for` apply `max(base, role_floor)` so a Sentinel
        // decider's resolution stays at least Attention.
        let event_type = match status {
            "approved" => "approval.approved",
            "rejected" | "denied" => "approval.rejected",
            "cancelled" => "approval.cancelled",
            _ => return,
        };
        // Author = the decider: Operator seat (human), the agent's seat (stripping
        // any `agent:` prefix), or Runtime (hidable) for mechanical resolutions.
        let (principal, role) = crate::runtime::session_timeline::decider_seat(decided_by);
        let refs = TimelineRefs {
            approval_request_id: Some(request_id.to_string()),
            ..Default::default()
        };
        let event = crate::runtime::session_timeline::build_timeline_event(
            root.unwrap_or_else(|| session.clone()),
            session,
            None,
            &principal,
            &role,
            event_type,
            None,
            Some(serde_json::json!({ "request_id": request_id, "decided_by": decided_by })),
            refs,
        );
        if let Err(e) = self.create_live_digest_event(&event) {
            tracing::debug!(target: "session_timeline", error = %e, "gate resolution timeline emit failed");
        }
    }

    pub fn get_pending_approvals(&self) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT request_id FROM approvals WHERE status = 'pending'")?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }

    pub fn get_pending_approvals_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals WHERE root_session_id = ?1 AND status = 'pending'",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }

    pub fn get_stale_approvals_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals WHERE root_session_id = ?1 AND status = 'stale'",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }

    pub fn get_approved_approvals_for_root(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals WHERE root_session_id = ?1 AND status = 'approved'",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }

    pub fn get_approved_approvals_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals WHERE session_id = ?1 AND status = 'approved'",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }

    pub fn find_matching_revision_promote_approval(
        &self,
        agent_id: &str,
        revision_id: &str,
        added_capabilities: &[String],
        broadened_capabilities: &[String],
    ) -> Result<Option<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id, action_payload, status FROM approvals
             WHERE action_type = 'revision_promote'
               AND status IN ('pending', 'approved')
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![], |row| {
            let id: String = row.get(0)?;
            let payload: String = row.get(1)?;
            let status: String = row.get(2)?;
            Ok((id, payload, status))
        })?;
        let added: std::collections::HashSet<String> = added_capabilities.iter().cloned().collect();
        let broadened: std::collections::HashSet<String> =
            broadened_capabilities.iter().cloned().collect();
        for row in rows {
            let (id, payload, status) = row?;
            let action: autonoetic_types::background::ScheduledAction =
                match serde_json::from_str(&payload) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
            if let autonoetic_types::background::ScheduledAction::RevisionPromote {
                agent_id: a_id,
                revision_id: r_id,
                added_capabilities: a_caps,
                broadened_capabilities: b_caps,
                ..
            } = action
            {
                if a_id == agent_id
                    && r_id == revision_id
                    && a_caps
                        .iter()
                        .cloned()
                        .collect::<std::collections::HashSet<String>>()
                        == added
                    && b_caps
                        .iter()
                        .cloned()
                        .collect::<std::collections::HashSet<String>>()
                        == broadened
                {
                    return Self::get_approval_with_conn(&conn, &id).map(|opt| {
                        opt.map(|mut req| {
                            // Surface the stored status so callers can distinguish
                            // pending from already-approved matches.
                            req.status = match status.as_str() {
                                "approved" => {
                                    Some(autonoetic_types::background::ApprovalStatus::Approved)
                                }
                                "rejected" => {
                                    Some(autonoetic_types::background::ApprovalStatus::Rejected)
                                }
                                "cancelled" => {
                                    Some(autonoetic_types::background::ApprovalStatus::Cancelled)
                                }
                                _ => None,
                            };
                            req
                        })
                    });
                }
            }
        }
        Ok(None)
    }

    pub fn find_matching_revision_promote_approval_for_root(
        &self,
        root_session_id: &str,
        agent_id: &str,
        revision_id: &str,
        added_capabilities: &[String],
        broadened_capabilities: &[String],
        outgoing_revision_id: &str,
    ) -> Result<Option<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id, action_payload, status FROM approvals
             WHERE root_session_id = ?1
               AND action_type = 'revision_promote'
               AND status IN ('pending', 'approved')
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: String = row.get(0)?;
            let payload: String = row.get(1)?;
            let status: String = row.get(2)?;
            Ok((id, payload, status))
        })?;
        let added: std::collections::HashSet<String> = added_capabilities.iter().cloned().collect();
        let broadened: std::collections::HashSet<String> =
            broadened_capabilities.iter().cloned().collect();
        for row in rows {
            let (id, payload, status) = row?;
            let action: autonoetic_types::background::ScheduledAction =
                match serde_json::from_str(&payload) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
            if let autonoetic_types::background::ScheduledAction::RevisionPromote {
                agent_id: a_id,
                revision_id: r_id,
                outgoing_revision_id: a_outgoing,
                added_capabilities: a_caps,
                broadened_capabilities: b_caps,
                ..
            } = action
            {
                // The outgoing baseline is part of the match key (#658 /
                // review on #660): a stale approval acknowledged a delta against
                // a *specific* outgoing revision. If the alias has since moved,
                // reusing it would misrepresent what the operator approved (and
                // could bypass the reassignment informed-consent gate after a
                // slot shape change). An empty `outgoing_revision_id` (brand-new
                // agent) matches only approvals that were also first-admission.
                if a_id == agent_id
                    && r_id == revision_id
                    && a_outgoing == outgoing_revision_id
                    && a_caps
                        .iter()
                        .cloned()
                        .collect::<std::collections::HashSet<String>>()
                        == added
                    && b_caps
                        .iter()
                        .cloned()
                        .collect::<std::collections::HashSet<String>>()
                        == broadened
                {
                    return Self::get_approval_with_conn(&conn, &id).map(|opt| {
                        opt.map(|mut req| {
                            req.status = match status.as_str() {
                                "approved" => {
                                    Some(autonoetic_types::background::ApprovalStatus::Approved)
                                }
                                "rejected" => {
                                    Some(autonoetic_types::background::ApprovalStatus::Rejected)
                                }
                                "cancelled" => {
                                    Some(autonoetic_types::background::ApprovalStatus::Cancelled)
                                }
                                _ => None,
                            };
                            req
                        })
                    });
                }
            }
        }
        Ok(None)
    }

    /// #1094: promotion-identity dedup. Find any `revision_promote` approval —
    /// pending, approved, or rejected — for the same promotion identity
    /// `(agent_id, revision_id, outgoing_revision_id, added, broadened)`.
    /// Scoped to the escalation's own `root_session_id`: the bare ask may live
    /// in a child-session projection (same root), but an approval minted under
    /// a DIFFERENT root must not suppress a fresh operator decision here —
    /// same boundary as `find_matching_revision_promote_approval_for_root`.
    pub fn find_matching_revision_promote_approval_for_identity(
        &self,
        root_session_id: &str,
        agent_id: &str,
        revision_id: &str,
        outgoing_revision_id: &str,
        added_capabilities: &[String],
        broadened_capabilities: &[String],
    ) -> Result<Option<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id, action_payload, status FROM approvals
             WHERE action_type = 'revision_promote'
               AND root_session_id = ?1
               AND status IN ('pending', 'approved', 'rejected')
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: String = row.get(0)?;
            let payload: String = row.get(1)?;
            let status: String = row.get(2)?;
            Ok((id, payload, status))
        })?;
        let added: std::collections::HashSet<String> = added_capabilities.iter().cloned().collect();
        let broadened: std::collections::HashSet<String> =
            broadened_capabilities.iter().cloned().collect();
        for row in rows {
            let (id, payload, status) = row?;
            let action: autonoetic_types::background::ScheduledAction =
                match serde_json::from_str(&payload) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
            if let autonoetic_types::background::ScheduledAction::RevisionPromote {
                agent_id: a_id,
                revision_id: r_id,
                outgoing_revision_id: a_outgoing,
                added_capabilities: a_caps,
                broadened_capabilities: b_caps,
                ..
            } = action
            {
                if a_id == agent_id
                    && r_id == revision_id
                    && a_outgoing == outgoing_revision_id
                    && a_caps
                        .iter()
                        .cloned()
                        .collect::<std::collections::HashSet<String>>()
                        == added
                    && b_caps
                        .iter()
                        .cloned()
                        .collect::<std::collections::HashSet<String>>()
                        == broadened
                {
                    return Self::get_approval_with_conn(&conn, &id).map(|opt| {
                        opt.map(|mut req| {
                            req.status = match status.as_str() {
                                "approved" => {
                                    Some(autonoetic_types::background::ApprovalStatus::Approved)
                                }
                                "rejected" => {
                                    Some(autonoetic_types::background::ApprovalStatus::Rejected)
                                }
                                "cancelled" => {
                                    Some(autonoetic_types::background::ApprovalStatus::Cancelled)
                                }
                                _ => None,
                            };
                            req
                        })
                    });
                }
            }
        }
        Ok(None)
    }

    pub fn list_all_approvals_for_session(&self, session_id: &str) -> Result<Vec<ApprovalRequest>> {
        let root = crate::runtime::content_store::root_session_id(session_id);
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals WHERE session_id = ?1 OR root_session_id = ?2 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id, &root], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }

    pub fn get_recent_approvals_for_agent(
        &self,
        agent_id: &str,
        limit: usize,
    ) -> Result<Vec<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals WHERE agent_id = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![agent_id, limit as i64], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(app) = Self::get_approval_with_conn(&conn, &id)? {
                results.push(app);
            }
        }
        Ok(results)
    }

    pub fn get_approval_status_by_task_id(&self, task_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT status FROM approvals WHERE task_id = ?1 ORDER BY created_at DESC LIMIT 1",
            params![task_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_pending_approval_request_id_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT request_id FROM approvals WHERE task_id = ?1 AND status = 'pending' ORDER BY created_at DESC LIMIT 1",
            params![task_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Find the most recent still-open (pending or approved) approval bound to
    /// a session. Used by checkpoint integrity handling (#606) to locate the
    /// approval that must be cancelled when a tampered/mismatched checkpoint is
    /// detected on resume and the request id is not otherwise known.
    pub fn find_latest_open_approval_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT request_id FROM approvals \
             WHERE session_id = ?1 AND status IN ('pending', 'approved') \
             ORDER BY created_at DESC LIMIT 1",
            params![session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Forcefully move an approval to `cancelled` with reason
    /// `integrity_violation`, regardless of whether it is currently `pending`
    /// or `approved`. Unlike [`cancel_approval`], this is not guarded by a
    /// `status = 'pending'` predicate: a checkpoint integrity violation
    /// detected on resume means the (possibly already-approved) approval is no
    /// longer trustworthy and must be revoked. Already-terminal approvals
    /// (rejected/cancelled) are left untouched.
    pub fn cancel_approval_for_integrity_violation(&self, request_id: &str) -> Result<bool> {
        const DECIDED_BY: &str = "gateway:integrity_check";
        let decided_by_kind =
            autonoetic_types::principal::decider_principal_kind(DECIDED_BY).map(|k| k.tag());
        let now = chrono::Utc::now().to_rfc3339();
        let ctx = {
            let conn = self.conn.lock().unwrap();
            let rows = conn.execute(
                "UPDATE approvals \
                 SET status = 'cancelled', decided_by = ?1, decided_at = ?2, \
                     decision_reason = 'integrity_violation', decided_by_kind = ?3 \
                 WHERE request_id = ?4 AND status NOT IN ('cancelled', 'rejected')",
                params![DECIDED_BY, now, decided_by_kind, request_id],
            )?;
            if rows == 0 {
                return Ok(false);
            }
            resolution_context(&conn, request_id)
        };
        // Record the cancellation on the canonical session timeline, matching
        // record_decision / cancel_approval so §O accountability stays
        // consistent (#606 review).
        self.emit_gate_resolution(request_id, "cancelled", DECIDED_BY, ctx);
        Ok(true)
    }

    pub fn insert_session_grant(
        &self,
        root_session_id: &str,
        session_id: &str,
        agent_id: &str,
        scope: &GrantScope,
        targets: &[GrantTarget],
        granted_by: &str,
        granted_at: &str,
        source_approval_id: Option<&str>,
        expires_at: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        let primary_host = targets
            .iter()
            .find_map(|t| match t {
                GrantTarget::ExactHost(h) => Some(h.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "_multi_target_".to_string());

        conn.execute(
            "INSERT INTO session_approval_grants
             (root_session_id, session_id, agent_id, host, scope, granted_by, granted_at, source_approval_id, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(root_session_id, session_id, agent_id, scope, host) DO UPDATE SET
                 granted_by = excluded.granted_by,
                 granted_at = excluded.granted_at,
                 source_approval_id = excluded.source_approval_id,
                 expires_at = excluded.expires_at,
                 revoked_at = NULL,
                 revoked_reason = NULL",
            params![
                root_session_id,
                session_id,
                agent_id,
                primary_host,
                scope.as_str(),
                granted_by,
                granted_at,
                source_approval_id,
                expires_at,
            ],
        )?;

        let grant_id: i64 = conn.query_row(
            "SELECT id FROM session_approval_grants WHERE root_session_id = ?1 AND session_id = ?2 AND agent_id = ?3 AND scope = ?4 AND host = ?5",
            params![root_session_id, session_id, agent_id, scope.as_str(), primary_host],
            |row| row.get(0),
        )?;

        conn.execute(
            "DELETE FROM session_approval_grant_targets WHERE grant_id = ?1",
            params![grant_id],
        )?;

        for target in targets {
            let value = serde_json::to_string(target)?;
            conn.execute(
                "INSERT INTO session_approval_grant_targets (grant_id, kind, value) VALUES (?1, ?2, ?3)",
                params![grant_id, target.kind_str(), value],
            )?;
        }

        Ok(())
    }

    /// Backward-compatible wrapper: inserts ExactHost grants with RootSession scope.
    pub fn insert_session_grant_hosts(
        &self,
        root_session_id: &str,
        agent_id: &str,
        hosts: &[String],
        granted_by: &str,
        granted_at: &str,
        source_approval_id: Option<&str>,
    ) -> Result<()> {
        for host in hosts {
            let targets = vec![GrantTarget::ExactHost(host.clone())];
            self.insert_session_grant(
                root_session_id,
                root_session_id,
                agent_id,
                &GrantScope::RootSession,
                &targets,
                granted_by,
                granted_at,
                source_approval_id,
                None,
            )?;
        }
        Ok(())
    }

    pub fn get_session_grants(&self, root_session_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT host FROM session_approval_grants
             WHERE root_session_id = ?1 AND revoked_at IS NULL",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let host: String = row.get(0)?;
            Ok(host)
        })?;

        let mut results = Vec::new();
        for host_result in rows {
            results.push(host_result?);
        }
        results.sort();
        Ok(results)
    }

    pub fn get_session_grants_structured(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<SessionApprovalGrant>> {
        let conn = self.conn.lock().unwrap();
        self.get_session_grants_structured_with_conn(&conn, root_session_id)
    }

    fn get_session_grants_structured_with_conn(
        &self,
        conn: &Connection,
        root_session_id: &str,
    ) -> Result<Vec<SessionApprovalGrant>> {
        let mut stmt = conn.prepare(
            "SELECT id, root_session_id, session_id, agent_id, scope, granted_by, granted_at,
                    source_approval_id, expires_at
             FROM session_approval_grants
             WHERE root_session_id = ?1 AND revoked_at IS NULL",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: i64 = row.get(0)?;
            let root_session_id: String = row.get(1)?;
            let session_id: String = row.get(2)?;
            let agent_id: String = row.get(3)?;
            let scope_str: String = row.get(4)?;
            let granted_by: String = row.get(5)?;
            let granted_at: String = row.get(6)?;
            let source_approval_id: Option<String> = row.get(7)?;
            let expires_at: Option<String> = row.get(8)?;
            Ok((
                id,
                root_session_id,
                session_id,
                agent_id,
                scope_str,
                granted_by,
                granted_at,
                source_approval_id,
                expires_at,
            ))
        })?;

        let mut results = Vec::new();
        for row_result in rows {
            let (
                id,
                root_sid,
                sess_id,
                agent_id,
                scope_str,
                granted_by,
                granted_at,
                source_approval_id,
                expires_at,
            ) = row_result?;
            let targets = self.get_grant_targets_with_conn(conn, id)?;
            results.push(SessionApprovalGrant {
                id,
                root_session_id: root_sid,
                session_id: sess_id,
                agent_id,
                scope: GrantScope::from_str_lossy(&scope_str),
                granted_by,
                granted_at,
                source_approval_id,
                expires_at,
                targets,
            });
        }
        Ok(results)
    }

    fn get_grant_targets_with_conn(
        &self,
        conn: &Connection,
        grant_id: i64,
    ) -> Result<Vec<GrantTarget>> {
        let mut stmt = conn.prepare(
            "SELECT kind, value FROM session_approval_grant_targets WHERE grant_id = ?1",
        )?;
        let rows = stmt.query_map(params![grant_id], |row| {
            let kind: String = row.get(0)?;
            let value: String = row.get(1)?;
            Ok((kind, value))
        })?;
        let mut targets = Vec::new();
        for row in rows {
            let (kind, value) = row?;
            if let Some(t) = Self::parse_grant_target(&kind, &value) {
                targets.push(t);
            } else {
                eprintln!(
                    "warning: failed to parse session_approval_grant_targets row for grant_id={} (kind={}, value={})",
                    grant_id, kind, value
                );
            }
        }
        Ok(targets)
    }

    fn parse_grant_target(kind: &str, value: &str) -> Option<GrantTarget> {
        if let Ok(t) = serde_json::from_str::<GrantTarget>(value) {
            return Some(t);
        }
        let tagged = serde_json::json!({"kind": kind, "value": value});
        if let Ok(t) = serde_json::from_value::<GrantTarget>(tagged) {
            return Some(t);
        }
        match kind {
            "exact_host" => Some(GrantTarget::ExactHost(value.to_string())),
            "host_suffix" => Some(GrantTarget::HostSuffix(value.to_string())),
            "host_and_port" => {
                if let Some((h, p)) = value.rsplit_once(':') {
                    if let Ok(port) = p.parse::<u16>() {
                        return Some(GrantTarget::HostAndPort {
                            host: h.to_string(),
                            port,
                        });
                    }
                }
                None
            }
            "url_prefix" => Some(GrantTarget::UrlPrefix(value.to_string())),
            _ => None,
        }
    }

    pub fn session_grants_cover_targets(
        &self,
        root_session_id: &str,
        agent_id: &str,
        required_targets: &[String],
    ) -> bool {
        self.grants_cover_targets(root_session_id, root_session_id, agent_id, required_targets)
    }

    /// Scope-aware grant coverage check.  A request is covered when every
    /// required target is matched by at least one active (non-revoked,
    /// non-expired) grant whose scope covers the requesting session **and
    /// whose agent column matches the requesting agent**.
    ///
    /// Agent scoping: a grant minted from an approval records the agent whose
    /// action was approved, and covers that agent only — a sibling session
    /// running a *different* agent (e.g. a freshly built candidate in the
    /// evolution pipeline) does not inherit the approval. The one exception is
    /// `ROOT_WIDE_GRANT_AGENT`, the sentinel that envelope locks mint under
    /// (see `session_envelope::materialize_network_grant`): those deliberately
    /// cover every agent under the root, because the operator authorized the
    /// root before the agent set was known. The sentinel is `*`, which
    /// `validate_agent_id` excludes from the agent-id character set, so a real
    /// agent id can never alias it into root-wide coverage.
    pub fn grants_cover_targets(
        &self,
        session_id: &str,
        root_session_id: &str,
        agent_id: &str,
        required_targets: &[String],
    ) -> bool {
        if required_targets.is_empty() {
            return false;
        }

        let grants = match self.get_session_grants_structured(root_session_id) {
            Ok(g) => g,
            Err(_) => return false,
        };

        let now = chrono::Utc::now();

        let active: Vec<&SessionApprovalGrant> = grants
            .iter()
            .filter(|g| {
                if let Some(ref exp) = g.expires_at {
                    let expires_at = match chrono::DateTime::parse_from_rfc3339(exp) {
                        Ok(dt) => dt.with_timezone(&chrono::Utc),
                        Err(_) => return false,
                    };
                    if expires_at < now {
                        return false;
                    }
                }
                let agent_covers = g.agent_id == agent_id
                    || g.agent_id == autonoetic_types::background::ROOT_WIDE_GRANT_AGENT;
                if !agent_covers {
                    return false;
                }
                match g.scope {
                    GrantScope::RootSession => true,
                    GrantScope::Session => g.session_id == session_id,
                }
            })
            .collect();

        if active.is_empty() {
            return false;
        }

        required_targets.iter().all(|req| {
            active
                .iter()
                .any(|grant| grant.targets.iter().any(|t| t.matches(req)))
        })
    }

    pub fn delete_session_grants(&self, root_session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let grant_ids: Vec<i64> = {
            let mut stmt =
                conn.prepare("SELECT id FROM session_approval_grants WHERE root_session_id = ?1")?;
            let rows = stmt.query_map(params![root_session_id], |row| row.get(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for gid in &grant_ids {
            let _ = conn.execute(
                "DELETE FROM session_approval_grant_targets WHERE grant_id = ?1",
                params![gid],
            );
        }
        conn.execute(
            "DELETE FROM session_approval_grants WHERE root_session_id = ?1",
            params![root_session_id],
        )?;
        super::egress_declassification::delete_grants_for_root(&conn, root_session_id)?;
        Ok(())
    }

    pub fn revoke_session_grants(
        &self,
        root_session_id: &str,
        host: Option<&str>,
        reason: &str,
    ) -> Result<usize> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let count = match host {
            Some(h) => {
                let active_grants =
                    self.get_session_grants_structured_with_conn(&*conn, root_session_id)?;
                let mut matching_ids = Vec::new();
                for g in &active_grants {
                    if g.targets.iter().any(|t| t.matches(h)) {
                        matching_ids.push(g.id);
                    }
                }
                if matching_ids.is_empty() {
                    return Ok(0);
                }
                let placeholders: Vec<String> = matching_ids
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("?{}", i + 3))
                    .collect();
                let sql = format!(
                    "UPDATE session_approval_grants SET revoked_at = ?1, revoked_reason = ?2 WHERE id IN ({}) AND revoked_at IS NULL",
                    placeholders.join(", ")
                );
                let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> =
                    vec![Box::new(now.clone()), Box::new(reason.to_string())];
                for id in &matching_ids {
                    params_vec.push(Box::new(*id));
                }
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p.as_ref()).collect();
                conn.execute(&sql, params_refs.as_slice())?
            }
            None => conn.execute(
                "UPDATE session_approval_grants SET revoked_at = ?1, revoked_reason = ?2
                 WHERE root_session_id = ?3 AND revoked_at IS NULL",
                params![&now, reason, root_session_id],
            )?,
        };
        Ok(count)
    }

    /// Soft-revoke (set `revoked_at`) every ACTIVE grant whose
    /// `source_approval_id` matches `source`, scoped to `root_session_id`.
    /// Used by plan-as-capability-grant (Pillar C): when a plan's envelope
    /// expands, the grants materialized from the prior approved revision are
    /// withdrawn so the operator's re-approval re-materializes a clean
    /// envelope. Grants from other sources (explicit operator grants, other
    /// plans) are untouched. Returns the number revoked.
    pub fn revoke_session_grants_by_source(
        &self,
        root_session_id: &str,
        source: &str,
        reason: &str,
    ) -> Result<usize> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let count = conn.execute(
            "UPDATE session_approval_grants
             SET revoked_at = ?1, revoked_reason = ?2
             WHERE root_session_id = ?3
               AND source_approval_id = ?4
               AND revoked_at IS NULL",
            params![&now, reason, root_session_id, source],
        )?;
        Ok(count)
    }

    /// Soft-revoke a single ACTIVE session-approval grant by row id, scoped to
    /// the root session that owns it. Idempotent: a grant already revoked,
    /// missing, or owned by a different root reports `false`. Complements the
    /// by-host (`revoke_session_grants`) and by-source (`revoke_session_grants_by_source`)
    /// paths for the TUI grants panel's per-row revoke.
    ///
    /// SCOPING — see `revoke_grant_by_id` in `egress_declassification.rs`: row
    /// ids are `AUTOINCREMENT` and therefore enumerable, so the id alone must
    /// never authorize a revoke. The `root_session_id` predicate keeps one root
    /// from revoking another root's grant.
    pub fn revoke_session_grant_by_id(
        &self,
        root_session_id: &str,
        grant_id: i64,
        reason: &str,
    ) -> Result<bool> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let count = conn.execute(
            "UPDATE session_approval_grants
             SET revoked_at = ?1, revoked_reason = ?2
             WHERE id = ?3 AND root_session_id = ?4 AND revoked_at IS NULL",
            params![&now, reason, grant_id, root_session_id],
        )?;
        Ok(count > 0)
    }

    pub fn prune_expired_grants(&self) -> Result<usize> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let expired_ids: Vec<i64> = {
            let mut stmt = conn.prepare(
                "SELECT id FROM session_approval_grants WHERE expires_at IS NOT NULL AND expires_at < ?1",
            )?;
            let rows = stmt.query_map(params![now], |row| row.get(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        if expired_ids.is_empty() {
            return Ok(0);
        }
        let count = expired_ids.len();
        for gid in &expired_ids {
            let _ = conn.execute(
                "DELETE FROM session_approval_grant_targets WHERE grant_id = ?1",
                params![gid],
            );
        }
        for gid in &expired_ids {
            let _ = conn.execute(
                "DELETE FROM session_approval_grants WHERE id = ?1",
                params![gid],
            );
        }
        Ok(count)
    }

    /// Mark standalone (non-workflow) approvals whose `expires_at` has passed
    /// as `stale`. The approvals are NOT cancelled — they remain resolvable if
    /// the operator chooses to act — but they are surfaced as stale in
    /// `operator.pending`. Returns the IDs that changed.
    pub fn flag_expired_standalone_approvals(&self) -> Result<Vec<String>> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT request_id FROM approvals
             WHERE status = 'pending'
               AND workflow_id IS NULL
               AND task_id IS NULL
               AND expires_at IS NOT NULL
               AND expires_at < ?1",
        )?;
        let rows = stmt.query_map(params![now], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut flagged = Vec::new();
        for row in rows {
            let id = row?;
            let changed = conn.execute(
                "UPDATE approvals SET status = 'stale' WHERE request_id = ?1 AND status = 'pending'",
                params![id],
            )?;
            if changed > 0 {
                flagged.push(id);
            }
        }
        Ok(flagged)
    }

    pub fn get_approval_stats(
        &self,
        agent_id: Option<&str>,
        root_session_id: Option<&str>,
        since: Option<&str>,
    ) -> Result<serde_json::Value> {
        let conn = self.conn.lock().unwrap();

        let mut where_clauses = Vec::new();
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(aid) = agent_id {
            where_clauses.push(format!("agent_id = ?{}", param_values.len() + 1));
            param_values.push(Box::new(aid.to_string()));
        }
        if let Some(sid) = root_session_id {
            where_clauses.push(format!("root_session_id = ?{}", param_values.len() + 1));
            param_values.push(Box::new(sid.to_string()));
        }
        if let Some(s) = since {
            where_clauses.push(format!("created_at >= ?{}", param_values.len() + 1));
            param_values.push(Box::new(s.to_string()));
        }

        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM approvals {}", where_sql),
            params_refs.as_slice(),
            |row| row.get(0),
        )?;

        let approved_where = if where_sql.is_empty() {
            "WHERE status = 'approved'".to_string()
        } else {
            format!("{} AND status = 'approved'", where_sql)
        };
        let approved: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM approvals {}", approved_where),
            params_refs.as_slice(),
            |row| row.get(0),
        )?;

        let rejected_where = if where_sql.is_empty() {
            "WHERE status = 'rejected'".to_string()
        } else {
            format!("{} AND status = 'rejected'", where_sql)
        };
        let rejected: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM approvals {}", rejected_where),
            params_refs.as_slice(),
            |row| row.get(0),
        )?;

        let pending_where = if where_sql.is_empty() {
            "WHERE status IS NULL".to_string()
        } else {
            format!("{} AND status IS NULL", where_sql)
        };
        let pending: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM approvals {}", pending_where),
            params_refs.as_slice(),
            |row| row.get(0),
        )?;

        let mut top_agents_stmt = conn.prepare(
            &format!("SELECT agent_id, COUNT(*) as cnt FROM approvals {} GROUP BY agent_id ORDER BY cnt DESC LIMIT 10",
                if where_sql.is_empty() { String::new() } else { where_sql.clone() }),
        )?;
        let top_agents: Vec<serde_json::Value> = top_agents_stmt
            .query_map(params_refs.as_slice(), |row| {
                let agent_id: String = row.get(0)?;
                let count: i64 = row.get(1)?;
                Ok(serde_json::json!({"agent_id": agent_id, "count": count}))
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(serde_json::json!({
            "total": total,
            "approved": approved,
            "rejected": rejected,
            "pending": pending,
            "approval_rate": if total > 0 { format!("{:.1}%", (approved as f64 / total as f64) * 100.0) } else { "N/A".to_string() },
            "rejection_rate": if total > 0 { format!("{:.1}%", (rejected as f64 / total as f64) * 100.0) } else { "N/A".to_string() },
            "top_agents": top_agents,
        }))
    }
}

/// Fetch `(root_session_id, session_id, agent_id)` for a resolved approval so
/// the timeline event can be attributed to the requesting session. Returns
/// `None` (skipping emission) if the row can't be read.
fn resolution_context(
    conn: &rusqlite::Connection,
    request_id: &str,
) -> Option<(Option<String>, String, String)> {
    conn.query_row(
        "SELECT root_session_id, session_id, agent_id FROM approvals WHERE request_id = ?1",
        params![request_id],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )
    .ok()
}

#[cfg(test)]
mod decided_by_kind_tests {
    use super::GatewayStore;
    use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
    use tempfile::tempdir;

    fn pending(id: &str) -> ApprovalRequest {
        ApprovalRequest {
            request_id: id.to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "s1".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "echo hi".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
                intent: None,
            },
            approval_level: ApprovalLevel::Operator,
            created_at: "2026-06-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
            expires_at: None,
        }
    }

    fn stored_kind(store: &GatewayStore, id: &str) -> Option<String> {
        store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT decided_by_kind FROM approvals WHERE request_id = ?1",
                    [id],
                    |r| r.get::<_, Option<String>>(0),
                )?)
            })
            .unwrap()
    }

    #[test]
    fn record_decision_persists_derived_decider_kind() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        // Operator (human) decision.
        let mut a = pending("apr-h");
        store.create_approval(&mut a).unwrap();
        store
            .record_decision(
                "apr-h",
                "approved",
                "operator",
                "2026-06-01T01:00:00Z",
                None,
            )
            .unwrap();
        assert_eq!(stored_kind(&store, "apr-h").as_deref(), Some("human"));

        // Agent-decider decision.
        let mut b = pending("apr-a");
        store.create_approval(&mut b).unwrap();
        store
            .record_decision(
                "apr-a",
                "approved",
                "auditor.default",
                "2026-06-01T01:00:00Z",
                None,
            )
            .unwrap();
        assert_eq!(
            stored_kind(&store, "apr-a").as_deref(),
            Some("autonoetic_agent")
        );

        // Mechanical cancel ⇒ no principal kind.
        let mut c = pending("apr-g");
        store.create_approval(&mut c).unwrap();
        store
            .cancel_approval("apr-g", "gateway", "2026-06-01T01:00:00Z")
            .unwrap();
        assert_eq!(stored_kind(&store, "apr-g"), None);

        // Production emergency-stop cascade resolves via record_decision with a
        // "emergency_stop:<id>" decider — mechanical, must NOT be an agent (#374 review).
        let mut d = pending("apr-es");
        store.create_approval(&mut d).unwrap();
        store
            .record_decision(
                "apr-es",
                "cancelled",
                "emergency_stop:estop-1a2b3c4d",
                "2026-06-01T01:00:00Z",
                None,
            )
            .unwrap();
        assert_eq!(stored_kind(&store, "apr-es"), None);
    }

    #[test]
    fn record_decision_emits_resolution_onto_timeline() {
        use autonoetic_types::principal::PrincipalKind;
        use autonoetic_types::session_timeline::SessionRole;

        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        let mut a = pending("apr-r");
        store.create_approval(&mut a).unwrap();
        store
            .record_decision(
                "apr-r",
                "rejected",
                "operator",
                "2026-06-01T01:00:00Z",
                Some("out of scope"),
            )
            .unwrap();

        // session_id "s1" is the root (pending() leaves root_session_id None).
        let tl = store
            .list_session_timeline("s1", None, 50, None, None)
            .unwrap();
        let ev = tl
            .entries
            .iter()
            .find(|e| e.event_type == "approval.rejected")
            .expect("resolution event on timeline");
        assert_eq!(ev.principal.kind, PrincipalKind::Human);
        assert_eq!(ev.role, SessionRole::Operator);
        // A rejection draws attention.
        assert_eq!(
            ev.altitude,
            autonoetic_types::session_timeline::Altitude::Attention
        );
        assert_eq!(ev.refs.approval_request_id.as_deref(), Some("apr-r"));
    }

    #[test]
    fn find_matching_revision_promote_approval_matches_delta() {
        use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        let mut req = ApprovalRequest {
            request_id: "apr-rp-1".to_string(),
            agent_id: "planner.default".to_string(),
            session_id: "s1".to_string(),
            root_session_id: Some("root-1".to_string()),
            action: ScheduledAction::RevisionPromote {
                agent_id: "weather-agent".to_string(),
                revision_id: "rev-abc".to_string(),
                outgoing_revision_id: "".to_string(),
                added_capabilities: vec!["NetworkAccess".to_string()],
                broadened_capabilities: vec![],
                payload: None,
                federation_context: None,
            },
            approval_level: ApprovalLevel::Operator,
            created_at: "2026-06-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
        };
        store.create_approval(&mut req).unwrap();

        // Exact match returns the pending approval.
        let matched = store
            .find_matching_revision_promote_approval_for_root(
                "root-1",
                "weather-agent",
                "rev-abc",
                &["NetworkAccess".to_string()],
                &[],
                "",
            )
            .unwrap();
        assert!(matched.is_some());
        let matched = matched.unwrap();
        assert_eq!(matched.request_id, "apr-rp-1");
        assert_eq!(matched.status, None);

        // Different capability set does not match.
        let not_matched = store
            .find_matching_revision_promote_approval_for_root(
                "root-1",
                "weather-agent",
                "rev-abc",
                &["FileWrite".to_string()],
                &[],
                "",
            )
            .unwrap();
        assert!(not_matched.is_none());
    }

    #[test]
    fn find_matching_revision_promote_approval_sees_approved_status() {
        use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        let mut req = ApprovalRequest {
            request_id: "apr-rp-2".to_string(),
            agent_id: "planner.default".to_string(),
            session_id: "s1".to_string(),
            root_session_id: Some("root-2".to_string()),
            action: ScheduledAction::RevisionPromote {
                agent_id: "weather-agent".to_string(),
                revision_id: "rev-def".to_string(),
                outgoing_revision_id: "".to_string(),
                added_capabilities: vec!["NetworkAccess".to_string()],
                broadened_capabilities: vec![],
                payload: None,
                federation_context: None,
            },
            approval_level: ApprovalLevel::Operator,
            created_at: "2026-06-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
        };
        store.create_approval(&mut req).unwrap();
        store
            .record_decision(
                "apr-rp-2",
                "approved",
                "operator",
                "2026-06-01T01:00:00Z",
                None,
            )
            .unwrap();

        let matched = store
            .find_matching_revision_promote_approval_for_root(
                "root-2",
                "weather-agent",
                "rev-def",
                &["NetworkAccess".to_string()],
                &[],
                "",
            )
            .unwrap();
        assert_eq!(
            matched.unwrap().status,
            Some(autonoetic_types::background::ApprovalStatus::Approved)
        );
    }

    #[test]
    fn find_matching_revision_promote_approval_for_identity_matches_across_sessions() {
        use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        let mut req = ApprovalRequest {
            request_id: "apr-identity-1".to_string(),
            agent_id: "specialized_builder.default".to_string(),
            session_id: "s-child".to_string(),
            root_session_id: Some("root-1".to_string()),
            action: ScheduledAction::RevisionPromote {
                agent_id: "weather-agent".to_string(),
                revision_id: "rev-abc".to_string(),
                outgoing_revision_id: "rev-out".to_string(),
                added_capabilities: vec!["NetworkAccess".to_string()],
                broadened_capabilities: vec!["FileRead".to_string()],
                payload: None,
                federation_context: None,
            },
            approval_level: ApprovalLevel::Operator,
            created_at: "2026-06-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
        };
        store.create_approval(&mut req).unwrap();

        // Exact identity match within the root session.
        let matched = store
            .find_matching_revision_promote_approval_for_identity(
                "root-1",
                "weather-agent",
                "rev-abc",
                "rev-out",
                &["NetworkAccess".to_string()],
                &["FileRead".to_string()],
            )
            .unwrap();
        let matched = matched.expect("identity match must be found");
        assert_eq!(matched.request_id, "apr-identity-1");
        // Pending surfaces as None (the query's catch-all), like the
        // for_root variant.
        assert_eq!(matched.status, None);

        // Cross-root: the approval was filed under root-1; a lookup from
        // another root must NOT see it (a different operator context must
        // not suppress a fresh decision).
        let cross_root = store
            .find_matching_revision_promote_approval_for_identity(
                "root-other",
                "weather-agent",
                "rev-abc",
                "rev-out",
                &["NetworkAccess".to_string()],
                &["FileRead".to_string()],
            )
            .unwrap();
        assert!(cross_root.is_none());

        // Same revision but different outgoing baseline must NOT match — a
        // stale approval acknowledged against a different alias state.
        let wrong_outgoing = store
            .find_matching_revision_promote_approval_for_identity(
                "root-1",
                "weather-agent",
                "rev-abc",
                "rev-other",
                &["NetworkAccess".to_string()],
                &["FileRead".to_string()],
            )
            .unwrap();
        assert!(wrong_outgoing.is_none());

        // Same identity but different capability set must NOT match.
        let wrong_caps = store
            .find_matching_revision_promote_approval_for_identity(
                "root-1",
                "weather-agent",
                "rev-abc",
                "rev-out",
                &["FileWrite".to_string()],
                &["FileRead".to_string()],
            )
            .unwrap();
        assert!(wrong_caps.is_none());

        // Rejected decisions are visible too (the escalation side refuses to
        // re-ask on those).
        store
            .record_decision(
                "apr-identity-1",
                "rejected",
                "operator",
                "2026-06-01T01:00:00Z",
                None,
            )
            .unwrap();
        let rejected = store
            .find_matching_revision_promote_approval_for_identity(
                "root-1",
                "weather-agent",
                "rev-abc",
                "rev-out",
                &["NetworkAccess".to_string()],
                &["FileRead".to_string()],
            )
            .unwrap();
        assert_eq!(
            rejected.unwrap().status,
            Some(autonoetic_types::background::ApprovalStatus::Rejected)
        );
    }
}
