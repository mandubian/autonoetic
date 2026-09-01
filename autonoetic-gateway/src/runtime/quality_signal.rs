//! Per-session quality signal extraction (auto-learning pipeline).
//!
//! After each completed (non-suspended) session, a lightweight quality signal
//! is computed from execution traces and persisted as a Tier-2 memory tagged
//! `source:quality_signal`. The evolution curator uses these to score agents
//! without requiring explicit eval suites.
//!
//! Gated by `config.auto_learning.enabled && config.auto_learning.quality_signals`.

use std::sync::Arc;

use autonoetic_types::config::GatewayConfig;
use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};
use autonoetic_types::session_outcome::SessionCloseOutcome;

use crate::runtime::memory::MemoryStore;
use crate::scheduler::gateway_store::GatewayStore;

/// Compute and persist a quality signal for a completed session.
///
/// Called from the same site as `maybe_run_post_session_digest`, after
/// spawn/checkpoint completion.
pub async fn maybe_emit_quality_signal(
    config: &GatewayConfig,
    store: Option<&Arc<GatewayStore>>,
    memory_store: &dyn MemoryStore,
    session_id: &str,
    agent_id: &str,
    turn_count: u64,
    session_suspended: bool,
    close_outcome: SessionCloseOutcome,
) {
    if !config.auto_learning.enabled || !config.auto_learning.quality_signals {
        return;
    }
    if session_suspended {
        return;
    }
    let Some(store) = store else {
        return;
    };
    if turn_count < 1 {
        return;
    }

    let signal = match compute_signal(
        store,
        session_id,
        agent_id,
        turn_count,
        close_outcome.is_clean_completion(),
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                target: "quality_signal",
                session_id = %session_id,
                error = %e,
                "Failed to compute quality signal"
            );
            return;
        }
    };

    let session_prefix: String = session_id.chars().take(16).collect();
    let memory_id = format!("qs-{}-{}", session_prefix, autonoetic_types::id_format::short_random_id(""));
    let content = serde_json::to_string_pretty(&signal).unwrap_or_default();

    let mut obj = MemoryObject::new(
        memory_id,
        "quality_signals".to_string(),
        agent_id.to_string(),
        "gateway".to_string(),
        format!("session:{session_id}:quality_signal"),
        content,
    );
    obj.source_type = MemorySourceType::QualitySignal;
    obj.tags = vec![
        "source:quality_signal".to_string(),
        format!("agent:{agent_id}"),
        format!("turns:{turn_count}"),
    ];
    obj.confidence = Some(signal.overall_score());
    obj.visibility = MemoryVisibility::Global;

    if let Err(e) = memory_store.upsert(&obj).await {
        tracing::debug!(
            target: "quality_signal",
            session_id = %session_id,
            error = %e,
            "Failed to persist quality signal"
        );
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct QualitySignal {
    session_id: String,
    agent_id: String,
    turn_count: u64,
    tool_call_count: u64,
    error_count: u64,
    approval_count: u64,
    completed_normally: bool,
}

impl QualitySignal {
    pub fn overall_score(&self) -> f64 {
        let mut score = 0.7_f64;

        if self.turn_count > 0 && self.error_count == 0 {
            score += 0.1;
        }

        let error_ratio = if self.tool_call_count > 0 {
            self.error_count as f64 / self.tool_call_count as f64
        } else {
            0.0
        };
        score -= error_ratio * 0.3;

        if self.completed_normally {
            score += 0.1;
        }

        score.clamp(0.0, 1.0)
    }
}

fn compute_signal(
    store: &GatewayStore,
    session_id: &str,
    agent_id: &str,
    turn_count: u64,
    completed_normally: bool,
) -> anyhow::Result<QualitySignal> {
    let events = store.search_causal_events(Some(session_id), None, 500)?;

    let tool_call_count = events
        .iter()
        .filter(|e| {
            (e.category == "tool_invoke" && e.action == "completed")
                || (e.category == "tool" && e.action == "failure")
        })
        .count() as u64;

    let error_count = events
        .iter()
        .filter(|e| {
            e.status.eq_ignore_ascii_case("error") || e.status.eq_ignore_ascii_case("failed")
        })
        .count() as u64;

    let approval_count = events
        .iter()
        .filter(|e| {
            e.action == "approval_created"
                || e.action == "gate_suspended"
                || (e.category == "tool_invoke"
                    && e.action == "completed"
                    && e
                        .payload
                        .as_deref()
                        .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                        .and_then(|v| v.get("approval_request_id").cloned())
                        .is_some())
        })
        .count() as u64;

    Ok(QualitySignal {
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        turn_count,
        tool_call_count,
        error_count,
        approval_count,
        completed_normally,
    })
}

/// Aggregated per-agent metrics derived from persisted quality_signal memories.
#[derive(Debug, serde::Serialize)]
pub struct AgentQualityTrendRow {
    pub agent_id: String,
    pub sessions_observed: usize,
    pub avg_overall_score: f64,
    pub avg_error_count: f64,
    pub avg_tool_calls: f64,
    pub avg_approval_count: f64,
    pub fraction_completed_normally: f64,
}

/// Build a structured JSON trend report from recent Tier-2 `quality_signal` memories.
///
/// Returns an error when **no** parseable quality signals exist (operators expect explicit
/// failure rather than silent empty reports).
pub fn build_quality_trend_report(
    store: &GatewayStore,
    memory_limit: usize,
    agent_filter: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    use std::collections::HashMap;

    let memories = store.search_memories_by_tags(&["source:quality_signal"], memory_limit)?;

    let mut signals: Vec<QualitySignal> = Vec::new();
    for m in memories {
        match serde_json::from_str::<QualitySignal>(&m.content) {
            Ok(s) => {
                if let Some(prefix) = agent_filter {
                    if s.agent_id != prefix && !s.agent_id.contains(prefix) {
                        continue;
                    }
                }
                signals.push(s);
            }
            Err(e) => {
                tracing::warn!(
                    target: "quality_signal",
                    memory_id = %m.memory_id,
                    error = %e,
                    "Skipping malformed quality_signal memory row"
                );
            }
        }
    }

    if signals.is_empty() {
        anyhow::bail!(
            "No quality_signal memories available for trend analysis. \
             Complete agent sessions with auto_learning.enabled=true and auto_learning.quality_signals=true."
        );
    }

    let mut by_agent: HashMap<String, Vec<QualitySignal>> = HashMap::new();
    for s in signals {
        by_agent.entry(s.agent_id.clone()).or_default().push(s);
    }

    let mut rows: Vec<AgentQualityTrendRow> = Vec::new();
    for (agent_id, sess) in by_agent {
        let n = sess.len();
        let nf = n as f64;
        let sum_score: f64 = sess.iter().map(QualitySignal::overall_score).sum();
        let sum_err: f64 = sess.iter().map(|x| x.error_count as f64).sum();
        let sum_tools: f64 = sess.iter().map(|x| x.tool_call_count as f64).sum();
        let sum_appr: f64 = sess.iter().map(|x| x.approval_count as f64).sum();
        let completed: f64 = sess.iter().filter(|x| x.completed_normally).count() as f64;
        rows.push(AgentQualityTrendRow {
            agent_id,
            sessions_observed: n,
            avg_overall_score: sum_score / nf,
            avg_error_count: sum_err / nf,
            avg_tool_calls: sum_tools / nf,
            avg_approval_count: sum_appr / nf,
            fraction_completed_normally: completed / nf,
        });
    }

    rows.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));

    Ok(serde_json::json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "memory_limit_requested": memory_limit,
        "agent_filter": agent_filter,
        "agents": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::causal_chain::CausalEventRecord;

    fn event(session_id: &str, category: &str, action: &str, status: &str, payload: Option<String>) -> CausalEventRecord {
        CausalEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            agent_id: "planner.default".to_string(),
            session_id: session_id.to_string(),
            turn_id: Some("turn-000001".to_string()),
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: category.to_string(),
            action: action.to_string(),
            status: status.to_string(),
            enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
            target: None,
            payload,
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        }
    }

    #[test]
    fn completed_normally_true_adds_completion_bonus() {
        let tmp = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(tmp.path()).unwrap();
        let sid = "session-test-clean";
        store.create_causal_event(&event(sid, "session", "start", "SUCCESS", None)).unwrap();
        store.create_causal_event(&event(sid, "lifecycle", "hibernate", "SUCCESS", None)).unwrap();
        store.create_causal_event(&event(sid, "session", "history.persisted", "SUCCESS", None)).unwrap();

        let signal = compute_signal(&store, sid, "planner.default", 1, true).unwrap();
        assert!(signal.completed_normally);
        assert_eq!(signal.tool_call_count, 0);
        assert_eq!(signal.error_count, 0);
        assert_eq!(signal.approval_count, 0);
        assert!((signal.overall_score() - 0.9).abs() < 1e-9);
    }

    #[test]
    fn errored_close_is_not_completed_normally() {
        let tmp = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(tmp.path()).unwrap();
        let sid = "session-test-error";
        store.create_causal_event(&event(sid, "session", "start", "SUCCESS", None)).unwrap();
        store.create_causal_event(&event(sid, "context_governor", "error", "ERROR", None)).unwrap();

        let signal = compute_signal(&store, sid, "planner.default", 2, false).unwrap();
        assert!(!signal.completed_normally);
        assert_eq!(signal.error_count, 1);
        assert_eq!(signal.overall_score(), 0.7);
    }

    #[test]
    fn tool_calls_counted_from_tracer_vocabulary() {
        let tmp = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(tmp.path()).unwrap();
        let sid = "session-test-tools";
        store.create_causal_event(&event(sid, "tool_invoke", "requested", "SUCCESS", None)).unwrap();
        store.create_causal_event(&event(sid, "tool_invoke", "completed", "SUCCESS", None)).unwrap();
        store.create_causal_event(&event(sid, "tool", "failure", "ERROR", None)).unwrap();
        store.create_causal_event(&event(sid, "tool_call", "cache_hit", "SUCCESS", None)).unwrap();

        let signal = compute_signal(&store, sid, "planner.default", 1, true).unwrap();
        assert_eq!(signal.tool_call_count, 2);
        assert_eq!(signal.error_count, 1);
    }

    #[test]
    fn approvals_counted_from_tool_completion_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(tmp.path()).unwrap();
        let sid = "session-test-approval";
        store
            .create_causal_event(&event(
                sid,
                "tool_invoke",
                "completed",
                "SUCCESS",
                Some(serde_json::json!({"approval_request_id": "apr-1"}).to_string()),
            ))
            .unwrap();
        store.create_causal_event(&event(sid, "tool_invoke", "completed", "SUCCESS", None)).unwrap();

        let signal = compute_signal(&store, sid, "planner.default", 1, false).unwrap();
        assert_eq!(signal.approval_count, 1);
        assert_eq!(signal.tool_call_count, 2);
    }
}
