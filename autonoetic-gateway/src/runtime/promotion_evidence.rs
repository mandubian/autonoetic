//! Trace-based promotion evidence (#580 / P-2.9).

use autonoetic_types::causal_chain::ExecutionTraceRecord;
use autonoetic_types::promotion::{Finding, FindingSeverity, PromotionRecord, PromotionRole};

pub fn role_requires_execution_trace(role: &PromotionRole) -> bool {
    matches!(
        role,
        PromotionRole::Evaluator | PromotionRole::UnitTestRunner | PromotionRole::SealedEvaluator
    )
}

pub fn role_requires_execution_trace_str(role: &str) -> bool {
    matches!(role, "evaluator" | "unit_test_runner" | "sealed_evaluator")
}

pub fn trace_indicates_pass(trace: &ExecutionTraceRecord) -> bool {
    trace.exit_code == Some(0) && trace.success == 1
}

pub fn auditor_critical_veto(findings: &[Finding]) -> bool {
    findings
        .iter()
        .any(|f| matches!(f.severity, FindingSeverity::Critical))
}

/// Mechanical severity gating for roles that set `pass` explicitly (not
/// trace-derived).  Returns `true` when error/critical findings make
/// `pass=true` inadmissible — the gateway must reject or override the pass.
pub fn findings_block_explicit_pass(findings: &[Finding]) -> bool {
    findings.iter().any(|f| {
        matches!(
            f.severity,
            FindingSeverity::Error | FindingSeverity::Critical
        )
    })
}

pub fn execution_trace_id_for_role<'a>(record: &'a PromotionRecord, role: &str) -> Option<&'a str> {
    match role {
        "evaluator" => record.evaluator_execution_trace_id.as_deref(),
        "static_evaluator" => record.static_evaluator_execution_trace_id.as_deref(),
        "unit_test_runner" => record.unit_test_runner_execution_trace_id.as_deref(),
        "sealed_evaluator" => record.sealed_evaluator_execution_trace_id.as_deref(),
        _ => None,
    }
}

pub fn execution_role_passed_with_trace(
    record: &PromotionRecord,
    role: &str,
    trace: &ExecutionTraceRecord,
) -> bool {
    let Some(stored_id) = execution_trace_id_for_role(record, role) else {
        return false;
    };
    stored_id == trace.trace_id && trace_indicates_pass(trace)
}

/// Re-verify execution-role passes against durable traces at promote time (#580).
pub fn verify_stored_execution_traces(
    gateway_store: &crate::scheduler::gateway_store::GatewayStore,
    record: &PromotionRecord,
) -> anyhow::Result<Option<(String, String)>> {
    let roles: [(&str, bool, Option<&str>); 3] = [
        (
            "evaluator",
            record.evaluator_pass,
            record.evaluator_execution_trace_id.as_deref(),
        ),
        (
            "unit_test_runner",
            record.unit_test_runner_pass,
            record.unit_test_runner_execution_trace_id.as_deref(),
        ),
        (
            "sealed_evaluator",
            record.sealed_evaluator_pass,
            record.sealed_evaluator_execution_trace_id.as_deref(),
        ),
    ];
    for (role, pass, trace_id) in roles {
        if !pass {
            continue;
        }
        let Some(tid) = trace_id.filter(|s| !s.is_empty()) else {
            return Ok(Some((
                "missing_execution_evidence".to_string(),
                format!(
                    "Promotion gate: role '{}' passed for artifact '{}' but no execution_trace_id is recorded (P-2.9).",
                    role, record.artifact_id
                ),
            )));
        };
        let Some(trace) = gateway_store.get_execution_trace(tid)? else {
            return Ok(Some((
                "execution_trace_not_found".to_string(),
                format!(
                    "Promotion gate: execution trace '{}' for role '{}' not found.",
                    tid, role
                ),
            )));
        };
        if !trace_indicates_pass(&trace) {
            return Ok(Some((
                "execution_trace_failed".to_string(),
                format!(
                    "Promotion gate: execution trace '{}' for role '{}' did not succeed (exit_code={:?}).",
                    tid, role, trace.exit_code
                ),
            )));
        }
    }
    if record.auditor_pass && auditor_critical_veto(&record.auditor_findings) {
        return Ok(Some((
            "auditor_critical_veto".to_string(),
            format!(
                "Promotion gate: auditor recorded pass for artifact '{}' but critical findings remain.",
                record.artifact_id
            ),
        )));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_pass_requires_exit_zero_and_success() {
        let pass = ExecutionTraceRecord {
            trace_id: "t1".into(),
            event_id: None,
            agent_id: "a".into(),
            session_id: "s".into(),
            turn_id: None,
            timestamp: "now".into(),
            tool_name: "sandbox_exec".into(),
            command: None,
            exit_code: Some(0),
            stdout: None,
            stderr: None,
            duration_ms: 1,
            success: 1,
            error_type: None,
            error_summary: None,
            approval_required: None,
            approval_request_id: None,
            arguments: None,
            result: None,
        };
        assert!(trace_indicates_pass(&pass));

        let fail = ExecutionTraceRecord {
            exit_code: Some(1),
            success: 0,
            ..pass.clone()
        };
        assert!(!trace_indicates_pass(&fail));
    }

    // Verdict-safety invariant for the artifact_exec `ok` decoupling
    // (RFC: unit-test-runner-divergence-loop). After that change, a failing
    // test suite produces `ok: true` so the loop guard treats the run as
    // progress — which makes `infer_trace_success` record `success: 1`. The
    // promotion verdict must STILL be `fail` because the suite exited non-zero:
    // `pass` is gated on `exit_code == 0`, not on `success` alone.
    #[test]
    fn trace_with_nonzero_exit_is_fail_even_when_success_flag_set() {
        let trace = ExecutionTraceRecord {
            trace_id: "t1".into(),
            event_id: None,
            agent_id: "a".into(),
            session_id: "s".into(),
            turn_id: None,
            timestamp: "now".into(),
            tool_name: "artifact_exec".into(),
            command: None,
            exit_code: Some(1),
            stdout: None,
            stderr: None,
            duration_ms: 1,
            success: 1,
            error_type: None,
            error_summary: None,
            approval_required: None,
            approval_request_id: None,
            arguments: None,
            result: None,
        };
        assert!(!trace_indicates_pass(&trace));
    }
}
