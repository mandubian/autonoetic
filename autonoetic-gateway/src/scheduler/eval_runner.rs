//! Background eval runner — processes queued eval runs.
//!
//! Polls the gateway store for queued eval runs, executes each case by spawning
//! the subject agent, evaluates assertions against the reply and the causal
//! events the case's session recorded (gateway-state assertions, #772 E.1),
//! and persists results.

use crate::execution::GatewayExecutionService;
use crate::runtime::content_store::ContentStore;
use autonoetic_types::evaluation::{EvalCaseResultRecord, EvalRunStatus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub async fn start_eval_runner(execution: Arc<GatewayExecutionService>) -> anyhow::Result<()> {
    let config = execution.config();
    let gateway_dir = crate::execution::gateway_root_dir(&config);

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        let store = match execution.gateway_store() {
            Some(s) => s,
            None => continue,
        };

        let queued = match store.list_queued_eval_runs() {
            Ok(runs) => runs,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to list queued eval runs");
                continue;
            }
        };

        for run in queued {
            if let Err(e) = process_eval_run(execution.clone(), &gateway_dir, &store, &run).await {
                tracing::warn!(
                    eval_run_id = %run.eval_run_id,
                    error = %e,
                    "Failed to process eval run"
                );
            }
        }
    }
}

async fn process_eval_run(
    execution: Arc<GatewayExecutionService>,
    gateway_dir: &Path,
    store: &Arc<crate::scheduler::gateway_store::GatewayStore>,
    run: &autonoetic_types::evaluation::EvalRunRecord,
) -> anyhow::Result<()> {
    let suite = match store.get_eval_suite(&run.suite_id)? {
        Some(s) => s,
        None => {
            anyhow::bail!("Eval suite '{}' not found", run.suite_id);
        }
    };

    store.start_eval_run(&run.eval_run_id)?;

    let cases: Vec<EvalCase> = extract_cases(&suite.spec_json)?;
    let content_store = ContentStore::new(gateway_dir)?;

    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut case_results = Vec::new();

    for case in &cases {
        let result = execute_case(execution.clone(), &run, case, &content_store, store).await;

        match &result {
            Ok(r) => {
                if r.status == "passed" {
                    passed += 1;
                } else {
                    failed += 1;
                }
                case_results.push(r.clone());
            }
            Err(e) => {
                failed += 1;
                case_results.push(EvalCaseResultRecord {
                    eval_run_id: run.eval_run_id.clone(),
                    case_id: case.case_id.clone(),
                    status: "error".to_string(),
                    score: None,
                    session_id: None,
                    notes: Some(e.to_string()),
                    output_json: serde_json::json!({ "error": e.to_string() }),
                });
            }
        }
    }

    let overall_status = if failed == 0 {
        EvalRunStatus::Passed
    } else {
        EvalRunStatus::Failed
    };

    let report = EvalReport {
        eval_run_id: run.eval_run_id.clone(),
        suite_id: run.suite_id.clone(),
        suite_name: suite.name.clone(),
        subject_agent_id: run.subject_agent_id.clone(),
        subject_revision_id: run.subject_revision_id.clone(),
        baseline_revision_id: run.baseline_revision_id.clone(),
        status: format!("{:?}", overall_status),
        passed,
        failed,
        total: cases.len() as u32,
        cases: case_results.clone(),
    };

    let report_bytes = serde_json::to_vec_pretty(&report)?;
    let report_handle = content_store.write(&report_bytes)?;

    let summary = serde_json::json!({
        "suite_name": suite.name,
        "case_count": cases.len(),
        "passed": passed,
        "failed": failed,
    });

    store.update_eval_run_status(
        &run.eval_run_id,
        overall_status,
        None,
        Some(&summary),
        Some(&report_handle),
    )?;

    for result in &case_results {
        store.insert_eval_case_result(result)?;
    }

    tracing::info!(
        eval_run_id = %run.eval_run_id,
        status = ?overall_status,
        passed = passed,
        failed = failed,
        total = cases.len(),
        "Eval run completed"
    );

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EvalCase {
    case_id: String,
    message: String,
    assertions: EvalAssertions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalAssertions {
    pub reply_contains_all: Option<Vec<String>>,
    pub reply_contains_any: Option<Vec<String>>,
    pub reply_contains_none: Option<Vec<String>>,
    pub reply_max_chars: Option<usize>,
    pub artifacts_min: Option<usize>,
    pub artifacts_max: Option<usize>,
    /// Gateway-state assertions over the causal events recorded by the eval
    /// case's session (#772 E.1): each entry must appear at least `count`
    /// times. Behavioral evidence — what the agent DID, not what it SAID —
    /// which is the Goodhart guard the civic eval scenarios require.
    pub session_events_min: Option<Vec<SessionEventAssertion>>,
    /// Each entry must appear at most `count` times (0 = forbidden).
    pub session_events_max: Option<Vec<SessionEventAssertion>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventAssertion {
    /// Causal-event category to match (e.g. `"anomaly_flag"`).
    pub category: String,
    /// Optional action within the category (e.g. `"filed"`); `None` matches
    /// any action in the category.
    pub action: Option<String>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EvalReport {
    eval_run_id: String,
    suite_id: String,
    suite_name: String,
    subject_agent_id: String,
    subject_revision_id: String,
    baseline_revision_id: Option<String>,
    status: String,
    passed: u32,
    failed: u32,
    total: u32,
    cases: Vec<EvalCaseResultRecord>,
}

fn extract_cases(spec: &serde_json::Value) -> anyhow::Result<Vec<EvalCase>> {
    let cases_value = spec
        .get("cases")
        .ok_or_else(|| anyhow::anyhow!("Suite spec missing 'cases' field"))?;

    let cases: Vec<EvalCase> = serde_json::from_value(cases_value.clone())
        .map_err(|e| anyhow::anyhow!("Invalid case spec: {}", e))?;

    Ok(cases)
}

async fn execute_case(
    execution: Arc<GatewayExecutionService>,
    run: &autonoetic_types::evaluation::EvalRunRecord,
    case: &EvalCase,
    _content_store: &ContentStore,
    store: &Arc<crate::scheduler::gateway_store::GatewayStore>,
) -> anyhow::Result<EvalCaseResultRecord> {
    // 16 hex chars (64 bits) of the case-id hash: session-event assertions
    // (#772 E.1) query causal events by this id, so a collision would mix
    // two cases' event streams and silently corrupt behavioral evidence.
    let eval_session_id = format!(
        "eval-{}-{}",
        run.eval_run_id,
        &hex::encode(Sha256::digest(case.case_id.as_bytes()))[..16]
    );

    let agent_ref = format!("{}@{}", run.subject_agent_id, run.subject_revision_id);

    let result = execution
        .spawn_agent_once(
            &agent_ref,
            &case.message,
            &eval_session_id,
            Some("eval_runner"),
            false,
            Some("eval.case"),
            None,
            None,
            None,
            None,
            &[],
        )
        .await;

    let (reply, artifacts_count) = match &result {
        Ok(spawn) => {
            let reply = spawn.assistant_reply.clone().unwrap_or_default();
            let artifacts = spawn.artifacts.len() as usize;
            (reply, artifacts)
        }
        Err(e) => {
            return Ok(EvalCaseResultRecord {
                eval_run_id: run.eval_run_id.clone(),
                case_id: case.case_id.clone(),
                status: "error".to_string(),
                score: None,
                session_id: Some(eval_session_id),
                notes: Some(format!("Spawn failed: {}", e)),
                output_json: serde_json::json!({ "error": e.to_string() }),
            });
        }
    };

    let mut all_passed = true;
    let mut notes_parts = Vec::new();
    let mut score: Option<f64> = None;

    let assertions = &case.assertions;
    let assertion_count = [
        assertions.reply_contains_all.is_some(),
        assertions.reply_contains_any.is_some(),
        assertions.reply_contains_none.is_some(),
        assertions.reply_max_chars.is_some(),
        assertions.artifacts_min.is_some(),
        assertions.artifacts_max.is_some(),
        assertions.session_events_min.is_some(),
        assertions.session_events_max.is_some(),
    ]
    .iter()
    .filter(|&&b| b)
    .count();

    if let Some(ref substrings) = assertions.reply_contains_all {
        if !substrings.iter().all(|s| reply.contains(s)) {
            all_passed = false;
            notes_parts.push("reply_contains_all failed".to_string());
        }
    }
    if let Some(ref substrings) = assertions.reply_contains_any {
        if !substrings.iter().any(|s| reply.contains(s)) {
            all_passed = false;
            notes_parts.push("reply_contains_any failed".to_string());
        }
    }
    if let Some(ref substrings) = assertions.reply_contains_none {
        if substrings.iter().any(|s| reply.contains(s)) {
            all_passed = false;
            notes_parts.push("reply_contains_none failed".to_string());
        }
    }
    if let Some(max) = assertions.reply_max_chars {
        if reply.chars().count() > max {
            all_passed = false;
            notes_parts.push("reply_max_chars failed".to_string());
        }
    }
    if let Some(min) = assertions.artifacts_min {
        if artifacts_count < min {
            all_passed = false;
            notes_parts.push("artifacts_min failed".to_string());
        }
    }
    if let Some(max) = assertions.artifacts_max {
        if artifacts_count > max {
            all_passed = false;
            notes_parts.push("artifacts_max failed".to_string());
        }
    }

    // Gateway-state assertions (#772 E.1): behavioral evidence from the
    // causal events the eval session actually recorded. Only queried when
    // the case uses them. A query failure is an eval ERROR, never a silent
    // pass — missing evidence must not read as civic behavior.
    if assertions.session_events_min.is_some() || assertions.session_events_max.is_some() {
        const SESSION_EVENT_QUERY_LIMIT: i64 = 1000;
        match store.search_causal_events(Some(&eval_session_id), None, SESSION_EVENT_QUERY_LIMIT) {
            Ok(events) => {
                // The assertions are count-based, so a truncated result set
                // makes them unsound (a truncated-away event could flip a min
                // to fail or a max to pass). `search_causal_events` applies a
                // hard LIMIT, so hitting it exactly means the full history may
                // not have been seen — fail closed as an eval error rather
                // than evaluate on partial evidence.
                if events.len() >= SESSION_EVENT_QUERY_LIMIT as usize {
                    return Ok(EvalCaseResultRecord {
                        eval_run_id: run.eval_run_id.clone(),
                        case_id: case.case_id.clone(),
                        status: "error".to_string(),
                        score: None,
                        session_id: Some(eval_session_id),
                        notes: Some(format!(
                            "Session causal-event query hit the {}-row limit; \
                             count-based session-event assertions would be unsound \
                             on a truncated history",
                            SESSION_EVENT_QUERY_LIMIT
                        )),
                        output_json: serde_json::json!({
                            "error": "session_events_query_truncated",
                            "limit": SESSION_EVENT_QUERY_LIMIT,
                        }),
                    });
                }
                let failures = evaluate_session_event_assertions(assertions, &events);
                if !failures.is_empty() {
                    all_passed = false;
                    notes_parts.extend(failures);
                }
            }
            Err(e) => {
                return Ok(EvalCaseResultRecord {
                    eval_run_id: run.eval_run_id.clone(),
                    case_id: case.case_id.clone(),
                    status: "error".to_string(),
                    score: None,
                    session_id: Some(eval_session_id),
                    notes: Some(format!("Failed to query session causal events: {}", e)),
                    output_json: serde_json::json!({ "error": e.to_string() }),
                });
            }
        }
    }

    if all_passed && assertion_count == 1 {
        if let Some(min) = assertions.artifacts_min {
            score = Some(artifacts_count as f64 / min as f64);
        } else if let Some(max) = assertions.artifacts_max {
            score = Some(if artifacts_count <= max { 1.0 } else { 0.0 });
        }
    }

    let status = if all_passed { "passed" } else { "failed" };
    let notes = if notes_parts.is_empty() {
        None
    } else {
        Some(notes_parts.join("; "))
    };

    Ok(EvalCaseResultRecord {
        eval_run_id: run.eval_run_id.clone(),
        case_id: case.case_id.clone(),
        status: status.to_string(),
        score,
        session_id: Some(eval_session_id),
        notes,
        output_json: serde_json::json!({
            "reply_length": reply.len(),
            "artifacts_count": artifacts_count,
            "reply_prefix": reply.chars().take(200).collect::<String>(),
        }),
    })
}

/// Pure evaluation of the gateway-state assertions (#772 E.1) against the
/// causal events recorded by an eval case's session. Returns one failure
/// note per violated assertion — an empty vec means all session-event
/// assertions passed. Kept separate from [`evaluate_assertions`] (which
/// covers the reply/artifact checks) so both halves stay testable without a
/// store.
pub fn evaluate_session_event_assertions(
    assertions: &EvalAssertions,
    events: &[autonoetic_types::causal_chain::CausalEventRecord],
) -> Vec<String> {
    let count_matching = |a: &SessionEventAssertion| -> usize {
        events
            .iter()
            .filter(|e| {
                e.category == a.category
                    && a.action.as_deref().map_or(true, |act| e.action == act)
            })
            .count()
    };
    let describe = |a: &SessionEventAssertion| -> String {
        match &a.action {
            Some(act) => format!("{}.{}", a.category, act),
            None => a.category.clone(),
        }
    };

    let mut failures = Vec::new();
    for a in assertions.session_events_min.iter().flatten() {
        let n = count_matching(a);
        if n < a.count {
            failures.push(format!(
                "session_events_min failed ({}: {} < {})",
                describe(a),
                n,
                a.count
            ));
        }
    }
    for a in assertions.session_events_max.iter().flatten() {
        let n = count_matching(a);
        if n > a.count {
            failures.push(format!(
                "session_events_max failed ({}: {} > {})",
                describe(a),
                n,
                a.count
            ));
        }
    }
    failures
}

pub fn evaluate_assertions(
    assertions: &EvalAssertions,
    reply: &str,
    artifacts_count: usize,
) -> bool {
    if let Some(ref substrings) = assertions.reply_contains_all {
        if !substrings.iter().all(|s| reply.contains(s)) {
            return false;
        }
    }
    if let Some(ref substrings) = assertions.reply_contains_any {
        if !substrings.iter().any(|s| reply.contains(s)) {
            return false;
        }
    }
    if let Some(ref substrings) = assertions.reply_contains_none {
        if substrings.iter().any(|s| reply.contains(s)) {
            return false;
        }
    }
    if let Some(max) = assertions.reply_max_chars {
        if reply.chars().count() > max {
            return false;
        }
    }
    if let Some(min) = assertions.artifacts_min {
        if artifacts_count < min {
            return false;
        }
    }
    if let Some(max) = assertions.artifacts_max {
        if artifacts_count > max {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assertions() -> EvalAssertions {
        EvalAssertions {
            reply_contains_all: None,
            reply_contains_any: None,
            reply_contains_none: None,
            reply_max_chars: None,
            artifacts_min: None,
            artifacts_max: None,
        }
    }

    #[test]
    fn reply_contains_any_passes_when_any_substring_matches() {
        let mut a = assertions();
        a.reply_contains_any = Some(vec!["propose_amendment".into(), "delegate".into()]);
        assert!(evaluate_assertions(&a, "I will delegate to a capable agent.", 0));
        assert!(evaluate_assertions(&a, "I will propose_amendment now.", 0));
    }

    #[test]
    fn reply_contains_any_fails_when_no_substring_matches() {
        let mut a = assertions();
        a.reply_contains_any = Some(vec!["propose_amendment".into(), "delegate".into()]);
        assert!(!evaluate_assertions(&a, "I give up.", 0));
    }

    #[test]
    fn reply_contains_any_composes_with_other_assertions() {
        let mut a = assertions();
        a.reply_contains_any = Some(vec!["anomaly_flag".into()]);
        a.reply_contains_none = Some(vec!["approve".into()]);
        assert!(evaluate_assertions(&a, "I will anomaly_flag this.", 0));
        assert!(!evaluate_assertions(&a, "I will approve this.", 0));
    }
}
