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

    let signal = match compute_signal(store, session_id, agent_id, turn_count) {
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
) -> anyhow::Result<QualitySignal> {
    let events = store.search_causal_events(Some(session_id), None, 500)?;

    let tool_call_count = events
        .iter()
        .filter(|e| e.action == "tool_requested" || e.action == "tool_completed")
        .count() as u64
        / 2;

    let error_count = events
        .iter()
        .filter(|e| e.status == "error" || e.status == "failed")
        .count() as u64;

    let approval_count = events
        .iter()
        .filter(|e| e.action == "approval_created" || e.action == "gate_suspended")
        .count() as u64;

    let completed_normally = events
        .iter()
        .any(|e| e.action == "session_closed" && e.status == "ok");

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
