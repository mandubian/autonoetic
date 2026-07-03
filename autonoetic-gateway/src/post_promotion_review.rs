//! Post-promotion background review (Phase 4, Tier 1).
//!
//! For every promoted agent, periodically reviews:
//! - Causal event trends (tool failures, authorization denials, suspensions)
//! - Sentinel findings accumulated since the last review
//! - Emits findings or `operator_alert` timeline events when thresholds are exceeded
//!
//! #739 Part C: post-promotion anomalies are **alerts** (timeline events), not
//! actionable operator decisions. They no longer create an `escalations` row
//! that could masquerade as a resolvable decision in `operator.pending`. If a
//! future actionable "review rollback?" decision is needed, it should be an
//! explicit approval, not an anomaly escalation.

use std::sync::Arc;

use serde::Serialize;

use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::escalation::RoleVerdictSummary;

/// Result of a single agent's post-promotion review.
#[derive(Debug, Clone, Serialize)]
pub struct AgentReviewResult {
    pub agent_id: String,
    pub revision_id: String,
    pub tool_failures_since_last: i64,
    pub auth_denials_since_last: i64,
    pub suspensions_since_last: i64,
    pub sentinel_findings_since_last: i64,
    pub previous_tool_failures: i64,
    pub previous_auth_denials: i64,
    pub previous_suspensions: i64,
    pub findings: Vec<ReviewFinding>,
    pub escalated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewFinding {
    pub severity: String,
    pub message: String,
    pub metric: String,
    pub current_value: i64,
    pub previous_value: i64,
}

/// Run post-promotion review for all promoted agents.
///
/// Returns a list of agent review results. Escalations are created
/// in the store when critical findings are detected.
pub fn run_post_promotion_review(
    store: &Arc<GatewayStore>,
) -> anyhow::Result<Vec<AgentReviewResult>> {
    let last_review_at = get_last_review_timestamp(store)?;
    let now_rfc = chrono::Utc::now().to_rfc3339();

    let aliases = store.list_agent_aliases(None)?;

    let mut results = Vec::new();

    for alias in &aliases {
        let agent_id = &alias.agent_id;
        let revision_id = &alias.revision_id;

        let tool_failures = store.count_tool_failures_since(agent_id, &last_review_at)?;
        let auth_denials = store.count_auth_denials_since(agent_id, &last_review_at)?;
        let suspensions = store.count_suspensions_since(agent_id, &last_review_at)?;
        let sentinel_findings =
            store.count_sentinel_findings_for_agent_since(agent_id, &last_review_at)?;

        let previous = load_previous_review(store, agent_id)?;

        let mut findings = Vec::new();
        let mut escalated = false;

        let tool_failure_rate = if previous.tool_failures > 0 {
            (tool_failures as f64) / (previous.tool_failures as f64)
        } else if tool_failures > 0 {
            2.0
        } else {
            0.0
        };

        if tool_failure_rate > 1.5 {
            findings.push(ReviewFinding {
                severity: if tool_failure_rate > 3.0 { "critical".to_string() } else { "warning".to_string() },
                message: format!(
                    "Tool failure rate increased {:.1}x ({} vs {} in previous review)",
                    tool_failure_rate, tool_failures, previous.tool_failures
                ),
                metric: "tool_failure_rate".to_string(),
                current_value: tool_failures,
                previous_value: previous.tool_failures,
            });
        }

        if auth_denials > previous.auth_denials * 2 && auth_denials > 0 {
            findings.push(ReviewFinding {
                severity: "warning".to_string(),
                message: format!(
                    "Authorization denials increased ({} vs {} in previous review)",
                    auth_denials, previous.auth_denials
                ),
                metric: "auth_denials".to_string(),
                current_value: auth_denials,
                previous_value: previous.auth_denials,
            });
        }

        if suspensions > previous.suspensions * 2 && suspensions > 1 {
            findings.push(ReviewFinding {
                severity: "critical".to_string(),
                message: format!(
                    "Session suspensions increased ({} vs {} in previous review)",
                    suspensions, previous.suspensions
                ),
                metric: "suspensions".to_string(),
                current_value: suspensions,
                previous_value: previous.suspensions,
            });
        }

        if sentinel_findings > 0 {
            findings.push(ReviewFinding {
                severity: if sentinel_findings > 2 { "critical".to_string() } else { "warning".to_string() },
                message: format!(
                    "{} new sentinel finding(s) since last review",
                    sentinel_findings
                ),
                metric: "sentinel_findings".to_string(),
                current_value: sentinel_findings,
                previous_value: 0,
            });
        }

        let critical_findings: Vec<&ReviewFinding> =
            findings.iter().filter(|f| f.severity == "critical").collect();
        if !critical_findings.is_empty() {
            let verdicts: Vec<RoleVerdictSummary> = findings.iter().map(|f| {
                RoleVerdictSummary {
                    role: autonoetic_types::promotion::PromotionRole::Evaluator,
                    agent_id: "security_sentinel".to_string(),
                    passed: false,
                    findings_summary: f.message.clone(),
                    evidence_ref: None,
                    recorded_at: chrono::Utc::now().to_rfc3339(),
                }
            }).collect();

            let synthesis = format!(
                "Post-promotion review for '{}': {} critical and {} warning findings. {}",
                agent_id,
                critical_findings.len(),
                findings.len() - critical_findings.len(),
                critical_findings.iter().map(|f| f.message.as_str()).collect::<Vec<_>>().join("; "),
            );

            let artifact_id = store
                .get_agent_revision(revision_id)
                .ok()
                .flatten()
                .and_then(|r| r.artifact_id)
                .filter(|s| !s.is_empty())
                .unwrap_or_default();

            // #739 Part C: emit an `operator_alert` timeline event instead of
            // an actionable escalation row. The alert surfaces in the timeline
            // (read model) without masquerading as a resolvable decision in
            // `operator.pending`. No `escalations` row is created at all —
            // the timeline event is the only record of the anomaly.
            emit_post_promotion_anomaly_alert(
                store.as_ref(),
                agent_id,
                revision_id,
                &artifact_id,
                &synthesis,
                critical_findings.len(),
            );
            escalated = true;
        }

        store.record_post_promotion_review(
            agent_id,
            revision_id,
            &now_rfc,
            tool_failures,
            auth_denials,
            suspensions,
            sentinel_findings,
            &serde_json::to_string(&findings)?,
        )?;

        results.push(AgentReviewResult {
            agent_id: agent_id.clone(),
            revision_id: revision_id.clone(),
            tool_failures_since_last: tool_failures,
            auth_denials_since_last: auth_denials,
            suspensions_since_last: suspensions,
            sentinel_findings_since_last: sentinel_findings,
            previous_tool_failures: previous.tool_failures,
            previous_auth_denials: previous.auth_denials,
            previous_suspensions: previous.suspensions,
            findings,
            escalated,
        });
    }

    Ok(results)
}

struct PreviousReviewValues {
    tool_failures: i64,
    auth_denials: i64,
    suspensions: i64,
}

fn load_previous_review(
    store: &Arc<GatewayStore>,
    agent_id: &str,
) -> anyhow::Result<PreviousReviewValues> {
    if let Some(last) = store.get_last_post_promotion_review(agent_id)? {
        Ok(PreviousReviewValues {
            tool_failures: last.tool_failures.max(1),
            auth_denials: last.auth_denials,
            suspensions: last.suspensions,
        })
    } else {
        Ok(PreviousReviewValues {
            tool_failures: 1,
            auth_denials: 0,
            suspensions: 0,
        })
    }
}

fn get_last_review_timestamp(store: &Arc<GatewayStore>) -> anyhow::Result<String> {
    if let Some(last) = store.get_most_recent_review_timestamp()? {
        Ok(last)
    } else {
        Ok((chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339())
    }
}

/// #739 Part C: emit a post-promotion anomaly as an `operator_alert` timeline
/// event (the same shape as the #723 approval-flood alert). Anomalies are
/// alerts — they surface in the timeline read model without masquerading as
/// resolvable operator decisions in `operator.pending`. The `agent_id` is used
/// as the synthetic root so the alert is attributable and discoverable.
fn emit_post_promotion_anomaly_alert(
    store: &GatewayStore,
    agent_id: &str,
    revision_id: &str,
    artifact_id: &str,
    synthesis: &str,
    critical_count: usize,
) {
    // Attribute the alert to the agent so it is discoverable in the timeline
    // under a stable key. Use the agent_id as the synthetic root/session.
    let root = if agent_id.is_empty() {
        "system".to_string()
    } else {
        format!("ppr:{agent_id}")
    };
    let principal = autonoetic_types::principal::Principal::agent("gateway");
    let seat = crate::runtime::session_timeline::derive_role("gateway");
    let event = crate::runtime::session_timeline::build_timeline_event(
        root.clone(),
        root,
        None,
        &principal,
        &seat,
        "operator_alert",
        None, // base altitude ⇒ Attention
        Some(serde_json::json!({
            "alert": "post_promotion_anomaly",
            "agent_id": agent_id,
            "revision_id": revision_id,
            "artifact_id": artifact_id,
            "critical_findings": critical_count,
            "message": synthesis,
        })),
        autonoetic_types::session_timeline::TimelineRefs::default(),
    );
    if let Err(e) = store.create_live_digest_event(&event) {
        tracing::debug!(
            target: "session_timeline",
            error = %e,
            agent_id,
            "post_promotion_anomaly alert timeline emit failed"
        );
    }
}
