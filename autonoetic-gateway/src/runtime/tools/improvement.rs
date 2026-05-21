use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::evaluation::enqueue_eval_run;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::tagged;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AbReplayTool));
}

#[derive(Debug, Deserialize)]
struct TaskSpec {
    message: String,
    #[serde(default)]
    case_id: Option<String>,
    #[serde(default)]
    reply_contains_all: Option<Vec<String>>,
    #[serde(default)]
    reply_max_chars: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AbReplayArgs {
    task_specs: Vec<TaskSpec>,
    agent_id: String,
    revision_a: String,
    revision_b: String,
    #[serde(default = "default_replays")]
    replays_per_variant: u32,
    #[serde(default = "default_holdout")]
    holdout_ratio: f64,
    /// Optional pre-existing suite_id — skip suite creation and use this suite directly.
    #[serde(default)]
    suite_id: Option<String>,
}

fn default_replays() -> u32 {
    5
}
fn default_holdout() -> f64 {
    0.3
}

/// Estimated max cost per session (used for cost ceiling check).
const ESTIMATED_MAX_COST_PER_SESSION: f64 = 0.05;
/// Default per-invocation budget in USD.
const DEFAULT_COST_CEILING: f64 = 1.0;

pub struct AbReplayTool;

impl NativeTool for AbReplayTool {
    fn name(&self) -> &'static str {
        "improvement.ab_replay"
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
            description: "Run A/B replay: execute a set of tasks against two agent revisions \
                          and return a statistical comparison. Queues eval runs if needed."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_specs": {
                        "type": "array",
                        "description": "Task specifications to run against both revisions",
                        "items": {
                            "type": "object",
                            "properties": {
                                "message": { "type": "string", "description": "Input message to send to the agent" },
                                "case_id": { "type": "string", "description": "Optional stable case identifier" },
                                "reply_contains_all": {
                                    "type": "array", "items": { "type": "string" },
                                    "description": "Optional: expected substrings in agent reply"
                                },
                                "reply_max_chars": {
                                    "type": "integer",
                                    "description": "Optional: max allowed reply length"
                                }
                            },
                            "required": ["message"]
                        }
                    },
                    "agent_id": { "type": "string", "description": "Agent ID to replay (e.g. planner.default)" },
                    "revision_a": { "type": "string", "description": "Baseline revision ref (agent_id@rev_sha256:...)" },
                    "revision_b": { "type": "string", "description": "Candidate revision ref" },
                    "replays_per_variant": {
                        "type": "integer", "default": 5,
                        "description": "Number of replays per variant (used for cost estimation)"
                    },
                    "holdout_ratio": {
                        "type": "number", "default": 0.3,
                        "description": "Fraction of tasks held out for cross-validation"
                    },
                    "suite_id": {
                        "type": "string",
                        "description": "Optional: use an existing eval suite instead of creating one from task_specs"
                    }
                },
                "required": ["task_specs", "agent_id", "revision_a", "revision_b"],
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
        let args: AbReplayArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };
        let config = config.ok_or_else(|| anyhow::anyhow!("GatewayConfig is required"))?;
        let repo = crate::agent::repository::AgentRepository::from_config(config);

        // Resolve both revision refs
        let (ref_a, rev_a) =
            repo.resolve_agent(&args.revision_a, Some(gateway_store.as_ref()))?;
        let (ref_b, rev_b) =
            repo.resolve_agent(&args.revision_b, Some(gateway_store.as_ref()))?;
        anyhow::ensure!(
            ref_a.agent_id == ref_b.agent_id,
            "revision_a and revision_b must resolve to the same logical agent (got '{}' and '{}')",
            ref_a.agent_id,
            ref_b.agent_id
        );
        anyhow::ensure!(
            ref_a.agent_id == args.agent_id,
            "agent_id '{}' does not match resolved agent '{}' from revision refs",
            args.agent_id,
            ref_a.agent_id
        );

        // Policy check
        let decision = policy.can_evaluate_suite("ab-replay", &args.agent_id);
        if !decision.is_allowed() {
            return Err(anyhow::Error::from(
                tagged::Tagged::permission_with_rules(
                    anyhow::anyhow!(
                        "Permission Denied: agent '{}' lacks Evaluation capability",
                        manifest.agent.id
                    ),
                    decision.enforced_rules.iter().map(|s| s.to_string()).collect(),
                )
            ));
        }

        // Cost ceiling check
        let estimated_sessions = args.task_specs.len() as f64 * args.replays_per_variant as f64 * 2.0;
        let estimated_cost = estimated_sessions * ESTIMATED_MAX_COST_PER_SESSION;
        if estimated_cost > DEFAULT_COST_CEILING {
            return Ok(serde_json::json!({
                "ok": false,
                "status": "cost_exceeded",
                "estimated_cost_usd": estimated_cost,
                "max_budget_usd": DEFAULT_COST_CEILING,
                "message": format!(
                    "Estimated cost ${:.2} exceeds max budget ${:.2}. \
                     Reduce task_specs count, replays_per_variant, or increase budget.",
                    estimated_cost, DEFAULT_COST_CEILING
                ),
            }).to_string());
        }

        // Holdout: pick a suffix of task_specs for held-out validation
        let n_tasks = args.task_specs.len();
        let n_holdout = (n_tasks as f64 * args.holdout_ratio).ceil() as usize;
        let n_train = n_tasks.saturating_sub(n_holdout);
        let holdout_start = n_train;

        // Validate that holdout doesn't cover all tasks
        if n_holdout > 0 && n_train == 0 {
            return Err(anyhow::anyhow!(
                "holdout_ratio={} leaves no training tasks ({} total). \
                 Reduce holdout_ratio or provide more tasks.",
                args.holdout_ratio, n_tasks
            ));
        }

        // Determine suite_id: use existing or create temporary suite
        let suite_id = if let Some(sid) = &args.suite_id {
            // Verify the suite exists
            gateway_store
                .get_eval_suite(sid)?
                .ok_or_else(|| anyhow::anyhow!("Eval suite '{}' not found", sid))?;
            sid.clone()
        } else {
            create_temp_eval_suite(
                gateway_store.as_ref(),
                &args.agent_id,
                &args.task_specs,
                &manifest.agent.id,
            )?
        };

        // Check for existing completed runs
        let baseline_run =
            gateway_store.find_latest_completed_eval_run(&suite_id, &rev_a.revision_id)?;
        let candidate_run =
            gateway_store.find_latest_completed_eval_run(&suite_id, &rev_b.revision_id)?;

        let mut queued_ids: Vec<String> = Vec::new();

        if baseline_run.is_none() {
            let suite = gateway_store.get_eval_suite(&suite_id)?.ok_or_else(|| {
                anyhow::anyhow!("Eval suite '{}' disappeared", suite_id)
            })?;
            let run = enqueue_eval_run(
                gateway_store.as_ref(),
                &suite,
                &suite_id,
                &args.agent_id,
                &rev_a.revision_id,
                None,
                "improvement.ab_replay",
            )?;
            queued_ids.push(run.eval_run_id);
        }

        if candidate_run.is_none() {
            let suite = gateway_store.get_eval_suite(&suite_id)?.ok_or_else(|| {
                anyhow::anyhow!("Eval suite '{}' disappeared", suite_id)
            })?;
            let run = enqueue_eval_run(
                gateway_store.as_ref(),
                &suite,
                &suite_id,
                &args.agent_id,
                &rev_b.revision_id,
                Some(rev_a.revision_id.clone()),
                "improvement.ab_replay",
            )?;
            queued_ids.push(run.eval_run_id);
        }

        // If any runs were queued, return immediately
        if !queued_ids.is_empty() {
            return Ok(serde_json::json!({
                "ok": true,
                "status": "queued",
                "suite_id": suite_id,
                "queued_eval_run_ids": queued_ids,
                "message": "Queued eval runs for A/B replay. Call improvement.ab_replay again with the same args \
                            once the background eval runner completes the runs to get the comparison report.",
            }).to_string());
        }

        // Both runs exist and are completed → build comparison
        let baseline_run = baseline_run.expect("checked above");
        let candidate_run = candidate_run.expect("checked above");

        let baseline_cases =
            gateway_store.list_eval_case_results(&baseline_run.eval_run_id)?;
        let candidate_cases =
            gateway_store.list_eval_case_results(&candidate_run.eval_run_id)?;

        // Build per-variant maps
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

        // Status-based comparison across ALL cases
        let mut case_ids = std::collections::BTreeSet::new();
        case_ids.extend(baseline_map.keys().cloned());
        case_ids.extend(candidate_map.keys().cloned());

        let mut regressions: Vec<String> = Vec::new();
        let mut improvements: Vec<String> = Vec::new();

        for case_id in &case_ids {
            let base = baseline_map.get(case_id);
            let cand = candidate_map.get(case_id);
            let base_status = base.map(|c| c.status.as_str()).unwrap_or("missing");
            let cand_status = cand.map(|c| c.status.as_str()).unwrap_or("missing");
            if base_status == "passed" && cand_status != "passed" {
                regressions.push(case_id.clone());
            }
            if base_status != "passed" && cand_status == "passed" {
                improvements.push(case_id.clone());
            }
        }

        // Statistical comparison via eval_stats
        let stats = build_ab_stats(&baseline_map, &candidate_map, gateway_store.as_ref());

        // Holdout-specific comparison
        let holdout_cases: Vec<String> = case_ids
            .iter()
            .skip(holdout_start)
            .take(n_holdout)
            .cloned()
            .collect();

        let mut holdout_regressions: Vec<String> = Vec::new();
        for case_id in &holdout_cases {
            let base = baseline_map.get(case_id);
            let cand = candidate_map.get(case_id);
            let base_status = base.map(|c| c.status.as_str()).unwrap_or("missing");
            let cand_status = cand.map(|c| c.status.as_str()).unwrap_or("missing");
            if base_status == "passed" && cand_status != "passed" {
                holdout_regressions.push(case_id.clone());
            }
        }

        let baseline_passed = baseline_map.values().filter(|c| c.status == "passed").count();
        let candidate_passed = candidate_map.values().filter(|c| c.status == "passed").count();
        let baseline_total = baseline_map.len();
        let candidate_total = candidate_map.len();

        Ok(serde_json::json!({
            "ok": true,
            "status": "completed",
            "suite_id": suite_id,
            "agent_id": args.agent_id,
            "revision_a": ref_a.to_string(),
            "revision_b": ref_b.to_string(),
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
            },
            "holdout": {
                "total_held_out": n_holdout,
                "holdout_regressions": holdout_regressions,
            },
            "regressions": regressions,
            "improvements": improvements,
            "stats": stats,
            "cost": {
                "estimated_total_usd": estimated_cost,
                "ceiling_usd": DEFAULT_COST_CEILING,
            },
        })
        .to_string())
    }
}

/// Create a temporary eval suite from task specs and return its suite_id.
fn create_temp_eval_suite(
    gateway_store: &crate::scheduler::gateway_store::GatewayStore,
    agent_id: &str,
    task_specs: &[TaskSpec],
    caller_agent_id: &str,
) -> anyhow::Result<String> {
    let now = chrono::Utc::now().to_rfc3339();
    let suite_id = autonoetic_types::id_format::mint_hashed_prefixed_id(
        "suite-ab-",
        &format!("{}-{}", agent_id, now),
    );

    let cases: Vec<serde_json::Value> = task_specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let case_id = spec
                .case_id
                .clone()
                .unwrap_or_else(|| format!("task-{}", i));
            let mut assertions = serde_json::json!({});
            if let Some(ref contains) = spec.reply_contains_all {
                assertions["reply_contains_all"] = serde_json::json!(contains);
            }
            if let Some(max_chars) = spec.reply_max_chars {
                assertions["reply_max_chars"] = serde_json::json!(max_chars);
            }
            serde_json::json!({
                "case_id": case_id,
                "message": spec.message,
                "assertions": assertions,
            })
        })
        .collect();

    let spec_json = serde_json::json!({ "cases": cases });

    let suite = autonoetic_types::evaluation::EvalSuiteRecord {
        suite_id: suite_id.clone(),
        name: format!("ab-replay-{}", agent_id),
        description: format!("Temporary A/B replay suite for {}", agent_id),
        spec_json,
        created_at: now,
        created_by_type: "tool".to_string(),
        created_by_id: caller_agent_id.to_string(),
        origin_node_id: "gateway".to_string(),
        evaluated_targets: vec![agent_id.to_string()],
        author_agent_id: Some(caller_agent_id.to_string()),
        based_on_suite_id: None,
    };

    gateway_store.insert_eval_suite(&suite)?;
    Ok(suite_id)
}

/// Build VariantSamples from case results and run bootstrap CI comparison.
fn build_ab_stats(
    baseline_cases: &HashMap<String, autonoetic_types::evaluation::EvalCaseResultRecord>,
    candidate_cases: &HashMap<String, autonoetic_types::evaluation::EvalCaseResultRecord>,
    gateway_store: &crate::scheduler::gateway_store::GatewayStore,
) -> Option<serde_json::Value> {
    let (a, b) = crate::runtime::tools::evaluation::build_samples_from_case_results(
        baseline_cases,
        candidate_cases,
        gateway_store,
    )?;

    let config = crate::runtime::eval_stats::CompareConfig::default();
    match crate::runtime::eval_stats::compare(&a, &b, &config) {
        Ok(rec) => serde_json::to_value(rec).ok(),
        Err(e) => Some(serde_json::json!({"error": e})),
    }
}
