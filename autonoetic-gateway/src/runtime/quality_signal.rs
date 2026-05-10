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

    let memory_id = format!("qs-{}-{}", &session_id[..session_id.len().min(16)], &uuid::Uuid::new_v4().to_string()[..8]);
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

#[derive(Debug, serde::Serialize)]
struct QualitySignal {
    session_id: String,
    agent_id: String,
    turn_count: u64,
    tool_call_count: u64,
    error_count: u64,
    approval_count: u64,
    completed_normally: bool,
}

impl QualitySignal {
    fn overall_score(&self) -> f64 {
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
