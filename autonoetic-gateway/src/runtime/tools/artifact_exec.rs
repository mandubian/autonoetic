use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::approved_exec_cache::{
    compute_fingerprint, normalize_targets, ApprovedExecCache,
};
use crate::runtime::remote_access::{
    classify_network_coverage, is_safe_inspection_command, NetworkCoverage, RemoteAccessAnalyzer,
};
use crate::runtime::tools::{
    build_approval_details, load_session_content_mounts, CredentialEnvMapping, NativeTool,
    NativeToolRegistry,
};
use crate::sandbox::{SandboxDriverKind, SandboxMount, SandboxRunner};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::capability::Capability;
use secrecy::ExposeSecret;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ArtifactExecTool));
}

#[derive(Debug, Deserialize)]
struct ArtifactExecArgs {
    artifact_id: String,
    entrypoint: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
    #[serde(default)]
    approval_ref: Option<String>,
    #[serde(default)]
    deployment_ticket: Option<String>,
    #[serde(default)]
    credential_env: Option<Vec<CredentialEnvMapping>>,
}

pub struct ArtifactExecTool;

impl NativeTool for ArtifactExecTool {
    fn name(&self) -> &'static str {
        "artifact_exec"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::CodeExecution { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Execute an artifact entrypoint in a sandbox. Unlike sandbox.exec, this tool runs remote-access analysis against the artifact's source files (not the shell command string) and binds approval reuse to the artifact identity. Use this for transient validation, smoke tests, and ad hoc runs of built artifacts. For reusable capabilities, prefer creating a script-agent revision instead.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "Artifact ID to execute"
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
                "required": ["artifact_id", "entrypoint"],
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

        if args.artifact_id.starts_with("impl_") {
            return Ok(
                crate::runtime::tools::implicit_artifact_id_error(self.name(), &args.artifact_id)
                    .to_string(),
            );
        }

        if let Some(ticket_id) = &args.deployment_ticket {
            if let Some(store) = &gateway_store {
                if let Some(ticket) = crate::runtime::tools::artifact_prepare::resolve_deployment_ticket(
                    store, ticket_id,
                )? {
                    if !ticket.approved_domains.is_empty() {
                        tracing::info!(
                            target: "artifact_exec",
                            ticket_id = %ticket_id,
                            domains = ?ticket.approved_domains,
                            "Deployment ticket resolved — skipping approval"
                        );
                    }
                    return execute_with_ticket(
                        manifest, policy, agent_dir, gw_dir, &args, &ticket, config, gateway_store,
                    );
                } else {
                    anyhow::bail!("deployment_ticket '{}' not found or expired", ticket_id);
                }
            }
        }

        let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
        let bundle = artifact_store.inspect(&args.artifact_id)?;

        let entrypoint = &args.entrypoint;
        anyhow::ensure!(
            bundle.files.iter().any(|f| f.name == *entrypoint),
            "entrypoint '{}' not found in artifact '{}'",
            entrypoint,
            args.artifact_id
        );

        let resolved_files = artifact_store.resolve_files(&args.artifact_id)?;

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
            args.artifact_id
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
                        Some(&args.artifact_id),
                    );
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
        let (allowed, analysis) = policy.can_exec_shell_detailed(&command);
        if !allowed {
            let reason = match &analysis {
                Some(a) if !a.threats.is_empty() => {
                    format!(
                        "artifact exec denied by security policy: {}",
                        a.reason.as_deref().unwrap_or("security threats detected")
                    )
                }
                _ => "artifact exec denied by CodeExecution policy".to_string(),
            };
            anyhow::bail!(reason);
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

        if remote_analysis.requires_approval && !approval_validated_for_command {
            let detected_patterns = remote_analysis.detected_patterns.clone();
            let concrete_targets = normalize_targets(&detected_patterns);
            let coverage = classify_network_coverage(&detected_patterns, concrete_targets.clone());

            match &coverage {
                NetworkCoverage::Concrete { targets } => {
                    let targets = targets.clone();
                    let fingerprint = compute_fingerprint(
                        &manifest.agent.id,
                        &targets,
                        &artifact_code,
                        Some(&args.artifact_id),
                    );

                    if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                        if let Some(entry) = cache.find(&fingerprint) {
                            tracing::info!(
                                target: "artifact_exec",
                                fingerprint = %fingerprint,
                                "Cache hit: skipping approval"
                            );
                            let _ = cache.update_last_used(&fingerprint);
                            approval_validated_for_command = true;
                        }
                    }

                    if !approval_validated_for_command {
                        if let (Some(_cfg), Some(gw_store), Some(sid)) =
                            (config, &gateway_store, session_id)
                        {
                            let root_sid = crate::runtime::content_store::root_session_id(sid);
                            if !targets.is_empty() {
                                if let Ok(approved) =
                                    gw_store.get_approved_approvals_for_root(root_sid)
                                {
                                    if crate::runtime::tools::sandbox::approved_requests_cover_targets(
                                        &approved,
                                        &targets,
                                        agent_dir,
                                        gateway_dir,
                                    ) {
                                        tracing::info!(
                                            target: "artifact_exec",
                                            targets = ?targets,
                                            "Approved request covers targets"
                                        );
                                        approval_validated_for_command = true;

                                        if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                                            if cache.find(&fingerprint).is_none() {
                                                let entry = crate::runtime::approved_exec_cache::ApprovedExecEntry {
                                                    fingerprint: fingerprint.clone(),
                                                    agent_id: manifest.agent.id.clone(),
                                                    remote_targets: targets.clone(),
                                                    code_content: artifact_code.clone(),
                                                    approval_request_id: approved
                                                        .iter()
                                                        .find(|r| matches!(
                                                            r.action,
                                                            ScheduledAction::SandboxExec { .. }
                                                        ))
                                                        .map(|r| r.request_id.clone())
                                                        .unwrap_or_default(),
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
                        }
                    }

                    if !approval_validated_for_command {
                        if let (Some(gw_store), Some(sid)) = (&gateway_store, session_id) {
                            let root_sid = crate::runtime::content_store::root_session_id(sid);
                            if !targets.is_empty() {
                                if gw_store.session_grants_cover_targets(&root_sid, &targets) {
                                    tracing::info!(
                                        target: "artifact_exec",
                                        targets = ?targets,
                                        "Session grant covers targets"
                                    );
                                    approval_validated_for_command = true;
                                }
                            }
                        }
                    }
                }
                NetworkCoverage::Unresolved => {}
                NetworkCoverage::None => {}
            }

            if !approval_validated_for_command {
                if let Some(cfg) = config {
                    let sid = session_id.unwrap_or("");
                    let existing =
                        crate::scheduler::approval::pending_sandbox_exec_requests_for_session(
                            cfg,
                            gateway_store.as_deref(),
                            sid,
                        )?;
                    if !existing.is_empty() {
                        let primary = &existing[0];
                        let ids: Vec<String> =
                            existing.iter().map(|r| r.request_id.clone()).collect();
                        let approval = build_approval_details(
                            primary,
                            "artifact_exec",
                            format!("Artifact {}: remote access detected", args.artifact_id),
                            "approval_ref",
                            serde_json::json!({
                                "artifact_id": args.artifact_id,
                                "entrypoint": entrypoint,
                                "approval_already_pending": true,
                            }),
                        );
                        return Ok(serde_json::json!({
                            "ok": false,
                            "exit_code": null,
                            "stdout": "",
                            "stderr": format!("Remote access detected in artifact {}. Approval already pending.", args.artifact_id),
                            "approval_required": true,
                            "approval_already_pending": true,
                            "suspended": true,
                            "request_id": primary.request_id,
                            "pending_request_ids": ids,
                            "message": format!("Approval {} is already pending.", primary.request_id),
                            "approval": approval,
                        })
                        .to_string());
                    }
                }

                if let Some(cfg) = config {
                    let request_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
                    let summary =
                        format!("Artifact {}: {}", args.artifact_id, remote_analysis.summary);
                    let action = ScheduledAction::SandboxExec {
                        command: command.clone(),
                        dependencies: None,
                        requires_approval: true,
                        evidence_ref: None,
                        detected_hosts: Some(concrete_targets.clone()),
                    };
                    let approval_workflow_id = {
                        let sid = session_id.unwrap_or("");
                        let root = crate::runtime::content_store::root_session_id(sid);
                        crate::scheduler::resolve_workflow_id_for_root_session(cfg, &root)
                            .ok()
                            .flatten()
                    };
                    let sid = session_id.unwrap_or("");
                    let root_session_id = crate::runtime::content_store::root_session_id(sid);
                    let request = ApprovalRequest {
                        request_id: request_id.clone(),
                        agent_id: manifest.agent.id.clone(),
                        session_id: sid.to_string(),
                        root_session_id: Some(root_session_id.to_string()),
                        action: action.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        status: None,
                        decided_at: None,
                        decided_by: None,
                        reason: Some(format!(
                            "Artifact exec: {} → {}",
                            args.artifact_id, remote_analysis.summary
                        )),
                        evidence_ref: None,
                        workflow_id: approval_workflow_id.clone(),
                        decision_reason: None,
                        approval_level: crate::scheduler::approval::resolve_approval_level(
                            cfg, &action,
                        ),
                        task_id: match (&approval_workflow_id, session_id) {
                            (Some(wf_id), Some(sid)) => {
                                crate::scheduler::resolve_task_id_for_session(cfg, None, wf_id, sid)
                                    .ok()
                                    .flatten()
                            }
                            _ => None,
                        },
                        similar_to_request_id: None,
                        similarity_score: None,
                    };
                    if let Some(store) = &gateway_store {
                        store.create_approval(&request)?;
                    } else {
                        anyhow::bail!("GatewayStore missing; cannot persist approval request");
                    }

                    let approval = build_approval_details(
                        &request,
                        "artifact_exec",
                        summary,
                        "approval_ref",
                        serde_json::json!({
                            "artifact_id": args.artifact_id,
                            "entrypoint": entrypoint,
                            "remote_access_detected": true,
                            "detected_patterns": detected_patterns,
                            "normalized_targets": concrete_targets,
                        }),
                    );
                    return Ok(serde_json::json!({
                        "ok": false,
                        "exit_code": null,
                        "stdout": "",
                        "stderr": format!("Remote access detected in artifact {}. Operator approval required.", args.artifact_id),
                        "approval_required": true,
                        "request_id": request_id,
                        "suspended": true,
                        "message": format!("Execution suspended pending operator approval ({}).", request_id),
                        "approval": approval
                    })
                    .to_string());
                }

                return Ok(serde_json::json!({
                    "ok": false,
                    "exit_code": null,
                    "stdout": "",
                    "stderr": format!("Remote access detected in artifact {}. Operator approval required.", args.artifact_id),
                    "approval_required": true,
                    "suspended": true,
                })
                .to_string());
            }
        }

        let driver = SandboxDriverKind::parse(&manifest.runtime.sandbox)?;
        let agent_dir_str = agent_dir
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is not valid UTF-8"))?;

        let mut mounts = Vec::new();
        let temp_base = std::env::temp_dir()
            .join("autonoetic_artifact")
            .join(args.artifact_id.replace('/', "_"));
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
            let mut layer_python_paths = Vec::new();
            crate::runtime::tools::sandbox::extract_and_mount_layers(
                &artifact_layers,
                gw_dir,
                "artifact",
                &mut mounts,
                &mut layer_python_paths,
            )?;
        }

        let mut overrides =
            crate::sandbox::BwrapIsolationOverrides::from_capabilities(&manifest.capabilities);
        if approval_validated_for_command {
            overrides.share_net = true;
        }

        let mut extra_env: Vec<(String, String)> = args
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

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
                    let cred = store.get_credential(&mapping.credential_id)?.ok_or_else(|| {
                        anyhow::anyhow!("credential_env: credential reference not found in store")
                    })?;
                    let secret_value = vault
                        .get_secret(&cred.secret_name)
                        .ok_or_else(|| {
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

        let runner = SandboxRunner::spawn_with_session_content_and_env(
            driver,
            agent_dir_str,
            &command,
            None,
            mounts,
            Some(&overrides),
            &extra_env,
            root_session_id,
        )?;

        let output = runner.process.wait_with_output()?;
        let ok = output.status.success();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        let mut body = serde_json::json!({
            "ok": ok,
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
            "artifact_id": args.artifact_id,
            "entrypoint": entrypoint,
        });

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
            );
        }

        serde_json::to_string(&body).map_err(Into::into)
    }
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
) -> anyhow::Result<String> {
    let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
    let bundle = artifact_store.inspect(&args.artifact_id)?;
    let resolved_files = artifact_store.resolve_files(&args.artifact_id)?;

    let driver = SandboxDriverKind::parse(&manifest.runtime.sandbox)?;
    let agent_dir_str = agent_dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Agent directory is not valid UTF-8"))?;

    let mut mounts = Vec::new();
    let temp_base = std::env::temp_dir()
        .join("autonoetic_artifact")
        .join(args.artifact_id.replace('/', "_"));
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
        let mut layer_python_paths = Vec::new();
        crate::runtime::tools::sandbox::extract_and_mount_layers(
            &artifact_layers,
            gw_dir,
            "artifact",
            &mut mounts,
            &mut layer_python_paths,
        )?;
    }

    let mut overrides =
        crate::sandbox::BwrapIsolationOverrides::from_capabilities(&manifest.capabilities);
    overrides.share_net = !ticket.approved_domains.is_empty();

    let mut extra_env: Vec<(String, String)> = args
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

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
            let cred = store.get_credential(&mapping.credential_id)?.ok_or_else(|| {
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

    let runner = SandboxRunner::spawn_with_session_content_and_env(
        driver,
        agent_dir_str,
        &command,
        None,
        mounts,
        Some(&overrides),
        &extra_env,
        None,
    )?;

    let output = runner.process.wait_with_output()?;
    let ok = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let mut body = serde_json::json!({
        "ok": ok,
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
        "artifact_id": args.artifact_id,
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
        );
    }

    serde_json::to_string(&body).map_err(Into::into)
}
