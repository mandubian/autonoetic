use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(EvalSuitePublishTool));
    registry.register(Box::new(EvalRunTool));
    registry.register(Box::new(EvalCompareTool));
    registry.register(Box::new(EvalReportTool));
}

#[derive(Debug, Deserialize, Serialize)]
struct EvalSuitePublishArgs {
    name: String,
    description: String,
    spec: EvalSuiteSpec,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EvalSuiteSpec {
    pub cases: Vec<EvalSuiteCaseSpec>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EvalSuiteCaseSpec {
    pub case_id: String,
    pub message: String,
    pub assertions: serde_json::Value,
}

pub struct EvalSuitePublishTool;

impl NativeTool for EvalSuitePublishTool {
    fn name(&self) -> &'static str {
        "eval.suite.publish"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Evaluation { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Publish an evaluation suite defining test cases for agent validation."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Display name for the eval suite" },
                    "description": { "type": "string", "description": "Short description of what this suite tests" },
                    "spec": {
                        "type": "object",
                        "description": "Suite specification containing test cases",
                        "properties": {
                            "cases": {
                                "type": "array",
                                "description": "Array of test case definitions. Each case has: case_id, message, assertions",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "case_id": { "type": "string", "description": "Stable case identifier within the suite" },
                                        "message": { "type": "string", "description": "Input message to send to the agent" },
                                        "assertions": { "type": "object", "description": "Assertion object with keys like reply_max_chars, reply_contains_all, etc." }
                                    },
                                    "required": ["case_id", "message", "assertions"]
                                }
                            }
                        },
                        "required": ["cases"]
                    }
                },
                "required": ["name", "description", "spec"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: EvalSuitePublishArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };

        if !policy.can_evaluate_suite_publish(&args.name) {
            return Err(anyhow::anyhow!(
                "Permission Denied: agent '{}' lacks 'Evaluation' capability to publish suite '{}'",
                manifest.agent.id,
                args.name
            ));
        }

        validate_suite_spec(&args.spec)?;

        let suite_id = autonoetic_types::id_format::mint_hashed_prefixed_id(
            "suite-",
            &format!("{}-{}", args.name, chrono::Utc::now().to_rfc3339()),
        );
        let now = chrono::Utc::now().to_rfc3339();

        let spec_json = serde_json::to_value(&args.spec)?;

        let suite = autonoetic_types::evaluation::EvalSuiteRecord {
            suite_id: suite_id.clone(),
            name: args.name.clone(),
            description: args.description.clone(),
            spec_json,
            created_at: now.clone(),
            created_by_type: "agent".to_string(),
            created_by_id: manifest.agent.id.clone(),
            origin_node_id: "gateway".to_string(),
        };

        gateway_store.insert_eval_suite(&suite)?;

        Ok(serde_json::json!({
            "ok": true,
            "status": "published",
            "suite_id": suite_id,
            "name": args.name,
            "case_count": args.spec.cases.len(),
        })
        .to_string())
    }
}

pub fn validate_suite_spec(spec: &EvalSuiteSpec) -> anyhow::Result<()> {
    anyhow::ensure!(!spec.cases.is_empty(), "Suite must have at least one case");

    let mut seen_ids = std::collections::HashSet::new();
    for case in &spec.cases {
        anyhow::ensure!(!case.case_id.trim().is_empty(), "case_id must not be empty");
        anyhow::ensure!(
            seen_ids.insert(case.case_id.clone()),
            "Duplicate case_id: '{}'",
            case.case_id
        );
        anyhow::ensure!(
            !case.message.trim().is_empty(),
            "case '{}' message must not be empty",
            case.case_id
        );

        let assertions = &case.assertions;
        let obj = assertions.as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "case '{}' assertions must be an object, got {}",
                case.case_id,
                assertions
            )
        })?;

        let valid_keys = [
            "reply_contains_all",
            "reply_contains_none",
            "reply_max_chars",
            "artifacts_min",
            "artifacts_max",
        ];
        let mut has_assertion = false;
        for key in obj.keys() {
            anyhow::ensure!(
                valid_keys.contains(&key.as_str()),
                "case '{}' has unknown assertion type '{}'; valid types: {:?}",
                case.case_id,
                key,
                valid_keys
            );
            has_assertion = true;
        }
        anyhow::ensure!(
            has_assertion,
            "case '{}' must have at least one assertion",
            case.case_id
        );

        if let Some(v) = obj.get("reply_contains_all") {
            let arr: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| {
                anyhow::anyhow!(
                    "case '{}' reply_contains_all must be an array of strings",
                    case.case_id
                )
            })?;
            anyhow::ensure!(
                !arr.is_empty(),
                "case '{}' reply_contains_all must have at least one substring",
                case.case_id
            );
        }
        if let Some(v) = obj.get("reply_contains_none") {
            let arr: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| {
                anyhow::anyhow!(
                    "case '{}' reply_contains_none must be an array of strings",
                    case.case_id
                )
            })?;
            anyhow::ensure!(
                !arr.is_empty(),
                "case '{}' reply_contains_none must have at least one substring",
                case.case_id
            );
        }
        if let Some(v) = obj.get("reply_max_chars") {
            let _: u64 = serde_json::from_value(v.clone()).map_err(|_| {
                anyhow::anyhow!("case '{}' reply_max_chars must be a number", case.case_id)
            })?;
        }
        if let Some(v) = obj.get("artifacts_min") {
            let _: u64 = serde_json::from_value(v.clone()).map_err(|_| {
                anyhow::anyhow!("case '{}' artifacts_min must be a number", case.case_id)
            })?;
        }
        if let Some(v) = obj.get("artifacts_max") {
            let _: u64 = serde_json::from_value(v.clone()).map_err(|_| {
                anyhow::anyhow!("case '{}' artifacts_max must be a number", case.case_id)
            })?;
        }
    }

    Ok(())
}

fn enqueue_eval_run(
    gateway_store: &crate::scheduler::gateway_store::GatewayStore,
    suite: &autonoetic_types::evaluation::EvalSuiteRecord,
    suite_id: &str,
    subject_agent_id: &str,
    subject_revision_id: &str,
    baseline_revision_id: Option<String>,
    origin_node_id: &str,
) -> anyhow::Result<autonoetic_types::evaluation::EvalRunRecord> {
    let eval_run_id = autonoetic_types::id_format::mint_hashed_prefixed_id(
        "eval-",
        &format!(
            "{}-{}-{}",
            suite_id,
            subject_revision_id,
            chrono::Utc::now().to_rfc3339()
        ),
    );
    let now = chrono::Utc::now().to_rfc3339();
    let run = autonoetic_types::evaluation::EvalRunRecord {
        eval_run_id,
        suite_id: suite_id.to_string(),
        subject_agent_id: subject_agent_id.to_string(),
        subject_revision_id: subject_revision_id.to_string(),
        baseline_revision_id,
        status: autonoetic_types::evaluation::EvalRunStatus::Queued,
        queued_at: now,
        started_at: None,
        completed_at: None,
        summary_json: serde_json::json!({
            "suite_name": suite.name,
            "case_count": 0,
            "passed": 0,
            "failed": 0,
        }),
        report_handle: None,
        origin_node_id: origin_node_id.to_string(),
    };
    gateway_store.insert_eval_run(&run)?;
    Ok(run)
}

#[derive(Debug, Deserialize)]
struct EvalRunArgs {
    agent_ref: String,
    suite_id: String,
    baseline_ref: Option<String>,
}

pub struct EvalRunTool;

impl NativeTool for EvalRunTool {
    fn name(&self) -> &'static str {
        "eval.run"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Evaluation { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Execute an evaluation suite against an agent revision. Runs each case and records results.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_ref": { "type": "string", "description": "Agent reference in format 'agent_id@rev_sha256:<64 hex>' or 'agent_id@rev_<short_id>'" },
                    "suite_id": { "type": "string", "description": "ID of the eval suite to run" },
                    "baseline_ref": { "type": "string", "description": "Optional: baseline agent reference for comparison" }
                },
                "required": ["agent_ref", "suite_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: EvalRunArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };

        let config = config.ok_or_else(|| anyhow::anyhow!("GatewayConfig is required"))?;
        let repo = crate::agent::repository::AgentRepository::from_config(config);

        let (agent_ref, _rev) =
            repo.resolve_agent(&args.agent_ref, Some(gateway_store.as_ref()))?;

        if !policy.can_evaluate_suite(&args.suite_id, &agent_ref.agent_id) {
            return Err(anyhow::anyhow!(
                "Permission Denied: agent '{}' lacks 'Evaluation' capability to run suite '{}' against agent '{}'",
                manifest.agent.id, args.suite_id, agent_ref.agent_id
            ));
        }

        let suite = gateway_store
            .get_eval_suite(&args.suite_id)?
            .ok_or_else(|| anyhow::anyhow!("Eval suite '{}' not found", args.suite_id))?;

        let baseline_revision_id = if let Some(ref baseline) = args.baseline_ref {
            let (_, base_rev) = repo.resolve_agent(baseline, Some(gateway_store.as_ref()))?;
            Some(base_rev.revision_id)
        } else {
            None
        };

        let run = enqueue_eval_run(
            gateway_store.as_ref(),
            &suite,
            &args.suite_id,
            &agent_ref.agent_id,
            &agent_ref.revision_id,
            baseline_revision_id,
            "gateway",
        )?;

        Ok(serde_json::json!({
            "ok": true,
            "status": "queued",
            "eval_run_id": run.eval_run_id,
            "suite_id": args.suite_id,
            "subject_agent_id": agent_ref.agent_id,
            "subject_revision_id": agent_ref.revision_id,
            "baseline_revision_id": run.baseline_revision_id,
        })
        .to_string())
    }
}

#[derive(Debug, Deserialize)]
struct EvalCompareArgs {
    suite_id: String,
    baseline_ref: String,
    candidate_ref: String,
    #[serde(default)]
    queue_if_missing: Option<bool>,
}

pub struct EvalCompareTool;

impl NativeTool for EvalCompareTool {
    fn name(&self) -> &'static str {
        "eval.compare"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Evaluation { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Compare baseline and candidate revisions on the same eval suite. Reuses completed runs when available and queues missing runs.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "suite_id": { "type": "string" },
                    "baseline_ref": { "type": "string" },
                    "candidate_ref": { "type": "string" },
                    "queue_if_missing": { "type": "boolean", "description": "Default true. Queue missing baseline/candidate runs if no completed run exists yet." }
                },
                "required": ["suite_id", "baseline_ref", "candidate_ref"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: EvalCompareArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;
        let queue_if_missing = args.queue_if_missing.unwrap_or(true);

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };
        let config = config.ok_or_else(|| anyhow::anyhow!("GatewayConfig is required"))?;
        let repo = crate::agent::repository::AgentRepository::from_config(config);

        let (baseline_ref, baseline_rev) =
            repo.resolve_agent(&args.baseline_ref, Some(gateway_store.as_ref()))?;
        let (candidate_ref, candidate_rev) =
            repo.resolve_agent(&args.candidate_ref, Some(gateway_store.as_ref()))?;
        anyhow::ensure!(
            baseline_ref.agent_id == candidate_ref.agent_id,
            "baseline_ref and candidate_ref must resolve to the same logical agent (got '{}' and '{}')",
            baseline_ref.agent_id,
            candidate_ref.agent_id
        );
        anyhow::ensure!(
            policy.can_evaluate_suite(&args.suite_id, &candidate_ref.agent_id),
            "Permission Denied: agent '{}' lacks Evaluation capability to compare suite '{}' for '{}'",
            manifest.agent.id,
            args.suite_id,
            candidate_ref.agent_id
        );

        let suite = gateway_store
            .get_eval_suite(&args.suite_id)?
            .ok_or_else(|| anyhow::anyhow!("Eval suite '{}' not found", args.suite_id))?;

        let mut baseline_run = gateway_store
            .find_latest_completed_eval_run(&args.suite_id, &baseline_rev.revision_id)?;
        let mut candidate_run = gateway_store
            .find_latest_completed_eval_run(&args.suite_id, &candidate_rev.revision_id)?;
        let mut queued: Vec<String> = Vec::new();

        if baseline_run.is_none() && queue_if_missing {
            let run = enqueue_eval_run(
                gateway_store.as_ref(),
                &suite,
                &args.suite_id,
                &baseline_ref.agent_id,
                &baseline_ref.revision_id,
                None,
                "gateway",
            )?;
            queued.push(run.eval_run_id);
        }
        if candidate_run.is_none() && queue_if_missing {
            let run = enqueue_eval_run(
                gateway_store.as_ref(),
                &suite,
                &args.suite_id,
                &candidate_ref.agent_id,
                &candidate_ref.revision_id,
                Some(baseline_ref.revision_id.clone()),
                "gateway",
            )?;
            queued.push(run.eval_run_id);
        }

        if baseline_run.is_none() {
            baseline_run = gateway_store
                .find_latest_completed_eval_run(&args.suite_id, &baseline_rev.revision_id)?;
        }
        if candidate_run.is_none() {
            candidate_run = gateway_store
                .find_latest_completed_eval_run(&args.suite_id, &candidate_rev.revision_id)?;
        }

        if baseline_run.is_none() || candidate_run.is_none() {
            return Ok(serde_json::json!({
                "ok": true,
                "status": "queued",
                "suite_id": args.suite_id,
                "baseline_ref": baseline_ref.to_string(),
                "candidate_ref": candidate_ref.to_string(),
                "queued_eval_run_ids": queued,
                "message": "Queued missing eval runs. Call eval.compare again after both runs complete to get the comparison report."
            }).to_string());
        }

        let baseline_run = baseline_run.expect("checked above");
        let candidate_run = candidate_run.expect("checked above");
        let baseline_cases = gateway_store.list_eval_case_results(&baseline_run.eval_run_id)?;
        let candidate_cases = gateway_store.list_eval_case_results(&candidate_run.eval_run_id)?;

        let mut baseline_map: HashMap<String, autonoetic_types::evaluation::EvalCaseResultRecord> =
            HashMap::new();
        for c in baseline_cases {
            baseline_map.insert(c.case_id.clone(), c);
        }
        let mut candidate_map: HashMap<String, autonoetic_types::evaluation::EvalCaseResultRecord> =
            HashMap::new();
        for c in candidate_cases {
            candidate_map.insert(c.case_id.clone(), c);
        }

        let mut case_ids = BTreeSet::new();
        case_ids.extend(baseline_map.keys().cloned());
        case_ids.extend(candidate_map.keys().cloned());

        let mut regressions: Vec<String> = Vec::new();
        let mut improvements: Vec<String> = Vec::new();
        let mut changed_cases: Vec<serde_json::Value> = Vec::new();

        for case_id in case_ids {
            let base = baseline_map.get(&case_id);
            let cand = candidate_map.get(&case_id);
            let base_status = base.map(|c| c.status.as_str()).unwrap_or("missing");
            let cand_status = cand.map(|c| c.status.as_str()).unwrap_or("missing");
            if base_status == "passed" && cand_status != "passed" {
                regressions.push(case_id.clone());
            }
            if base_status != "passed" && cand_status == "passed" {
                improvements.push(case_id.clone());
            }
            if base_status != cand_status {
                changed_cases.push(serde_json::json!({
                    "case_id": case_id,
                    "baseline_status": base_status,
                    "candidate_status": cand_status,
                    "baseline_score": base.and_then(|c| c.score),
                    "candidate_score": cand.and_then(|c| c.score),
                }));
            }
        }

        let baseline_passed = baseline_map
            .values()
            .filter(|c| c.status == "passed")
            .count();
        let candidate_passed = candidate_map
            .values()
            .filter(|c| c.status == "passed")
            .count();
        let baseline_total = baseline_map.len();
        let candidate_total = candidate_map.len();

        Ok(serde_json::json!({
            "ok": true,
            "status": "completed",
            "suite_id": args.suite_id,
            "baseline_ref": baseline_ref.to_string(),
            "candidate_ref": candidate_ref.to_string(),
            "baseline_eval_run_id": baseline_run.eval_run_id,
            "candidate_eval_run_id": candidate_run.eval_run_id,
            "summary": {
                "baseline_passed": baseline_passed,
                "baseline_total": baseline_total,
                "candidate_passed": candidate_passed,
                "candidate_total": candidate_total,
                "delta_passed": candidate_passed as i64 - baseline_passed as i64,
                "regression_count": regressions.len(),
                "improvement_count": improvements.len(),
                "changed_case_count": changed_cases.len(),
            },
            "regressions": regressions,
            "improvements": improvements,
            "changed_cases": changed_cases,
        })
        .to_string())
    }
}

#[derive(Debug, Deserialize)]
struct EvalReportArgs {
    eval_run_id: String,
}

pub struct EvalReportTool;

impl NativeTool for EvalReportTool {
    fn name(&self) -> &'static str {
        "eval.report"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Evaluation { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description:
                "Get the report for a completed eval run, including case results and summary."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "eval_run_id": { "type": "string", "description": "ID of the eval run to report on" }
                },
                "required": ["eval_run_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: EvalReportArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };

        let run = gateway_store
            .get_eval_run(&args.eval_run_id)?
            .ok_or_else(|| anyhow::anyhow!("Eval run '{}' not found", args.eval_run_id))?;

        if !policy.can_evaluate_suite(&run.suite_id, &run.subject_agent_id) {
            return Err(anyhow::anyhow!(
                "Permission Denied: agent '{}' lacks 'Evaluation' capability to view report for suite '{}' against agent '{}'",
                manifest.agent.id, run.suite_id, run.subject_agent_id
            ));
        }

        let case_results = gateway_store.list_eval_case_results(&args.eval_run_id)?;

        Ok(serde_json::json!({
            "ok": true,
            "run": {
                "eval_run_id": run.eval_run_id,
                "suite_id": run.suite_id,
                "subject_agent_id": run.subject_agent_id,
                "subject_revision_id": run.subject_revision_id,
                "baseline_revision_id": run.baseline_revision_id,
                "status": format!("{:?}", run.status),
                "queued_at": run.queued_at,
                "started_at": run.started_at,
                "completed_at": run.completed_at,
                "summary": run.summary_json,
                "report_handle": run.report_handle,
            },
            "case_results": case_results,
            "case_count": case_results.len(),
        })
        .to_string())
    }
}
