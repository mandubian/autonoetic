//! Unified operator pending-decisions view (issue #722, Stage 1).
//!
//! An operator's outstanding decisions are spread across four backing stores —
//! `approvals`, `user_interactions`, `escalations`, and `plan_frames` — each
//! with its own list RPC and its own answer verb. There is no single "what is
//! waiting on me" query, so a headless operator must poll four RPC families and
//! the room TUI has to re-unify them client-side just to render one list.
//!
//! This module is the server-side version of that unification: a single
//! read-only aggregation over the four sources for one root session, returning
//! a normalized [`PendingDecision`] list (oldest-first) that carries, per item,
//! the answer RPC the operator would call to resolve it. It is deliberately
//! additive — it reads the existing tables and changes no control flow. Stage 2
//! (a single expiry policy) and Stage 3 (a CLI answer path) build on it.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::scheduler::gateway_store::GatewayStore;

/// The kind of pending decision, mirroring the room TUI's `GateKind` so both
/// surfaces classify the four sources identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingKind {
    Approval,
    Interaction,
    Escalation,
    Plan,
}

/// The RPC an operator calls to resolve a given decision, plus a hint of the
/// id-bearing params. This lets a non-TUI caller act without hard-coding which
/// verb goes with which kind.
#[derive(Debug, Clone, Serialize)]
pub struct AnswerHint {
    /// JSON-RPC method name (e.g. `approvals.approve`).
    pub method: String,
    /// Minimal params the method needs to target this item (e.g.
    /// `{"request_id": "apr-…"}`). Callers add their decision fields
    /// (`decided_by`, `reason`, answer text, …) on top.
    pub params: serde_json::Value,
}

/// One outstanding operator decision, normalized across the four sources.
#[derive(Debug, Clone, Serialize)]
pub struct PendingDecision {
    pub kind: PendingKind,
    /// Native id in its source table (`apr-…` / `ui-…` / `esc_…` / plan id).
    pub id: String,
    pub root_session_id: Option<String>,
    pub agent_id: String,
    /// Set for workflow-bound approvals (resume routes through task dispatch).
    pub workflow_id: Option<String>,
    /// RFC3339 creation timestamp as stored.
    pub created_at: String,
    /// Seconds since `created_at`, or `None` if the timestamp is unparseable.
    pub age_secs: Option<i64>,
    /// One-line human summary for a queue row.
    pub summary: String,
    /// How to resolve this item.
    pub answer: AnswerHint,
}

fn age_secs(created_at: &str, now: DateTime<Utc>) -> Option<i64> {
    DateTime::parse_from_rfc3339(created_at)
        .ok()
        .map(|t| (now - t.with_timezone(&Utc)).num_seconds())
}

/// Short label for an approval's action, taken from the `ScheduledAction`
/// serde `type` tag so it stays correct as variants are added/removed.
fn approval_action_label(action: &autonoetic_types::background::ScheduledAction) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(|t| t.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "action".to_string())
}

/// Collect every pending decision for a root session, normalized and sorted
/// oldest-first. Read-only; a failure in any one source is surfaced (the whole
/// call fails) rather than silently returning a partial queue.
pub fn collect_pending_for_root(
    store: &GatewayStore,
    root_session_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<PendingDecision>> {
    let mut out: Vec<PendingDecision> = Vec::new();

    // 1. Approvals (network/exec/promote/credential/session-continue/wiki).
    for app in store.get_pending_approvals_for_root(root_session_id)? {
        let label = approval_action_label(&app.action);
        let summary = match app.reason.as_deref() {
            Some(r) if !r.trim().is_empty() => format!("{label}: {r}"),
            _ => format!("{label} — approval required"),
        };
        out.push(PendingDecision {
            kind: PendingKind::Approval,
            answer: AnswerHint {
                method: "approvals.approve".to_string(),
                params: serde_json::json!({ "request_id": app.request_id }),
            },
            age_secs: age_secs(&app.created_at, now),
            id: app.request_id,
            root_session_id: app.root_session_id,
            agent_id: app.agent_id,
            workflow_id: app.workflow_id,
            created_at: app.created_at,
            summary,
        });
    }

    // 2. User interactions (agent questions, divergence-stop prompts).
    for i in store.get_pending_interactions_for_root_session(root_session_id)? {
        out.push(PendingDecision {
            kind: PendingKind::Interaction,
            answer: AnswerHint {
                method: "interaction.answer".to_string(),
                params: serde_json::json!({ "interaction_id": i.interaction_id }),
            },
            age_secs: age_secs(&i.created_at, now),
            id: i.interaction_id,
            root_session_id: Some(i.root_session_id),
            agent_id: i.agent_id,
            workflow_id: None,
            created_at: i.created_at,
            summary: i.question,
        });
    }

    // 3. Escalations (guidance requests + federation promotion reviews). The
    //    store lists them globally; filter to this root in-memory (Stage 1 —
    //    a root-scoped store query is a fast-follow if this proves hot).
    for e in store.list_pending_escalations()? {
        if e.root_session_id != root_session_id {
            continue;
        }
        let summary = if e.planner_synthesis.trim().is_empty() {
            format!(
                "{}: {} rev {}",
                e.escalation_type.as_str().replace('_', " "),
                e.agent_id,
                e.revision_id
            )
        } else {
            e.planner_synthesis.clone()
        };
        out.push(PendingDecision {
            kind: PendingKind::Escalation,
            answer: AnswerHint {
                method: "admin.escalation_resolve".to_string(),
                params: serde_json::json!({ "escalation_id": e.escalation_id }),
            },
            age_secs: age_secs(&e.created_at, now),
            id: e.escalation_id,
            root_session_id: Some(e.root_session_id),
            agent_id: e.agent_id,
            workflow_id: None,
            created_at: e.created_at,
            summary,
        });
    }

    // 4. Plan frames awaiting approval.
    for p in store.list_pending_plan_frames_for_root(root_session_id)? {
        out.push(PendingDecision {
            kind: PendingKind::Plan,
            answer: AnswerHint {
                method: "planframes.approve".to_string(),
                params: serde_json::json!({ "plan_id": p.plan_id }),
            },
            age_secs: age_secs(&p.created_at, now),
            id: p.plan_id,
            root_session_id: Some(p.root_session_id),
            agent_id: p.created_by_agent_id,
            workflow_id: Some(p.workflow_id),
            created_at: p.created_at,
            summary: p.title,
        });
    }

    // Oldest-first: the operator sees what has waited longest at the top.
    // Unparseable timestamps (None age) sort last.
    out.sort_by(|a, b| match (b.age_secs, a.age_secs) {
        (Some(bs), Some(as_)) => bs.cmp(&as_),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    Ok(out)
}
