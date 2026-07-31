use super::{GatewayStore, LiveDigestEventRecord, LIVE_DIGEST_BUFFER_CAPACITY};
use super::util::decode_egress_label_json;
use anyhow::Result;
use autonoetic_types::config::RetentionConfig;
use rusqlite::params;
use std::collections::BTreeMap;

fn execution_trace_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<autonoetic_types::causal_chain::ExecutionTraceRecord> {
    let egress_raw: Option<String> = row.get(19).ok().flatten();
    Ok(autonoetic_types::causal_chain::ExecutionTraceRecord {
        trace_id: row.get(0)?,
        event_id: row.get(1)?,
        agent_id: row.get(2)?,
        session_id: row.get(3)?,
        turn_id: row.get(4)?,
        timestamp: row.get(5)?,
        tool_name: row.get(6)?,
        command: row.get(7)?,
        exit_code: row.get(8)?,
        stdout: row.get(9)?,
        stderr: row.get(10)?,
        duration_ms: row.get(11)?,
        success: row.get(12)?,
        error_type: row.get(13)?,
        error_summary: row.get(14)?,
        approval_required: row.get(15)?,
        approval_request_id: row.get(16)?,
        arguments: row.get(17)?,
        result: row.get(18)?,
        egress_label: decode_egress_label_json(egress_raw)?,
    })
}

fn looks_like_fts_syntax(query: &str) -> bool {
    query.chars().any(|c| {
        matches!(c, '.' | '(' | ')' | '"' | '*' | '-' | '+' | '&' | ':')
    })
}

fn should_fallback_to_like(err: &rusqlite::Error, query: &str) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.extended_code == rusqlite::ffi::SQLITE_ERROR && looks_like_fts_syntax(query)
    )
}

fn is_sqlite_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<rusqlite::Error>()
        .map(|e| matches!(e, rusqlite::Error::SqliteFailure(_, _)))
        .unwrap_or(false)
}

/// Per-agent tally for the **civic-health** view (#772 E.2 / #771 D.2). See
/// [`GatewayStore::civic_health`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CivicHealthEntry {
    pub agent_id: String,
    pub proposals_filed: u64,
    pub proposals_pending: u64,
    pub flags_filed: u64,
    pub flags_pending: u64,
    /// #771 D.2: mechanical amendment invitations issued to this agent
    /// (all-time in window) and how many are open vs answered.
    pub invitations_issued: u64,
    pub invitations_open: u64,
    pub invitations_answered: u64,
}

/// Standing civic-health view: how often each agent exercises its civic
/// affordances (constitutional proposals, anomaly flags), and how much of
/// that is still awaiting a decision. See [`GatewayStore::civic_health`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CivicHealth {
    /// Sorted by (proposals_filed + flags_filed) desc, then agent_id asc.
    pub by_agent: Vec<CivicHealthEntry>,
}

impl GatewayStore {
    /// Prune execution_traces older than `days`. 0 = no pruning.
    pub fn prune_execution_traces(&self, days: u32) -> Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        self.prune_execution_traces_with_cutoff(&Some(cutoff))
    }

    /// Prune execution_traces older than cutoff. None = no pruning.
    pub fn prune_execution_traces_with_cutoff(&self, cutoff: &Option<String>) -> Result<u64> {
        let Some(cutoff) = cutoff else {
            return Ok(0);
        };
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM execution_traces WHERE timestamp < ?1",
            params![cutoff],
        )?;
        Ok(n as u64)
    }

    /// Prune causal_events older than `days`. 0 = no pruning.
    pub fn prune_causal_events(&self, days: u32) -> Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        self.prune_causal_events_with_cutoff(&Some(cutoff))
    }

    /// Prune causal_events older than cutoff. None = no pruning.
    pub fn prune_causal_events_with_cutoff(&self, cutoff: &Option<String>) -> Result<u64> {
        let Some(cutoff) = cutoff else {
            return Ok(0);
        };
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM causal_events WHERE timestamp < ?1",
            params![cutoff],
        )?;
        Ok(n as u64)
    }

    /// Apply retention policy from config. Call once on gateway startup.
    /// Emits a `retention.pruned` causal event with counts of pruned rows.
    pub fn apply_retention_policy(&self, retention: &RetentionConfig) -> Result<()> {
        let now = chrono::Utc::now();

        let traces_cutoff = if retention.execution_traces_days > 0 {
            Some(
                (now - chrono::Duration::days(retention.execution_traces_days as i64)).to_rfc3339(),
            )
        } else {
            None
        };
        let events_cutoff = if retention.causal_events_days > 0 {
            Some((now - chrono::Duration::days(retention.causal_events_days as i64)).to_rfc3339())
        } else {
            None
        };

        let traces_pruned = match self.prune_execution_traces_with_cutoff(&traces_cutoff) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    target: "gateway_store",
                    error = %e,
                    "Failed to prune execution_traces"
                );
                0
            }
        };
        let events_pruned = match self.prune_causal_events_with_cutoff(&events_cutoff) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    target: "gateway_store",
                    error = %e,
                    "Failed to prune causal_events"
                );
                0
            }
        };

        if traces_pruned > 0 || events_pruned > 0 {
            let mut rules = autonoetic_types::causal_chain::default_enforced_rules();
            rules.push("P-8.17".to_string());

            let payload = serde_json::json!({
                "execution_traces_pruned": traces_pruned,
                "causal_events_pruned": events_pruned,
                "execution_traces_cutoff": traces_cutoff,
                "causal_events_cutoff": events_cutoff,
                "retention_config": {
                    "execution_traces_days": retention.execution_traces_days,
                    "causal_events_days": retention.causal_events_days,
                },
            });
            if let Err(e) =
                self.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    agent_id: "gateway".to_string(),
                    session_id: "system".to_string(),
                    turn_id: None,
                    event_seq: now.timestamp_millis().max(0) as u64,
                    timestamp: now.to_rfc3339(),
                    category: "retention".to_string(),
                    action: "pruned".to_string(),
                    status: autonoetic_types::causal_chain::EntryStatus::Success.to_string(),
                    enforced_rules: rules,
                    target: None,
                    payload: serde_json::to_string(&payload).ok(),
                    payload_ref: None,
                    evidence_ref: None,
                    reason: None,
                })
            {
                tracing::warn!(
                    target: "gateway_store",
                    error = %e,
                    "Failed to record retention.pruned causal event"
                );
            }
        }

        Ok(())
    }

    pub fn emit_vault_key_probe_event(&self, result: &crate::vault::KeyProbeResult) {
        let now = chrono::Utc::now();
        let mut rules = autonoetic_types::causal_chain::default_enforced_rules();
        rules.push("R+8".to_string());
        let (status, reason, payload) = match result {
            crate::vault::KeyProbeResult::Present { source } => (
                autonoetic_types::causal_chain::EntryStatus::Success.to_string(),
                format!("Vault key available via {}", match source {
                    crate::vault::KeySource::EnvVar => "AUTONOETIC_VAULT_KEY",
                    crate::vault::KeySource::FilePath => "AUTONOETIC_VAULT_KEY_PATH",
                    crate::vault::KeySource::AutoGenerated => "auto-generated vault.key",
                }),
                serde_json::json!({ "source": match source {
                    crate::vault::KeySource::EnvVar => "env_var",
                    crate::vault::KeySource::FilePath => "file_path",
                    crate::vault::KeySource::AutoGenerated => "auto_generated",
                }}),
            ),
            crate::vault::KeyProbeResult::Missing { source, path } => (
                "ERROR".to_string(),
                format!("Vault key missing: {} points to non-existent file {}", 
                    match source {
                        crate::vault::KeySource::EnvVar => "AUTONOETIC_VAULT_KEY",
                        crate::vault::KeySource::FilePath => "AUTONOETIC_VAULT_KEY_PATH",
                        crate::vault::KeySource::AutoGenerated => "auto-generated",
                    },
                    path),
                serde_json::json!({
                    "source": match source {
                        crate::vault::KeySource::EnvVar => "env_var",
                        crate::vault::KeySource::FilePath => "file_path",
                        crate::vault::KeySource::AutoGenerated => "auto_generated",
                    },
                    "path": path,
                }),
            ),
            crate::vault::KeyProbeResult::Invalid { source, reason: detail } => (
                "ERROR".to_string(),
                format!("Vault key invalid ({}): {}", 
                    match source {
                        crate::vault::KeySource::EnvVar => "AUTONOETIC_VAULT_KEY",
                        crate::vault::KeySource::FilePath => "AUTONOETIC_VAULT_KEY_PATH",
                        crate::vault::KeySource::AutoGenerated => "auto-generated",
                    },
                    detail),
                serde_json::json!({
                    "source": match source {
                        crate::vault::KeySource::EnvVar => "env_var",
                        crate::vault::KeySource::FilePath => "file_path",
                        crate::vault::KeySource::AutoGenerated => "auto_generated",
                    },
                    "reason": detail,
                }),
            ),
            crate::vault::KeyProbeResult::NotConfigured => (
                "ERROR".to_string(),
                "Vault key not configured: no AUTONOETIC_VAULT_KEY, AUTONOETIC_VAULT_KEY_PATH, or auto-generated vault.key found".to_string(),
                serde_json::json!({ "source": "none" }),
            ),
        };

        if let Err(e) =
            self.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
                event_id: uuid::Uuid::new_v4().to_string(),
                agent_id: "gateway".to_string(),
                session_id: "system".to_string(),
                turn_id: None,
                event_seq: now.timestamp_millis().max(0) as u64,
                timestamp: now.to_rfc3339(),
                category: "vault".to_string(),
                action: "key_probe".to_string(),
                status,
                enforced_rules: rules,
                target: None,
                payload: serde_json::to_string(&payload).ok(),
                payload_ref: None,
                evidence_ref: None,
                reason: Some(reason),
            })
        {
            tracing::warn!(
                target: "gateway_store",
                error = %e,
                "Failed to record vault.key_probe causal event"
            );
        }
    }

    pub fn create_causal_event(
        &self,
        event: &autonoetic_types::causal_chain::CausalEventRecord,
    ) -> Result<()> {
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO causal_events (
                event_id, agent_id, session_id, turn_id, event_seq, timestamp,
                category, action, status, enforced_rules, target, payload, payload_ref, evidence_ref, reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    &event.event_id,
                    &event.agent_id,
                    &event.session_id,
                    event.turn_id.as_deref(),
                    event.event_seq as i64,
                    &event.timestamp,
                    &event.category,
                    &event.action,
                    &event.status,
                    serde_json::to_string(&event.enforced_rules)?,
                    event.target.as_deref(),
                    event.payload.as_deref(),
                    event.payload_ref.as_deref(),
                    event.evidence_ref.as_deref(),
                    event.reason.as_deref(),
                ],
            )?;
        }
        if let Ok(guard) = self.policy_hook_executor.lock() {
            if let Some(w) = guard.as_ref() {
                if let Some(exec) = w.upgrade() {
                    exec.maybe_dispatch_policy_decision_hook(event);
                }
            }
        }
        // Mirror `egress.*` causal events onto the room timeline (#972): the
        // causal event is the one durable record of egress enforcement, so
        // the live-digest row is derived here rather than at each of the many
        // emission sites. Best-effort — a timeline write failure must never
        // fail the causal event itself.
        if event.category == "egress" {
            if let Some(record) =
                crate::runtime::session_timeline::egress_causal_event_to_timeline(event)
            {
                if let Err(e) = self.create_live_digest_event(&record) {
                    tracing::warn!(
                        target: "session_timeline",
                        error = %e,
                        action = %event.action,
                        "egress timeline mirror emit failed"
                    );
                }
            }
        }
        Ok(())
    }

    /// Query causal events with filters.
    pub fn search_causal_events(
        &self,
        session_id: Option<&str>,
        agent_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::causal_chain::CausalEventRecord>> {
        let conn = self.conn.lock().unwrap();

        let mut conditions = Vec::new();
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        let mut param_idx = 1;

        if let Some(sid) = session_id {
            conditions.push("session_id = ?");
            params.push(rusqlite::types::Value::Text(sid.to_string()));
            param_idx += 1;
        }

        if let Some(aid) = agent_id {
            conditions.push("agent_id = ?");
            params.push(rusqlite::types::Value::Text(aid.to_string()));
            param_idx += 1;
        }

        let where_clause = if conditions.is_empty() {
            "1".to_string()
        } else {
            conditions.join(" AND ")
        };

        let query = format!(
            "SELECT event_id, agent_id, session_id, turn_id, event_seq, timestamp, category, action, status, enforced_rules, target, payload, payload_ref, evidence_ref, reason FROM causal_events WHERE {} ORDER BY timestamp DESC LIMIT ?{}",
            where_clause, param_idx
        );

        let mut stmt = conn.prepare(&query)?;
        let mut params_with_limit = params.clone();
        params_with_limit.push(rusqlite::types::Value::Integer(limit));

        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_with_limit.iter()),
            |row| {
                let enforced_rules_json: String = row.get(9)?;
                let enforced_rules = Some(enforced_rules_json.as_str())
                    .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
                    .unwrap_or_else(autonoetic_types::causal_chain::default_enforced_rules);
                Ok(autonoetic_types::causal_chain::CausalEventRecord {
                    event_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    session_id: row.get(2)?,
                    turn_id: row.get(3)?,
                    event_seq: row.get::<_, i64>(4)? as u64,
                    timestamp: row.get(5)?,
                    category: row.get(6)?,
                    action: row.get(7)?,
                    status: row.get(8)?,
                    enforced_rules,
                    target: row.get(10)?,
                    payload: row.get(11)?,
                    payload_ref: row.get(12)?,
                    evidence_ref: row.get(13)?,
                    reason: row.get(14)?,
                })
            },
        )?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    /// Standing **contract-health** view (#302): tally how often each
    /// constitutional clause (principle/right) has been enforced, by reading
    /// the `enforced_rules` carried on causal events and attributing each
    /// rule/right ID to its owning clause via the enforcement register.
    ///
    /// The `R+++3` event-attribution placeholder (every event carries it by
    /// default) is skipped — only events that named a concrete rule/right
    /// contribute. Real rule IDs not yet in the register surface in
    /// `ContractHealth::unattributed`, keeping migration gaps visible rather
    /// than silently dropped.
    ///
    /// `since` is an optional RFC3339 lower bound on `timestamp`; `None` scans
    /// all retained events. The bound is parsed and compared by absolute
    /// instant (not raw text), so offset forms like `Z` vs `+02:00` behave
    /// correctly and malformed operator input fails clearly.
    pub fn contract_health(
        &self,
        since: Option<&str>,
    ) -> Result<crate::enforcement_register::ContractHealth> {
        // Parse + validate the bound up front: raw SQLite text comparison on
        // RFC3339 is wrong across offset forms, and an unparsed string would
        // silently yield empty/partial results instead of a clear error.
        let since_dt = match since {
            Some(ts) => Some(chrono::DateTime::parse_from_rfc3339(ts).map_err(|e| {
                anyhow::anyhow!("invalid `since` timestamp {ts:?}: {e} (expected RFC3339)")
            })?),
            None => None,
        };

        let conn = self.conn.lock().unwrap();
        let placeholder = autonoetic_types::causal_chain::RULE_ID_EVENT_ATTRIBUTION;

        let mut stmt = conn.prepare("SELECT enforced_rules, timestamp FROM causal_events")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;

        let mut rule_ids: Vec<String> = Vec::new();
        for r in rows {
            let (raw, ts) = r?;
            // Filter by absolute time when a bound is set. An event whose own
            // timestamp won't parse is kept rather than dropped — we never
            // hide enforcement activity over a formatting quirk.
            if let Some(bound) = since_dt {
                if let Ok(event_dt) = chrono::DateTime::parse_from_rfc3339(&ts) {
                    if event_dt < bound {
                        continue;
                    }
                }
            }
            // Each cell is a JSON array of rule/right IDs; tolerate malformed
            // rows by skipping rather than failing the whole tally.
            if let Ok(ids) = serde_json::from_str::<Vec<String>>(&raw) {
                rule_ids.extend(ids.into_iter().filter(|id| id != placeholder));
            }
        }

        Ok(crate::enforcement_register::contract_health(rule_ids))
    }

    /// Standing **civic-health** view (#772 E.2): the dual of contract-health.
    /// Contract-health measures whether the *gateway* honors the law;
    /// civic-health measures whether *agents* use it — tallying each agent's
    /// constitutional proposals and anomaly flags, filed vs still-pending, so
    /// both that agents exercise voice/witnessing and whether it is being
    /// answered are visible in one view.
    ///
    /// `since` is an optional RFC3339 lower bound on `created_at`, compared
    /// by absolute instant like `contract_health`; items whose own timestamp
    /// fails to parse are kept rather than dropped.
    ///
    /// Full scan over both tables streaming only the columns the tally needs
    /// (mirrors `contract_health` above) — deliberately no row cap: a hard
    /// `LIMIT` would silently truncate the view once a table grew past it
    /// and present partial counts as complete.
    pub fn civic_health(&self, since: Option<&str>) -> Result<CivicHealth> {
        let since_dt = match since {
            Some(ts) => Some(chrono::DateTime::parse_from_rfc3339(ts).map_err(|e| {
                anyhow::anyhow!("invalid `since` timestamp {ts:?}: {e} (expected RFC3339)")
            })?),
            None => None,
        };

        let in_window = |ts: &str| -> bool {
            match since_dt {
                Some(bound) => match chrono::DateTime::parse_from_rfc3339(ts) {
                    Ok(dt) => dt >= bound,
                    Err(_) => true,
                },
                None => true,
            }
        };

        let conn = self.conn.lock().unwrap();
        let mut by_agent: BTreeMap<String, CivicHealthEntry> = BTreeMap::new();

        {
            let mut stmt = conn.prepare(
                "SELECT proposer_agent_id, status, created_at FROM constitutional_proposals",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for r in rows {
                let (agent_id, status, created_at) = r?;
                if !in_window(&created_at) {
                    continue;
                }
                let entry = by_agent
                    .entry(agent_id.clone())
                    .or_insert_with(|| CivicHealthEntry {
                        agent_id,
                        proposals_filed: 0,
                        proposals_pending: 0,
                        flags_filed: 0,
                        flags_pending: 0,
                        invitations_issued: 0,
                        invitations_open: 0,
                        invitations_answered: 0,
                    });
                entry.proposals_filed += 1;
                if !super::constitutional_proposals::PROPOSAL_TERMINAL_DECISION_STATUSES
                    .contains(&status.as_str())
                {
                    entry.proposals_pending += 1;
                }
            }
        }

        {
            let mut stmt =
                conn.prepare("SELECT reporter_agent_id, status, created_at FROM anomaly_flags")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for r in rows {
                let (agent_id, status, created_at) = r?;
                if !in_window(&created_at) {
                    continue;
                }
                let entry = by_agent
                    .entry(agent_id.clone())
                    .or_insert_with(|| CivicHealthEntry {
                        agent_id,
                        proposals_filed: 0,
                        proposals_pending: 0,
                        flags_filed: 0,
                        flags_pending: 0,
                        invitations_issued: 0,
                        invitations_open: 0,
                        invitations_answered: 0,
                    });
                entry.flags_filed += 1;
                if !super::anomaly_flags::FLAG_TERMINAL_DECISION_STATUSES
                    .contains(&status.as_str())
                {
                    entry.flags_pending += 1;
                }
            }
        }

        {
            let mut stmt = conn.prepare(
                "SELECT agent_id, status, created_at FROM amendment_invitations",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for r in rows {
                let (agent_id, status, created_at) = r?;
                if !in_window(&created_at) {
                    continue;
                }
                let entry = by_agent
                    .entry(agent_id.clone())
                    .or_insert_with(|| CivicHealthEntry {
                        agent_id,
                        proposals_filed: 0,
                        proposals_pending: 0,
                        flags_filed: 0,
                        flags_pending: 0,
                        invitations_issued: 0,
                        invitations_open: 0,
                        invitations_answered: 0,
                    });
                entry.invitations_issued += 1;
                match status.as_str() {
                    "open" => entry.invitations_open += 1,
                    "answered" => entry.invitations_answered += 1,
                    _ => {}
                }
            }
        }

        let mut by_agent: Vec<CivicHealthEntry> = by_agent.into_values().collect();
        by_agent.sort_by(|a, b| {
            let a_total = a.proposals_filed + a.flags_filed;
            let b_total = b.proposals_filed + b.flags_filed;
            b_total
                .cmp(&a_total)
                .then_with(|| a.agent_id.cmp(&b.agent_id))
        });

        Ok(CivicHealth { by_agent })
    }

    /// **DISCRETION LEAK register** read side (#771 D.3): tally
    /// `discretion_leak` causal events by (rule, kind) — the "top leaks
    /// this window" standing agenda the steward office (RFC Part F) drafts
    /// amendments against. `since` is an optional RFC3339 lower bound,
    /// compared by absolute instant like `contract_health`; events whose
    /// own timestamp fails to parse are kept rather than dropped. Sorted
    /// by descending count, then (rule, kind) for stable output.
    pub fn discretion_leak_summary(
        &self,
        since: Option<&str>,
    ) -> Result<Vec<crate::runtime::discretion_leak::DiscretionLeakTally>> {
        let since_dt = match since {
            Some(ts) => Some(chrono::DateTime::parse_from_rfc3339(ts).map_err(|e| {
                anyhow::anyhow!("invalid `since` timestamp {ts:?}: {e} (expected RFC3339)")
            })?),
            None => None,
        };

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT enforced_rules, action, timestamp FROM causal_events \
             WHERE category = 'discretion_leak'",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        let mut counts: BTreeMap<(String, String), u64> = BTreeMap::new();
        for r in rows {
            let (raw_rules, kind, ts) = r?;
            if let Some(bound) = since_dt {
                if let Ok(event_dt) = chrono::DateTime::parse_from_rfc3339(&ts) {
                    if event_dt < bound {
                        continue;
                    }
                }
            }
            // Tolerate malformed rule cells by skipping the row rather than
            // failing the whole tally (mirrors `contract_health`).
            let Ok(rule_ids) = serde_json::from_str::<Vec<String>>(&raw_rules) else {
                continue;
            };
            for rule_id in rule_ids {
                *counts.entry((rule_id, kind.clone())).or_insert(0) += 1;
            }
        }

        let mut out: Vec<crate::runtime::discretion_leak::DiscretionLeakTally> = counts
            .into_iter()
            .map(
                |((rule_id, kind), count)| crate::runtime::discretion_leak::DiscretionLeakTally {
                    rule_id,
                    kind,
                    count,
                },
            )
            .collect();
        out.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.rule_id.cmp(&b.rule_id))
                .then_with(|| a.kind.cmp(&b.kind))
        });
        Ok(out)
    }

    /// List curator decision events (`category = 'curator'`, `action = 'decision'`)
    /// for a specific target URI/id. Used by operator workflows asking
    /// "why was memory X dropped" (issue #30). The underlying `idx_causal_target`
    /// index makes this an O(log n) point query.
    pub fn list_curator_decisions_by_target(
        &self,
        target: &str,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::causal_chain::CausalEventRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT event_id, agent_id, session_id, turn_id, event_seq, timestamp,
                    category, action, status, enforced_rules, target, payload,
                    payload_ref, evidence_ref, reason
             FROM causal_events
             WHERE category = 'curator' AND action = 'decision' AND target = ?1
             ORDER BY timestamp DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![target, limit], |row| {
            let enforced_rules_json: String = row.get(9)?;
            let enforced_rules = Some(enforced_rules_json.as_str())
                .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
                .unwrap_or_else(autonoetic_types::causal_chain::default_enforced_rules);
            Ok(autonoetic_types::causal_chain::CausalEventRecord {
                event_id: row.get(0)?,
                agent_id: row.get(1)?,
                session_id: row.get(2)?,
                turn_id: row.get(3)?,
                event_seq: row.get::<_, i64>(4)? as u64,
                timestamp: row.get(5)?,
                category: row.get(6)?,
                action: row.get(7)?,
                status: row.get(8)?,
                enforced_rules,
                target: row.get(10)?,
                payload: row.get(11)?,
                payload_ref: row.get(12)?,
                evidence_ref: row.get(13)?,
                reason: row.get(14)?,
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn get_execution_trace(
        &self,
        trace_id: &str,
    ) -> Result<Option<autonoetic_types::causal_chain::ExecutionTraceRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT trace_id, event_id, agent_id, session_id, turn_id, timestamp,
                tool_name, command, exit_code, stdout, stderr, duration_ms,
                success, error_type, error_summary, approval_required,
                approval_request_id, arguments, result, egress_label_json
             FROM execution_traces WHERE trace_id = ?1",
        )?;
        let mut rows = stmt.query(params![trace_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(execution_trace_from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn create_execution_trace(
        &self,
        trace: &autonoetic_types::causal_chain::ExecutionTraceRecord,
    ) -> Result<()> {
        let egress_label_json = match &trace.egress_label {
            Some(label) => Some(serde_json::to_string(label)?),
            None => None,
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO execution_traces (
                trace_id, event_id, agent_id, session_id, turn_id, timestamp,
                tool_name, command, exit_code, stdout, stderr, duration_ms,
                success, error_type, error_summary, approval_required, approval_request_id,
                arguments, result, egress_label_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                &trace.trace_id,
                trace.event_id.as_deref(),
                &trace.agent_id,
                &trace.session_id,
                trace.turn_id.as_deref(),
                &trace.timestamp,
                &trace.tool_name,
                trace.command.as_deref(),
                trace.exit_code,
                trace.stdout.as_deref(),
                trace.stderr.as_deref(),
                &trace.duration_ms,
                &trace.success,
                trace.error_type.as_deref(),
                trace.error_summary.as_deref(),
                trace.approval_required,
                trace.approval_request_id.as_deref(),
                trace.arguments.as_deref(),
                trace.result.as_deref(),
                egress_label_json,
            ],
        )?;
        Ok(())
    }

    pub fn create_live_digest_event(&self, event: &LiveDigestEventRecord) -> Result<()> {
        let mut buf = self.live_digest_buffer.lock().unwrap();
        buf.push(event.clone());
        if buf.len() >= LIVE_DIGEST_BUFFER_CAPACITY {
            drop(buf);
            return self.flush_live_digest_events();
        }
        Ok(())
    }

    pub fn search_execution_traces(
        &self,
        tool_name: Option<&str>,
        success: Option<bool>,
        error_type: Option<&str>,
        command_pattern: Option<&str>,
        agent_id: Option<&str>,
        session_branch: Option<&str>,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::causal_chain::ExecutionTraceRecord>> {
        let conn = self.conn.lock().unwrap();

        let mut conditions = Vec::new();
        let mut sql_params: Vec<rusqlite::types::Value> = Vec::new();
        let mut param_idx = 1;

        if let Some(name) = tool_name {
            conditions.push("tool_name = ?");
            sql_params.push(rusqlite::types::Value::Text(name.to_string()));
            param_idx += 1;
        }

        if let Some(s) = success {
            conditions.push("success = ?");
            sql_params.push(rusqlite::types::Value::Integer(if s { 1 } else { 0 }));
            param_idx += 1;
        }

        if let Some(et) = error_type {
            conditions.push("error_type = ?");
            sql_params.push(rusqlite::types::Value::Text(et.to_string()));
            param_idx += 1;
        }

        if let Some(pattern) = command_pattern {
            conditions.push("command LIKE ?");
            sql_params.push(rusqlite::types::Value::Text(format!("%{}%", pattern)));
            param_idx += 1;
        }

        if let Some(aid) = agent_id {
            conditions.push("agent_id = ?");
            sql_params.push(rusqlite::types::Value::Text(aid.to_string()));
            param_idx += 1;
        }

        if let Some(sid) = session_branch {
            conditions.push("(session_id = ? OR session_id LIKE ? ESCAPE '\\')");
            sql_params.push(rusqlite::types::Value::Text(sid.to_string()));
            let escaped = super::escape_sqlite_like_fragment(sid);
            sql_params.push(rusqlite::types::Value::Text(format!("{}/%", escaped)));
            param_idx += 2;
        }

        let where_clause = if conditions.is_empty() {
            "1".to_string()
        } else {
            format!("{}", conditions.join(" AND "))
        };

        let query = format!(
            "SELECT * FROM execution_traces WHERE {} ORDER BY timestamp DESC LIMIT ?{}",
            where_clause, param_idx
        );

        let mut stmt = conn.prepare(&query)?;
        let mut params_with_limit = sql_params.clone();
        params_with_limit.push(rusqlite::types::Value::Integer(limit));

        let rows = stmt.query_map(rusqlite::params_from_iter(params_with_limit), |row| {
            execution_trace_from_row(row)
        })?;

        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    /// Bulk-set `egress_label_json` on memories matching optional filters.
    /// Returns the number of rows updated.
    pub fn memory_relabel(
        &self,
        new_label: &autonoetic_types::egress::EgressLabel,
        scope: Option<&str>,
        only_unlabeled: bool,
    ) -> Result<u64> {
        let label_json = serde_json::to_string(new_label)?;
        let conn = self.conn.lock().unwrap();
        let n = match (scope, only_unlabeled) {
            (Some(s), true) => conn.execute(
                "UPDATE memories SET egress_label_json = ?1, updated_at = ?2
                 WHERE scope = ?3 AND (egress_label_json IS NULL OR egress_label_json = '')",
                params![label_json, chrono::Utc::now().to_rfc3339(), s],
            )?,
            (Some(s), false) => conn.execute(
                "UPDATE memories SET egress_label_json = ?1, updated_at = ?2 WHERE scope = ?3",
                params![label_json, chrono::Utc::now().to_rfc3339(), s],
            )?,
            (None, true) => conn.execute(
                "UPDATE memories SET egress_label_json = ?1, updated_at = ?2
                 WHERE egress_label_json IS NULL OR egress_label_json = ''",
                params![label_json, chrono::Utc::now().to_rfc3339()],
            )?,
            (None, false) => conn.execute(
                "UPDATE memories SET egress_label_json = ?1, updated_at = ?2",
                params![label_json, chrono::Utc::now().to_rfc3339()],
            )?,
        };
        Ok(n as u64)
    }

    /// Bulk-set `egress_label_json` on execution_traces. Returns rows updated.
    pub fn execution_trace_relabel(
        &self,
        new_label: &autonoetic_types::egress::EgressLabel,
        session_id: Option<&str>,
        only_unlabeled: bool,
    ) -> Result<u64> {
        let label_json = serde_json::to_string(new_label)?;
        let conn = self.conn.lock().unwrap();
        let n = match (session_id, only_unlabeled) {
            (Some(sid), true) => conn.execute(
                "UPDATE execution_traces SET egress_label_json = ?1
                 WHERE (session_id = ?2 OR session_id LIKE ?3 ESCAPE '\\')
                   AND (egress_label_json IS NULL OR egress_label_json = '')",
                params![
                    label_json,
                    sid,
                    format!("{}/%", super::escape_sqlite_like_fragment(sid))
                ],
            )?,
            (Some(sid), false) => conn.execute(
                "UPDATE execution_traces SET egress_label_json = ?1
                 WHERE session_id = ?2 OR session_id LIKE ?3 ESCAPE '\\'",
                params![
                    label_json,
                    sid,
                    format!("{}/%", super::escape_sqlite_like_fragment(sid))
                ],
            )?,
            (None, true) => conn.execute(
                "UPDATE execution_traces SET egress_label_json = ?1
                 WHERE egress_label_json IS NULL OR egress_label_json = ''",
                params![label_json],
            )?,
            (None, false) => conn.execute(
                "UPDATE execution_traces SET egress_label_json = ?1",
                params![label_json],
            )?,
        };
        Ok(n as u64)
    }

    pub fn upsert_session_transcript(
        &self,
        record: &autonoetic_types::causal_chain::SessionTranscriptRecord,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_transcripts (
                transcript_id, session_id, root_session_id, agent_id,
                revision_id, user_id, started_at, ended_at, status,
                turn_count, transcript_handle, excerpt, origin_node_id,
                lifecycle_state
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                CASE ?9
                    WHEN 'active' THEN 'active'
                    WHEN 'completed' THEN 'terminated:completed'
                    WHEN 'closed' THEN 'terminated:completed'
                    WHEN 'failed' THEN 'terminated:failed'
                    WHEN 'suspended' THEN 'awaiting_gate'
                    ELSE 'active'
                END
            )
            ON CONFLICT(session_id) DO UPDATE SET
                transcript_id = excluded.transcript_id,
                ended_at = CASE
                    WHEN excluded.status = 'active'
                     AND session_transcripts.status IN ('completed', 'failed', 'suspended', 'closed')
                    THEN session_transcripts.ended_at
                    ELSE excluded.ended_at
                END,
                status = CASE
                    WHEN excluded.status = 'active'
                     AND session_transcripts.status IN ('completed', 'failed', 'suspended', 'closed')
                    THEN session_transcripts.status
                    ELSE excluded.status
                END,
                turn_count = excluded.turn_count,
                transcript_handle = excluded.transcript_handle,
                excerpt = excluded.excerpt,
                lifecycle_state = CASE
                    WHEN excluded.status = 'active'
                     AND session_transcripts.status IN ('completed', 'failed', 'suspended', 'closed')
                    THEN session_transcripts.lifecycle_state
                    ELSE excluded.lifecycle_state
                END",
            params![
                record.transcript_id,
                record.session_id,
                record.root_session_id,
                record.agent_id,
                record.revision_id,
                record.user_id,
                record.started_at,
                record.ended_at,
                record.status,
                record.turn_count,
                record.transcript_handle,
                record.excerpt,
                record.origin_node_id,
            ],
        )?;
        Ok(())
    }

    /// Resolve the agent identity that owns a session. Used to authenticate a
    /// caller-supplied `decider_session_id` against the recorded owner before
    /// applying R-10.7 trust-boundary checks. Consults `session_transcripts`
    /// first (covers root and child sessions), then falls back to
    /// `session_spawn_lineage.target_agent_id` for child sessions whose
    /// transcript has not yet been written. Returns `None` when the session is
    /// not recorded anywhere (treated as untrusted by callers).
    pub fn session_owner_agent(&self, session_id: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().unwrap();
        let agent: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM session_transcripts WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        if agent.is_some() {
            return Ok(agent);
        }
        let lineage_agent: Option<String> = conn
            .query_row(
                "SELECT target_agent_id FROM session_spawn_lineage WHERE child_session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(lineage_agent)
    }

    pub fn finalize_session_transcript(
        &self,
        session_id: &str,
        ended_at: &str,
        status: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE session_transcripts SET ended_at = ?1, status = ?2 WHERE session_id = ?3",
            params![ended_at, status, session_id],
        )?;
        // #742: set lifecycle state to match the terminal status.
        // Only "completed" and "failed" are truly terminal; "suspended" is
        // resumable (its lifecycle was already set by save_yield_checkpoint).
        // For "completed", avoid overwriting "hibernated" (between-turn yield)
        // — only set "terminated:completed" when the current state is not
        // already a resumable lifecycle state.
        let lifecycle = match status {
            "completed" => Some("terminated:completed"),
            "failed" => Some("terminated:failed"),
            _ => None,
        };
        if let Some(lifecycle) = lifecycle {
            // Only overwrite if the current lifecycle is not a resumable state.
            // This preserves "hibernated" between turns and "awaiting_gate"
            // for gate-suspended sessions. Truly terminal sessions (headless
            // complete, error, etc.) will have "active" or NULL lifecycle.
            conn.execute(
                "UPDATE session_transcripts
                 SET lifecycle_state = ?1
                 WHERE session_id = ?2
                   AND (lifecycle_state IS NULL
                        OR lifecycle_state NOT IN ('hibernated', 'awaiting_gate'))",
                params![lifecycle, session_id],
            )?;
        }
        Ok(())
    }

    /// Re-open a previously finalized session for a new turn.
    ///
    /// Between turns the session's transcript status is `completed` (from
    /// `close_session`) and lifecycle_state is `hibernated` (from the yield
    /// checkpoint). Call this at the start of each turn to restore `active`
    /// in both fields so child agents spawned during execution are not
    /// immediately orphaned (siblings of a `hibernated` parent are protected
    /// by the lifecycle-state-based orphan reaper, but we still need a clean
    /// slate for the new turn).
    pub fn reopen_session_transcript(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE session_transcripts SET status = 'active', ended_at = NULL, lifecycle_state = 'active' WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// #742: set the session lifecycle state explicitly.
    pub fn set_session_lifecycle_state(
        &self,
        session_id: &str,
        state: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE session_transcripts SET lifecycle_state = ?1 WHERE session_id = ?2",
            params![state, session_id],
        )?;
        Ok(())
    }

    /// #742: read the session lifecycle state.
    pub fn get_session_lifecycle_state(
        &self,
        session_id: &str,
    ) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().unwrap();
        let state: Option<String> = conn
            .query_row(
                "SELECT lifecycle_state FROM session_transcripts WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(state)
    }

    pub fn find_transcript_by_handle(
        &self,
        handle: &str,
    ) -> Result<Option<autonoetic_types::causal_chain::SessionTranscriptRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT transcript_id, session_id, root_session_id, agent_id,
                    revision_id, user_id, started_at, ended_at, status,
                    turn_count, transcript_handle, excerpt, origin_node_id
             FROM session_transcripts
             WHERE transcript_handle = ?1
             LIMIT 1",
        )?;
        let result = stmt.query_row(params![handle], |row| {
            Ok(autonoetic_types::causal_chain::SessionTranscriptRecord {
                transcript_id: row.get(0)?,
                session_id: row.get(1)?,
                root_session_id: row.get(2)?,
                agent_id: row.get(3)?,
                revision_id: row.get(4)?,
                user_id: row.get(5)?,
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                status: row.get(8)?,
                turn_count: row.get(9)?,
                transcript_handle: row.get(10)?,
                excerpt: row.get(11)?,
                origin_node_id: row.get(12)?,
            })
        });

        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn find_transcript_by_session_id(
        &self,
        session_id: &str,
    ) -> Result<Option<autonoetic_types::causal_chain::SessionTranscriptRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT transcript_id, session_id, root_session_id, agent_id,
                    revision_id, user_id, started_at, ended_at, status,
                    turn_count, transcript_handle, excerpt, origin_node_id
             FROM session_transcripts
             WHERE session_id = ?1
             LIMIT 1",
        )?;
        let result = stmt.query_row(params![session_id], |row| {
            Ok(autonoetic_types::causal_chain::SessionTranscriptRecord {
                transcript_id: row.get(0)?,
                session_id: row.get(1)?,
                root_session_id: row.get(2)?,
                agent_id: row.get(3)?,
                revision_id: row.get(4)?,
                user_id: row.get(5)?,
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                status: row.get(8)?,
                turn_count: row.get(9)?,
                transcript_handle: row.get(10)?,
                excerpt: row.get(11)?,
                origin_node_id: row.get(12)?,
            })
        });
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn search_session_transcripts(
        &self,
        query: Option<&str>,
        agent_id: Option<&str>,
        root_session_id: Option<&str>,
        status: Option<&str>,
        since: Option<&str>,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::causal_chain::SessionTranscriptRecord>> {
        let conn = self.conn.lock().unwrap();

        let mut conditions: Vec<String> = Vec::new();
        let mut sql_params: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(aid) = agent_id {
            conditions.push("st.agent_id = ?".to_string());
            sql_params.push(rusqlite::types::Value::Text(aid.to_string()));
        }

        if let Some(rid) = root_session_id {
            conditions.push("st.root_session_id = ?".to_string());
            sql_params.push(rusqlite::types::Value::Text(rid.to_string()));
        }

        if let Some(s) = status {
            conditions.push("st.status = ?".to_string());
            sql_params.push(rusqlite::types::Value::Text(s.to_string()));
        }

        if let Some(since_time) = since {
            conditions.push("st.started_at >= ?".to_string());
            sql_params.push(rusqlite::types::Value::Text(since_time.to_string()));
        }

        let where_clause = if conditions.is_empty() {
            "1".to_string()
        } else {
            conditions.join(" AND ")
        };

        if let Some(q) = query {
            let fts_sql = format!(
                "SELECT st.transcript_id, st.session_id, st.root_session_id, st.agent_id,
                        st.revision_id, st.user_id, st.started_at, st.ended_at, st.status,
                        st.turn_count, st.transcript_handle, st.excerpt, st.origin_node_id
                 FROM session_transcripts st
                 JOIN session_transcripts_fts ON st.rowid = session_transcripts_fts.rowid
                 WHERE session_transcripts_fts MATCH ?1
                   AND {where_clause}
                 ORDER BY rank ASC
                 LIMIT ?",
                where_clause = where_clause,
            );

            let mut fts_params = vec![rusqlite::types::Value::Text(q.to_string())];
            fts_params.extend(sql_params.clone());
            fts_params.push(rusqlite::types::Value::Integer(limit));

            let mut stmt = conn.prepare(&fts_sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(fts_params), |row| {
                Ok(autonoetic_types::causal_chain::SessionTranscriptRecord {
                    transcript_id: row.get(0)?,
                    session_id: row.get(1)?,
                    root_session_id: row.get(2)?,
                    agent_id: row.get(3)?,
                    revision_id: row.get(4)?,
                    user_id: row.get(5)?,
                    started_at: row.get(6)?,
                    ended_at: row.get(7)?,
                    status: row.get(8)?,
                    turn_count: row.get(9)?,
                    transcript_handle: row.get(10)?,
                    excerpt: row.get(11)?,
                    origin_node_id: row.get(12)?,
                })
            });

            match rows {
                Ok(r) => {
                    let mut results = Vec::new();
                    for res in r {
                        results.push(res?);
                    }
                    return Ok(results);
                }
                Err(ref e) if should_fallback_to_like(e, q) => {
                    // FTS parse error with FTS-like syntax — fall back to LIKE search
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Non-FTS path (no query, or FTS fallback)
        let (sql, like_params) = if let Some(q) = query {
            let mut fb_conditions = conditions.clone();
            fb_conditions.push("st.excerpt LIKE ?".to_string());
            let mut fb_params = sql_params.clone();
            fb_params.push(rusqlite::types::Value::Text(format!("%{}%", q)));
            let fb_where = fb_conditions.join(" AND ");
            (
                format!(
                    "SELECT st.transcript_id, st.session_id, st.root_session_id, st.agent_id,
                        st.revision_id, st.user_id, st.started_at, st.ended_at, st.status,
                        st.turn_count, st.transcript_handle, st.excerpt, st.origin_node_id
                 FROM session_transcripts st
                 WHERE {fb_where}
                 ORDER BY st.started_at DESC
                 LIMIT ?",
                    fb_where = fb_where,
                ),
                fb_params,
            )
        } else {
            (
                format!(
                    "SELECT st.transcript_id, st.session_id, st.root_session_id, st.agent_id,
                        st.revision_id, st.user_id, st.started_at, st.ended_at, st.status,
                        st.turn_count, st.transcript_handle, st.excerpt, st.origin_node_id
                 FROM session_transcripts st
                 WHERE {where_clause}
                 ORDER BY st.started_at DESC
                 LIMIT ?",
                    where_clause = where_clause,
                ),
                sql_params,
            )
        };

        let mut final_params = like_params;
        final_params.push(rusqlite::types::Value::Integer(limit));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(final_params), |row| {
            Ok(autonoetic_types::causal_chain::SessionTranscriptRecord {
                transcript_id: row.get(0)?,
                session_id: row.get(1)?,
                root_session_id: row.get(2)?,
                agent_id: row.get(3)?,
                revision_id: row.get(4)?,
                user_id: row.get(5)?,
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                status: row.get(8)?,
                turn_count: row.get(9)?,
                transcript_handle: row.get(10)?,
                excerpt: row.get(11)?,
                origin_node_id: row.get(12)?,
            })
        })?;

        let mut results = Vec::new();
        for res in rows {
            results.push(res?);
        }
        Ok(results)
    }

    pub fn upsert_published_session_report(
        &self,
        record: &autonoetic_types::causal_chain::PublishedSessionReportRecord,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO published_session_reports (
                root_session_id, report_handle, overview_handle, html_handle,
                narrative_handle, title, status, started_at, ended_at,
                agent_count, error_count, approval_count, search_text,
                generated_at, report_version
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            ON CONFLICT(root_session_id) DO UPDATE SET
                report_handle = excluded.report_handle,
                overview_handle = excluded.overview_handle,
                html_handle = excluded.html_handle,
                narrative_handle = excluded.narrative_handle,
                title = excluded.title,
                status = excluded.status,
                ended_at = COALESCE(excluded.ended_at, published_session_reports.ended_at),
                agent_count = excluded.agent_count,
                error_count = excluded.error_count,
                approval_count = excluded.approval_count,
                search_text = excluded.search_text,
                generated_at = excluded.generated_at,
                report_version = excluded.report_version",
            params![
                &record.root_session_id,
                &record.report_handle,
                record.overview_handle.as_deref(),
                record.html_handle.as_deref(),
                record.narrative_handle.as_deref(),
                &record.title,
                &record.status,
                record.started_at.as_deref(),
                record.ended_at.as_deref(),
                record.agent_count,
                record.error_count,
                record.approval_count,
                &record.search_text,
                &record.generated_at,
                record.report_version,
            ],
        )?;

        conn.execute(
            "DELETE FROM published_session_reports_fts WHERE root_session_id = ?1",
            params![&record.root_session_id],
        )?;
        conn.execute(
            "INSERT INTO published_session_reports_fts (root_session_id, title, search_text, status) VALUES (?1, ?2, ?3, ?4)",
            params![
                &record.root_session_id,
                &record.title,
                &record.search_text,
                &record.status,
            ],
        )?;

        Ok(())
    }

    pub fn find_published_report(
        &self,
        root_session_id: &str,
    ) -> Result<Option<autonoetic_types::causal_chain::PublishedSessionReportRecord>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT root_session_id, report_handle, overview_handle, html_handle,
                    narrative_handle, title, status, started_at, ended_at,
                    agent_count, error_count, approval_count, search_text,
                    generated_at, report_version
             FROM published_session_reports
             WHERE root_session_id = ?1",
            params![root_session_id],
            |row| {
                Ok(
                    autonoetic_types::causal_chain::PublishedSessionReportRecord {
                        root_session_id: row.get(0)?,
                        report_handle: row.get(1)?,
                        overview_handle: row.get(2)?,
                        html_handle: row.get(3)?,
                        narrative_handle: row.get(4)?,
                        title: row.get(5)?,
                        status: row.get(6)?,
                        started_at: row.get(7)?,
                        ended_at: row.get(8)?,
                        agent_count: row.get(9)?,
                        error_count: row.get(10)?,
                        approval_count: row.get(11)?,
                        search_text: row.get(12)?,
                        generated_at: row.get(13)?,
                        report_version: row.get(14)?,
                    },
                )
            },
        );
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn search_published_reports(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<autonoetic_types::causal_chain::PublishedSessionReportRecord>> {
        let conn = self.conn.lock().unwrap();

        let fts_sql = "\
            SELECT r.root_session_id, r.report_handle, r.overview_handle, r.html_handle,
                    r.narrative_handle, r.title, r.status, r.started_at, r.ended_at,
                    r.agent_count, r.error_count, r.approval_count, r.search_text,
                    r.generated_at, r.report_version
             FROM published_session_reports r
             WHERE r.root_session_id IN (
                 SELECT f.root_session_id FROM published_session_reports_fts f WHERE f MATCH ?1
             )
             ORDER BY r.generated_at DESC
             LIMIT ?2";

        match Self::read_published_rows(&conn, fts_sql, params![query, limit]) {
            Ok(rows) => return Ok(rows),
            Err(e) => {
                tracing::debug!(target: "observability", error = %e, "FTS search failed, falling back to LIKE");
                // Always fall back to LIKE on FTS error
            }
        }

        let like_sql = "\
            SELECT root_session_id, report_handle, overview_handle, html_handle,
                    narrative_handle, title, status, started_at, ended_at,
                    agent_count, error_count, approval_count, search_text,
                    generated_at, report_version
             FROM published_session_reports
             WHERE search_text LIKE ?1
             ORDER BY generated_at DESC
             LIMIT ?2";
        Self::read_published_rows(&conn, like_sql, params![format!("%{}%", query), limit])
    }

    fn read_published_rows(
        conn: &rusqlite::Connection,
        sql: &str,
        p: impl rusqlite::Params,
    ) -> Result<Vec<autonoetic_types::causal_chain::PublishedSessionReportRecord>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(p, |row| {
            Ok(
                autonoetic_types::causal_chain::PublishedSessionReportRecord {
                    root_session_id: row.get(0)?,
                    report_handle: row.get(1)?,
                    overview_handle: row.get(2)?,
                    html_handle: row.get(3)?,
                    narrative_handle: row.get(4)?,
                    title: row.get(5)?,
                    status: row.get(6)?,
                    started_at: row.get(7)?,
                    ended_at: row.get(8)?,
                    agent_count: row.get(9)?,
                    error_count: row.get(10)?,
                    approval_count: row.get(11)?,
                    search_text: row.get(12)?,
                    generated_at: row.get(13)?,
                    report_version: row.get(14)?,
                },
            )
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    /// Find active or suspended sessions whose parent session has ended (orphan detection for R+12).
    ///
    /// Returns (child_session_id, parent_session_id, root_session_id, agent_id) tuples
    /// for each orphaned child. A child is orphaned if its parent's `lifecycle_state`
    /// is `terminated:*`. Children of `active`, `hibernated`, or `awaiting_gate`
    /// parents are protected — the parent is expected to resume and coordinate them.
    /// Children that are themselves already terminated are excluded.
    pub fn find_orphaned_sessions(&self) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        // Select non-terminated child sessions (session_id contains '/').
        // Pre-migration rows (lifecycle_state IS NULL) use status as fallback.
        let mut stmt = conn.prepare(
            "SELECT session_id, root_session_id, agent_id
             FROM session_transcripts
             WHERE session_id LIKE '%/%'
               AND (lifecycle_state IS NULL AND status IN ('active', 'suspended')
                    OR lifecycle_state IS NOT NULL AND lifecycle_state NOT LIKE 'terminated:%')",
        )?;
        let children: Vec<(String, String, String, String)> = stmt
            .query_map([], |row| {
                let sid: String = row.get(0)?;
                let root: String = row.get(1)?;
                let agent: String = row.get(2)?;
                let parent = sid
                    .rsplit_once('/')
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_default();
                Ok((sid, parent, root, agent))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut orphans = Vec::new();
        for (child_session_id, parent_session_id, root_session_id, agent_id) in children {
            let parent_lifecycle: Option<String> = conn
                .query_row(
                    "SELECT COALESCE(lifecycle_state,
                        CASE status
                            WHEN 'completed' THEN 'terminated:completed'
                            WHEN 'closed' THEN 'terminated:completed'
                            WHEN 'failed' THEN 'terminated:failed'
                            ELSE 'active'
                        END
                     ) FROM session_transcripts WHERE session_id = ?1",
                    params![parent_session_id],
                    |row| row.get(0),
                )
                .ok();
            // #742: parent is terminated → child is orphaned, full stop.
            // Lifecycle states active, hibernated, awaiting_gate protect children.
            // Falls back to status-based inference for pre-migration data.
            match parent_lifecycle.as_deref() {
                Some(s) if s.starts_with("terminated:") => {
                    orphans.push((
                        child_session_id,
                        parent_session_id,
                        root_session_id,
                        agent_id,
                    ));
                }
                _ => {}
            }
        }
        Ok(orphans)
    }

    pub fn record_sandbox_escape_attempt(
        &self,
        session_id: &str,
        root_session_id: &str,
        agent_id: &str,
        indicator: &str,
        detail: &str,
        exit_code: Option<i32>,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("gateway_store conn mutex");
        conn.execute(
            "INSERT INTO sandbox_escape_attempts
                (session_id, root_session_id, agent_id, indicator, detail, exit_code, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                root_session_id,
                agent_id,
                indicator,
                detail,
                exit_code,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        // Release the conn lock before emitting — create_live_digest_event re-locks.
        drop(conn);

        // Surface the escape attempt on the canonical timeline (#413). It was
        // store-only (sandbox_escape_attempts table), so a security-critical,
        // session-scoped event was invisible in the room. Attributed to the agent
        // whose sandbox tripped; always Error. Fall back to session_id as the
        // timeline root if the caller passed an empty root (upstream fallback
        // bug) — never silently drop a recorded security event from the room.
        let timeline_root = if root_session_id.is_empty() {
            session_id
        } else {
            root_session_id
        };
        if !timeline_root.is_empty() {
            let principal = autonoetic_types::principal::Principal::agent(agent_id);
            let seat = crate::runtime::session_timeline::derive_role(agent_id);
            let event = crate::runtime::session_timeline::build_timeline_event(
                timeline_root.to_string(),
                session_id.to_string(),
                None,
                &principal,
                &seat,
                "security.sandbox_escape",
                None, // base_altitude ⇒ Error
                Some(serde_json::json!({
                    "indicator": indicator,
                    "detail": crate::log_redaction::redact_text_for_logs(detail),
                    "exit_code": exit_code,
                    "agent_id": agent_id,
                })),
                autonoetic_types::session_timeline::TimelineRefs::default(),
            );
            if let Err(e) = self.create_live_digest_event(&event) {
                tracing::debug!(
                    target: "session_timeline",
                    error = %e,
                    "sandbox_escape timeline emit failed"
                );
            }
        }
        Ok(())
    }

    pub fn count_sandbox_escape_attempts_for_session(&self, session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("gateway_store conn mutex");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sandbox_escape_attempts WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn count_sandbox_escape_attempts_for_root(&self, root_session_id: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("gateway_store conn mutex");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sandbox_escape_attempts WHERE root_session_id = ?1",
            params![root_session_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn emit_escape_threshold_event(
        &self,
        session_id: &str,
        root_session_id: &str,
        count: usize,
        threshold: usize,
        level: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now();
        let mut rules = autonoetic_types::causal_chain::default_enforced_rules();
        rules.push("R++8".to_string());
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("escape-threshold-{}", uuid::Uuid::new_v4()),
            agent_id: "gateway".to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: now.timestamp_millis().max(0) as u64,
            timestamp: now.to_rfc3339(),
            category: "security".to_string(),
            action: format!("sandbox.escape_threshold_{}", level),
            status: autonoetic_types::causal_chain::EntryStatus::Error.to_string(),
            enforced_rules: rules,
            target: Some(root_session_id.to_string()),
            payload: Some(
                serde_json::json!({
                    "session_id": session_id,
                    "root_session_id": root_session_id,
                    "count": count,
                    "threshold": threshold,
                    "level": level,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some(format!(
                "session {} has {} escape attempts (threshold: {})",
                session_id, count, threshold
            )),
        };
        self.create_causal_event(&event)?;

        let timeline_root = if root_session_id.is_empty() {
            session_id
        } else {
            root_session_id
        };
        if !timeline_root.is_empty() {
            let (principal, seat) = crate::runtime::session_timeline::actor_from_kind_id(
                "system",
                "gateway",
            );
            let timeline_event = crate::runtime::session_timeline::build_timeline_event(
                timeline_root.to_string(),
                session_id.to_string(),
                None,
                &principal,
                &seat,
                "security.escape_threshold",
                None,
                Some(serde_json::json!({
                    "level": level,
                    "count": count,
                    "threshold": threshold,
                    "session_id": session_id,
                    "root_session_id": root_session_id,
                })),
                autonoetic_types::session_timeline::TimelineRefs::default(),
            );
            if let Err(e) = self.create_live_digest_event(&timeline_event) {
                tracing::debug!(
                    target: "session_timeline",
                    error = %e,
                    "escape_threshold timeline emit failed"
                );
            }
        }

        Ok(())
    }

    /// Surface a per-host `sandbox_exec` probe-budget trip (issue #853) to the
    /// operator: a causal event plus an Attention-altitude timeline entry, so a
    /// researcher stuck re-probing one host reads as a triage item at the root,
    /// not only as the agent's own tool error. Mirrors `emit_escape_threshold_event`.
    /// Emitted once, at the moment the host reaches the strike cap.
    pub fn emit_host_probe_budget_exhausted_event(
        &self,
        session_id: &str,
        root_session_id: &str,
        agent_id: &str,
        host: &str,
        strikes: u32,
        cap: usize,
    ) -> Result<()> {
        let now = chrono::Utc::now();
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("host-budget-{}", uuid::Uuid::new_v4()),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: now.timestamp_millis().max(0) as u64,
            timestamp: now.to_rfc3339(),
            category: "sandbox".to_string(),
            action: "sandbox.host_budget_exhausted".to_string(),
            status: "active".to_string(),
            // Observational: a mechanical resource cap, not a numbered
            // constitutional clause — carries the baseline attribution
            // placeholder so it does not inflate any clause's contract-health.
            enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
            target: Some(host.to_string()),
            payload: Some(
                serde_json::json!({
                    "session_id": session_id,
                    "root_session_id": root_session_id,
                    "host": host,
                    "strikes": strikes,
                    "max_probes_per_host": cap,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some(format!(
                "host {} probed {} times this session without new information (cap {})",
                host, strikes, cap
            )),
        };
        self.create_causal_event(&event)?;

        let timeline_root = if root_session_id.is_empty() {
            session_id
        } else {
            root_session_id
        };
        if !timeline_root.is_empty() {
            let (principal, seat) =
                crate::runtime::session_timeline::actor_from_kind_id("system", "gateway");
            let timeline_event = crate::runtime::session_timeline::build_timeline_event(
                timeline_root.to_string(),
                session_id.to_string(),
                None,
                &principal,
                &seat,
                "sandbox.host_budget_exhausted",
                None,
                Some(serde_json::json!({
                    "host": host,
                    "strikes": strikes,
                    "max_probes_per_host": cap,
                    "session_id": session_id,
                })),
                autonoetic_types::session_timeline::TimelineRefs::default(),
            );
            if let Err(e) = self.create_live_digest_event(&timeline_event) {
                tracing::debug!(
                    target: "session_timeline",
                    error = %e,
                    "host_budget_exhausted timeline emit failed"
                );
            }
        }

        Ok(())
    }

    pub fn sessions_exceeding_escape_threshold(
        &self,
        threshold: usize,
    ) -> Result<Vec<(String, String, usize)>> {
        if threshold == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().expect("gateway_store conn mutex");
        let mut stmt = conn.prepare(
            "SELECT session_id, root_session_id, COUNT(*) as cnt \
             FROM sandbox_escape_attempts \
             GROUP BY session_id \
             HAVING cnt >= ?1",
        )?;
        let rows = stmt.query_map(params![threshold as i64], |row| {
            let session_id: String = row.get(0)?;
            let root_session_id: String = row.get(1)?;
            let count: i64 = row.get(2)?;
            Ok((session_id, root_session_id, count as usize))
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn list_recent_sessions(
        &self,
        limit: i64,
        agent_id: Option<&str>,
    ) -> Result<Vec<(String, String, String)>> {
        // Query live_digest_events (the canonical timeline) grouped by
        // root_session_id so only root sessions appear — child sessions are
        // excluded because they share the same root_session_id.
        // When `agent_id` is provided, push the filter into SQL so the LIMIT
        // applies *after* filtering.
        let conn = self.conn.lock().unwrap();
        let (sql, params_vec): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match agent_id {
            Some(_) => (
                "SELECT e.root_session_id,
                        COALESCE(
                            (SELECT e_start.source_agent_id FROM live_digest_events e_start WHERE e_start.root_session_id = e.root_session_id AND e_start.event_type = 'session.start' LIMIT 1),
                            (SELECT e_min.source_agent_id FROM live_digest_events e_min WHERE e_min.root_session_id = e.root_session_id ORDER BY e_min.created_at ASC LIMIT 1),
                            e.source_agent_id
                        ) as root_agent_id,
                        MAX(e.created_at) as last_ts
                 FROM live_digest_events e
                 WHERE e.source_agent_id = ?2
                    OR e.root_session_id IN (
                        SELECT e_sub.root_session_id FROM live_digest_events e_sub WHERE e_sub.source_agent_id = ?2
                    )
                 GROUP BY e.root_session_id
                 ORDER BY last_ts DESC, e.root_session_id DESC
                 LIMIT ?1",
                vec![Box::new(limit), Box::new(agent_id.unwrap().to_string())],
            ),
            None => (
                "SELECT e.root_session_id,
                        COALESCE(
                            (SELECT e_start.source_agent_id FROM live_digest_events e_start WHERE e_start.root_session_id = e.root_session_id AND e_start.event_type = 'session.start' LIMIT 1),
                            (SELECT e_min.source_agent_id FROM live_digest_events e_min WHERE e_min.root_session_id = e.root_session_id ORDER BY e_min.created_at ASC LIMIT 1),
                            e.source_agent_id
                        ) as root_agent_id,
                        MAX(e.created_at) as last_ts
                 FROM live_digest_events e
                 GROUP BY e.root_session_id
                 ORDER BY last_ts DESC, e.root_session_id DESC
                 LIMIT ?1",
                vec![Box::new(limit)],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }
}
mod fts_fallback_tests {
    use super::*;

    #[test]
    fn test_looks_like_fts_syntax_dot() {
        assert!(looks_like_fts_syntax("runtime.lock"));
    }

    #[test]
    fn test_looks_like_fts_syntax_parens() {
        assert!(looks_like_fts_syntax("config)"));
        assert!(looks_like_fts_syntax("(config"));
    }

    #[test]
    fn test_looks_like_fts_syntax_wildcard() {
        assert!(looks_like_fts_syntax("config*"));
    }

    #[test]
    fn test_looks_like_fts_syntax_operators() {
        assert!(looks_like_fts_syntax("a-b"));
        assert!(looks_like_fts_syntax("a+b"));
        assert!(looks_like_fts_syntax("a&b"));
        assert!(looks_like_fts_syntax("a\"b"));
    }

    #[test]
    fn test_looks_like_fts_syntax_plain() {
        assert!(!looks_like_fts_syntax("config"));
        assert!(!looks_like_fts_syntax("runtime lock"));
    }

    #[test]
    fn test_looks_like_fts_syntax_column_qualifier() {
        assert!(looks_like_fts_syntax("report:error"));
        assert!(looks_like_fts_syntax("title: foo"));
        assert!(looks_like_fts_syntax("status:failed"));
    }

    #[test]
    fn test_should_fallback_true_on_sqlite_error() {
        let err = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR as i32),
            None,
        );
        assert!(should_fallback_to_like(&err, "runtime.lock"));
    }

    #[test]
    fn test_should_fallback_false_on_non_fts_syntax() {
        let err = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR as i32),
            None,
        );
        assert!(!should_fallback_to_like(&err, "plain query"));
    }

    #[test]
    fn test_should_fallback_false_on_other_error() {
        let err = rusqlite::Error::InvalidParameterName("test".into());
        assert!(!should_fallback_to_like(&err, "runtime.lock"));
    }
}

#[cfg(test)]
mod sandbox_escape_timeline_tests {
    use super::*;
    use autonoetic_types::session_timeline::{Altitude, SessionRole};
    use tempfile::tempdir;

    #[test]
    fn record_sandbox_escape_surfaces_on_timeline() {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        store
            .record_sandbox_escape_attempt(
                "root-1",
                "root-1",
                "coder.default",
                "ptrace syscall",
                "blocked by seccomp",
                Some(159),
            )
            .unwrap();

        let result = store
            .list_session_timeline("root-1", None, 50, Some(Altitude::Detail), None)
            .unwrap();
        let ev = result
            .entries
            .iter()
            .find(|e| e.event_type == "security.sandbox_escape")
            .expect("sandbox escape must reach the timeline");
        // Security-critical ⇒ Error; attributed to the agent whose sandbox tripped.
        assert_eq!(ev.altitude, Altitude::Error);
        assert!(matches!(ev.role, SessionRole::Specialist { .. }));
        assert_eq!(ev.principal.id, "coder.default");
        assert!(ev.payload.as_deref().unwrap().contains("ptrace syscall"));
    }
}

/// Civic-health view (#772 E.2): the dual of contract-health. Tallies each
/// agent's constitutional proposals and anomaly flags, filed vs pending.
#[cfg(test)]
mod civic_health_tests {
    use super::*;
    use crate::scheduler::gateway_store::amendment_invitations::AmendmentInvitation;
    use crate::scheduler::gateway_store::anomaly_flags::AnomalyFlag;
    use crate::scheduler::gateway_store::constitutional_proposals::ConstitutionalProposal;
    use tempfile::tempdir;

    fn proposal(id: &str, proposer: &str, status: &str, created_at: &str) -> ConstitutionalProposal {
        ConstitutionalProposal {
            proposal_id: id.to_string(),
            proposer_agent_id: proposer.to_string(),
            proposer_session_id: None,
            kind: "add_right".to_string(),
            target_id: None,
            proposed_text: Some("agents may do X".to_string()),
            justification: "closes a gap".to_string(),
            evidence_json: serde_json::json!({}),
            status: status.to_string(),
            operator_decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            published_in_release: None,
            created_at: created_at.to_string(),
            sla_breached_at: None,
        }
    }

    fn flag(id: &str, reporter: &str, status: &str, created_at: &str) -> AnomalyFlag {
        AnomalyFlag {
            flag_id: id.to_string(),
            reporter_agent_id: reporter.to_string(),
            reporter_session_id: None,
            subject_ref: "sess-target".to_string(),
            observation: "tool call bypassed policy check".to_string(),
            evidence_json: serde_json::json!([]),
            severity: "high".to_string(),
            status: status.to_string(),
            decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            created_at: created_at.to_string(),
            sla_breached_at: None,
        }
    }

    fn invitation(
        id: &str,
        agent: &str,
        rule: &str,
        status: &str,
        created_at: &str,
    ) -> AmendmentInvitation {
        AmendmentInvitation {
            invitation_id: id.to_string(),
            agent_id: agent.to_string(),
            rule_id: rule.to_string(),
            denial_count: 3,
            threshold: 3,
            window_secs: 604800,
            status: status.to_string(),
            answered_proposal_id: None,
            created_at: created_at.to_string(),
            resolved_at: None,
        }
    }

    /// Two agents with mixed proposal/flag statuses: filed totals count
    /// every item; pending excludes terminal statuses but *includes*
    /// `under_review` (a non-terminal review-start transition). A third,
    /// more active agent proves the by-total-desc sort, and a tie between
    /// the first two proves the agent-id-asc tiebreak.
    #[test]
    fn civic_health_tallies_filed_and_pending_by_agent() {
        let temp = tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();

        // alpha.agent: 1 proposal (under_review, non-terminal -> pending) +
        // 2 flags (1 confirmed terminal, 1 pending non-terminal). total = 3.
        store
            .insert_constitutional_proposal(&proposal(
                "prop-alpha-1",
                "alpha.agent",
                "under_review",
                "2026-05-01T00:00:00Z",
            ))
            .unwrap();
        store
            .insert_anomaly_flag(&flag(
                "flag-alpha-1",
                "alpha.agent",
                "confirmed",
                "2026-05-01T00:00:01Z",
            ))
            .unwrap();
        store
            .insert_anomaly_flag(&flag(
                "flag-alpha-2",
                "alpha.agent",
                "pending",
                "2026-05-01T00:00:02Z",
            ))
            .unwrap();

        // beta.agent: 2 proposals (1 pending non-terminal, 1 approved
        // terminal) + 1 flag (dismissed terminal). total = 3 (tie w/ alpha).
        store
            .insert_constitutional_proposal(&proposal(
                "prop-beta-1",
                "beta.agent",
                "pending",
                "2026-05-01T00:00:03Z",
            ))
            .unwrap();
        store
            .insert_constitutional_proposal(&proposal(
                "prop-beta-2",
                "beta.agent",
                "approved",
                "2026-05-01T00:00:04Z",
            ))
            .unwrap();
        store
            .insert_anomaly_flag(&flag(
                "flag-beta-1",
                "beta.agent",
                "dismissed",
                "2026-05-01T00:00:05Z",
            ))
            .unwrap();

        // gamma.agent: 5 pending proposals, no flags. total = 5 (highest).
        for i in 0..5 {
            store
                .insert_constitutional_proposal(&proposal(
                    &format!("prop-gamma-{i}"),
                    "gamma.agent",
                    "pending",
                    "2026-05-01T00:00:06Z",
                ))
                .unwrap();
        }

        let health = store.civic_health(None).unwrap();
        assert_eq!(
            health
                .by_agent
                .iter()
                .map(|e| e.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["gamma.agent", "alpha.agent", "beta.agent"],
            "sorted by total-filed desc, ties broken by agent_id asc"
        );

        let alpha = health
            .by_agent
            .iter()
            .find(|e| e.agent_id == "alpha.agent")
            .unwrap();
        assert_eq!(alpha.proposals_filed, 1);
        assert_eq!(alpha.proposals_pending, 1); // under_review counts as pending
        assert_eq!(alpha.flags_filed, 2);
        assert_eq!(alpha.flags_pending, 1); // confirmed is terminal, pending is not
        assert_eq!(alpha.invitations_issued, 0);
        assert_eq!(alpha.invitations_open, 0);
        assert_eq!(alpha.invitations_answered, 0);

        let beta = health
            .by_agent
            .iter()
            .find(|e| e.agent_id == "beta.agent")
            .unwrap();
        assert_eq!(beta.proposals_filed, 2);
        assert_eq!(beta.proposals_pending, 1);
        assert_eq!(beta.flags_filed, 1);
        assert_eq!(beta.flags_pending, 0); // dismissed is terminal
        assert_eq!(beta.invitations_issued, 0);
        assert_eq!(beta.invitations_open, 0);
        assert_eq!(beta.invitations_answered, 0);

        let gamma = health
            .by_agent
            .iter()
            .find(|e| e.agent_id == "gamma.agent")
            .unwrap();
        assert_eq!(gamma.proposals_filed, 5);
        assert_eq!(gamma.proposals_pending, 5);
        assert_eq!(gamma.flags_filed, 0);
        assert_eq!(gamma.flags_pending, 0);
        assert_eq!(gamma.invitations_issued, 0);
        assert_eq!(gamma.invitations_open, 0);
        assert_eq!(gamma.invitations_answered, 0);
    }

    /// #771 D.2: invitations are tallied alongside proposals/flags, with
    /// issued/open/answered split. Expired invitations count as issued but
    /// neither open nor answered.
    #[test]
    fn civic_health_tallies_invitations() {
        let temp = tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();

        store
            .insert_amendment_invitation(&invitation(
                "ainv-1",
                "agent-a",
                "P-1.5",
                "open",
                "2026-05-01T00:00:00Z",
            ))
            .unwrap();
        store
            .insert_amendment_invitation(&invitation(
                "ainv-2",
                "agent-a",
                "P-1.9",
                "open",
                "2026-05-01T00:00:01Z",
            ))
            .unwrap();
        store
            .insert_amendment_invitation(&invitation(
                "ainv-3",
                "agent-a",
                "P-1.5",
                "answered",
                "2026-05-01T00:00:02Z",
            ))
            .unwrap();
        store
            .insert_amendment_invitation(&invitation(
                "ainv-4",
                "agent-a",
                "P-7.5",
                "expired",
                "2026-05-01T00:00:03Z",
            ))
            .unwrap();
        store
            .insert_amendment_invitation(&invitation(
                "ainv-5",
                "agent-b",
                "P-1.5",
                "open",
                "2026-05-01T00:00:04Z",
            ))
            .unwrap();

        let health = store.civic_health(None).unwrap();
        assert_eq!(health.by_agent.len(), 2);
        let a = health.by_agent.iter().find(|e| e.agent_id == "agent-a").unwrap();
        assert_eq!(a.invitations_issued, 4);
        assert_eq!(a.invitations_open, 2);
        assert_eq!(a.invitations_answered, 1);
        let b = health.by_agent.iter().find(|e| e.agent_id == "agent-b").unwrap();
        assert_eq!(b.invitations_issued, 1);
        assert_eq!(b.invitations_open, 1);
        assert_eq!(b.invitations_answered, 0);
    }

    /// `since` excludes an old item and includes a recent one, compared by
    /// absolute instant like `contract_health`.
    #[test]
    fn civic_health_since_filters_by_absolute_instant() {
        let temp = tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();

        store
            .insert_constitutional_proposal(&proposal(
                "prop-old",
                "delta.agent",
                "pending",
                "2020-01-01T00:00:00Z",
            ))
            .unwrap();
        store
            .insert_constitutional_proposal(&proposal(
                "prop-recent",
                "delta.agent",
                "pending",
                "2026-06-01T00:00:00Z",
            ))
            .unwrap();

        let health = store
            .civic_health(Some("2025-01-01T00:00:00Z"))
            .unwrap();
        assert_eq!(health.by_agent.len(), 1);
        assert_eq!(health.by_agent[0].proposals_filed, 1);
        assert_eq!(health.by_agent[0].proposals_pending, 1);

        let health_all = store.civic_health(None).unwrap();
        assert_eq!(health_all.by_agent[0].proposals_filed, 2);
    }

    #[test]
    fn civic_health_invalid_since_errors_clearly() {
        let temp = tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();
        let err = store
            .civic_health(Some("not-a-timestamp"))
            .expect_err("invalid since must error");
        assert!(
            err.to_string().contains("invalid `since` timestamp"),
            "unexpected error: {err}"
        );
    }
}

/// DISCRETION LEAK register read side (#771 D.3): "top leaks this window".
#[cfg(test)]
mod discretion_leak_summary_tests {
    use super::*;
    use crate::runtime::discretion_leak::DiscretionLeakTally;
    use autonoetic_types::causal_chain::CausalEventRecord;
    use tempfile::tempdir;

    fn leak(seq: u64, rule: &str, kind: &str, secs_ago: i64) -> CausalEventRecord {
        CausalEventRecord {
            event_id: format!("leak-{seq}"),
            agent_id: "coder.default".to_string(),
            session_id: "sess-1".to_string(),
            turn_id: None,
            event_seq: seq,
            timestamp: (chrono::Utc::now() - chrono::Duration::seconds(secs_ago)).to_rfc3339(),
            category: "discretion_leak".to_string(),
            action: kind.to_string(),
            status: "recorded".to_string(),
            enforced_rules: vec![rule.to_string()],
            target: None,
            payload: None,
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        }
    }

    #[test]
    fn summary_tallies_by_rule_and_kind_with_window() {
        let temp = tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();

        store.create_causal_event(&leak(1, "P-5.2", "lenient_string_coercion", 10)).unwrap();
        store.create_causal_event(&leak(2, "P-5.2", "lenient_string_coercion", 20)).unwrap();
        store.create_causal_event(&leak(3, "P-5.2", "fuzzy_patch_match", 30)).unwrap();
        store.create_causal_event(&leak(4, "P-5.8", "gateway_authored_repair", 40)).unwrap();
        // A non-leak event in the same window must not be tallied.
        store
            .create_causal_event(&CausalEventRecord {
                event_id: "not-a-leak".to_string(),
                agent_id: "coder.default".to_string(),
                session_id: "sess-1".to_string(),
                turn_id: None,
                event_seq: 99,
                timestamp: chrono::Utc::now().to_rfc3339(),
                category: "tool".to_string(),
                action: "failure".to_string(),
                status: "ERROR".to_string(),
                enforced_rules: vec!["P-1.5".to_string()],
                target: None,
                payload: None,
                payload_ref: None,
                evidence_ref: None,
                reason: None,
            })
            .unwrap();

        let all = store.discretion_leak_summary(None).unwrap();
        assert_eq!(
            all,
            vec![
                DiscretionLeakTally {
                    rule_id: "P-5.2".to_string(),
                    kind: "lenient_string_coercion".to_string(),
                    count: 2,
                },
                DiscretionLeakTally {
                    rule_id: "P-5.2".to_string(),
                    kind: "fuzzy_patch_match".to_string(),
                    count: 1,
                },
                DiscretionLeakTally {
                    rule_id: "P-5.8".to_string(),
                    kind: "gateway_authored_repair".to_string(),
                    count: 1,
                },
            ]
        );

        // Window bound: only the last 15s — one coercion event remains.
        let since = (chrono::Utc::now() - chrono::Duration::seconds(15)).to_rfc3339();
        let windowed = store.discretion_leak_summary(Some(&since)).unwrap();
        assert_eq!(
            windowed,
            vec![DiscretionLeakTally {
                rule_id: "P-5.2".to_string(),
                kind: "lenient_string_coercion".to_string(),
                count: 1,
            }]
        );
    }

    #[test]
    fn summary_invalid_since_errors_clearly() {
        let temp = tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();
        let err = store
            .discretion_leak_summary(Some("not-a-timestamp"))
            .expect_err("invalid since must error");
        assert!(
            err.to_string().contains("invalid `since` timestamp"),
            "unexpected error: {err}"
        );
    }
}
