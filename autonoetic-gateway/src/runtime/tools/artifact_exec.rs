use crate::llm::ToolDefinition;
use crate::policy::{PolicyDecision, PolicyEngine, SecurityAnalyzer};
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::approved_exec_cache::{
    compute_fingerprint, normalize_targets, ApprovedExecCache,
};
use crate::runtime::remote_access::{
    approval_remote_operator_suffix, classify_network_coverage, default_remote_access_detector, NetworkCoverage,
};
use crate::runtime::tools::{
    build_approval_details,
    promotion::{
        manifest_may_exec_artifact_in_promotion_gate, manifest_may_record_promotion_verdicts,
        manifest_sandbox_allows_tool,
    },
    CredentialEnvMapping, NativeTool, NativeToolRegistry,
};
use crate::sandbox::{SandboxDriverKind, SandboxMount, SandboxRunner};
use autonoetic_types::agent::{AgentManifest, ScriptInputMode};
use autonoetic_types::background::{
    ApprovalDecision, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::ToolError;
use secrecy::ExposeSecret;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ArtifactExecTool));
}

#[derive(Debug, Deserialize)]
struct ArtifactExecArgs {
    artifact_ref: String,
    entrypoint: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    intent: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::runtime::tools::deserialize_string_map_values_lenient"
    )]
    env: std::collections::HashMap<String, String>,
    /// Payload delivered to the script via `autonoetic_sdk.load_input()` —
    /// the gateway serializes it to the `AUTONOETIC_INPUT` env var the SDK
    /// reads. Use this for scripts that call `load_input()`; use `args` only
    /// for scripts that read argv directly. Mutually exclusive with
    /// `env.AUTONOETIC_INPUT` (supplying both is rejected as `input_env_conflict`).
    #[serde(default)]
    input: Option<serde_json::Value>,
    #[serde(default)]
    approval_ref: Option<String>,
    #[serde(default)]
    deployment_ticket: Option<String>,
    #[serde(default)]
    credential_env: Option<Vec<CredentialEnvMapping>>,
    /// Optional fixture set ref to replay a recorded fixture set.
    #[serde(default)]
    fixture_set_ref: Option<String>,
}

pub struct ArtifactExecTool;

const ARTIFACT_APPROVAL_SUMMARY_CMD_MAX: usize = 260;
const ARTIFACT_APPROVAL_INTENT_PREVIEW_MAX: usize = 280;
const ARTIFACT_APPROVAL_PATTERN_APPEND_MAX: usize = 8;

fn truncate_unicode_display(s: &str, max_chars: usize) -> String {
    let t = s.trim();
    let n = t.chars().count();
    if n <= max_chars {
        t.to_string()
    } else {
        let keep = max_chars.saturating_sub(3);
        format!("{}...", t.chars().take(keep).collect::<String>())
    }
}

fn artifact_exec_approval_summary_line(
    agent_id: &str,
    artifact_ref: &str,
    entrypoint: &str,
    command: &str,
    intent: Option<&str>,
) -> String {
    let cmd = truncate_unicode_display(command, ARTIFACT_APPROVAL_SUMMARY_CMD_MAX);
    match intent.map(str::trim).filter(|s| !s.is_empty()) {
        Some(i) => {
            let ip = truncate_unicode_display(i, ARTIFACT_APPROVAL_INTENT_PREVIEW_MAX);
            format!("Artifact exec ({agent_id}): {ip} — `{artifact_ref}:{entrypoint}` · `{cmd}`")
        }
        None => format!("Artifact exec ({agent_id}): `{artifact_ref}:{entrypoint}` · `{cmd}`"),
    }
}

fn artifact_exec_approval_operator_reason(
    artifact_ref: &str,
    artifact_id: &str,
    entrypoint: &str,
    command: &str,
    intent: Option<&str>,
    remote_summary: &str,
    remote_suffix: &str,
    patterns: &[crate::runtime::remote_access::DetectedPattern],
) -> String {
    let mut sections = vec![
        format!("What will run:\n{}", command.trim()),
        format!(
            "Artifact target:\nref `{}` → id `{}`\nentrypoint `{}`",
            artifact_ref, artifact_id, entrypoint
        ),
        format!(
            "Analyzed for network/reachable APIs: `{}` inside artifact `{}`",
            entrypoint, artifact_id
        ),
    ];
    if let Some(i) = intent.map(str::trim).filter(|s| !s.is_empty()) {
        sections.push(format!("Agent-stated purpose:\n{}", i));
    }
    let mut trigger = format!(
        "Why approval is required:\n{}{}",
        remote_summary.trim(),
        remote_suffix.trim_end()
    );
    if !patterns.is_empty() {
        trigger.push_str("\n\nStatic analysis cues:");
        for p in patterns.iter().take(ARTIFACT_APPROVAL_PATTERN_APPEND_MAX) {
            let line = p
                .line_number
                .map(|n| format!("line {n}"))
                .unwrap_or_else(|| "line ?".into());
            let excerpt = truncate_unicode_display(&p.pattern, 120);
            trigger.push_str(&format!(
                "\n- [{}] [{}] `{}` — {}",
                line,
                p.category,
                excerpt,
                p.reason.trim()
            ));
        }
        if patterns.len() > ARTIFACT_APPROVAL_PATTERN_APPEND_MAX {
            trigger.push_str(&format!(
                "\n- … (+{} more pattern(s))",
                patterns.len() - ARTIFACT_APPROVAL_PATTERN_APPEND_MAX
            ));
        }
    }
    sections.push(trigger);
    sections.join("\n\n")
}

impl NativeTool for ArtifactExecTool {
    fn name(&self) -> &'static str {
        "artifact_exec"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        let has_artifact_exec = manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ArtifactExecution));
        if has_artifact_exec {
            return true;
        }

        let has_eval = manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::Evaluation { .. }));
        if has_eval {
            // Evaluation alone is too broad (auditor/static_evaluator use it for
            // promotion_record). Require explicit SandboxFunctions listing of
            // artifact_exec. Note: manifest_may_exec_artifact_in_promotion_gate
            // always returns false here because it checks !has_broad_cap, and
            // Evaluation IS a broad cap — so we only need the sandbox check.
            return manifest_sandbox_allows_tool(manifest, "artifact_exec");
        }

        manifest_may_exec_artifact_in_promotion_gate(manifest)
    }

    fn guidance(&self) -> Vec<crate::runtime::guidance::GuidanceBlock> {
        // Same approval-continuation block as sandbox_exec (deduped by id at
        // compose), so artifact_exec-only agents still get it (#466).
        vec![crate::runtime::tools::sandbox::exec_approval_continuation_block()]
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Execute an artifact entrypoint in a sandbox. Unlike sandbox_exec, this tool runs remote-access analysis against the artifact's source files (not the shell command string) and binds approval reuse to the artifact identity. Use this for transient validation, smoke tests, and ad hoc runs of built artifacts. For reusable capabilities, prefer creating a script-agent revision instead.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_ref": {
                        "type": "string",
                        "description": "Artifact ref to execute (e.g., 'ar.aabb1234ef56')"
                    },
                    "entrypoint": {
                        "type": "string",
                        "description": "Entrypoint file within the artifact (e.g., 'main.py')"
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Arguments to pass to the entrypoint"
                    },
                    "intent": {
                        "type": "string",
                        "description": "Short human explanation for the operator (recommended when this run may need approval): what the execution does and why it is safe/necessary."
                    },
                    "env": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "Environment variables to set in the sandbox. Note: to deliver payload to a script using `autonoetic_sdk.load_input()`, prefer the `input` field over setting AUTONOETIC_INPUT here."
                    },
                    "input": {
                        "description": "Payload delivered to the script via `autonoetic_sdk.load_input()`. Pass any JSON value (number, string, object, array) — the gateway serializes it to the AUTONOETIC_INPUT env var the SDK reads. Use this for scripts that call load_input(); use `args` only for scripts that read argv. Mutually exclusive with env.AUTONOETIC_INPUT."
                    },
                    "credential_env": {
                        "type": "array",
                        "description": "Inject vault-stored credentials as environment variables into the sandbox. The gateway resolves the secret server-side — it never appears in tool arguments or responses. Use credential_id from credential_check output.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "credential_id": { "type": "string", "description": "Credential ID (from credential_check or delegated by planner)" },
                                "env_var": { "type": "string", "description": "Environment variable name to inject (e.g., 'API_KEY')" }
                            },
                            "required": ["credential_id", "env_var"]
                        }
                    },
                    "approval_ref": {
                        "type": "string",
                        "description": "Approval request ID from a previous approval_required response"
                    },
                    "deployment_ticket": {
                        "type": "string",
                        "description": "Deployment ticket from artifact_prepare. When provided, remote-access approval and credential injection are resolved from the ticket — no separate approval_ref or credential_env needed."
                    }
                },
                "required": ["artifact_ref", "entrypoint"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: ArtifactExecArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        // `input` and `env.AUTONOETIC_INPUT` both target the same SDK contract
        // (load_input()). Allowing both would require the gateway to silently
        // pick one — exactly the LLM-judgment trap P-5.11 forbids. Reject
        // mechanically and name the rule so the caller can self-correct.
        if args.input.is_some() && args.env.contains_key(crate::runtime::tools::AUTONOETIC_INPUT_ENV) {
            return Ok(ToolError::conflict(
                "artifact_exec received both `input` and `env.AUTONOETIC_INPUT` — these target the same SDK contract (load_input()).",
                Some("Pass payload via `input` (preferred) OR `env.AUTONOETIC_INPUT`, not both."),
            )
            .with_code("input_env_conflict")
            .to_error_response());
        }

        let gw_dir = gateway_dir
            .ok_or_else(|| anyhow::anyhow!("artifact.exec requires a gateway directory"))?;

        // Resolve artifact_ref → artifact_id
        let artifact_id = if let Some(store) = &gateway_store {
            let sid = session_id.unwrap_or_default();
            crate::runtime::tools::artifact::resolve_artifact_ref_or_canonical(
                &args.artifact_ref,
                sid,
                store,
                gw_dir,
            )?
            .artifact_id
        } else {
            return Ok(ToolError::resource(
                "artifact_exec requires GatewayStore to be configured",
                None::<String>,
            )
            .to_error_response());
        };

        if let Some(ticket_id) = &args.deployment_ticket {
            if let Some(store) = &gateway_store {
                if let Some(ticket) =
                    crate::runtime::tools::artifact_prepare::resolve_deployment_ticket(
                        store, ticket_id,
                    )?
                {
                    if !ticket.approved_domains.is_empty() {
                        tracing::info!(
                            target: "artifact_exec",
                            ticket_id = %ticket_id,
                            domains = ?ticket.approved_domains,
                            "Deployment ticket resolved — skipping approval"
                        );
                    }
                    return execute_with_ticket(
                        manifest,
                        policy,
                        agent_dir,
                        gw_dir,
                        &args,
                        &ticket,
                        config,
                        gateway_store,
                        session_id,
                    );
                } else {
                    return Ok(ToolError::resource(
                        format!("deployment_ticket '{}' not found or expired", ticket_id),
                        Some("Re-run artifact.prepare to get a new deployment ticket.".to_string()),
                    )
                    .to_error_response());
                }
            }
        }

        let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
        let bundle = artifact_store.inspect(&artifact_id)?;

        let entrypoint = &args.entrypoint;
        anyhow::ensure!(
            bundle.files.iter().any(|f| f.name == *entrypoint),
            "entrypoint '{}' not found in artifact '{}'",
            entrypoint,
            artifact_id
        );

        let resolved_files = artifact_store.resolve_files(&artifact_id)?;
        let artifact_input_mode = artifact_script_input_mode(&resolved_files);

        let mut artifact_code = String::new();
        let mut workspace_files: Vec<(String, String)> = Vec::new();
        for (name, content) in &resolved_files {
            if let Ok(text) = std::str::from_utf8(content) {
                if name == entrypoint {
                    artifact_code = text.to_string();
                }
                workspace_files.push((name.clone(), text.to_string()));
            }
        }

        anyhow::ensure!(
            !artifact_code.is_empty(),
            "entrypoint '{}' could not be read as text from artifact '{}'",
            entrypoint,
            artifact_id
        );

        let command = build_command(entrypoint, &args.args);
        let mut approval_validated_for_command = false;
        if let Some(approval_ref) = args.approval_ref.as_ref() {
            if let Some(store) = &gateway_store {
                if let Some(req) = store.get_approval(approval_ref)? {
                    if req.status != Some(ApprovalStatus::Approved) {
                        return Err(autonoetic_types::tool_error::tagged::Tagged::validation(
                            anyhow::anyhow!(
                                "approval_ref '{}' references a request that is not approved",
                                approval_ref
                            ),
                        )
                        .into());
                    }
                    let decision = req.into_decision()?;
                    validate_approval_ref_context(&decision, &manifest.agent.id, session_id)?;
                    approval_validated_for_command = true;

                    let normalized_targets =
                        normalize_targets_from_artifact(&artifact_code, &workspace_files);
                    let fingerprint = compute_fingerprint(
                        &manifest.agent.id,
                        &normalized_targets,
                        &artifact_code,
                        Some(&format!("exec:{}", command)),
                        &manifest.capabilities,
                    );
                    if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                        if cache.find(&fingerprint, 0).is_none() {
                            let entry = crate::runtime::approved_exec_cache::ApprovedExecEntry {
                                fingerprint: fingerprint.clone(),
                                agent_id: manifest.agent.id.clone(),
                                remote_targets: normalized_targets,
                                code_content: artifact_code.clone(),
                                approval_request_id: approval_ref.clone(),
                                approved_at: chrono::Utc::now().to_rfc3339(),
                                approved_by: "operator".to_string(),
                                last_used_at: chrono::Utc::now().to_rfc3339(),
                            };
                            let _ = cache.record(entry);
                        }
                    }
                } else {
                    return Err(autonoetic_types::tool_error::tagged::Tagged::validation(
                        anyhow::anyhow!("approval_ref '{}' not found in store", approval_ref),
                    )
                    .into());
                }
            }
        }

        let decision = if manifest_may_exec_artifact_in_promotion_gate(manifest) {
            promotion_gate_artifact_command_decision(&command)
        } else {
            artifact_command_decision(&command)
        };
        if !decision.is_allowed() {
            return Err(
                autonoetic_types::tool_error::tagged::Tagged::permission_with_rules(
                    anyhow::anyhow!(decision.explain_shell_denial("Artifact execution")),
                    decision
                        .enforced_rules
                        .into_iter()
                        .map(|rule| rule.to_string())
                        .collect(),
                )
                .into(),
            );
        }

        let remote_analysis =
            default_remote_access_detector()
                .analyze_code_with_workspace(&artifact_code, &workspace_files);

        let agent_has_network_access = manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { .. }));

        if agent_has_network_access
            && remote_analysis.requires_approval
            && !approval_validated_for_command
        {
            tracing::info!(
                target: "artifact_exec",
                agent_id = %manifest.agent.id,
                patterns = ?remote_analysis.detected_patterns,
                "Agent has NetworkAccess capability — auto-approving"
            );
            approval_validated_for_command = true;
        }

        // Phase 1.D: preapproved remote_access bypass.
        // If the agent declares remote_access.approval_mode=preapproved AND
        // (has NetworkAccess OR sandbox_network is Sealed/Recording), auto-approve.
        // The sealed proxy intercepts HTTP calls for sealed-network agents,
        // so NetworkAccess capability is not required in that case.
        if !approval_validated_for_command && remote_analysis.requires_approval {
            let declared_remote_access =
                crate::runtime::network_policy::load_manifest_remote_access_declaration(agent_dir);
            // #1106: any + preapproved + non-wildcard capability is a silent
            // any-host auto-approval — fail shut unless the sealed proxy is
            // the network control anyway.
            let sealed_or_recording = matches!(
                manifest.sandbox_network,
                autonoetic_types::agent::SandboxNetworkPolicy::Sealed
                    | autonoetic_types::agent::SandboxNetworkPolicy::Recording
            );
            if !sealed_or_recording {
                if let Some(decl) = declared_remote_access.as_ref() {
                    if let Err(violation) =
                        crate::runtime::network_policy::validate_any_preapproval_shape(
                            manifest, decl,
                        )
                    {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "error_type": violation.error_type,
                            "message": violation.message,
                            "repair_hint": violation.repair_hint,
                        })
                        .to_string());
                    }
                }
            }
            let remote_approval_mode = declared_remote_access
                .as_ref()
                .map(|d| d.approval_mode)
                .unwrap_or(autonoetic_types::agent::RemoteAccessApprovalMode::Required);
            if matches!(
                remote_approval_mode,
                autonoetic_types::agent::RemoteAccessApprovalMode::Preapproved
            ) && (agent_has_network_access
                || matches!(
                    manifest.sandbox_network,
                    autonoetic_types::agent::SandboxNetworkPolicy::Sealed
                        | autonoetic_types::agent::SandboxNetworkPolicy::Recording
                ))
            {
                tracing::info!(
                    target: "artifact_exec",
                    agent_id = %manifest.agent.id,
                    sandbox_network = ?manifest.sandbox_network,
                    patterns = ?remote_analysis.detected_patterns,
                    "remote_access.approval_mode is preapproved — auto-approving remote access patterns"
                );
                approval_validated_for_command = true;
            }
        }

        // Promotion-verdict roles (unit_test_runner, evaluators, auditor) run in
        // a physically network-isolated sandbox under promotion_gate_overrides()
        // (force_network_off). When the configured driver *guarantees* the run is
        // offline (see SandboxDriverKind::guarantees_network_off — bubblewrap with
        // force_network_off, docker `--network none`, wasm WASI-no-sockets), we do
        // NOT statically pre-deny when RemoteAccessAnalyzer merely *detects* a
        // network import: the deterministic suite is run inside the isolated
        // sandbox. Mocked tests pass; tests that genuinely reach the network fail
        // at runtime with a ConnectionError, which the verdict role reports as
        // `unable_to_evaluate`. The detected patterns are surfaced as informational
        // findings on the run output, not a hard block — so a service that imports
        // `urllib` but mocks the HTTP caller is no longer falsely blocked.
        //
        // Drivers that cannot guarantee the run is offline (today: microvm, whose
        // NIC is controlled by the operator firecracker config) keep the
        // deterministic-without-network pre-deny (P-3.10).
        let promotion_verdict_role = manifest_may_record_promotion_verdicts(manifest);
        let promotion_isolated_run = promotion_run_is_network_isolated(manifest);
        let informational_remote_patterns = if promotion_isolated_run {
            remote_analysis.detected_patterns.clone()
        } else {
            Vec::new()
        };

        if remote_analysis.requires_approval
            && !approval_validated_for_command
            && !promotion_isolated_run
        {
            if promotion_verdict_role {
                return Ok(serde_json::json!({
                    "ok": false,
                    "exit_code": null,
                    "stdout": "",
                    "stderr": "Promotion-gate execution (P-3.10): artifact test run requires network access and the configured sandbox driver cannot guarantee network isolation. Unit tests must be deterministic without live network.",
                    "promotion_gate_network_denied": true,
                    "recommendation": "unable_to_evaluate",
                    "detected_patterns": remote_analysis.detected_patterns,
                })
                .to_string());
            }
            let detected_patterns = remote_analysis.detected_patterns.clone();
            let concrete_targets = normalize_targets(&detected_patterns);
            let coverage = classify_network_coverage(&detected_patterns, concrete_targets.clone());

            // Pre-check: exec cache for concrete targets
            let mut pre_validated = false;
            let fingerprint_for_backfill: Option<String> = match &coverage {
                NetworkCoverage::Concrete { targets } => {
                    if let Some(gw_dir) = gateway_dir {
                        let fingerprint = compute_fingerprint(
                            &manifest.agent.id,
                            targets,
                            &artifact_code,
                            Some(&format!("exec:{}", command)),
                            &manifest.capabilities,
                        );
                        if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                            if let Some(_entry) = cache.find(
                                &fingerprint,
                                crate::runtime::approved_exec_cache::cache_ttl_secs(config),
                            ) {
                                tracing::info!(
                                    target: "artifact_exec",
                                    fingerprint = %fingerprint,
                                    "Cache hit: skipping approval"
                                );
                                let _ = cache.update_last_used(&fingerprint);
                                pre_validated = true;
                            }
                        }
                        Some(fingerprint)
                    } else {
                        None
                    }
                }
                _ => None,
            };

            if pre_validated {
                approval_validated_for_command = true;
            } else if let Some(cfg) = config {
                let summary = artifact_exec_approval_summary_line(
                    &manifest.agent.id,
                    &args.artifact_ref,
                    &entrypoint,
                    &command,
                    args.intent.as_deref(),
                );
                let remote_hint_suffix =
                    approval_remote_operator_suffix(&concrete_targets, &detected_patterns);
                let action = ScheduledAction::SandboxExec {
                    command: command.clone(),
                    dependencies: None,
                    requires_approval: true,
                    evidence_ref: None,
                    detected_hosts: Some(concrete_targets.clone()),
                    intent: args.intent.clone(),
                };
                let reason = artifact_exec_approval_operator_reason(
                    &args.artifact_ref,
                    &artifact_id,
                    &entrypoint,
                    &command,
                    args.intent.as_deref(),
                    &remote_analysis.summary,
                    &remote_hint_suffix,
                    &detected_patterns,
                );

                if let Some(store) = &gateway_store {
                    let gate = crate::runtime::human_gate::GateService::new(store.clone());
                    let gate_result = gate.check(
                        crate::runtime::human_gate::GateRequest {
                            kind: crate::runtime::human_gate::GateKind::Approval {
                                action: action.clone(),
                                targets: concrete_targets.clone(),
                                match_strategy: crate::runtime::human_gate::MatchStrategy::SubstituteCommand,
                            },
                            manifest,
                            session_id,
                            run_context,
                            config: Some(cfg),
                            context: crate::runtime::human_gate::DecisionContext::tier2(
                                format!(
                                    "artifact.exec {} ({}): {}",
                                    args.artifact_ref, entrypoint, command
                                ),
                                if concrete_targets.is_empty() {
                                    "executing a stored artifact requires operator approval".to_string()
                                } else {
                                    format!(
                                        "artifact execution reaching host(s) [{}] not covered by an approved network grant",
                                        concrete_targets.join(", ")
                                    )
                                },
                                if concrete_targets.is_empty() {
                                    format!(
                                        "runs artifact {} in the sandbox; effects depend on the entrypoint",
                                        artifact_id
                                    )
                                } else {
                                    format!(
                                        "runs artifact {} in the sandbox with network access to [{}]; effects depend on the entrypoint",
                                        artifact_id,
                                        concrete_targets.join(", ")
                                    )
                                },
                                "Approve if the artifact, entrypoint, and any network targets are expected for this agent's task; reject or escalate if any are unexpected",
                            )
                            .with_analysis(reason.clone()),
                            summary: summary.clone(),
                            approval_ref: None,
                            pre_validated,
                            cache_backfill: None,
                            request_id: None,
                            turn_id: None,
                        },
                    )?;
                    match gate_result {
                        crate::runtime::human_gate::GateResult::Cleared { source, .. } => {
                            if source == crate::runtime::human_gate::ClearanceSource::SessionGrant {
                                if let Some(fp) = fingerprint_for_backfill {
                                    if let Some(gw_dir) = gateway_dir {
                                        if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                                            if cache.find(&fp, 0).is_none() {
                                                let entry = crate::runtime::approved_exec_cache::ApprovedExecEntry {
                                                    fingerprint: fp,
                                                    agent_id: manifest.agent.id.clone(),
                                                    remote_targets: concrete_targets.clone(),
                                                    code_content: artifact_code.clone(),
                                                    approval_request_id: String::new(),
                                                    approved_at: chrono::Utc::now().to_rfc3339(),
                                                    approved_by: "operator".to_string(),
                                                    last_used_at: chrono::Utc::now().to_rfc3339(),
                                                };
                                                let _ = cache.record(entry);
                                            }
                                        }
                                    }
                                }
                            }
                            approval_validated_for_command = true;
                        }
                        crate::runtime::human_gate::GateResult::AlreadyPending {
                            gate_id, ..
                        } => {
                            let (cmd, pending_action) = match store.get_approval(&gate_id)? {
                                Some(pending) => match &pending.action {
                                    ScheduledAction::SandboxExec { command, .. } => {
                                        (command.clone(), pending.action.clone())
                                    }
                                    _ => (command.clone(), pending.action.clone()),
                                },
                                None => (command.clone(), action.clone()),
                            };
                            let summary = artifact_exec_approval_summary_line(
                                &manifest.agent.id,
                                &args.artifact_ref,
                                &entrypoint,
                                &cmd,
                                args.intent.as_deref(),
                            );
                            let approval = build_approval_details(
                                &autonoetic_types::background::ApprovalRequest {
                                    request_id: gate_id.clone(),
                                    agent_id: manifest.agent.id.clone(),
                                    session_id: session_id.unwrap_or("").to_string(),
                                    root_session_id: None,
                                    workflow_id: None,
                                    task_id: None,
                                    action: pending_action,
                                    created_at: String::new(),
                                    status: None,
                                    decided_at: None,
                                    decided_by: None,
                                    reason: Some(reason),
                                    evidence_ref: None,
                                    decision_reason: None,
                                    approval_level:
                                        autonoetic_types::background::ApprovalLevel::Operator,
                                    min_dwell_ms: None,
                                    confirm_phrase: None,
                                    code_excerpts: None,
                                    risk_summary: None,

                                    expires_at: None,
                                },
                                "artifact_exec",
                                summary.clone(),
                                "approval_ref",
                                serde_json::json!({
                                    "artifact_ref": args.artifact_ref,
                                    "artifact_id": artifact_id,
                                    "entrypoint": entrypoint,
                                    "args": args.args,
                                    "intent": args.intent,
                                    "command": cmd,
                                    "approval_already_pending": true,
                                }),
                            );
                            return Ok(serde_json::json!({
                                "ok": false,
                                "exit_code": null,
                                "stdout": "",
                                "stderr": format!(
                                    "{}\n\nApproval already pending for this session.",
                                    summary
                                ),
                                "approval_required": true,
                                "approval_already_pending": true,
                                "suspended": true,
                                "request_id": gate_id,
                                "message": format!("Approval {} is already pending.", gate_id),
                                "approval": approval,
                            })
                            .to_string());
                        }
                        crate::runtime::human_gate::GateResult::Suspended { gate_id, .. } => {
                            // Populate code excerpts + risk summary for operator inspection.
                            if let Some(gw_dir) = gateway_dir {
                                let excerpts = crate::runtime::code_excerpts::build_code_excerpts(
                                    &artifact_id,
                                    gw_dir,
                                );
                                let _ = store.set_approval_code_excerpts(
                                    &gate_id,
                                    excerpts.as_deref(),
                                    None,
                                );
                                let artifact_store = crate::ArtifactStore::new(gw_dir).ok();
                                let risk_summary =
                                    crate::runtime::code_excerpts::build_risk_summary(
                                        Some(&concrete_targets),
                                        None,
                                        &artifact_id,
                                        artifact_store.as_ref(),
                                    );
                                if let Some(rs) = risk_summary {
                                    let _ =
                                        store.set_approval_code_excerpts(&gate_id, None, Some(&rs));
                                }
                            }

                            let approval = build_approval_details(
                                &autonoetic_types::background::ApprovalRequest {
                                    request_id: gate_id.clone(),
                                    agent_id: manifest.agent.id.clone(),
                                    session_id: session_id.unwrap_or("").to_string(),
                                    root_session_id: None,
                                    workflow_id: None,
                                    task_id: None,
                                    action: action.clone(),
                                    created_at: String::new(),
                                    status: None,
                                    decided_at: None,
                                    decided_by: None,
                                    reason: Some(reason),
                                    evidence_ref: None,
                                    decision_reason: None,
                                    approval_level:
                                        autonoetic_types::background::ApprovalLevel::Operator,
                                    min_dwell_ms: None,
                                    confirm_phrase: None,
                                    code_excerpts: None,
                                    risk_summary: None,

                                    expires_at: None,
                                },
                                "artifact_exec",
                                summary.clone(),
                                "approval_ref",
                                serde_json::json!({
                                    "artifact_ref": args.artifact_ref,
                                    "artifact_id": artifact_id,
                                    "entrypoint": entrypoint,
                                    "args": args.args,
                                    "intent": args.intent,
                                    "command": command,
                                    "remote_access_detected": true,
                                    "detected_patterns": detected_patterns,
                                    "normalized_targets": concrete_targets,
                                }),
                            );
                            return serde_json::to_string(&serde_json::json!({
                                "ok": false,
                                "exit_code": null,
                                "stdout": "",
                                "stderr": format!(
                                    "{}\n\nTechnical: Remote access scan: {}. Operator approval required to execute artifact code that may reach the network/APIs.",
                                    approval["summary"].as_str().unwrap_or("Artifact exec pending operator approval."),
                                    format!("{}{}", remote_analysis.summary, remote_hint_suffix)
                                ),
                                "approval_required": true,
                                "request_id": gate_id,
                                "suspended": true,
                                "message": format!("Execution suspended pending operator approval ({}).", gate_id),
                                "approval": approval
                            }))
                            .map_err(Into::into);
                        }
                        other => {
                            return Err(anyhow::anyhow!(
                                "Unexpected gate result for artifact.exec: {:?}",
                                other
                            ));
                        }
                    }
                } else {
                    return Ok(ToolError::resource(
                        "GatewayStore missing; cannot persist artifact.exec approval request",
                        None::<String>,
                    )
                    .to_error_response());
                }
            } else {
                return Err(anyhow::anyhow!(
                    "Remote access approval required but GatewayConfig is not available \
                     to enforce the approval gate."
                ));
            }
        }

        let driver = SandboxDriverKind::parse(&manifest.runtime.sandbox)?;
        let agent_dir_str = agent_dir
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is not valid UTF-8"))?;

        let mut mounts = Vec::new();
        let mut layer_python_paths: Vec<String> = Vec::new();
        let mut layer_node_paths: Vec<String> = Vec::new();
        let temp_base = std::env::temp_dir()
            .join("autonoetic_artifact")
            .join(artifact_id.replace('/', "_"));
        std::fs::create_dir_all(&temp_base)?;

        for (name, content) in resolved_files {
            let temp_file = temp_base.join(&name);
            if let Some(parent) = temp_file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&temp_file, &content)?;
            let dest_path = format!("/tmp/{}", name);
            mounts.push(SandboxMount {
                source: temp_file,
                dest: dest_path,
                readonly: false,
            });
        }

        if !bundle.layers.is_empty() {
            let artifact_layers: Vec<crate::runtime::tools::sandbox::LayerMount> = bundle
                .layers
                .iter()
                .map(|l| crate::runtime::tools::sandbox::LayerMount {
                    layer_id: l.layer_id.clone(),
                    mount_path: l.mount_path.clone(),
                })
                .collect();
            crate::runtime::tools::sandbox::extract_and_mount_layers(
                &artifact_layers,
                gw_dir,
                "artifact",
                &mut mounts,
                &mut layer_python_paths,
                &mut layer_node_paths,
            )?;
        }

        // If a fixture_set_ref is provided, pre-populate the artifact's fixture
        // directory from the recorded fixture set.
        if let Some(fs_ref) = &args.fixture_set_ref {
            if let Some(store) = &gateway_store {
                let fixture_set = store
                    .get_fixture_set(fs_ref)?
                    .ok_or_else(|| anyhow::anyhow!("Fixture set '{}' not found", fs_ref))?;
                let recording_session = store
                    .get_recording_session(&fixture_set.recording_session_id)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Recording session for fixture set '{}' not found", fs_ref)
                    })?;
                let staging_dir = gw_dir
                    .join("recordings")
                    .join(&recording_session.session_id)
                    .join("fixtures");
                if staging_dir.exists() {
                    let dest = temp_base.join("fixtures");
                    copy_fixture_dir(&staging_dir, &dest)?;
                    tracing::info!(
                        target: "artifact_exec",
                        fixture_set = %fs_ref,
                        from = %staging_dir.display(),
                        to = %dest.display(),
                        "Pre-populated fixture directory for artifact exec"
                    );
                } else {
                    tracing::warn!(
                        target: "artifact_exec",
                        fixture_set = %fs_ref,
                        path = %staging_dir.display(),
                        "Fixture staging directory not found"
                    );
                }
            }
        }

        let mut overrides = if manifest_may_record_promotion_verdicts(manifest) {
            crate::sandbox::BwrapIsolationOverrides::promotion_gate_overrides()
        } else {
            crate::sandbox::BwrapIsolationOverrides::from_capabilities(&manifest.capabilities)
        };
        // DP-1: one key for the whole bubblewrap tier. The promotion gate runs
        // unpromoted candidate code, so it is the last path that should keep
        // the deprecated whole-host ro-bind when the operator asked for the
        // asserted set.
        overrides.host_fs_allow_set = crate::sandbox::host_fs_allow_set(config);
        if approval_validated_for_command && !manifest_may_record_promotion_verdicts(manifest) {
            overrides.share_net = true;
        }

        let mut extra_env: Vec<(String, String)> = args
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // First-class `input` parameter → AUTONOETIC_INPUT env var. Closes the
        // recurring executor gap where argv (`args`) was wrongly used for
        // scripts that call load_input(). The conflict with env.AUTONOETIC_INPUT
        // is rejected earlier (mechanical, no silent override).
        let serialized_input = args
            .input
            .as_ref()
            .map(|input| crate::runtime::tools::serialize_tool_input(input));
        if let Some(payload) = &serialized_input {
            extra_env.push((
                crate::runtime::tools::AUTONOETIC_INPUT_ENV.to_string(),
                payload.clone(),
            ));
        }

        if !layer_python_paths.is_empty() {
            let layer_pp = layer_python_paths.join(":");
            match extra_env.iter().position(|(k, _)| k == "PYTHONPATH") {
                Some(idx) => {
                    let existing = std::mem::take(&mut extra_env[idx].1);
                    extra_env[idx].1 = format!("{}:{}", layer_pp, existing);
                }
                None => {
                    extra_env.push(("PYTHONPATH".to_string(), layer_pp));
                }
            }
        }
        if !layer_node_paths.is_empty() {
            let layer_np = layer_node_paths.join(":");
            match extra_env.iter().position(|(k, _)| k == "NODE_PATH") {
                Some(idx) => {
                    let existing = std::mem::take(&mut extra_env[idx].1);
                    extra_env[idx].1 = format!("{}:{}", layer_np, existing);
                }
                None => {
                    extra_env.push(("NODE_PATH".to_string(), layer_np));
                }
            }
        }

        if let Some(credential_mappings) = &args.credential_env {
            if let (Some(gw_dir), Some(store)) = (gateway_dir, &gateway_store) {
                crate::vault::ensure_default_key(gw_dir)?;
                let vault_path = crate::vault::default_vault_path(gw_dir);
                let vault = match crate::vault::Vault::load_from_file(&vault_path) {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(anyhow::anyhow!(
                            "credential_env requires a valid vault but vault could not be loaded: {}",
                            e
                        ));
                    }
                };
                for mapping in credential_mappings {
                    crate::runtime::tools::ensure_safe_credential_id_reference(
                        &mapping.credential_id,
                    )?;
                    let cred = store
                        .get_credential(&mapping.credential_id)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "credential_env: credential reference not found in store"
                            )
                        })?;
                    let secret_value = vault.get_secret(&cred.secret_name).ok_or_else(|| {
                        anyhow::anyhow!(
                            "credential_env: secret for referenced credential not found in vault"
                        )
                    })?;
                    tracing::info!(
                        target: "artifact_exec",
                        credential_id = %mapping.credential_id,
                        env_var = %mapping.env_var,
                        "Injecting credential into sandbox as environment variable"
                    );
                    extra_env.push((
                        mapping.env_var.clone(),
                        secret_value.expose_secret().to_string(),
                    ));
                }
            } else {
                return Err(anyhow::anyhow!(
                    "credential_env requires gateway_dir and GatewayStore"
                ));
            }
        }

        let root_session_id = session_id.map(crate::runtime::content_store::root_session_id);

        // RFC scope 5.2c-advisory: if the agent's manifest declares
        // sandbox_network = Sealed/Recording, start the sealed proxy,
        // inject HTTP_PROXY env vars, force share_net so the sandbox
        // can reach the proxy on host loopback. Advisory only — catches
        // HTTP_PROXY-aware clients (Python/Node/Go/curl-with-env). The
        // enforcing seal (netns + nftables transparent redirect) is a
        // future scope (5.2c-enforcing). Until then, raw-socket clients
        // escape.
        let sealed_proxy = crate::runtime::sealed_network_proxy::setup_sealed_proxy_for_exec(
            manifest.sandbox_network,
            temp_base.clone(),
            &mut extra_env,
            &mut overrides,
            Some(gw_dir),
            session_id,
            gateway_store.clone(),
            Some(&manifest.agent.id),
        )?;

        let exec_kind = crate::exec_request::ExecutionKind::shell(command.clone());
        // #1002 slice 1 (follow-up): record what this execution can see —
        // same gateway-asserted record sandbox_exec reports.
        let mount_set = crate::sandbox::compose_mount_set(
            driver,
            agent_dir_str,
            &mounts,
            !overrides.host_fs_allow_set,
        );
        let mut runner = SandboxRunner::spawn_with_session_content_and_env(
            driver,
            agent_dir_str,
            gw_dir,
            &exec_kind,
            None,
            mounts,
            Some(&overrides),
            &extra_env,
            root_session_id,
        )?;

        // Dual delivery, mirroring `execute_script_in_sandbox` (the agent-spawn
        // fast path sets AUTONOETIC_INPUT *and* writes stdin for stdin-mode
        // scripts): when the artifact's SKILL.md declares the default stdin
        // input mode, the payload also goes to the entrypoint's stdin. Without
        // this, a stdin-reading entrypoint run ad-hoc under artifact_exec saw
        // an empty stdin with exit 0 — the silent wrong-output class diagnosed
        // in session-ed19b4ca (same family as the #1 re-federation cause in
        // session-964ea6d7). The env var stays set either way so
        // load_input()-style scripts keep working; `script_input_mode: args`
        // artifacts keep reading $1 and simply never read stdin.
        if let Some(payload) = &serialized_input {
            if artifact_input_mode == ScriptInputMode::Stdin {
                use std::io::Write;
                if let Some(mut stdin) = runner.process.stdin.take() {
                    stdin.write_all(payload.as_bytes()).map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to write artifact input to entrypoint stdin: {e}"
                        )
                    })?;
                }
            }
        }

        let output = runner.process.wait_with_output()?;
        crate::runtime::sealed_network_proxy::shutdown_sealed_proxy(sealed_proxy);
        let exit_code = output.status.code();
        let command_succeeded = output.status.success();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // `ok` reports TOOL-execution success: the sandbox ran the command to
        // completion. A non-zero exit code in the normal range is a DOMAIN
        // result the caller must process (e.g. a unit-test suite that failed)
        // — NOT a tool failure — so it must not be counted as a loop-guard
        // failure or a trajectory divergence. A signal kill (no exit code) or
        // any signal-derived exit code (128 + signal: SIGKILL/OOM 137,
        // SIGTERM 143, SIGSYS/seccomp 159, …) is a genuine sandbox-level fault
        // and stays `ok: false`, so repeated OOM/timeout kills are not mistaken
        // for progress. `command_succeeded` carries the exit-0 signal for
        // consumers that need it. (RFC: unit-test-runner-divergence-loop)
        let ok = matches!(exit_code, Some(code) if (0..128).contains(&code));

        let mut body = serde_json::json!({
            "ok": ok,
            "command_succeeded": command_succeeded,
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "artifact_ref": args.artifact_ref,
            "entrypoint": entrypoint,
            "mount_set": mount_set,
        });

        // Informational only: on the network-isolated promotion-gate path the
        // detected remote-access patterns are NOT a block — the run already
        // happened offline. Surface them so the verdict role can reason about
        // mocked-vs-live coverage without re-running its own analyzer.
        if !informational_remote_patterns.is_empty() {
            body["network_isolated_run"] = serde_json::Value::Bool(true);
            body["detected_patterns"] = serde_json::to_value(&informational_remote_patterns)
                .unwrap_or(serde_json::Value::Array(vec![]));
        }

        if !overrides.share_net {
            let has_network_cap = manifest
                .capabilities
                .iter()
                .any(|c| matches!(c, Capability::NetworkAccess { .. }));
            let stdout_str = stdout.clone();
            let stderr_str = stderr.clone();
            crate::runtime::tools::sandbox::apply_network_isolation_failure_to_result(
                &mut body,
                &stdout_str,
                &stderr_str,
                has_network_cap,
                false,
            );
        }

        serde_json::to_string(&body).map_err(Into::into)
    }
}

/// Whether a promotion-verdict artifact run executes in a physically
/// network-isolated sandbox. When true, `RemoteAccessAnalyzer` detections are
/// treated as informational findings on the run output rather than a static
/// pre-deny: the deterministic suite is allowed to run inside the isolated
/// sandbox (mocked tests pass; tests that genuinely reach the network fail at
/// runtime → the verdict role reports `unable_to_evaluate`).
///
/// True for promotion-verdict roles (`manifest_may_record_promotion_verdicts`)
/// on any driver that *guarantees* the run is offline under the promotion-gate
/// overrides — see [`SandboxDriverKind::guarantees_network_off`] for the
/// per-driver truth (bubblewrap with `force_network_off`, docker `--network none`,
/// wasm WASI-no-sockets → yes; microvm → no, the operator firecracker config
/// controls the NIC, so its promotion runs keep the deterministic-without-network
/// pre-deny, P-3.10).
/// P-3.8 security analysis for ordinary artifact runs. Capability availability
/// is checked by the native registry before dispatch; P-1.9 command-pattern
/// matching does not apply because the gateway synthesizes this command from a
/// validated artifact entrypoint and argument vector.
fn artifact_command_decision(command: &str) -> PolicyDecision {
    let security = SecurityAnalyzer::analyze_command(command);
    if !security.is_safe {
        PolicyDecision::deny_with_analysis("P-3.8", security)
    } else {
        PolicyDecision::allow("P-1.1")
    }
}

/// P-3.8 security analysis for promotion-gate `artifact_exec` runs. Like
/// ordinary artifact execution, this skips CodeExecution pattern matching
/// because the command is synthesized by the gateway.
fn promotion_gate_artifact_command_decision(command: &str) -> PolicyDecision {
    let security = SecurityAnalyzer::analyze_command(command);
    if !security.is_safe {
        PolicyDecision::deny_with_analysis("P-3.8", security)
    } else {
        PolicyDecision::allow("P-3.10")
    }
}

pub fn promotion_run_is_network_isolated(manifest: &AgentManifest) -> bool {
    manifest_may_record_promotion_verdicts(manifest)
        && SandboxDriverKind::parse(&manifest.runtime.sandbox)
            .map(|d| {
                d.guarantees_network_off(
                    &crate::sandbox::BwrapIsolationOverrides::promotion_gate_overrides(),
                )
            })
            .unwrap_or(false)
}

/// The artifact's declared script input mode, read from its SKILL.md
/// frontmatter (`metadata.autonoetic.script_input_mode`). Defaults to
/// [`ScriptInputMode::Stdin`] — the manifest-wide default — when the artifact
/// carries no SKILL.md or the frontmatter does not parse: artifact_exec also
/// runs plain script artifacts that have no manifest at all, and those follow
/// the stdin default like every script agent.
fn artifact_script_input_mode(resolved_files: &[(String, Vec<u8>)]) -> ScriptInputMode {
    resolved_files
        .iter()
        .find(|(name, _)| name == "SKILL.md")
        .and_then(|(_, bytes)| std::str::from_utf8(bytes).ok())
        .and_then(|text| crate::runtime::parser::SkillParser::parse(text).ok())
        .map(|(manifest, _)| manifest.script_input_mode)
        .unwrap_or_default()
}

fn build_command(entrypoint: &str, args: &[String]) -> String {
    let ext = std::path::Path::new(entrypoint)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let interpreter = match ext {
        "py" => "python3",
        "js" => "node",
        "sh" => "bash",
        "rb" => "ruby",
        _ => "python3",
    };

    let mut cmd = format!("{} /tmp/{}", interpreter, entrypoint);
    for arg in args {
        cmd.push(' ');
        if arg.contains(' ') || arg.contains('"') || arg.contains('\'') || arg.contains('\\') {
            cmd.push_str(&format!("'{}'", arg.replace('\'', "'\\''")));
        } else {
            cmd.push_str(arg);
        }
    }
    cmd
}

fn normalize_targets_from_artifact(
    code: &str,
    workspace_files: &[(String, String)],
) -> Vec<String> {
    let analysis =
        default_remote_access_detector().analyze_code_with_workspace(code, workspace_files);
    normalize_targets(&analysis.detected_patterns)
}

fn validate_approval_ref_context(
    decision: &ApprovalDecision,
    current_agent_id: &str,
    current_session_id: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        decision.agent_id == current_agent_id,
        "approval_ref belongs to agent '{}' but current agent is '{}'",
        decision.agent_id,
        current_agent_id
    );
    let sid = current_session_id
        .ok_or_else(|| anyhow::anyhow!("approval_ref requires a session context"))?;
    let current_root = crate::runtime::content_store::root_session_id(sid);
    let approved_root = crate::runtime::tools::sandbox::effective_root_session_id(
        &decision.session_id,
        decision.root_session_id.as_deref(),
    );
    anyhow::ensure!(
        approved_root == current_root,
        "approval_ref belongs to root session '{}' but current root session is '{}'",
        approved_root,
        current_root
    );
    Ok(())
}

fn execute_with_ticket(
    manifest: &AgentManifest,
    _policy: &PolicyEngine,
    agent_dir: &Path,
    gw_dir: &Path,
    args: &ArtifactExecArgs,
    ticket: &crate::runtime::tools::artifact_prepare::DeploymentTicket,
    config: Option<&autonoetic_types::config::GatewayConfig>,
    gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
    session_id: Option<&str>,
) -> anyhow::Result<String> {
    let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
    let bundle = artifact_store.inspect(&ticket.artifact_id)?;
    let resolved_files = artifact_store.resolve_files(&ticket.artifact_id)?;
    let artifact_input_mode = artifact_script_input_mode(&resolved_files);

    let driver = SandboxDriverKind::parse(&manifest.runtime.sandbox)?;
    let agent_dir_str = agent_dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Agent directory is not valid UTF-8"))?;

    let mut mounts = Vec::new();
    let mut layer_python_paths: Vec<String> = Vec::new();
    let mut layer_node_paths: Vec<String> = Vec::new();
    let temp_base = std::env::temp_dir()
        .join("autonoetic_artifact")
        .join(args.artifact_ref.replace('/', "_"));
    std::fs::create_dir_all(&temp_base)?;

    for (name, content) in resolved_files {
        let temp_file = temp_base.join(&name);
        if let Some(parent) = temp_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&temp_file, &content)?;
        let dest_path = format!("/tmp/{}", name);
        mounts.push(SandboxMount {
            source: temp_file,
            dest: dest_path,
            readonly: false,
        });
    }

    if !bundle.layers.is_empty() {
        let artifact_layers: Vec<crate::runtime::tools::sandbox::LayerMount> = bundle
            .layers
            .iter()
            .map(|l| crate::runtime::tools::sandbox::LayerMount {
                layer_id: l.layer_id.clone(),
                mount_path: l.mount_path.clone(),
            })
            .collect();
        crate::runtime::tools::sandbox::extract_and_mount_layers(
            &artifact_layers,
            gw_dir,
            "artifact",
            &mut mounts,
            &mut layer_python_paths,
            &mut layer_node_paths,
        )?;
    }

    // If a fixture_set_ref is provided, pre-populate the artifact's fixture
    // directory from the recorded fixture set.
    if let Some(fs_ref) = &args.fixture_set_ref {
        if let Some(store) = &gateway_store {
            let fixture_set = store
                .get_fixture_set(fs_ref)?
                .ok_or_else(|| anyhow::anyhow!("Fixture set '{}' not found", fs_ref))?;
            let recording_session = store
                .get_recording_session(&fixture_set.recording_session_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!("Recording session for fixture set '{}' not found", fs_ref)
                })?;
            let staging_dir = gw_dir
                .join("recordings")
                .join(&recording_session.session_id)
                .join("fixtures");
            if staging_dir.exists() {
                let dest = temp_base.join("fixtures");
                copy_fixture_dir(&staging_dir, &dest)?;
                tracing::info!(
                    target: "artifact_exec",
                    fixture_set = %fs_ref,
                    from = %staging_dir.display(),
                    to = %dest.display(),
                    "Pre-populated fixture directory"
                );
            } else {
                tracing::warn!(
                    target: "artifact_exec",
                    fixture_set = %fs_ref,
                    path = %staging_dir.display(),
                    "Fixture staging directory not found"
                );
            }
        }
    }

    let mut overrides =
        crate::sandbox::BwrapIsolationOverrides::from_capabilities(&manifest.capabilities);
    // DP-1: same key as every other bubblewrap exec path (see execute()).
    overrides.host_fs_allow_set = crate::sandbox::host_fs_allow_set(config);
    overrides.share_net = !ticket.approved_domains.is_empty();

    let mut extra_env: Vec<(String, String)> = args
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // First-class `input` parameter → AUTONOETIC_INPUT env var. Mirrors the
    // main execute() path — same conflict rejection happens before this fn
    // is entered (execute_with_ticket is only called from execute()).
    let serialized_input = args
        .input
        .as_ref()
        .map(|input| crate::runtime::tools::serialize_tool_input(input));
    if let Some(payload) = &serialized_input {
        extra_env.push((
            crate::runtime::tools::AUTONOETIC_INPUT_ENV.to_string(),
            payload.clone(),
        ));
    }

    if !layer_python_paths.is_empty() {
        let layer_pp = layer_python_paths.join(":");
        match extra_env.iter().position(|(k, _)| k == "PYTHONPATH") {
            Some(idx) => {
                let existing = std::mem::take(&mut extra_env[idx].1);
                extra_env[idx].1 = format!("{}:{}", layer_pp, existing);
            }
            None => {
                extra_env.push(("PYTHONPATH".to_string(), layer_pp));
            }
        }
    }

    if !layer_node_paths.is_empty() {
        let layer_np = layer_node_paths.join(":");
        match extra_env.iter().position(|(k, _)| k == "NODE_PATH") {
            Some(idx) => {
                let existing = std::mem::take(&mut extra_env[idx].1);
                extra_env[idx].1 = format!("{}:{}", layer_np, existing);
            }
            None => {
                extra_env.push(("NODE_PATH".to_string(), layer_np));
            }
        }
    }

    if !ticket.credential_env.is_empty() {
        let store = gateway_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("deployment_ticket with credentials requires GatewayStore")
        })?;
        crate::vault::ensure_default_key(gw_dir)?;
        let vault_path = crate::vault::default_vault_path(gw_dir);
        let vault = crate::vault::Vault::load_from_file(&vault_path)?;
        for mapping in &ticket.credential_env {
            crate::runtime::tools::ensure_safe_credential_id_reference(&mapping.credential_id)?;
            let cred = store
                .get_credential(&mapping.credential_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!("deployment_ticket: credential reference not found in store")
                })?;
            let secret_value = vault.get_secret(&cred.secret_name).ok_or_else(|| {
                anyhow::anyhow!(
                    "deployment_ticket: secret for referenced credential not found in vault"
                )
            })?;
            tracing::info!(
                target: "artifact_exec",
                credential_id = %mapping.credential_id,
                env_var = %mapping.env_var,
                "Injecting credential from deployment ticket"
            );
            extra_env.push((
                mapping.env_var.clone(),
                secret_value.expose_secret().to_string(),
            ));
        }
    }

    let command = build_command(&args.entrypoint, &args.args);

    // RFC scope 5.2c-advisory: see the matching block in the main
    // `artifact_exec` execute() path. Same wiring on the
    // execute_with_ticket path (used for resumed approval flows).
    let sealed_proxy = crate::runtime::sealed_network_proxy::setup_sealed_proxy_for_exec(
        manifest.sandbox_network,
        temp_base.clone(),
        &mut extra_env,
        &mut overrides,
        Some(gw_dir),
        session_id,
        gateway_store.clone(),
        Some(&manifest.agent.id),
    )?;

    let exec_kind = crate::exec_request::ExecutionKind::shell(command.clone());
    // #1002 slice 1 (follow-up): see the matching block in the execute() path.
    let mount_set = crate::sandbox::compose_mount_set(
            driver,
            agent_dir_str,
            &mounts,
            !overrides.host_fs_allow_set,
        );
    let mut runner = SandboxRunner::spawn_with_session_content_and_env(
        driver,
        agent_dir_str,
        gw_dir,
        &exec_kind,
        None,
        mounts,
        Some(&overrides),
        &extra_env,
        None,
    )?;

    // Dual delivery on the ticket path too — same contract and same
    // session-ed19b4ca rationale as the matching block in execute().
    if let Some(payload) = &serialized_input {
        if artifact_input_mode == ScriptInputMode::Stdin {
            use std::io::Write;
            if let Some(mut stdin) = runner.process.stdin.take() {
                stdin.write_all(payload.as_bytes()).map_err(|e| {
                    anyhow::anyhow!("Failed to write artifact input to entrypoint stdin: {e}")
                })?;
            }
        }
    }

    let output = runner.process.wait_with_output()?;
    crate::runtime::sealed_network_proxy::shutdown_sealed_proxy(sealed_proxy);
    let exit_code = output.status.code();
    let command_succeeded = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // See the finalizer above: `ok` reports tool-execution success (the sandbox
    // ran the command to completion), not the command's exit status. A non-zero
    // exit code in the normal range is a domain result; a signal kill (no exit
    // code) or any signal-derived code (>= 128, e.g. SIGKILL/OOM 137,
    // SIGSYS 159) stays `ok: false`. (RFC: unit-test-runner-divergence-loop)
    let ok = matches!(exit_code, Some(code) if (0..128).contains(&code));

    let mut body = serde_json::json!({
        "ok": ok,
        "command_succeeded": command_succeeded,
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "artifact_ref": args.artifact_ref,
        "entrypoint": args.entrypoint,
        "deployment_ticket": args.deployment_ticket,
        "mount_set": mount_set,
    });

    if !overrides.share_net {
        let has_network_cap = manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { .. }));
        crate::runtime::tools::sandbox::apply_network_isolation_failure_to_result(
            &mut body,
            &stdout,
            &stderr,
            has_network_cap,
            false,
        );
    }

    serde_json::to_string(&body).map_err(Into::into)
}

/// Recursively copy fixture files from source to destination.
/// Used when replaying a recorded fixture set during artifact execution.
fn copy_fixture_dir(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<u64> {
    if !src.exists() {
        return Ok(0);
    }
    std::fs::create_dir_all(dst)?;
    let mut count = 0;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let dest_path = dst.join(&file_name);
        if path.is_dir() {
            count += copy_fixture_dir(&path, &dest_path)?;
        } else {
            std::fs::copy(&path, &dest_path)?;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::{
        artifact_exec_approval_operator_reason, artifact_exec_approval_summary_line,
        artifact_script_input_mode, promotion_gate_artifact_command_decision, ArtifactExecArgs,
        ArtifactExecTool,
    };
    use crate::runtime::remote_access::{DetectedPattern, DetectedPatternCategory};
    use crate::runtime::tools::NativeTool;
    use autonoetic_types::agent::ScriptInputMode;
    use autonoetic_types::capability::Capability;

    #[test]
    fn artifact_exec_args_accepts_optional_intent() {
        let args: ArtifactExecArgs = serde_json::from_str(
            r#"{"artifact_ref":"ar.abcd1234","entrypoint":"main.py","intent":"Smoke-test output formatting","args":["--json"]}"#,
        )
        .unwrap();
        assert_eq!(args.intent.as_deref(), Some("Smoke-test output formatting"));
        assert_eq!(args.args, vec!["--json"]);
    }

    #[test]
    fn artifact_exec_args_accepts_optional_input_number() {
        // The session-3739f831 shape: a numeric payload for load_input().
        let args: ArtifactExecArgs = serde_json::from_str(
            r#"{"artifact_ref":"ar.abcd1234","entrypoint":"sqrt_calculator.py","input":25.0}"#,
        )
        .unwrap();
        assert_eq!(args.input, Some(serde_json::json!(25.0)));
    }

    #[test]
    fn artifact_exec_args_accepts_optional_input_object() {
        // Structured payload (the more common SDK contract).
        let args: ArtifactExecArgs = serde_json::from_str(
            r#"{"artifact_ref":"ar.abcd1234","entrypoint":"main.py","input":{"record_id":"abc","format":"summary"}}"#,
        )
        .unwrap();
        assert_eq!(
            args.input,
            Some(serde_json::json!({"record_id":"abc","format":"summary"}))
        );
    }

    #[test]
    fn artifact_exec_args_input_defaults_to_none() {
        // Backward compat: existing calls without `input` deserialize cleanly.
        let args: ArtifactExecArgs = serde_json::from_str(
            r#"{"artifact_ref":"ar.abcd1234","entrypoint":"main.py"}"#,
        )
        .unwrap();
        assert!(args.input.is_none());
    }

    #[test]
    fn serialize_tool_input_round_trips_strings_verbatim() {
        // Strings must NOT be re-quoted — the SDK's _parse_json_or_text would
        // otherwise parse a quoted string as a JSON literal and unwrap it,
        // changing the type the script receives.
        use crate::runtime::tools::serialize_tool_input;
        assert_eq!(serialize_tool_input(&serde_json::json!("hello")), "hello");
        assert_eq!(
            serialize_tool_input(&serde_json::json!("{\"a\":1}")),
            "{\"a\":1}"
        );
    }

    #[test]
    fn serialize_tool_input_jsonifies_structured_values() {
        // Objects/arrays must be JSON-serialized so json.loads can round-trip them.
        use crate::runtime::tools::serialize_tool_input;
        let s = serialize_tool_input(&serde_json::json!({"a":1,"b":[2,3]}));
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, serde_json::json!({"a":1,"b":[2,3]}));
    }

    #[test]
    fn serialize_tool_input_renders_scalars_as_bare_json() {
        // Numbers/bools render as their bare JSON form, which the SDK's
        // json.loads parses back faithfully.
        use crate::runtime::tools::serialize_tool_input;
        assert_eq!(serialize_tool_input(&serde_json::json!(25.0)), "25.0");
        assert_eq!(serialize_tool_input(&serde_json::json!(true)), "true");
        // Null serializes as the JSON literal "null" so both SDKs'
        // json.loads/JSON.parse return None/null, which load_input(default)
        // then replaces with the default. Empty-string would round-trip as
        // "" (a real value), silently breaking the default fallback.
        // (Copilot PR #892.)
        assert_eq!(serialize_tool_input(&serde_json::Value::Null), "null");
    }

    #[test]
    fn artifact_exec_summary_prefers_intent_when_present() {
        let summary = artifact_exec_approval_summary_line(
            "coder.default",
            "ar.abcd1234",
            "main.py",
            "python3 /tmp/main.py --json",
            Some("Run smoke tests with deterministic args"),
        );
        assert!(summary.contains("Run smoke tests with deterministic args"));
        assert!(summary.contains("`ar.abcd1234:main.py`"));
        assert!(summary.contains("`python3 /tmp/main.py --json`"));
    }

    #[test]
    fn artifact_exec_reason_includes_sections_and_pattern_cues() {
        let patterns = vec![DetectedPattern {
            category: DetectedPatternCategory::Import,
            pattern: "import requests".to_string(),
            line_number: Some(12),
            reason: "HTTP client library".to_string(),
        }];
        let reason = artifact_exec_approval_operator_reason(
            "ar.abcd1234",
            "art_deadbeef",
            "main.py",
            "python3 /tmp/main.py --json",
            Some("Validate formatting with a real API call"),
            "Detected 1 remote access pattern(s) in categories: import",
            " → signals: import:import requests",
            &patterns,
        );
        assert!(reason.contains("What will run:"));
        assert!(reason.contains("Artifact target:"));
        assert!(reason.contains("Agent-stated purpose:"));
        assert!(reason.contains("Static analysis cues:"));
        assert!(reason.contains("[line 12] [import] `import requests`"));
    }

    #[test]
    fn copy_fixture_dir_copies_files_recursively() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(src.path().join("api.example.com")).unwrap();
        std::fs::write(
            src.path().join("api.example.com").join("GET-items.json"),
            r#"{"status":200}"#,
        )
        .unwrap();
        std::fs::write(
            src.path().join("api.example.com").join("POST-submit.json"),
            r#"{"status":201}"#,
        )
        .unwrap();

        let count = super::copy_fixture_dir(src.path(), dst.path()).unwrap();
        assert_eq!(count, 2);
        assert!(dst
            .path()
            .join("api.example.com")
            .join("GET-items.json")
            .exists());
        assert!(dst
            .path()
            .join("api.example.com")
            .join("POST-submit.json")
            .exists());
    }

    #[test]
    fn copy_fixture_dir_empty_source_returns_zero() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        assert_eq!(super::copy_fixture_dir(src.path(), dst.path()).unwrap(), 0);
    }

    #[test]
    fn copy_fixture_dir_nonexistent_source_returns_zero() {
        let dst = tempfile::tempdir().unwrap();
        assert_eq!(
            super::copy_fixture_dir(&std::path::Path::new("/nonexistent"), dst.path()).unwrap(),
            0
        );
    }

    #[test]
    fn promotion_gate_artifact_command_allows_synthesized_test_runner() {
        let decision =
            promotion_gate_artifact_command_decision("python3 /tmp/tests/test_fibonacci.py -v");
        assert!(decision.is_allowed(), "{decision:?}");
        assert!(decision.enforced_rules.contains(&"P-3.10"));
    }

    #[test]
    fn promotion_gate_artifact_command_denies_destructive_shell() {
        let decision = promotion_gate_artifact_command_decision("rm -rf /");
        assert!(!decision.is_allowed(), "{decision:?}");
        assert!(decision.enforced_rules.contains(&"P-3.8"));
    }

    #[test]
    fn artifact_exec_available_for_unit_test_runner_without_code_execution() {
        use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
        let tool = ArtifactExecTool;
        let manifest = AgentManifest {
            remote_access: None,
            messaging: None,
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                mounts: Vec::new(),
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "unit_test_runner.default".to_string(),
                name: "Unit Test Runner".to_string(),
                description: "test".to_string(),
                singleton: false,
                resident_idle_ttl_secs: None,
            },
            capabilities: vec![
                Capability::SandboxFunctions {
                    allowed: vec![
                        "knowledge_".to_string(),
                        "artifact_inspect".to_string(),
                        "artifact_exec".to_string(),
                        "promotion_".to_string(),
                    ],
                },
                Capability::ReadAccess {
                    scopes: vec!["self.*".to_string(), "skills/*".to_string()],
                },
            ],
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            adapter: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            sections: Vec::new(),
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
            egress: None,
        };
        assert!(tool.is_available(&manifest));
    }

    #[test]
    fn artifact_exec_not_available_for_auditor_with_evaluation_but_no_sandbox_allow() {
        use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
        let tool = ArtifactExecTool;
        let manifest = AgentManifest {
            remote_access: None,
            messaging: None,
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                mounts: Vec::new(),
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "auditor.default".to_string(),
                name: "Auditor".to_string(),
                description: "test".to_string(),
                singleton: false,
                resident_idle_ttl_secs: None,
            },
            capabilities: vec![
                Capability::SandboxFunctions {
                    allowed: vec!["knowledge_".to_string(), "promotion_".to_string()],
                },
                Capability::ReadAccess {
                    scopes: vec!["self.*".to_string(), "skills/*".to_string()],
                },
                Capability::WriteAccess {
                    scopes: vec!["self.*".to_string(), "skills/*".to_string()],
                },
                Capability::Evaluation {
                    patterns: vec!["*".to_string()],
                },
            ],
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            adapter: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            sections: Vec::new(),
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
            egress: None,
        };
        assert!(
            !tool.is_available(&manifest),
            "auditor has Evaluation for promotion_record but should not see artifact_exec \
             unless SandboxFunctions explicitly allows it"
        );
    }

    #[test]
    fn artifact_exec_available_for_evaluator_with_explicit_sandbox_allow() {
        use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
        let tool = ArtifactExecTool;
        let manifest = AgentManifest {
            remote_access: None,
            messaging: None,
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                mounts: Vec::new(),
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "sealed_evaluator.default".to_string(),
                name: "Sealed Evaluator".to_string(),
                description: "test".to_string(),
                singleton: false,
                resident_idle_ttl_secs: None,
            },
            capabilities: vec![
                Capability::SandboxFunctions {
                    allowed: vec![
                        "knowledge_".to_string(),
                        "artifact_exec".to_string(),
                        "promotion_".to_string(),
                    ],
                },
                Capability::ReadAccess {
                    scopes: vec!["self.*".to_string(), "skills/*".to_string()],
                },
                Capability::Evaluation {
                    patterns: vec!["*".to_string()],
                },
            ],
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            adapter: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            sections: Vec::new(),
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
            egress: None,
        };
        assert!(
            tool.is_available(&manifest),
            "Evaluation agents that explicitly allow artifact_exec in SandboxFunctions should see it"
        );
    }

    #[test]
    fn artifact_exec_not_available_for_static_evaluator() {
        use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
        let tool = ArtifactExecTool;
        let manifest = AgentManifest {
            remote_access: None,
            messaging: None,
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                mounts: Vec::new(),
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "static_evaluator.default".to_string(),
                name: "Static Evaluator".to_string(),
                description: "test".to_string(),
                singleton: false,
                resident_idle_ttl_secs: None,
            },
            capabilities: vec![
                Capability::SandboxFunctions {
                    allowed: vec!["knowledge_".to_string(), "promotion_".to_string()],
                },
                Capability::ReadAccess {
                    scopes: vec!["self.*".to_string(), "skills/*".to_string()],
                },
            ],
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            adapter: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            sections: Vec::new(),
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
            egress: None,
        };
        assert!(!tool.is_available(&manifest));
    }

    /// The artifact's SKILL.md input mode drives stdin dual delivery. Declared
    /// `args` must opt out; a missing or unparseable SKILL.md must follow the
    /// manifest-wide stdin default, not silently skip delivery.
    #[test]
    fn artifact_script_input_mode_follows_skill_md_declaration() {
        let files = |entries: &[(&str, &str)]| -> Vec<(String, Vec<u8>)> {
            entries
                .iter()
                .map(|(n, c)| (n.to_string(), c.as_bytes().to_vec()))
                .collect()
        };

        // Declared args → opt-out honored.
        let skill_args = files(&[(
            "SKILL.md",
            "---\nname: a\ndescription: d\nmetadata:\n  autonoetic:\n    script_input_mode: args\n---\nbody",
        )]);
        assert_eq!(
            artifact_script_input_mode(&skill_args),
            ScriptInputMode::Args,
            "an explicit script_input_mode: args must disable stdin delivery"
        );

        // Declared stdin → Stdin.
        let skill_stdin = files(&[(
            "SKILL.md",
            "---\nname: a\ndescription: d\nmetadata:\n  autonoetic:\n    script_input_mode: stdin\n---\nbody",
        )]);
        assert_eq!(artifact_script_input_mode(&skill_stdin), ScriptInputMode::Stdin);

        // SKILL.md present but declaring nothing → default (Stdin).
        let skill_plain = files(&[(
            "SKILL.md",
            "---\nname: a\ndescription: d\n---\nbody",
        )]);
        assert_eq!(
            artifact_script_input_mode(&skill_plain),
            ScriptInputMode::Stdin,
            "the manifest-wide default is stdin"
        );

        // No SKILL.md at all (plain script artifact) → default (Stdin).
        let no_skill = files(&[("main.py", "print(1)")]);
        assert_eq!(
            artifact_script_input_mode(&no_skill),
            ScriptInputMode::Stdin,
            "manifest-less artifacts follow the stdin default"
        );

        // Unparseable SKILL.md → default (Stdin), never a hard failure —
        // artifact_exec also runs non-bundle artifacts.
        let bad_skill = files(&[("SKILL.md", "not frontmatter at all")]);
        assert_eq!(
            artifact_script_input_mode(&bad_skill),
            ScriptInputMode::Stdin,
            "an unreadable SKILL.md must degrade to the default, not fail exec"
        );

        // Non-UTF-8 SKILL.md → default (Stdin).
        let binary_skill = vec![("SKILL.md".to_string(), vec![0xff, 0xfe, 0x00])];
        assert_eq!(
            artifact_script_input_mode(&binary_skill),
            ScriptInputMode::Stdin
        );
    }
}
