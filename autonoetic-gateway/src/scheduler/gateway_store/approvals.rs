use anyhow::Result;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, GrantScope, GrantTarget, SessionApprovalGrant,
};
use rusqlite::{params, Connection, OptionalExtension};

use super::GatewayStore;

impl GatewayStore {
    pub fn create_approval(&self, request: &ApprovalRequest) -> Result<()> {
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
        conn.execute(
            "INSERT INTO approvals (
                request_id, agent_id, session_id, root_session_id, workflow_id, task_id,
                action_type, action_payload, reason, evidence_ref, status, created_at,
                approval_level, similar_to_request_id, similarity_score
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
            ],
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
            "SELECT request_id, agent_id, session_id, action_payload, created_at, workflow_id, task_id, root_session_id, status, decided_at, decided_by, reason, evidence_ref, approval_level, decision_reason, similar_to_request_id, similarity_score FROM approvals WHERE request_id = ?1",
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
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE approvals SET status = ?1, decided_by = ?2, decided_at = ?3, decision_reason = ?4 WHERE request_id = ?5 AND status = 'pending'",
            params![status, decided_by, decided_at, decision_reason, request_id],
        )?;
        if rows == 0 {
            anyhow::bail!(
                "Approval {} is no longer pending (already decided or not found)",
                request_id
            );
        }
        Ok(())
    }

    pub fn cancel_approval(
        &self,
        request_id: &str,
        cancelled_by: &str,
        cancelled_at: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE approvals SET status = 'cancelled', decided_by = ?1, decided_at = ?2 WHERE request_id = ?3 AND status = 'pending'",
            params![cancelled_by, cancelled_at, request_id],
        )?;
        if rows == 0 {
            anyhow::bail!(
                "Approval {} is no longer pending (already decided or not found)",
                request_id
            );
        }
        Ok(())
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
        let now = chrono::Utc::now();
        let conn = self.conn.lock().unwrap();
        let expired_ids: Vec<i64> = {
            let mut stmt = conn.prepare(
                "SELECT id, expires_at FROM session_approval_grants WHERE expires_at IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |row| {
                let id: i64 = row.get(0)?;
                let expires_at: String = row.get(1)?;
                Ok((id, expires_at))
            })?;
            rows.filter_map(|r| r.ok())
                .filter(|(_, exp)| {
                    chrono::DateTime::parse_from_rfc3339(exp)
                        .map(|dt| dt.with_timezone(&chrono::Utc) < now)
                        .unwrap_or(false)
                })
                .map(|(id, _)| id)
                .collect()
        };
        for gid in &expired_ids {
            let _ = conn.execute(
                "DELETE FROM session_approval_grant_targets WHERE grant_id = ?1",
                params![gid],
            );
        }
        let count = expired_ids.len();
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
