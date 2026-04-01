use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::{NativeToolRunContext, SandboxPidGuard};
use crate::runtime::tools::{
    build_approval_details, dependency_plan_from_args_or_lock, load_session_content_mounts,
    NativeTool, NativeToolRegistry, SandboxExecArgs,
};
use crate::sandbox::{SandboxDriverKind, SandboxMount, SandboxRunner};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::ScheduledAction;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::tagged;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SandboxExecTool));
}

pub struct SandboxExecTool;

fn extract_code_for_analysis(
    command: &str,
    agent_dir: &Path,
    gateway_dir: Option<&Path>,
    session_id: Option<&str>,
) -> String {
    let trimmed = command.trim();

    for python_cmd in &["python3", "python", "python3.11", "python3.12"] {
        if trimmed.starts_with(python_cmd) || trimmed.starts_with(&format!("{} ", python_cmd)) {
            let after_python = trimmed[python_cmd.len()..].trim();

            if after_python.starts_with('-') {
                if let Some(code) = after_python.strip_prefix("-c").map(str::trim_start) {
                    let code = code.trim_matches('"').trim_matches('\'');
                    if !code.is_empty() {
                        return code.to_string();
                    }
                }
                return command.to_string();
            }

            let script_path = after_python.split_whitespace().next().unwrap_or("");
            if script_path.is_empty() {
                return command.to_string();
            }

            if script_path.starts_with("/tmp/") {
                let content_name = &script_path[5..];

                if let (Some(gw_dir), Some(sid)) = (gateway_dir, session_id) {
                    if let Ok(store) = crate::runtime::content_store::ContentStore::new(gw_dir) {
                        if let Ok(content) = store.read_by_name_or_handle(sid, content_name) {
                            if let Ok(content_str) = String::from_utf8(content) {
                                return content_str;
                            }
                        }
                        if let Ok(content) = store.read_by_name(sid, content_name) {
                            if let Ok(content_str) = String::from_utf8(content) {
                                return content_str;
                            }
                        }
                    }
                }

                let actual_path = agent_dir.join(&script_path[5..]);
                if let Ok(content) = std::fs::read_to_string(&actual_path) {
                    return content;
                }

                return command.to_string();
            }

            if script_path.starts_with('/') {
                let actual_path = std::path::PathBuf::from(script_path);
                if let Ok(content) = std::fs::read_to_string(&actual_path) {
                    return content;
                }
            } else {
                let actual_path = agent_dir.join(script_path);
                if let Ok(content) = std::fs::read_to_string(&actual_path) {
                    return content;
                }
            }

            return command.to_string();
        }
    }

    command.to_string()
}

#[cfg(unix)]
fn sandbox_exec_pid_guard(
    runner: &SandboxRunner,
    run_context: Option<&NativeToolRunContext>,
) -> Option<SandboxPidGuard> {
    let ctx = run_context?;
    let pid = runner.process.id();
    if pid == 0 {
        return None;
    }
    Some(
        ctx.registry
            .register_sandbox_child_pid(&ctx.root_session_id, pid),
    )
}

#[cfg(not(unix))]
fn sandbox_exec_pid_guard(
    _runner: &SandboxRunner,
    _run_context: Option<&NativeToolRunContext>,
) -> Option<SandboxPidGuard> {
    None
}

impl NativeTool for SandboxExecTool {
    fn name(&self) -> &'static str {
        "sandbox.exec"
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
            description: "Run any shell command in a secure sandbox. Execute python3 scripts, node.js, bash commands, install packages (pip install, npm install), run tests, compile code, use git, grep, awk, sed, curl (internal network), and more. The sandbox isolates your execution with a read-only host filesystem — only your agent directory is writable. Network access (outbound HTTP, sockets) triggers operator approval; retry with approval_ref after approval. Dangerous commands (sudo, rm -rf, dd, mkfs) are blocked by security policy.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "dependencies": {
                        "type": "object",
                        "properties": {
                            "runtime": { "type": "string", "enum": ["python", "nodejs", "node"] },
                            "packages": {
                                "type": "array",
                                "items": { "type": "string" },
                                "minItems": 1
                            }
                        },
                        "required": ["runtime", "packages"]
                    },
                    "approval_ref": {
                        "type": "string",
                        "description": "Approval request ID (from previous approval_required response). Provide this after operator approval to execute code with remote access."
                    },
                    "artifact_id": {
                        "type": "string",
                        "description": "Optional artifact ID. When provided, only artifact files are mounted into the sandbox (closed boundary). When omitted, all session content is mounted."
                    },
                    "capture_paths": {
                        "type": "array",
                        "description": "Paths inside the sandbox to capture as layers after execution completes. Each path is archived as a separate layer with its content-addressed digest. Use this to capture installed dependencies (e.g., venv/, site-packages/, node_modules/).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Absolute path inside the sandbox to capture (e.g., '/tmp/venv'). The path must exist and be accessible when capture occurs." },
                                "mount_as": { "type": "string", "description": "The mount paths where this layer will be mounted inside the sandbox when the artifact is later used (e.g., '/opt/venv'). This must match the expected path the artifact consumer expects." }
                            },
                            "required": ["path", "mount_as"]
                        }
                    }
                },
                "required": ["command"],
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
        let args: SandboxExecArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(
            !args.command.trim().is_empty(),
            "sandbox command must not be empty"
        );

        let mut approval_validated_for_command = false;
        let mut effective_command = args.command.clone();
        if let Some(approval_ref) = args.approval_ref.as_ref() {
            if let Some(store) = &gateway_store {
                if let Some(req) = store.get_approval(approval_ref)? {
                    if req.status != Some(autonoetic_types::background::ApprovalStatus::Approved) {
                        return Err(tagged::Tagged::validation(anyhow::anyhow!(
                            "approval_ref '{}' references a request that is not approved",
                            approval_ref
                        ))
                        .into());
                    }
                    let decision = req.into_decision()?;
                    match &decision.action {
                        ScheduledAction::SandboxExec { command, .. } => {
                            effective_command = command.clone();
                            tracing::info!(
                                target: "sandbox.exec",
                                approval_ref = %approval_ref,
                                approved_command = %effective_command,
                                "Proceeding with approved sandbox execution (command from store)"
                            );
                            approval_validated_for_command = true;
                        }
                        _ => {
                            return Err(tagged::Tagged::validation(anyhow::anyhow!(
                                "approval_ref '{}' does not reference a sandbox.exec action",
                                approval_ref
                            ))
                            .into());
                        }
                    }
                } else {
                    return Err(tagged::Tagged::validation(anyhow::anyhow!(
                        "approval_ref '{}' not found in store",
                        approval_ref
                    ))
                    .into());
                }
            } else {
                return Err(tagged::Tagged::validation(anyhow::anyhow!(
                    "GatewayStore is required to validate approval_ref"
                ))
                .into());
            }
        }

        let (allowed, analysis) = policy.can_exec_shell_detailed(&effective_command);
        if !allowed {
            let reason = match &analysis {
                Some(a) if !a.threats.is_empty() => {
                    format!(
                        "sandbox command denied by security policy: {}",
                        a.reason.as_deref().unwrap_or("security threats detected")
                    )
                }
                _ => "sandbox command denied by CodeExecution policy".to_string(),
            };
            anyhow::bail!(reason);
        }

        let code_to_analyze =
            extract_code_for_analysis(&effective_command, agent_dir, gateway_dir, session_id);

        let artifact_id_for_approval = args.artifact_id.clone();
        let artifact_remote_needs_approval = if let Some(ref aid) = args.artifact_id {
            if let Some(gw_dir) = &gateway_dir {
                let cache_path = std::path::Path::new(gw_dir)
                    .join("artifacts")
                    .join(aid)
                    .join("remote_access_analysis.json");
                if cache_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&cache_path) {
                        if let Ok(analysis) = serde_json::from_str::<
                            crate::runtime::remote_access::RemoteAccessAnalysis,
                        >(&content)
                        {
                            Some(analysis.requires_approval)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    let mut artifact_code = String::new();
                    let mut needs_analysis = false;
                    if let Ok(store) = crate::artifact_store::ArtifactStore::new(gw_dir) {
                        if let Ok(bundle) = store.inspect(aid) {
                            let content_store =
                                crate::runtime::content_store::ContentStore::new(gw_dir).ok();
                            for entry in &bundle.entrypoints {
                                if let Some(file) = bundle.files.iter().find(|f| f.name == *entry) {
                                    if let Some(cs) = &content_store {
                                        if let Ok(content) = cs.read(&file.handle) {
                                            if let Ok(text) = String::from_utf8(content) {
                                                artifact_code.push_str("\n");
                                                artifact_code.push_str(&text);
                                                needs_analysis = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if needs_analysis && !artifact_code.is_empty() {
                        let analysis =
                            crate::runtime::remote_access::RemoteAccessAnalyzer::analyze_code(
                                &artifact_code,
                            );
                        let _ = std::fs::create_dir_all(cache_path.parent().unwrap());
                        let _ = std::fs::write(
                            &cache_path,
                            serde_json::to_string(&analysis).unwrap_or_default(),
                        );
                        Some(analysis.requires_approval)
                    } else {
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        if artifact_remote_needs_approval == Some(true) && !approval_validated_for_command {
            let artifact_domains: Vec<String> = if let Some(gw_dir) = gateway_dir {
                if let Some(ref aid) = artifact_id_for_approval {
                    let cache_path = gw_dir
                        .join("artifacts")
                        .join(aid)
                        .join("remote_access_analysis.json");
                    if cache_path.exists() {
                        if let Ok(content) = std::fs::read_to_string(&cache_path) {
                            if let Ok(analysis) = serde_json::from_str::<
                                crate::runtime::remote_access::RemoteAccessAnalysis,
                            >(&content)
                            {
                                analysis
                                    .detected_patterns
                                    .iter()
                                    .filter(|p| p.category == "url_literal")
                                    .filter_map(|p| {
                                        let url = &p.pattern;
                                        url.strip_prefix("https://")
                                            .or_else(|| url.strip_prefix("http://"))
                                            .and_then(|rest| rest.split('/').next())
                                            .map(|d| d.to_string())
                                    })
                                    .collect()
                            } else {
                                vec![]
                            }
                        } else {
                            vec![]
                        }
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                }
            } else {
                vec![]
            };

            let artifact_already_approved =
                if let (Some(gw_store), Some(_aid)) = (&gateway_store, &artifact_id_for_approval) {
                    if artifact_domains.is_empty() {
                        false
                    } else {
                        let sid = session_id.unwrap_or("");
                        let root_sid = crate::runtime::content_store::root_session_id(sid);
                        let approved = gw_store
                            .get_approved_approvals_for_root(root_sid)
                            .unwrap_or_default();
                        approved.iter().any(|r| {
                            matches!(&r.action, ScheduledAction::SandboxExec { .. })
                                && artifact_domains.iter().any(|d| {
                                    r.reason.as_ref().map(|s| s.contains(d)).unwrap_or(false)
                                })
                        })
                    }
                } else {
                    false
                };

            if artifact_already_approved {
                approval_validated_for_command = true;
            } else {
                if let Some(cfg) = config {
                    let sid = session_id.unwrap_or("");
                    let root_sid = crate::runtime::content_store::root_session_id(sid);
                    let existing_root =
                        crate::scheduler::approval::pending_approval_requests_for_root(
                            cfg,
                            gateway_store.as_deref(),
                            root_sid,
                        )
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|r| matches!(r.action, ScheduledAction::SandboxExec { .. }))
                        .collect::<Vec<_>>();
                    let existing_session =
                        crate::scheduler::approval::pending_sandbox_exec_requests_for_session(
                            cfg,
                            gateway_store.as_deref(),
                            sid,
                        )
                        .unwrap_or_default();
                    let mut existing = existing_root;
                    for e in existing_session {
                        if !existing.iter().any(|r| r.request_id == e.request_id) {
                            existing.push(e);
                        }
                    }
                    if !existing.is_empty() {
                        let ids: Vec<String> =
                            existing.iter().map(|a| a.request_id.clone()).collect();
                        let primary = &existing[0];
                        let summary = format!(
                            "Artifact {}: remote access detected",
                            artifact_id_for_approval.as_deref().unwrap_or("")
                        );
                        let approval = build_approval_details(
                            primary,
                            "sandbox_exec",
                            summary.clone(),
                            "approval_ref",
                            serde_json::json!({
                                "artifact_id": artifact_id_for_approval.as_deref().unwrap_or(""),
                                "approval_already_pending": true,
                                "note": "A sandbox approval is already pending for this artifact. After operator approval, the approved command will execute automatically.",
                            }),
                        );
                        return Ok(serde_json::json!({
                        "ok": false,
                        "exit_code": null,
                        "stdout": "",
                        "stderr": format!("Remote access detected in artifact {}. Operator approval required.", artifact_id_for_approval.as_deref().unwrap_or("")),
                        "approval_required": true,
                        "approval_already_pending": true,
                        "suspended": true,
                        "request_id": primary.request_id,
                        "pending_request_ids": ids,
                        "message": format!("Execution suspended. Approval {} is pending for artifact. The approved command is already persisted and will be used automatically on resume.", primary.request_id),
                        "approval": approval,
                    }).to_string());
                    }
                }
                if let Some(cfg) = config {
                    let request_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
                    let summary = format!(
                        "Artifact {}: remote access detected",
                        artifact_id_for_approval.as_deref().unwrap_or("")
                    );
                    let action = ScheduledAction::SandboxExec {
                        command: effective_command.clone(),
                        dependencies: args.dependencies.as_ref().map(|d| {
                            autonoetic_types::background::ScheduledActionDependencies {
                                runtime: d.runtime.clone(),
                                packages: d.packages.clone(),
                            }
                        }),
                        requires_approval: true,
                        evidence_ref: None,
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
                    let reason_text = if artifact_domains.is_empty() {
                        format!(
                            "Remote access detected in artifact {}",
                            artifact_id_for_approval.as_deref().unwrap_or("")
                        )
                    } else {
                        format!(
                            "Remote access detected in artifact {}. Domains: {}",
                            artifact_id_for_approval.as_deref().unwrap_or(""),
                            artifact_domains.join(", ")
                        )
                    };
                    let request = autonoetic_types::background::ApprovalRequest {
                        request_id: request_id.clone(),
                        agent_id: manifest.agent.id.clone(),
                        session_id: sid.to_string(),
                        root_session_id: Some(root_session_id.to_string()),
                        action,
                        created_at: chrono::Utc::now().to_rfc3339(),
                        status: None,
                        decided_at: None,
                        decided_by: None,
                        reason: Some(reason_text),
                        evidence_ref: None,
                        workflow_id: approval_workflow_id.clone(),
                        task_id: match (&approval_workflow_id, session_id) {
                            (Some(wf_id), Some(sid)) => {
                                crate::scheduler::resolve_task_id_for_session(cfg, None, wf_id, sid)
                                    .ok()
                                    .flatten()
                            }
                            _ => None,
                        },
                    };
                    if let Some(store) = &gateway_store {
                        let _ = store.create_approval(&request);
                    }
                    let approval = build_approval_details(
                        &request,
                        "sandbox_exec",
                        summary.clone(),
                        "approval_ref",
                        serde_json::json!({
                            "artifact_id": artifact_id_for_approval.as_deref().unwrap_or(""),
                        }),
                    );
                    return Ok(serde_json::json!({
                    "ok": false,
                    "exit_code": null,
                    "stdout": "",
                    "stderr": format!("Remote access detected in artifact {}. Operator approval required.", artifact_id_for_approval.as_deref().unwrap_or("")),
                    "approval_required": true,
                    "request_id": request_id,
                    "suspended": true,
                    "message": format!("Execution suspended pending operator approval ({}). The approved command is persisted and will be used automatically on resume.", request_id),
                    "approval": approval,
                }).to_string());
                } else {
                    return Ok(serde_json::json!({
                    "ok": false,
                    "exit_code": null,
                    "stdout": "",
                    "stderr": format!("Remote access detected in artifact {}. Operator approval required (no config available to persist approval).", artifact_id_for_approval.as_deref().unwrap_or("")),
                    "approval_required": true,
                    "suspended": true,
                }).to_string());
                }
            }
        }

        let remote_analysis =
            crate::runtime::remote_access::RemoteAccessAnalyzer::analyze_code(&code_to_analyze);
        tracing::info!(
            target: "sandbox.exec",
            agent_id = %manifest.agent.id,
            session_id = %session_id.unwrap_or(""),
            approval_ref_validated = approval_validated_for_command,
            will_require_approval = remote_analysis.requires_approval && !approval_validated_for_command,
            pattern_count = remote_analysis.detected_patterns.len(),
            summary = %remote_analysis.summary,
            "Remote access scan for sandbox.exec (imports, URL literals, IPs). If will_require_approval=true, execution stops until operator approves and caller retries with approval_ref."
        );
        if remote_analysis.requires_approval && !approval_validated_for_command {
            tracing::warn!(
                target: "sandbox",
                patterns = ?remote_analysis.detected_patterns,
                "Code requires remote access - operator approval required"
            );

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
                    let ids: Vec<String> = existing.iter().map(|r| r.request_id.clone()).collect();
                    let (cmd, cmd_deps) = match &primary.action {
                        ScheduledAction::SandboxExec {
                            command,
                            dependencies,
                            ..
                        } => (command.clone(), dependencies),
                        _ => {
                            anyhow::bail!(
                                "internal: pending sandbox approval has wrong action type"
                            )
                        }
                    };
                    let summary = format!("Sandbox exec: {}", &cmd[..cmd.len().min(60)]);
                    let approval = build_approval_details(
                        primary,
                        "sandbox_exec",
                        summary.clone(),
                        "approval_ref",
                        serde_json::json!({
                            "command": cmd,
                            "dependencies": cmd_deps.as_ref().map(|d| serde_json::json!({
                                "runtime": d.runtime,
                                "packages": d.packages,
                            })),
                            "approval_already_pending": true,
                            "note": "A sandbox approval is already pending for this session. After operator approval, retry with approval_ref. The approved command will be used automatically.",
                        }),
                    );
                    let dup_note = if existing.len() > 1 {
                        format!(
                            " ({} pending sandbox requests for this session; resolve or reject them.)",
                            existing.len()
                        )
                    } else {
                        String::new()
                    };
                    return serde_json::to_string(&serde_json::json!({
                        "ok": false,
                        "exit_code": null,
                        "stdout": "",
                        "stderr": format!(
                            "Sandbox approval already pending for this session (request_id(s): {}). After approval, the persisted command will execute automatically.{}",
                            ids.join(", "),
                            dup_note
                        ),
                        "approval_required": true,
                        "approval_already_pending": true,
                        "suspended": true,
                        "request_id": primary.request_id,
                        "pending_request_ids": ids,
                        "message": format!(
                            "Execution suspended. Approval {} is pending. The approved command is already persisted and will be used automatically on resume.",
                            primary.request_id
                        ),
                        "approval": approval,
                    }))
                    .map_err(Into::into);
                }
            }

            let detected_patterns = remote_analysis.detected_patterns.clone();
            let normalized_targets =
                crate::runtime::approved_exec_cache::normalize_targets(&detected_patterns);

            if let Some(cfg) = config {
                let request_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
                let summary = format!(
                    "Sandbox exec: {}",
                    &effective_command[..effective_command.len().min(60)]
                );
                let action = ScheduledAction::SandboxExec {
                    command: effective_command.clone(),
                    dependencies: args.dependencies.as_ref().map(|d| {
                        autonoetic_types::background::ScheduledActionDependencies {
                            runtime: d.runtime.clone(),
                            packages: d.packages.clone(),
                        }
                    }),
                    requires_approval: true,
                    evidence_ref: None,
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
                let request = autonoetic_types::background::ApprovalRequest {
                    request_id: request_id.clone(),
                    agent_id: manifest.agent.id.clone(),
                    session_id: sid.to_string(),
                    root_session_id: Some(root_session_id.to_string()),
                    action,
                    created_at: chrono::Utc::now().to_rfc3339(),
                    status: None,
                    decided_at: None,
                    decided_by: None,
                    reason: Some({
                        let mut r = format!("Remote access detected: {}", remote_analysis.summary);
                        if !normalized_targets.is_empty() {
                            r.push_str(&format!(" → hosts: {}", normalized_targets.join(", ")));
                        }
                        r
                    }),
                    evidence_ref: None,
                    workflow_id: approval_workflow_id.clone(),
                    task_id: match (&approval_workflow_id, session_id) {
                        (Some(wf_id), Some(sid)) => {
                            crate::scheduler::resolve_task_id_for_session(cfg, None, wf_id, sid)
                                .ok()
                                .flatten()
                        }
                        _ => None,
                    },
                };
                if let Some(store) = &gateway_store {
                    if let Err(e) = store.create_approval(&request) {
                        tracing::error!(target: "sandbox", error = %e, "Failed to create approval request in store");
                    }
                } else {
                    tracing::error!(target: "sandbox", "GatewayStore missing; cannot create sandbox approval");
                }

                let approval = build_approval_details(
                    &request,
                    "sandbox_exec",
                    summary.clone(),
                    "approval_ref",
                    serde_json::json!({
                        "command": effective_command,
                        "dependencies": args.dependencies.as_ref().map(|d| serde_json::json!({
                            "runtime": d.runtime,
                            "packages": d.packages,
                        })),
                        "remote_access_detected": true,
                        "detected_patterns": detected_patterns,
                        "normalized_targets": normalized_targets,
                        "hosts": normalized_targets
                    }),
                );

                return serde_json::to_string(&serde_json::json!({
                    "ok": false,
                    "exit_code": null,
                    "stdout": "",
                    "stderr": format!("Remote access detected: {}. Operator approval required to execute code with network access.", remote_analysis.summary),
                    "approval_required": true,
                    "request_id": request_id,
                    "remote_access_detected": true,
                    "detected_patterns": remote_analysis.detected_patterns,
                    "suspended": true,
                    "message": format!("Execution suspended pending operator approval ({}). The approved command is persisted and will be used automatically on resume.", request_id),
                    "approval": approval
                }))
                .map_err(Into::into);
            }

            let detected_patterns = remote_analysis.detected_patterns.clone();
            let normalized_targets =
                crate::runtime::approved_exec_cache::normalize_targets(&detected_patterns);
            return serde_json::to_string(&serde_json::json!({
                "ok": false,
                "exit_code": null,
                "stdout": "",
                "stderr": format!("Remote access detected: {}. Operator approval required to execute code with network access.", remote_analysis.summary),
                "approval_required": true,
                "remote_access_detected": true,
                "detected_patterns": remote_analysis.detected_patterns,
                "approval": {
                    "kind": "sandbox_exec",
                    "reason": format!("Remote access detected: {}", remote_analysis.summary),
                    "summary": format!("Sandbox exec: {}", &effective_command[..effective_command.len().min(60)]),
                    "requested_by_agent_id": manifest.agent.id,
                    "session_id": session_id.unwrap_or(""),
                    "retry_field": "approval_ref",
                    "subject": {
                        "command": effective_command,
                        "dependencies": args.dependencies.as_ref().map(|d| serde_json::json!({
                            "runtime": d.runtime,
                            "packages": d.packages,
                        })),
                        "remote_access_detected": true,
                        "detected_patterns": detected_patterns,
                        "normalized_targets": normalized_targets,
                        "hosts": normalized_targets
                    }
                }
            }))
            .map_err(Into::into);
        }

        let dep_plan = dependency_plan_from_args_or_lock(manifest, agent_dir, args.dependencies)?;
        let driver = SandboxDriverKind::parse(&manifest.runtime.sandbox)?;
        let agent_dir_str = agent_dir
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is not valid UTF-8"))?;

        let session_content_mounts = if let Some(artifact_id) = &args.artifact_id {
            let Some(gw_dir) = gateway_dir else {
                anyhow::bail!("artifact_id requires gateway directory to be configured");
            };
            let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
            let resolved_files = artifact_store.resolve_files(artifact_id)?;

            let mut mounts = Vec::new();
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
                });
            }

            let bundle = artifact_store.inspect(artifact_id)?;
            if !bundle.layers.is_empty() {
                let layer_store = crate::layer_store::LayerStore::new(gw_dir, Default::default())?;
                for layer in &bundle.layers {
                    let layer_temp_base = std::env::temp_dir()
                        .join("autonoetic_layer")
                        .join(&layer.layer_id);
                    std::fs::create_dir_all(&layer_temp_base)?;

                    if let Err(e) = layer_store.extract_to(&layer.layer_id, &layer_temp_base) {
                        tracing::warn!(
                            target: "sandbox",
                            layer_id = %layer.layer_id,
                            error = %e,
                            "Failed to extract layer for sandbox mounting"
                        );
                        continue;
                    }

                    tracing::info!(
                        target: "sandbox",
                        layer_id = %layer.layer_id,
                        mount_path = %layer.mount_path,
                        "Mounting artifact layer into sandbox"
                    );

                    mounts.push(SandboxMount {
                        source: layer_temp_base,
                        dest: layer.mount_path.clone(),
                    });
                }
            }

            tracing::info!(
                target: "sandbox",
                artifact_id = %artifact_id,
                mount_count = mounts.len(),
                layer_count = bundle.layers.len(),
                "Mounting artifact files + layers into sandbox (closed boundary)"
            );

            mounts
        } else {
            load_session_content_mounts(gateway_dir, session_id.unwrap_or(&manifest.agent.id))?
        };

        let mut overrides =
            crate::sandbox::BwrapIsolationOverrides::from_capabilities(&manifest.capabilities);

        if approval_validated_for_command {
            overrides.share_net = true;
        }

        let runner = if session_content_mounts.is_empty() {
            SandboxRunner::spawn_with_driver_and_dependencies(
                driver,
                agent_dir_str,
                &effective_command,
                dep_plan.as_ref(),
                Some(&overrides),
            )?
        } else {
            tracing::info!(
                target: "sandbox",
                mount_count = session_content_mounts.len(),
                "Mounting session content files into sandbox"
            );
            SandboxRunner::spawn_with_session_content(
                driver,
                agent_dir_str,
                &effective_command,
                dep_plan.as_ref(),
                session_content_mounts,
                Some(&overrides),
            )?
        };

        let _sandbox_pid_guard = sandbox_exec_pid_guard(&runner, run_context);
        let output = runner.process.wait_with_output()?;
        let ok = output.status.success();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let mut body = serde_json::json!({
            "ok": ok,
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr
        });

        if let Some(ref capture_paths) = args.capture_paths {
            if !capture_paths.is_empty() {
                if let Some(gw_dir) = gateway_dir {
                    match crate::layer_store::LayerStore::new(gw_dir, Default::default()) {
                        Ok(layer_store) => {
                            let mut captured_layers = Vec::new();
                            for cap in capture_paths {
                                let sandbox_prefix = "/tmp";
                                let host_path = if cap.path.starts_with(sandbox_prefix) {
                                    agent_dir.join(
                                        cap.path
                                            .trim_start_matches(sandbox_prefix)
                                            .trim_start_matches('/'),
                                    )
                                } else {
                                    agent_dir.join(cap.path.trim_start_matches('/'))
                                };

                                if host_path.exists() {
                                    match layer_store.create_from_dir(
                                        &host_path,
                                        &cap.path,
                                        &cap.mount_as,
                                    ) {
                                        Ok(layer) => {
                                            tracing::info!(
                                                target: "sandbox",
                                                path = %cap.path,
                                                mount_as = %cap.mount_as,
                                                layer_id = %layer.layer_id,
                                                "Captured sandbox path as layer"
                                            );
                                            captured_layers.push(serde_json::json!({
                                                "path": cap.path,
                                                "mount_as": cap.mount_as,
                                                "layer_id": layer.layer_id,
                                                "digest": layer.digest,
                                                "file_count": layer.file_count,
                                                "size_bytes": layer.size_bytes,
                                            }));
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                target: "sandbox",
                                                path = %cap.path,
                                                error = %e,
                                                "Failed to capture sandbox path as layer"
                                            );
                                        }
                                    }
                                } else {
                                    tracing::warn!(
                                        target: "sandbox",
                                        path = %cap.path,
                                        host_path = %host_path.display(),
                                        "Capture path does not exist in sandbox workspace"
                                    );
                                }
                            }
                            if !captured_layers.is_empty() {
                                body["captured_layers"] = serde_json::Value::Array(captured_layers);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(target: "sandbox", error = %e, "Failed to create layer store for capture");
                        }
                    }
                }
            }
        }

        serde_json::to_string(&body).map_err(Into::into)
    }
}
