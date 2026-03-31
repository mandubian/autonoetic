//! Background eval runner — processes queued eval runs.
//!
//! Polls the gateway store for queued eval runs, executes each case by spawning
//! the subject agent, evaluates assertions against the reply, and persists results.

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
        let result = execute_case(execution.clone(), &run, case, &content_store).await;

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
    pub reply_contains_none: Option<Vec<String>>,
    pub reply_max_chars: Option<usize>,
    pub artifacts_min: Option<usize>,
    pub artifacts_max: Option<usize>,
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
) -> anyhow::Result<EvalCaseResultRecord> {
    let eval_session_id = format!(
        "eval-{}-{}",
        run.eval_run_id,
        &hex::encode(Sha256::digest(case.case_id.as_bytes()))[..8]
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
        assertions.reply_contains_none.is_some(),
        assertions.reply_max_chars.is_some(),
        assertions.artifacts_min.is_some(),
        assertions.artifacts_max.is_some(),
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
