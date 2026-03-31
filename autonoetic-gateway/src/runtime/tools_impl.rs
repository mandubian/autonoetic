//! Native tool implementations.
//!
//! The `NativeTool` trait, `NativeToolRegistry`, and shared helpers live in `tools/mod.rs`.
//! This file contains the 36 tool implementations and the `default_registry()` constructor.

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::{NativeToolRunContext, SandboxPidGuard};
use crate::sandbox::{
    DependencyPlan, DependencyRuntime, SandboxDriverKind, SandboxMount, SandboxRunner,
};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::{
    ScheduledAction,
    UserInteractionStatus,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::causal_chain::EntryStatus;
use autonoetic_types::config::{GatewayConfig, SchemaEnforcementConfig, SchemaEnforcementMode};
use autonoetic_types::schema_enforcement::{default_enforcer, EnforcementResult, SchemaEnforcer};
use autonoetic_types::tool_error::tagged;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowEventRecord};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration as StdDuration, Instant};

use crate::runtime::tools::{
    block_on_http, build_approval_details, capability_type_name, default_true, extract_host,
    load_session_content_mounts, NativeTool, NativeToolRegistry,
    tier2_memory_for_native_tool, ToolMetadata, validate_agent_id,
};

// ---------------------------------------------------------------------------
// Sandbox Exec Tool
// ---------------------------------------------------------------------------

pub struct SandboxExecTool;

/// Extract code content for security analysis.
/// If running a script file (e.g., "python3 script.py"), reads the script content.
/// First checks the content store (session content), then falls back to filesystem.
/// Otherwise, returns the command itself for analysis.
fn extract_code_for_analysis(
    command: &str,
    agent_dir: &Path,
    gateway_dir: Option<&Path>,
    session_id: Option<&str>,
) -> String {
    let trimmed = command.trim();

    // Pattern: python3 /path/to/script.py or python /path/to/script.py
    for python_cmd in &["python3", "python", "python3.11", "python3.12"] {
        if trimmed.starts_with(python_cmd) || trimmed.starts_with(&format!("{} ", python_cmd)) {
            let after_python = trimmed[python_cmd.len()..].trim();

            // Skip flags like -c, -m, -u
            if after_python.starts_with('-') {
                // For "python -c ...", analyze the inline code payload.
                // Keep parsing aligned with the sandbox runner's split_whitespace
                // behavior so analysis sees the same token shape execution receives.
                if let Some(code) = after_python.strip_prefix("-c").map(str::trim_start) {
                    // Strip surrounding shell quotes so the regex can match at
                    // the start of the line (e.g. python3 -c "import urllib...")
                    let code = code.trim_matches('"').trim_matches('\'');
                    if !code.is_empty() {
                        return code.to_string();
                    }
                }
                return command.to_string();
            }

            // Extract script path
            let script_path = after_python.split_whitespace().next().unwrap_or("");
            if script_path.is_empty() {
                return command.to_string();
            }

            // For /tmp/ paths, try to read from content store first (session content mounting)
            if script_path.starts_with("/tmp/") {
                let content_name = &script_path[5..]; // Remove "/tmp/" prefix

                // Try to read from content store
                if let (Some(gw_dir), Some(sid)) = (gateway_dir, session_id) {
                    if let Ok(store) = crate::runtime::content_store::ContentStore::new(gw_dir) {
                        if let Ok(content) = store.read_by_name_or_handle(sid, content_name) {
                            if let Ok(content_str) = String::from_utf8(content) {
                                return content_str;
                            }
                        }
                        // Also try without hierarchical (direct session lookup)
                        if let Ok(content) = store.read_by_name(sid, content_name) {
                            if let Ok(content_str) = String::from_utf8(content) {
                                return content_str;
                            }
                        }
                    }
                }

                // Fallback: map sandbox /tmp/ path to host agent_dir
                let actual_path = agent_dir.join(&script_path[5..]);
                if let Ok(content) = std::fs::read_to_string(&actual_path) {
                    return content;
                }

                return command.to_string();
            }

            // Absolute path (not /tmp/)
            if script_path.starts_with('/') {
                let actual_path = std::path::PathBuf::from(script_path);
                if let Ok(content) = std::fs::read_to_string(&actual_path) {
                    return content;
                }
            } else {
                // Relative path
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
                                "mount_as": { "type": "string", "description": "The mount path where this layer will be mounted inside the sandbox when the artifact is later used (e.g., '/opt/venv'). This must match the expected path the artifact consumer expects." }
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

        // Check if this is a retry with approval_ref.
        // If validated, use the APPROVED command from the store rather than
        // requiring the LLM to reproduce the exact original payload.
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
                        autonoetic_types::background::ScheduledAction::SandboxExec {
                            command,
                            ..
                        } => {
                            // Use the approved command from the store, not the
                            // agent's retry payload. This avoids brittle exact-match
                            // failures when the LLM reformats or adds whitespace.
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

        // Check policy with detailed security analysis
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

        // Static analysis for remote access detection
        // Analyzes both the command AND the script content (if running a script file)
        // For /tmp/ paths, reads from content store (session content mounting)
        let code_to_analyze =
            extract_code_for_analysis(&effective_command, agent_dir, gateway_dir, session_id);

        // When artifact_id is provided, check cached analysis first.
        // Artifacts are immutable, so analysis is done once per artifact.
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
                    // Not cached — analyze artifact entrypoint files
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
                        // Cache the result (best effort — don't fail if write fails)
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

        // If artifact analysis says approval is needed and no approval_ref was provided, block
        if artifact_remote_needs_approval == Some(true) && !approval_validated_for_command {
            // Extract domains from the artifact's cached analysis for approval matching
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

            // Check if this artifact's network destinations have already been approved
            // at the root workflow level.
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
                            matches!(
                                &r.action,
                                autonoetic_types::background::ScheduledAction::SandboxExec { .. }
                            ) && artifact_domains
                                .iter()
                                .any(|d| r.reason.as_ref().map(|s| s.contains(d)).unwrap_or(false))
                        })
                    }
                } else {
                    false
                };

            if artifact_already_approved {
                approval_validated_for_command = true;
            } else {
                // Check for existing pending approval for this artifact
                if let Some(cfg) = config {
                    let sid = session_id.unwrap_or("");
                    let root_sid = crate::runtime::content_store::root_session_id(sid);
                    // First check root-level pending approvals (covers cross-session reuse)
                    let existing_root =
                        crate::scheduler::approval::pending_approval_requests_for_root(
                            cfg,
                            gateway_store.as_deref(),
                            root_sid,
                        )
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|r| {
                            matches!(
                                r.action,
                                autonoetic_types::background::ScheduledAction::SandboxExec { .. }
                            )
                        })
                        .collect::<Vec<_>>();
                    // Also check exact session-level pending approvals
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
                // Mint new approval request for the artifact and persist it
                if let Some(cfg) = config {
                    let request_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
                    let summary = format!(
                        "Artifact {}: remote access detected",
                        artifact_id_for_approval.as_deref().unwrap_or("")
                    );
                    let action = autonoetic_types::background::ScheduledAction::SandboxExec {
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
        // Always log so operators can see why a run proceeds vs blocks (static analysis only;
        // unrelated to install approval retry behavior — see docs/agent-install-approval-retry.md).
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

            // Do not mint a new apr-* if this session already has a pending sandbox approval (stops
            // LLM retry loops from flooding pending/).
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

            // Create an actual approval request so operator can approve
            if let Some(cfg) = config {
                let request_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
                let summary = format!(
                    "Sandbox exec: {}",
                    &effective_command[..effective_command.len().min(60)]
                );
                let action = autonoetic_types::background::ScheduledAction::SandboxExec {
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
                // Resolve workflow_id from session
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
                    // Bind to workflow + task if available
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

            // No config available - return basic response
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

        // Load mounts: either from artifact (closed boundary) or all session content
        let session_content_mounts = if let Some(artifact_id) = &args.artifact_id {
            // Artifact mode: mount only artifact files (closed boundary)
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

            // Mount artifact layers if any
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
            // Default: mount all session content
            load_session_content_mounts(gateway_dir, session_id.unwrap_or(&manifest.agent.id))?
        };

        let mut overrides =
            crate::sandbox::BwrapIsolationOverrides::from_capabilities(&manifest.capabilities);

        // If this execution was approved for remote access, ensure the sandbox
        // shares the network namespace regardless of the agent's capabilities.
        // This allows agents without NetworkAccess to run approved network code.
        if approval_validated_for_command {
            overrides.share_net = true;
        }

        let runner = if session_content_mounts.is_empty() {
            // No session content - use original spawn method
            SandboxRunner::spawn_with_driver_and_dependencies(
                driver,
                agent_dir_str,
                &effective_command,
                dep_plan.as_ref(),
                Some(&overrides),
            )?
        } else {
            // Has session content - mount files into sandbox at their original paths
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

        // Capture paths as layers if requested
        if let Some(ref capture_paths) = args.capture_paths {
            if !capture_paths.is_empty() {
                if let Some(gw_dir) = gateway_dir {
                    match crate::layer_store::LayerStore::new(gw_dir, Default::default()) {
                        Ok(layer_store) => {
                            let mut captured_layers = Vec::new();
                            for cap in capture_paths {
                                // Sandbox workspace is /tmp which maps to agent_dir on host.
                                // Strip the /tmp prefix to get the host path.
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

        // Classify known non-retryable sandbox environment failure so agents can
        // stop looping on identical retries and route to a different capability.
        if !ok
            && body["stderr"]
                .as_str()
                .unwrap_or("")
                .contains("bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted")
        {
            body["error_kind"] =
                serde_json::Value::String("sandbox_network_namespace_unavailable".to_string());
            body["retry_recommended"] = serde_json::Value::Bool(false);
            body["diagnostic"] = serde_json::Value::String(
                "Sandbox cannot configure loopback networking on this host; retrying the same sandbox.exec command is unlikely to succeed."
                    .to_string(),
            );
        }
        serde_json::to_string(&body).map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Web Search Tool
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    engine_url: Option<String>,
    #[serde(default)]
    duckduckgo_engine_url: Option<String>,
    #[serde(default)]
    google_engine_url: Option<String>,
    #[serde(default)]
    google_engine_id: Option<String>,
    #[serde(default)]
    google_api_key_env: Option<String>,
    #[serde(default)]
    google_engine_id_env: Option<String>,
    #[serde(default)]
    cache_ttl_secs: Option<u64>,
}

fn default_web_search_engine_url() -> String {
    "https://duckduckgo.com/".to_string()
}

fn default_google_search_engine_url() -> String {
    "https://www.googleapis.com/customsearch/v1".to_string()
}

const GOOGLE_API_KEY_ENV_DEFAULT: &str = "AUTONOETIC_GOOGLE_SEARCH_API_KEY";
const GOOGLE_API_KEY_ENV_LEGACY: &str = "GOOGLE_SEARCH_API_KEY";
const GOOGLE_ENGINE_ID_ENV_DEFAULT: &str = "AUTONOETIC_GOOGLE_SEARCH_ENGINE_ID";
const GOOGLE_ENGINE_ID_ENV_LEGACY: &str = "GOOGLE_SEARCH_ENGINE_ID";
const GOOGLE_ENGINE_ID_ENV_LEGACY_ALT: &str = "GOOGLE_SEARCH_CX";
const WEB_SEARCH_CACHE_TTL_DEFAULT_SECS: u64 = 120;
const WEB_SEARCH_CACHE_TTL_MAX_SECS: u64 = 3_600;

#[derive(Debug, Clone)]
struct WebSearchCacheEntry {
    expires_at: Instant,
    payload: serde_json::Value,
}

static WEB_SEARCH_CACHE: LazyLock<Mutex<HashMap<String, WebSearchCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebSearchProvider {
    Auto,
    DuckDuckGo,
    Google,
}

impl WebSearchProvider {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::DuckDuckGo => "duckduckgo",
            Self::Google => "google",
        }
    }
}

fn parse_web_search_provider(raw: Option<&str>) -> anyhow::Result<WebSearchProvider> {
    let normalized = raw
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "auto".to_string());
    match normalized.as_str() {
        "auto" => Ok(WebSearchProvider::Auto),
        "duckduckgo" | "ddg" => Ok(WebSearchProvider::DuckDuckGo),
        "google" => Ok(WebSearchProvider::Google),
        other => Err(anyhow::Error::from(tagged::Tagged::validation(
            anyhow::anyhow!(
                "Unsupported web.search provider '{}'. Use 'auto', 'duckduckgo', or 'google'.",
                other
            ),
        ))),
    }
}

fn resolve_duckduckgo_engine_url(args: &WebSearchArgs) -> String {
    args.duckduckgo_engine_url
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| {
            args.engine_url
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
        .unwrap_or_else(default_web_search_engine_url)
}

fn resolve_google_engine_url(args: &WebSearchArgs) -> String {
    args.google_engine_url
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| {
            args.engine_url
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
        .unwrap_or_else(default_google_search_engine_url)
}

fn resolve_web_search_cache_ttl_secs(args: &WebSearchArgs) -> u64 {
    args.cache_ttl_secs
        .unwrap_or(WEB_SEARCH_CACHE_TTL_DEFAULT_SECS)
        .min(WEB_SEARCH_CACHE_TTL_MAX_SECS)
}

fn web_search_cache_key(
    args: &WebSearchArgs,
    provider: WebSearchProvider,
    requested_max_results: usize,
    timeout_secs: u64,
) -> String {
    let query = args.query.trim();
    let ddg_engine_url = resolve_duckduckgo_engine_url(args);
    let google_engine_url = resolve_google_engine_url(args);
    let google_engine_id = args
        .google_engine_id
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let google_api_key_env = args
        .google_api_key_env
        .as_deref()
        .unwrap_or(GOOGLE_API_KEY_ENV_DEFAULT);
    let google_engine_id_env = args
        .google_engine_id_env
        .as_deref()
        .unwrap_or(GOOGLE_ENGINE_ID_ENV_DEFAULT);
    format!(
        "provider={}|query={}|max_results={}|timeout_secs={}|ddg_engine_url={}|google_engine_url={}|google_engine_id={}|google_api_key_env={}|google_engine_id_env={}",
        provider.as_str(),
        query,
        requested_max_results,
        timeout_secs,
        ddg_engine_url,
        google_engine_url,
        google_engine_id,
        google_api_key_env,
        google_engine_id_env
    )
}

fn web_search_cache_get(key: &str) -> Option<serde_json::Value> {
    let now = Instant::now();
    let mut cache = WEB_SEARCH_CACHE.lock().ok()?;
    cache.retain(|_, entry| entry.expires_at > now);
    cache.get(key).map(|entry| entry.payload.clone())
}

fn web_search_cache_put(key: String, payload: serde_json::Value, ttl_secs: u64) {
    if ttl_secs == 0 {
        return;
    }
    if let Ok(mut cache) = WEB_SEARCH_CACHE.lock() {
        let now = Instant::now();
        cache.retain(|_, entry| entry.expires_at > now);
        cache.insert(
            key,
            WebSearchCacheEntry {
                expires_at: now + StdDuration::from_secs(ttl_secs),
                payload,
            },
        );
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn resolve_google_api_key(args: &WebSearchArgs) -> anyhow::Result<String> {
    let key_env = args
        .google_api_key_env
        .as_deref()
        .unwrap_or(GOOGLE_API_KEY_ENV_DEFAULT);
    let key = non_empty_env(key_env).or_else(|| {
        if args.google_api_key_env.is_none() {
            non_empty_env(GOOGLE_API_KEY_ENV_LEGACY)
        } else {
            None
        }
    });
    key.ok_or_else(|| {
        anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
            "Google web.search requires API key env '{}'",
            key_env
        )))
    })
}

fn resolve_google_engine_id(args: &WebSearchArgs) -> anyhow::Result<String> {
    if let Some(explicit) = args
        .google_engine_id
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        return Ok(explicit.to_string());
    }
    let engine_id_env = args
        .google_engine_id_env
        .as_deref()
        .unwrap_or(GOOGLE_ENGINE_ID_ENV_DEFAULT);
    let engine_id = non_empty_env(engine_id_env).or_else(|| {
        if args.google_engine_id_env.is_none() {
            non_empty_env(GOOGLE_ENGINE_ID_ENV_LEGACY)
                .or_else(|| non_empty_env(GOOGLE_ENGINE_ID_ENV_LEGACY_ALT))
        } else {
            None
        }
    });
    engine_id.ok_or_else(|| {
        anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
            "Google web.search requires engine id via argument 'google_engine_id' or env '{}'",
            engine_id_env
        )))
    })
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collect_duckduckgo_results(
    payload: &serde_json::Value,
    max_results: usize,
) -> Vec<serde_json::Value> {
    fn maybe_push(
        out: &mut Vec<serde_json::Value>,
        seen_urls: &mut HashSet<String>,
        text: &str,
        url: &str,
        max_results: usize,
    ) {
        if out.len() >= max_results {
            return;
        }
        if text.trim().is_empty() || url.trim().is_empty() {
            return;
        }
        if !seen_urls.insert(url.to_string()) {
            return;
        }
        out.push(serde_json::json!({
            "title": normalize_text(text),
            "url": url,
            "snippet": normalize_text(text),
        }));
    }

    fn walk(
        node: &serde_json::Value,
        out: &mut Vec<serde_json::Value>,
        seen_urls: &mut HashSet<String>,
        max_results: usize,
    ) {
        if out.len() >= max_results {
            return;
        }

        if let Some(obj) = node.as_object() {
            if let (Some(text), Some(url)) = (
                obj.get("Text").and_then(|v| v.as_str()),
                obj.get("FirstURL").and_then(|v| v.as_str()),
            ) {
                maybe_push(out, seen_urls, text, url, max_results);
            }
            if let Some(topics) = obj.get("Topics").and_then(|v| v.as_array()) {
                for topic in topics {
                    walk(topic, out, seen_urls, max_results);
                    if out.len() >= max_results {
                        return;
                    }
                }
            }
            return;
        }

        if let Some(arr) = node.as_array() {
            for item in arr {
                walk(item, out, seen_urls, max_results);
                if out.len() >= max_results {
                    return;
                }
            }
        }
    }

    let mut out = Vec::new();
    let mut seen_urls = HashSet::new();

    if let Some(results) = payload.get("Results").and_then(|v| v.as_array()) {
        for result in results {
            walk(result, &mut out, &mut seen_urls, max_results);
            if out.len() >= max_results {
                return out;
            }
        }
    }
    if let Some(related) = payload.get("RelatedTopics").and_then(|v| v.as_array()) {
        for topic in related {
            walk(topic, &mut out, &mut seen_urls, max_results);
            if out.len() >= max_results {
                return out;
            }
        }
    }
    out
}

fn collect_google_results(
    payload: &serde_json::Value,
    max_results: usize,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut seen_urls = HashSet::new();
    if let Some(items) = payload.get("items").and_then(|v| v.as_array()) {
        for item in items {
            if out.len() >= max_results {
                break;
            }
            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let url = item
                .get("link")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let snippet = item
                .get("snippet")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if title.trim().is_empty() || url.trim().is_empty() {
                continue;
            }
            if !seen_urls.insert(url.to_string()) {
                continue;
            }
            out.push(serde_json::json!({
                "title": normalize_text(title),
                "url": url,
                "snippet": normalize_text(snippet),
            }));
        }
    }
    out
}

#[derive(Debug)]
struct WebSearchResponse {
    provider: WebSearchProvider,
    engine_url: String,
    status_code: u16,
    results: Vec<serde_json::Value>,
    abstract_text: Option<String>,
    total_results: Option<u64>,
}

fn execute_duckduckgo_search(
    policy: &PolicyEngine,
    query: &str,
    engine_url: String,
    max_results: usize,
    timeout_secs: u64,
) -> anyhow::Result<WebSearchResponse> {
    let engine_host = extract_host(&engine_url)?;
    if !policy.can_connect_net(&engine_host) {
        return Err(anyhow::Error::from(tagged::Tagged::permission(
            anyhow::anyhow!(
                "Permission Denied: NetworkAccess does not allow host '{}'",
                engine_host
            ),
        )));
    }

    let request_engine_url = engine_url.clone();
    let request_query = query.to_string();
    let (status_code, payload) = block_on_http(async move {
        let mut request_url = reqwest::Url::parse(&request_engine_url).map_err(|e| {
            anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
                "Invalid search engine URL '{}': {}",
                request_engine_url,
                e
            )))
        })?;
        {
            let mut pairs = request_url.query_pairs_mut();
            pairs.append_pair("q", request_query.as_str());
            pairs.append_pair("format", "json");
            pairs.append_pair("no_html", "1");
            pairs.append_pair("skip_disambig", "1");
        }

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| anyhow::anyhow!("web.search client build failed: {}", e))?;
        let response = client
            .get(request_url)
            .timeout(StdDuration::from_secs(timeout_secs))
            .send()
            .await
            .map_err(|e| {
                anyhow::Error::from(tagged::Tagged::resource(anyhow::anyhow!(
                    "web.search request failed: {}",
                    e
                )))
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::Error::from(tagged::Tagged::resource(
                anyhow::anyhow!("web.search request failed with status {}", status),
            )));
        }
        let payload = response.json::<serde_json::Value>().await.map_err(|e| {
            anyhow::Error::from(tagged::Tagged::execution(anyhow::anyhow!(
                "web.search could not decode JSON response: {}",
                e
            )))
        })?;
        Ok((status.as_u16(), payload))
    })?;

    let results = collect_duckduckgo_results(&payload, max_results);
    let abstract_text = payload
        .get("AbstractText")
        .and_then(|v| v.as_str())
        .map(normalize_text)
        .filter(|text| !text.is_empty());

    Ok(WebSearchResponse {
        provider: WebSearchProvider::DuckDuckGo,
        engine_url,
        status_code,
        results,
        abstract_text,
        total_results: None,
    })
}

fn execute_google_search(
    policy: &PolicyEngine,
    query: &str,
    engine_url: String,
    api_key: String,
    engine_id: String,
    max_results: usize,
    timeout_secs: u64,
) -> anyhow::Result<WebSearchResponse> {
    let engine_host = extract_host(&engine_url)?;
    if !policy.can_connect_net(&engine_host) {
        return Err(anyhow::Error::from(tagged::Tagged::permission(
            anyhow::anyhow!(
                "Permission Denied: NetworkAccess does not allow host '{}'",
                engine_host
            ),
        )));
    }

    let request_engine_url = engine_url.clone();
    let request_query = query.to_string();
    let (status_code, payload) = block_on_http(async move {
        let mut request_url = reqwest::Url::parse(&request_engine_url).map_err(|e| {
            anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
                "Invalid search engine URL '{}': {}",
                request_engine_url,
                e
            )))
        })?;
        {
            let mut pairs = request_url.query_pairs_mut();
            pairs.append_pair("q", request_query.as_str());
            pairs.append_pair("key", api_key.as_str());
            pairs.append_pair("cx", engine_id.as_str());
            pairs.append_pair("num", &max_results.to_string());
        }

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| anyhow::anyhow!("web.search client build failed: {}", e))?;
        let response = client
            .get(request_url)
            .timeout(StdDuration::from_secs(timeout_secs))
            .send()
            .await
            .map_err(|e| {
                anyhow::Error::from(tagged::Tagged::resource(anyhow::anyhow!(
                    "web.search request failed: {}",
                    e
                )))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::Error::from(tagged::Tagged::resource(
                anyhow::anyhow!("web.search request failed with status {}", status),
            )));
        }
        let payload = response.json::<serde_json::Value>().await.map_err(|e| {
            anyhow::Error::from(tagged::Tagged::execution(anyhow::anyhow!(
                "web.search could not decode JSON response: {}",
                e
            )))
        })?;
        Ok((status.as_u16(), payload))
    })?;

    if let Some(error_payload) = payload.get("error") {
        let message = error_payload
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown google search error");
        return Err(anyhow::Error::from(tagged::Tagged::execution(
            anyhow::anyhow!("web.search google provider returned error: {}", message),
        )));
    }

    let results = collect_google_results(&payload, max_results);
    let total_results = payload
        .pointer("/searchInformation/totalResults")
        .and_then(|v| v.as_str())
        .and_then(|value| value.parse::<u64>().ok());

    Ok(WebSearchResponse {
        provider: WebSearchProvider::Google,
        engine_url,
        status_code,
        results,
        abstract_text: None,
        total_results,
    })
}

fn web_search_response_to_payload(query: &str, response: WebSearchResponse) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "ok": true,
        "provider": response.provider.as_str(),
        "query": query,
        "engine_url": response.engine_url,
        "status_code": response.status_code,
        "result_count": response.results.len(),
        "results": response.results
    });
    if let Some(abstract_text) = response.abstract_text {
        payload["abstract"] = serde_json::json!(abstract_text);
    }
    if let Some(total_results) = response.total_results {
        payload["total_results"] = serde_json::json!(total_results);
    }
    payload
}

pub struct WebSearchTool;

impl NativeTool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web.search"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::NetworkAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description:
                "Search the web via provider-backed JSON APIs (duckduckgo, google, or auto fallback)."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "provider": { "type": "string", "enum": ["auto", "duckduckgo", "google"] },
                    "max_results": { "type": "integer", "minimum": 1, "maximum": 20 },
                    "timeout_secs": { "type": "integer", "minimum": 5, "maximum": 120 },
                    "engine_url": { "type": "string" },
                    "duckduckgo_engine_url": { "type": "string" },
                    "google_engine_url": { "type": "string" },
                    "google_engine_id": { "type": "string" },
                    "google_api_key_env": { "type": "string" },
                    "google_engine_id_env": { "type": "string" },
                    "cache_ttl_secs": { "type": "integer", "minimum": 0, "maximum": 3600 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: WebSearchArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.query.trim().is_empty(), "query must not be empty");
        let query = args.query.trim().to_string();
        let requested_provider = parse_web_search_provider(args.provider.as_deref())?;
        let timeout_secs = args.timeout_secs.unwrap_or(20).clamp(5, 120);
        let requested_max_results = args.max_results.unwrap_or(5).clamp(1, 20);
        let cache_ttl_secs = resolve_web_search_cache_ttl_secs(&args);
        let cache_key = web_search_cache_key(
            &args,
            requested_provider,
            requested_max_results,
            timeout_secs,
        );

        if cache_ttl_secs > 0 {
            if let Some(mut cached_payload) = web_search_cache_get(&cache_key) {
                cached_payload["cache_hit"] = serde_json::json!(true);
                cached_payload["cache_ttl_secs"] = serde_json::json!(cache_ttl_secs);
                return serde_json::to_string(&cached_payload).map_err(Into::into);
            }
        }

        let mut attempted_providers = Vec::new();
        let mut fallback_reason: Option<String> = None;

        let response = match requested_provider {
            WebSearchProvider::DuckDuckGo => {
                attempted_providers.push(WebSearchProvider::DuckDuckGo.as_str().to_string());
                execute_duckduckgo_search(
                    policy,
                    &query,
                    resolve_duckduckgo_engine_url(&args),
                    requested_max_results.clamp(1, 20),
                    timeout_secs,
                )?
            }
            WebSearchProvider::Google => {
                attempted_providers.push(WebSearchProvider::Google.as_str().to_string());
                let api_key = resolve_google_api_key(&args)?;
                let engine_id = resolve_google_engine_id(&args)?;
                execute_google_search(
                    policy,
                    &query,
                    resolve_google_engine_url(&args),
                    api_key,
                    engine_id,
                    requested_max_results.clamp(1, 10),
                    timeout_secs,
                )?
            }
            WebSearchProvider::Auto => {
                let ddg_engine_url = resolve_duckduckgo_engine_url(&args);
                let google_engine_url = resolve_google_engine_url(&args);
                let ddg_max_results = requested_max_results.clamp(1, 20);
                let google_max_results = requested_max_results.clamp(1, 10);

                let google_credentials = resolve_google_api_key(&args).and_then(|api_key| {
                    resolve_google_engine_id(&args).map(|engine_id| (api_key, engine_id))
                });

                match google_credentials {
                    Ok((api_key, engine_id)) => {
                        attempted_providers.push(WebSearchProvider::Google.as_str().to_string());
                        match execute_google_search(
                            policy,
                            &query,
                            google_engine_url,
                            api_key,
                            engine_id,
                            google_max_results,
                            timeout_secs,
                        ) {
                            Ok(google_response) if !google_response.results.is_empty() => {
                                google_response
                            }
                            Ok(_) => {
                                fallback_reason = Some("google returned no results".to_string());
                                attempted_providers
                                    .push(WebSearchProvider::DuckDuckGo.as_str().to_string());
                                execute_duckduckgo_search(
                                    policy,
                                    &query,
                                    ddg_engine_url,
                                    ddg_max_results,
                                    timeout_secs,
                                )?
                            }
                            Err(google_err) => {
                                let google_error_text = google_err.to_string();
                                fallback_reason =
                                    Some(format!("google provider failed: {google_error_text}"));
                                attempted_providers
                                    .push(WebSearchProvider::DuckDuckGo.as_str().to_string());
                                match execute_duckduckgo_search(
                                    policy,
                                    &query,
                                    ddg_engine_url,
                                    ddg_max_results,
                                    timeout_secs,
                                ) {
                                    Ok(ddg_response) => ddg_response,
                                    Err(ddg_err) => {
                                        return Err(anyhow::Error::from(tagged::Tagged::resource(
                                            anyhow::anyhow!(
                                                "web.search auto provider failed: google error: {}; duckduckgo error: {}",
                                                google_error_text,
                                                ddg_err
                                            ),
                                        )));
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {
                        fallback_reason =
                            Some("google credentials unavailable; used duckduckgo".to_string());
                        attempted_providers
                            .push(WebSearchProvider::DuckDuckGo.as_str().to_string());
                        execute_duckduckgo_search(
                            policy,
                            &query,
                            ddg_engine_url,
                            ddg_max_results,
                            timeout_secs,
                        )?
                    }
                }
            }
        };

        let mut payload = web_search_response_to_payload(&query, response);
        payload["requested_provider"] = serde_json::json!(requested_provider.as_str());
        payload["attempted_providers"] = serde_json::json!(attempted_providers);
        if let Some(reason) = fallback_reason {
            payload["fallback_reason"] = serde_json::json!(reason);
        }
        payload["cache_hit"] = serde_json::json!(false);
        payload["cache_ttl_secs"] = serde_json::json!(cache_ttl_secs);

        if cache_ttl_secs > 0 {
            web_search_cache_put(cache_key, payload.clone(), cache_ttl_secs);
        }

        serde_json::to_string(&payload).map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Web Fetch Tool
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WebFetchArgs {
    url: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_chars: Option<usize>,
}

pub struct WebFetchTool;

impl NativeTool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web.fetch"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::NetworkAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Fetch a web page by URL and return its textual payload.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "timeout_secs": { "type": "integer", "minimum": 5, "maximum": 120 },
                    "max_chars": { "type": "integer", "minimum": 512, "maximum": 200000 }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: WebFetchArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.url.trim().is_empty(), "url must not be empty");
        let host = extract_host(&args.url)?;
        if !policy.can_connect_net(&host) {
            return Err(anyhow::Error::from(tagged::Tagged::permission(
                anyhow::anyhow!(
                    "Permission Denied: NetworkAccess does not allow host '{}'",
                    host
                ),
            )));
        }

        let timeout_secs = args.timeout_secs.unwrap_or(20).clamp(5, 120);
        let max_chars = args.max_chars.unwrap_or(20_000).clamp(512, 200_000);
        let fetch_url = args.url.clone();
        let (status_code, content_type, body) = block_on_http(async move {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| anyhow::anyhow!("web.fetch client build failed: {}", e))?;
            let response = client
                .get(&fetch_url)
                .timeout(StdDuration::from_secs(timeout_secs))
                .send()
                .await
                .map_err(|e| {
                    anyhow::Error::from(tagged::Tagged::resource(anyhow::anyhow!(
                        "web.fetch request failed: {}",
                        e
                    )))
                })?;

            let status = response.status();
            if !status.is_success() {
                return Err(anyhow::Error::from(tagged::Tagged::resource(
                    anyhow::anyhow!("web.fetch request failed with status {}", status),
                )));
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string());
            let body = response.text().await.map_err(|e| {
                anyhow::Error::from(tagged::Tagged::execution(anyhow::anyhow!(
                    "web.fetch could not decode text response: {}",
                    e
                )))
            })?;
            Ok((status.as_u16(), content_type, body))
        })?;

        let total_chars = body.chars().count();
        let truncated = total_chars > max_chars;
        let content = if truncated {
            body.chars().take(max_chars).collect::<String>()
        } else {
            body
        };

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "url": args.url,
            "status_code": status_code,
            "content_type": content_type,
            "truncated": truncated,
            "total_chars": total_chars,
            "content": content
        }))
        .map_err(Into::into)
    }
}

/// Short scoped alias for agents (`artifact_ref`); global uniqueness enforced by SQLite primary key.
fn mint_artifact_ref_id() -> String {
    let b = *uuid::Uuid::new_v4().as_bytes();
    format!(
        "ar.{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5]
    )
}

// ---------------------------------------------------------------------------
// Artifact Build Tool
// ---------------------------------------------------------------------------

/// Builds an immutable artifact bundle from session-visible content.
/// Artifacts are the only units that may be reviewed, installed, or executed
/// beyond scratch use.
pub struct ArtifactBuildTool;

impl NativeTool for ArtifactBuildTool {
    fn name(&self) -> &'static str {
        "artifact.build"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::WriteAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Build an immutable artifact bundle from session content. Returns an artifact ID for review/install/closed-boundary execution. Artifacts are specialist-boundary objects: use them for evaluation, installation, and reproducible execution. For ordinary parent-child output handoff, prefer the implicit output from workflow.wait instead.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "inputs": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of content names or handles to include in the artifact"
                    },
                    "entrypoints": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of entrypoint filenames (must be in inputs)"
                    },
                    "layers": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "layer_id": { "type": "string" },
                                "name": { "type": "string" },
                                "mount_path": { "type": "string" },
                                "digest": { "type": "string" }
                            },
                            "required": ["layer_id", "name", "mount_path", "digest"]
                        },
                        "description": "Optional list of layer references to include in the artifact"
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["binary", "skill_bundle", "agent_bundle", "dataset", "gateway_runtime", "report"],
                        "description": "Optional artifact kind for downstream policy checks. Defaults to 'binary'."
                    }
                },
                "required": ["inputs"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            inputs: Vec<String>,
            entrypoints: Option<Vec<String>>,
            layers: Option<Vec<autonoetic_types::layer::ArtifactLayer>>,
            kind: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Artifact store requires gateway directory to be configured");
        };

        let sid = _session_id.unwrap_or(&_manifest.agent.id);
        let store = crate::artifact_store::ArtifactStore::new(gw_dir)?;

        if let Some(ref layers) = args.layers {
            let layer_store = crate::layer_store::LayerStore::new(gw_dir, Default::default())?;
            for layer in layers {
                let manifest = layer_store.inspect(&layer.layer_id).map_err(|_| {
                    anyhow::anyhow!(
                        "Layer '{}' referenced in artifact.build does not exist in layer store",
                        layer.layer_id
                    )
                })?;
                if manifest.digest != layer.digest {
                    anyhow::bail!(
                        "Layer digest mismatch for '{}': artifact.build references digest '{}' but layer store has '{}'",
                        layer.layer_id,
                        layer.digest,
                        manifest.digest
                    );
                }
            }
        }

        let raw_kind = args.kind.clone();
        let kind = raw_kind
            .as_deref()
            .map(|raw| {
                serde_json::from_value::<autonoetic_types::artifact::ArtifactKind>(
                    serde_json::Value::String(raw.to_string()),
                )
            })
            .transpose()
            .map_err(|_| anyhow::anyhow!("Invalid artifact kind '{}'", raw_kind.unwrap_or_default()))?
            .unwrap_or(autonoetic_types::artifact::ArtifactKind::Binary);

        let bundle = store.build_with_kind(
            &args.inputs,
            args.entrypoints.as_deref(),
            args.layers.as_deref(),
            kind.clone(),
            sid,
        )?;

        let root = crate::runtime::content_store::root_session_id(sid);
        let (scope_type, scope_id) = match config {
            Some(cfg) => {
                match crate::scheduler::workflow_store::resolve_workflow_id_for_root_session(
                    cfg, root,
                ) {
                    Ok(Some(wf_id)) => (
                        autonoetic_types::artifact::ArtifactRefScopeType::Workflow,
                        wf_id,
                    ),
                    _ => (
                        autonoetic_types::artifact::ArtifactRefScopeType::Session,
                        sid.to_string(),
                    ),
                }
            }
            None => (
                autonoetic_types::artifact::ArtifactRefScopeType::Session,
                sid.to_string(),
            ),
        };

        let mut artifact_ref: Option<String> = None;
        let mut artifact_ref_scope: Option<serde_json::Value> = None;
        if let Some(gs) = gateway_store {
            // New bundle only: dedup reuse returns the same artifact_id/digest; avoid minting
            // a fresh short ref row on every identical build.
            if !bundle.reused {
                let ref_id = mint_artifact_ref_id();
                let record = autonoetic_types::artifact::ArtifactRefRecord {
                    ref_id: ref_id.clone(),
                    scope_type,
                    scope_id: scope_id.clone(),
                    artifact_id: bundle.artifact_id.clone(),
                    artifact_digest: bundle.digest.clone(),
                    created_by_agent_id: _manifest.agent.id.clone(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                    expires_at: None,
                    revoked_at: None,
                };
                gs.create_artifact_ref(&record)?;
                artifact_ref = Some(ref_id);
                artifact_ref_scope = Some(serde_json::json!({
                    "type": scope_type.as_str(),
                    "id": scope_id,
                }));
            }
        }

        let mut out = serde_json::json!({
            "ok": true,
            "artifact_id": bundle.artifact_id,
            "kind": serde_json::to_value(&bundle.kind)
                .unwrap_or(serde_json::Value::String("binary".to_string())),
            "digest": bundle.digest,
            "artifact_digest": bundle.digest,
            "files": bundle.files.iter().map(|f| serde_json::json!({
                "name": f.name,
                "handle": f.handle,
                "alias": f.alias,
            })).collect::<Vec<_>>(),
            "entrypoints": bundle.entrypoints,
            "created_at": bundle.created_at,
            "reused": bundle.reused,
            "message": if bundle.reused {
                "Reused existing artifact with same inputs"
            } else {
                "Created new artifact"
            }
        });
        if let (Some(r), Some(scope)) = (artifact_ref, artifact_ref_scope) {
            if let Some(obj) = out.as_object_mut() {
                obj.insert("artifact_ref".to_string(), serde_json::Value::String(r));
                obj.insert("artifact_ref_scope".to_string(), scope);
            }
        }
        serde_json::to_string(&out).map_err(Into::into)
    }

    fn extract_metadata(&self, arguments_json: &str) -> ToolMetadata {
        let mut meta = ToolMetadata::default();
        if let Ok(parsed_args) = serde_json::from_str::<serde_json::Value>(arguments_json) {
            if let Some(inputs) = parsed_args.get("inputs").and_then(|v| v.as_array()) {
                if let Some(first) = inputs.first().and_then(|v| v.as_str()) {
                    meta.path = Some(first.to_string());
                }
            }
        }
        meta
    }
}

// ---------------------------------------------------------------------------
// Artifact Inspect Tool
// ---------------------------------------------------------------------------

/// Inspects an artifact by ID — returns its manifest, file list, and metadata.
/// Used by evaluator/auditor to understand what they are reviewing.
pub struct ArtifactInspectTool;

impl NativeTool for ArtifactInspectTool {
    fn name(&self) -> &'static str {
        "artifact.inspect"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Inspect an artifact by ID. Returns file list, entrypoints, layers, digests, and metadata. Use this for specialist-boundary review (evaluation, audit, installation). For ordinary content sharing between agents, prefer content.read with implicit output handles from workflow.wait.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "The artifact ID to inspect (e.g., 'art_a1b2c3d4')"
                    }
                },
                "required": ["artifact_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            artifact_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Artifact store requires gateway directory to be configured");
        };

        let store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
        let bundle = store.inspect(&args.artifact_id)?;

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "artifact_id": bundle.artifact_id,
            "digest": bundle.digest,
            "files": bundle.files.iter().map(|f| serde_json::json!({
                "name": f.name,
                "alias": f.alias,
            })).collect::<Vec<_>>(),
            "layers": bundle.layers.iter().map(|l| serde_json::json!({
                "layer_id": l.layer_id,
                "name": l.name,
                "mount_path": l.mount_path,
                "digest": l.digest,
            })).collect::<Vec<_>>(),
            "entrypoints": bundle.entrypoints,
            "created_at": bundle.created_at,
            "builder_session_id": bundle.builder_session_id,
        }))
        .map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Artifact Resolve Ref Tool
// ---------------------------------------------------------------------------

/// Resolves a short scoped artifact reference to its canonical artifact identity.
///
/// This tool provides the agent contract for resolving short refs (e.g., "ar.wf9f3.004.k7p2")
/// that child tasks emit, without requiring inlined file handles in natural language output.
pub struct ArtifactResolveRefTool;

impl NativeTool for ArtifactResolveRefTool {
    fn name(&self) -> &'static str {
        "artifact.resolve_ref"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Resolve a short scoped artifact reference to its canonical artifact identity. Use this to inspect artifacts passed from child tasks without inlined file handles. Fails hard if the ref is missing, expired, revoked, or has a digest mismatch.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "ref_id": {
                        "type": "string",
                        "description": "The short artifact reference ID (e.g., 'ar.wf9f3.004.k7p2')"
                    },
                    "scope_type": {
                        "type": "string",
                        "enum": ["session", "workflow", "global"],
                        "description": "The scope namespace: 'session', 'workflow', or 'global'"
                    },
                    "scope_id": {
                        "type": "string",
                        "description": "The scope ID: session_id, workflow_id, or '__global__'"
                    }
                },
                "required": ["ref_id", "scope_type", "scope_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            ref_id: String,
            scope_type: String,
            scope_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.ref_id.trim().is_empty(), "ref_id must not be empty");
        anyhow::ensure!(
            !args.scope_id.trim().is_empty(),
            "scope_id must not be empty"
        );

        let scope_type =
            autonoetic_types::artifact::ArtifactRefScopeType::from_str(&args.scope_type)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Invalid scope_type '{}'. Must be 'session', 'workflow', or 'global'.",
                        args.scope_type
                    )
                })?;

        let Some(store) = gateway_store else {
            anyhow::bail!("artifact.resolve_ref requires GatewayStore to be configured");
        };

        let Some(ref_record) =
            store.resolve_artifact_ref(scope_type, &args.scope_id, &args.ref_id)?
        else {
            return Err(anyhow::Error::from(
                autonoetic_types::tool_error::tagged::Tagged::validation(anyhow::anyhow!(
                    "Artifact ref '{}' not found in {} scope '{}', or it is expired/revoked.",
                    args.ref_id,
                    scope_type.as_str(),
                    args.scope_id
                )),
            )
            .into());
        };

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("artifact.resolve_ref requires gateway directory to be configured");
        };

        let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
        let bundle = artifact_store.inspect(&ref_record.artifact_id)?;

        if bundle.digest != ref_record.artifact_digest {
            return Err(anyhow::Error::from(autonoetic_types::tool_error::tagged::Tagged::validation(
                anyhow::anyhow!(
                    "Artifact digest mismatch for ref '{}'. Ref claims '{}' but artifact manifest has '{}'. Possible tampering or corruption.",
                    args.ref_id,
                    ref_record.artifact_digest,
                    bundle.digest
                )
            )).into());
        }

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "artifact_id": bundle.artifact_id,
            "artifact_digest": bundle.digest,
            "files": bundle.files.iter().map(|f| serde_json::json!({
                "name": f.name,
                "handle": f.handle,
                "alias": f.alias,
            })).collect::<Vec<_>>(),
            "entrypoints": bundle.entrypoints,
            "created_at": bundle.created_at,
            "builder_session_id": bundle.builder_session_id,
            "ref_created_at": ref_record.created_at,
            "ref_created_by": ref_record.created_by_agent_id,
        }))
        .map_err(Into::into)
    }
}


// Knowledge Store Tool (renamed from memory.remember)
// ---------------------------------------------------------------------------

/// Stores a durable fact in the gateway's knowledge base (Tier 2 memory).
///
/// Knowledge is stored with full provenance tracking and can be shared across agents.
pub struct KnowledgeStoreTool;

impl NativeTool for KnowledgeStoreTool {
    fn name(&self) -> &'static str {
        "knowledge.store"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        // Uses WriteAccess capability (same as memory.remember)
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::WriteAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Store a durable fact in the knowledge base. Knowledge persists across sessions and can be shared with other agents. Each fact includes provenance tracking (who wrote it, when, from what source).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Unique identifier for this knowledge" },
                    "content": { "type": "string", "description": "The fact or information to store" },
                    "scope": { "type": "string", "description": "Category/namespace for organizing knowledge (e.g., 'api-keys', 'user-preferences')", "default": "general" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Tags for searchability" },
                    "confidence": { "type": "number", "description": "Confidence level (0.0 to 1.0)", "default": 1.0 }
                },
                "required": ["id", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
            content: String,
            #[serde(default = "default_scope")]
            scope: String,
            #[serde(default)]
            tags: Vec<String>,
            #[serde(default = "default_confidence")]
            confidence: f64,
        }
        fn default_scope() -> String {
            "general".to_string()
        }
        fn default_confidence() -> f64 {
            1.0
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.id.trim().is_empty(), "id must not be empty");
        anyhow::ensure!(!args.content.trim().is_empty(), "content must not be empty");
        anyhow::ensure!(
            args.confidence >= 0.0 && args.confidence <= 1.0,
            "confidence must be between 0.0 and 1.0"
        );

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Knowledge requires gateway directory to be configured");
        };

        let sid = session_id.unwrap_or(&manifest.agent.id);
        let source_ref = match turn_id {
            Some(tid) => format!("session:{}:turn:{}", sid, tid),
            None => format!("session:{}", sid),
        };

        let mem = tier2_memory_for_native_tool(gw_dir, gateway_store.as_ref(), &manifest.agent.id)?;

        let mut memory = autonoetic_types::memory::MemoryObject::new(
            args.id.clone(),
            args.scope.clone(),
            manifest.agent.id.clone(),
            manifest.agent.id.clone(),
            source_ref,
            args.content.clone(),
        );
        memory.confidence = Some(args.confidence);
        memory.tags = args.tags.clone();
        let memory = mem.save_memory(&memory)?;

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "id": memory.memory_id,
            "scope": memory.scope,
            "content_hash": memory.content_hash,
            "created_at": memory.created_at,
        }))
        .map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Knowledge Recall Tool (renamed from memory.recall)
// ---------------------------------------------------------------------------

/// Retrieves a durable fact from the knowledge base by ID.
pub struct KnowledgeRecallTool;

impl NativeTool for KnowledgeRecallTool {
    fn name(&self) -> &'static str {
        "knowledge.recall"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        // Uses ReadAccess capability (same as memory.recall)
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Recall a durable fact from the knowledge base by its ID. Respects visibility and access control - you can only recall knowledge you have access to.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The knowledge ID to recall" }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.id.trim().is_empty(), "id must not be empty");

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Knowledge requires gateway directory to be configured");
        };

        let mem = tier2_memory_for_native_tool(gw_dir, gateway_store.as_ref(), &manifest.agent.id)?;
        let memory = mem.recall(&args.id)?;

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "id": memory.memory_id,
            "content": memory.content,
            "scope": memory.scope,
            "writer": memory.writer_agent_id,
            "created_at": memory.created_at,
            "confidence": memory.confidence,
        }))
        .map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Knowledge Search Tool (renamed from memory.search)
// ---------------------------------------------------------------------------

/// Searches the knowledge base by scope and optional query.
pub struct KnowledgeSearchTool;

impl NativeTool for KnowledgeSearchTool {
    fn name(&self) -> &'static str {
        "knowledge.search"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        // Search is included in ReadAccess capability
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search the knowledge base by scope and optional query. Returns all knowledge in the scope that you have access to, optionally filtered by content matching the query.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "description": "The scope/namespace to search in (e.g., 'api-keys', 'user-preferences')" },
                    "query": { "type": "string", "description": "Optional search term to filter by content" }
                },
                "required": ["scope"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            scope: String,
            query: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.scope.trim().is_empty(), "scope must not be empty");

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Knowledge requires gateway directory to be configured");
        };

        let mem = tier2_memory_for_native_tool(gw_dir, gateway_store.as_ref(), &manifest.agent.id)?;
        let results = mem.search(&args.scope, args.query.as_deref())?;

        let items: Vec<serde_json::Value> = results
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.memory_id,
                    "content": m.content,
                    "writer": m.writer_agent_id,
                    "created_at": m.created_at,
                    "confidence": m.confidence,
                })
            })
            .collect();

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "scope": args.scope,
            "results": items,
            "count": items.len(),
        }))
        .map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Knowledge search by tags (Tier 2 JSON tag array)
// ---------------------------------------------------------------------------

/// Searches the knowledge base by scope, requiring every listed tag on the stored JSON `tags` array.
pub struct KnowledgeSearchByTagsTool;

impl NativeTool for KnowledgeSearchByTagsTool {
    fn name(&self) -> &'static str {
        "knowledge.search_by_tags"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search the knowledge base by scope and tags. Each result's `tags` JSON array must contain every tag you pass (AND semantics). Optional `text` filters `content` with a SQL LIKE substring match.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "description": "Scope/namespace (e.g. 'lessons', 'general')" },
                    "tags": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "All of these tag strings must appear in the record's tags list" },
                    "text": { "type": "string", "description": "Optional substring filter on content" },
                    "limit": { "type": "integer", "description": "Max results (1–100)", "default": 10 }
                },
                "required": ["scope", "tags"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            scope: String,
            tags: Vec<String>,
            text: Option<String>,
            #[serde(default = "default_limit")]
            limit: u32,
        }
        fn default_limit() -> u32 {
            10
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.scope.trim().is_empty(), "scope must not be empty");
        anyhow::ensure!(!args.tags.is_empty(), "tags must be a non-empty array");
        anyhow::ensure!(
            (1..=100).contains(&args.limit),
            "limit must be between 1 and 100 inclusive"
        );
        let limit = args.limit as usize;

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Knowledge requires gateway directory to be configured");
        };

        let mem = tier2_memory_for_native_tool(gw_dir, gateway_store.as_ref(), &manifest.agent.id)?;
        let results = mem.search_by_tags(&args.scope, &args.tags, args.text.as_deref(), limit)?;

        let items: Vec<serde_json::Value> = results
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.memory_id,
                    "content": m.content,
                    "scope": m.scope,
                    "tags": m.tags,
                    "writer": m.writer_agent_id,
                    "created_at": m.created_at,
                    "confidence": m.confidence,
                })
            })
            .collect();

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "scope": args.scope,
            "tags": args.tags,
            "results": items,
            "count": items.len(),
        }))
        .map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Digest query (Tier-2 memories + post-session narrative)
// ---------------------------------------------------------------------------

/// Truncate to at most `max_chars` Unicode scalar values without splitting codepoints.
fn truncate_narrative_to_char_boundary(s: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(1);
    let mut count = 0usize;
    let mut end_byte = 0usize;
    for (i, c) in s.char_indices() {
        if count >= max_chars {
            break;
        }
        count += 1;
        end_byte = i + c.len_utf8();
    }
    if end_byte >= s.len() {
        s.to_string()
    } else {
        format!("{}… (truncated)", &s[..end_byte])
    }
}

/// Combines tag-based memory search with the stored post-session narrative for a root session.
pub struct DigestQueryTool;

impl NativeTool for DigestQueryTool {
    fn name(&self) -> &'static str {
        "digest.query"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search digest-scoped Tier-2 memories by scope and tags, and optionally load the post-session narrative: either as `post_session_narrative.md` for the session root, or by explicit content handle/alias via `narrative_handle` (uses the same resolution rules as `content.read`).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "description": "Memory scope/namespace (e.g. 'digest.lesson')" },
                    "tags": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "AND-matched tags on the memory record" },
                    "text": { "type": "string", "description": "Optional substring filter on memory content" },
                    "session_id": { "type": "string", "description": "Session id for resolving narrative by name or handle (see `narrative_handle`). If omitted, the active tool session id is used when available." },
                    "narrative_handle": { "type": "string", "description": "Optional content handle (sha256:…), short alias, or name for the post-session narrative blob. Requires `session_id` or an active tool session for visibility checks." },
                    "narrative_max_chars": { "type": "integer", "description": "Max Unicode scalars of narrative to return (default 16000)", "default": 16000 },
                    "limit": { "type": "integer", "description": "Max memory results (1–100, default 10)", "default": 10 }
                },
                "required": ["scope", "tags"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            scope: String,
            tags: Vec<String>,
            text: Option<String>,
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            narrative_handle: Option<String>,
            #[serde(default = "default_narrative_cap")]
            narrative_max_chars: usize,
            #[serde(default = "default_limit")]
            limit: u32,
        }
        fn default_limit() -> u32 {
            10
        }
        fn default_narrative_cap() -> usize {
            16_000
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.scope.trim().is_empty(), "scope must not be empty");
        anyhow::ensure!(!args.tags.is_empty(), "tags must be non-empty");
        anyhow::ensure!((1..=100).contains(&args.limit), "limit must be 1–100");

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("digest.query requires gateway directory");
        };

        let mem = tier2_memory_for_native_tool(gw_dir, gateway_store.as_ref(), &manifest.agent.id)?;
        let results = mem.search_by_tags(
            &args.scope,
            &args.tags,
            args.text.as_deref(),
            args.limit as usize,
        )?;

        let items: Vec<serde_json::Value> = results
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.memory_id,
                    "content": m.content,
                    "scope": m.scope,
                    "tags": m.tags,
                    "writer": m.writer_agent_id,
                    "created_at": m.created_at,
                    "confidence": m.confidence,
                })
            })
            .collect();

        let sid_for_narrative = args
            .session_id
            .as_deref()
            .or(session_id)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let narrative = if let Some(ref raw) = args.narrative_handle {
            let nh = raw.trim();
            anyhow::ensure!(
                !nh.is_empty(),
                "narrative_handle must be non-empty when provided"
            );
            let sid = sid_for_narrative.ok_or_else(|| {
                anyhow::anyhow!(
                    "digest.query narrative_handle requires session_id (argument) or an active tool session context"
                )
            })?;
            let store = crate::runtime::content_store::ContentStore::new(gw_dir)?;
            let bytes = store.read_by_name_or_handle(sid, nh)?;
            let text = String::from_utf8(bytes)
                .map_err(|e| anyhow::anyhow!("narrative content is not valid UTF-8: {e}"))?;
            let truncated =
                truncate_narrative_to_char_boundary(&text, args.narrative_max_chars.max(1));
            Some(serde_json::json!({
                "session_id": sid,
                "handle_or_name": nh,
                "text": truncated,
            }))
        } else if let Some(sid_raw) = sid_for_narrative {
            let base = crate::runtime::live_digest::base_session_id(sid_raw).to_string();
            let store = crate::runtime::content_store::ContentStore::new(gw_dir)?;
            match store.read_by_name(
                &base,
                crate::runtime::post_session_digest::POST_SESSION_NARRATIVE_CONTENT_NAME,
            ) {
                Ok(bytes) => {
                    let text = String::from_utf8(bytes).map_err(|e| {
                        anyhow::anyhow!("post_session_narrative.md is not valid UTF-8: {e}")
                    })?;
                    let truncated =
                        truncate_narrative_to_char_boundary(&text, args.narrative_max_chars.max(1));
                    Some(serde_json::json!({
                        "root_session_id": base,
                        "name": crate::runtime::post_session_digest::POST_SESSION_NARRATIVE_CONTENT_NAME,
                        "text": truncated,
                    }))
                }
                Err(_) => None,
            }
        } else {
            None
        };

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "scope": args.scope,
            "tags": args.tags,
            "memories": items,
            "memory_count": items.len(),
            "narrative": narrative,
        }))
        .map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Knowledge Share Tool (renamed from memory.share)
// ---------------------------------------------------------------------------

/// Shares knowledge with specific agents.
pub struct KnowledgeShareTool;

impl NativeTool for KnowledgeShareTool {
    fn name(&self) -> &'static str {
        "knowledge.share"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        // Sharing is included in WriteAccess capability
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::WriteAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Share knowledge with specific agents. Requires ownership or write access to the knowledge. Once shared, the target agents can recall and search for this knowledge.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The knowledge ID to share" },
                    "with_agents": { "type": "array", "items": { "type": "string" }, "description": "List of agent IDs to share with" }
                },
                "required": ["id", "with_agents"],
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
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            id: String,
            with_agents: Vec<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.id.trim().is_empty(), "id must not be empty");
        anyhow::ensure!(
            !args.with_agents.is_empty(),
            "with_agents must not be empty"
        );

        // Check if agent is allowed to share with these targets
        for target in &args.with_agents {
            anyhow::ensure!(
                policy.can_share_memory(target),
                "Cannot share knowledge with agent '{}': not in allowed_targets",
                target
            );
        }

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Knowledge requires gateway directory to be configured");
        };

        let mem = tier2_memory_for_native_tool(gw_dir, gateway_store.as_ref(), &manifest.agent.id)?;
        let memory = mem.share_with(&args.id, args.with_agents.clone())?;

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "id": memory.memory_id,
            "visibility": "shared",
            "allowed_agents": memory.allowed_agents,
        }))
        .map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Skill Draft Tool
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Agent Install Tool
// ---------------------------------------------------------------------------

/// A file to be installed as part of an agent.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct InstallAgentFile {
    pub path: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Agent Spawn Tool
// ---------------------------------------------------------------------------

/// Allows agents to request help when stuck.
/// Supports escalation to reasoning LLM, specialist agent, or human.
pub struct SessionEscalateTool;

impl NativeTool for SessionEscalateTool {
    fn name(&self) -> &'static str {
        "session.escalate"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        // Available to all agents
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Request help when stuck. Use this when you've tried reasonable approaches but cannot proceed correctly.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "Clear explanation of why you're stuck"
                    },
                    "context": {
                        "type": "string",
                        "description": "Relevant context: what you tried, what failed, error messages"
                    },
                    "target": {
                        "type": "string",
                        "enum": ["reasoning_llm", "specialist", "human"],
                        "default": "reasoning_llm",
                        "description": "Who to ask for help"
                    },
                    "urgency": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "default": "medium"
                    },
                    "suggested_actions": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Possible next steps you're considering (helps target respond better)"
                    }
                },
                "required": ["reason", "context"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            reason: String,
            context: String,
            #[serde(default = "default_target")]
            target: String,
            #[serde(default = "default_urgency")]
            urgency: String,
            #[serde(default)]
            suggested_actions: Option<Vec<String>>,
        }

        fn default_target() -> String {
            "reasoning_llm".to_string()
        }

        fn default_urgency() -> String {
            "medium".to_string()
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        // Resolve workflow_id for event emission
        let workflow_id = session_id
            .map(|sid| {
                let root = crate::runtime::content_store::root_session_id(sid);
                let agents_dir = agent_dir.parent().unwrap_or(agent_dir);
                let fallback_config = GatewayConfig {
                    agents_dir: agents_dir.to_path_buf(),
                    ..GatewayConfig::default()
                };
                let gw_config = config.unwrap_or(&fallback_config);
                crate::scheduler::resolve_workflow_id_for_root_session(gw_config, &root)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "unknown".to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());

        let suggested_actions = args.suggested_actions.clone().unwrap_or_default();

        let mut response = match args.target.as_str() {
            "reasoning_llm" => {
                serde_json::json!({
                    "escalation_type": "reasoning_llm",
                    "analysis": format!(
                        "Based on your situation:\n\nProblem: {}\n\nContext: {}\n\nSuggestions:\n1. Review your assumptions - check if you're working with correct data/parameters\n2. Break down the problem into smaller steps\n3. Consider alternative approaches you may have overlooked",
                        args.reason, args.context
                    ),
                    "confidence": "medium",
                    "next_steps": suggested_actions.clone()
                })
            }
            "specialist" => {
                serde_json::json!({
                    "escalation_type": "specialist",
                    "message": "To escalate to a specialist agent, use agent.spawn() with the appropriate specialist (e.g., 'researcher.default', 'architect.default', 'debugger.default')",
                    "suggested_specialists": [
                        "researcher.default - for information gathering and analysis",
                        "architect.default - for structural design and planning",
                        "debugger.default - for troubleshooting and root cause analysis",
                        "evaluator.default - for testing and validation",
                        "auditor.default - for security and compliance review"
                    ],
                    "original_reason": args.reason,
                    "original_context": args.context
                })
            }
            "human" => {
                serde_json::json!({
                    "escalation_type": "human",
                    "message": "This escalation has been logged. A human operator will review your request.",
                    "urgency": args.urgency,
                    "reason": args.reason,
                    "context": args.context,
                    "suggested_actions": suggested_actions.clone(),
                    "note": "You should EndTurn after escalating to human to allow them to review and respond."
                })
            }
            _ => {
                serde_json::json!({
                    "error": "Unknown escalation target",
                    "valid_targets": ["reasoning_llm", "specialist", "human"]
                })
            }
        };

        // Emit workflow.escalated event for visibility in chat TUI and digest
        let event = autonoetic_types::workflow::WorkflowEventRecord {
            event_id: format!("esc-{}", uuid::Uuid::new_v4()),
            workflow_id: workflow_id.clone(),
            task_id: None,
            event_type: "workflow.escalated".to_string(),
            agent_id: Some(manifest.agent.id.clone()),
            payload: serde_json::json!({
                "target": args.target,
                "urgency": args.urgency,
                "reason": args.reason,
                "context": args.context,
                "suggested_actions": suggested_actions,
            }),
            occurred_at: chrono::Utc::now().to_rfc3339(),
        };
        let _ = crate::scheduler::workflow_store::append_workflow_event(
            config.unwrap_or(&GatewayConfig::default()),
            gateway_store.as_deref(),
            &event,
        );

        // Add escalation metadata to response
        response["escalation_id"] = serde_json::json!(event.event_id);
        response["workflow_id"] = serde_json::json!(workflow_id);

        serde_json::to_string(&response).map_err(Into::into)
    }
}

#[derive(Debug, Deserialize)]
struct SpawnAgentArgs {
    agent_id: String,
    message: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(default)]
    session_id: Option<String>,
    /// When true, enqueue the task for async execution and return immediately with task_id.
    /// The scheduler will execute the child agent in the background.
    /// Use workflow.wait to check task status.
    #[serde(default)]
    r#async: bool,
    /// Join group name. Tasks in the same join group are awaited together by the planner.
    #[serde(default)]
    join_group: Option<String>,
}

pub struct AgentSpawnTool;

impl NativeTool for AgentSpawnTool {
    fn name(&self) -> &'static str {
        "agent.spawn"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentSpawn { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Delegate a task to a specialist agent. With async=false (default), blocks until the child completes and returns its reply. With async=true, returns immediately with a task_id — use workflow.wait to check status. Spawn multiple children in parallel with async=true, then wait for all of them.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "message": { "type": "string" },
                    "metadata": { "type": "object" },
                    "session_id": { "type": "string" },
                    "async": { "type": "boolean", "description": "If true, enqueue for background execution and return immediately with task_id. Default: false (synchronous)." },
                    "join_group": { "type": "string", "description": "Optional group name for join semantics. Tasks in the same group are awaited together before planner resumes." }
                },
                "required": ["agent_id", "message"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: SpawnAgentArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;
        validate_agent_id(&args.agent_id)?;
        anyhow::ensure!(!args.message.trim().is_empty(), "message must not be empty");

        // Schema enforcement hook
        let default_enforcement_config = SchemaEnforcementConfig::default();
        let enforcement_config = config
            .map(|c| &c.schema_enforcement)
            .unwrap_or(&default_enforcement_config);

        if enforcement_config.mode != SchemaEnforcementMode::Disabled {
            let agents_dir = agent_dir.parent().ok_or_else(|| {
                anyhow::anyhow!("Agent directory is missing its agents root parent")
            })?;
            let target_agent_path = agents_dir.join(&args.agent_id).join("SKILL.md");

            if target_agent_path.exists() {
                if let Ok(manifest_content) = std::fs::read_to_string(&target_agent_path) {
                    if let Some(frontmatter) = manifest_content.split("---").nth(1) {
                        if let Ok(target_manifest) =
                            serde_yaml::from_str::<AgentManifest>(frontmatter)
                        {
                            if let Some(io) = &target_manifest.io {
                                if let Some(accepts) = &io.accepts {
                                    let enforcer = default_enforcer();
                                    let payload = serde_json::json!({
                                        "message": args.message,
                                        "metadata": args.metadata,
                                        "session_id": args.session_id,
                                    });

                                    match enforcer.enforce(&payload, accepts) {
                                        EnforcementResult::Reject(details) => {
                                            return Err(anyhow::anyhow!(
                                                "Schema validation failed: {}. Hint: {}",
                                                details.reason,
                                                details.hint.unwrap_or_default()
                                            ));
                                        }
                                        EnforcementResult::Coerced(details) => {
                                            if enforcement_config.audit {
                                                tracing::info!(
                                                    target: "schema_enforcement",
                                                    agent_id = %args.agent_id,
                                                    transformations = ?details.transformations,
                                                    "Schema enforcement: payload coerced"
                                                );
                                            }
                                        }
                                        EnforcementResult::Pass => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let resolved_session_id = args
            .session_id
            .clone()
            .or_else(|| session_id.map(ToOwned::to_owned))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let agents_dir = agent_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is missing its agents root parent"))?;

        let fallback_gateway_config = GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        };
        let gw_config = config.unwrap_or(&fallback_gateway_config);

        // Role-agnostic: block synchronous delegation while any approval for this *root*
        // session is still pending. Async spawn (args.async) is NOT blocked — it queues
        // independently and the scheduler picks it up regardless of approval state.
        if !args.r#async {
            let root_for_approval_check =
                crate::runtime::content_store::root_session_id(&resolved_session_id);
            let pending = crate::scheduler::approval::pending_approval_requests_for_root(
                gw_config,
                gateway_store.as_deref(),
                &root_for_approval_check,
            )?;
            if !pending.is_empty() {
                let ids: Vec<String> = pending.iter().map(|r| r.request_id.clone()).collect();
                return Err(anyhow::anyhow!(
                    "Cannot delegate (agent.spawn) while approval(s) are pending for this session. Pending request id(s): {}. Approve or reject with `autonoetic gateway approvals approve|reject <id> --config <path>`, then continue.",
                    ids.join(", ")
                ));
            }
        }

        let root = crate::runtime::content_store::root_session_id(&resolved_session_id);
        let source_agent_id = manifest.agent.id.clone();
        let workflow = crate::scheduler::ensure_workflow_for_root_session(
            gw_config,
            gateway_store.as_deref(),
            &root,
            Some(source_agent_id.as_str()),
        )?;
        let workflow_id = workflow.workflow_id.clone();
        let task_id = crate::scheduler::new_task_id();

        let execution_config = GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        };
        let execution =
            crate::execution::GatewayExecutionService::new(execution_config, gateway_store.clone());

        let target_agent_id = args.agent_id.clone();
        let kickoff_message = match &args.metadata {
            Some(value) => format!("{}\n\nDelegation metadata: {}", args.message, value),
            None => args.message.clone(),
        };

        // Set up hierarchical content namespace for the child agent
        // The child gets a unique delegation path (e.g., "demo-session-1/coder-abc123")
        // so content written by the child is visible to the parent via the hierarchy
        let child_delegation_path = format!(
            "{}/{}-{}",
            resolved_session_id,
            args.agent_id,
            &uuid::Uuid::new_v4().to_string()[..8]
        );

        let ts = Utc::now().to_rfc3339();
        let task = TaskRun {
            task_id: task_id.clone(),
            workflow_id: workflow_id.clone(),
            agent_id: target_agent_id.clone(),
            session_id: child_delegation_path.clone(),
            parent_session_id: resolved_session_id.clone(),
            status: TaskRunStatus::Running,
            created_at: ts.clone(),
            updated_at: ts,
            source_agent_id: Some(source_agent_id.clone()),
            result_summary: None,
            join_group: None,
            message: Some(kickoff_message.clone()),
            metadata: args.metadata.clone(),
        };
        crate::scheduler::save_task_run(gw_config, gateway_store.as_deref(), &task)?;
        crate::scheduler::append_workflow_event(
            gw_config,
            gateway_store.as_deref(),
            &WorkflowEventRecord {
                event_id: format!("wevt-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                workflow_id: workflow_id.clone(),
                task_id: Some(task_id.clone()),
                event_type: "task.spawned".to_string(),
                agent_id: Some(target_agent_id.clone()),
                payload: serde_json::json!({
                    "target_agent_id": target_agent_id,
                    "child_session_id": child_delegation_path,
                    "parent_session_id": resolved_session_id,
                }),
                occurred_at: Utc::now().to_rfc3339(),
            },
        )?;

        crate::scheduler::workflow_causal::mirror_orchestration_event(
            gw_config,
            root,
            "workflow.task.spawned",
            EntryStatus::Success,
            serde_json::json!({
                "workflow_id": workflow_id,
                "task_id": task_id,
                "target_agent_id": target_agent_id,
                "child_session_id": child_delegation_path,
                "parent_session_id": resolved_session_id,
                "source_agent_id": source_agent_id,
            }),
        );

        // Set root session relationship so child's session-visible content is visible to parent
        // Must use the same gateway_dir as the execution engine, NOT agent_dir.parent()
        if let Some(gw_dir) = _gateway_dir {
            if let Ok(store) = crate::runtime::content_store::ContentStore::new(gw_dir) {
                if let Err(e) = store.set_root_session(&child_delegation_path, root) {
                    tracing::warn!(
                        target: "content_store",
                        error = %e,
                        parent_session = %resolved_session_id,
                        child_delegation = %child_delegation_path,
                        "Failed to set root session for child agent"
                    );
                } else {
                    tracing::info!(
                        target: "content_store",
                        parent_session = %resolved_session_id,
                        child_delegation = %child_delegation_path,
                        "Set up hierarchical content namespace for child agent"
                    );
                }
            }
        }

        // --- Async branch: enqueue and return immediately ---
        if args.r#async {
            let queued = autonoetic_types::workflow::QueuedTaskRun {
                task_id: task_id.clone(),
                workflow_id: workflow_id.clone(),
                agent_id: target_agent_id.clone(),
                message: kickoff_message,
                child_session_id: child_delegation_path.clone(),
                parent_session_id: resolved_session_id.clone(),
                source_agent_id: source_agent_id.clone(),
                metadata: args.metadata.clone(),
                join_group: args.join_group,
                blocks_planner: true,
                enqueued_at: Utc::now().to_rfc3339(),
            };
            crate::scheduler::enqueue_task(gw_config, gateway_store.as_deref(), &queued)?;

            // Update task status from Running → Pending (it's queued, not yet executing)
            let _ = crate::scheduler::update_task_run_status(
                gw_config,
                gateway_store.as_deref(),
                &workflow_id,
                &task_id,
                TaskRunStatus::Pending,
                Some("queued".to_string()),
            );

            return serde_json::to_string(&serde_json::json!({
                "ok": true,
                "accepted": true,
                "status": "queued",
                "workflow_id": workflow_id,
                "task_id": task_id,
                "agent_id": target_agent_id,
                "session_id": child_delegation_path,
                "message": "Task queued for async execution. Use workflow.wait with task_ids to check completion status."
            }))
            .map_err(Into::into);
        }

        // --- Synchronous branch (existing behavior) ---
        let wf_id_clone = workflow_id.clone();
        let tid_clone = task_id.clone();
        let spawn_future = async move {
            execution
                .spawn_agent_once(
                    &target_agent_id,
                    &kickoff_message,
                    &child_delegation_path, // Use delegation path as session_id for content namespace
                    Some(&source_agent_id),
                    false,
                    None,
                    args.metadata.as_ref(),
                    Some(&wf_id_clone),
                    Some(&tid_clone),
                )
                .await
        };

        let spawn_result = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(spawn_future))
        } else {
            tokio::runtime::Runtime::new()?.block_on(spawn_future)
        };

        match spawn_result {
            Ok(result) => {
                if let Some(request_id) = &result.suspended_for_approval {
                    let summary = format!("awaiting approval {}", request_id);
                    if let Err(e) = crate::scheduler::update_task_run_status(
                        gw_config,
                        gateway_store.as_deref(),
                        &workflow_id,
                        &task_id,
                        TaskRunStatus::AwaitingApproval,
                        Some(summary),
                    ) {
                        tracing::warn!(
                            target: "workflow",
                            error = %e,
                            workflow_id = %workflow_id,
                            task_id = %task_id,
                            "Failed to persist task awaiting approval status"
                        );
                    }

                    let _ = crate::scheduler::checkpoint_task(
                        gw_config,
                        gateway_store.as_deref(),
                        &workflow_id,
                        &task_id,
                        "awaiting_approval".to_string(),
                        serde_json::json!({
                            "status": "awaiting_approval",
                            "approval_request_id": request_id,
                        }),
                    );

                    // Return success — the task is queued and will resume after approval.
                    // The planner should call workflow.wait to get the result.
                    // Do NOT expose awaiting_approval status — the planner doesn't need to know.
                    let summary = format!(
                        "Task queued: {} will execute after approval ({}). Call workflow.wait to get the result.",
                        result.agent_id, request_id
                    );
                    return Ok(serde_json::json!({
                        "ok": true,
                        "status": "queued",
                        "workflow_id": workflow_id,
                        "task_id": task_id,
                        "agent_id": result.agent_id,
                        "session_id": result.session_id,
                        "result_summary": summary,
                        "files": result.files,
                    })
                    .to_string());
                }

                let summary = result.assistant_reply.as_ref().map(|s| {
                    const MAX: usize = 512;
                    if s.len() <= MAX {
                        s.clone()
                    } else {
                        format!("{}…", &s[..MAX])
                    }
                });
                if let Err(e) = crate::scheduler::update_task_run_status(
                    gw_config,
                    gateway_store.as_deref(),
                    &workflow_id,
                    &task_id,
                    TaskRunStatus::Succeeded,
                    summary,
                ) {
                    tracing::warn!(
                        target: "workflow",
                        error = %e,
                        workflow_id = %workflow_id,
                        task_id = %task_id,
                        "Failed to persist task completion status"
                    );
                }
                Ok(serde_json::json!({
                    "ok": true,
                    "status": "agent_spawned",
                    "workflow_id": workflow_id,
                    "task_id": task_id,
                    "agent_id": result.agent_id,
                    "session_id": result.session_id,
                    "assistant_reply": result.assistant_reply,
                    "artifacts": result.artifacts,
                    // All named content written by the child — use name/handle/alias with content.read
                    "files": result.files,
                    "shared_knowledge": result.shared_knowledge,
                    "llm_usage": result.llm_usage,
                })
                .to_string())
            }
            Err(e) => {
                if let Err(inner) = crate::scheduler::update_task_run_status(
                    gw_config,
                    gateway_store.as_deref(),
                    &workflow_id,
                    &task_id,
                    TaskRunStatus::Failed,
                    Some(e.to_string()),
                ) {
                    tracing::warn!(
                        target: "workflow",
                        error = %inner,
                        workflow_id = %workflow_id,
                        task_id = %task_id,
                        "Failed to persist task failure status"
                    );
                }
                Err(e)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Approval Status Tool
// ---------------------------------------------------------------------------

/// Query the status of an approval request.
/// Allows agents to check whether an approval is pending, approved, or rejected.
pub struct ApprovalStatusTool;

impl NativeTool for ApprovalStatusTool {
    fn name(&self) -> &'static str {
        "approval.status"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Query the status of an approval request. Returns the current status (pending, approved, rejected) and associated details.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "approval_id": {
                        "type": "string",
                        "description": "The approval request ID to check (e.g., 'apr-abc123')"
                    }
                },
                "required": ["approval_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true // Available to all agents
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            approval_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": true,
                "approval_id": args.approval_id,
                "status": "unknown",
                "message": "Gateway store not available"
            }))?);
        };

        match store.get_approval(&args.approval_id) {
            Ok(Some(request)) => {
                let status = match &request.status {
                    Some(s) => match s {
                        autonoetic_types::background::ApprovalStatus::Approved => "approved",
                        autonoetic_types::background::ApprovalStatus::Rejected => "rejected",
                        autonoetic_types::background::ApprovalStatus::Cancelled => "cancelled",
                    },
                    None => "pending",
                }
                .to_string();

                let response = serde_json::json!({
                    "ok": true,
                    "approval_id": args.approval_id,
                    "status": status,
                    "agent_id": request.agent_id,
                    "session_id": request.session_id,
                    "created_at": request.created_at,
                    "decided_at": request.decided_at,
                    "decided_by": request.decided_by,
                    "reason": request.reason,
                    "workflow_id": request.workflow_id,
                    "task_id": request.task_id
                });

                serde_json::to_string(&response).map_err(Into::into)
            }
            Ok(None) => {
                let response = serde_json::json!({
                    "ok": true,
                    "approval_id": args.approval_id,
                    "status": "not_found",
                    "message": "Approval request not found"
                });
                serde_json::to_string(&response).map_err(Into::into)
            }
            Err(e) => {
                let response = serde_json::json!({
                    "ok": false,
                    "approval_id": args.approval_id,
                    "error": e.to_string()
                });
                serde_json::to_string(&response).map_err(Into::into)
            }
        }
    }

    fn extract_metadata(&self, arguments_json: &str) -> ToolMetadata {
        let mut meta = ToolMetadata::default();
        if let Ok(parsed_args) = serde_json::from_str::<serde_json::Value>(arguments_json) {
            if let Some(approval_id) = parsed_args.get("approval_id").and_then(|v| v.as_str()) {
                meta.path = Some(approval_id.to_string());
            }
        }
        meta
    }
}

// ---------------------------------------------------------------------------
// Workflow Wait Tool
// ---------------------------------------------------------------------------

/// Checks the status of async tasks spawned with `agent.spawn(async: true)`.
/// Supports blocking mode: polls until all tasks complete or timeout expires.
pub struct WorkflowWaitTool;

fn check_task_statuses(
    config: &autonoetic_types::config::GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_ids: &[String],
    gateway_dir: Option<&Path>,
    session_id: Option<&str>,
) -> (
    Vec<serde_json::Value>,
    bool,
    bool,
    bool,
    usize,
    Vec<serde_json::Value>,
) {
    let mut tasks_status = Vec::new();
    let mut all_done = true;
    let mut any_failed = false;
    let mut any_not_found = false;
    let mut failed_task_count = 0;
    let mut failure_summary: Vec<serde_json::Value> = Vec::new();

    for task_id in task_ids {
        let task = crate::scheduler::load_task_run(config, store, workflow_id, task_id)
            .ok()
            .flatten();
        match task {
            Some(t) => {
                let is_terminal = matches!(
                    t.status,
                    autonoetic_types::workflow::TaskRunStatus::Succeeded
                        | autonoetic_types::workflow::TaskRunStatus::Failed
                        | autonoetic_types::workflow::TaskRunStatus::Cancelled
                        | autonoetic_types::workflow::TaskRunStatus::Aborted
                );
                if !is_terminal {
                    all_done = false;
                }
                if t.status == autonoetic_types::workflow::TaskRunStatus::Failed {
                    any_failed = true;
                    failed_task_count += 1;
                    let mut fentry = serde_json::json!({
                        "task_id": t.task_id,
                        "agent_id": t.agent_id,
                        "result_summary": t.result_summary,
                    });
                    if let Ok(Some(cp)) = crate::scheduler::load_task_checkpoint(
                        config,
                        store,
                        workflow_id,
                        &t.task_id,
                    ) {
                        fentry["checkpoint_step"] = serde_json::Value::String(cp.step);
                        if cp.state != serde_json::Value::Null {
                            fentry["checkpoint_state"] = cp.state;
                        }
                    }
                    if failure_summary.len() < 5 {
                        failure_summary.push(fentry);
                    }
                }
                let mut entry = serde_json::json!({
                    "task_id": t.task_id,
                    "agent_id": t.agent_id,
                    "session_id": t.session_id,
                    "status": format!("{:?}", t.status),
                    "result_summary": t.result_summary,
                });
                // Consume task checkpoint: include last step/state
                if let Ok(Some(cp)) =
                    crate::scheduler::load_task_checkpoint(config, store, workflow_id, &t.task_id)
                {
                    entry["checkpoint_step"] = serde_json::Value::String(cp.step);
                    entry["checkpoint_version"] = serde_json::json!(cp.version);
                    if cp.state != serde_json::Value::Null {
                        entry["checkpoint_state"] = cp.state;
                    }
                }
                // Check for implicit artifact created for this task
                if t.status == autonoetic_types::workflow::TaskRunStatus::Succeeded {
                    if let (Some(gw_dir), Some(sid)) = (gateway_dir, session_id) {
                        let implicit_name = format!("impl_{}", t.task_id);
                        if let Ok(content_store) =
                            crate::runtime::content_store::ContentStore::new(gw_dir)
                        {
                            if let Ok(content) = content_store.read_by_name(sid, &implicit_name) {
                                if let Ok(artifact_data) =
                                    serde_json::from_slice::<serde_json::Value>(&content)
                                {
                                    let output = serde_json::json!({
                                        "artifact_id": artifact_data.get("artifact_id").and_then(|v| v.as_str()),
                                        "summary": artifact_data.get("summary").and_then(|v| v.as_str()),
                                        "created_at": artifact_data.get("created_at").and_then(|v| v.as_str()),
                                    });
                                    entry["output"] = output;
                                }
                            }
                        }
                    }
                }
                tasks_status.push(entry);
            }
            None => {
                let queued = crate::scheduler::load_queued_tasks(config, store, workflow_id)
                    .unwrap_or_default();
                let is_queued = queued.iter().any(|q| q.task_id == *task_id);
                if is_queued {
                    all_done = false;
                    tasks_status.push(serde_json::json!({
                        "task_id": task_id,
                        "status": "queued",
                    }));
                } else {
                    all_done = false;
                    any_not_found = true;
                    tasks_status.push(serde_json::json!({
                        "task_id": task_id,
                        "status": "not_found",
                    }));
                }
            }
        }
    }
    (
        tasks_status,
        all_done,
        any_failed,
        any_not_found,
        failed_task_count,
        failure_summary,
    )
}

impl NativeTool for WorkflowWaitTool {
    fn name(&self) -> &'static str {
        "workflow.wait"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentSpawn { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Wait for async tasks to complete. Pass task_ids from agent.spawn(async=true). Returns structured status for each task. Succeeded tasks include an 'output' field with a stable implicit artifact_id (e.g., 'impl_task-abc123') — use content.read with that ID to consume the child's result. This is the canonical parent-child output handoff mechanism for ordinary agents. With timeout_secs=0 (default), returns current status immediately. With timeout_secs>0, polls until all tasks finish or timeout expires.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of task IDs to wait for (from agent.spawn responses with async=true)."
                    },
                    "workflow_id": {
                        "type": "string",
                        "description": "Optional workflow ID. If omitted, resolved from the current session's root."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 300,
                        "description": "Max seconds to wait. 0 = check once and return (default). >0 = poll until all tasks finish or timeout."
                    },
                    "poll_interval_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 30,
                        "description": "Seconds between status polls when blocking. Default: 2."
                    }
                },
                "required": ["task_ids"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            task_ids: Vec<String>,
            #[serde(default)]
            workflow_id: Option<String>,
            #[serde(default)]
            timeout_secs: Option<u64>,
            #[serde(default)]
            poll_interval_secs: Option<u64>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.task_ids.is_empty(), "task_ids must not be empty");

        let agents_dir = agent_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is missing its agents root parent"))?;

        let fallback_config = GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        };
        let gw_config = config.unwrap_or(&fallback_config);

        // Resolve workflow_id from session if not provided
        let workflow_id = match args.workflow_id {
            Some(id) => id,
            None => {
                let sid = session_id.unwrap_or(&manifest.agent.id);
                let root = crate::runtime::content_store::root_session_id(sid);
                crate::scheduler::resolve_workflow_id_for_root_session(gw_config, &root)?
                    .unwrap_or_else(|| "unknown".to_string())
            }
        };

        let timeout_secs = args.timeout_secs.unwrap_or(0).min(300);
        let poll_interval_secs = args.poll_interval_secs.unwrap_or(2).clamp(1, 30);

        // Non-blocking mode: check once and return
        if timeout_secs == 0 {
            let (
                tasks_status,
                all_done,
                any_failed,
                any_not_found,
                failed_task_count,
                failure_summary,
            ) = check_task_statuses(
                gw_config,
                gateway_store.as_deref(),
                &workflow_id,
                &args.task_ids,
                _gateway_dir,
                session_id,
            );
            return serde_json::to_string(&serde_json::json!({
                "ok": true,
                "workflow_id": workflow_id,
                "tasks": tasks_status,
                "join_satisfied": all_done,
                "any_failed": any_failed,
                "any_not_found": any_not_found,
                "failed_task_count": failed_task_count,
                "failure_summary": failure_summary,
                "waited_secs": 0,
                "message": if all_done {
                    if any_failed {
                        "All tasks completed (some failed). Review task results and proceed."
                    } else {
                        "All tasks completed successfully. You may proceed with the results."
                    }
                } else if any_not_found {
                    "One or more tasks were not found. Verify task_ids and workflow_id."
                } else {
                    "Some tasks are still running. Call workflow.wait with timeout_secs > 0 to block until they finish, or continue with other work."
                }
            }))
            .map_err(Into::into);
        }

        // Blocking mode: poll until join satisfied or timeout
        let task_ids = args.task_ids.clone();
        let wf_id = workflow_id.clone();
        let gw_config_arc = std::sync::Arc::new(gw_config.clone());

        let (
            tasks_status,
            all_done,
            any_failed,
            any_not_found,
            waited_secs,
            failed_task_count,
            failure_summary,
        ) = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                handle.block_on(async {
                    poll_until_join(
                        gw_config_arc.as_ref(),
                        gateway_store.as_deref(),
                        &wf_id,
                        &task_ids,
                        timeout_secs,
                        poll_interval_secs,
                        _gateway_dir,
                        session_id,
                    )
                    .await
                })
            })
        } else {
            tokio::runtime::Runtime::new()?.block_on(async {
                poll_until_join(
                    gw_config_arc.as_ref(),
                    gateway_store.as_deref(),
                    &wf_id,
                    &task_ids,
                    timeout_secs,
                    poll_interval_secs,
                    _gateway_dir,
                    session_id,
                )
                .await
            })
        };

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "workflow_id": workflow_id,
            "tasks": tasks_status,
            "join_satisfied": all_done,
            "any_failed": any_failed,
            "any_not_found": any_not_found,
            "failed_task_count": failed_task_count,
            "failure_summary": failure_summary,
            "waited_secs": waited_secs,
            "message": if all_done {
                if any_failed {
                    format!("All tasks completed after {}s (some failed). Review task results and proceed.", waited_secs)
                } else {
                    format!("All tasks completed successfully after {}s. You may proceed with the results.", waited_secs)
                }
            } else if any_not_found {
                "One or more tasks were not found. Verify task_ids and workflow_id.".to_string()
            } else {
                format!("Timed out after {}s. Some tasks are still running. Call workflow.wait again or proceed with partial results.", waited_secs)
            }
        }))
        .map_err(Into::into)
    }
}

async fn poll_until_join(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_ids: &[String],
    timeout_secs: u64,
    poll_interval_secs: u64,
    gateway_dir: Option<&Path>,
    session_id: Option<&str>,
) -> (
    Vec<serde_json::Value>,
    bool,
    bool,
    bool,
    u64,
    usize,
    Vec<serde_json::Value>,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut waited_secs = 0u64;

    loop {
        let (tasks_status, all_done, any_failed, any_not_found, failed_task_count, failure_summary) =
            check_task_statuses(
                config,
                store,
                workflow_id,
                task_ids,
                gateway_dir,
                session_id,
            );
        if all_done {
            return (
                tasks_status,
                true,
                any_failed,
                any_not_found,
                waited_secs,
                failed_task_count,
                failure_summary,
            );
        }
        if any_not_found {
            return (
                tasks_status,
                false,
                any_failed,
                true,
                waited_secs,
                failed_task_count,
                failure_summary,
            );
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            return (
                tasks_status,
                false,
                any_failed,
                any_not_found,
                waited_secs,
                failed_task_count,
                failure_summary,
            );
        }

        let remaining = (deadline - now).as_secs().min(poll_interval_secs).max(1);
        waited_secs += remaining;
        tokio::time::sleep(std::time::Duration::from_secs(remaining)).await;
    }
}

// ---------------------------------------------------------------------------
// Workflow State Tool
// ---------------------------------------------------------------------------

/// Exposes compact, structured workflow state to agents for deterministic resume.
/// Returns the current workflow step, completed tasks, pending approvals, and
/// valid next actions — replacing prose-based history inference.
pub struct WorkflowStateTool;

impl NativeTool for WorkflowStateTool {
    fn name(&self) -> &'static str {
        "workflow.state"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Returns compact, structured workflow state for deterministic resume. Use this instead of re-inferring state from conversation history. Returns: current step, completed tasks with artifact IDs, pending approvals, active tasks, and reuse guards.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "workflow_id": {
                        "type": "string",
                        "description": "Optional workflow ID. If omitted, resolved from the current session's root."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            workflow_id: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let agents_dir = agent_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is missing its agents root parent"))?;

        let fallback_config = GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        };
        let gw_config = config.unwrap_or(&fallback_config);

        let workflow_id = match args.workflow_id {
            Some(id) => id,
            None => {
                let sid = session_id.unwrap_or(&manifest.agent.id);
                let root = crate::runtime::content_store::root_session_id(sid);
                crate::scheduler::resolve_workflow_id_for_root_session(gw_config, &root)?
                    .unwrap_or_else(|| "unknown".to_string())
            }
        };

        let workflow = crate::scheduler::workflow_store::load_workflow_run(
            gw_config,
            gateway_store.as_deref(),
            &workflow_id,
        )?;

        let tasks = crate::scheduler::workflow_store::list_task_runs_for_workflow(
            gw_config,
            gateway_store.as_deref(),
            &workflow_id,
        )?;

        // Load pending approvals for this workflow to enrich pending_approvals entries
        let pending_approvals_map: HashMap<String, String> = {
            let root = workflow
                .as_ref()
                .map(|w| w.root_session_id.as_str())
                .unwrap_or("");
            if let Some(store) = gateway_store.as_deref() {
                store
                    .get_pending_approvals_for_root(root)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|a| a.task_id.map(|tid| (tid, a.request_id)))
                    .collect()
            } else {
                HashMap::new()
            }
        };

        let mut completed_tasks = Vec::new();
        let mut pending_approvals = Vec::new();
        let mut active_tasks = Vec::new();
        let mut latest_artifact_by_role: HashMap<String, serde_json::Value> = HashMap::new();
        let mut failed_task_count = 0usize;
        let mut failure_summary: Vec<serde_json::Value> = Vec::new();

        for task in &tasks {
            let implicit_artifact_id = format!("impl_{}", task.task_id);
            let entry = serde_json::json!({
                "task_id": task.task_id,
                "agent_id": task.agent_id,
                "status": format!("{:?}", task.status),
                "result_summary": task.result_summary,
                "implicit_artifact_id": implicit_artifact_id,
            });

            match task.status {
                autonoetic_types::workflow::TaskRunStatus::Succeeded => {
                    completed_tasks.push(entry.clone());
                    if let Some(ref summary) = task.result_summary {
                        let role = task.agent_id.split('.').next().unwrap_or("unknown");
                        latest_artifact_by_role.insert(
                            role.to_string(),
                            serde_json::json!({
                                "task_id": task.task_id,
                                "agent_id": task.agent_id,
                                "implicit_artifact_id": implicit_artifact_id,
                                "summary": summary,
                            }),
                        );
                    }
                }
                autonoetic_types::workflow::TaskRunStatus::AwaitingApproval => {
                    let mut entry = entry.clone();
                    if let Some(req_id) = pending_approvals_map.get(&task.task_id) {
                        entry.as_object_mut().unwrap().insert(
                            "approval_request_id".to_string(),
                            serde_json::Value::String(req_id.clone()),
                        );
                    }
                    pending_approvals.push(entry);
                }
                autonoetic_types::workflow::TaskRunStatus::Running
                | autonoetic_types::workflow::TaskRunStatus::Runnable
                | autonoetic_types::workflow::TaskRunStatus::Pending => {
                    active_tasks.push(entry);
                }
                autonoetic_types::workflow::TaskRunStatus::Failed
                | autonoetic_types::workflow::TaskRunStatus::Cancelled
                | autonoetic_types::workflow::TaskRunStatus::Aborted => {
                    failed_task_count += 1;
                    let mut fentry = entry.clone();
                    if let Ok(Some(cp)) = crate::scheduler::load_task_checkpoint(
                        gw_config,
                        gateway_store.as_deref(),
                        &workflow_id,
                        &task.task_id,
                    ) {
                        fentry.as_object_mut().unwrap().insert(
                            "checkpoint_step".to_string(),
                            serde_json::Value::String(cp.step),
                        );
                        if cp.state != serde_json::Value::Null {
                            fentry
                                .as_object_mut()
                                .unwrap()
                                .insert("checkpoint_state".to_string(), cp.state);
                        }
                    }
                    if failure_summary.len() < 5 {
                        failure_summary.push(fentry);
                    }
                }
                _ => {}
            }
        }

        let wf_status = workflow
            .as_ref()
            .map(|w| format!("{:?}", w.status))
            .unwrap_or_else(|| "unknown".to_string());

        let _latest_artifact_id = latest_artifact_by_role
            .get("coder")
            .and_then(|v| {
                v.get("task_id")
                    .and_then(|t| t.as_str())
                    .map(|t| format!("impl_task-{}", t.strip_prefix("task-").unwrap_or(t)))
            })
            .or_else(|| {
                latest_artifact_by_role.get("evaluator").and_then(|v| {
                    v.get("task_id")
                        .and_then(|t| t.as_str())
                        .map(|t| format!("impl_task-{}", t.strip_prefix("task-").unwrap_or(t)))
                })
            });

        let reuse_guards = serde_json::json!({
            "has_coder_artifact": latest_artifact_by_role.contains_key("coder"),
            "has_evaluator_result": latest_artifact_by_role.contains_key("evaluator"),
            "has_auditor_result": latest_artifact_by_role.contains_key("auditor"),
            "pending_approvals": !pending_approvals.is_empty(),
            "active_tasks_running": !active_tasks.is_empty(),
        });

        let state = serde_json::json!({
            "workflow_id": workflow_id,
            "workflow_status": wf_status,
            "completed_tasks": completed_tasks,
            "pending_approvals": pending_approvals,
            "active_tasks": active_tasks,
            "latest_artifact_by_role": latest_artifact_by_role,
            "reuse_guards": reuse_guards,
            "failed_task_count": failed_task_count,
            "failure_summary": failure_summary,
            "resume_hint": if !pending_approvals.is_empty() {
                "approval_pending — do not spawn new tasks, wait for approval"
            } else if !active_tasks.is_empty() {
                "tasks_running — wait for completion or proceed with partial results"
            } else if latest_artifact_by_role.contains_key("evaluator") && latest_artifact_by_role.contains_key("auditor") {
                "evaluation_complete — proceed to specialized_builder or coder iteration"
            } else if latest_artifact_by_role.contains_key("coder") && !latest_artifact_by_role.contains_key("evaluator") {
                "coder_done — proceed to evaluator/auditor"
            } else if !completed_tasks.is_empty() {
                "some_tasks_done — check completed_tasks for next step"
            } else {
                "fresh_start — no prior work found"
            },
        });

        serde_json::to_string(&state).map_err(Into::into)
    }
}

#[derive(Debug, Deserialize)]
struct AgentExistsArgs {
    agent_id: String,
}

pub struct AgentExistsTool;

impl NativeTool for AgentExistsTool {
    fn name(&self) -> &'static str {
        "agent.exists"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentSpawn { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Check if an agent with the given ID already exists in the repository (authoring tree or resolved revision paths).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "The agent ID to check for existence" }
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: AgentExistsArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        validate_agent_id(&args.agent_id)?;

        let agents_dir = agent_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is missing its agents root parent"))?;

        let repo = crate::agent::AgentRepository::new(agents_dir.to_path_buf());

        match repo.get_sync(&args.agent_id) {
            Ok(_) => Ok(serde_json::json!({
                "ok": true,
                "exists": true,
                "agent_id": args.agent_id,
                "status": "healthy",
            })
            .to_string()),
            Err(e) => {
                let error_msg = e.to_string();
                if error_msg.contains("not found") {
                    Ok(serde_json::json!({
                        "ok": true,
                        "exists": false,
                        "agent_id": args.agent_id,
                        "status": "not_found",
                    })
                    .to_string())
                } else if error_msg.contains("identity mismatch") {
                    Ok(serde_json::json!({
                        "ok": true,
                        "exists": true,
                        "agent_id": args.agent_id,
                        "status": "identity_mismatch",
                        "error": error_msg,
                        "message": "Agent directory exists but manifest ID does not match directory name. This agent needs to be fixed before use."
                    })
                    .to_string())
                } else {
                    Ok(serde_json::json!({
                        "ok": true,
                        "exists": true,
                        "agent_id": args.agent_id,
                        "status": "load_error",
                        "error": error_msg,
                        "message": "Agent exists but cannot be loaded. Check SKILL.md syntax or file permissions."
                    })
                    .to_string())
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct AgentDiscoverArgs {
    intent: String,
    #[serde(default)]
    required_capabilities: Vec<String>,
    #[serde(default)]
    exclude_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AgentDiscoveryResult {
    score: f64,
    agent_id: String,
    name: String,
    description: String,
    capabilities: Vec<String>,
    match_reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    io: Option<serde_json::Value>,
}

pub struct AgentDiscoverTool;

impl NativeTool for AgentDiscoverTool {
    fn name(&self) -> &'static str {
        "agent.discover"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentSpawn { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Discover existing agents that match the given intent and capabilities. Returns ranked candidates with match scores and reasons. Use this before deciding to install a new agent to prefer reuse over creation.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "intent": { "type": "string", "description": "The task intent or goal to match against agent descriptions" },
                    "required_capabilities": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of required capability types (e.g., 'CodeExecution', 'WriteAccess', 'NetworkAccess')"
                    },
                    "exclude_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Agent IDs to exclude from results"
                    }
                },
                "required": ["intent"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: AgentDiscoverArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.intent.trim().is_empty(), "intent must not be empty");

        let agents_dir = agent_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is missing its agents root parent"))?;

        let repo = crate::agent::AgentRepository::new(agents_dir.to_path_buf());
        let loaded_agents = repo.list_loaded_sync()?;

        let mut results: Vec<AgentDiscoveryResult> = loaded_agents
            .into_iter()
            .filter(|agent| !args.exclude_ids.contains(&agent.id().to_string()))
            .map(|agent| {
                let mut score = 0.0;
                let mut match_reasons = Vec::new();

                let description_lower = agent.instructions.to_lowercase();
                let intent_lower = args.intent.to_lowercase();

                if description_lower.contains(&intent_lower) {
                    score += 30.0;
                    match_reasons.push("exact intent match in description".to_string());
                } else {
                    let keywords: Vec<String> = intent_lower
                        .split_whitespace()
                        .filter(|w| w.len() > 3)
                        .map(|s| s.to_string())
                        .collect();
                    let matched_keywords: Vec<String> = keywords
                        .iter()
                        .filter(|k| description_lower.contains(*k))
                        .cloned()
                        .collect();
                    if !matched_keywords.is_empty() {
                        let keyword_score =
                            (matched_keywords.len() as f64 / keywords.len() as f64) * 20.0;
                        score += keyword_score;
                        match_reasons.push(format!("keyword match: {:?}", matched_keywords));
                    }
                }

                let agent_cap_types: Vec<String> = agent
                    .manifest
                    .capabilities
                    .iter()
                    .map(|c| capability_type_name(c))
                    .collect();

                for req_cap in &args.required_capabilities {
                    if agent_cap_types.iter().any(|cap| cap == req_cap) {
                        score += 15.0;
                        match_reasons.push(format!("has required capability: {}", req_cap));
                    }
                }

                if agent
                    .manifest
                    .background
                    .as_ref()
                    .map(|b| b.enabled)
                    .unwrap_or(false)
                {
                    score += 5.0;
                    match_reasons.push("supports background execution".to_string());
                }

                let io_schema = agent.manifest.io.as_ref().map(|io| {
                    serde_json::json!({
                        "accepts": io.accepts,
                        "returns": io.returns,
                    })
                });

                AgentDiscoveryResult {
                    score,
                    agent_id: agent.id().to_string(),
                    name: agent.manifest.agent.name,
                    description: agent.manifest.agent.description,
                    capabilities: agent_cap_types,
                    match_reasons,
                    io: io_schema,
                }
            })
            .filter(|r| r.score > 0.0)
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(serde_json::json!({
            "ok": true,
            "query": {
                "intent": args.intent,
                "required_capabilities": args.required_capabilities,
            },
            "results": results,
            "result_count": results.len(),
        })
        .to_string())
    }
}

/// Builds the default registry with the core native tools.

#[derive(Debug, Deserialize)]
pub(crate) struct SandboxExecArgs {
    command: String,
    #[serde(default)]
    dependencies: Option<SandboxExecDependencies>,
    #[serde(default)]
    approval_ref: Option<String>,
    /// When provided, only mount artifact files instead of all session content.
    #[serde(default)]
    artifact_id: Option<String>,
    /// Paths inside the sandbox to capture as layers after execution.
    #[serde(default)]
    capture_paths: Option<Vec<CapturePath>>,
}

#[derive(Debug, Deserialize)]
pub struct SandboxExecDependencies {
    pub runtime: String,
    pub packages: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CapturePath {
    pub path: String,
    pub mount_as: String,
}

fn dependency_plan_from_args_or_lock(
    manifest: &AgentManifest,
    agent_dir: &Path,
    deps: Option<SandboxExecDependencies>,
) -> anyhow::Result<Option<DependencyPlan>> {
    if let Some(deps) = deps {
        return parse_dependency_plan(deps.runtime.as_str(), deps.packages).map(Some);
    }

    let lock_path = agent_dir.join(&manifest.runtime.runtime_lock);
    if !lock_path.exists() {
        return Ok(None);
    }
    let lock = crate::runtime_lock::resolve_runtime_lock(&lock_path)?;
    if lock.dependencies.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        lock.dependencies.len() == 1,
        "runtime.lock currently supports exactly one dependency set"
    );
    let locked = &lock.dependencies[0];
    parse_dependency_plan(locked.runtime.as_str(), locked.packages.clone()).map(Some)
}
fn parse_dependency_plan(runtime: &str, packages: Vec<String>) -> anyhow::Result<DependencyPlan> {
    let runtime = match runtime.to_ascii_lowercase().as_str() {
        "python" => DependencyRuntime::Python,
        "nodejs" | "node" => DependencyRuntime::NodeJs,
        other => anyhow::bail!("Unsupported dependency runtime '{}'", other),
    };
    // Empty packages is OK - means use runtime from sandbox without extra packages
    Ok(DependencyPlan { runtime, packages })
}

// ---------------------------------------------------------------------------
// workflow.cancel_task
// ---------------------------------------------------------------------------

pub struct WorkflowCancelTaskTool;

impl NativeTool for WorkflowCancelTaskTool {
    fn name(&self) -> &'static str {
        "workflow.cancel_task"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentSpawn { .. }))
    }

    fn definition(&self) -> crate::llm::ToolDefinition {
        crate::llm::ToolDefinition {
            name: self.name().to_string(),
            description: "Cancel a task that is AwaitingApproval or Pending. Running tasks cannot be cancelled. Deletes any saved continuation and marks the task as Cancelled, which triggers the join condition check so the planner is notified.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["workflow_id", "task_id"],
                "properties": {
                    "workflow_id": {
                        "type": "string",
                        "description": "The workflow ID containing the task."
                    },
                    "task_id": {
                        "type": "string",
                        "description": "The task ID to cancel."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why the task is being cancelled."
                    }
                }
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let config = config
            .ok_or_else(|| anyhow::anyhow!("Gateway config required for workflow.cancel_task"))?;
        let args: serde_json::Value = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid arguments: {}", e))?;

        let workflow_id = args["workflow_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("workflow_id is required"))?;
        let task_id = args["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("task_id is required"))?;
        let reason = args["reason"].as_str().map(str::to_string);

        let store = gateway_store.as_deref();
        let task = crate::scheduler::load_task_run(config, store, workflow_id, task_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("Task '{}' not found in workflow '{}'", task_id, workflow_id)
            })?;

        let cancellable = matches!(
            task.status,
            autonoetic_types::workflow::TaskRunStatus::AwaitingApproval
                | autonoetic_types::workflow::TaskRunStatus::Pending
                | autonoetic_types::workflow::TaskRunStatus::Runnable
        );
        if !cancellable {
            return Ok(serde_json::json!({
                "ok": false,
                "task_id": task_id,
                "status": format!("{:?}", task.status),
                "error": format!("Task is {:?} and cannot be cancelled. Only AwaitingApproval, Pending, and Runnable tasks can be cancelled.", task.status)
            })
            .to_string());
        }

        // Delete any saved continuation file.
        let _ = crate::runtime::continuation::delete_continuation(config, task_id);

        // Mark as Cancelled (triggers join condition check).
        crate::scheduler::workflow_store::update_task_run_status(
            config,
            store,
            workflow_id,
            task_id,
            autonoetic_types::workflow::TaskRunStatus::Cancelled,
            reason
                .clone()
                .or_else(|| Some("Cancelled by operator".to_string())),
        )?;

        // Remove from queue if present.
        let _ = crate::scheduler::workflow_store::dequeue_task(config, store, workflow_id, task_id);

        Ok(serde_json::json!({
            "ok": true,
            "task_id": task_id,
            "workflow_id": workflow_id,
            "status": "Cancelled",
            "reason": reason.unwrap_or_else(|| "Cancelled by operator".to_string())
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// User Interaction Tools
// ---------------------------------------------------------------------------

/// Ask the user a question. The agent's turn is suspended until the user answers.
///
/// Supports clarification, decision, proposal, and confirmation types.
/// Options can be provided for structured choices, or freeform answers allowed.
pub struct UserAskTool;

impl NativeTool for UserAskTool {
    fn name(&self) -> &'static str {
        "user.ask"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true // Available to all agents
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Ask the user a question. Execution suspends until the user answers. Use this for clarifications, decisions, proposals, and confirmations.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["clarification", "decision", "proposal", "confirmation"],
                        "default": "clarification",
                        "description": "Type of question being asked"
                    },
                    "question": {
                        "type": "string",
                        "description": "The question to ask the user"
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional context explaining why this question matters"
                    },
                    "options": {
                        "type": "array",
                        "description": "Optional structured choices for the user",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "label": { "type": "string" },
                                "value": { "type": "string" }
                            },
                            "required": ["id", "label", "value"]
                        }
                    },
                    "allow_freeform": {
                        "type": "boolean",
                        "default": true,
                        "description": "Whether free text answers are allowed"
                    }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        use autonoetic_types::background::{
            UserInteraction, UserInteractionKind, UserInteractionOption, UserInteractionStatus,
        };

        #[derive(Deserialize)]
        struct Args {
            #[serde(default = "default_kind")]
            kind: String,
            question: String,
            #[serde(default)]
            context: Option<String>,
            #[serde(default)]
            options: Vec<serde_json::Value>,
            #[serde(default = "default_true")]
            allow_freeform: bool,
        }

        fn default_kind() -> String {
            "clarification".to_string()
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let sid = session_id.unwrap_or("unknown");
        let root_session_id = crate::runtime::content_store::root_session_id(sid).to_string();

        // Runtime guard: user.ask is not allowed while orchestrating workflow tasks.
        // This prevents the planner from using user.ask as a substitute for workflow.wait,
        // which would strand the session (user.ask creates a UserInputRequired checkpoint
        // that blocks workflow join signals from resuming the session).
        if let (Some(cfg), Some(store)) = (_config, &gateway_store) {
            // Check for active child tasks in this root session's workflow
            let workflow_id =
                crate::scheduler::resolve_workflow_id_for_root_session(cfg, &root_session_id)
                    .ok()
                    .flatten();
            if let Some(wf_id) = &workflow_id {
                let task_runs = crate::scheduler::workflow_store::list_task_runs_for_workflow(
                    cfg,
                    Some(store.as_ref()),
                    wf_id,
                )
                .unwrap_or_default();
                let has_active_children = task_runs.iter().any(|t| {
                    matches!(
                        t.status,
                        autonoetic_types::workflow::TaskRunStatus::Pending
                            | autonoetic_types::workflow::TaskRunStatus::Runnable
                            | autonoetic_types::workflow::TaskRunStatus::Running
                            | autonoetic_types::workflow::TaskRunStatus::AwaitingApproval
                            | autonoetic_types::workflow::TaskRunStatus::Paused
                    )
                });
                if has_active_children {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error": "user.ask is not available while workflow tasks are active. Use workflow.wait to handle pending child tasks, or respond in prose for clarifications."
                    }).to_string());
                }
            }

            // Check for pending approvals for this root session
            let pending_approvals = store
                .get_pending_approvals_for_root(&root_session_id)
                .unwrap_or_default();
            if !pending_approvals.is_empty() {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error": "user.ask is not available while approvals are pending. Use workflow.wait to handle pending approvals."
                }).to_string());
            }
        }

        let interaction_id = format!("ui-{}", &uuid::Uuid::new_v4().to_string()[..8]);

        let kind = match args.kind.as_str() {
            "decision" => UserInteractionKind::Decision,
            "proposal" => UserInteractionKind::Proposal,
            "confirmation" => UserInteractionKind::Confirmation,
            _ => UserInteractionKind::Clarification,
        };

        let options: Vec<UserInteractionOption> = args
            .options
            .into_iter()
            .filter_map(|v| {
                Some(UserInteractionOption {
                    id: v.get("id")?.as_str()?.to_string(),
                    label: v.get("label")?.as_str()?.to_string(),
                    value: v.get("value")?.as_str()?.to_string(),
                })
            })
            .collect();

        let now = chrono::Utc::now().to_rfc3339();

        let interaction = UserInteraction {
            interaction_id: interaction_id.clone(),
            session_id: sid.to_string(),
            root_session_id,
            agent_id: _manifest.agent.id.clone(),
            turn_id: turn_id.unwrap_or("unknown").to_string(),
            kind,
            question: args.question,
            context: args.context,
            options,
            allow_freeform: args.allow_freeform,
            status: UserInteractionStatus::Pending,
            answer_option_id: None,
            answer_text: None,
            answered_by: None,
            created_at: now,
            answered_at: None,
            expires_at: None,
            workflow_id: None,
            task_id: None,
            checkpoint_turn_id: None,
        };

        // Persist interaction to gateway store
        if let Some(store) = gateway_store {
            store.create_user_interaction(&interaction)?;
            tracing::info!(
                target: "user_interaction",
                interaction_id = %interaction_id,
                session_id = %sid,
                "User interaction created; agent will suspend"
            );
            if let Some(ctx) = _run_context {
                if let Some(w) = &ctx.live_digest {
                    let opts_summary = if interaction.options.is_empty() {
                        None
                    } else {
                        Some(
                            interaction
                                .options
                                .iter()
                                .map(|o| format!("{}: {}", o.id, o.label))
                                .collect::<Vec<_>>()
                                .join("; "),
                        )
                    };
                    if let Ok(mut g) = w.lock() {
                        let _ = g.record_user_ask_pending(
                            &interaction.question,
                            opts_summary.as_deref(),
                        );
                    }
                }
            }
            if let Some(ctx) = _run_context {
                let _ = store.create_live_digest_event(
                    &crate::scheduler::gateway_store::LiveDigestEventRecord {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        root_session_id: ctx.root_session_id.clone(),
                        source_session_id: ctx.session_id.clone(),
                        turn_id: turn_id.map(|s| s.to_string()),
                        source_agent_id: Some(_manifest.agent.id.clone()),
                        source_node_id: std::env::var("AUTONOETIC_NODE_ID")
                            .unwrap_or_else(|_| "gateway".to_string()),
                        event_type: "user.ask.pending".to_string(),
                        payload: Some(
                            serde_json::json!({
                                "interaction_id": interaction_id.clone(),
                                "question": crate::log_redaction::redact_text_for_logs(&interaction.question),
                                "options_count": interaction.options.len(),
                            })
                            .to_string(),
                        ),
                        created_at: chrono::Utc::now().to_rfc3339(),
                    },
                );
            }
        } else {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "Gateway store not available; user.ask requires persistent store"
            })
            .to_string());
        }

        // Return a marker that the lifecycle will detect to trigger suspension
        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "interaction_required": true,
            "interaction_id": interaction_id,
            "status": "awaiting_user"
        }))
        .map_err(Into::into)
    }
}

/// Query the status of a user interaction.
/// Allows agents to check whether an interaction is pending, answered, cancelled, or expired.
pub struct UserInteractionStatusTool;

impl NativeTool for UserInteractionStatusTool {
    fn name(&self) -> &'static str {
        "user.interaction.status"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true // Available to all agents
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Query the status of a user interaction. Returns the current status (pending, answered, cancelled, expired) and the answer if available.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "interaction_id": {
                        "type": "string",
                        "description": "The interaction ID to check (e.g., 'ui-abc123')"
                    }
                },
                "required": ["interaction_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            interaction_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": true,
                "interaction_id": args.interaction_id,
                "status": "unknown",
                "message": "Gateway store not available"
            }))?);
        };

        match store.get_user_interaction(&args.interaction_id) {
            Ok(Some(interaction)) => {
                let status = match &interaction.status {
                    UserInteractionStatus::Pending => "pending",
                    UserInteractionStatus::Answered => "answered",
                    UserInteractionStatus::Cancelled => "cancelled",
                    UserInteractionStatus::Expired => "expired",
                };

                let mut response = serde_json::json!({
                    "ok": true,
                    "interaction_id": args.interaction_id,
                    "status": status,
                    "kind": interaction.kind.as_str(),
                    "question": interaction.question,
                    "agent_id": interaction.agent_id,
                    "session_id": interaction.session_id,
                    "created_at": interaction.created_at,
                });

                if let Some(answered_at) = &interaction.answered_at {
                    response["answered_at"] = serde_json::Value::String(answered_at.clone());
                }
                if let Some(answer_text) = &interaction.answer_text {
                    response["answer_text"] = serde_json::Value::String(answer_text.clone());
                }
                if let Some(answer_option_id) = &interaction.answer_option_id {
                    response["answer_option_id"] =
                        serde_json::Value::String(answer_option_id.clone());
                }

                serde_json::to_string(&response).map_err(Into::into)
            }
            Ok(None) => serde_json::to_string(&serde_json::json!({
                "ok": true,
                "interaction_id": args.interaction_id,
                "status": "not_found",
                "message": "User interaction not found"
            }))
            .map_err(Into::into),
            Err(e) => serde_json::to_string(&serde_json::json!({
                "ok": false,
                "interaction_id": args.interaction_id,
                "error": e.to_string()
            }))
            .map_err(Into::into),
        }
    }
}

pub fn default_registry() -> NativeToolRegistry {
    let mut registry = NativeToolRegistry::new();
    crate::runtime::tools::execution::register_tools(&mut registry);
    crate::runtime::tools::digest::register_tools(&mut registry);
    crate::runtime::tools::session::register_tools(&mut registry);
    crate::runtime::tools::content::register_tools(&mut registry);
    crate::runtime::tools::agent_revision::register_tools(&mut registry);
    crate::runtime::tools::evaluation::register_tools(&mut registry);
    registry.register(Box::new(KnowledgeStoreTool));
    registry.register(Box::new(KnowledgeRecallTool));
    registry.register(Box::new(KnowledgeSearchTool));
    registry.register(Box::new(KnowledgeSearchByTagsTool));
    registry.register(Box::new(DigestQueryTool));
    registry.register(Box::new(KnowledgeShareTool));
    registry.register(Box::new(SandboxExecTool));
    registry.register(Box::new(WebSearchTool));
    registry.register(Box::new(WebFetchTool));
    registry.register(Box::new(ArtifactBuildTool));
    registry.register(Box::new(ArtifactInspectTool));
    registry.register(Box::new(ArtifactResolveRefTool));
    registry.register(Box::new(AgentSpawnTool));
    registry.register(Box::new(AgentExistsTool));
    registry.register(Box::new(AgentDiscoverTool));
    registry.register(Box::new(ApprovalStatusTool));
    registry.register(Box::new(WorkflowWaitTool));
    registry.register(Box::new(WorkflowStateTool));
    registry.register(Box::new(WorkflowCancelTaskTool));
    registry.register(Box::new(UserAskTool));
    registry.register(Box::new(UserInteractionStatusTool));
    registry.register(Box::new(
        crate::runtime::tools_promotion::PromotionRecordTool,
    ));
    registry.register(Box::new(
        crate::runtime::tools_promotion::PromotionQueryTool,
    ));
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};
    use autonoetic_types::capability::Capability;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::thread;
    use tempfile::tempdir;

    fn test_manifest(capabilities: Vec<Capability>) -> AgentManifest {
        test_manifest_with_id("test-agent", capabilities)
    }

    fn test_manifest_with_id(agent_id: &str, capabilities: Vec<Capability>) -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: agent_id.to_string(),
                name: agent_id.to_string(),
                description: "test".to_string(),
            },
            capabilities,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            gateway_url: None,
            gateway_token: None,

            response_contract: None,
        }
    }

    fn spawn_one_shot_http_server(
        status: &str,
        content_type: &str,
        body: String,
    ) -> (String, thread::JoinHandle<()>) {
        let status = status.to_string();
        let content_type = content_type.to_string();
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let addr = listener
            .local_addr()
            .expect("listener should expose local addr");
        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request_buf = [0_u8; 2048];
                let _ = stream.read(&mut request_buf);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://{}", addr), handle)
    }

    fn spawn_counting_http_server(
        status: &str,
        content_type: &str,
        body: String,
        expected_requests: usize,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let status = status.to_string();
        let content_type = content_type.to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let addr = listener
            .local_addr()
            .expect("listener should expose local addr");
        let handle = thread::spawn(move || {
            for _ in 0..expected_requests {
                if let Ok((mut stream, _)) = listener.accept() {
                    hits_clone.fetch_add(1, Ordering::SeqCst);
                    let mut request_buf = [0_u8; 2048];
                    let _ = stream.read(&mut request_buf);
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            }
        });
        (format!("http://{}", addr), hits, handle)
    }

    #[test]
    fn test_native_tool_registry_availability() {
        let registry = default_registry();
        let manifest_none = test_manifest(vec![]);
        // SessionEscalateTool, ApprovalStatusTool, ExecutionSearchTool, UserAskTool, UserInteractionStatusTool, DigestAnnotateTool are always available
        assert_eq!(registry.available_definitions(&manifest_none).len(), 6);
        let manifest_shell = test_manifest(vec![Capability::CodeExecution {
            patterns: vec!["*".into()],
        }]);
        let defs = registry.available_definitions(&manifest_shell);
        // sandbox.exec (1) + always-available (6) = 7
        assert_eq!(defs.len(), 7);
        assert!(defs.iter().any(|d| d.name == "sandbox.exec"));

        let manifest_all = test_manifest(vec![
            Capability::CodeExecution { patterns: vec![] },
            Capability::ReadAccess { scopes: vec![] },
            Capability::WriteAccess { scopes: vec![] },
        ]);
        let defs_all = registry.available_definitions(&manifest_all);
        // sandbox.exec (1) + content.write, content.read (2) +
        // artifact.build, artifact.inspect, artifact.resolve_ref (3) +
        // execution.search (1) +
        // knowledge.store, knowledge.recall, knowledge.search, knowledge.search_by_tags, digest.query (5) +
        // knowledge.share (1) +
        // promotion.query (1) +
        // workflow.state (1, now gated by ReadAccess) +
        // always-available (6) = 20
        assert_eq!(defs_all.len(), 20);

        let manifest_spawn = test_manifest(vec![Capability::AgentSpawn { max_children: 4 }]);
        let defs_spawn = registry.available_definitions(&manifest_spawn);
        // Keep this assertion non-brittle as always-available tool set can evolve.
        assert!(defs_spawn.len() >= 7);
        assert!(defs_spawn.iter().any(|d| d.name == "agent.spawn"));
        assert!(defs_spawn.iter().any(|d| d.name == "agent.exists"));
        assert!(defs_spawn.iter().any(|d| d.name == "agent.discover"));
        assert!(defs_spawn.iter().any(|d| d.name == "workflow.wait"));


        let manifest_revision = test_manifest(vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }]);
        let defs_revision = registry.available_definitions(&manifest_revision);
        assert!(defs_revision
            .iter()
            .any(|d| d.name == "agent.revision.create"));
        assert!(defs_revision
            .iter()
            .any(|d| d.name == "agent.revision.list"));
        assert!(defs_revision
            .iter()
            .any(|d| d.name == "agent.revision.inspect"));
        assert!(defs_revision
            .iter()
            .any(|d| d.name == "agent.revision.promote"));
        assert!(defs_revision
            .iter()
            .any(|d| d.name == "agent.revision.rollback"));
        assert!(defs_revision
            .iter()
            .any(|d| d.name == "agent.revision.diff"));

        let manifest_net = test_manifest(vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }]);
        let defs_net = registry.available_definitions(&manifest_net);
        // web.search, web.fetch (2) + always-available (6) = 8
        assert_eq!(defs_net.len(), 8);
        assert!(defs_net.iter().any(|d| d.name == "web.search"));
        assert!(defs_net.iter().any(|d| d.name == "web.fetch"));
    }

    #[test]
    fn test_workflow_wait_missing_task_returns_immediately_in_blocking_mode() {
        let manifest = test_manifest(vec![Capability::AgentSpawn { max_children: 4 }]);
        let policy = PolicyEngine::new(manifest.clone());
        let registry = default_registry();
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        let caller_dir = agents_dir.join("planner.default");
        std::fs::create_dir_all(&caller_dir).expect("caller dir should create");

        let args = serde_json::json!({
            "workflow_id": "wf-missing",
            "task_ids": ["task-missing"],
            "timeout_secs": 30,
            "poll_interval_secs": 30
        });

        let started = std::time::Instant::now();
        let result = registry
            .execute(
                "workflow.wait",
                &manifest,
                &policy,
                &caller_dir,
                None,
                &args.to_string(),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("workflow.wait should succeed");

        let elapsed = started.elapsed();
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("workflow.wait result should decode");
        assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
        assert_eq!(
            parsed.get("join_satisfied"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(parsed.get("any_not_found"), Some(&serde_json::json!(true)));
        assert_eq!(parsed.get("waited_secs"), Some(&serde_json::json!(0)));
        assert_eq!(
            parsed.get("message").and_then(|v| v.as_str()),
            Some("One or more tasks were not found. Verify task_ids and workflow_id.")
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "blocking workflow.wait should fail fast for missing tasks"
        );
    }

    #[test]
    fn test_web_fetch_tool_roundtrip_local_server() {
        let manifest = test_manifest(vec![Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");
        let (base_url, handle) = spawn_one_shot_http_server(
            "200 OK",
            "text/plain; charset=utf-8",
            "hello web fetch".to_string(),
        );

        let args = serde_json::json!({
            "url": format!("{}/doc", base_url),
            "timeout_secs": 10,
            "max_chars": 4096
        });

        let registry = default_registry();
        let result = registry
            .execute(
                "web.fetch",
                &manifest,
                &policy,
                temp.path(),
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("web.fetch should succeed");

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("web.fetch result should decode");
        assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
        assert!(parsed
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .contains("hello web fetch"));

        handle.join().expect("server thread should join");
    }

    #[test]
    fn test_web_fetch_tool_denied_by_netconnect_policy() {
        let manifest = test_manifest(vec![Capability::NetworkAccess {
            hosts: vec!["example.com".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");

        let args = serde_json::json!({
            "url": "http://127.0.0.1:65535/forbidden"
        });

        let registry = default_registry();
        let err = registry
            .execute(
                "web.fetch",
                &manifest,
                &policy,
                temp.path(),
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect_err("web.fetch should be denied");
        assert!(err.to_string().contains("NetworkAccess"));
    }

    #[test]
    fn test_web_search_tool_denied_by_netconnect_policy() {
        let manifest = test_manifest(vec![Capability::NetworkAccess {
            hosts: vec!["example.com".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");

        let args = serde_json::json!({
            "query": "rust",
            "engine_url": "http://127.0.0.1:65535/search"
        });

        let registry = default_registry();
        let err = registry
            .execute(
                "web.search",
                &manifest,
                &policy,
                temp.path(),
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect_err("web.search should be denied");
        assert!(err.to_string().contains("NetworkAccess"));
    }

    #[test]
    fn test_web_search_tool_roundtrip_local_engine() {
        let manifest = test_manifest(vec![Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");
        let body = serde_json::json!({
            "Results": [],
            "RelatedTopics": [
                {
                    "Text": "Rust language homepage",
                    "FirstURL": "https://www.rust-lang.org/"
                },
                {
                    "Name": "Docs",
                    "Topics": [
                        {
                            "Text": "The Rust book",
                            "FirstURL": "https://doc.rust-lang.org/book/"
                        }
                    ]
                }
            ]
        })
        .to_string();
        let (engine_url, handle) = spawn_one_shot_http_server("200 OK", "application/json", body);

        let args = serde_json::json!({
            "query": "rust language",
            "provider": "duckduckgo",
            "engine_url": engine_url,
            "max_results": 5
        });

        let registry = default_registry();
        let result = registry
            .execute(
                "web.search",
                &manifest,
                &policy,
                temp.path(),
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("web.search should succeed");

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("web.search result should decode");
        assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
        assert!(
            parsed
                .get("result_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                >= 2
        );

        handle.join().expect("server thread should join");
    }

    #[test]
    fn test_web_search_google_requires_api_key_env() {
        let manifest = test_manifest(vec![Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");

        let args = serde_json::json!({
            "query": "rust",
            "provider": "google",
            "engine_url": "http://127.0.0.1:65535/search",
            "google_engine_id": "cx-test",
            "google_api_key_env": "AUTONOETIC_TEST_GOOGLE_KEY_MISSING"
        });

        let registry = default_registry();
        let err = registry
            .execute(
                "web.search",
                &manifest,
                &policy,
                temp.path(),
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect_err("google search without key should fail");
        assert!(err.to_string().contains("requires API key env"));
    }

    #[test]
    fn test_web_search_google_roundtrip_local_engine() {
        let manifest = test_manifest(vec![Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");
        let body = serde_json::json!({
            "searchInformation": {
                "totalResults": "123"
            },
            "items": [
                {
                    "title": "Rust language",
                    "link": "https://www.rust-lang.org/",
                    "snippet": "Rust empowers everyone."
                },
                {
                    "title": "The Rust Book",
                    "link": "https://doc.rust-lang.org/book/",
                    "snippet": "Learn Rust."
                }
            ]
        })
        .to_string();
        let (engine_url, handle) = spawn_one_shot_http_server("200 OK", "application/json", body);

        let key_env = "AUTONOETIC_TEST_GOOGLE_KEY_OK";
        let cx_env = "AUTONOETIC_TEST_GOOGLE_CX_OK";
        let prior_key = std::env::var(key_env).ok();
        let prior_cx = std::env::var(cx_env).ok();
        std::env::set_var(key_env, "test-api-key");
        std::env::set_var(cx_env, "test-cx-id");

        let args = serde_json::json!({
            "query": "rust language",
            "provider": "google",
            "engine_url": engine_url,
            "google_api_key_env": key_env,
            "google_engine_id_env": cx_env
        });

        let registry = default_registry();
        let result = registry
            .execute(
                "web.search",
                &manifest,
                &policy,
                temp.path(),
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("google web.search should succeed");

        match prior_key {
            Some(value) => std::env::set_var(key_env, value),
            None => std::env::remove_var(key_env),
        }
        match prior_cx {
            Some(value) => std::env::set_var(cx_env, value),
            None => std::env::remove_var(cx_env),
        }
        handle.join().expect("server thread should join");

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("web.search result should decode");
        assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
        assert_eq!(parsed.get("provider"), Some(&serde_json::json!("google")));
        assert_eq!(parsed.get("total_results"), Some(&serde_json::json!(123)));
        assert_eq!(parsed.get("result_count"), Some(&serde_json::json!(2)));
    }

    #[test]
    fn test_web_search_google_legacy_cx_env_alias_roundtrip() {
        let manifest = test_manifest(vec![Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");

        let body = serde_json::json!({
            "searchInformation": {
                "totalResults": "7"
            },
            "items": [
                {
                    "title": "Example result",
                    "link": "https://example.com/",
                    "snippet": "example"
                }
            ]
        })
        .to_string();
        let (engine_url, handle) = spawn_one_shot_http_server("200 OK", "application/json", body);

        let key_env = "GOOGLE_SEARCH_API_KEY";
        let cx_env = "GOOGLE_SEARCH_CX";
        let prior_key = std::env::var(key_env).ok();
        let prior_cx = std::env::var(cx_env).ok();
        std::env::set_var(key_env, "legacy-test-api-key");
        std::env::set_var(cx_env, "legacy-test-cx");

        let args = serde_json::json!({
            "query": "legacy cx alias",
            "provider": "google",
            "engine_url": engine_url
        });

        let registry = default_registry();
        let result = registry
            .execute(
                "web.search",
                &manifest,
                &policy,
                temp.path(),
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("google web.search should accept GOOGLE_SEARCH_CX legacy alias");

        match prior_key {
            Some(value) => std::env::set_var(key_env, value),
            None => std::env::remove_var(key_env),
        }
        match prior_cx {
            Some(value) => std::env::set_var(cx_env, value),
            None => std::env::remove_var(cx_env),
        }
        handle.join().expect("server thread should join");

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("web.search result should decode");
        assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
        assert_eq!(parsed.get("provider"), Some(&serde_json::json!("google")));
        assert_eq!(parsed.get("result_count"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn test_web_search_auto_falls_back_to_duckduckgo_when_google_fails() {
        let manifest = test_manifest(vec![Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");

        let google_body = serde_json::json!({
            "error": { "message": "quota exceeded" }
        })
        .to_string();
        let (google_engine_url, google_handle) = spawn_one_shot_http_server(
            "500 Internal Server Error",
            "application/json",
            google_body,
        );

        let ddg_body = serde_json::json!({
            "Results": [],
            "RelatedTopics": [
                {
                    "Text": "Rust official site",
                    "FirstURL": "https://www.rust-lang.org/"
                }
            ]
        })
        .to_string();
        let (duckduckgo_engine_url, ddg_handle) =
            spawn_one_shot_http_server("200 OK", "application/json", ddg_body);

        let key_env = "AUTONOETIC_TEST_GOOGLE_KEY_AUTO";
        let cx_env = "AUTONOETIC_TEST_GOOGLE_CX_AUTO";
        let prior_key = std::env::var(key_env).ok();
        let prior_cx = std::env::var(cx_env).ok();
        std::env::set_var(key_env, "test-api-key");
        std::env::set_var(cx_env, "test-cx-id");

        let args = serde_json::json!({
            "query": "rust language",
            "provider": "auto",
            "google_engine_url": google_engine_url,
            "duckduckgo_engine_url": duckduckgo_engine_url,
            "google_api_key_env": key_env,
            "google_engine_id_env": cx_env
        });

        let registry = default_registry();
        let result = registry
            .execute(
                "web.search",
                &manifest,
                &policy,
                temp.path(),
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("auto provider should fall back to duckduckgo");

        match prior_key {
            Some(value) => std::env::set_var(key_env, value),
            None => std::env::remove_var(key_env),
        }
        match prior_cx {
            Some(value) => std::env::set_var(cx_env, value),
            None => std::env::remove_var(cx_env),
        }

        google_handle
            .join()
            .expect("google server thread should join");
        ddg_handle.join().expect("ddg server thread should join");

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("web.search result should decode");
        assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
        assert_eq!(
            parsed.get("requested_provider"),
            Some(&serde_json::json!("auto"))
        );
        assert_eq!(
            parsed.get("provider"),
            Some(&serde_json::json!("duckduckgo"))
        );
        let attempted = parsed
            .get("attempted_providers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(attempted.contains(&serde_json::json!("google")));
        assert!(attempted.contains(&serde_json::json!("duckduckgo")));
        assert!(parsed
            .get("fallback_reason")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .contains("google provider failed"));
    }

    #[test]
    fn test_web_search_cache_hits_without_second_network_call() {
        let manifest = test_manifest(vec![Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");

        let body = serde_json::json!({
            "Results": [],
            "RelatedTopics": [
                {
                    "Text": "Rust language homepage",
                    "FirstURL": "https://www.rust-lang.org/"
                }
            ]
        })
        .to_string();
        let (engine_url, hits, handle) =
            spawn_counting_http_server("200 OK", "application/json", body, 1);

        let unique_query = format!(
            "rust cache {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after unix epoch")
                .as_nanos()
        );
        let args = serde_json::json!({
            "query": unique_query,
            "provider": "duckduckgo",
            "duckduckgo_engine_url": engine_url,
            "cache_ttl_secs": 300
        });

        let registry = default_registry();
        let first = registry
            .execute(
                "web.search",
                &manifest,
                &policy,
                temp.path(),
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("first web.search call should succeed");
        let second = registry
            .execute(
                "web.search",
                &manifest,
                &policy,
                temp.path(),
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("second web.search call should succeed");

        let first_parsed: serde_json::Value =
            serde_json::from_str(&first).expect("first response should decode");
        let second_parsed: serde_json::Value =
            serde_json::from_str(&second).expect("second response should decode");
        assert_eq!(
            first_parsed.get("cache_hit"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            second_parsed.get("cache_hit"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        handle.join().expect("server thread should join");
    }

    #[test]
    fn test_agent_spawn_tool_validates_non_empty_message() {
        let manifest = test_manifest(vec![Capability::AgentSpawn { max_children: 2 }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        let parent_dir = agents_dir.join("planner.default");
        std::fs::create_dir_all(&parent_dir).expect("parent dir should create");

        let args = serde_json::json!({
            "agent_id": "researcher.default",
            "message": ""
        });

        let registry = default_registry();
        let err = registry
            .execute(
                "agent.spawn",
                &manifest,
                &policy,
                &parent_dir,
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                Some("session-1"),
                None,
                None,
                None,
                None,
            )
            .expect_err("empty message should be rejected");
        assert!(err.to_string().contains("message must not be empty"));
    }

    #[test]
    fn test_agent_spawn_tool_accepts_metadata_argument() {
        let manifest = test_manifest(vec![Capability::AgentSpawn { max_children: 2 }]);
        let policy = PolicyEngine::new(manifest.clone());
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        let parent_dir = agents_dir.join("planner.default");
        std::fs::create_dir_all(&parent_dir).expect("parent dir should create");

        let args = serde_json::json!({
            "agent_id": "researcher.default",
            "message": "",
            "metadata": {
                "delegated_role": "researcher",
                "expected_outputs": ["summary.md", "sources.json"]
            }
        });

        let registry = default_registry();
        let err = registry
            .execute(
                "agent.spawn",
                &manifest,
                &policy,
                &parent_dir,
                None,
                &serde_json::to_string(&args).expect("json should encode"),
                Some("session-1"),
                None,
                None,
                None,
                None,
            )
            .expect_err("empty message should still be rejected");
        assert!(err.to_string().contains("message must not be empty"));
    }

}
