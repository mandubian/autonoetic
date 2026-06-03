use anyhow::Result;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, GrantScope, GrantTarget, SessionApprovalGrant,
};
use rusqlite::{params, Connection, OptionalExtension};

use super::GatewayStore;

impl GatewayStore {
    pub fn create_approval(&self, request: &mut ApprovalRequest) -> Result<()> {
        crate::scheduler::approval_hardening::enrich_request(request);
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
                    anyhow::bail!(
                        "approval_flood: root session '{}' already has {} pending approvals (cap {})",
                        root_session_id,
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
        conn.execute(
            "INSERT INTO approvals (
                request_id, agent_id, session_id, root_session_id, workflow_id, task_id,
                action_type, action_payload, reason, evidence_ref, status, created_at,
                approval_level, similar_to_request_id, similarity_score,
                min_dwell_ms, confirm_phrase, code_excerpts, risk_summary
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
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
                request.similar_to_request_id,
                request.similarity_score,
                request.min_dwell_ms,
                request.confirm_phrase,
                code_excerpts_json,
                risk_summary_json,
            ],
        )?;
        Ok(())
    }

    pub fn set_approval_code_excerpts(
        &self,
        request_id: &str,
        code_excerpts: Option<&[autonoetic_types::background::CodeExcerpt]>,
        risk_summary: Option<&autonoetic_types::background::RiskSummary>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let code_json = code_excerpts
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let risk_json = risk_summary
            .map(|v| serde_json::to_string(v).unwrap_or_default());
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
            "SELECT request_id, agent_id, session_id, action_payload, created_at, workflow_id, task_id, root_session_id, status, decided_at, decided_by, reason, evidence_ref, approval_level, decision_reason, similar_to_request_id, similarity_score, min_dwell_ms, confirm_phrase, code_excerpts, risk_summary FROM approvals WHERE request_id = ?1",
            params![request_id],
            |row| {
                let action_payload: String = row.get(3)?;
                let status_str: Option<String> = row.get(8)?;
                let status = status_str.and_then(|s| match s.as_str() {
                    "approved" => Some(autonoetic_types::background::ApprovalStatus::Approved),
                    "rejected" => Some(autonoetic_types::background::ApprovalStatus::Rejected),
                    "cancelled" => Some(autonoetic_types::background::ApprovalStatus::Cancelled),
                    _ => None,
                });
                let action = serde_json::from_str(&action_payload).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
                })?;
                let level_str: String = row.get(13)?;
                let approval_level: ApprovalLevel = serde_json::from_str(&level_str).unwrap_or(ApprovalLevel::Operator);
                let code_excerpts_json: Option<String> = row.get(19)?;
                let risk_summary_json: Option<String> = row.get(20)?;
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
                    similar_to_request_id: row.get(15)?,
                    similarity_score: row.get(16)?,
                    min_dwell_ms: row.get(17)?,
                    confirm_phrase: row.get(18)?,
                    code_excerpts,
                    risk_summary,
                })
            },
        ).optional().map_err(Into::into)
    }

    pub fn get_approval(&self, request_id: &str) -> Result<Option<ApprovalRequest>> {
        let conn = self.conn.lock().unwrap();
        Self::get_approval_with_conn(&conn, request_id)
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
                "UPDATE approvals SET status = ?1, decided_by = ?2, decided_at = ?3, decision_reason = ?4, decided_by_kind = ?5 WHERE request_id = ?6 AND status = 'pending'",
                params![status, decided_by, decided_at, decision_reason, decided_by_kind, request_id],
            )?;
            if rows == 0 {
                anyhow::bail!(
                    "Approval {} is no longer pending (already decided or not found)",
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
        use autonoetic_types::session_timeline::{Altitude, TimelineRefs};

        let Some((root, session, _agent)) = ctx else {
            return;
        };
        let (event_type, altitude) = match status {
            "approved" => ("approval.approved", Altitude::Normal),
            "rejected" | "denied" => ("approval.rejected", Altitude::Attention),
            "cancelled" => ("approval.cancelled", Altitude::Detail),
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
            Some(altitude),
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

    pub fn list_all_approvals_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<ApprovalRequest>> {
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
        required_targets: &[String],
    ) -> bool {
        self.grants_cover_targets(root_session_id, root_session_id, required_targets)
    }

    /// Scope-aware grant coverage check.  A request is covered when every
    /// required target is matched by at least one active (non-revoked,
    /// non-expired) grant whose scope covers the requesting session.
    pub fn grants_cover_targets(
        &self,
        session_id: &str,
        root_session_id: &str,
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
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
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
            .record_decision("apr-h", "approved", "operator", "2026-06-01T01:00:00Z", None)
            .unwrap();
        assert_eq!(stored_kind(&store, "apr-h").as_deref(), Some("human"));

        // Agent-decider decision.
        let mut b = pending("apr-a");
        store.create_approval(&mut b).unwrap();
        store
            .record_decision("apr-a", "approved", "auditor.default", "2026-06-01T01:00:00Z", None)
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
            .record_decision("apr-r", "rejected", "operator", "2026-06-01T01:00:00Z", Some("out of scope"))
            .unwrap();

        // session_id "s1" is the root (pending() leaves root_session_id None).
        let tl = store.list_session_timeline("s1", None, 50, None, None).unwrap();
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
    fn resolution_attribution_for_agent_and_mechanical_branches() {
        use autonoetic_types::principal::PrincipalKind;
        use autonoetic_types::session_timeline::{Altitude, SessionRole};

        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        // Agent-decider (agent: prefix) approval.
        let mut a = pending("apr-ag");
        store.create_approval(&mut a).unwrap();
        store
            .record_decision("apr-ag", "approved", "agent:auditor.default", "2026-06-01T01:00:00Z", None)
            .unwrap();

        // Mechanical emergency-stop cancel via record_decision.
        let mut m = pending("apr-mech");
        store.create_approval(&mut m).unwrap();
        store
            .record_decision("apr-mech", "cancelled", "emergency_stop:estop-1a2b3c4d", "2026-06-01T01:00:01Z", None)
            .unwrap();

        let tl = store.list_session_timeline("s1", None, 50, None, None).unwrap();

        let agent_ev = tl.entries.iter().find(|e| e.event_type == "approval.approved").unwrap();
        assert_eq!(agent_ev.principal.kind, PrincipalKind::AutonoeticAgent);
        assert_eq!(agent_ev.principal.id, "auditor.default"); // prefix stripped
        assert_eq!(agent_ev.role, SessionRole::Auditor);

        let mech_ev = tl.entries.iter().find(|e| e.event_type == "approval.cancelled").unwrap();
        assert_eq!(mech_ev.role, SessionRole::Runtime);
        assert_eq!(mech_ev.altitude, Altitude::Detail); // hidable
    }
}
