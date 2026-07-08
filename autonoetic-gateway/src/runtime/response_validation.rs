//! Response validation gate — validates agent outputs against declared constraints.
//!
//! When enabled, gateway checks SpawnResult against the agent's output policy.
//! Returns violations for each failed check.

use autonoetic_types::agent::{IoReturnsEnforcement, OutputPolicy};
use autonoetic_types::causal_chain::{CausalEventRecord, EntryStatus};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::trajectory::FeedbackEvent;
use regex::RegexBuilder;
use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::causal_chain::CausalLogger;
use crate::execution::{GatewayExecutionService, SpawnResult};
use crate::runtime::live_digest::{
    append_repair_attempt_best_effort, append_repair_passed_best_effort, base_session_id,
};

/// A single validation violation found during response checking.
#[derive(Debug, Clone)]
pub struct ValidationViolation {
    /// Which rule was violated (e.g. "required_artifacts", "max_artifacts").
    pub rule: String,
    /// Human-readable description of what went wrong.
    pub message: String,
    /// Hint the agent can use to repair the violation.
    pub repair_hint: String,
}

impl std::fmt::Display for ValidationViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.rule, self.message)
    }
}

/// Order-independent fingerprint of a violation set (`(rule, message)` pairs,
/// sorted). Two repair attempts that produce the same fingerprint made no
/// progress — the same content failed the same way — so retrying is wasteful.
fn violation_fingerprint(violations: &[ValidationViolation]) -> Vec<(String, String)> {
    let mut fp: Vec<(String, String)> = violations
        .iter()
        .map(|v| (v.rule.clone(), v.message.clone()))
        .collect();
    fp.sort();
    fp
}

/// Convert validation violations into feedback events for the trajectory monitor.
pub fn violations_to_feedback_events(violations: &[ValidationViolation]) -> Vec<FeedbackEvent> {
    violations
        .iter()
        .map(|v| FeedbackEvent::Validation {
            rule: v.rule.clone(),
            field_path: None,
        })
        .collect()
}

/// Kinds of self-report claims an agent reply can make. Each variant maps to a
/// deterministic verifier that reconciles the claim against observable gateway
/// state. New fabrication modes become one enum variant instead of a new
/// hand-written guard (Change A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    /// `status == "delegated"` asserts a child TaskRun was spawned.
    Delegated,
    /// `plan_id` mentioned anywhere in the reply asserts a PlanFrame exists.
    PlanId,
    /// `promotion_record` claim asserts a matching evaluator/auditor trace exists.
    PromotionVerdict,
    /// `artifact_ref` cited in the reply asserts the artifact store contains it.
    ArtifactBuilt,
    /// Declared capability envelope (e.g. NetworkAccess hosts) must match
    /// detected authority-op patterns.
    CapabilityEnvelope,
}

/// Result of reconciling one claim kind against gateway state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimVerdict {
    /// Claim is present and checks out against observable state.
    Ok,
    /// Claim is absent from the reply or the gateway cannot verify it right now.
    Unverified,
    /// Claim is present and demonstrably false.
    Fabricated(String),
}

/// Context needed to verify any claim kind.
pub struct ClaimCtx<'a> {
    pub assistant_reply: Option<&'a str>,
    pub workflow_id: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub gateway_store: Option<&'a crate::scheduler::gateway_store::GatewayStore>,
    pub config: Option<&'a GatewayConfig>,
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub gateway_dir: &'a Path,
    pub agent_is_spawn_capable: bool,
}

impl ClaimKind {
    /// All claim kinds the validator walks over. Keeping this list closed makes
    /// "every claimable field has a verifier" statically checkable.
    pub fn all() -> &'static [ClaimKind] {
        &[
            ClaimKind::Delegated,
            ClaimKind::PlanId,
            ClaimKind::PromotionVerdict,
            ClaimKind::ArtifactBuilt,
            ClaimKind::CapabilityEnvelope,
        ]
    }

    /// Human-readable path to the claim in the reply.
    pub fn field_path(&self) -> &'static str {
        match self {
            ClaimKind::Delegated => "status",
            ClaimKind::PlanId => "plan_id",
            ClaimKind::PromotionVerdict => "promotion_record",
            ClaimKind::ArtifactBuilt => "artifact_ref",
            ClaimKind::CapabilityEnvelope => "capability_envelope",
        }
    }

    /// Reconcile this claim kind against the provided context.
    pub fn verify(&self, ctx: &ClaimCtx) -> ClaimVerdict {
        match self {
            ClaimKind::Delegated => verify_delegated_claim(ctx),
            ClaimKind::PlanId => verify_plan_id_claim(ctx),
            // Remaining variants are future scope; their fields are not yet
            // mechanically reconciled here, so report Unverified rather than
            // Fabricated.
            ClaimKind::PromotionVerdict => ClaimVerdict::Unverified,
            ClaimKind::ArtifactBuilt => ClaimVerdict::Unverified,
            ClaimKind::CapabilityEnvelope => ClaimVerdict::Unverified,
        }
    }
}

/// Convert a fabricated verdict into the corresponding validation violation.
fn claim_verdict_to_violation(kind: ClaimKind, verdict: ClaimVerdict) -> Option<ValidationViolation> {
    match (kind, verdict) {
        (_, ClaimVerdict::Ok | ClaimVerdict::Unverified) => None,
        (ClaimKind::Delegated, ClaimVerdict::Fabricated(_)) => {
            Some(delegated_without_spawn_violation())
        }
        (ClaimKind::PlanId, ClaimVerdict::Fabricated(plan_id)) => {
            Some(fabricated_plan_id_violation(&plan_id))
        }
        // Future claim kinds: map to their violation constructors here.
        (kind, ClaimVerdict::Fabricated(detail)) => Some(ValidationViolation {
            rule: format!("{:?}", kind).to_lowercase(),
            message: detail,
            repair_hint: "Reconcile this claim against observable state.".into(),
        }),
    }
}

fn verify_delegated_claim(ctx: &ClaimCtx) -> ClaimVerdict {
    if !ctx.agent_is_spawn_capable || !reply_is_delegated(ctx.assistant_reply) {
        return ClaimVerdict::Unverified;
    }
    let spawned = match ctx.workflow_id {
        None => Some(false),
        Some(wid) => match ctx.config {
            None => None,
            Some(cfg) => match crate::scheduler::workflow_store::list_task_runs_for_workflow(
                cfg,
                ctx.gateway_store,
                wid,
            ) {
                Ok(tasks) => Some(tasks.iter().any(|t| Some(t.task_id.as_str()) != ctx.task_id)),
                Err(e) => {
                    tracing::warn!(
                        target: "response_validation",
                        workflow_id = %wid,
                        error = %e,
                        "delegated-spawn claim: task listing failed; treating as unverified"
                    );
                    None
                }
            },
        },
    };
    match spawned {
        Some(true) => ClaimVerdict::Ok,
        Some(false) => ClaimVerdict::Fabricated(
            "reported status \"delegated\" but no child agent was spawned this turn".into(),
        ),
        None => ClaimVerdict::Unverified,
    }
}

fn verify_plan_id_claim(ctx: &ClaimCtx) -> ClaimVerdict {
    let Some(claimed) = reply_claimed_plan_id(ctx.assistant_reply) else {
        return ClaimVerdict::Unverified;
    };
    let Some(store) = ctx.gateway_store else {
        return ClaimVerdict::Unverified;
    };
    match store.load_plan_frame(&claimed) {
        Ok(Some(_)) => ClaimVerdict::Ok,
        Ok(None) => ClaimVerdict::Fabricated(claimed),
        Err(e) => {
            tracing::warn!(
                target: "response_validation",
                plan_id = %claimed,
                error = %e,
                "plan-id claim: load failed; treating as unverified"
            );
            ClaimVerdict::Unverified
        }
    }
}

/// Reconcile all claims found in a reply and return any violations.
pub fn reconcile_claims(ctx: &ClaimCtx) -> Vec<ValidationViolation> {
    let mut violations = Vec::new();
    for &kind in ClaimKind::all() {
        if let Some(v) = claim_verdict_to_violation(kind, kind.verify(ctx)) {
            violations.push(v);
        }
    }
    violations
}

/// RFC C — advisory claim reconciliation on the child→parent result path.
///
/// The child's full `SpawnResult` is already validated against `io.returns`
/// before the task is marked complete. This pass is an additional, advisory
/// reconciliation of the *result summary* that crosses back to the parent via
/// `workflow.state` / `workflow.wait` / child-state notifications. Mismatches
/// are logged but never block — the goal is to surface upstream fabrication
/// early for the classifier while the corpus test (§5.3) measures the false-
/// positive rate.
pub fn advisory_reconcile_child_result_summary(
    result_summary: Option<&str>,
    child_session_id: &str,
    parent_session_id: &str,
    child_agent_id: &str,
    gateway_dir: &Path,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    config: Option<&GatewayConfig>,
) -> Vec<ValidationViolation> {
    let Some(summary) = result_summary else {
        return Vec::new();
    };
    // Fabricated claims can only be detected when the summary carries
    // structured references. Empty or trivial summaries are unverifiable.
    if summary.trim().is_empty() || summary.len() < 8 {
        return Vec::new();
    }

    let ctx = ClaimCtx {
        assistant_reply: Some(summary),
        workflow_id: None,
        task_id: None,
        gateway_store,
        config,
        agent_id: child_agent_id,
        session_id: child_session_id,
        gateway_dir,
        // Advisory path: we do not have the child task's workflow_id/task_id,
        // so delegated-spawn verification cannot be performed accurately. Leave
        // it disabled to avoid false positives; `io.returns` validation already
        // ran on the full child reply before the task was marked complete.
        agent_is_spawn_capable: false,
    };
    let violations = reconcile_claims(&ctx);
    if !violations.is_empty() {
        let rules: Vec<_> = violations.iter().map(|v| v.rule.as_str()).collect();
        tracing::warn!(
            target: "response_validation",
            child_session_id = %child_session_id,
            parent_session_id = %parent_session_id,
            child_agent_id = %child_agent_id,
            rules = ?rules,
            "response.validation.advisory: child→parent result summary contains fabricated claim(s)"
        );
    }
    violations
}

/// Parse an `OutputPolicy` from metadata.
pub fn parse_output_policy(
    metadata: Option<&serde_json::Value>,
) -> anyhow::Result<Option<OutputPolicy>> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let Some(io_value) = metadata.get("io") else {
        return Ok(None);
    };
    let Some(policy_value) = io_value.get("output_policy") else {
        return Ok(None);
    };

    let mut policy: OutputPolicy = serde_json::from_value(policy_value.clone())
        .map_err(|e| anyhow::anyhow!("invalid io.output_policy metadata: {}", e))?;

    policy.normalize();

    for pattern in &policy.prohibited_text_patterns {
        RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|e| {
                anyhow::anyhow!(
                    "invalid prohibited_text_patterns regex '{}': {}",
                    pattern,
                    e
                )
            })?;
    }

    if policy.is_empty() {
        return Ok(None);
    }
    Ok(Some(policy))
}

/// Validate a `SpawnResult` against output schema and output policy.
///
/// Returns an empty vector when all checks pass.
pub fn validate_spawn_response(
    result: &SpawnResult,
    output_schema: Option<&serde_json::Value>,
    policy: &OutputPolicy,
    gateway_dir: Option<&Path>,
) -> Vec<ValidationViolation> {
    let mut violations = Vec::new();

    // 1. Required artifacts
    for required in &policy.required_artifacts {
        let found = result.artifacts.iter().any(|a| a.name == *required)
            || result.files.iter().any(|f| f.name == *required);
        if !found {
            violations.push(ValidationViolation {
                rule: "required_artifacts".into(),
                message: format!("required artifact '{}' not produced", required),
                repair_hint: format!(
                    "Create '{}' with content.write then register with artifact.build",
                    required
                ),
            });
        }
    }

    // 2. Max artifacts
    if let Some(max) = policy.max_artifacts {
        if result.artifacts.len() > max {
            violations.push(ValidationViolation {
                rule: "max_artifacts".into(),
                message: format!(
                    "artifact count {} exceeds max_artifacts ({})",
                    result.artifacts.len(),
                    max
                ),
                repair_hint: format!("Reduce artifacts to {} or fewer", max),
            });
        }
    }

    // 3. Max total size of unique named outputs.
    if let Some(max_mb) = policy.max_total_size_mb {
        match compute_total_output_size_bytes(result, gateway_dir) {
            Ok(total_bytes) => {
                let max_bytes = max_mb.saturating_mul(1024 * 1024);
                if total_bytes > max_bytes {
                    violations.push(ValidationViolation {
                        rule: "max_total_size_mb".into(),
                        message: format!(
                            "total output size {} bytes exceeds max_total_size_mb ({} bytes)",
                            total_bytes, max_bytes
                        ),
                        repair_hint: format!(
                            "Reduce output size to {} MiB or fewer by removing or shrinking generated files",
                            max_mb
                        ),
                    });
                }
            }
            Err(e) => {
                violations.push(ValidationViolation {
                    rule: "max_total_size_mb".into(),
                    message: format!("cannot verify output size: {}", e),
                    repair_hint: "Ensure the gateway content store is available and output files are written via content.write".into(),
                });
            }
        }
    }

    // 4. Max reply length
    if let Some(max_chars) = policy.max_reply_length_chars {
        if let Some(ref reply) = result.assistant_reply {
            if reply.len() > max_chars {
                violations.push(ValidationViolation {
                    rule: "max_reply_length_chars".into(),
                    message: format!("reply {} chars exceeds max ({})", reply.len(), max_chars),
                    repair_hint: format!("Shorten reply to {} chars", max_chars),
                });
            }
        }
    }

    // 5. Prohibited text patterns — compile the validated regex and match case-insensitively.
    if let Some(ref reply) = result.assistant_reply {
        for pattern in &policy.prohibited_text_patterns {
            // Patterns were validated at parse_output_policy time; compile is safe.
            let Ok(re) = RegexBuilder::new(pattern).case_insensitive(true).build() else {
                continue; // defensive — should never happen after parse validation
            };
            if re.is_match(reply) {
                violations.push(ValidationViolation {
                    rule: "prohibited_text_pattern".into(),
                    message: format!("reply matches prohibited pattern '{}'", pattern),
                    repair_hint: "Remove or redact the matched text".into(),
                });
            }
        }
    }

    // 6. Output schema (JSON only, lightweight validation)
    if let Some(schema) = output_schema {
        let schema_is_constrained =
            schema.get("required").is_some() || schema.get("properties").is_some();
        match result.assistant_reply.as_deref() {
            None if schema_is_constrained => {
                violations.push(ValidationViolation {
                    rule: "output_schema".into(),
                    message: "no reply produced but output_schema requires structured output"
                        .into(),
                    repair_hint: "Return JSON matching the declared schema".into(),
                });
            }
            Some(reply) => {
                // Strip <think> reasoning blocks first (minimax-m3, DeepSeek),
                // then try parsing as-is.
                let reply_clean = strip_think_blocks(reply);
                // First try: reply after stripping <think> blocks (may be valid
                // JSON that happens to contain ``` inside a string value —
                // stripping markdown fences would destroy it, so we try raw first).
                let json = serde_json::from_str::<serde_json::Value>(reply_clean.trim())
                    .ok()
                    .or_else(|| {
                        // Fallback: strip markdown code fences and retry.
                        let stripped = strip_markdown_code_fences(&reply_clean);
                        serde_json::from_str(&stripped).ok()
                    });
                match json {
                    Some(parsed) => {
                        violations.extend(validate_json_against_schema(&parsed, schema));
                    }
                    None if schema_is_constrained => {
                        violations.push(ValidationViolation {
                            rule: "output_schema".into(),
                            message:
                                "reply is not valid JSON but output schema requires structured output"
                                    .into(),
                            repair_hint: "Return JSON matching the declared schema".into(),
                        });
                    }
                    None => {}
                }
            }
            None => {} // schema has no constraints; no reply is acceptable
        }
    }

    violations
}

fn compute_total_output_size_bytes(
    result: &SpawnResult,
    gateway_dir: Option<&Path>,
) -> anyhow::Result<u64> {
    let Some(gateway_dir) = gateway_dir else {
        anyhow::bail!("gateway directory unavailable");
    };

    let store = crate::runtime::content_store::ContentStore::new(gateway_dir)?;
    let mut unique_handles = HashSet::new();
    let mut total_bytes = 0u64;

    for file in &result.files {
        if !unique_handles.insert(file.handle.clone()) {
            continue;
        }

        let blob_path = store.blob_path(&file.handle);
        let metadata = std::fs::metadata(&blob_path).map_err(|e| {
            anyhow::anyhow!(
                "failed reading blob metadata for '{}' ({}) : {}",
                file.name,
                file.handle,
                e
            )
        })?;
        total_bytes = total_bytes.saturating_add(metadata.len());
    }

    Ok(total_bytes)
}

/// True iff the reply is a JSON object whose `status` is exactly `"delegated"`.
/// Cheap — used to gate the (filesystem-touching) spawn-less-delegation guard so
/// task listing only happens for actual delegation claims.
fn reply_is_delegated(assistant_reply: Option<&str>) -> bool {
    assistant_reply
        .and_then(|r| {
            let clean = strip_think_blocks(r);
            let stripped = strip_markdown_code_fences(&clean);
            serde_json::from_str::<serde_json::Value>(stripped.trim()).ok()
        })
        .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(|s| s == "delegated"))
        .unwrap_or(false)
}

/// A `plan_id` the reply claims (top-level or under `result`), if any non-empty
/// one is present. Used to catch a fabricated reference, NOT to require plans —
/// agents that never mention a `plan_id` (e.g. `planner.default`) are unaffected.
fn reply_claimed_plan_id(assistant_reply: Option<&str>) -> Option<String> {
    // Strip <think> blocks and markdown code fences first — models often wrap
    // their reasoning or JSON reply in these, and a fenced reply must not slip
    // the fabricated-plan_id guard.
    let cleaned = strip_think_blocks(assistant_reply?);
    let stripped = strip_markdown_code_fences(&cleaned);
    let v: serde_json::Value = serde_json::from_str(stripped.trim()).ok()?;
    v.get("plan_id")
        .or_else(|| v.get("result").and_then(|r| r.get("plan_id")))
        .and_then(|p| p.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The violation for a spawn-less `delegated` self-report.
///
/// An `AgentSpawn`-capable agent reporting `status: "delegated"` asserts it
/// handed work to a child; if it never called `agent_spawn`, the assertion is
/// false and the workflow would just end with nothing delegated. Deterministic
/// truthfulness check on the agent's own `io.returns` status; the violation feeds
/// the existing bounded repair loop (P-5.8), and on exhaustion returns an error
/// (Ri-0.12 (e)). A legitimate delegation (a child was spawned) never trips it.
fn delegated_without_spawn_violation() -> ValidationViolation {
    ValidationViolation {
        rule: "delegated_without_spawn".into(),
        message: "reported status \"delegated\" but no child agent was spawned this turn".into(),
        repair_hint: "To delegate you must actually call `agent_spawn` (async=true), then report \
`delegated`. If you are not delegating, report a truthful status (`ok`, `partial`, \
`clarification_needed`, or `failed`)."
            .into(),
    }
}

/// The violation for a reply that references a non-existent `plan_id`.
///
/// PlanFrames are optional (e.g. `planner.default` never uses them), so this is
/// NOT a "you must propose a plan" check — it only fires when a reply explicitly
/// names a `plan_id` that does not exist. A weak model may fabricate one (e.g.
/// `plan-a1b2c3d4`) and report `awaiting_approval` without ever calling
/// `planframe_propose`, leaving nothing to approve and stalling the flow.
/// Deterministic truthfulness check against observable state; feeds the bounded
/// repair loop.
fn fabricated_plan_id_violation(plan_id: &str) -> ValidationViolation {
    ValidationViolation {
        rule: "unknown_plan_id".into(),
        message: format!(
            "reply references plan_id \"{plan_id}\" but no such PlanFrame exists"
        ),
        repair_hint: "Do not invent a plan_id. If you proposed a plan, use the exact plan_id \
returned by `planframe_propose`; otherwise omit `plan_id` and report a truthful status."
            .into(),
    }
}

/// Validate that a required promotion.record was called during the session.
///
/// When metadata contains `require_promotion_record: true`, the gateway checks
/// the PromotionStore for a matching record. Two failure modes:
/// 1. No record exists at all → agent forgot to call `promotion.record` → repairable
/// 2. Record exists but pass=false → evaluator/auditor rejected the artifact → terminal
pub fn validate_promotion_record(
    gateway_dir: Option<&Path>,
    promotion_artifact_id: &str,
    promotion_role: &str,
) -> Vec<ValidationViolation> {
    let mut violations = Vec::new();

    let Some(gw_dir) = gateway_dir else {
        violations.push(ValidationViolation {
            rule: "promotion_record".into(),
            message: "cannot verify promotion record: gateway directory unavailable".into(),
            repair_hint: "Ensure the gateway directory is configured".into(),
        });
        return violations;
    };

    let store = match crate::runtime::promotion_store::PromotionStore::new(gw_dir) {
        Ok(s) => s,
        Err(e) => {
            violations.push(ValidationViolation {
                rule: "promotion_record".into(),
                message: format!("cannot load promotion store: {}", e),
                repair_hint: "Ensure the gateway promotion store is accessible".into(),
            });
            return violations;
        }
    };

    match store.get_promotion(promotion_artifact_id) {
        None => {
            violations.push(ValidationViolation {
                rule: "promotion_record_missing".into(),
                message: format!(
                    "completed without a matching promotion_record within the session for artifact '{}' (role: {})",
                    promotion_artifact_id, promotion_role
                ),
                repair_hint: format!(
                    "Call promotion_record with artifact_id='{}', role='{}', pass=true (or false if validation failed). Example: promotion_record({{\"artifact_id\": \"{}\", \"role\": \"{}\", \"pass\": true}})",
                    promotion_artifact_id, promotion_role, promotion_artifact_id, promotion_role
                ),
            });
        }
        Some(record) => {
            let (passed, findings) = match record.get_role_result(promotion_role) {
                Some(v) => v,
                None => {
                    violations.push(ValidationViolation {
                        rule: "promotion_record".into(),
                        message: format!("unknown promotion role '{}'", promotion_role),
                        repair_hint: "Use a known promotion role name (evaluator, auditor, static_evaluator, unit_test_runner, sealed_evaluator)".into(),
                    });
                    return violations;
                }
            };

            if !passed {
                let findings_summary = if findings.is_empty() {
                    "no findings provided".to_string()
                } else {
                    findings
                        .iter()
                        .map(|f| format!("[{:?}] {}", f.severity, f.description))
                        .collect::<Vec<_>>()
                        .join("; ")
                };
                violations.push(ValidationViolation {
                    rule: "promotion_record_failed".into(),
                    message: format!(
                        "{} recorded pass=false for artifact '{}': {}",
                        promotion_role, promotion_artifact_id, findings_summary
                    ),
                    repair_hint: format!(
                        "The {} rejected the artifact. Fix the issues and re-run validation before installing.",
                        promotion_role
                    ),
                });
            }
        }
    }

    violations
}

/// Validate durable tool-evidence requirements using gateway execution traces.
pub fn validate_session_evidence(
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
    policy: &OutputPolicy,
) -> Vec<ValidationViolation> {
    let mut violations = Vec::new();

    let min_builds = policy.min_artifact_builds.unwrap_or(0);
    if min_builds == 0 {
        return violations;
    }

    let Some(store) = gateway_store else {
        violations.push(ValidationViolation {
            rule: "artifact_build_evidence".into(),
            message: "cannot verify artifact.build evidence: gateway store unavailable".into(),
            repair_hint:
                "Retry with gateway store enabled, and ensure artifact.build is called before finishing"
                    .into(),
        });
        return violations;
    };

    let traces = match store.search_execution_traces(
        Some("artifact_build"),
        Some(true),
        None,
        None,
        None,
        Some(session_id),
        10_000,
    ) {
        Ok(t) => t,
        Err(e) => {
            violations.push(ValidationViolation {
                rule: "artifact_build_evidence".into(),
                message: format!("failed querying execution traces: {}", e),
                repair_hint: "Retry the run and ensure gateway tracing is operational".into(),
            });
            return violations;
        }
    };

    let build_count = traces.len() as u32;
    if build_count < min_builds {
        violations.push(ValidationViolation {
            rule: "artifact_build_evidence".into(),
            message: format!(
                "requires at least {} successful artifact.build call(s), found {}",
                min_builds, build_count
            ),
            repair_hint:
                "Create required files with content.write, then call artifact.build before finishing"
                    .into(),
        });
    }

    violations
}

/// Build a structured repair prompt to inject back into the child agent session.
///
/// Uses clear section headers, reasoning, and examples to help LLM agents understand
/// what failed and how to fix it. Designed for LLM reasoning patterns (prose > JSON).
pub fn build_repair_prompt(
    violations: &[ValidationViolation],
    attempt: usize,
    max_repair_rounds: usize,
) -> String {
    let remaining = max_repair_rounds - attempt + 1;

    // Build violations section with reasoning
    let violations_section: Vec<String> = violations
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let rule_explanation = match v.rule.as_str() {
                "required_artifacts" => "You must produce this file as a deliverable.",
                "max_artifacts" => "You produced too many files; consolidate them.",
                "max_total_size_mb" => "Your output is too large; reduce file sizes.",
                "max_reply_length_chars" => "Your text reply is too long; be concise.",
                "prohibited_text_pattern" => "Your reply contains sensitive data or unsafe content.",
                "output_schema" => "Your output does not match the required JSON schema.",
                "promotion_record_missing" => "You forgot to call promotion_record — this is required for artifact promotion gates.",
                "promotion_record_failed" => "The evaluator or auditor rejected the artifact. This cannot be auto-repaired.",
                _ => "Your output violates a declared constraint.",
            };

            format!(
                "{}. [{}] {}\n   Why: {}\n   Fix: {}",
                i + 1,
                v.rule,
                v.message,
                rule_explanation,
                v.repair_hint
            )
        })
        .collect();

    // Build repair instructions based on violation types
    let repair_examples = build_repair_examples(violations);

    format!(
        "[GATEWAY_VALIDATION] REPAIR REQUIRED — Attempt {}/{}\n\
═══════════════════════════════════════════════════════════════════════\n\n\
WHAT FAILED:\n\
───────────────────────────────────────────────────────────────────────\n\
Your previous output failed validation. These {} constraint(s) must be fixed:\n\n\
{}\n\n\
WHAT TO DO:\n\
───────────────────────────────────────────────────────────────────────\n\
For each violation above:\n\
• Understand why it failed (the \"Why\" explanation)\n\
• Apply the fix (the \"Fix\" hint)\n\
• Use your normal tools: artifact.build() for bundles, content.write() to create a new file, content.patch() to edit an existing file, etc.\n\
• Re-run your workflow to regenerate the output\n\n\
EXAMPLES OF CORRECT OUTPUT:\n\
───────────────────────────────────────────────────────────────────────\n\
{}\n\n\
CONSTRAINT SUMMARY:\n\
───────────────────────────────────────────────────────────────────────\n\
✓ Fix ALL {} issue(s) above before finishing\n\
✓ {} repair attempt(s) remaining\n\
✓ After fixes, run your workflow again to produce corrected output\n\n\
Continue repairing your output.",
        attempt,
        max_repair_rounds,
        violations.len(),
        violations_section.join("\n"),
        repair_examples,
        violations.len(),
        remaining
    )
}

/// Generate contextual repair examples based on violation types.
fn build_repair_examples(violations: &[ValidationViolation]) -> String {
    let mut examples = Vec::new();

    for v in violations {
        match v.rule.as_str() {
            "required_artifacts" => {
                examples.push(
                    "Required Artifact:\n  \
                     If the file already exists in the session, edit it with content.patch({\"name\": \"filename.ext\", \"old_string\": \"...\", \"new_string\": \"...\"}).\n  \
                     If it does not exist yet, create it with content.write(\"path/to/file\", contents) or artifact.build({\"inputs\": [...]})."
                        .to_string(),
                );
            }
            "max_reply_length_chars" => {
                examples.push(
                    "Reply Length:\n  \
                     Condense your response. Remove verbose explanations and keep only essential info.\n  \
                     Target: 1-2 paragraph summary instead of detailed analysis."
                        .to_string()
                );
            }
            "prohibited_text_pattern" => {
                examples.push(
                    "Sensitive Data:\n  \
                     ❌ BAD:  api_key = \"sk-1234567890abcdef\"\n  \
                     ✓ GOOD: api_key = \"<use_credential_store>\" or \"${SECRET_API_KEY}\""
                        .to_string(),
                );
            }
            "output_schema" => {
                examples.push(
                    "JSON Schema:\n  \
                     ❌ BAD:  \"result completed\"\n  \
                     ✓ GOOD: {\"status\": \"success\", \"result\": \"...\"}"
                        .to_string(),
                );
            }
            "max_artifacts" => {
                examples.push(
                    "Artifact Consolidation:\n  \
                     Combine similar files into fewer artifacts or use subdirectories."
                        .to_string(),
                );
            }
            "promotion_record_missing" => {
                examples.push(
                    "Promotion Record:\n  \
                     Call promotion_record as a tool (not via sandbox_exec):\n  \
                     promotion_record({\"artifact_id\": \"<your_artifact_id>\", \"role\": \"evaluator\", \"pass\": true, \"summary\": \"Tests passed\"})"
                        .to_string()
                );
            }
            _ => {}
        }
    }

    if examples.is_empty() {
        "Generic: Review your output carefully and ensure all violations are resolved.".to_string()
    } else {
        examples.join("\n\n  ")
    }
}

/// Convert violations into a terminal `anyhow::Error` for propagation to the caller.
///
/// When `include_session_context` is true (repair mode was active), the error includes
/// the `session_id` and a "Repair hints" block so the calling agent can understand
/// the failure and take corrective action at a higher level.
pub fn violations_to_final_error(
    violations: &[ValidationViolation],
    session_id: &str,
    include_session_context: bool,
    actual_reply: Option<&str>,
) -> anyhow::Error {
    let summary: String = violations
        .iter()
        .map(|v| {
            format!(
                "[{}] {} (repair_hint: {})",
                v.rule, v.message, v.repair_hint
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    if include_session_context {
        let hints: String = violations
            .iter()
            .map(|v| v.repair_hint.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut msg = format!(
            "response validation failed: {}. Session: {}. Repair hints: {}",
            summary,
            session_id,
            hints
        );
        if let Some(reply) = actual_reply.filter(|r| !r.is_empty()) {
            let snippet = if reply.len() > 200 {
                format!("{}...", &reply[..200])
            } else {
                reply.to_string()
            };
            msg.push_str(&format!(". Agent produced: {}", snippet));
        }
        anyhow::anyhow!(msg)
    } else {
        anyhow::anyhow!("response validation failed: {}", summary)
    }
}

/// Strip `<think>…</think>` reasoning blocks that some models (minimax-m3,
/// DeepSeek, Qwen) emit inline in the assistant reply text. Unlike Anthropic's
/// native thinking channel, these are part of the reply payload and leak into
/// validation, history, and display. Stripping them early ensures downstream
/// JSON parsing, schema validation, and chat rendering see clean content.
///
/// Handles both closed (`<think>…</think>`) and unclosed (`<think>…` to end)
/// blocks. Returns the text with all think blocks removed and leading/trailing
/// whitespace trimmed.
pub fn strip_think_blocks(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains("<think>") {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 7..];
        if let Some(end) = after_open.find("</think>") {
            rest = &after_open[end + 8..];
        } else {
            // Unclosed think block — discard everything after `<think>`.
            rest = "";
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out.trim().to_string())
}

/// Strip markdown code fences wrapping a JSON payload.
///
/// LLMs (especially DeepSeek) often return JSON wrapped in
/// `` ```json ... ``` `` or `` ``` ... ``` ``, sometimes with prose
/// before/after the fence. This helper extracts the first JSON-like
/// code fence block so `serde_json::from_str` can parse the inner JSON.
fn strip_markdown_code_fences(s: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = s.trim();
    if !trimmed.contains("```") {
        return std::borrow::Cow::Borrowed(s);
    }
    if let Some(extracted) = extract_first_fenced_json(trimmed) {
        // Observable tolerance (M1 doctrine, #619): emitted only when we actually
        // changed the input by unwrapping markdown fences. Both call sites
        // (output_schema validation and the fabricated-plan_id guard) go through
        // this single helper, so instrumenting here covers both. `detail` is a
        // redacted summary — never the reply body.
        crate::runtime::tool_call_processor::note_llm_normalization(
            "markdown_code_fence",
            "stripped markdown code fences wrapping a JSON reply",
        );
        return std::borrow::Cow::Owned(extracted);
    }
    std::borrow::Cow::Borrowed(s)
}

fn extract_first_fenced_json(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut pos = 0;
    while pos < len {
        if bytes[pos] == b'`' && pos + 2 < len && &bytes[pos..pos + 3] == b"```" {
            let fence_start = pos;
            pos += 3;
            while pos < len && bytes[pos] != b'\n' && bytes[pos] != b'\r' {
                pos += 1;
            }
            if pos < len && bytes[pos] == b'\r' {
                pos += 1;
            }
            if pos < len && bytes[pos] == b'\n' {
                pos += 1;
            }
            let content_start = pos;
            let mut search = content_start;
            while search < len {
                if bytes[search] == b'`'
                    && search + 2 < len
                    && &bytes[search..search + 3] == b"```"
                {
                    let content = s[content_start..search].trim();
                    if serde_json::from_str::<serde_json::Value>(content).is_ok() {
                        return Some(content.to_owned());
                    }
                    break;
                }
                search += 1;
            }
            pos = if search + 3 < len { search + 3 } else { len };
        } else {
            pos += 1;
        }
    }
    None
}

/// Lightweight JSON schema validation (required + type + enum + minLength).
fn validate_json_against_schema(
    json: &serde_json::Value,
    schema: &serde_json::Value,
) -> Vec<ValidationViolation> {
    let mut violations = Vec::new();

    // Required fields
    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        for req in required {
            if let Some(field) = req.as_str() {
                if json.get(field).is_none() {
                    violations.push(ValidationViolation {
                        rule: "output_schema".into(),
                        message: format!("required field '{}' missing", field),
                        repair_hint: format!("Include '{}' in your JSON reply", field),
                    });
                }
            }
        }
    }

    // Property checks
    if let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) {
        for (key, prop_schema) in properties {
            let Some(value) = json.get(key) else { continue };

            // Type check
            if let Some(type_spec) = prop_schema.get("type").and_then(|v| v.as_str()) {
                let matches = match type_spec {
                    "string" => value.is_string(),
                    "number" => value.is_number(),
                    "integer" => value.is_i64() || value.is_u64(),
                    "boolean" => value.is_boolean(),
                    "object" => value.is_object(),
                    "array" => value.is_array(),
                    "null" => value.is_null(),
                    _ => true,
                };
                if !matches {
                    violations.push(ValidationViolation {
                        rule: "output_schema".into(),
                        message: format!(
                            "field '{}' expected type '{}', got {}",
                            key,
                            type_spec,
                            json_type_name(value)
                        ),
                        repair_hint: format!("Set '{}' to type '{}'", key, type_spec),
                    });
                }
            }

            // Enum check
            if let Some(enum_vals) = prop_schema.get("enum").and_then(|v| v.as_array()) {
                if !enum_vals.contains(value) {
                    violations.push(ValidationViolation {
                        rule: "output_schema".into(),
                        message: format!(
                            "field '{}' value {:?} not in enum {:?}",
                            key, value, enum_vals
                        ),
                        repair_hint: format!("Use one of the allowed values for '{}'", key),
                    });
                }
            }

            // minLength for strings
            if let Some(min_len) = prop_schema.get("minLength").and_then(|v| v.as_u64()) {
                if let Some(s) = value.as_str() {
                    if (s.len() as u64) < min_len {
                        violations.push(ValidationViolation {
                            rule: "output_schema".into(),
                            message: format!(
                                "field '{}' length {} < minLength {}",
                                key,
                                s.len(),
                                min_len
                            ),
                            repair_hint: format!("Ensure '{}' has at least {} chars", key, min_len),
                        });
                    }
                }
            }
        }
    }

    violations
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

pub(crate) fn log_nested_spawn_to_gateway(
    config: &GatewayConfig,
    session_id: &str,
    source_agent_id: Option<&str>,
    agent_id: &str,
    message: &str,
    result: &SpawnResult,
) {
    let logger = match crate::execution::init_gateway_causal_logger(config) {
        Ok(l) => l,
        Err(_) => return,
    };
    let path = logger.path().to_path_buf();
    let entries = match CausalLogger::read_entries(&path) {
        Ok(e) => e,
        Err(err) => {
            if path.exists() {
                tracing::warn!(
                    error = %err,
                    "Failed to read existing gateway causal entries before input schema log"
                );
                return;
            }
            Vec::new()
        }
    };
    let mut seq = entries.last().map(|e| e.event_seq + 1).unwrap_or(1);
    let requested_data = serde_json::json!({
        "agent_id": agent_id,
        "source_agent_id": source_agent_id,
        "session_id": session_id,
        "message_len": message.len(),
        "message_sha256": crate::execution::sha256_hex(message),
    });
    crate::execution::log_gateway_causal_event(
        &logger,
        &crate::execution::gateway_actor_id(),
        session_id,
        seq,
        "agent.spawn.requested",
        EntryStatus::Success,
        Some(requested_data),
    );
    seq += 1;
    let completed_data = serde_json::json!({
        "agent_id": result.agent_id,
        "source_agent_id": source_agent_id,
        "session_id": result.session_id,
        "assistant_reply_len": result.assistant_reply.as_ref().map(|s| s.len()).unwrap_or(0),
        "assistant_reply_sha256": result.assistant_reply.as_ref().map(|s| crate::execution::sha256_hex(s)),
        "llm_usage": result.llm_usage,
    });
    crate::execution::log_gateway_causal_event(
        &logger,
        &crate::execution::gateway_actor_id(),
        session_id,
        seq,
        "agent.spawn.completed",
        EntryStatus::Success,
        Some(completed_data),
    );
}

pub(crate) fn contract_event_seq() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn log_contract_enforcement_event_to_gateway(
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    agent_id: &str,
    session_id: &str,
    action: &str,
    status: EntryStatus,
    target_agent_id: Option<&str>,
    payload: serde_json::Value,
) {
    let Some(store) = gateway_store else {
        return;
    };

    let payload_str = serde_json::to_string(&payload).ok();
    let reason = payload
        .get("reason")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);

    if let Err(error) = store.create_causal_event(&CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        event_seq: contract_event_seq(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "contract".to_string(),
        action: action.to_string(),
        status: status.to_string(),
        enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
        target: target_agent_id.map(ToOwned::to_owned),
        payload: payload_str,
        payload_ref: None,
        evidence_ref: None,
        reason,
    }) {
        tracing::warn!(
            target: "response_validation",
            error = %error,
            action = action,
            agent_id = agent_id,
            session_id = session_id,
            "Failed to persist contract enforcement event"
        );
    }
}

impl GatewayExecutionService {
    pub(crate) async fn validate_and_maybe_repair(
        &self,
        agent_id: &str,
        mut result: SpawnResult,
        output_schema: Option<&serde_json::Value>,
        output_policy: &autonoetic_types::agent::OutputPolicy,
        returns_enforcement: IoReturnsEnforcement,
        source_agent_id: Option<&str>,
        workflow_id: Option<&str>,
        task_id: Option<&str>,
        agent_is_spawn_capable: bool,
        mut feedback_out: Option<&mut Vec<FeedbackEvent>>,
    ) -> anyhow::Result<SpawnResult> {
        let max_duration_ms = output_policy.validation_max_duration_ms;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(max_duration_ms as u64);
        let repair_enabled =
            self.config().response_validation.repair_enabled && output_policy.repair.auto;
        let max_repair_rounds = output_policy.declared_repair_attempts().min(
            self.config()
                .response_validation
                .max_repair_attempts_ceiling as usize,
        );

        let gateway_dir = self.config().agents_dir.join(".gateway");

        let mut violations =
            validate_spawn_response(&result, output_schema, output_policy, Some(&gateway_dir));
        violations.extend(validate_session_evidence(
            self.gateway_store().as_deref(),
            &result.session_id,
            output_policy,
        ));
        // Spawn-less `delegated` guard: a `delegated` status asserts a child was
        // spawned; verify one actually was (a child TaskRun exists in the
        // workflow, distinct from this agent's own task). Gate the filesystem
        // task-listing on the cheap status check so it only runs for actual
        // delegation claims; if listing fails, skip the guard rather than risk a
        // false positive from a transient/operational error.
        //
        // Fabricated-plan-id guard: PlanFrames are optional, so this does NOT
        // require a plan — it only fires when a reply explicitly names a `plan_id`
        // that doesn't exist (a weak model inventing one, e.g. `plan-a1b2c3d4`,
        // and claiming `awaiting_approval` without calling `planframe_propose`).
        // Gate the DB lookup on the cheap reply check; skip on lookup error to
        // avoid a false positive.
        //
        // Both guards are implemented as typed `ClaimKind` verifiers so new
        // self-report fabrications become one enum variant instead of a new
        // hand-written guard (Change A).
        let config = self.config();
        let gateway_store = self.gateway_store();
        let claim_ctx = ClaimCtx {
            assistant_reply: result.assistant_reply.as_deref(),
            workflow_id,
            task_id,
            gateway_store: gateway_store.as_deref(),
            config: Some(config.as_ref()),
            agent_id,
            session_id: &result.session_id,
            gateway_dir: &gateway_dir,
            agent_is_spawn_capable,
        };
        violations.extend(reconcile_claims(&claim_ctx));
        if violations.is_empty() {
            tracing::debug!(
                target: "response_validation",
                agent_id = %agent_id,
                session_id = %result.session_id,
                "response.validation.pass"
            );

            if let Some(out) = feedback_out {
                out.extend(violations_to_feedback_events(&violations));
            }

            // Issue #30 / #752: persist any `decision_journal` entries as
            // `curator.decision` events. Independent of io.returns schema
            // validation — see `persist_curator_decision_journal`.
            self.persist_curator_decision_journal(&result, agent_id);

            return Ok(result);
        }

        // Advisory enforcement: output_schema violations are logged but not blocking.
        // Non-schema violations (prohibited_text_pattern, required_artifacts, etc.)
        // are still enforced.
        if returns_enforcement == IoReturnsEnforcement::Advisory {
            let (schema_violations, mut policy_violations): (Vec<_>, Vec<_>) = violations
                .iter()
                .partition(|v| v.rule == "output_schema");

            if !schema_violations.is_empty() {
                let summary: String = schema_violations
                    .iter()
                    .map(|v| format!("[{}] {}", v.rule, v.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                tracing::warn!(
                    target: "response_validation",
                    agent_id = %agent_id,
                    session_id = %result.session_id,
                    enforcement = "advisory",
                    violations = %summary,
                    "response.validation.advisory: io.returns schema violations ignored (advisory mode)"
                );
                log_contract_enforcement_event_to_gateway(
                    self.gateway_store().as_deref(),
                    agent_id,
                    &result.session_id,
                    "io.returns.advisory",
                    EntryStatus::Success,
                    source_agent_id,
                    serde_json::json!({
                        "contract": "io.returns",
                        "enforcement": "advisory",
                        "result": "advisory_skip",
                        "violations": schema_violations.iter().map(|v| &v.message).collect::<Vec<_>>(),
                    }),
                );
            }

            if policy_violations.is_empty() {
                // Issue #752: journal extraction is independent of io.returns
                // schema validation — persist before the Advisory early return.
                self.persist_curator_decision_journal(&result, agent_id);
                return Ok(result);
            }

            if let Some(out) = feedback_out.as_deref_mut() {
                let pv_owned: Vec<ValidationViolation> = policy_violations.iter().map(|v| (*v).clone()).collect();
                out.extend(violations_to_feedback_events(&pv_owned));
            }

            // Continue enforcement with only non-schema violations.
            violations = policy_violations.into_iter().cloned().collect();
        }

        tracing::warn!(
            target: "response_validation",
            agent_id = %agent_id,
            session_id = %result.session_id,
            violation_count = violations.len(),
            "response.validation.fail"
        );

        if let Some(out) = feedback_out.as_deref_mut() {
            out.extend(violations_to_feedback_events(&violations));
        }

        if !repair_enabled || max_repair_rounds == 0 {
            // Persist validation feedback to the latest checkpoint so a later
            // retry/resume can detect ignored feedback even when repair is
            // disabled or exhausted.
            self.persist_validation_feedback(
                &result.session_id,
                &violations,
            );
            return Err(violations_to_final_error(
                &violations,
                &result.session_id,
                repair_enabled,
                result.assistant_reply.as_deref(),
            ));
        }

        // RFC D.4: enter repair-loop-aware accounting before cycling. Repair
        // iterations count against their own budget, not
        // `max_loops_without_progress`.
        let repair_session_id = result.session_id.clone();
        let _ = crate::runtime::checkpoint::enter_repair_mode_on_latest_checkpoint(
            self.config().as_ref(),
            &repair_session_id,
            max_repair_rounds as u32 + 2,
        );

        let repair_outcome: anyhow::Result<SpawnResult> = async move {
            for attempt in 1..=max_repair_rounds {
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        target: "response_validation",
                        agent_id = %agent_id,
                        attempt = attempt,
                        "response.repair.exhausted: deadline reached"
                    );
                    self.persist_validation_feedback(
                        &result.session_id,
                        &violations,
                    );
                    return Err(violations_to_final_error(
                        &violations,
                        &result.session_id,
                        true,
                        result.assistant_reply.as_deref(),
                    ));
                }

                let repair_msg = build_repair_prompt(&violations, attempt, max_repair_rounds);

                tracing::info!(
                    target: "response_validation",
                    agent_id = %agent_id,
                    session_id = %result.session_id,
                    attempt = attempt,
                    max_repair_rounds = max_repair_rounds,
                    "response.repair.start"
                );

                let base = base_session_id(&result.session_id);
                let violation_summary = violations
                    .iter()
                    .map(|v| v.rule.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                append_repair_attempt_best_effort(
                    &gateway_dir,
                    base,
                    attempt,
                    max_repair_rounds,
                    &format!("{} ({})", violation_summary, violations.len()),
                );

                // Fingerprint of the violation set we're asking the agent to
                // fix this attempt, to detect a no-progress respawn below.
                let pre_repair_fingerprint = violation_fingerprint(&violations);

                let repaired = match self
                    .respawn_from_checkpoint(
                        agent_id,
                        &result.session_id,
                        Some(&repair_msg),
                        source_agent_id,
                        workflow_id,
                        task_id,
                        &violations_to_feedback_events(&violations),
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(
                            target: "response_validation",
                            agent_id = %agent_id,
                            error = %e,
                            "response.repair.error: respawn failed"
                        );
                        return Err(violations_to_final_error(
                            &violations,
                            &result.session_id,
                            true,
                            result.assistant_reply.as_deref(),
                        ));
                    }
                };

                if repaired.suspended_for_approval.is_some() || repaired.suspended_for_user_input {
                    tracing::warn!(
                        target: "response_validation",
                        agent_id = %agent_id,
                        "response.repair.aborted: session suspended during repair"
                    );
                    return Err(anyhow::anyhow!(
                        "repair aborted: agent suspended during repair; session: {}",
                        result.session_id
                    ));
                }

                if let Ok(Some(cp)) = crate::runtime::checkpoint::load_latest_checkpoint(
                    &self.config(),
                    &repaired.session_id,
                ) {
                    if matches!(
                        cp.yield_reason,
                        crate::runtime::checkpoint::YieldReason::UserInputRequired { .. }
                    ) {
                        tracing::warn!(
                            target: "response_validation",
                            agent_id = %agent_id,
                            session_id = %repaired.session_id,
                            "response.repair.aborted: session suspended for user interaction during repair"
                        );
                        return Err(anyhow::anyhow!(
                            "repair aborted: agent suspended for user interaction during repair; session: {}",
                            result.session_id
                        ));
                    }
                }

                violations = validate_spawn_response(
                    &repaired,
                    output_schema,
                    output_policy,
                    Some(&gateway_dir),
                );
                violations.extend(validate_session_evidence(
                    self.gateway_store().as_deref(),
                    &repaired.session_id,
                    output_policy,
                ));
                result = repaired;

                if let Some(out) = feedback_out.as_deref_mut() {
                    out.extend(violations_to_feedback_events(&violations));
                }

                if violations.is_empty() {
                    tracing::info!(
                        target: "response_validation",
                        agent_id = %agent_id,
                        session_id = %result.session_id,
                        attempt = attempt,
                        "response.repair.pass"
                    );
                    append_repair_passed_best_effort(
                        &gateway_dir,
                        base_session_id(&result.session_id),
                        attempt,
                    );
                    return Ok(result);
                }

                tracing::warn!(
                    target: "response_validation",
                    agent_id = %agent_id,
                    attempt = attempt,
                    violation_count = violations.len(),
                    "response.repair.fail"
                );

                // No-progress short-circuit: the respawn reproduced the exact
                // same violation set it was asked to fix. Another full-context
                // respawn is unlikely to differ, so stop instead of burning the
                // remaining repair budget (same "mechanical, not reactive"
                // principle as the LoopGuard no-progress trip).
                if violation_fingerprint(&violations) == pre_repair_fingerprint {
                    tracing::warn!(
                        target: "response_validation",
                        agent_id = %agent_id,
                        attempt = attempt,
                        violation_count = violations.len(),
                        "response.repair.no_progress: identical violations after respawn, stopping early"
                    );
                    self.persist_validation_feedback(&result.session_id, &violations);
                    return Err(violations_to_final_error(
                        &violations,
                        &result.session_id,
                        true,
                        result.assistant_reply.as_deref(),
                    ));
                }

                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        target: "response_validation",
                        agent_id = %agent_id,
                        attempt = attempt,
                        "response.repair.exhausted: deadline reached after respawn"
                    );
                    self.persist_validation_feedback(
                        &result.session_id,
                        &violations,
                    );
                    return Err(violations_to_final_error(
                        &violations,
                        &result.session_id,
                        true,
                        result.assistant_reply.as_deref(),
                    ));
                }
            }

            tracing::warn!(
                target: "response_validation",
                agent_id = %agent_id,
                "response.repair.exhausted: max_loops reached"
            );
            self.persist_validation_feedback(
                &result.session_id,
                &violations,
            );
            Err(violations_to_final_error(
                &violations,
                &result.session_id,
                true,
                result.assistant_reply.as_deref(),
            ))
        }.await;

        match repair_outcome {
            Ok(result) => {
                let _ = crate::runtime::checkpoint::reset_after_successful_repair_on_latest_checkpoint(
                    self.config().as_ref(),
                    &result.session_id,
                );
                // Issue #752: a successfully repaired reply may still carry a
                // `decision_journal`; persist it like any other Ok path.
                self.persist_curator_decision_journal(&result, agent_id);
                Ok(result)
            }
            Err(e) => {
                let _ = crate::runtime::checkpoint::exit_repair_mode_on_latest_checkpoint(
                    self.config().as_ref(),
                    &repair_session_id,
                );
                Err(e)
            }
        }
    }

    /// Persist any `decision_journal` entries the reply carries as one
    /// `curator.decision` causal event per entry (Issue #30).
    ///
    /// This is deliberately independent of io.returns schema validation
    /// (Issue #752): even when the reply is incomplete or was admitted under
    /// Advisory enforcement — or only passed after a repair round — the
    /// journal entries that *are* present must still be recorded so the
    /// causal-chain audit trail is never silently dropped. Call this at every
    /// `Ok` exit of [`validate_and_maybe_repair`].
    fn persist_curator_decision_journal(&self, result: &SpawnResult, agent_id: &str) {
        let (Some(store), Some(reply)) =
            (self.gateway_store(), result.assistant_reply.as_deref())
        else {
            return;
        };
        let revision_id = store
            .get_session_agent_binding(&result.session_id)
            .ok()
            .flatten()
            .map(|b| b.revision_id)
            .filter(|s| !s.is_empty());
        match crate::runtime::curator_journal::extract_and_persist(
            store.as_ref(),
            "curator",
            agent_id,
            &result.session_id,
            revision_id.as_deref(),
            reply,
        ) {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                target: "curator_journal",
                agent_id = %agent_id,
                session_id = %result.session_id,
                entry_count = n,
                "decision_journal persisted"
            ),
            Err(e) => tracing::warn!(
                target: "curator_journal",
                agent_id = %agent_id,
                session_id = %result.session_id,
                error = %e,
                "decision_journal persistence failed"
            ),
        }
    }

    fn persist_validation_feedback(
        &self,
        session_id: &str,
        violations: &[ValidationViolation],
    ) {
        let events = violations_to_feedback_events(violations);
        if events.is_empty() {
            return;
        }
        if let Err(e) = crate::runtime::checkpoint::append_feedback_to_latest_checkpoint(
            self.config().as_ref(),
            session_id,
            &events,
        ) {
            tracing::warn!(
                target: "response_validation",
                session_id = %session_id,
                error = %e,
                "failed to persist validation feedback to checkpoint"
            );
        }
    }

    fn resolve_artifact_ref_to_id(
        &self,
        artifact_ref: &str,
        session_id: &str,
    ) -> Option<String> {
        self.gateway_store()
            .and_then(|gs| {
                gs.resolve_artifact_ref_any_scope(artifact_ref, session_id)
                    .ok()
                    .flatten()
                    .map(|r| r.artifact_id)
            })
    }

    pub(crate) async fn validate_promotion_gate(
        &self,
        agent_id: &str,
        mut result: SpawnResult,
        metadata: Option<&serde_json::Value>,
        source_agent_id: Option<&str>,
        workflow_id: Option<&str>,
        task_id: Option<&str>,
    ) -> anyhow::Result<SpawnResult> {
        if result.suspended_for_approval.is_some() || result.suspended_for_user_input {
            return Ok(result);
        }

        let require_promotion = metadata
            .and_then(|m| m.get("require_promotion_record"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if !require_promotion {
            return Ok(result);
        }

        let raw_artifact_id = metadata
            .and_then(|m| {
                m.get("promotion_artifact_id")
                    .or_else(|| m.get("promotion_artifact_ref"))
            })
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let promotion_role = metadata
            .and_then(|m| m.get("promotion_role"))
            .and_then(|v| v.as_str())
            .unwrap_or("evaluator");

        let promotion_artifact_id = if raw_artifact_id.starts_with("ar.")
            || raw_artifact_id.starts_with("ar_")
        {
            match self.resolve_artifact_ref_to_id(raw_artifact_id, &result.session_id) {
                Some(id) => id,
                None => raw_artifact_id.to_string(),
            }
        } else {
            raw_artifact_id.to_string()
        };

        let gateway_dir = self.config().agents_dir.join(".gateway");
        let promotion_violations = validate_promotion_record(
            Some(&gateway_dir),
            &promotion_artifact_id,
            promotion_role,
        );

        if !promotion_violations.is_empty() {
            let repair_enabled = self.config().response_validation.repair_enabled;
            let is_missing = promotion_violations
                .iter()
                .any(|v| v.rule == "promotion_record_missing");

            if is_missing && repair_enabled {
                let max_repair_rounds: usize = 2;
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(5000);

                for attempt in 1..=max_repair_rounds {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }

                    let repair_msg = build_repair_prompt(
                        &promotion_violations,
                        attempt,
                        max_repair_rounds,
                    );

                    tracing::info!(
                        target: "promotion_validation",
                        agent_id = %agent_id,
                        session_id = %result.session_id,
                        attempt,
                        "promotion.record repair attempt"
                    );

                    let repaired = match self
                        .respawn_from_checkpoint(
                            agent_id,
                            &result.session_id,
                            Some(&repair_msg),
                            source_agent_id,
                            workflow_id,
                            task_id,
                            &[],
                        )
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(
                                target: "promotion_validation",
                                agent_id = %agent_id,
                                error = %e,
                                "promotion.record repair: respawn failed"
                            );
                            break;
                        }
                    };

                    if repaired.suspended_for_approval.is_some()
                        || repaired.suspended_for_user_input
                    {
                        break;
                    }

                    let remaining = validate_promotion_record(
                        Some(&gateway_dir),
                        &promotion_artifact_id,
                        promotion_role,
                    );
                    result = repaired;

                    if remaining.is_empty() {
                        tracing::info!(
                            target: "promotion_validation",
                            agent_id = %agent_id,
                            session_id = %result.session_id,
                            attempt,
                            "promotion.record repair succeeded"
                        );
                        return Ok(result);
                    }

                    if remaining
                        .iter()
                        .any(|v| v.rule == "promotion_record_failed")
                    {
                        break;
                    }
                }
            }

            let summary: String = promotion_violations
                .iter()
                .map(|v| format!("[{}] {}", v.rule, v.message))
                .collect::<Vec<_>>()
                .join("; ");
            let hints: String = promotion_violations
                .iter()
                .map(|v| v.repair_hint.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow::anyhow!(
                "execution — {} Repair hints: {}",
                summary,
                hints
            ));
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{ArtifactMetadata, ContentFile};

    fn violation(rule: &str, msg: &str) -> ValidationViolation {
        ValidationViolation {
            rule: rule.into(),
            message: msg.into(),
            repair_hint: String::new(),
        }
    }

    #[test]
    fn violation_fingerprint_is_order_independent() {
        let a = vec![violation("schema", "missing field x"), violation("evidence", "no trace")];
        let b = vec![violation("evidence", "no trace"), violation("schema", "missing field x")];
        assert_eq!(violation_fingerprint(&a), violation_fingerprint(&b));
    }

    #[test]
    fn violation_fingerprint_distinguishes_progress() {
        // Same rule, different message (partial progress) => different fingerprint.
        let before = vec![violation("schema", "missing x and y")];
        let after = vec![violation("schema", "missing y")];
        assert_ne!(violation_fingerprint(&before), violation_fingerprint(&after));
        // A resolved violation (fewer) => different fingerprint.
        let none: Vec<ValidationViolation> = vec![];
        assert_ne!(violation_fingerprint(&before), violation_fingerprint(&none));
    }

    fn make_result(
        artifacts: Vec<ArtifactMetadata>,
        files: Vec<ContentFile>,
        reply: Option<&str>,
    ) -> SpawnResult {
        SpawnResult {
            agent_id: "test.agent".into(),
            session_id: "sess-1".into(),
            assistant_reply: reply.map(|s| s.to_string()),
            workflow_note: None,
            should_signal_background: false,
            artifacts,
            files,
            shared_knowledge: vec![],
            llm_usage: vec![],
            suspended_for_approval: None,
            suspended_for_user_input: false,
            suspended_for_child_wait: false,
        }
    }

    fn make_artifact(name: &str) -> ArtifactMetadata {
        ArtifactMetadata {
            id: format!("art-{}", name),
            name: name.to_string(),
            description: String::new(),
            files: vec![],
            entry_point: None,
            io: None,
        }
    }

    #[test]
    fn violations_to_feedback_events_maps_rules() {
        let violations = vec![
            ValidationViolation {
                rule: "required_artifacts".into(),
                message: "missing foo".into(),
                repair_hint: "create foo".into(),
            },
            ValidationViolation {
                rule: "output_schema".into(),
                message: "bad json".into(),
                repair_hint: "fix json".into(),
            },
        ];
        let events = violations_to_feedback_events(&violations);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            FeedbackEvent::Validation {
                rule: "required_artifacts".into(),
                field_path: None,
            }
        );
        assert_eq!(
            events[1].signature_key(),
            "validation:output_schema:*"
        );
    }

    #[test]
    fn empty_violations_yield_empty_feedback_events() {
        assert!(violations_to_feedback_events(&[]).is_empty());
    }

    #[test]
    fn test_required_artifacts_pass() {
        let p = autonoetic_types::agent::OutputPolicy {
            required_artifacts: vec!["report.md".into()],
            ..Default::default()
        };
        let r = make_result(vec![make_artifact("report.md")], vec![], Some("done"));
        assert!(validate_spawn_response(&r, None, &p, None).is_empty());
    }

    #[test]
    fn test_required_artifacts_fail() {
        let p = autonoetic_types::agent::OutputPolicy {
            required_artifacts: vec!["report.md".into(), "data.json".into()],
            ..Default::default()
        };
        let r = make_result(vec![make_artifact("report.md")], vec![], Some("done"));
        let v = validate_spawn_response(&r, None, &p, None);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "required_artifacts");
    }

    #[test]
    fn test_max_artifacts() {
        let p = autonoetic_types::agent::OutputPolicy {
            max_artifacts: Some(2),
            ..Default::default()
        };
        let r = make_result(
            vec![make_artifact("a"), make_artifact("b"), make_artifact("c")],
            vec![],
            None,
        );
        assert_eq!(validate_spawn_response(&r, None, &p, None).len(), 1);
    }

    #[test]
    fn test_prohibited_text() {
        let p = autonoetic_types::agent::OutputPolicy {
            prohibited_text_patterns: vec!["API_KEY".into()],
            ..Default::default()
        };
        let r = make_result(vec![], vec![], Some("key is API_KEY=xyz"));
        assert_eq!(validate_spawn_response(&r, None, &p, None).len(), 1);
    }

    #[test]
    fn test_output_schema_required_fields() {
        let p = autonoetic_types::agent::OutputPolicy::default();
        let schema = serde_json::json!({"required": ["status", "summary"]});
        let r = make_result(vec![], vec![], Some(r#"{"status": "ok"}"#));
        let v = validate_spawn_response(&r, Some(&schema), &p, None);
        assert!(v.iter().any(|v| v.message.contains("summary")));
    }

    #[test]
    fn test_output_schema_type_check() {
        let p = autonoetic_types::agent::OutputPolicy::default();
        let schema = serde_json::json!({
            "properties": {"count": {"type": "integer"}}
        });
        let r = make_result(vec![], vec![], Some(r#"{"count": "not_a_number"}"#));
        let v = validate_spawn_response(&r, Some(&schema), &p, None);
        assert!(v
            .iter()
            .any(|v| v.message.contains("count") && v.message.contains("integer")));
    }

    #[test]
    fn test_no_contract_passes() {
        let p = autonoetic_types::agent::OutputPolicy::default();
        let r = make_result(vec![], vec![], Some("anything"));
        assert!(validate_spawn_response(&r, None, &p, None).is_empty());
    }

    #[test]
    fn test_non_json_reply_fails_schema_validation() {
        let p = autonoetic_types::agent::OutputPolicy::default();
        let schema = serde_json::json!({"required": ["status"]});
        let r = make_result(vec![], vec![], Some("plain text reply"));
        let v = validate_spawn_response(&r, Some(&schema), &p, None);
        assert!(v.iter().any(|v| v.rule == "output_schema"));
        assert!(v.iter().any(|v| v.message.contains("valid JSON")));
    }

    #[test]
    fn test_missing_reply_fails_schema_validation() {
        let p = autonoetic_types::agent::OutputPolicy::default();
        let schema = serde_json::json!({"required": ["status"]});
        let r = make_result(vec![], vec![], None);
        let v = validate_spawn_response(&r, Some(&schema), &p, None);
        assert!(v.iter().any(|v| v.rule == "output_schema"));
        assert!(v.iter().any(|v| v.message.contains("no reply produced")));
    }

    #[test]
    fn test_max_total_size_mb_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();
        let store = crate::runtime::content_store::ContentStore::new(&gw).unwrap();
        let handle = store.write(&vec![b'x'; 2 * 1024 * 1024]).unwrap();

        let p = autonoetic_types::agent::OutputPolicy {
            max_total_size_mb: Some(1),
            ..Default::default()
        };
        let r = make_result(
            vec![],
            vec![ContentFile {
                name: "big.bin".into(),
                handle,
                alias: "deadbeef".into(),
                content_ref: "cnt_deadbeef".into(),
                sandbox_path: "/tmp/big.bin".into(),
            }],
            Some("done"),
        );
        let v = validate_spawn_response(&r, None, &p, Some(&gw));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "max_total_size_mb");
        assert!(v[0].message.contains("exceeds"));
    }

    #[test]
    fn test_text_pattern_regex_matching() {
        // Regex anchor: matches only word boundary, not substring
        let p = autonoetic_types::agent::OutputPolicy {
            prohibited_text_patterns: vec!["\\bsecret\\b".into()],
            ..Default::default()
        };
        let r_match = make_result(vec![], vec![], Some("this is a secret value"));
        let r_no_match = make_result(vec![], vec![], Some("secretive behavior"));
        assert_eq!(validate_spawn_response(&r_match, None, &p, None).len(), 1);
        assert!(validate_spawn_response(&r_no_match, None, &p, None).is_empty());
    }

    #[test]
    fn test_text_pattern_case_insensitive() {
        let p = autonoetic_types::agent::OutputPolicy {
            prohibited_text_patterns: vec!["API_KEY".into()],
            ..Default::default()
        };
        let r = make_result(vec![], vec![], Some("the api_key was leaked"));
        let v = validate_spawn_response(&r, None, &p, None);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn test_parse_output_policy_invalid_regex() {
        let metadata = serde_json::json!({
            "io": {
                "output_policy": {
                    "prohibited_text_patterns": ["[invalid regex"]
                }
            }
        });
        assert!(parse_output_policy(Some(&metadata)).is_err());
    }

    #[test]
    fn test_build_repair_prompt_contains_violations() {
        let violations = vec![ValidationViolation {
            rule: "required_artifacts".into(),
            message: "missing 'report.md'".into(),
            repair_hint: "create report.md".into(),
        }];
        let prompt = build_repair_prompt(&violations, 1, 2);
        assert!(prompt.contains("[GATEWAY_VALIDATION]"));
        assert!(prompt.contains("required_artifacts"));
        assert!(prompt.contains("Attempt 1/2"));
        assert!(prompt.contains("create report.md"));
    }

    #[test]
    fn test_violations_to_final_error_without_context() {
        let violations = vec![ValidationViolation {
            rule: "required_artifacts".into(),
            message: "missing 'x.md'".into(),
            repair_hint: "create x.md".into(),
        }];
        let e = violations_to_final_error(&violations, "sess-abc", false, None);
        let msg = e.to_string();
        assert!(msg.contains("required_artifacts"));
        assert!(msg.contains("repair_hint"));
        assert!(!msg.contains("sess-abc"));
    }

    #[test]
    fn test_violations_to_final_error_with_context() {
        let violations = vec![ValidationViolation {
            rule: "required_artifacts".into(),
            message: "missing 'x.md'".into(),
            repair_hint: "create x.md".into(),
        }];
        let e = violations_to_final_error(&violations, "sess-abc", true, None);
        let msg = e.to_string();
        assert!(msg.contains("sess-abc"));
        assert!(msg.contains("Repair hints"));
        assert!(msg.contains("create x.md"));
    }

    #[test]
    fn test_promotion_record_missing() {
        let temp = tempfile::tempdir().unwrap();
        let violations = validate_promotion_record(Some(temp.path()), "art_missing", "evaluator");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "promotion_record_missing");
        assert!(violations[0].message.contains("art_missing"));
        assert!(violations[0].repair_hint.contains("promotion_record"));
    }

    #[test]
    fn test_promotion_record_evaluator_pass() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::runtime::promotion_store::PromotionStore::new(temp.path()).unwrap();
        use autonoetic_types::promotion::PromotionRole;
        store
            .record_promotion(
                "art_good".to_string(),
                None,
                None,
                PromotionRole::Evaluator,
                "evaluator.default",
                true,
                vec![],
                Some("all good".to_string()),
                None,
            )
            .unwrap();

        let violations = validate_promotion_record(Some(temp.path()), "art_good", "evaluator");
        assert!(violations.is_empty());
    }

    #[test]
    fn test_promotion_record_evaluator_fail() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::runtime::promotion_store::PromotionStore::new(temp.path()).unwrap();
        use autonoetic_types::promotion::{Finding, FindingSeverity, PromotionRole};
        store
            .record_promotion(
                "art_bad".to_string(),
                None,
                None,
                PromotionRole::Evaluator,
                "evaluator.default",
                false,
                vec![Finding {
                    severity: FindingSeverity::Error,
                    description: "tests failed".to_string(),
                    evidence: None,
                }],
                None,
                None,
            )
            .unwrap();

        let violations = validate_promotion_record(Some(temp.path()), "art_bad", "evaluator");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "promotion_record_failed");
        assert!(violations[0].message.contains("pass=false"));
        assert!(violations[0].message.contains("tests failed"));
    }

    #[test]
    fn test_promotion_record_auditor_fail() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::runtime::promotion_store::PromotionStore::new(temp.path()).unwrap();
        use autonoetic_types::promotion::{Finding, FindingSeverity, PromotionRole};
        store
            .record_promotion(
                "art_audit".to_string(),
                None,
                None,
                PromotionRole::Auditor,
                "auditor.default",
                false,
                vec![Finding {
                    severity: FindingSeverity::Critical,
                    description: "security risk".to_string(),
                    evidence: Some("found network access".to_string()),
                }],
                None,
                None,
            )
            .unwrap();

        let violations = validate_promotion_record(Some(temp.path()), "art_audit", "auditor");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "promotion_record_failed");
        assert!(violations[0].message.contains("security risk"));
    }

    #[test]
    fn test_promotion_record_no_gateway_dir() {
        let violations = validate_promotion_record(None, "art_x", "evaluator");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "promotion_record");
    }

    #[test]
    fn test_strip_json_code_fence() {
        let input = "```json\n{\"status\": \"pass\"}\n```";
        let stripped = strip_markdown_code_fences(input);
        assert_eq!(stripped, "{\"status\": \"pass\"}");
    }

    #[test]
    fn test_strip_plain_code_fence() {
        let input = "```\n{\"status\": \"pass\"}\n```";
        let stripped = strip_markdown_code_fences(input);
        assert_eq!(stripped, "{\"status\": \"pass\"}");
    }

    #[test]
    fn test_strip_code_fence_uppercase() {
        let input = "```JSON\n{\"status\": \"pass\"}\n```";
        let stripped = strip_markdown_code_fences(input);
        assert_eq!(stripped, "{\"status\": \"pass\"}");
    }

    #[test]
    fn test_no_strip_bare_json() {
        let input = "{\"status\": \"pass\"}";
        let stripped = strip_markdown_code_fences(input);
        assert_eq!(&*stripped, input);
    }

    #[test]
    fn test_output_schema_passes_with_code_fence() {
        let p = autonoetic_types::agent::OutputPolicy::default();
        let schema = serde_json::json!({"required": ["status"]});
        let r = make_result(
            vec![],
            vec![],
            Some("```json\n{\"status\": \"pass\"}\n```"),
        );
        let v = validate_spawn_response(&r, Some(&schema), &p, None);
        assert!(v.is_empty(), "expected no violations, got: {:?}", v);
    }

    #[test]
    fn test_output_schema_passes_with_prose_and_code_fence() {
        let p = autonoetic_types::agent::OutputPolicy::default();
        let schema = serde_json::json!({"required": ["status"]});
        let r = make_result(
            vec![],
            vec![],
            Some("## Result\n\n```json\n{\"status\": \"pass\"}\n```"),
        );
        let v = validate_spawn_response(&r, Some(&schema), &p, None);
        assert!(v.is_empty(), "expected no violations, got: {:?}", v);
    }

    #[test]
    fn test_output_schema_auditor_real_world_deepseek() {
        let p = autonoetic_types::agent::OutputPolicy::default();
        let schema = serde_json::json!({
            "required": ["status", "auditor_pass", "findings"],
            "properties": {
                "status": {"type": "string"},
                "auditor_pass": {"type": "boolean"},
                "findings": {"type": "array"}
            }
        });
        let r = make_result(
            vec![],
            vec![],
            Some("```json\n{\n  \"status\": \"pass\",\n  \"auditor_pass\": true,\n  \"security_risk\": \"low\",\n  \"findings\": []\n}\n```"),
        );
        let v = validate_spawn_response(&r, Some(&schema), &p, None);
        assert!(v.is_empty(), "expected no violations, got: {:?}", v);
    }

    #[test]
    fn test_no_strip_pure_prose() {
        let input = "The artifact contains only moltbook_agent.py — no test files.";
        let stripped = strip_markdown_code_fences(input);
        assert_eq!(&*stripped, input);
    }

    #[test]
    fn test_output_schema_auditor_prose_header_deepseek() {
        let p = autonoetic_types::agent::OutputPolicy::default();
        let schema = serde_json::json!({
            "required": ["status", "auditor_pass", "findings"],
            "properties": {
                "status": {"type": "string"},
                "auditor_pass": {"type": "boolean"},
                "findings": {"type": "array"}
            }
        });
        let r = make_result(
            vec![],
            vec![],
            Some("## Audit Verdict\n\n```json\n{\n  \"status\": \"pass\",\n  \"auditor_pass\": true,\n  \"security_risk\": \"low\",\n  \"findings\": []\n}\n```"),
        );
        let v = validate_spawn_response(&r, Some(&schema), &p, None);
        assert!(v.is_empty(), "expected no violations, got: {:?}", v);
    }

    #[test]
    fn reply_is_delegated_detection() {
        assert!(reply_is_delegated(Some(r#"{"status":"delegated","summary":"x"}"#)));
        // truthful non-delegated status
        assert!(!reply_is_delegated(Some(r#"{"status":"ok"}"#)));
        // non-JSON / no status / none → not delegated
        assert!(!reply_is_delegated(Some("just prose")));
        assert!(!reply_is_delegated(Some(r#"{"summary":"no status"}"#)));
        assert!(!reply_is_delegated(None));
    }

    #[test]
    fn reply_claimed_plan_id_detection() {
        // top-level plan_id
        assert_eq!(
            reply_claimed_plan_id(Some(r#"{"status":"awaiting_approval","plan_id":"plan-a1b2c3d4"}"#)).as_deref(),
            Some("plan-a1b2c3d4")
        );
        // nested under result
        assert_eq!(
            reply_claimed_plan_id(Some(r#"{"status":"ok","result":{"plan_id":"plan-xyz"}}"#)).as_deref(),
            Some("plan-xyz")
        );
        // JSON wrapped in a markdown code fence is still detected
        assert_eq!(
            reply_claimed_plan_id(Some("```json\n{\"status\":\"awaiting_approval\",\"plan_id\":\"plan-fenced\"}\n```")).as_deref(),
            Some("plan-fenced")
        );
        // no plan_id (e.g. planner.default) → None, guard never fires
        assert_eq!(reply_claimed_plan_id(Some(r#"{"status":"ok","summary":"done"}"#)), None);
        // empty / prose / none
        assert_eq!(reply_claimed_plan_id(Some(r#"{"plan_id":"  "}"#)), None);
        assert_eq!(reply_claimed_plan_id(Some("just prose")), None);
        assert_eq!(reply_claimed_plan_id(None), None);
    }

    #[test]
    fn fabricated_plan_id_violation_shape() {
        let v = fabricated_plan_id_violation("plan-a1b2c3d4");
        assert_eq!(v.rule, "unknown_plan_id");
        assert!(v.message.contains("plan-a1b2c3d4"));
        assert!(v.repair_hint.contains("planframe_propose"));
    }

    #[test]
    fn delegated_without_spawn_violation_shape() {
        assert_eq!(delegated_without_spawn_violation().rule, "delegated_without_spawn");
    }

    #[test]
    fn claim_delegated_unverified_when_not_spawn_capable() {
        let ctx = ClaimCtx {
            assistant_reply: Some(r#"{"status":"delegated"}"#),
            workflow_id: None,
            task_id: None,
            gateway_store: None,
            config: None,
            agent_id: "agent",
            session_id: "sess",
            gateway_dir: std::path::Path::new("/tmp"),
            agent_is_spawn_capable: false,
        };
        assert_eq!(ClaimKind::Delegated.verify(&ctx), ClaimVerdict::Unverified);
    }

    #[test]
    fn claim_delegated_unverified_when_no_delegated_status() {
        let ctx = ClaimCtx {
            assistant_reply: Some(r#"{"status":"ok"}"#),
            workflow_id: None,
            task_id: None,
            gateway_store: None,
            config: None,
            agent_id: "agent",
            session_id: "sess",
            gateway_dir: std::path::Path::new("/tmp"),
            agent_is_spawn_capable: true,
        };
        assert_eq!(ClaimKind::Delegated.verify(&ctx), ClaimVerdict::Unverified);
    }

    #[test]
    fn claim_delegated_fabricated_when_no_workflow() {
        let ctx = ClaimCtx {
            assistant_reply: Some(r#"{"status":"delegated"}"#),
            workflow_id: None,
            task_id: None,
            gateway_store: None,
            config: None,
            agent_id: "agent",
            session_id: "sess",
            gateway_dir: std::path::Path::new("/tmp"),
            agent_is_spawn_capable: true,
        };
        assert!(
            matches!(ClaimKind::Delegated.verify(&ctx), ClaimVerdict::Fabricated(_)),
            "delegated status with no workflow should be fabricated"
        );
    }

    #[test]
    fn claim_plan_id_unverified_when_no_plan_id() {
        let ctx = ClaimCtx {
            assistant_reply: Some(r#"{"status":"ok"}"#),
            workflow_id: None,
            task_id: None,
            gateway_store: None,
            config: None,
            agent_id: "agent",
            session_id: "sess",
            gateway_dir: std::path::Path::new("/tmp"),
            agent_is_spawn_capable: false,
        };
        assert_eq!(ClaimKind::PlanId.verify(&ctx), ClaimVerdict::Unverified);
    }

    #[test]
    fn claim_plan_id_fabricated_when_plan_missing() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(temp.path()).unwrap();
        let ctx = ClaimCtx {
            assistant_reply: Some(r#"{"status":"awaiting_approval","plan_id":"plan-missing"}"#),
            workflow_id: None,
            task_id: None,
            gateway_store: Some(&store),
            config: None,
            agent_id: "agent",
            session_id: "sess",
            gateway_dir: temp.path(),
            agent_is_spawn_capable: false,
        };
        assert_eq!(
            ClaimKind::PlanId.verify(&ctx),
            ClaimVerdict::Fabricated("plan-missing".into())
        );
    }

    #[test]
    fn claim_plan_id_ok_when_plan_exists() {
        use autonoetic_types::plan_frame::{PlanFrame, PlanStatus};

        let temp = tempfile::tempdir().unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(temp.path()).unwrap();
        let plan = PlanFrame {
            plan_id: "plan-real".into(),
            version: 1,
            parent_version: None,
            workflow_id: "wf-1".into(),
            root_session_id: "root".into(),
            title: "t".into(),
            objective: "o".into(),
            status: PlanStatus::AwaitingApproval,
            steps: vec![],
            validation_policy: Default::default(),
            capability_envelope: vec![],
            approved_by: None,
            approved_at: None,
            created_by_agent_id: "agent".into(),
            reason: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: None,
        };
        store.save_plan_frame(&plan).unwrap();

        let ctx = ClaimCtx {
            assistant_reply: Some(r#"{"status":"awaiting_approval","plan_id":"plan-real"}"#),
            workflow_id: None,
            task_id: None,
            gateway_store: Some(&store),
            config: None,
            agent_id: "agent",
            session_id: "sess",
            gateway_dir: temp.path(),
            agent_is_spawn_capable: false,
        };
        assert_eq!(ClaimKind::PlanId.verify(&ctx), ClaimVerdict::Ok);
    }

    #[test]
    fn reconcile_claims_returns_fabricated_plan_id_violation() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(temp.path()).unwrap();
        let ctx = ClaimCtx {
            assistant_reply: Some(r#"{"status":"awaiting_approval","plan_id":"plan-fake"}"#),
            workflow_id: None,
            task_id: None,
            gateway_store: Some(&store),
            config: None,
            agent_id: "agent",
            session_id: "sess",
            gateway_dir: temp.path(),
            agent_is_spawn_capable: false,
        };
        let violations = reconcile_claims(&ctx);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "unknown_plan_id");
        assert!(violations[0].message.contains("plan-fake"));
    }

    // ──────────────────────────────────────────────────────────────────────
    // RFC C — advisory child→parent result claim reconciliation
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn advisory_child_result_summary_empty_is_unverified() {
        let temp = tempfile::tempdir().unwrap();
        let violations = advisory_reconcile_child_result_summary(
            None,
            "child-sess",
            "parent-sess",
            "agent",
            temp.path(),
            None,
            None,
        );
        assert!(violations.is_empty());
    }

    #[test]
    fn advisory_child_result_summary_short_is_unverified() {
        let temp = tempfile::tempdir().unwrap();
        let violations = advisory_reconcile_child_result_summary(
            Some("ok"),
            "child-sess",
            "parent-sess",
            "agent",
            temp.path(),
            None,
            None,
        );
        assert!(violations.is_empty(), "trivial summaries should not be flagged");
    }

    #[test]
    fn advisory_child_result_summary_finds_fabricated_plan_id() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(temp.path()).unwrap();
        let violations = advisory_reconcile_child_result_summary(
            Some(r#"{"status":"awaiting_approval","plan_id":"plan-fake"}"#),
            "child-sess",
            "parent-sess",
            "agent",
            temp.path(),
            Some(&store),
            None,
        );
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "unknown_plan_id");
    }

    #[test]
    fn advisory_child_result_summary_delegated_not_flagged() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(temp.path()).unwrap();
        let violations = advisory_reconcile_child_result_summary(
            Some(r#"{"status":"delegated","summary":"handed off"}"#),
            "child-sess",
            "parent-sess",
            "agent",
            temp.path(),
            Some(&store),
            None,
        );
        assert!(violations.is_empty(), "advisory path must not flag delegated claims; got: {violations:?}");
    }

    // ──────────────────────────────────────────────────────────────────────
    // strip_think_blocks
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn strip_think_blocks_removes_closed_block() {
        let input = "<think>reasoning here</think>{\"status\":\"ok\"}";
        assert_eq!(strip_think_blocks(input).as_ref(), "{\"status\":\"ok\"}");
    }

    #[test]
    fn strip_think_blocks_removes_unclosed_block() {
        let input = "hello<think>still thinking...";
        assert_eq!(strip_think_blocks(input).as_ref(), "hello");
    }

    #[test]
    fn strip_think_blocks_preserves_text_without_think_tags() {
        let input = "{\"status\":\"ok\",\"summary\":\"hello\"}";
        assert_eq!(strip_think_blocks(input).as_ref(), input);
    }

    #[test]
    fn strip_think_blocks_handles_multiple_blocks() {
        let input = "<think>part 1</think>hello<think>part 2</think> world";
        assert_eq!(strip_think_blocks(input).as_ref(), "hello world");
    }

    #[test]
    fn strip_think_blocks_reply_is_delegated_works_with_think_prefix() {
        let reply = "<think>let me delegate</think>{\"status\":\"delegated\"}";
        assert!(reply_is_delegated(Some(reply)));
    }

    #[test]
    fn strip_think_blocks_reply_claimed_plan_id_works_with_think_prefix() {
        let reply = "<think>planning</think>{\"status\":\"ok\",\"plan_id\":\"plan-abc\"}";
        assert_eq!(reply_claimed_plan_id(Some(reply)).as_deref(), Some("plan-abc"));
    }
}
