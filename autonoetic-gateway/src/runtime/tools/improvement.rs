use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::evaluation::enqueue_eval_run;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::evaluation::EvalRunStatus;
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
    #[serde(default = "default_holdout")]
    holdout_ratio: f64,
    #[serde(default)]
    suite_id: Option<String>,
}

fn default_holdout() -> f64 {
    0.3
}

/// Estimated max cost per session (used for cost ceiling check).
const ESTIMATED_MAX_COST_PER_SESSION: f64 = 0.05;
/// Default per-invocation budget in USD.
const DEFAULT_COST_CEILING: f64 = 1.0;

fn is_terminal_status(status: &EvalRunStatus) -> bool {
    matches!(
        status,
        EvalRunStatus::Passed | EvalRunStatus::Failed | EvalRunStatus::Cancelled
    )
}

pub struct AbReplayTool;

impl NativeTool for AbReplayTool {
    fn name(&self) -> &'static str {
        "improvement_ab_replay"
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
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let mut args: AbReplayArgs = serde_json::from_str(arguments_json)
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

        // P4/P5 surface-change gate. The three-state policy lives in
        // `evaluate_surface_change_policy`; this block applies its
        // verdict and threads the audit trail into the response.
        //
        // `policy_applied` is set to one of six values so consumers
        // can log/inspect what happened uniformly:
        //   * `gate_disabled`                          — restrict_to_prompt_only=false
        //   * `not_evaluated`                          — gate enabled but no gateway_dir
        //   * `no_delta`                               — surfaces match
        //   * `prompt_only_violation`                  — reject (operator not opted in)
        //   * `high_blast_radius`                      — reject (never automatable)
        //   * `capability_change_with_strict_holdout`  — allow + maybe coerce holdout
        //
        // See docs/design/self-improvement-loop-validation.md §8 (P5).
        let mut policy_applied = if config.improve.restrict_to_prompt_only {
            // Will be overwritten once we actually evaluate.
            "not_evaluated".to_string()
        } else {
            "gate_disabled".to_string()
        };
        let mut holdout_coerced_from: Option<f64> = None;
        if config.improve.restrict_to_prompt_only {
            // Validate the high-blast list against the canonical set
            // from autonoetic-types. Typos here would silently disable
            // detection for that kind, so we surface them loudly.
            warn_unknown_high_blast_kinds(&config.improve.high_blast_radius_capability_kinds);

            // The revisions live under `gateway_dir/revisions/agents/<agent>/<rev>/`.
            // Use the `gateway_dir` parameter the runtime threads in
            // (production: JSON-RPC dispatch supplies it). When absent,
            // we cannot enforce the surface check — log a warning and
            // skip, rather than silently rejecting all comparisons.
            match gateway_dir {
                Some(gw_dir) => {
                    let policy = evaluate_surface_change_policy(
                        &repo,
                        gw_dir,
                        &args.agent_id,
                        &rev_a.revision_id,
                        &rev_b.revision_id,
                        &config.improve,
                    )?;
                    match policy {
                        SurfaceChangePolicy::Allow => {
                            policy_applied = "no_delta".to_string();
                        }
                        SurfaceChangePolicy::Reject { reason, classification } => {
                            // policy_applied mirrors `classification` so all
                            // responses share a single audit field. The
                            // legacy `classification` field stays for
                            // backward compatibility with operator
                            // tooling already keyed on it.
                            return Ok(serde_json::json!({
                                "ok": false,
                                "status": "surface_drift_rejected",
                                "agent_id": args.agent_id,
                                "revision_a": ref_a.to_string(),
                                "revision_b": ref_b.to_string(),
                                "reason": reason,
                                "policy_applied": classification.clone(),
                                "classification": classification,
                                "guardrail": "improve.restrict_to_prompt_only",
                                "message":
                                    "Refused: candidate revision's capability/tool-tier surface differs \
                                     from baseline. To allow this comparison, either \
                                     (1) set `improve.allow_capability_changes: true` and ensure the change \
                                     is not high-blast-radius, or \
                                     (2) promote the revision manually through the R++2 capability-delta gate.".to_string(),
                            }).to_string());
                        }
                        SurfaceChangePolicy::AllowWithStrictHoldout { reason } => {
                            policy_applied = "capability_change_with_strict_holdout".to_string();
                            // Clamp the configured min into a sane range
                            // before comparing — guards against a
                            // misconfigured `capability_change_min_holdout`
                            // value (e.g. >1, NaN) breaking the holdout
                            // math downstream.
                            let min_holdout =
                                clamp_holdout_ratio(config.improve.capability_change_min_holdout);
                            if args.holdout_ratio < min_holdout {
                                holdout_coerced_from = Some(args.holdout_ratio);
                                args.holdout_ratio = min_holdout;
                                tracing::info!(
                                    target: "improvement",
                                    agent_id = %args.agent_id,
                                    requested_holdout = holdout_coerced_from.unwrap_or(0.0),
                                    coerced_to = min_holdout,
                                    reason = %reason,
                                    "P5: holdout coerced up for capability-change comparison"
                                );
                            }
                        }
                    }
                }
                None => {
                    // policy_applied stays "not_evaluated" — set above.
                    tracing::warn!(
                        target: "improvement",
                        agent_id = %args.agent_id,
                        "improve.restrict_to_prompt_only is enabled but no gateway_dir \
                         was supplied to the tool — surface-change policy not evaluated. \
                         Production callers should pass gateway_dir."
                    );
                }
            }
        }

        // Determine suite_id and case count for cost estimation
        let (suite_id, case_count) = if let Some(sid) = &args.suite_id {
            let suite = gateway_store
                .get_eval_suite(sid)?
                .ok_or_else(|| anyhow::anyhow!("Eval suite '{}' not found", sid))?;
            let count = suite.spec_json["cases"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            (sid.clone(), count)
        } else {
            let sid = create_temp_eval_suite(
                gateway_store.as_ref(),
                &args.agent_id,
                &args.task_specs,
                &manifest.agent.id,
            )?;
            (sid, args.task_specs.len())
        };

        // Policy check with real suite_id
        let decision = policy.can_evaluate_suite(&suite_id, &args.agent_id);
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

        // Cost ceiling check: 1 run per revision × 2 revisions
        let estimated_sessions = case_count as f64 * 2.0;
        let estimated_cost = estimated_sessions * ESTIMATED_MAX_COST_PER_SESSION;
        if estimated_cost > DEFAULT_COST_CEILING {
            return Ok(serde_json::json!({
                "ok": false,
                "status": "cost_exceeded",
                "estimated_cost_usd": estimated_cost,
                "max_budget_usd": DEFAULT_COST_CEILING,
                "case_count": case_count,
                "message": format!(
                    "Estimated cost ${:.2} exceeds max budget ${:.2} ({} cases × 2 runs × ${:.2}/session). \
                     Reduce task count or increase budget.",
                    estimated_cost, DEFAULT_COST_CEILING, case_count, ESTIMATED_MAX_COST_PER_SESSION
                ),
            }).to_string());
        }

        // Derive all case_ids from task_specs in order (for stable holdout)
        let all_case_ids: Vec<String> = args
            .task_specs
            .iter()
            .enumerate()
            .map(|(i, spec)| {
                spec.case_id
                    .clone()
                    .unwrap_or_else(|| format!("task-{}", i))
            })
            .collect();

        // Holdout: pick a suffix of task_specs for held-out validation
        let n_tasks = all_case_ids.len();
        let n_holdout = (n_tasks as f64 * args.holdout_ratio).ceil() as usize;
        let n_train = n_tasks.saturating_sub(n_holdout);

        if n_holdout > 0 && n_train == 0 {
            return Err(anyhow::anyhow!(
                "holdout_ratio={} leaves no training tasks ({} total). \
                 Reduce holdout_ratio or provide more tasks.",
                args.holdout_ratio, n_tasks
            ));
        }

        // Check for existing runs (any status) to detect pending runs and prevent duplicate enqueues
        let baseline_existing =
            gateway_store.find_latest_eval_run(&suite_id, &rev_a.revision_id)?;
        let candidate_existing =
            gateway_store.find_latest_eval_run(&suite_id, &rev_b.revision_id)?;

        // If either has a non-terminal run, return queued without enqueuing
        let has_pending = baseline_existing
            .as_ref()
            .is_some_and(|r| !is_terminal_status(&r.status))
            || candidate_existing
                .as_ref()
                .is_some_and(|r| !is_terminal_status(&r.status));

        if has_pending {
            let pending_ids: Vec<String> = [baseline_existing.as_ref(), candidate_existing.as_ref()]
                .into_iter()
                .flatten()
                .filter(|r| !is_terminal_status(&r.status))
                .map(|r| r.eval_run_id.clone())
                .collect();

            return Ok(serde_json::json!({
                "ok": true,
                "status": "queued",
                "suite_id": suite_id,
                "policy_applied": policy_applied,
                "holdout_coerced_from": holdout_coerced_from,
                "queued_eval_run_ids": pending_ids,
                "message": "Eval runs already pending. Re-invoke with same args once complete.",
            }).to_string());
        }

        // Only completed runs are useful — enqueue missing
        let baseline_completed = baseline_existing.filter(|r| is_terminal_status(&r.status));
        let candidate_completed = candidate_existing.filter(|r| is_terminal_status(&r.status));

        let mut queued_ids: Vec<String> = Vec::new();

        if baseline_completed.is_none() {
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
                "improvement_ab_replay",
            )?;
            queued_ids.push(run.eval_run_id);
        }

        if candidate_completed.is_none() {
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
                "improvement_ab_replay",
            )?;
            queued_ids.push(run.eval_run_id);
        }

        // If any runs were queued, return immediately
        if !queued_ids.is_empty() {
            return Ok(serde_json::json!({
                "ok": true,
                "status": "queued",
                "suite_id": suite_id,
                "policy_applied": policy_applied,
                "holdout_coerced_from": holdout_coerced_from,
                "queued_eval_run_ids": queued_ids,
                "message": "Queued eval runs for A/B replay. Call improvement_ab_replay again with the same args \
                            once the background eval runner completes the runs to get the comparison report.",
            }).to_string());
        }

        // Both runs exist and are completed → build comparison
        let baseline_run = baseline_completed.expect("checked above");
        let candidate_run = candidate_completed.expect("checked above");

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

        // Status-based comparison across all case_ids (in task_specs order)
        let mut regressions: Vec<String> = Vec::new();
        let mut improvements: Vec<String> = Vec::new();

        for case_id in &all_case_ids {
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

        // Holdout-specific comparison (from task_specs ordering)
        let holdout_cases: Vec<String> = all_case_ids
            .iter()
            .skip(n_train)
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
            "policy_applied": policy_applied,
            "holdout_coerced_from": holdout_coerced_from,
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
            // Default assertion to ensure meaningful pass/fail signal
            if assertions == serde_json::json!({}) {
                assertions["reply_max_chars"] = serde_json::json!(100000);
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
        created_by_type: autonoetic_types::principal::PrincipalKind::AutonoeticAgent.tag().to_string(),
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
        Err(e) => Some(serde_json::json!({
            "ok": false,
            "error_type": "execution",
            "error": "improvement_ab_comparison_failed",
            "message": format!("{}", e),
            "repair_hint": "Check the evaluation data and retry."
        })),
    }
}

/// Three-state verdict from [`evaluate_surface_change_policy`]. The
/// caller (the gate inside `AbReplayTool::execute`) either proceeds
/// untouched, proceeds with a coerced minimum holdout, or returns a
/// reject response to the caller.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SurfaceChangePolicy {
    /// No capability or tool-tier delta. Proceed normally.
    Allow,
    /// Reject. `classification` is `"prompt_only_violation"` (operator
    /// has not opted in) or `"high_blast_radius"` (broadens
    /// sandbox/network/code-exec/credentials/scheduler/revision —
    /// never automatable). `reason` carries the concrete diff for
    /// audit.
    Reject {
        reason: String,
        classification: String,
    },
    /// Capability delta exists, operator opted in, change is not
    /// high-blast-radius. Proceed but enforce
    /// `improve.capability_change_min_holdout`. `reason` carries the
    /// diff for the audit trail.
    AllowWithStrictHoldout { reason: String },
}

/// Evaluate the surface-change policy (P4 prompt-only gate + P5
/// capability-change extension) for an A/B replay.
///
/// Compares baseline vs candidate manifests on:
/// 1. `capabilities` via
///    [`autonoetic_types::capability::compute_capability_delta`] —
///    understands parameter-level widening, not just kind add/remove
/// 2. `allowed_tool_tiers` set equality
///
/// Then classifies the verdict:
/// - **No delta** → `Allow`
/// - **Delta + `!allow_capability_changes`** → `Reject(prompt_only_violation)`
/// - **Delta + opted in + ANY add/broaden in
///   `high_blast_radius_capability_kinds`** → `Reject(high_blast_radius)`
/// - **Delta + opted in + low-blast** → `AllowWithStrictHoldout`
pub(crate) fn evaluate_surface_change_policy(
    repo: &crate::agent::repository::AgentRepository,
    gateway_dir: &std::path::Path,
    agent_id: &str,
    baseline_revision_id: &str,
    candidate_revision_id: &str,
    improve_config: &autonoetic_types::config::ImproveConfig,
) -> anyhow::Result<SurfaceChangePolicy> {
    let loaded_baseline =
        repo.load_from_revision_dir(gateway_dir, agent_id, baseline_revision_id)?;
    let loaded_candidate =
        repo.load_from_revision_dir(gateway_dir, agent_id, candidate_revision_id)?;

    let delta = autonoetic_types::capability::compute_capability_delta(
        &loaded_baseline.manifest.capabilities,
        &loaded_candidate.manifest.capabilities,
    );
    let tiers_a = tool_tier_surface(&loaded_baseline.manifest);
    let tiers_b = tool_tier_surface(&loaded_candidate.manifest);
    let tier_delta_added: Vec<String> = tiers_b.difference(&tiers_a).cloned().collect();
    let tier_delta_removed: Vec<String> = tiers_a.difference(&tiers_b).cloned().collect();
    let has_capability_delta = !delta.added.is_empty()
        || !delta.broadened.is_empty()
        || !delta.narrowed.is_empty()
        || !delta.removed.is_empty();
    let has_tier_delta = !tier_delta_added.is_empty() || !tier_delta_removed.is_empty();

    if !has_capability_delta && !has_tier_delta {
        return Ok(SurfaceChangePolicy::Allow);
    }

    let broadened_kinds: Vec<String> = delta
        .broadened
        .iter()
        .map(|b| b.capability_type.clone())
        .collect();
    let diff_reason = format!(
        "capabilities: added={:?}, broadened={:?}, narrowed={:?}, removed={:?}; \
         allowed_tool_tiers: added={:?}, removed={:?}",
        delta.added, broadened_kinds, delta.narrowed, delta.removed,
        tier_delta_added, tier_delta_removed,
    );

    if !improve_config.allow_capability_changes {
        return Ok(SurfaceChangePolicy::Reject {
            reason: diff_reason,
            classification: "prompt_only_violation".to_string(),
        });
    }

    // P5 blast-radius classifier: ADDED kinds OR BROADENED kinds in
    // the high-blast list = reject. Removed/narrowed kinds do NOT
    // count as high-blast (removing privileges is safe). Tool-tier
    // changes are not blast-classified separately — they go through
    // the strict-holdout path unless paired with a high-blast cap
    // change, which the cap check above already catches.
    let high_blast: &[String] = &improve_config.high_blast_radius_capability_kinds;
    let added_high: Vec<&str> = delta
        .added
        .iter()
        .filter(|kind| high_blast.iter().any(|h| h == *kind))
        .map(|s| s.as_str())
        .collect();
    let broadened_high: Vec<&str> = broadened_kinds
        .iter()
        .filter(|kind| high_blast.iter().any(|h| h == *kind))
        .map(|s| s.as_str())
        .collect();
    if !added_high.is_empty() || !broadened_high.is_empty() {
        return Ok(SurfaceChangePolicy::Reject {
            reason: format!(
                "{} | high-blast kinds touched: added={:?}, broadened={:?}",
                diff_reason, added_high, broadened_high
            ),
            classification: "high_blast_radius".to_string(),
        });
    }

    Ok(SurfaceChangePolicy::AllowWithStrictHoldout {
        reason: diff_reason,
    })
}

/// Clamp a holdout ratio into `[0.0, 1.0]`. `NaN` becomes `0.0`. Used
/// at the use site rather than at config load so misconfiguration is
/// neutralized at the boundary without a hard parse error (which would
/// fail the whole gateway start). Emits an `info` log when clamping
/// actually changed the value, so an operator who set 1.5 by mistake
/// gets a hint.
fn clamp_holdout_ratio(raw: f64) -> f64 {
    let clamped = if raw.is_nan() {
        0.0
    } else {
        raw.max(0.0).min(1.0)
    };
    if (clamped - raw).abs() > f64::EPSILON || raw.is_nan() {
        tracing::info!(
            target: "improvement",
            requested = raw,
            clamped = clamped,
            "improve.capability_change_min_holdout out of [0.0, 1.0] — clamped"
        );
    }
    clamped
}

/// Validate the operator-supplied high-blast list against the canonical
/// capability-kind names from `autonoetic-types`. Anything unknown
/// (typo, casing, deprecated kind) is logged at WARN level so the
/// operator notices the silent disablement. We don't reject the config
/// — a single typo shouldn't fail the gateway — but the warning is
/// loud enough to surface in any reasonable log review.
fn warn_unknown_high_blast_kinds(configured: &[String]) {
    let known: std::collections::HashSet<&str> =
        autonoetic_types::capability::all_capability_kind_names()
            .iter()
            .copied()
            .collect();
    for kind in configured {
        if !known.contains(kind.as_str()) {
            tracing::warn!(
                target: "improvement",
                unknown_kind = %kind,
                "improve.high_blast_radius_capability_kinds contains an unknown capability \
                 kind — high-blast detection will silently miss this kind. Check spelling \
                 against autonoetic_types::capability::all_capability_kind_names()."
            );
        }
    }
}

fn tool_tier_surface(manifest: &AgentManifest) -> std::collections::BTreeSet<String> {
    // ToolTier is Hash+Eq but not Ord (foreign-type constraint). We
    // convert to its `Debug`-derived slug so the comparison set is
    // ordered for deterministic diff messages.
    manifest
        .allowed_tool_tiers
        .iter()
        .map(|t| format!("{:?}", t))
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// Tests for the P4 prompt-only guardrail. The capability comparison
// itself is `autonoetic_types::capability::compute_capability_delta`
// which is tested in that crate. The tests here pin the tier helper
// and the gate's *integration* (loading manifests + composing the
// reject reason); behavioural tests for the gate sit in
// `tests/improvement_ab_replay_integration.rs`.
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod surface_drift_tests {
    use super::*;
    use autonoetic_types::agent::{
        AgentIdentity, AgentManifest, RuntimeDeclaration, SandboxNetworkPolicy, ToolTier,
    };

    fn base_manifest() -> AgentManifest {
        AgentManifest {
            version: "1.0".into(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".into(),
                gateway_version: "0.1.0".into(),
                sdk_version: "0.1.0".into(),
                runtime_type: "stateful".into(),
                sandbox: "bubblewrap".into(),
                runtime_lock: "runtime.lock".into(),
            },
            agent: AgentIdentity {
                id: "test.default".into(),
                name: "test".into(),
                description: "test".into(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
            capabilities: vec![],
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: SandboxNetworkPolicy::default(),
            egress: None,
        }
    }

    #[test]
    fn tier_surface_detects_added_tier() {
        let mut a = base_manifest();
        let mut b = base_manifest();
        a.allowed_tool_tiers = vec![ToolTier::Core];
        b.allowed_tool_tiers = vec![ToolTier::Core, ToolTier::Specialized];
        assert_ne!(tool_tier_surface(&a), tool_tier_surface(&b));
    }

    #[test]
    fn tier_surface_is_order_insensitive() {
        let mut a = base_manifest();
        let mut b = base_manifest();
        a.allowed_tool_tiers = vec![ToolTier::Core, ToolTier::Specialized];
        b.allowed_tool_tiers = vec![ToolTier::Specialized, ToolTier::Core];
        assert_eq!(tool_tier_surface(&a), tool_tier_surface(&b));
    }

    // The capability-comparison semantics — including the crucial
    // "parameter widening counts" case (e.g. `ReadAccess { scopes:
    // ["self.*"] }` → `ReadAccess { scopes: ["*"] }`) — are exercised
    // end-to-end in `tests/improvement_ab_replay_integration.rs`:
    //
    //   * test_ab_replay_prompt_only_guard_rejects_capability_widening
    //   * test_ab_replay_prompt_only_guard_rejects_parameter_widening
    //   * test_ab_replay_prompt_only_guard_allows_identical_surface
    //   * test_ab_replay_prompt_only_guard_can_be_disabled
}
