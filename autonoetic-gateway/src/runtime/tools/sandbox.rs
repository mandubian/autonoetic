use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::{NativeToolRunContext, SandboxPidGuard};
use crate::runtime::approved_exec_cache::{
    compute_fingerprint, normalize_targets, ApprovedExecCache,
};
use crate::runtime::tools::{
    build_approval_details, dependency_plan_from_args_or_lock, load_session_content_mounts,
    NativeTool, NativeToolRegistry, SandboxExecArgs,
};
use crate::sandbox::{SandboxDriverKind, SandboxMount, SandboxRunner};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::{ApprovalDecision, ApprovalRequest, ScheduledAction};
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::tagged;
use std::collections::BTreeSet;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SandboxExecTool));
}

pub struct SandboxExecTool;

/// True if `command` uses a content-store digest (`sha256:` + hex) like a shell path.
/// Session files are mounted at `/tmp/<name>`; digests must only go to `content.read`, not `cp`/`python` argv.
fn sandbox_command_misuses_content_digest_as_path(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find("sha256:") {
        let after_prefix = search_from + rel + "sha256:".len();
        let rest = &lower[after_prefix..];
        let hex_len = rest.chars().take_while(|c| c.is_ascii_hexdigit()).count();
        if hex_len >= 8 {
            return true;
        }
        search_from = after_prefix;
    }
    false
}

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

/// § 3.6 — Detects network error patterns in sandbox output.
/// Returns the names of matched patterns (e.g., "ConnectionError", "URLError").
/// Used when the sandbox ran without network access to surface blocked network calls
/// that would otherwise be silently swallowed (exit=0).
fn detect_network_errors_in_output(output: &str) -> Vec<String> {
    // Substrings typical of real library tracebacks / errno text (not plain "connection" in prose).
    const PATTERNS: &[(&str, &str)] = &[
        ("ConnectionError:", "ConnectionError"),
        ("ConnectionRefusedError:", "ConnectionRefusedError"),
        ("URLError:", "URLError"),
        ("urllib.error.URLError", "urllib URLError"),
        ("socket.gaierror", "socket.gaierror"),
        ("Name or service not known", "DNS resolution failed"),
        ("Network is unreachable", "Network unreachable"),
        ("Connection refused", "Connection refused"),
        ("ConnectTimeoutError", "ConnectTimeoutError"),
        ("NewConnectionError", "NewConnectionError"),
        ("MaxRetryError", "MaxRetryError"),
        ("OSError: [Errno 101]", "Network unreachable (errno 101)"),
        ("OSError: [Errno 111]", "Connection refused (errno 111)"),
        ("requests.exceptions.", "requests HTTP error"),
        ("httpx.ConnectError", "httpx connection error"),
        ("httpx.ConnectTimeout", "httpx timeout"),
        ("aiohttp.ClientConnectorError", "aiohttp connector error"),
        ("aiohttp.ClientError", "aiohttp client error"),
        ("Could not resolve host", "DNS resolution failed (curl)"),
    ];

    let mut found = Vec::new();
    for (pattern, label) in PATTERNS {
        if output.contains(pattern) {
            found.push(label.to_string());
        }
    }
    found
}

fn apply_network_isolation_failure_to_result(
    body: &mut serde_json::Value,
    stdout: &str,
    stderr: &str,
    has_network_cap: bool,
) -> Option<Vec<String>> {
    let combined_output = format!("{stdout}\n{stderr}");
    let network_errors = detect_network_errors_in_output(&combined_output);
    if network_errors.is_empty() {
        return None;
    }

    let summary = format!(
        "Sandbox ran without outbound network and output indicates a network failure \
         ({}). {}",
        network_errors.join(", "),
        if has_network_cap {
            "This agent declares NetworkAccess but this run did not enable the network \
             namespace (e.g. missing operator approval or misconfiguration)."
        } else {
            "This agent does not declare NetworkAccess: outbound calls are blocked. \
             Add scoped NetworkAccess, or use builder.default layers so tests run offline."
        }
    );
    if let Some(obj) = body.as_object_mut() {
        obj.insert("ok".to_string(), serde_json::json!(false));
        obj.insert(
            "error_type".to_string(),
            serde_json::json!("network_isolated"),
        );
        obj.insert("network_blocked".to_string(), serde_json::json!(true));
        obj.insert(
            "network_error_patterns".to_string(),
            serde_json::json!(&network_errors),
        );
        obj.insert("network_warning".to_string(), serde_json::json!(&summary));
        obj.insert("message".to_string(), serde_json::json!(&summary));
    }
    Some(network_errors)
}

fn effective_root_session_id(session_id: &str, explicit_root: Option<&str>) -> String {
    explicit_root
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| crate::runtime::content_store::root_session_id(session_id).to_string())
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
    let sid = current_session_id.ok_or_else(|| {
        anyhow::anyhow!("approval_ref requires a session context but no session_id was provided")
    })?;
    let current_root = crate::runtime::content_store::root_session_id(sid);
    let approved_root =
        effective_root_session_id(&decision.session_id, decision.root_session_id.as_deref());
    anyhow::ensure!(
        approved_root == current_root,
        "approval_ref belongs to root session '{}' but current root session is '{}'",
        approved_root,
        current_root
    );
    Ok(())
}

fn approved_request_targets(
    req: &ApprovalRequest,
    agent_dir: &Path,
    gateway_dir: Option<&Path>,
) -> Vec<String> {
    let (command, dep_packages) = match &req.action {
        ScheduledAction::SandboxExec {
            command,
            dependencies,
            ..
        } => (
            command.as_str(),
            dependencies.as_ref().map(|d| d.packages.clone()),
        ),
        _ => return Vec::new(),
    };
    let code = extract_code_for_analysis(command, agent_dir, gateway_dir, Some(&req.session_id));
    let analysis =
        crate::runtime::remote_access::RemoteAccessAnalyzer::analyze_command_and_dependencies(
            &code,
            dep_packages.as_deref(),
        );
    let mut targets = normalize_targets(&analysis.detected_patterns);
    if targets.is_empty() {
        targets = extract_hosts_from_text(command);
    }
    targets
}

fn extract_hosts_from_text(text: &str) -> Vec<String> {
    let Ok(re) = regex::Regex::new(r#"(?i)https?://([^/\s:"'`]+)"#) else {
        return Vec::new();
    };
    let mut hosts: Vec<String> = re
        .captures_iter(text)
        .filter_map(|cap| cap.get(1))
        .map(|m| m.as_str().trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

fn approved_requests_cover_targets(
    approved: &[ApprovalRequest],
    required_targets: &[String],
    agent_dir: &Path,
    gateway_dir: Option<&Path>,
) -> bool {
    if required_targets.is_empty() {
        return false;
    }
    let required: BTreeSet<String> = required_targets.iter().cloned().collect();
    approved.iter().any(|req| {
        if !matches!(req.action, ScheduledAction::SandboxExec { .. }) {
            return false;
        }
        let granted: BTreeSet<String> = approved_request_targets(req, agent_dir, gateway_dir)
            .into_iter()
            .collect();
        required.is_subset(&granted)
    })
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
                    validate_approval_ref_context(&decision, &manifest.agent.id, session_id)
                        .map_err(tagged::Tagged::validation)?;
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

                            // Record to cache immediately upon approval validation,
                            // before any execution attempt. This ensures retries after
                            // sandbox failures (e.g. SUN_LEN) still get cached.
                            let code_to_analyze = extract_code_for_analysis(
                                &effective_command,
                                agent_dir,
                                gateway_dir,
                                session_id,
                            );
                            let dep_packages: Option<Vec<String>> =
                                args.dependencies.as_ref().map(|d| d.packages.clone());
                            let remote_analysis =
                                crate::runtime::remote_access::RemoteAccessAnalyzer::analyze_command_and_dependencies(
                                    &code_to_analyze,
                                    dep_packages.as_deref(),
                                );
                            let normalized_targets =
                                normalize_targets(&remote_analysis.detected_patterns);
                            let fingerprint = compute_fingerprint(
                                &manifest.agent.id,
                                &normalized_targets,
                                &code_to_analyze,
                            );
                            if let Some(gw_dir) = gateway_dir {
                                if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                                    if cache.find(&fingerprint).is_none() {
                                        let entry = crate::runtime::approved_exec_cache::ApprovedExecEntry {
                                            fingerprint: fingerprint.clone(),
                                            agent_id: manifest.agent.id.clone(),
                                            remote_targets: normalized_targets,
                                            code_content: code_to_analyze,
                                            approval_request_id: approval_ref.clone(),
                                            approved_at: chrono::Utc::now().to_rfc3339(),
                                            approved_by: "operator".to_string(),
                                            last_used_at: chrono::Utc::now().to_rfc3339(),
                                        };
                                        if let Err(e) = cache.record(entry) {
                                            tracing::warn!(
                                                target: "sandbox.exec", error = %e,
                                                fingerprint = %fingerprint,
                                                "Failed to record approved exec cache entry"
                                            );
                                        } else {
                                            tracing::info!(
                                                target: "sandbox.exec", fingerprint = %fingerprint,
                                                "Cached approved exec on approval_ref validation"
                                            );
                                        }
                                    }
                                }
                            }
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

        if sandbox_command_misuses_content_digest_as_path(&effective_command) {
            anyhow::bail!(
                "sandbox.exec: content digests (sha256:...) are not filesystem paths in the sandbox. \
Use the path from content.write (`sandbox_path`, typically /tmp/<name>), or pass artifact_id so artifact files are mounted under /tmp/."
            );
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
                            // Analyze ALL files in the artifact, not just entrypoints.
                            // Network patterns may live in non-entrypoint files (e.g.,
                            // an API client module imported by the main script).
                            for file in &bundle.files {
                                if let Some(cs) = &content_store {
                                    if let Ok(content) = cs.read(&file.handle) {
                                        if let Ok(text) = String::from_utf8(content) {
                                            artifact_code
                                                .push_str(&format!("\n# --- {} ---\n", file.name));
                                            artifact_code.push_str(&text);
                                            needs_analysis = true;
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
                                normalize_targets(&analysis.detected_patterns)
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
                        approved_requests_cover_targets(
                            &approved,
                            &artifact_domains,
                            agent_dir,
                            gateway_dir,
                        )
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
                        action: action.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        status: None,
                        decided_at: None,
                        decided_by: None,
                        reason: Some(reason_text),
                        evidence_ref: None,
                        workflow_id: approval_workflow_id.clone(),
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
                    };
                    if let Some(store) = &gateway_store {
                        store.create_approval(&request).map_err(|e| {
                            anyhow::anyhow!(
                                "Failed to persist sandbox approval request '{}': {}",
                                request_id,
                                e
                            )
                        })?;
                    } else {
                        anyhow::bail!(
                            "GatewayStore missing; cannot persist sandbox approval request '{}'",
                            request_id
                        );
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

        let dep_packages: Option<Vec<String>> =
            args.dependencies.as_ref().map(|d| d.packages.clone());
        let remote_analysis =
            crate::runtime::remote_access::RemoteAccessAnalyzer::analyze_command_and_dependencies(
                &code_to_analyze,
                dep_packages.as_deref(),
            );

        let agent_has_network_access = manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { .. }));

        if agent_has_network_access
            && remote_analysis.requires_approval
            && !approval_validated_for_command
        {
            tracing::info!(
                target: "sandbox.exec",
                agent_id = %manifest.agent.id,
                patterns = ?remote_analysis.detected_patterns,
                "Agent has NetworkAccess capability — auto-approving remote access patterns"
            );
            approval_validated_for_command = true;
        }

        tracing::info!(
            target: "sandbox.exec",
            agent_id = %manifest.agent.id,
            session_id = %session_id.unwrap_or(""),
            approval_ref_validated = approval_validated_for_command,
            will_require_approval = remote_analysis.requires_approval && !approval_validated_for_command,
            pattern_count = remote_analysis.detected_patterns.len(),
            dep_package_count = dep_packages.as_ref().map(|p| p.len()).unwrap_or(0),
            summary = %remote_analysis.summary,
            "Remote access scan for sandbox.exec (imports, URLs, IPs, network commands, dependencies). If will_require_approval=true, execution stops until operator approves and caller retries with approval_ref."
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
            let normalized_targets = normalize_targets(&detected_patterns);

            // Check if this exact execution was previously approved (cache hit).
            // Only use the cache when we have concrete targets (URLs, IPs).
            // Import-only and other opaque patterns always require re-approval
            // because they can resolve to different concrete targets at runtime.
            let has_concrete =
                crate::runtime::approved_exec_cache::has_concrete_targets(&detected_patterns);
            if has_concrete {
                if let Some(gw_dir) = gateway_dir {
                    let fingerprint = compute_fingerprint(
                        &manifest.agent.id,
                        &normalized_targets,
                        &code_to_analyze,
                    );
                    if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                        if let Some(entry) = cache.find(&fingerprint) {
                            tracing::info!(
                                target: "sandbox.exec",
                                fingerprint = %fingerprint,
                                previously_approved_by = %entry.approved_by,
                                previously_approved_at = %entry.approved_at,
                                "Cache hit: skipping approval for previously approved sandbox exec"
                            );
                            let _ = cache.update_last_used(&fingerprint);
                            approval_validated_for_command = true;
                        }
                    }
                }

                // Also check for recently approved requests in the store (not just pending).
                // This catches cases where the operator approved but the cache hasn't been
                // populated yet (e.g., first run after cache was cleared).
                if !approval_validated_for_command {
                    if let (Some(_cfg), Some(gw_store), Some(sid)) =
                        (config, &gateway_store, session_id)
                    {
                        let root_sid = crate::runtime::content_store::root_session_id(sid);
                        if let Ok(approved) = gw_store.get_approved_approvals_for_root(root_sid) {
                            for req in &approved {
                                if let ScheduledAction::SandboxExec { command, .. } = &req.action {
                                    if command == &effective_command {
                                        tracing::info!(
                                            target: "sandbox.exec",
                                            request_id = %req.request_id,
                                            "Found matching approved request in store, skipping new approval"
                                        );
                                        approval_validated_for_command = true;

                                        // Also cache this so future checks hit the fast path
                                        let dep_packages: Option<Vec<String>> =
                                            args.dependencies.as_ref().map(|d| d.packages.clone());
                                        let remote_analysis = crate::runtime::remote_access::RemoteAccessAnalyzer::analyze_command_and_dependencies(
                                        &code_to_analyze,
                                        dep_packages.as_deref(),
                                    );
                                        let normalized_targets =
                                            normalize_targets(&remote_analysis.detected_patterns);
                                        let fingerprint = compute_fingerprint(
                                            &manifest.agent.id,
                                            &normalized_targets,
                                            &code_to_analyze,
                                        );
                                        if let Some(gw_dir) = gateway_dir {
                                            if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                                                if cache.find(&fingerprint).is_none() {
                                                    let entry = crate::runtime::approved_exec_cache::ApprovedExecEntry {
                                                    fingerprint: fingerprint.clone(),
                                                    agent_id: manifest.agent.id.clone(),
                                                    remote_targets: normalized_targets,
                                                    code_content: code_to_analyze.clone(),
                                                    approval_request_id: req.request_id.clone(),
                                                    approved_at: chrono::Utc::now().to_rfc3339(),
                                                    approved_by: "operator".to_string(),
                                                    last_used_at: chrono::Utc::now().to_rfc3339(),
                                                };
                                                    let _ = cache.record(entry);
                                                }
                                            }
                                        }

                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            } // end has_concrete guard

            // If still not validated, check for pending approvals
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
            }

            // Still need approval (no cache hit, no pending)
            if !approval_validated_for_command {
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
                        action: action.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        status: None,
                        decided_at: None,
                        decided_by: None,
                        reason: Some({
                            let mut r =
                                format!("Remote access detected: {}", remote_analysis.summary);
                            if !normalized_targets.is_empty() {
                                r.push_str(&format!(" → hosts: {}", normalized_targets.join(", ")));
                            }
                            r
                        }),
                        evidence_ref: None,
                        workflow_id: approval_workflow_id.clone(),
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
                    };
                    if let Some(store) = &gateway_store {
                        store.create_approval(&request).map_err(|e| {
                            anyhow::anyhow!(
                                "Failed to persist sandbox approval request '{}': {}",
                                request_id,
                                e
                            )
                        })?;
                    } else {
                        anyhow::bail!(
                            "GatewayStore missing; cannot persist sandbox approval request '{}'",
                            request_id
                        );
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
                let normalized_targets = normalize_targets(&detected_patterns);
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
        }

        let dep_packages: Option<Vec<String>> =
            args.dependencies.as_ref().map(|d| d.packages.clone());
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

        // § 3.6 — Network error detection for agents running without network access.
        // When the sandbox ran with network isolated (share_net=false) and the output
        // contains network error patterns, surface a clear warning instead of letting
        // the caller assume exit=0 means the code works.
        if !overrides.share_net {
            let has_network_cap = manifest
                .capabilities
                .iter()
                .any(|c| matches!(c, Capability::NetworkAccess { .. }));
            if let Some(network_errors) = apply_network_isolation_failure_to_result(
                &mut body,
                &stdout,
                &stderr,
                has_network_cap,
            ) {
                tracing::warn!(
                    target: "sandbox.exec",
                    agent_id = %manifest.agent.id,
                    has_network_cap = has_network_cap,
                    patterns = ?network_errors,
                    "Network errors detected in sandbox output but network was isolated. \
                     Treating tool result as failure (ok=false)."
                );
            }
        }

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

        // Record to approved exec cache when an operator-granted approval was used.
        // We cache on approval grant, not execution success — the operator's decision
        // that this code may access these hosts is independent of whether the command
        // runs correctly.
        if approval_validated_for_command && args.approval_ref.is_some() {
            let remote_analysis_cached =
                crate::runtime::remote_access::RemoteAccessAnalyzer::analyze_command_and_dependencies(
                    &code_to_analyze,
                    dep_packages.as_deref(),
                );
            let normalized_targets = normalize_targets(&remote_analysis_cached.detected_patterns);
            let fingerprint =
                compute_fingerprint(&manifest.agent.id, &normalized_targets, &code_to_analyze);
            if let Some(gw_dir) = gateway_dir {
                if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                    if cache.find(&fingerprint).is_none() {
                        let approval_request_id = args.approval_ref.clone().unwrap_or_default();
                        let entry = crate::runtime::approved_exec_cache::ApprovedExecEntry {
                            fingerprint: fingerprint.clone(),
                            agent_id: manifest.agent.id.clone(),
                            remote_targets: normalized_targets,
                            code_content: code_to_analyze.clone(),
                            approval_request_id,
                            approved_at: chrono::Utc::now().to_rfc3339(),
                            approved_by: "operator".to_string(),
                            last_used_at: chrono::Utc::now().to_rfc3339(),
                        };
                        if let Err(e) = cache.record(entry) {
                            tracing::warn!(
                                target: "sandbox.exec",
                                error = %e,
                                fingerprint = %fingerprint,
                                "Failed to record approved exec cache entry"
                            );
                        } else {
                            tracing::info!(
                                target: "sandbox.exec",
                                fingerprint = %fingerprint,
                                "Recorded approved execution in cache (operator-granted approval)"
                            );
                        }
                    }
                }
            }
        }

        serde_json::to_string(&body).map_err(Into::into)
    }
}

#[cfg(test)]
mod sandbox_digest_path_tests {
    use super::sandbox_command_misuses_content_digest_as_path;

    #[test]
    fn detects_sha256_hex_as_path_misuse() {
        assert!(sandbox_command_misuses_content_digest_as_path(
            "cp sha256:30db6cfe48acf14817e914345f2a9657b510a8138a1442c3015103beef35279a x.py"
        ));
    }

    #[test]
    fn allows_normal_paths_and_short_sha256_prefix_only() {
        assert!(!sandbox_command_misuses_content_digest_as_path(
            "python3 /tmp/weather_agent.py Paris today"
        ));
        assert!(!sandbox_command_misuses_content_digest_as_path(
            "echo sha256: not a digest"
        ));
    }
}

#[cfg(test)]
mod network_error_detection_tests {
    use super::{apply_network_isolation_failure_to_result, detect_network_errors_in_output};
    use serde_json::json;

    #[test]
    fn empty_output_matches_nothing() {
        assert!(detect_network_errors_in_output("").is_empty());
    }

    #[test]
    fn detects_stdlib_url_errors() {
        let s = "Traceback...\nurllib.error.URLError: <urlopen error timed out>";
        let v = detect_network_errors_in_output(s);
        assert!(v.iter().any(|x| x.contains("urllib")), "{v:?}");
    }

    #[test]
    fn detects_requests_traceback() {
        let s = "requests.exceptions.ConnectionError: HTTPSConnectionPool(host='x')";
        let v = detect_network_errors_in_output(s);
        assert!(v.iter().any(|x| x.contains("requests")), "{v:?}");
    }

    #[test]
    fn ignores_plain_connection_word() {
        assert!(detect_network_errors_in_output("log: connection reset by policy").is_empty());
    }

    #[test]
    fn marks_result_as_failed_when_network_failure_detected() {
        let mut body = json!({
            "ok": true,
            "exit_code": 0,
            "stdout": "done",
            "stderr": ""
        });
        let detected = apply_network_isolation_failure_to_result(
            &mut body,
            "requests.exceptions.ConnectionError: boom",
            "",
            false,
        );
        assert!(
            detected.is_some(),
            "expected network patterns to be detected"
        );
        assert_eq!(body["ok"], json!(false));
        assert_eq!(body["error_type"], json!("network_isolated"));
        assert_eq!(body["network_blocked"], json!(true));
    }

    #[test]
    fn leaves_result_untouched_when_no_network_failure_detected() {
        let mut body = json!({
            "ok": true,
            "exit_code": 0,
            "stdout": "all good",
            "stderr": ""
        });
        let detected =
            apply_network_isolation_failure_to_result(&mut body, "normal output", "", false);
        assert!(detected.is_none(), "should not detect network patterns");
        assert_eq!(body["ok"], json!(true));
        assert!(body.get("error_type").is_none());
        assert!(body.get("network_blocked").is_none());
    }
}

#[cfg(test)]
mod approval_binding_tests {
    use super::{
        approved_requests_cover_targets, extract_hosts_from_text, validate_approval_ref_context,
    };
    use autonoetic_types::background::{
        ApprovalDecision, ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
    };
    use std::path::Path;

    #[test]
    fn approval_ref_rejects_cross_agent_use() {
        let decision = ApprovalDecision {
            request_id: "apr-1".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root/coder.default-1".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "echo ok".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: "2026-01-01T00:00:00Z".to_string(),
            decided_by: "operator".to_string(),
            reason: None,
            root_session_id: Some("root".to_string()),
            workflow_id: None,
            task_id: None,
            approval_level: ApprovalLevel::Operator,
        };
        let err =
            validate_approval_ref_context(&decision, "evaluator.default", Some("root/eval-1"))
                .expect_err("cross-agent approval_ref should be rejected");
        assert!(err.to_string().contains("belongs to agent"));
    }

    #[test]
    fn approval_ref_rejects_cross_root_use() {
        let decision = ApprovalDecision {
            request_id: "apr-2".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root-a/coder.default-1".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "echo ok".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: "2026-01-01T00:00:00Z".to_string(),
            decided_by: "operator".to_string(),
            reason: None,
            root_session_id: Some("root-a".to_string()),
            workflow_id: None,
            task_id: None,
            approval_level: ApprovalLevel::Operator,
        };
        let err = validate_approval_ref_context(&decision, "coder.default", Some("root-b/coder-2"))
            .expect_err("cross-root approval_ref should be rejected");
        assert!(err.to_string().contains("root session"));
    }

    #[test]
    fn approved_requests_cover_targets_requires_structured_host_match() {
        let req = ApprovalRequest {
            request_id: "apr-host".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root/coder-1".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "curl https://api.example.com/v1".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            created_at: "2026-01-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            root_session_id: Some("root".to_string()),
            workflow_id: None,
            task_id: None,
            status: Some(ApprovalStatus::Approved),
            decided_at: Some("2026-01-01T00:00:01Z".to_string()),
            decided_by: Some("operator".to_string()),
            approval_level: ApprovalLevel::Operator,
        };
        assert!(approved_requests_cover_targets(
            &[req.clone()],
            &["api.example.com".to_string()],
            Path::new("."),
            None
        ));
        assert!(!approved_requests_cover_targets(
            &[req],
            &["evil.com".to_string()],
            Path::new("."),
            None
        ));
    }

    #[test]
    fn extracts_hosts_from_command_text_urls() {
        let hosts = extract_hosts_from_text("curl https://api.example.com/v1 && wget http://x.y/z");
        assert_eq!(
            hosts,
            vec!["api.example.com".to_string(), "x.y".to_string()]
        );
    }
}
