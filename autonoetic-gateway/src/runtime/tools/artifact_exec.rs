use crate::llm::ToolDefinition;
use crate::policy::{PolicyDecision, PolicyEngine, SecurityAnalyzer};
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::approved_exec_cache::{
    compute_fingerprint, normalize_targets, ApprovedExecCache,
};
use crate::runtime::remote_access::{
    approval_remote_operator_suffix, classify_network_coverage, is_safe_inspection_command,
    NetworkCoverage, RemoteAccessAnalyzer,
};
use crate::runtime::tools::{
    build_approval_details, load_session_content_mounts,
    promotion::{
        manifest_may_exec_artifact_in_promotion_gate, manifest_may_record_promotion_verdicts,
    },
    CredentialEnvMapping, NativeTool, NativeToolRegistry,
};
use crate::sandbox::{SandboxDriverKind, SandboxMount, SandboxRunner};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::ToolError;
use secrecy::ExposeSecret;
use serde::Deserialize;
use std::collections::BTreeSet;
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
        manifest.capabilities.iter().any(|cap| {
            matches!(cap, Capability::CodeExecution { .. })
                || matches!(cap, Capability::Evaluation { .. })
        }) || manifest_may_exec_artifact_in_promotion_gate(manifest)
    }

    fn guidance(&self) -> Vec<crate::runtime::guidance::GuidanceBlock> {
        // Same approval-continuation block as sandbox_exec (deduped by id at
        // compose), so artifact_exec-only agents still get it (#466).
        vec![crate::runtime::tools::sandbox::exec_approval_continuation_block()]
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Execute an artifact entrypoint in a sandbox. Unlike sandbox.exec, this tool runs remote-access analysis against the artifact's source files (not the shell command string) and binds approval reuse to the artifact identity. Use this for transient validation, smoke tests, and ad hoc runs of built artifacts. For reusable capabilities, prefer creating a script-agent revision instead.".to_string(),
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
                        "description": "Environment variables to set in the sandbox"
                    },
                    "credential_env": {
                        "type": "array",
                        "description": "Inject vault-stored credentials as environment variables into the sandbox. The gateway resolves the secret server-side — it never appears in tool arguments or responses. Use credential_id from credential.check output.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "credential_id": { "type": "string", "description": "Credential ID (from credential.check or delegated by planner)" },
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
                        "description": "Deployment ticket from artifact.prepare. When provided, remote-access approval and credential injection are resolved from the ticket — no separate approval_ref or credential_env needed."
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
            return Ok(ToolError::resource("artifact_exec requires GatewayStore to be configured", None::<String>).to_error_response());
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
                    ).to_error_response());
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
                        Some(&bundle.artifact_canonical_digest),
                        &manifest.capabilities,                    );
                    if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                        if cache.find(&fingerprint).is_none() {
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

        let command = build_command(entrypoint, &args.args);
        let decision = if manifest_may_exec_artifact_in_promotion_gate(manifest) {
            promotion_gate_artifact_command_decision(&command)
        } else {
            policy.can_exec_shell_detailed(&command)
        };
        if !decision.is_allowed() {
            return Err(autonoetic_types::tool_error::tagged::Tagged::permission_with_rules(
                anyhow::anyhow!(decision.explain_shell_denial("Artifact execution")),
                decision
                    .enforced_rules
                    .into_iter()
                    .map(|rule| rule.to_string())
                    .collect(),
            )
            .into());
        }

        let remote_analysis =
            RemoteAccessAnalyzer::analyze_code_with_workspace(&artifact_code, &workspace_files);

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
                            Some(&bundle.artifact_canonical_digest),
                            &manifest.capabilities,
                        );
                        if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                            if let Some(entry) = cache.find(&fingerprint) {
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
                            reason: reason.clone(),
                            summary: summary.clone(),
                            approval_ref: None,
                            pre_validated,
                            cache_backfill: None,
                            turn_id: None,
                        },
                    )?;
                    match gate_result {
                        crate::runtime::human_gate::GateResult::Cleared { source, .. } => {
                            if source == crate::runtime::human_gate::ClearanceSource::SessionGrant {
                                if let Some(fp) = fingerprint_for_backfill {
                                    if let Some(gw_dir) = gateway_dir {
                                        if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                                            if cache.find(&fp).is_none() {
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
                        crate::runtime::human_gate::GateResult::AlreadyPending { gate_id, .. } => {
                            let (cmd, pending_action) = match store.get_approval(&gate_id)? {
                                Some(pending) => match &pending.action {
                                    ScheduledAction::SandboxExec { command, .. } => (
                                        command.clone(),
                                        pending.action.clone(),
                                    ),
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
                                    approval_level: autonoetic_types::background::ApprovalLevel::Operator,
                                    min_dwell_ms: None,
                                    confirm_phrase: None,
                                    code_excerpts: None,
                                    risk_summary: None,
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
                                let excerpts = crate::runtime::code_excerpts::build_code_excerpts(&artifact_id, gw_dir);
                                let _ = store.set_approval_code_excerpts(
                                    &gate_id, excerpts.as_deref(), None,
                                );
                                let artifact_store = crate::ArtifactStore::new(gw_dir).ok();
                                let risk_summary = crate::runtime::code_excerpts::build_risk_summary(
                                    Some(&concrete_targets),
                                    None,
                                    &artifact_id,
                                    artifact_store.as_ref(),
                                );
                                if let Some(rs) = risk_summary {
                                    let _ = store.set_approval_code_excerpts(
                                        &gate_id, None, Some(&rs),
                                    );
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
                                    approval_level: autonoetic_types::background::ApprovalLevel::Operator,
                                    min_dwell_ms: None,
                                    confirm_phrase: None,
                                    code_excerpts: None,
                                    risk_summary: None,
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
            )?;
        }

        // If a fixture_set_ref is provided, pre-populate the artifact's fixture
        // directory from the recorded fixture set.
        if let Some(fs_ref) = &args.fixture_set_ref {
            if let Some(store) = &gateway_store {
                let fixture_set = store.get_fixture_set(fs_ref)?.ok_or_else(|| {
                    anyhow::anyhow!("Fixture set '{}' not found", fs_ref)
                })?;
                let recording_session = store
                    .get_recording_session(&fixture_set.recording_session_id)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Recording session for fixture set '{}' not found",
                            fs_ref
                        )
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
        if approval_validated_for_command && !manifest_may_record_promotion_verdicts(manifest) {
            overrides.share_net = true;
        }

        let mut extra_env: Vec<(String, String)> = args
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

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

        if let Some(credential_mappings) = &args.credential_env {
            if let (Some(gw_dir), Some(store)) = (gateway_dir, &gateway_store) {
                let vault_dir = gw_dir.parent().unwrap_or(gw_dir);
                crate::vault::ensure_default_key(vault_dir)?;
                let vault_path = crate::vault::default_vault_path(vault_dir);
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
        let sealed_proxy =
            crate::runtime::sealed_network_proxy::setup_sealed_proxy_for_exec(
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
        let runner = SandboxRunner::spawn_with_session_content_and_env(
            driver,
            agent_dir_str,
            &exec_kind,
            None,
            mounts,
            Some(&overrides),
            &extra_env,
            root_session_id,
        )?;

        let output = runner.process.wait_with_output()?;
        crate::runtime::sealed_network_proxy::shutdown_sealed_proxy(sealed_proxy);
        let exit_code = output.status.code();
        let command_succeeded = output.status.success();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // `ok` reports TOOL-execution success: the sandbox ran the command to
        // completion. A non-zero exit code is a DOMAIN result the caller must
        // process (e.g. a unit-test suite that failed) — NOT a tool failure —
        // so it must not be counted as a loop-guard failure or a trajectory
        // divergence. A signal kill (no exit code) or a seccomp SIGSYS
        // (exit 159 under the shell wrapper) is a genuine sandbox-level fault
        // and stays `ok: false`. `command_succeeded` carries the exit-0 signal
        // for consumers that need it. (RFC: unit-test-runner-divergence-loop)
        let ok = matches!(exit_code, Some(code) if code != 159);

        let mut body = serde_json::json!({
            "ok": ok,
            "command_succeeded": command_succeeded,
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "artifact_ref": args.artifact_ref,
            "entrypoint": entrypoint,
        });

        // Informational only: on the network-isolated promotion-gate path the
        // detected remote-access patterns are NOT a block — the run already
        // happened offline. Surface them so the verdict role can reason about
        // mocked-vs-live coverage without re-running its own analyzer.
        if !informational_remote_patterns.is_empty() {
            body["network_isolated_run"] = serde_json::Value::Bool(true);
            body["detected_patterns"] =
                serde_json::to_value(&informational_remote_patterns)
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
/// P-3.8 security analysis for promotion-gate `artifact_exec` runs. Skips
/// CodeExecution pattern matching (P-1.9) because the command is synthesized
/// from a gateway-controlled entrypoint + args, not operator-supplied shell.
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
    let analysis = RemoteAccessAnalyzer::analyze_code_with_workspace(code, workspace_files);
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
    _config: Option<&autonoetic_types::config::GatewayConfig>,
    gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
    session_id: Option<&str>,
) -> anyhow::Result<String> {
    let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
    let bundle = artifact_store.inspect(&ticket.artifact_id)?;
    let resolved_files = artifact_store.resolve_files(&ticket.artifact_id)?;

    let driver = SandboxDriverKind::parse(&manifest.runtime.sandbox)?;
    let agent_dir_str = agent_dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Agent directory is not valid UTF-8"))?;

    let mut mounts = Vec::new();
    let mut layer_python_paths: Vec<String> = Vec::new();
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
        )?;
    }

    // If a fixture_set_ref is provided, pre-populate the artifact's fixture
    // directory from the recorded fixture set.
    if let Some(fs_ref) = &args.fixture_set_ref {
        if let Some(store) = &gateway_store {
            let fixture_set = store.get_fixture_set(fs_ref)?.ok_or_else(|| {
                anyhow::anyhow!("Fixture set '{}' not found", fs_ref)
            })?;
            let recording_session = store
                .get_recording_session(&fixture_set.recording_session_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Recording session for fixture set '{}' not found",
                        fs_ref
                    )
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
    overrides.share_net = !ticket.approved_domains.is_empty();

    let mut extra_env: Vec<(String, String)> = args
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

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

    if !ticket.credential_env.is_empty() {
        let store = gateway_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("deployment_ticket with credentials requires GatewayStore")
        })?;
        let vault_dir = gw_dir.parent().unwrap_or(gw_dir);
        crate::vault::ensure_default_key(vault_dir)?;
        let vault_path = crate::vault::default_vault_path(vault_dir);
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
    let runner = SandboxRunner::spawn_with_session_content_and_env(
        driver,
        agent_dir_str,
        &exec_kind,
        None,
        mounts,
        Some(&overrides),
        &extra_env,
        None,
    )?;

    let output = runner.process.wait_with_output()?;
    crate::runtime::sealed_network_proxy::shutdown_sealed_proxy(sealed_proxy);
    let exit_code = output.status.code();
    let command_succeeded = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // See the finalizer above: `ok` reports tool-execution success (the sandbox
    // ran the command to completion), not the command's exit status. A non-zero
    // exit code is a domain result; a signal kill / seccomp SIGSYS (exit 159)
    // stays `ok: false`. (RFC: unit-test-runner-divergence-loop)
    let ok = matches!(exit_code, Some(code) if code != 159);

    let mut body = serde_json::json!({
        "ok": ok,
        "command_succeeded": command_succeeded,
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "artifact_ref": args.artifact_ref,
        "entrypoint": args.entrypoint,
        "deployment_ticket": args.deployment_ticket,
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
        promotion_gate_artifact_command_decision, ArtifactExecArgs,
    };
    use crate::runtime::remote_access::DetectedPattern;

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
            category: "import".to_string(),
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
        assert!(dst.path().join("api.example.com").join("GET-items.json").exists());
        assert!(dst.path().join("api.example.com").join("POST-submit.json").exists());
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
}
