use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::{NativeToolRunContext, SandboxPidGuard};
use crate::runtime::approved_exec_cache::{
    compute_fingerprint, normalize_targets, ApprovedExecCache,
};
use crate::runtime::tools::{
    build_approval_details, dependency_plan_from_args_or_lock, dependency_plan_from_lock,
    load_session_content_mounts, NativeTool, NativeToolRegistry, SandboxExecArgs,
};
use crate::sandbox::{SandboxDriverKind, SandboxMount, SandboxRunner};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::{
    ApprovalDecision, ApprovalRequest, LayerMountScopeInfo, ScheduledAction,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::layer::LayerApprovalScope;
use autonoetic_types::runtime_lock::LockedLayerMount;
use autonoetic_types::tool_error::tagged;
use secrecy::ExposeSecret;
use std::collections::BTreeSet;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SandboxExecTool));
}

pub struct SandboxExecTool;

pub struct LayerMount {
    pub layer_id: String,
    pub mount_path: String,
}

pub fn extract_and_mount_layers(
    layers: &[LayerMount],
    gw_dir: &Path,
    source_label: &str,
    mounts: &mut Vec<SandboxMount>,
    python_paths: &mut Vec<String>,
) -> anyhow::Result<()> {
    let layer_store = crate::layer_store::LayerStore::new(gw_dir, Default::default())?;
    for layer in layers {
        let layer_temp_base = std::env::temp_dir()
            .join("autonoetic_layer")
            .join(&layer.layer_id);
        std::fs::create_dir_all(&layer_temp_base)?;

        if let Err(e) = layer_store.extract_to(&layer.layer_id, &layer_temp_base) {
            tracing::warn!(
                target: "sandbox",
                layer_id = %layer.layer_id,
                source = source_label,
                error = %e,
                "Failed to extract layer for sandbox mounting"
            );
            continue;
        }

        tracing::info!(
            target: "sandbox",
            layer_id = %layer.layer_id,
            mount_path = %layer.mount_path,
            source = source_label,
            "Mounting layer into sandbox"
        );

        mounts.push(SandboxMount {
            source: layer_temp_base,
            dest: layer.mount_path.clone(),
            readonly: true,
        });

        let python_site_packages = std::path::Path::new(&layer.mount_path)
            .join("lib")
            .join("python3.12")
            .join("site-packages");
        if python_site_packages.starts_with("/") {
            python_paths.push(python_site_packages.to_string_lossy().to_string());
        }
        python_paths.push(layer.mount_path.clone());
    }
    Ok(())
}

/// True if `pattern` matches a package manager install command.
/// Used to detect when a non-NetworkAccess agent is trying to install
/// dependencies and should be redirected to packager.default.
fn is_package_manager_command(pattern: &str) -> bool {
    matches!(
        pattern,
        "pip install"
            | "pip3 install"
            | "npm install"
            | "yarn install"
            | "yarn add"
            | "pnpm install"
            | "bun install"
            | "go get"
            | "go mod download"
            | "cargo install"
            | "gem install"
            | "composer install"
            | "composer require"
    )
}

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

/// True if `command` appears to use a content handle (`cnt_<8hex>`) as a sandbox path.
/// Handles are stable content references for content.read, not filesystem paths.
fn sandbox_command_misuses_content_handle_as_path(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0usize;
    while i + 12 <= bytes.len() {
        if &bytes[i..i + 4] == b"cnt_" {
            let hex = &bytes[i + 4..i + 12];
            if hex.iter().all(|b| b.is_ascii_hexdigit()) {
                let prev_ok = if i == 0 {
                    true
                } else {
                    matches!(
                        bytes[i - 1],
                        b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'`' | b'/'
                    )
                };
                let next_ok = if i + 12 >= bytes.len() {
                    true
                } else {
                    matches!(
                        bytes[i + 12],
                        b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'`' | b'/'
                    )
                };
                if prev_ok && next_ok {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

pub fn extract_artifact_source(gw_dir: &Path, artifact_id: &str) -> String {
    let mut artifact_code = String::new();
    if let Ok(store) = crate::artifact_store::ArtifactStore::new(gw_dir) {
        if let Ok(bundle) = store.inspect(artifact_id) {
            let content_store = crate::runtime::content_store::ContentStore::new(gw_dir).ok();
            for file in &bundle.files {
                if let Some(cs) = &content_store {
                    if let Ok(content) = cs.read(&file.handle) {
                        if let Ok(text) = String::from_utf8(content) {
                            artifact_code.push_str(&format!("\n# --- {} ---\n", file.name));
                            artifact_code.push_str(&text);
                        }
                    }
                }
            }
        }
    }
    artifact_code
}

pub fn extract_and_cache_artifact_analysis(
    gw_dir: &Path,
    artifact_id: &str,
) -> Option<(String, crate::runtime::remote_access::RemoteAccessAnalysis)> {
    let cache_path = gw_dir
        .join("artifacts")
        .join(artifact_id)
        .join("remote_access_analysis.json");

    if cache_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&cache_path) {
            if let Ok(analysis) = serde_json::from_str::<
                crate::runtime::remote_access::RemoteAccessAnalysis,
            >(&content)
            {
                let code = extract_artifact_source(gw_dir, artifact_id);
                if !code.is_empty() {
                    return Some((code, analysis));
                }
            }
        }
    }

    let code = extract_artifact_source(gw_dir, artifact_id);
    if code.is_empty() {
        return None;
    }

    let analysis = crate::runtime::remote_access::RemoteAccessAnalyzer::analyze_code(&code);
    let _ = std::fs::create_dir_all(cache_path.parent().unwrap());
    let _ = std::fs::write(
        &cache_path,
        serde_json::to_string(&analysis).unwrap_or_default(),
    );

    Some((code, analysis))
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

const SANDBOX_APPROVAL_SUMMARY_CMD_MAX: usize = 260;
const SANDBOX_APPROVAL_INTENT_PREVIEW_MAX: usize = 280;
const SANDBOX_APPROVAL_PATTERN_APPEND_MAX: usize = 8;

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

fn infer_primary_script_display(command: &str) -> Option<String> {
    let trimmed = command.trim();
    for python_cmd in &["python3", "python", "python3.11", "python3.12"] {
        if trimmed.starts_with(python_cmd) || trimmed.starts_with(&format!("{python_cmd} ")) {
            let after_python = trimmed[python_cmd.len()..].trim();
            if after_python.starts_with('-') {
                return None;
            }
            let script_path = after_python.split_whitespace().next()?;
            if script_path.is_empty() {
                return None;
            }
            return Some(script_path.to_string());
        }
    }
    None
}

/// One-line label for approvals (session overview, notifications).
fn sandbox_approval_summary_line(agent_id: &str, command: &str, intent: Option<&str>) -> String {
    let cmd = truncate_unicode_display(command, SANDBOX_APPROVAL_SUMMARY_CMD_MAX);
    match intent.map(str::trim).filter(|s| !s.is_empty()) {
        Some(i) => {
            let ip = truncate_unicode_display(i, SANDBOX_APPROVAL_INTENT_PREVIEW_MAX);
            format!("Sandbox exec ({agent_id}): {ip} — `{cmd}`")
        }
        None => match infer_primary_script_display(command) {
            Some(s) => format!("Sandbox exec ({agent_id}): `{cmd}` · analyzes `{s}`"),
            None => format!("Sandbox exec ({agent_id}): `{cmd}`"),
        },
    }
}

fn sandbox_approval_operator_reason(
    command: &str,
    intent: Option<&str>,
    remote_summary: &str,
    remote_suffix: &str,
    patterns: &[crate::runtime::remote_access::DetectedPattern],
) -> String {
    let mut sections: Vec<String> = Vec::new();
    sections.push(format!("What will run:\n{}", command.trim()));
    if let Some(i) = intent.map(str::trim).filter(|s| !s.is_empty()) {
        sections.push(format!("Agent-stated purpose:\n{}", i.trim()));
    }
    if let Some(s) = infer_primary_script_display(command) {
        sections.push(format!(
            "Analyzed for network/reachable APIs: `{s}` (file content loaded from session/workspace)",
        ));
    }
    let mut trigger = format!(
        "Why approval is required:\n{}{}",
        remote_summary.trim(),
        remote_suffix.trim_end()
    );
    if !patterns.is_empty() {
        trigger.push_str("\n\nStatic analysis cues:");
        for p in patterns.iter().take(SANDBOX_APPROVAL_PATTERN_APPEND_MAX) {
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
        if patterns.len() > SANDBOX_APPROVAL_PATTERN_APPEND_MAX {
            trigger.push_str(&format!(
                "\n- … (+{} more pattern(s))",
                patterns.len() - SANDBOX_APPROVAL_PATTERN_APPEND_MAX
            ));
        }
    }
    sections.push(trigger);
    sections.join("\n\n")
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

pub fn apply_network_isolation_failure_to_result(
    body: &mut serde_json::Value,
    stdout: &str,
    stderr: &str,
    has_network_cap: bool,
    evaluation_blocked: bool,
) -> Option<Vec<String>> {
    let combined_output = format!("{stdout}\n{stderr}");
    let network_errors = detect_network_errors_in_output(&combined_output);
    if network_errors.is_empty() {
        return None;
    }

    let reason = if evaluation_blocked {
        "Promotion evaluation sessions have no network access (gateway constitution rule R+16). \
         Use constitution.read to inspect the rule. Mock all external services in tests so they \
         run offline."
    } else if has_network_cap {
        "This agent declares NetworkAccess but this run did not enable the network \
         namespace (e.g. missing operator approval or misconfiguration)."
    } else {
        "This agent does not declare NetworkAccess: outbound calls are blocked. \
         Add scoped NetworkAccess, or use packager.default layers so tests run offline."
    };

    let summary = format!(
        "Sandbox ran without outbound network and output indicates a network failure \
         ({}). {}",
        network_errors.join(", "),
        reason
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

pub fn effective_root_session_id(session_id: &str, explicit_root: Option<&str>) -> String {
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

pub fn approved_requests_cover_targets(
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

fn layer_mount_approval_covers_scope_issues(
    approved_layers: &[LayerMountScopeInfo],
    current_issues: &[LayerMountScopeInfo],
) -> bool {
    current_issues.iter().all(|issue| {
        approved_layers.iter().any(|approved| {
            approved.layer_id == issue.layer_id
                && approved.digest == issue.digest
                && approved.mount_path == issue.mount_path
                && approved.source == issue.source
                && issue
                    .build_time_approved_hosts
                    .iter()
                    .all(|host| approved.build_time_approved_hosts.contains(host))
                && issue
                    .unapproved_delta
                    .iter()
                    .all(|host| approved.unapproved_delta.contains(host))
        })
    })
}

/// Reads the approval scope (and human-readable name) for a layer from its manifest on disk.
/// Returns `(scope, name)` where `name` is the friendly layer name stored at capture time.
fn load_layer_manifest_info(
    gateway_dir: &Path,
    layer_id: &str,
) -> anyhow::Result<(Option<LayerApprovalScope>, String)> {
    let manifest_path = gateway_dir
        .join("layers")
        .join(layer_id)
        .join("manifest.json");
    let content = std::fs::read_to_string(&manifest_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to read layer manifest for layer '{}' at '{}': {}",
            layer_id,
            manifest_path.display(),
            e
        )
    })?;
    let manifest: autonoetic_types::layer::LayerManifest =
        serde_json::from_str(&content).map_err(|e| {
            anyhow::anyhow!(
                "failed to parse layer manifest for layer '{}' at '{}': {}",
                layer_id,
                manifest_path.display(),
                e
            )
        })?;
    let name = if manifest.name.is_empty() {
        layer_id.to_string()
    } else {
        manifest.name
    };
    Ok((manifest.approval_scope, name))
}

/// Information about a single layer needed for scope checking.
/// Captures immutable layer identity plus caller-provided context (name, source).
struct LayerScopeCheckInfo {
    layer_id: String,
    /// Caller-provided name (from artifact bundle or same as layer_id for runtime.lock)
    name: String,
    mount_path: String,
    digest: String,
    /// Where this layer is used: "artifact:<id>" or "runtime.lock"
    source: String,
}

/// For each layer, returns a `LayerMountScopeInfo` if the layer's build-time approved
/// hosts are not fully covered by `session_grants`.
/// Layers without an `approval_scope` (legacy or network-free) are skipped.
fn collect_layer_scope_issues(
    layers: &[LayerScopeCheckInfo],
    gateway_dir: &Path,
    session_grants: &[String],
) -> anyhow::Result<Vec<LayerMountScopeInfo>> {
    let granted: std::collections::HashSet<&str> =
        session_grants.iter().map(|s| s.as_str()).collect();
    let mut issues = Vec::new();
    for layer_info in layers {
        let (scope_opt, manifest_name) =
            load_layer_manifest_info(gateway_dir, &layer_info.layer_id)?;
        let Some(scope) = scope_opt else {
            continue;
        };
        if scope.approved_hosts.is_empty() {
            continue;
        }
        let unapproved: Vec<String> = scope
            .approved_hosts
            .iter()
            .filter(|h| !granted.contains(h.as_str()))
            .cloned()
            .collect();
        if !unapproved.is_empty() {
            // Prefer the manifest name (captured at build time) over the caller-provided name.
            let display_name = if manifest_name != layer_info.layer_id {
                manifest_name
            } else {
                layer_info.name.clone()
            };
            issues.push(LayerMountScopeInfo {
                layer_id: layer_info.layer_id.clone(),
                digest: layer_info.digest.clone(),
                name: display_name,
                mount_path: layer_info.mount_path.clone(),
                build_time_approved_hosts: scope.approved_hosts.clone(),
                unapproved_delta: unapproved,
                source: layer_info.source.clone(),
            });
        }
    }
    Ok(issues)
}

impl NativeTool for SandboxExecTool {
    fn name(&self) -> &'static str {
        "sandbox_exec"
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
                    "intent": {
                        "type": "string",
                        "description": "Short human explanation for the operator (recommended when this run may need approval): what the command does and why it is safe/necessary."
                    },
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
        let mut layer_mount_approved = false;
        let mut approved_layer_mount_layers: Option<Vec<LayerMountScopeInfo>> = None;
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
                                target: "sandbox_exec",
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
                                args.artifact_id.as_deref(),
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
                                                target: "sandbox_exec", error = %e,
                                                fingerprint = %fingerprint,
                                                "Failed to record approved exec cache entry"
                                            );
                                        } else {
                                            tracing::info!(
                                                target: "sandbox_exec", fingerprint = %fingerprint,
                                                "Cached approved exec on approval_ref validation"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        ScheduledAction::LayerMount {
                            command: approved_cmd,
                            layers,
                        } => {
                            // LayerMount approval is tied to both the specific layer scope AND the command.
                            // Verify the incoming command matches what was originally approved.
                            if &args.command != approved_cmd {
                                return Err(tagged::Tagged::validation(anyhow::anyhow!(
                                    "approval_ref '{}' was approved for command {:?}, but received {:?}. \
                                     Layer mount approvals are command-specific to prevent scope bypass.",
                                    approval_ref,
                                    approved_cmd,
                                    args.command
                                ))
                                .into());
                            }
                            tracing::info!(
                                target: "sandbox_exec",
                                approval_ref = %approval_ref,
                                "Proceeding with approved layer mount (command and scope match approval)"
                            );
                            approval_validated_for_command = true;
                            layer_mount_approved = true;
                            approved_layer_mount_layers = Some(layers.clone());
                        }
                        _ => {
                            return Err(tagged::Tagged::validation(anyhow::anyhow!(
                                "approval_ref '{}' does not reference a sandbox.exec or layer_mount action",
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

        if let Some(artifact_id) = args.artifact_id.as_deref() {
            if artifact_id.starts_with("impl_") {
                return Ok(crate::runtime::tools::implicit_artifact_id_error(
                    self.name(),
                    artifact_id,
                )
                .to_string());
            }
        }

        if sandbox_command_misuses_content_digest_as_path(&effective_command) {
            anyhow::bail!(
                "sandbox.exec: content digests (sha256:...) are not filesystem paths in the sandbox. \
Use the path from content.write (`sandbox_path`, typically /tmp/<name>), or pass artifact_id so artifact files are mounted under /tmp/."
            );
        }

        if sandbox_command_misuses_content_handle_as_path(&effective_command) {
            anyhow::bail!(
                "sandbox.exec: content handles (cnt_...) are not filesystem paths in the sandbox. \
Use content.read(cnt_...) to inspect content by handle, or use the path returned by content.write (`sandbox_path`, typically /tmp/<name>) when executing files."
            );
        }

        let decision = policy.can_exec_shell_detailed(&effective_command);
        if !decision.is_allowed() {
            let reason = decision.explain_shell_denial("Sandbox execution");
            return Err(tagged::Tagged::permission_with_rules(
                anyhow::anyhow!(reason),
                decision
                    .enforced_rules
                    .into_iter()
                    .map(|rule| rule.to_string())
                    .collect(),
            )
            .into());
        }

        let mut artifact_analysis_override: Option<
            crate::runtime::remote_access::RemoteAccessAnalysis,
        > = None;
        let code_to_analyze = if let Some(ref aid) = args.artifact_id {
            if let Some(gw_dir) = gateway_dir {
                match extract_and_cache_artifact_analysis(gw_dir, aid) {
                    Some((code, analysis)) => {
                        artifact_analysis_override = Some(analysis);
                        code
                    }
                    None => extract_code_for_analysis(
                        &effective_command,
                        agent_dir,
                        gateway_dir,
                        session_id,
                    ),
                }
            } else {
                extract_code_for_analysis(&effective_command, agent_dir, gateway_dir, session_id)
            }
        } else {
            extract_code_for_analysis(&effective_command, agent_dir, gateway_dir, session_id)
        };

        let agent_has_network_access = manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { .. }));

        if !agent_has_network_access && !approval_validated_for_command {
            let early_analysis =
                crate::runtime::remote_access::RemoteAccessAnalyzer::detect_network_commands(
                    &code_to_analyze,
                );
            let is_dep_install = early_analysis
                .iter()
                .any(|p| p.category == "network_command" && is_package_manager_command(&p.pattern));
            if is_dep_install {
                let detected: Vec<String> = early_analysis
                    .iter()
                    .filter(|p| p.category == "network_command")
                    .map(|p| p.pattern.clone())
                    .collect();
                return Ok(serde_json::json!({
                    "ok": false,
                    "dependency_layer_required": true,
                    "recommended_agent": "packager.default",
                    "reason": "External packages must be resolved into layers by packager.default before execution.",
                    "detected_commands": detected,
                }).to_string());
            }
        }

        let dep_packages: Option<Vec<String>> =
            args.dependencies.as_ref().map(|d| d.packages.clone());
        let remote_analysis = if let Some(artifact_analysis) = artifact_analysis_override {
            artifact_analysis
        } else {
            crate::runtime::remote_access::RemoteAccessAnalyzer::analyze_command_and_dependencies(
                &code_to_analyze,
                dep_packages.as_deref(),
            )
        };

        let agent_has_network_access = manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { .. }));

        if agent_has_network_access
            && remote_analysis.requires_approval
            && !approval_validated_for_command
        {
            tracing::info!(
                target: "sandbox_exec",
                agent_id = %manifest.agent.id,
                patterns = ?remote_analysis.detected_patterns,
                "Agent has NetworkAccess capability — auto-approving remote access patterns"
            );
            approval_validated_for_command = true;
        }

        let mut safe_inspection_bypass = false;
        if remote_analysis.requires_approval
            && !approval_validated_for_command
            && crate::runtime::remote_access::is_safe_inspection_command(&effective_command)
        {
            tracing::info!(
                target: "sandbox_exec",
                command = %effective_command,
                "Safe inspection command — skipping approval (no network needed)"
            );
            approval_validated_for_command = true;
            safe_inspection_bypass = true;
        }

        if !agent_has_network_access
            && !approval_validated_for_command
            && remote_analysis.requires_approval
        {
            let is_dep_install = remote_analysis
                .detected_patterns
                .iter()
                .any(|p| p.category == "network_command" && is_package_manager_command(&p.pattern));
            if is_dep_install {
                let detected: Vec<String> = remote_analysis
                    .detected_patterns
                    .iter()
                    .filter(|p| p.category == "network_command")
                    .map(|p| p.pattern.clone())
                    .collect();
                return Ok(serde_json::json!({
                    "ok": false,
                    "dependency_layer_required": true,
                    "recommended_agent": "packager.default",
                    "reason": "External packages must be resolved into layers by packager.default before execution.",
                    "detected_commands": detected,
                }).to_string());
            }
        }

        tracing::info!(
            target: "sandbox_exec",
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
                    let summary = sandbox_approval_summary_line(&manifest.agent.id, &cmd, None);
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

            let coverage = crate::runtime::remote_access::classify_network_coverage(
                &detected_patterns,
                normalized_targets.clone(),
            );

            match &coverage {
                crate::runtime::remote_access::NetworkCoverage::Concrete { targets } => {
                    let targets = targets.clone();

                    // Cache lookup: check if this exact execution was previously approved.
                    if let Some(gw_dir) = gateway_dir {
                        let fingerprint = compute_fingerprint(
                            &manifest.agent.id,
                            &targets,
                            &code_to_analyze,
                            args.artifact_id.as_deref(),
                        );
                        if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                            if let Some(entry) = cache.find(&fingerprint) {
                                tracing::info!(
                                    target: "sandbox_exec",
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

                    // Approved request coverage: check recently approved requests in the store.
                    if !approval_validated_for_command {
                        if let (Some(_cfg), Some(gw_store), Some(sid)) =
                            (config, &gateway_store, session_id)
                        {
                            let root_sid = crate::runtime::content_store::root_session_id(sid);
                            if !targets.is_empty() {
                                if let Ok(approved) =
                                    gw_store.get_approved_approvals_for_root(root_sid)
                                {
                                    if approved_requests_cover_targets(
                                        &approved,
                                        &targets,
                                        agent_dir,
                                        gateway_dir,
                                    ) {
                                        tracing::info!(
                                            target: "sandbox_exec",
                                            targets = ?targets,
                                            "Approved request covers targets, skipping new approval"
                                        );
                                        approval_validated_for_command = true;

                                        // Backfill exec cache so future checks hit the fast path
                                        let fingerprint = compute_fingerprint(
                                            &manifest.agent.id,
                                            &targets,
                                            &code_to_analyze,
                                            args.artifact_id.as_deref(),
                                        );
                                        if let Some(gw_dir) = gateway_dir {
                                            if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                                                if cache.find(&fingerprint).is_none() {
                                                    let entry = crate::runtime::approved_exec_cache::ApprovedExecEntry {
                                                        fingerprint: fingerprint.clone(),
                                                        agent_id: manifest.agent.id.clone(),
                                                        remote_targets: targets.clone(),
                                                        code_content: code_to_analyze.clone(),
                                                        approval_request_id: approved.iter()
                                                            .find(|r| matches!(r.action, ScheduledAction::SandboxExec { .. }))
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
                    }

                    // Session grants: host-level approvals within root session.
                    if !approval_validated_for_command {
                        if let (Some(gw_store), Some(sid)) = (&gateway_store, session_id) {
                            let root_sid = crate::runtime::content_store::root_session_id(sid);
                            if !targets.is_empty() {
                                if gw_store.session_grants_cover_targets(&root_sid, &targets) {
                                    tracing::info!(
                                        target: "sandbox_exec",
                                        agent_id = %manifest.agent.id,
                                        root_session_id = %root_sid,
                                        targets = ?targets,
                                        "Session grant covers targets — auto-approving sandbox exec"
                                    );
                                    approval_validated_for_command = true;
                                }
                            }
                        }
                    }
                }
                crate::runtime::remote_access::NetworkCoverage::Unresolved => {
                    // Network behavior present but no stable concrete host coverage.
                    // Skip cache, approved-requests, and session grants — go to pending check.
                }
                crate::runtime::remote_access::NetworkCoverage::None => {
                    // Should not happen (requires_approval is true), but handle gracefully.
                }
            }

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
                        let summary = sandbox_approval_summary_line(&manifest.agent.id, &cmd, None);
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
                    let summary = sandbox_approval_summary_line(
                        &manifest.agent.id,
                        &effective_command,
                        args.intent.as_deref(),
                    );
                    let remote_hint_suffix =
                        crate::runtime::remote_access::approval_remote_operator_suffix(
                            &normalized_targets,
                            &detected_patterns,
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
                        detected_hosts: Some(normalized_targets.clone()),
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
                    let mut request = autonoetic_types::background::ApprovalRequest {
                        request_id: request_id.clone(),
                        agent_id: manifest.agent.id.clone(),
                        session_id: sid.to_string(),
                        root_session_id: Some(root_session_id.to_string()),
                        action: action.clone(),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        status: None,
                        decided_at: None,
                        decided_by: None,
                        reason: Some(sandbox_approval_operator_reason(
                            &effective_command,
                            args.intent.as_deref(),
                            &remote_analysis.summary,
                            &remote_hint_suffix,
                            &detected_patterns,
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
                        min_dwell_ms: None,
                        confirm_phrase: None,
                    };
                    if let Some(store) = &gateway_store {
                        store.create_approval(&mut request).map_err(|e| {
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
                            "intent": args.intent.clone(),
                            "primary_script": infer_primary_script_display(&effective_command),
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
                        "stderr": format!(
                            "{}\n\nTechnical: Remote access scan: {}. Operator approval required to execute code that may reach the network/APIs.",
                            summary,
                            format!("{}{}", remote_analysis.summary, remote_hint_suffix)
                        ),
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

                let detected_patterns_fallback = remote_analysis.detected_patterns.clone();
                let normalized_targets = normalize_targets(&detected_patterns_fallback);
                let remote_hint_suffix =
                    crate::runtime::remote_access::approval_remote_operator_suffix(
                        &normalized_targets,
                        &detected_patterns_fallback,
                    );
                let summary_fallback = sandbox_approval_summary_line(
                    &manifest.agent.id,
                    &effective_command,
                    args.intent.as_deref(),
                );
                let reason_fallback = sandbox_approval_operator_reason(
                    &effective_command,
                    args.intent.as_deref(),
                    &remote_analysis.summary,
                    &remote_hint_suffix,
                    &detected_patterns_fallback,
                );
                return serde_json::to_string(&serde_json::json!({
                    "ok": false,
                    "exit_code": null,
                    "stdout": "",
                    "stderr": format!(
                        "{}\n\nTechnical: Remote access scan: {}. Operator approval required to execute code that may reach the network/APIs.",
                        summary_fallback,
                        format!("{}{}", remote_analysis.summary, remote_hint_suffix)
                    ),
                    "approval_required": true,
                    "remote_access_detected": true,
                    "detected_patterns": remote_analysis.detected_patterns,
                    "approval": {
                        "kind": "sandbox_exec",
                        "reason": reason_fallback,
                        "summary": summary_fallback,
                        "requested_by_agent_id": manifest.agent.id,
                        "session_id": session_id.unwrap_or(""),
                        "retry_field": "approval_ref",
                        "subject": {
                            "command": effective_command,
                            "intent": args.intent.clone(),
                            "primary_script": infer_primary_script_display(&effective_command),
                            "dependencies": args.dependencies.as_ref().map(|d| serde_json::json!({
                                "runtime": d.runtime,
                                "packages": d.packages,
                            })),
                            "remote_access_detected": true,
                            "detected_patterns": detected_patterns_fallback,
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
        // Parse runtime.lock once — used for both dependency plan and layer mounting.
        let parsed_lock: Option<autonoetic_types::runtime_lock::RuntimeLock> = {
            if args.dependencies.is_none() {
                let lock_path = agent_dir.join(&manifest.runtime.runtime_lock);
                if lock_path.exists() {
                    match crate::runtime_lock::resolve_runtime_lock(&lock_path) {
                        Ok(lock) => Some(lock),
                        Err(e) => {
                            tracing::warn!(
                                target: "sandbox",
                                path = %lock_path.display(),
                                error = %e,
                                "Failed to parse runtime.lock; skipping layer mounting"
                            );
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };

        let dep_plan = if args.dependencies.is_some() {
            dependency_plan_from_args_or_lock(manifest, agent_dir, args.dependencies)?
        } else if let Some(ref lock) = parsed_lock {
            dependency_plan_from_lock(lock)?
        } else {
            None
        };
        let runtime_lock_layers: Vec<LockedLayerMount> =
            parsed_lock.map(|lock| lock.layers).unwrap_or_default();

        // ── Layer approval scope check ─────────────────────────────────────────────
        // Before mounting any layer, verify that the current session's approval grants
        // cover all hosts the layer was built with. A layer carrying NetworkAccess to
        // pypi.org must not be silently mounted into a session that never approved pypi.org.
        // Agents with NetworkAccess capability (already gated by operator at install) skip
        // this check — they implicitly approve any layer scope.
        if !agent_has_network_access {
            if let Some(gw_dir) = gateway_dir {
                // Gather session grants for scope subsumption check.
                let session_grants: Vec<String> = if let Some(store) = &gateway_store {
                    let sid = session_id.unwrap_or("");
                    let root = crate::runtime::content_store::root_session_id(sid);
                    store.get_session_grants(&root).unwrap_or_default()
                } else {
                    Vec::new()
                };

                // Resolve the effective artifact_id early for the scope check.
                let effective_artifact_id_for_scope = args
                    .artifact_id
                    .as_ref()
                    .or_else(|| run_context.as_ref().and_then(|c| c.artifact_id.as_ref()));

                // Collect layers (artifact + runtime.lock) for scope checking.
                let mut scope_layers: Vec<LayerScopeCheckInfo> = Vec::new();

                // Artifact layers.
                if let Some(artifact_id) = effective_artifact_id_for_scope {
                    if let Ok(artifact_store) = crate::artifact_store::ArtifactStore::new(gw_dir) {
                        if let Ok(bundle) = artifact_store.inspect(artifact_id) {
                            for l in &bundle.layers {
                                scope_layers.push(LayerScopeCheckInfo {
                                    layer_id: l.layer_id.clone(),
                                    name: l.name.clone(),
                                    mount_path: l.mount_path.clone(),
                                    digest: l.digest.clone(),
                                    source: format!("artifact:{}", artifact_id),
                                });
                            }
                        }
                    }
                }

                // Runtime.lock layers.
                for l in &runtime_lock_layers {
                    scope_layers.push(LayerScopeCheckInfo {
                        layer_id: l.layer_id.clone(),
                        name: l.layer_id.clone(), // name not stored in LockedLayerMount; use layer_id
                        mount_path: l.mount_path.clone(),
                        digest: l.digest.clone(),
                        source: "runtime.lock".to_string(),
                    });
                }

                let scope_issues =
                    collect_layer_scope_issues(&scope_layers, gw_dir, &session_grants)?;

                if !scope_issues.is_empty() {
                    if layer_mount_approved {
                        let approved_layers = approved_layer_mount_layers.as_deref().unwrap_or(&[]);
                        if !layer_mount_approval_covers_scope_issues(approved_layers, &scope_issues)
                        {
                            return Err(tagged::Tagged::validation(anyhow::anyhow!(
                                "approval_ref '{}' does not cover the currently requested layer scope; retry without approval_ref to request approval for the new layers or hosts",
                                args.approval_ref.as_deref().unwrap_or("")
                            ))
                            .into());
                        }
                        tracing::info!(
                            target: "sandbox_exec",
                            layer_count = scope_issues.len(),
                            "Approved LayerMount still covers current layer scope"
                        );
                    } else {
                        tracing::warn!(
                            target: "sandbox_exec",
                            agent_id = %manifest.agent.id,
                            issue_count = scope_issues.len(),
                            "Layer mount approval required: build-time scope not covered by session grants"
                        );

                        if let Some(cfg) = config {
                            let request_id =
                                format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
                            let summary = format!(
                            "Layer mount approval: {} layer(s) with unapproved build-time hosts",
                            scope_issues.len()
                        );
                            let action = ScheduledAction::LayerMount {
                                layers: scope_issues.clone(),
                                command: effective_command.clone(),
                            };
                            let approval_workflow_id = {
                                let sid = session_id.unwrap_or("");
                                let root = crate::runtime::content_store::root_session_id(sid);
                                crate::scheduler::resolve_workflow_id_for_root_session(cfg, &root)
                                    .ok()
                                    .flatten()
                            };
                            let sid = session_id.unwrap_or("");
                            let root_session_id =
                                crate::runtime::content_store::root_session_id(sid);
                            let all_unapproved: Vec<String> = scope_issues
                                .iter()
                                .flat_map(|i| i.unapproved_delta.iter().cloned())
                                .collect::<std::collections::BTreeSet<_>>()
                                .into_iter()
                                .collect();
                            let mut request = autonoetic_types::background::ApprovalRequest {
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
                                "Layer mount scope check: {} layer(s) were captured with network access to hosts [{}] not yet approved in this session.",
                                scope_issues.len(),
                                all_unapproved.join(", ")
                            )),
                            evidence_ref: None,
                            workflow_id: approval_workflow_id.clone(),
                            decision_reason: None,
                            approval_level:
                                crate::scheduler::approval::resolve_approval_level(
                                    cfg, &action,
                                ),
                            task_id: match (&approval_workflow_id, session_id) {
                                (Some(wf_id), Some(sid)) => {
                                    crate::scheduler::resolve_task_id_for_session(
                                        cfg, None, wf_id, sid,
                                    )
                                    .ok()
                                    .flatten()
                                }
                                _ => None,
                            },
                            similar_to_request_id: None,
                            similarity_score: None,
                            min_dwell_ms: None,
                            confirm_phrase: None,
                        };
                            if let Some(store) = &gateway_store {
                                store.create_approval(&mut request).map_err(|e| {
                                    anyhow::anyhow!(
                                        "Failed to persist layer mount approval request '{}': {}",
                                        request_id,
                                        e
                                    )
                                })?;
                            } else {
                                anyhow::bail!(
                                "GatewayStore missing; cannot persist layer mount approval request '{}'",
                                request_id
                            );
                            }
                            let approval = build_approval_details(
                                &request,
                                "layer_mount",
                                summary.clone(),
                                "approval_ref",
                                serde_json::json!({
                                    "layers": scope_issues.iter().map(|i| serde_json::json!({
                                        "layer_id": i.layer_id,
                                        "name": i.name,
                                        "mount_path": i.mount_path,
                                        "source": i.source,
                                        "build_time_approved_hosts": i.build_time_approved_hosts,
                                        "unapproved_delta": i.unapproved_delta,
                                    })).collect::<Vec<_>>(),
                                    "command": effective_command,
                                }),
                            );
                            return serde_json::to_string(&serde_json::json!({
                            "ok": false,
                            "exit_code": null,
                            "stdout": "",
                            "stderr": format!(
                                "Layer mount approval required: {} layer(s) were captured with network access to hosts [{}] not yet approved in this session. Retry with approval_ref after operator approves.",
                                scope_issues.len(),
                                all_unapproved.join(", ")
                            ),
                            "approval_required": true,
                            "layer_mount_approval_required": true,
                            "request_id": request_id,
                            "suspended": true,
                            "message": format!(
                                "Execution suspended pending layer mount approval ({}). Retry sandbox.exec with approval_ref after operator approves.",
                                request_id
                            ),
                            "approval": approval
                        }))
                        .map_err(Into::into);
                        }

                        // No config available — fail closed rather than silently bypassing the gate.
                        // Layer scope violations must be explicitly approved; a misconfigured gateway
                        // should not silently allow untrusted supply-chain content to run.
                        return Err(anyhow::anyhow!(
                        "Layer mount approval required: {} layer(s) have build-time scope not covered by \
                         session grants, but GatewayConfig is not available to enforce approvals. \
                         Approvals cannot be bypass-able via misconfiguration.",
                        scope_issues.len()
                    ));
                    }
                }
            }
        }
        // ─────────────────────────────────────────────────────────────────────────

        let driver = SandboxDriverKind::parse(&manifest.runtime.sandbox)?;
        let agent_dir_str = agent_dir
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is not valid UTF-8"))?;

        // Resolve effective artifact_id: explicit arg takes priority, then fall back to
        // the artifact_id from the tool run context (set by parent agent.spawn).
        let effective_artifact_id = args
            .artifact_id
            .as_ref()
            .or_else(|| run_context.as_ref().and_then(|c| c.artifact_id.as_ref()));

        let mut layer_python_paths: Vec<String> = Vec::new();
        let session_content_mounts = if let Some(artifact_id) = effective_artifact_id {
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
                    readonly: false,
                });
            }

            let bundle = artifact_store.inspect(artifact_id)?;
            if !bundle.layers.is_empty() {
                let artifact_layers: Vec<LayerMount> = bundle
                    .layers
                    .iter()
                    .map(|l| LayerMount {
                        layer_id: l.layer_id.clone(),
                        mount_path: l.mount_path.clone(),
                    })
                    .collect();
                extract_and_mount_layers(
                    &artifact_layers,
                    gw_dir,
                    "artifact",
                    &mut mounts,
                    &mut layer_python_paths,
                )?;
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

        // Mount runtime.lock layers (pre-built dependency layers pinned in the agent's runtime closure).
        // These are mounted read-only alongside artifact layers, with PYTHONPATH injection for Python deps.
        let mut runtime_lock_mounts: Vec<SandboxMount> = Vec::new();
        if !runtime_lock_layers.is_empty() {
            if let Some(gw_dir) = gateway_dir {
                let lock_layers: Vec<LayerMount> = runtime_lock_layers
                    .iter()
                    .map(|l| LayerMount {
                        layer_id: l.layer_id.clone(),
                        mount_path: l.mount_path.clone(),
                    })
                    .collect();
                extract_and_mount_layers(
                    &lock_layers,
                    gw_dir,
                    "runtime.lock",
                    &mut runtime_lock_mounts,
                    &mut layer_python_paths,
                )?;

                tracing::info!(
                    target: "sandbox",
                    runtime_lock_layer_count = runtime_lock_mounts.len(),
                    "Mounted runtime.lock layers into sandbox"
                );
            } else {
                tracing::warn!(
                    target: "sandbox",
                    "runtime.lock layers present but gateway_dir not configured; skipping mount"
                );
            }
        }

        let mut overrides =
            crate::sandbox::BwrapIsolationOverrides::from_capabilities(&manifest.capabilities);

        if approval_validated_for_command && !safe_inspection_bypass {
            overrides.share_net = true;
        }

        let has_evaluation_cap = manifest.capabilities.iter().any(|c| {
            matches!(
                c,
                autonoetic_types::capability::Capability::Evaluation { .. }
            )
        });
        if has_evaluation_cap {
            overrides.force_network_off = true;
            overrides.share_net = false;
        }

        let layer_python_path_str = layer_python_paths.join(":");
        let mut extra_env: Vec<(String, String)> = if !layer_python_path_str.is_empty() {
            vec![("PYTHONPATH".to_string(), layer_python_path_str)]
        } else {
            vec![]
        };

        if let Some(credential_mappings) = &args.credential_env {
            if let (Some(gw_dir), Some(store)) = (gateway_dir, &gateway_store) {
                let vault_dir = gw_dir.parent().unwrap_or(gw_dir);
                crate::vault::ensure_default_key(vault_dir)?;
                let vault_path = crate::vault::default_vault_path(vault_dir);
                let vault = match crate::vault::Vault::load_from_file(&vault_path) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            target: "sandbox_exec",
                            error = %e,
                            "Failed to load vault for credential_env resolution"
                        );
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
                        target: "sandbox_exec",
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

        // Merge runtime.lock mounts into session content mounts
        let mut all_mounts = session_content_mounts;
        if !runtime_lock_mounts.is_empty() {
            all_mounts.extend(runtime_lock_mounts);
        }

        let runner = if all_mounts.is_empty() {
            SandboxRunner::spawn_with_driver_and_dependencies_and_env(
                driver,
                agent_dir_str,
                &effective_command,
                dep_plan.as_ref(),
                Some(&overrides),
                &extra_env,
                root_session_id,
            )?
        } else {
            tracing::info!(
                target: "sandbox",
                mount_count = all_mounts.len(),
                "Mounting session content files into sandbox"
            );
            SandboxRunner::spawn_with_session_content_and_env(
                driver,
                agent_dir_str,
                &effective_command,
                dep_plan.as_ref(),
                all_mounts,
                Some(&overrides),
                &extra_env,
                root_session_id,
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
                has_evaluation_cap,
            ) {
                tracing::warn!(
                    target: "sandbox_exec",
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
                    // Build the approval scope to record in each captured layer.
                    // `approved_hosts` is populated from the static analysis of the build command.
                    // Because `share_net` was true, the operator explicitly approved network
                    // access to these hosts for this session — detected patterns == build-time
                    // approved hosts. Future sessions mounting this layer will need the same
                    // approval. Layers built without network access get None (no scope gate).
                    let capture_approval_scope: Option<LayerApprovalScope> = if overrides.share_net
                    {
                        let detected = normalize_targets(&remote_analysis.detected_patterns);
                        Some(LayerApprovalScope {
                            approved_hosts: detected,
                            built_by_agent_id: manifest.agent.id.clone(),
                            captured_at: chrono::Utc::now().to_rfc3339(),
                        })
                    } else {
                        None
                    };

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
                                        capture_approval_scope.clone(),
                                    ) {
                                        Ok(layer) => {
                                            tracing::info!(
                                                target: "sandbox",
                                                path = %cap.path,
                                                mount_as = %cap.mount_as,
                                                layer_id = %layer.layer_id,
                                                has_scope = layer.approval_scope.is_some(),
                                                "Captured sandbox path as layer"
                                            );
                                            captured_layers.push(serde_json::json!({
                                                "path": cap.path,
                                                "mount_as": cap.mount_as,
                                                "layer_id": layer.layer_id,
                                                "digest": layer.digest,
                                                "file_count": layer.file_count,
                                                "size_bytes": layer.size_bytes,
                                                "approval_scope": layer.approval_scope,
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
            let fingerprint = compute_fingerprint(
                &manifest.agent.id,
                &normalized_targets,
                &code_to_analyze,
                args.artifact_id.as_deref(),
            );
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
                                target: "sandbox_exec",
                                error = %e,
                                fingerprint = %fingerprint,
                                "Failed to record approved exec cache entry"
                            );
                        } else {
                            tracing::info!(
                                target: "sandbox_exec",
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
    use super::{
        sandbox_command_misuses_content_digest_as_path,
        sandbox_command_misuses_content_handle_as_path,
    };

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

    #[test]
    fn detects_cnt_handle_path_misuse() {
        assert!(sandbox_command_misuses_content_handle_as_path(
            "python3 /tmp/cnt_deadbeef"
        ));
        assert!(sandbox_command_misuses_content_handle_as_path(
            "cat cnt_deadbeef"
        ));
    }

    #[test]
    fn allows_normal_tmp_paths_without_cnt_pattern() {
        assert!(!sandbox_command_misuses_content_handle_as_path(
            "python3 /tmp/weather_agent.py"
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
            apply_network_isolation_failure_to_result(&mut body, "normal output", "", false, false);
        assert!(detected.is_none(), "should not detect network patterns");
        assert_eq!(body["ok"], json!(true));
        assert!(body.get("error_type").is_none());
        assert!(body.get("network_blocked").is_none());
    }

    #[test]
    fn evaluation_blocked_message_mentions_r16_and_constitution() {
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
            true,
            true,
        );
        assert!(detected.is_some());
        assert_eq!(body["ok"], json!(false));
        let msg = body["network_warning"].as_str().unwrap();
        assert!(
            msg.contains("R+16"),
            "evaluation-blocked message should reference R+16: {msg}"
        );
        assert!(
            msg.contains("constitution"),
            "evaluation-blocked message should mention constitution: {msg}"
        );
        assert!(
            msg.contains("Mock all external services"),
            "evaluation-blocked message should advise mocking: {msg}"
        );
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
                detected_hosts: None,
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
                detected_hosts: None,
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
                detected_hosts: None,
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
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
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

#[cfg(test)]
mod approval_message_tests {
    use super::{sandbox_approval_operator_reason, sandbox_approval_summary_line};
    use crate::runtime::remote_access::DetectedPattern;

    #[test]
    fn sandbox_approval_summary_uses_intent_and_command() {
        let summary = sandbox_approval_summary_line(
            "coder.default",
            "python3 /tmp/test_main.py",
            Some("Run mocked unit tests for weather query"),
        );
        assert!(summary.contains("Run mocked unit tests for weather query"));
        assert!(summary.contains("`python3 /tmp/test_main.py`"));
        assert!(summary.contains("Sandbox exec (coder.default):"));
    }

    #[test]
    fn sandbox_approval_reason_lists_cues() {
        let patterns = vec![DetectedPattern {
            category: "import".to_string(),
            pattern: "import requests".to_string(),
            line_number: Some(67),
            reason: "HTTP client library".to_string(),
        }];
        let reason = sandbox_approval_operator_reason(
            "python3 /tmp/test_main.py",
            Some("Validate output formatting against API response"),
            "Detected 1 remote access pattern(s) in categories: import",
            " → signals: import:import requests",
            &patterns,
        );
        assert!(reason.contains("What will run:"));
        assert!(reason.contains("Agent-stated purpose:"));
        assert!(reason.contains("Why approval is required:"));
        assert!(reason.contains("Static analysis cues:"));
        assert!(reason.contains("[line 67] [import] `import requests`"));
    }
}
