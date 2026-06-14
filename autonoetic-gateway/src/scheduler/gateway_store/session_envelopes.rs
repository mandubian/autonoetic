use anyhow::{Context, Result};
use autonoetic_types::background::ScheduledAction;
use autonoetic_types::capability::Capability;
use autonoetic_types::causal_chain::ExecutionTraceRecord;
use rusqlite::{params, Connection};
use std::collections::BTreeSet;

use crate::runtime::approved_exec_cache::normalize_targets;
use crate::runtime::remote_access::RemoteAccessAnalyzer;

use super::GatewayStore;

const NETWORK_TRACE_TOOLS: &[&str] = &[
    "sandbox_exec",
    "artifact_exec",
    "web_fetch",
    "web_call",
    "web_search",
];

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionEnvelopeRecord {
    pub id: i64,
    pub root_session_id: String,
    pub capability: Capability,
    pub source: String,
    pub observed_at: Option<String>,
    pub locked_at: Option<String>,
    pub locked_by: Option<String>,
    pub plan_id: Option<String>,
    pub created_at: String,
}

impl GatewayStore {
    /// Hosts that this root session has actually touched, derived from
    /// `execution_traces` and resolved (approved) approval requests.
    pub fn discover_observed_hosts(&self, root_session_id: &str) -> Result<Vec<String>> {
        let mut hosts = BTreeSet::new();

        let traces = self.search_execution_traces(
            None,
            None,
            None,
            None,
            None,
            Some(root_session_id),
            10_000,
        )?;
        for trace in traces {
            collect_hosts_from_trace(&trace, &mut hosts);
        }

        for approval in self.get_approved_approvals_for_root(root_session_id)? {
            collect_hosts_from_action(&approval.action, &mut hosts);
        }

        let mut sorted: Vec<String> = hosts.into_iter().collect();
        sorted.sort();
        Ok(sorted)
    }

    pub fn insert_envelope_proposal(
        &self,
        root_session_id: &str,
        capability: &Capability,
        source: &str,
        observed_at: Option<&str>,
        plan_id: Option<&str>,
        created_at: &str,
    ) -> Result<i64> {
        let capability_json =
            serde_json::to_string(capability).context("serialize session envelope capability")?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_envelopes (
                root_session_id, capability_json, source, observed_at,
                locked_at, locked_by, plan_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?6)",
            params![
                root_session_id,
                capability_json,
                source,
                observed_at,
                plan_id,
                created_at,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn lock_envelope(
        &self,
        envelope_id: i64,
        locked_by: &str,
        locked_at: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE session_envelopes
             SET locked_at = ?1, locked_by = ?2
             WHERE id = ?3 AND locked_at IS NULL",
            params![locked_at, locked_by, envelope_id],
        )?;
        Ok(updated > 0)
    }

    pub fn get_active_envelopes(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<SessionEnvelopeRecord>> {
        let conn = self.conn.lock().unwrap();
        load_envelopes(
            &conn,
            "SELECT id, root_session_id, capability_json, source, observed_at,
                    locked_at, locked_by, plan_id, created_at
             FROM session_envelopes
             WHERE root_session_id = ?1 AND locked_at IS NOT NULL
             ORDER BY created_at ASC",
            params![root_session_id],
        )
    }

    pub fn get_proposed_envelopes(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<SessionEnvelopeRecord>> {
        let conn = self.conn.lock().unwrap();
        load_envelopes(
            &conn,
            "SELECT id, root_session_id, capability_json, source, observed_at,
                    locked_at, locked_by, plan_id, created_at
             FROM session_envelopes
             WHERE root_session_id = ?1 AND locked_at IS NULL
             ORDER BY created_at ASC",
            params![root_session_id],
        )
    }

    pub fn get_envelope_by_id(&self, envelope_id: i64) -> Result<Option<SessionEnvelopeRecord>> {
        let conn = self.conn.lock().unwrap();
        load_envelopes(
            &conn,
            "SELECT id, root_session_id, capability_json, source, observed_at,
                    locked_at, locked_by, plan_id, created_at
             FROM session_envelopes
             WHERE id = ?1",
            params![envelope_id],
        )
        .map(|mut v| v.pop())
    }
}

fn load_envelopes<P>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<SessionEnvelopeRecord>>
where
    P: rusqlite::Params,
{
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |row| {
        let capability_json: String = row.get(2)?;
        let capability: Capability = serde_json::from_str(&capability_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;
        Ok(SessionEnvelopeRecord {
            id: row.get(0)?,
            root_session_id: row.get(1)?,
            capability,
            source: row.get(3)?,
            observed_at: row.get(4)?,
            locked_at: row.get(5)?,
            locked_by: row.get(6)?,
            plan_id: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

fn collect_hosts_from_trace(trace: &ExecutionTraceRecord, hosts: &mut BTreeSet<String>) {
    if !NETWORK_TRACE_TOOLS.contains(&trace.tool_name.as_str()) {
        return;
    }

    if let Some(command) = trace.command.as_deref() {
        extend_hosts(hosts, hosts_from_text(command));
    }
    if let Some(arguments) = trace.arguments.as_deref() {
        extend_hosts(hosts, hosts_from_trace_arguments(&trace.tool_name, arguments));
    }
}

fn collect_hosts_from_action(action: &ScheduledAction, hosts: &mut BTreeSet<String>) {
    if let Some(detected) = action.detected_hosts() {
        extend_hosts(hosts, detected);
        return;
    }

    match action {
        ScheduledAction::SandboxExec { command, .. } => {
            extend_hosts(hosts, hosts_from_text(command));
        }
        ScheduledAction::WebFetch { url, .. }
        | ScheduledAction::WebCall { url, .. } => {
            extend_hosts(hosts, hosts_from_text(url));
        }
        ScheduledAction::WebSearch {
            engine_url,
            duckduckgo_engine_url,
            google_engine_url,
            ..
        } => {
            for url in [engine_url, duckduckgo_engine_url, google_engine_url]
                .into_iter()
                .flatten()
            {
                extend_hosts(hosts, hosts_from_text(url));
            }
        }
        _ => {}
    }
}

fn hosts_from_trace_arguments(tool_name: &str, arguments_json: &str) -> Vec<String> {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments_json) else {
        return Vec::new();
    };

    let mut hosts = Vec::new();
    for key in [
        "command",
        "cmd",
        "url",
        "engine_url",
        "duckduckgo_engine_url",
        "google_engine_url",
    ] {
        if let Some(text) = args.get(key).and_then(|v| v.as_str()) {
            hosts.extend(hosts_from_text(text));
        }
    }
    if tool_name == "sandbox_exec" {
        if let Some(script) = args.get("script").and_then(|v| v.as_str()) {
            hosts.extend(hosts_from_text(script));
        }
    }
    hosts
}

fn hosts_from_text(text: &str) -> Vec<String> {
    let analysis =
        RemoteAccessAnalyzer::analyze_command_and_dependencies(text, None);
    let mut hosts = normalize_targets(&analysis.detected_patterns);
    if hosts.is_empty() {
        hosts = extract_url_hosts_from_text(text);
    }
    hosts
}

fn extract_url_hosts_from_text(text: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"(?i)https?://([^/\s:"'`]+)"#).unwrap());
    let mut hosts: Vec<String> = re
        .captures_iter(text)
        .filter_map(|cap| cap.get(1))
        .map(|m| m.as_str().trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| is_concrete_host(h))
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

fn extend_hosts(hosts: &mut BTreeSet<String>, discovered: Vec<String>) {
    for host in discovered {
        if is_concrete_host(&host) {
            hosts.insert(host);
        }
    }
}

fn is_concrete_host(host: &str) -> bool {
    !host.is_empty() && host != "*"
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::background::{ApprovalLevel, ApprovalRequest};
    use autonoetic_types::causal_chain::ExecutionTraceRecord;
    use tempfile::tempdir;

    fn curl_trace(session_id: &str, command: &str) -> ExecutionTraceRecord {
        ExecutionTraceRecord {
            trace_id: format!("trace-{}", uuid::Uuid::new_v4()),
            event_id: None,
            agent_id: "researcher.default".to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            timestamp: "2026-06-14T12:00:00Z".to_string(),
            tool_name: "sandbox_exec".to_string(),
            command: Some(command.to_string()),
            exit_code: Some(0),
            stdout: None,
            stderr: None,
            duration_ms: 10,
            success: 1,
            error_type: None,
            error_summary: None,
            approval_required: None,
            approval_request_id: None,
            arguments: Some(format!(r#"{{"command":"{command}"}}"#)),
            result: None,
        }
    }

    #[test]
    fn discover_observed_hosts_from_execution_traces() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-root-501";

        store.create_execution_trace(&curl_trace(
            root,
            "curl -s https://api.open-meteo.com/v1/forecast?latitude=48.8",
        ))?;
        store.create_execution_trace(&curl_trace(
            &format!("{root}/child"),
            "curl -s https://geocoding-api.open-meteo.com/v1/search?name=Paris",
        ))?;

        let hosts = store.discover_observed_hosts(root)?;
        assert_eq!(
            hosts,
            vec![
                "api.open-meteo.com".to_string(),
                "geocoding-api.open-meteo.com".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn discover_observed_hosts_unions_approved_approvals() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-root-approval";

        let approval = ApprovalRequest {
            request_id: "apr-501".to_string(),
            agent_id: "researcher.default".to_string(),
            session_id: root.to_string(),
            action: ScheduledAction::WebFetch {
                url: "https://archive.example.org/data".to_string(),
                timeout_secs: None,
                max_chars: None,
                detected_hosts: Some(vec!["archive.example.org".to_string()]),
                payload: None,
            },
            approval_level: ApprovalLevel::Operator,
            created_at: "2026-06-14T12:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: Some(root.to_string()),
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
        };
        let mut approval = approval;
        store.create_approval(&mut approval)?;
        store.record_decision(
            "apr-501",
            "approved",
            "operator",
            "2026-06-14T12:01:00Z",
            None,
        )?;

        let hosts = store.discover_observed_hosts(root)?;
        assert_eq!(hosts, vec!["archive.example.org".to_string()]);
        Ok(())
    }

    #[test]
    fn discover_observed_hosts_dedups_across_traces_and_approvals() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-root-dedup";
        let host = "api.open-meteo.com";

        store.create_execution_trace(&curl_trace(
            root,
            "curl -s https://api.open-meteo.com/v1/forecast",
        ))?;

        let mut approval = ApprovalRequest {
            request_id: "apr-dedup".to_string(),
            agent_id: "researcher.default".to_string(),
            session_id: root.to_string(),
            action: ScheduledAction::WebFetch {
                url: format!("https://{host}/v1/forecast"),
                timeout_secs: None,
                max_chars: None,
                detected_hosts: Some(vec![host.to_string()]),
                payload: None,
            },
            approval_level: ApprovalLevel::Operator,
            created_at: "2026-06-14T12:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: Some(root.to_string()),
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
        };
        store.create_approval(&mut approval)?;
        store.record_decision(
            "apr-dedup",
            "approved",
            "operator",
            "2026-06-14T12:01:00Z",
            None,
        )?;

        let hosts = store.discover_observed_hosts(root)?;
        assert_eq!(hosts, vec![host.to_string()]);
        Ok(())
    }

    #[test]
    fn session_envelope_crud_propose_lock_query() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-root-envelope";
        let capability = Capability::NetworkAccess {
            hosts: vec!["api.open-meteo.com".to_string()],
        };

        let id = store.insert_envelope_proposal(
            root,
            &capability,
            "discovered",
            Some("2026-06-14T11:00:00Z"),
            None,
            "2026-06-14T12:00:00Z",
        )?;

        let proposed = store.get_proposed_envelopes(root)?;
        assert_eq!(proposed.len(), 1);
        assert_eq!(proposed[0].id, id);
        assert!(proposed[0].locked_at.is_none());
        assert!(matches!(
            &proposed[0].capability,
            Capability::NetworkAccess { hosts } if hosts == &["api.open-meteo.com"]
        ));

        assert!(store.lock_envelope(id, "operator", "2026-06-14T12:05:00Z")?);
        assert!(!store.lock_envelope(id, "operator", "2026-06-14T12:06:00Z")?);

        assert!(store.get_proposed_envelopes(root)?.is_empty());
        let active = store.get_active_envelopes(root)?;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].locked_by.as_deref(), Some("operator"));
        assert_eq!(
            active[0].locked_at.as_deref(),
            Some("2026-06-14T12:05:00Z")
        );
        Ok(())
    }
}
