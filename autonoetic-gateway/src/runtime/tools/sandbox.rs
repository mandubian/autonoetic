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
use autonoetic_types::agent::{AgentManifest, RemoteAccessApprovalMode};
use autonoetic_types::background::{
    ApprovalDecision, ApprovalRequest, LayerMountScopeInfo, ScheduledAction,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::layer::LayerApprovalScope;
use autonoetic_types::runtime_lock::LockedLayerMount;
use autonoetic_types::tool_error::tagged;
use secrecy::ExposeSecret;
use std::path::Path;

/// The bubblewrap driver builds its gateway-secret mask from this path, so an
/// absent gateway dir would mean an unmasked sandbox. Fail closed rather than
/// deriving one from the agent dir — that derivation is #1145.
fn require_gateway_dir(gateway_dir: Option<&Path>) -> anyhow::Result<&Path> {
    gateway_dir.ok_or_else(|| {
        anyhow::anyhow!(
            "gateway_dir is required to spawn a sandbox: the secret mask is built from it"
        )
    })
}

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SandboxExecTool));
}

pub struct SandboxExecTool;

/// Compute the tool-level `ok` flag for a sandbox execution.
///
/// `ok` is true when the sandbox itself ran the command to completion with an
/// exit code in the normal range (0–127). A non-zero exit in that range is a
/// DOMAIN result (e.g. a failed unit-test suite), not a tool malfunction, and
/// must not feed the LoopGuard failure budget or trajectory divergence. Exit
/// codes >= 128 are signal-derived (e.g. SIGKILL/OOM 137, SIGTERM 143,
/// SIGSYS/seccomp 159) and indicate a genuine sandbox-level fault, so they
/// stay `ok: false`. No exit code at all means the process was killed by a
/// signal without a normal exit.
pub fn compute_sandbox_exec_ok(exit_code: Option<i32>) -> bool {
    matches!(exit_code, Some(code) if (0..128).contains(&code))
}

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
    node_paths: &mut Vec<String>,
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
            source: layer_temp_base.clone(),
            dest: layer.mount_path.clone(),
            readonly: true,
        });

        // Discover Python site-packages inside the layer.
        let mut found_site_packages = false;
        if let Ok(lib_entries) = std::fs::read_dir(layer_temp_base.join("lib")) {
            for entry in lib_entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("python3.") {
                    let site = std::path::Path::new(&layer.mount_path)
                        .join("lib")
                        .join(name_str.as_ref())
                        .join("site-packages");
                    if site.starts_with("/") {
                        python_paths.push(site.to_string_lossy().to_string());
                        found_site_packages = true;
                    }
                }
            }
        }
        python_paths.push(layer.mount_path.clone());

        // Discover Node.js node_modules directories inside the layer.
        let node_modules_temp = layer_temp_base.join("node_modules");
        if node_modules_temp.is_dir() {
            let node_mount = std::path::Path::new(&layer.mount_path).join("node_modules");
            if node_mount.has_root() {
                node_paths.push(node_mount.to_string_lossy().to_string());
            }
        }
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

/// Extract a single named file from an artifact.
fn extract_artifact_file_source(
    gw_dir: &Path,
    artifact_id: &str,
    filename: &str,
) -> Option<String> {
    let store = crate::artifact_store::ArtifactStore::new(gw_dir).ok()?;
    let bundle = store.inspect(artifact_id).ok()?;
    let content_store = crate::runtime::content_store::ContentStore::new(gw_dir).ok()?;
    for file in &bundle.files {
        if file.name.ends_with(&format!("/{}", filename)) || file.name == filename {
            if let Ok(content) = content_store.read(&file.handle) {
                return String::from_utf8(content).ok();
            }
        }
    }
    None
}

/// If the command runs a test runner against a specific test file, return
/// the test filename. This is used to scope artifact-level remote-access
/// analysis to only the test file, avoiding false positives from application
/// code that imports network libraries but is never called during tests.
fn extract_test_target_from_command(command: &str) -> Option<String> {
    let trimmed = command.trim();
    let after_prefix = trimmed
        .split("&&")
        .last()
        .map(|s| s.trim())
        .unwrap_or(trimmed);
    for python_cmd in &["python3", "python", "python3.11", "python3.12"] {
        if !after_prefix.starts_with(python_cmd)
            && !after_prefix.starts_with(&format!("{} ", python_cmd))
        {
            continue;
        }
        let after_python = after_prefix[python_cmd.len()..].trim();
        if let Some(rest) = after_python.strip_prefix("-m unittest") {
            let module = rest.trim().split_whitespace().next()?;
            let module_name = module.split('.').next()?;
            return Some(format!("{}.py", module_name));
        }
        if !after_python.starts_with('-') {
            let first_arg = after_python.split_whitespace().next()?;
            let filename = std::path::Path::new(first_arg)
                .file_name()?
                .to_string_lossy()
                .to_string();
            if (filename.starts_with("test_") || filename.ends_with("_test.py"))
                && filename.ends_with(".py")
            {
                return Some(filename);
            }
        }
    }
    None
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

    let analysis = crate::runtime::remote_access::default_remote_access_detector()
        .analyze_code(&code);
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
pub struct EscapeAttempt {
    pub indicator: String,
    pub detail: String,
}

pub fn detect_sandbox_escape_indicators(
    stderr: &str,
    exit_code: Option<i32>,
) -> Vec<EscapeAttempt> {
    let mut attempts = Vec::new();

    let combined = stderr.to_lowercase();
    let is_sigsys = exit_code == Some(159);

    if is_sigsys {
        attempts.push(EscapeAttempt {
            indicator: "SIGSYS".to_string(),
            detail: "Process killed by SIGSYS (seccomp violation or bad syscall)".to_string(),
        });
    }

    if combined.contains("bad system call") {
        if !is_sigsys {
            attempts.push(EscapeAttempt {
                indicator: "SIGSYS".to_string(),
                detail: "stderr contains 'Bad system call' (seccomp/deny signal)".to_string(),
            });
        }
    }

    let seccomp_patterns: &[(&str, &str)] = &[
        ("seccomp", "seccomp violation"),
        ("operation not permitted", "EPERM from privileged syscall"),
        ("permission denied", "EACCES from security policy"),
    ];
    for (pattern, label) in seccomp_patterns {
        if combined.contains(pattern) {
            attempts.push(EscapeAttempt {
                indicator: "SECCOMP_DENY".to_string(),
                detail: format!("stderr contains '{}': {}", pattern, label),
            });
        }
    }

    let escape_patterns: &[(&str, &str)] = &[
        ("mount: ", "mount command output (mount attempt)"),
        ("umount: ", "umount command output (unmount attempt)"),
        ("ptrace", "ptrace reference (debugging/tracing attempt)"),
        (
            "/proc/self/exe",
            "/proc/self/exe access (self-replacement attempt)",
        ),
        ("kexec", "kexec reference (kernel replacement attempt)"),
        ("nsenter", "nsenter reference (namespace escape attempt)"),
    ];
    for (pattern, label) in escape_patterns {
        if combined.contains(pattern) {
            attempts.push(EscapeAttempt {
                indicator: "ESCAPE_SYSCALL".to_string(),
                detail: label.to_string(),
            });
        }
    }

    attempts
}

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
        "This agent declares NetworkAccess, but the capability is a ceiling — each exec \
         still needs its own grant, and this run had none, so the network namespace stayed \
         unshared. Usually the target was invisible to static analysis (a host built at \
         runtime, read from the environment, or reached through a dynamic import), so no \
         approval was ever requested. Make the target visible — use a literal URL/host in \
         the code and list it in metadata.autonoetic.remote_access.targets — then retry so \
         the operator can approve it."
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
        obj.insert("command_succeeded".to_string(), serde_json::json!(false));
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

/// Connectivity-only fingerprint table for script-mode failure classification.
///
/// Unlike [`detect_network_errors_in_output`] — which also flags broad
/// HTTP-level library exceptions like `requests.exceptions.HTTPError: 404`
/// (right for the `sandbox_exec` path, which knows the run was isolated) — this
/// list matches only errors that prove the outbound *connection* itself failed
/// (no route, refused, timed out, DNS). A script-mode run may have had the
/// network namespace shared, so an HTTP-level error is evidence of nothing and
/// must not be annotated as `network_isolated`.
fn detect_connectivity_errors_in_output(output: &str) -> Vec<String> {
    const PATTERNS: &[(&str, &str)] = &[
        ("Network is unreachable", "Network unreachable"),
        ("OSError: [Errno 101]", "Network unreachable (errno 101)"),
        ("Connection refused", "Connection refused"),
        ("OSError: [Errno 111]", "Connection refused (errno 111)"),
        ("ConnectionRefusedError:", "ConnectionRefusedError"),
        ("socket.gaierror", "socket.gaierror"),
        ("Name or service not known", "DNS resolution failed"),
        ("Could not resolve host", "DNS resolution failed (curl)"),
        ("ConnectionError:", "ConnectionError"),
        ("ConnectTimeoutError", "ConnectTimeoutError"),
        ("NewConnectionError", "NewConnectionError"),
        ("MaxRetryError", "MaxRetryError"),
        ("httpx.ConnectError", "httpx connection error"),
        ("httpx.ConnectTimeout", "httpx timeout"),
        ("aiohttp.ClientConnectorError", "aiohttp connector error"),
    ];

    let mut found = Vec::new();
    for (pattern, label) in PATTERNS {
        if output.contains(pattern) {
            found.push(label.to_string());
        }
    }
    found
}

/// § 3.6 sibling of [`apply_network_isolation_failure_to_result`] for
/// script-mode spawns (`execute_script_in_sandbox`): a script run returns a
/// plain error string instead of a tool-result JSON body, so the network
/// diagnosis is folded into the error text. Returns `None` when the output
/// carries no connectivity-failure fingerprint.
pub(crate) fn classify_script_network_failure(
    stdout: &str,
    stderr: &str,
    has_network_cap: bool,
    evaluation_blocked: bool,
) -> Option<String> {
    let network_errors = detect_connectivity_errors_in_output(&format!("{stdout}\n{stderr}"));
    if network_errors.is_empty() {
        return None;
    }
    let reason = if evaluation_blocked {
        "promotion/evaluation runs are network-isolated (constitution rule R+16): the sandbox ran \
         with the network namespace unshared on purpose. Mock all external services so the script \
         runs offline."
    } else if has_network_cap {
        "this agent declares NetworkAccess, so the sandbox shared the host network namespace and \
         the script's outbound call failed at the connection layer. With the namespace shared this \
         is most often an environmental egress problem — no route to the target (e.g. proxy-only \
         egress or a firewall) — not a code bug, though 'connection refused' can also mean the \
         target is reachable and the service or port is unavailable. Operator approval cannot create \
         connectivity; grant real egress (route/tunnel/firewall), or run the script from a host \
         with outbound access, then retry."
    } else {
        "this agent does not declare NetworkAccess, so the sandbox ran with outbound network \
         blocked. Add a scoped NetworkAccess capability to the manifest (with the target listed in \
         metadata.autonoetic.remote_access.targets), or change the script to reach the target \
         through an allowed path, then retry."
    };
    Some(format!(
        "network_isolated: the script failed on an outbound network call ({}). {}",
        network_errors.join(", "),
        reason
    ))
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

/// Shared approval-continuation doctrine (#466), contributed by both
/// `sandbox_exec` and `artifact_exec` (same `id`, deduped at compose). Centralized
/// from coder/sealed_evaluator SKILL.md.
pub(crate) fn exec_approval_continuation_block() -> crate::runtime::guidance::GuidanceBlock {
    use crate::runtime::guidance::{GuidanceBlock, GuidanceCondition};
    GuidanceBlock {
        id: "exec.approval_continuation",
        // Fires for exec-capable agents, but NOT promotion-gate agents: under
        // P-3.10 their sandbox has network permanently denied, so "seek approval
        // and retry" is wrong for them (artifact_exec returns
        // `promotion_gate_network_denied`, not `approval_required`). Roles here
        // mirror `promotion::manifest_may_record_promotion_verdicts` — keep in sync.
        when: GuidanceCondition::All(vec![
            GuidanceCondition::Any(vec![
                GuidanceCondition::ToolPresent("sandbox_exec"),
                GuidanceCondition::ToolPresent("artifact_exec"),
            ]),
            GuidanceCondition::Not(Box::new(GuidanceCondition::Any(vec![
                GuidanceCondition::Role("sealed_evaluator"),
                GuidanceCondition::Role("auditor"),
                GuidanceCondition::Role("static_evaluator"),
                GuidanceCondition::Role("unit_test_runner"),
            ]))),
        ]),
        priority: 11,
        prose: "**Approval continuation.** If `sandbox_exec`/`artifact_exec` returns \
`approval_required: true` with a `request_id`, do not invent or guess ids — return that exact \
`request_id` to your caller and stop. After the operator approves and you resume, retry the \
**exact same** command with the `approval_ref` input set to the approved `request_id`, then continue \
your task; do NOT `EndTurn` immediately after resumption.\n\n\
**Manifest declaration gap.** If the result has `error_type: \"undeclared_remote_pattern\"` or \
`\"missing_remote_access_declaration\"`, the fix is the manifest, not the code: do NOT rewrite code \
to remove the network access, and do NOT retry the same code — you cannot edit your own installed \
SKILL.md. Stop and report the `error_type` + `undeclared_patterns` to your caller; the caller must \
have the builder flow (agent-factory / specialized_builder) re-issue the install intent or a \
revision whose `remote_access` declaration covers the listed patterns. For \
`undeclared_remote_pattern`, the listed patterns are always hosts/IPs (fix: add them to \
`remote_access.targets`) or shell/package-manager commands (fix: `shell_commands` / \
`package_manager_commands`). For `missing_remote_access_declaration`, the payload may also \
include import/call signals as context — those are advisory only; the actionable fix is still \
to declare `targets` (and command surfaces when used). Imports and call patterns never cause \
`undeclared_remote_pattern`, so there is no need to enumerate them."
            .to_string(),
    }
}

/// Build the rejection payload for manifest-declaration-gap errors
/// (`missing_remote_access_declaration`, `undeclared_remote_pattern`).
///
/// These rejections look like code diagnostics (per-line `undeclared_patterns`)
/// but the running agent cannot fix them: its installed SKILL.md is an
/// immutable revision, editable only through the builder flow
/// (agent-factory / specialized_builder via AgentRevision or a re-issued
/// install intent). The old `repair_hint` offered "declare ... or change the
/// code" — the first option was unactionable, so agents always picked code
/// edits and looped for hundreds of turns (see
/// docs/postmortems/session-b6d27af2-weather-agent.md). The payload now says
/// `fix_target: manifest` explicitly and directs the agent to stop local
/// repair and report to its caller.
fn manifest_declaration_gap_response(
    error_type: &str,
    message: String,
    undeclared: Vec<serde_json::Value>,
) -> String {
    serde_json::json!({
        "ok": false,
        "error_type": error_type,
        "error_class": "manifest_declaration_gap",
        "fix_target": "manifest",
        "message": message,
        "repair_hint": "This is a manifest declaration gap, not a code bug — do NOT edit code to remove the network access, and do NOT retry the same code. You cannot edit your own installed SKILL.md. Stop local repair and report error_type + undeclared_patterns to your caller; the caller must have agent-factory.default / specialized_builder.default re-issue the install intent or a revision whose remote_access declaration (imports/function_calls/shell_commands/package_manager_commands/targets) covers the listed patterns.",
        "available_actions": [
            {
                "action": "report_to_caller",
                "reason": "manifest_declaration_gap",
                "detail": "Surface error_type + undeclared_patterns to whoever spawned you. Do not retry the same code or rewrite it to avoid the network access."
            },
            {
                "action": "delegate",
                "delegate": "agent-factory.default",
                "reason": "manifest_declaration_gap",
                "detail": "Only the builder flow can change an installed agent's remote_access declaration (install intent / AgentRevision)."
            }
        ],
        "undeclared_patterns": undeclared,
    })
    .to_string()
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

    fn guidance(&self) -> Vec<crate::runtime::guidance::GuidanceBlock> {
        use crate::runtime::guidance::{GuidanceBlock, GuidanceCondition};
        // Centralized from coder/debugger/executor SKILL.md (#466). The gateway
        // enforces this for every `sandbox_exec` call, so it belongs with the
        // tool rather than copy-pasted per role.
        vec![
            GuidanceBlock {
                id: "sandbox.forbidden_commands",
                when: GuidanceCondition::ToolPresent("sandbox_exec"),
                priority: 10,
                prose: "**Forbidden shell commands** (blocked by gateway security policy): destructive \
file/disk operations (`rm`, `rmdir`, `unlink`, `find … -delete`, `mkfs`, `shred`, `wipefs`, \
`dd if=`/`dd of=/dev/…`, redirects to `/dev/…`); privilege escalation (`sudo`, `su`, `doas`, \
`setuid`/`setgid`, `chmod +s`, `chown root`); and environment/process-secret disclosure (`env`, \
`printenv`, `declare -x`, and reads of `/proc/self/environ`, `/proc/1/environ`, `/etc/environment`)."
                    .to_string(),
            },
            exec_approval_continuation_block(),
        ]
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Run any shell command in a secure sandbox. Execute python3 scripts, node.js, bash commands, install packages (pip install, npm install), run tests, compile code, use git, grep, awk, sed, curl (internal network), and more. The sandbox isolates your execution with a read-only host filesystem — only your agent directory is writable. Network access (outbound HTTP, sockets) triggers operator approval; retry with approval_ref after approval. Dangerous commands (sudo, rm -rf, dd, mkfs) are blocked by security policy. NOTE: large stdout/stderr is truncated to a character budget (~4000 chars) before reaching you; the JSON structure and exit_code are always preserved. Pipe verbose output to a file and use resolve to page through it if you need the full output.".to_string(),
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
                        "description": "Optional artifact identifier. Prefer artifact_ref (ar.*) instead."
                    },
                    "artifact_ref": {
                        "type": "string",
                        "description": "Optional artifact ref (e.g., ar.* from artifact_build). Resolved server-side. Preferred over artifact_id."
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
                    "input": {
                        "description": "Payload delivered to the script via `autonoetic_sdk.load_input()`. Pass any JSON value (number, string, object, array) — the gateway serializes it to the AUTONOETIC_INPUT env var the SDK reads. Use this for scripts that call load_input() inside a sandbox_exec command; for scripts that read argv, pass arguments in `command` directly."
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
        turn_id: Option<&str>,
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

        let artifact_ref_trimmed = args
            .artifact_ref
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        if let Some(aid) = args.artifact_id.as_deref() {
            let aid = aid.trim();
            if aid.starts_with("ar.") {
                return Err(tagged::Tagged::validation(anyhow::anyhow!(
                    "sandbox_exec: '{}' looks like an artifact ref (ar.*). \
                     Use the \"artifact_ref\" field, not \"artifact_id\". \
                     \"artifact_id\" must be the canonical on-disk bundle id (art_*...).",
                    aid
                ))
                .into());
            }
        }

        let resolved_from_ref: Option<String> = if let Some(ref aref) = artifact_ref_trimmed {
            let Some(store) = gateway_store.as_ref() else {
                return Err(tagged::Tagged::validation(anyhow::anyhow!(
                    "sandbox_exec: artifact_ref requires GatewayStore to be configured"
                ))
                .into());
            };
            let Some(sid) = session_id else {
                return Err(tagged::Tagged::validation(anyhow::anyhow!(
                    "sandbox_exec: artifact_ref requires an active session (session_id)"
                ))
                .into());
            };
            let Some(gw_d) = gateway_dir else {
                return Err(tagged::Tagged::validation(anyhow::anyhow!(
                    "sandbox_exec: artifact_ref requires gateway_dir"
                ))
                .into());
            };
            match crate::runtime::tools::artifact::resolve_artifact_ref_or_canonical(
                aref, sid, store, gw_d,
            ) {
                Ok(r) => Some(r.artifact_id),
                Err(e) => return Err(tagged::Tagged::validation(e).into()),
            }
        } else {
            None
        };

        let explicit_mount_artifact_id: Option<String> = match (
            &args.artifact_id,
            &resolved_from_ref,
        ) {
            (Some(id), Some(rid)) => {
                let id = id.trim();
                if id == rid.as_str() {
                    Some(id.to_string())
                } else {
                    return Err(tagged::Tagged::validation(anyhow::anyhow!(
                            "sandbox_exec: artifact_id '{}' does not match artifact_ref '{}' (resolved to '{}')",
                            id,
                            artifact_ref_trimmed.as_deref().unwrap_or_default(),
                            rid
                        ))
                        .into());
                }
            }
            (Some(id), None) => Some(id.trim().to_string()),
            (None, Some(rid)) => Some(rid.clone()),
            (None, None) => None,
        };
        let explicit_mount_artifact_id = explicit_mount_artifact_id.filter(|s| !s.is_empty());

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

        if let Some(artifact_id) = explicit_mount_artifact_id.as_deref() {
            if artifact_id.starts_with("impl_") {
                return Ok(crate::runtime::tools::implicit_artifact_id_error(
                    self.name(),
                    artifact_id,
                )
                .to_string());
            }
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
        let code_to_analyze = if let Some(ref aid) = explicit_mount_artifact_id {
            if let Some(gw_dir) = gateway_dir {
                // When the command is a test runner, analyze only the test file
                // instead of the full artifact. This avoids false positives from
                // application code that imports network libraries but is never
                // called during tests (mocked with unittest.mock.patch).
                let test_filename = extract_test_target_from_command(&effective_command);
                if let Some(ref test_file) = test_filename {
                    if let Some(test_code) = extract_artifact_file_source(gw_dir, aid, test_file) {
                        let analysis = crate::runtime::remote_access::default_remote_access_detector()
                            .analyze_code(&test_code);
                        artifact_analysis_override = Some(analysis);
                        test_code
                    } else {
                        extract_code_for_analysis(
                            &effective_command,
                            agent_dir,
                            gateway_dir,
                            session_id,
                        )
                    }
                } else {
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
            let early_analysis = crate::runtime::remote_access::default_remote_access_detector()
                .detect_network_commands(&code_to_analyze);
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
        let declared_remote_access =
            crate::runtime::network_policy::load_manifest_remote_access_declaration(agent_dir);
        let remote_analysis = if let Some(artifact_analysis) = artifact_analysis_override {
            artifact_analysis
        } else {
            crate::runtime::remote_access::default_remote_access_detector()
                .analyze_command_and_dependencies_with_declaration(
                    &code_to_analyze,
                    dep_packages.as_deref(),
                    declared_remote_access.as_ref(),
                )
        };

        // Per-host `sandbox_exec` probe budget (#853). Refuse a probe against a
        // host this session has already exhausted its budget on — the
        // mechanical backstop for the "stop retrying one dead host" guidance
        // the rotating-poll guard misses (each retry ships a different script,
        // so the `(tool, args)` fingerprint never repeats). Checked BEFORE
        // approval/execution so the wasted probe never runs.
        if let (Some(store), Some(sid)) = (gateway_store.as_ref(), session_id) {
            if store.host_probe_budget.cap() > 0 {
                let probe_hosts = crate::runtime::approved_exec_cache::normalize_targets(
                    &remote_analysis.detected_patterns,
                );
                for host in &probe_hosts {
                    if let Some(strikes) = store.host_probe_budget.exhausted(sid, host) {
                        return Ok(
                            crate::runtime::host_probe_budget::host_budget_exhausted_response(
                                host,
                                strikes,
                                store.host_probe_budget.cap(),
                            ),
                        );
                    }
                }
            }
        }

        if declared_remote_access.is_none() && remote_analysis.requires_approval {
            let undeclared: Vec<serde_json::Value> = remote_analysis
                .detected_patterns
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "category": p.category,
                        "pattern": p.pattern,
                        "line_number": p.line_number,
                        "reason": p.reason,
                    })
                })
                .collect();
            return Ok(manifest_declaration_gap_response(
                "missing_remote_access_declaration",
                format!(
                    "Agent `{}` triggered remote-access signals but has no metadata.autonoetic.remote_access declaration in its installed SKILL.md.",
                    manifest.agent.id
                ),
                undeclared,
            ));
        }

        let undeclared_remote_patterns =
            crate::runtime::remote_access::undeclared_patterns_against_manifest(
                &remote_analysis.detected_patterns,
                declared_remote_access.as_ref(),
            );
        if !undeclared_remote_patterns.is_empty() {
            let undeclared: Vec<serde_json::Value> = undeclared_remote_patterns
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "category": p.category,
                        "pattern": p.pattern,
                        "line_number": p.line_number,
                        "reason": p.reason,
                    })
                })
                .collect();
            return Ok(manifest_declaration_gap_response(
                "undeclared_remote_pattern",
                "The code requires network access not covered by the agent's installed remote_access declaration (see undeclared_patterns)."
                    .to_string(),
                undeclared,
            ));
        }

        // Declaration drift (#1023): import/function-call signals the declaration
        // does not name. Advisory by design — `targets` is the authoritative
        // contract, and mirroring analyzer pattern strings is a contract agents
        // cannot keep. Logged rather than enforced, so manifest hygiene stays
        // observable without turning a stale hint into a refusal. The operator also
        // sees these patterns in the approval prompt itself.
        let advisory_drift = crate::runtime::remote_access::advisory_undeclared_patterns(
            &remote_analysis.detected_patterns,
            declared_remote_access.as_ref(),
        );
        if !advisory_drift.is_empty() {
            tracing::info!(
                target: "sandbox_exec",
                agent_id = %manifest.agent.id,
                undeclared_advisory = ?advisory_drift
                    .iter()
                    .map(|p| format!("{}:{}", p.category, p.pattern))
                    .collect::<Vec<_>>(),
                "Remote-access signals outside the agent's declared import/function lists (advisory — targets are the gating contract)"
            );
        }

        let agent_has_network_access = manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { .. }));

        // #1106: any + preapproved + non-wildcard capability is a silent
        // any-host auto-approval (the preapproved branch below checks mere
        // capability presence). Fail shut as a manifest inconsistency before
        // any of the preapproved branches can fire.
        if let Some(decl) = declared_remote_access.as_ref() {
            if let Err(violation) =
                crate::runtime::network_policy::validate_any_preapproval_shape(manifest, decl)
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

        let remote_approval_mode = declared_remote_access
            .as_ref()
            .map(|d| d.approval_mode)
            .unwrap_or(RemoteAccessApprovalMode::Required);

        // Boundary fail-closed: unknown taint ⇒ refuse (never treat as unrestricted).
        let session_taint = match crate::runtime::egress_labeler::require_boundary_session_taint(
            run_context,
            gateway_store.as_deref(),
            session_id,
        ) {
            Ok(t) => t,
            Err(e) => {
                if let Some(store) = &gateway_store {
                    crate::runtime::egress_labeler::emit_surface_boundary_refused(
                        store,
                        session_id.unwrap_or(""),
                        &manifest.agent.id,
                        turn_id,
                        "sandbox",
                        &autonoetic_types::egress::EgressLabel::empty(),
                        &[],
                        &format!("session egress taint unresolved: {e}"),
                    );
                }
                return Ok(serde_json::json!({
                    "ok": false,
                    "error_type": "egress_boundary_refused",
                    "surface": "sandbox",
                    "tool": "sandbox_exec",
                    "message": format!(
                        "sandbox_exec refused: cannot establish session egress taint ({e})"
                    ),
                    "repair_hint": "Ensure the tool runs with a session id and GatewayStore so taint can be confirmed.",
                })
                .to_string());
            }
        };
        let network_sink_excluded =
            !session_taint.allows(autonoetic_types::egress::Sink::Network);
        let root_for_declass = run_context
            .map(|c| c.root_session_id.as_str())
            .or_else(|| session_id.and_then(|s| s.split('/').next()))
            .unwrap_or("");
        // Host-scoped declassification (#909 follow-up): `share_net` may be
        // enabled by a session-wide grant, or by host-scoped grants covering
        // **every** concrete host static analysis resolved. Commands with no
        // concrete hosts stay gated (the `Unresolved` hard-refuse below fires
        // first for those).
        let declass_hosts = normalize_targets(&remote_analysis.detected_patterns);
        let network_declassified = if network_sink_excluded {
            gateway_store
                .as_ref()
                .map(|store| {
                    crate::runtime::egress_labeler::session_network_declassified_for_hosts(
                        store.as_ref(),
                        session_id.unwrap_or(""),
                        root_for_declass,
                        &declass_hosts,
                    )
                })
                .unwrap_or(false)
        } else {
            true
        };
        let manifest_grants_share_net = manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::NetworkAccess { hosts } if !hosts.is_empty())
        });
        let requires_network_gate = remote_analysis.requires_approval
            || (network_sink_excluded && manifest_grants_share_net);

        if matches!(remote_approval_mode, RemoteAccessApprovalMode::Preapproved)
            && !agent_has_network_access
            && remote_analysis.requires_approval
        {
            return Ok(serde_json::json!({
                "ok": false,
                "error_type": "remote_preapproval_requires_network_capability",
                "message": format!(
                    "Agent `{}` declared remote_access.approval_mode=preapproved but does not have NetworkAccess capability.",
                    manifest.agent.id
                ),
                "repair_hint": "Either add NetworkAccess capability or set metadata.autonoetic.remote_access.approval_mode to required.",
            })
            .to_string());
        }

        if matches!(remote_approval_mode, RemoteAccessApprovalMode::Preapproved)
            && agent_has_network_access
            && remote_analysis.requires_approval
            && !approval_validated_for_command
            && !network_sink_excluded
        {
            tracing::info!(
                target: "sandbox_exec",
                agent_id = %manifest.agent.id,
                patterns = ?remote_analysis.detected_patterns,
                "remote_access.approval_mode is preapproved and NetworkAccess is present - auto-approving remote access patterns"
            );
            approval_validated_for_command = true;
        }

        let mut safe_inspection_bypass = false;
        if requires_network_gate
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
            will_require_approval = requires_network_gate && !approval_validated_for_command,
            pattern_count = remote_analysis.detected_patterns.len(),
            dep_package_count = dep_packages.as_ref().map(|p| p.len()).unwrap_or(0),
            summary = %remote_analysis.summary,
            "Remote access scan for sandbox.exec (imports, URLs, IPs, network commands, dependencies). If will_require_approval=true, execution stops until operator approves and caller retries with approval_ref."
        );
        if requires_network_gate && !approval_validated_for_command {
            tracing::warn!(
                target: "sandbox",
                patterns = ?remote_analysis.detected_patterns,
                network_sink_excluded = network_sink_excluded,
                "Code or session egress taint requires network gate — operator approval required"
            );

            let detected_patterns = remote_analysis.detected_patterns.clone();
            let normalized_targets = normalize_targets(&detected_patterns);

            let coverage = crate::runtime::remote_access::classify_network_coverage(
                &detected_patterns,
                normalized_targets.clone(),
            );

            if matches!(
                coverage,
                crate::runtime::remote_access::NetworkCoverage::Unresolved
            ) && network_sink_excluded
            {
                if let Some(store) = &gateway_store {
                    crate::runtime::egress_labeler::emit_surface_boundary_refused(
                        store,
                        session_id.unwrap_or(""),
                        &manifest.agent.id,
                        turn_id,
                        "sandbox",
                        &session_taint,
                        &[],
                        "session egress taint excludes Network and remote targets are Unresolved (RFC §7)",
                    );
                }
                return Ok(serde_json::json!({
                    "ok": false,
                    "error_type": "egress_boundary_refused",
                    "surface": "sandbox",
                    "tool": "sandbox_exec",
                    "message": "sandbox_exec refused: session egress taint excludes Network and network targets could not be resolved to concrete hosts",
                    "repair_hint": "Operator-declassify Sink::Network for this session (egress.declassified), or use a command with concrete network targets.",
                })
                .to_string());
            }

            // Pre-check: exec cache for concrete targets (sets pre_validated for GateService bypass)
            let mut pre_validated = false;
            let mut cache_backfill: Option<
                crate::runtime::approved_exec_cache::ApprovedExecCacheBackfill,
            > = None;
            if let crate::runtime::remote_access::NetworkCoverage::Concrete { targets } = &coverage
            {
                if let Some(gw_dir) = gateway_dir {
                    let fingerprint = compute_fingerprint(
                        &manifest.agent.id,
                        targets,
                        &code_to_analyze,
                        explicit_mount_artifact_id.as_deref(),
                        &manifest.capabilities,
                    );
                    if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                        if let Some(entry) = cache.find(
                            &fingerprint,
                            crate::runtime::approved_exec_cache::cache_ttl_secs(config),
                        ) {
                            tracing::info!(
                                target: "sandbox_exec",
                                fingerprint = %fingerprint,
                                previously_approved_by = %entry.approved_by,
                                previously_approved_at = %entry.approved_at,
                                "Cache hit: skipping approval for previously approved sandbox exec"
                            );
                            let _ = cache.update_last_used(&fingerprint);
                            pre_validated = true;
                        }
                    }
                    if !pre_validated {
                        cache_backfill = Some(
                            crate::runtime::approved_exec_cache::ApprovedExecCacheBackfill {
                                gateway_dir: gw_dir.to_path_buf(),
                                fingerprint,
                                agent_id: manifest.agent.id.clone(),
                                remote_targets: normalized_targets.clone(),
                                code_content: code_to_analyze.clone(),
                                approval_request_id: String::new(),
                                ttl_secs:
                                    crate::runtime::approved_exec_cache::cache_ttl_secs(config),
                            },
                        );
                    }
                }
            }

            if pre_validated {
                // Exec cache hit — fast path, no GateService or store needed.
                approval_validated_for_command = true;
            } else if let Some(cfg) = config {
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
                    intent: args.intent.clone(),
                };
                let reason = sandbox_approval_operator_reason(
                    &effective_command,
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
                                targets: normalized_targets.clone(),
                                match_strategy: crate::runtime::human_gate::MatchStrategy::SubstituteCommand,
                            },
                            manifest,
                            session_id,
                            run_context,
                            config: Some(cfg),
                            context: crate::runtime::human_gate::DecisionContext::tier2(
                                format!("sandbox.exec: {}", effective_command),
                                if normalized_targets.is_empty() {
                                    "sandbox code execution requires operator approval".to_string()
                                } else {
                                    format!(
                                        "sandbox code execution reaching host(s) [{}] not covered by an approved network grant",
                                        normalized_targets.join(", ")
                                    )
                                },
                                if normalized_targets.is_empty() {
                                    "runs agent-supplied code in the sandbox; effects depend on the command".to_string()
                                } else {
                                    format!(
                                        "runs agent-supplied code in the sandbox with network access to [{}]; effects depend on the command",
                                        normalized_targets.join(", ")
                                    )
                                },
                                "Approve if the command and any network targets are expected for this agent's task; reject or escalate if the command or hosts are unexpected",
                            )
                            .with_analysis(reason.clone()),
                            summary: summary.clone(),
                            approval_ref: None,
                            pre_validated,
                            cache_backfill,
                            request_id: None,
                            turn_id: None,
                        },
                    )?;
                    match gate_result {
                        crate::runtime::human_gate::GateResult::Cleared { .. } => {
                            approval_validated_for_command = true;
                        }
                        crate::runtime::human_gate::GateResult::AlreadyPending {
                            gate_id, ..
                        } => {
                            let (cmd, cmd_deps, pending_action) = match store
                                .get_approval(&gate_id)?
                            {
                                Some(pending) => match &pending.action {
                                    ScheduledAction::SandboxExec {
                                        command,
                                        dependencies,
                                        ..
                                    } => (
                                        command.clone(),
                                        dependencies.clone(),
                                        pending.action.clone(),
                                    ),
                                    _ => (effective_command.clone(), None, pending.action.clone()),
                                },
                                None => (effective_command.clone(), None, action.clone()),
                            };
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
                            return serde_json::to_string(&serde_json::json!({
                                "ok": false,
                                "exit_code": null,
                                "stdout": "",
                                "stderr": format!(
                                    "Sandbox approval already pending (request_id: {}). After approval, the persisted command will execute automatically.",
                                    gate_id
                                ),
                                "approval_required": true,
                                "approval_already_pending": true,
                                "suspended": true,
                                "request_id": gate_id,
                                "message": format!(
                                    "Execution suspended. Approval {} is pending. The approved command is already persisted and will be used automatically on resume.",
                                    gate_id
                                ),
                                "approval": approval,
                            }))
                            .map_err(Into::into);
                        }
                        crate::runtime::human_gate::GateResult::Suspended { gate_id, .. } => {
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

                            // Populate code excerpts for operator inspection (Phase 1).
                            if let Some(ref art_id) = explicit_mount_artifact_id {
                                if let Some(gw_dir) = gateway_dir {
                                    let excerpts =
                                        crate::runtime::code_excerpts::build_code_excerpts(
                                            art_id, gw_dir,
                                        );
                                    let _ = store.set_approval_code_excerpts(
                                        &gate_id,
                                        excerpts.as_deref(),
                                        None,
                                    );
                                }
                            }

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
                                "request_id": gate_id,
                                "remote_access_detected": true,
                                "detected_patterns": remote_analysis.detected_patterns,
                                "suspended": true,
                                "message": format!("Execution suspended pending operator approval ({}). The approved command is persisted and will be used automatically on resume.", gate_id),
                                "approval": approval
                            }))
                            .map_err(Into::into);
                        }
                        other => {
                            return Err(anyhow::anyhow!(
                                "Unexpected gate result for sandbox.exec gate: {:?}",
                                other
                            ));
                        }
                    }
                } else {
                    return Err(tagged::Tagged::resource(anyhow::anyhow!(
                        "GatewayStore missing; cannot persist sandbox approval request"
                    ))
                    .into());
                }
            } else {
                // No config — fail closed. Approvals must not be bypassable
                // via misconfiguration.
                return Err(anyhow::anyhow!(
                    "Remote access approval required but GatewayConfig is not available \
                     to enforce the approval gate."
                ));
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
                let effective_artifact_id_for_scope = explicit_mount_artifact_id
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
                            let summary = format!(
                            "Layer mount approval: {} layer(s) with unapproved build-time hosts",
                            scope_issues.len()
                        );
                            let action = ScheduledAction::LayerMount {
                                layers: scope_issues.clone(),
                                command: effective_command.clone(),
                            };
                            let all_unapproved: Vec<String> = scope_issues
                                .iter()
                                .flat_map(|i| i.unapproved_delta.iter().cloned())
                                .collect::<std::collections::BTreeSet<_>>()
                                .into_iter()
                                .collect();
                            let reason = format!(
                                "Layer mount scope check: {} layer(s) were captured with network access to hosts [{}] not yet approved in this session.",
                                scope_issues.len(),
                                all_unapproved.join(", ")
                            );

                            if let Some(store) = &gateway_store {
                                let gate =
                                    crate::runtime::human_gate::GateService::new(store.clone());
                                let gate_result = gate.check(
                                    crate::runtime::human_gate::GateRequest {
                                        kind: crate::runtime::human_gate::GateKind::Approval {
                                            action: action.clone(),
                                            targets: all_unapproved.clone(),
                                            match_strategy: crate::runtime::human_gate::MatchStrategy::HostLevel,
                                        },
                                        manifest,
                                        session_id,
                                        run_context,
                                        config: Some(cfg),
                                        context: crate::runtime::human_gate::DecisionContext::tier2(
                                            format!(
                                                "mount {} layer(s) for sandbox.exec: {}",
                                                scope_issues.len(),
                                                effective_command
                                            ),
                                            format!(
                                                "{} layer(s) were captured with build-time network access to host(s) [{}] not yet approved in this session",
                                                scope_issues.len(),
                                                all_unapproved.join(", ")
                                            ),
                                            format!(
                                                "mounts pre-built layers whose build reached host(s) [{}]; the layer contents become available to the executed command",
                                                all_unapproved.join(", ")
                                            ),
                                            "Approve if these build-time hosts are expected for the layers being mounted; reject or escalate if any host is unexpected",
                                        )
                                        .with_analysis(reason.clone()),
                                        summary: summary.clone(),
                                        approval_ref: None,
                                        pre_validated: false,
                                        cache_backfill: None,
                                request_id: None,
                                turn_id: None,
                                    },
                                )?;
                                match gate_result {
                                    crate::runtime::human_gate::GateResult::Cleared { .. } => {}
                                    crate::runtime::human_gate::GateResult::AlreadyPending {
                                        gate_id,
                                        ..
                                    } => {
                                        return serde_json::to_string(&serde_json::json!({
                                            "ok": false,
                                            "exit_code": null,
                                            "stdout": "",
                                            "stderr": format!(
                                                "Layer mount approval already pending (request_id: {}). Retry with approval_ref after operator approves.",
                                                gate_id
                                            ),
                                            "approval_required": true,
                                            "layer_mount_approval_required": true,
                                            "approval_already_pending": true,
                                            "request_id": gate_id,
                                            "suspended": true,
                                            "message": format!(
                                                "Execution suspended. Layer mount approval {} is pending.",
                                                gate_id
                                            ),
                                            "approval": {
                                                "kind": "layer_mount",
                                                "summary": summary,
                                                "retry_field": "approval_ref",
                                            }
                                        }))
                                        .map_err(Into::into);
                                    }
                                    crate::runtime::human_gate::GateResult::Suspended {
                                        gate_id,
                                        ..
                                    } => {
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


    expires_at: None,
},
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
                                            "request_id": gate_id,
                                            "suspended": true,
                                            "message": format!(
                                                "Execution suspended pending layer mount approval ({}). Retry sandbox.exec with approval_ref after operator approves.",
                                                gate_id
                                            ),
                                            "approval": approval
                                        }))
                                        .map_err(Into::into);
                                    }
                                    other => {
                                        tracing::warn!(
                                            target: "sandbox_exec",
                                            gate_result = ?other,
                                            "Unexpected gate result for layer mount gate"
                                        );
                                    }
                                }
                            } else {
                                return Err(tagged::Tagged::resource(anyhow::anyhow!(
                                    "GatewayStore missing; cannot persist layer mount approval request"
                                ))
                                .into());
                            }
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

        // Resolve effective artifact_id: explicit arg (or artifact_ref) takes priority, then fall
        // back to the artifact_id from the tool run context (set by parent agent.spawn).
        let effective_artifact_id = explicit_mount_artifact_id
            .as_ref()
            .or_else(|| run_context.as_ref().and_then(|c| c.artifact_id.as_ref()));

        let mut layer_python_paths: Vec<String> = Vec::new();
        let mut layer_node_paths: Vec<String> = Vec::new();
        let mut artifact_fixture_root: Option<std::path::PathBuf> = None;
        let session_content_mounts = if let Some(artifact_id) = effective_artifact_id {
            let Some(gw_dir) = gateway_dir else {
                return Err(tagged::Tagged::resource(anyhow::anyhow!(
                    "artifact_id requires gateway directory to be configured"
                ))
                .into());
            };
            let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
            let resolved_files = artifact_store.resolve_files(artifact_id)?;

            let mut mounts = Vec::new();
            let temp_base = std::env::temp_dir()
                .join("autonoetic_artifact")
                .join(artifact_id.replace('/', "_"));
            std::fs::create_dir_all(&temp_base)?;
            artifact_fixture_root = Some(temp_base.clone());

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
                    &mut layer_node_paths,
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
                    &mut layer_node_paths,
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

        let has_evaluation_cap = manifest.capabilities.iter().any(|c| {
            matches!(
                c,
                autonoetic_types::capability::Capability::Evaluation { .. }
            )
        });

        // The per-exec network decision (#1022). `share_net` is granted here, not
        // inherited: the `NetworkAccess` capability is a ceiling ("may this agent
        // ever reach the network"), and only an explicit grant for *this* exec
        // (approval_ref, cleared gate, declared preapproval, approved-exec cache
        // hit) turns the namespace on. Seeding it from the capability let an exec
        // that raised no gate — because static analysis found no signal to ask
        // about — run with `--share-net` and no operator prompt. See
        // `runtime::network_grant` and docs/sandbox-network-grant.md.
        //
        // Widening under a session taint that excludes Network still requires an
        // active declassification grant (`egress.declassified`), not host
        // approval alone. Safe-inspection and the Evaluation capability keep the
        // network off regardless.
        let network_decision = crate::runtime::network_grant::decide_share_net(
            crate::runtime::network_grant::ShareNetInputs {
                capability_allows_network: manifest
                    .capabilities
                    .iter()
                    .any(|c| matches!(c, Capability::NetworkAccess { hosts } if !hosts.is_empty())),
                approval_validated: approval_validated_for_command,
                safe_inspection_bypass,
                network_sink_excluded,
                network_declassified,
                force_network_off: has_evaluation_cap,
            },
        );
        let mut overrides = crate::sandbox::BwrapIsolationOverrides {
            share_net: network_decision.share_net,
            force_network_off: has_evaluation_cap,
        };
        if network_decision.capability_ceiling_unused {
            tracing::info!(
                target: "sandbox_exec",
                agent_id = %manifest.agent.id,
                reason = network_decision.reason.as_str(),
                pattern_count = remote_analysis.detected_patterns.len(),
                "Agent declares NetworkAccess but this exec has no network grant — running with the network namespace unshared"
            );
        }

        let layer_python_path_str = layer_python_paths.join(":");
        let layer_node_path_str = layer_node_paths.join(":");
        let mut extra_env: Vec<(String, String)> = Vec::new();
        if !layer_python_path_str.is_empty() {
            extra_env.push(("PYTHONPATH".to_string(), layer_python_path_str));
        }
        if !layer_node_path_str.is_empty() {
            extra_env.push(("NODE_PATH".to_string(), layer_node_path_str));
        }

        // First-class `input` parameter → AUTONOETIC_INPUT env var. Mirrors
        // artifact_exec: gives scripts that call load_input() a discoverable
        // handle without the agent needing to know the env var name. No
        // conflict check needed here — sandbox_exec has no free-form `env`
        // field for the caller to also set AUTONOETIC_INPUT through.
        if let Some(input) = &args.input {
            extra_env.push((
                crate::runtime::tools::AUTONOETIC_INPUT_ENV.to_string(),
                crate::runtime::tools::serialize_tool_input(input),
            ));
        }

        if let Some(credential_mappings) = &args.credential_env {
            if let (Some(gw_dir), Some(store)) = (gateway_dir, &gateway_store) {
                crate::vault::ensure_default_key(gw_dir)?;
                let vault_path = crate::vault::default_vault_path(gw_dir);
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

        // RFC scope 5.2c-advisory: sealed-network proxy for sandbox_exec.
        // Same wiring as artifact_exec — when the agent's manifest declares
        // sandbox_network = Sealed/Recording, start the proxy so HTTP clients
        // inside the sandbox route through it. Uses the artifact's temp dir
        // as the fixture root when an artifact is mounted; otherwise falls
        // back to the agent dir (no fixtures → all requests get
        // unfixtured_target).
        let sealed_proxy_fixture_root =
            artifact_fixture_root.unwrap_or_else(|| std::path::PathBuf::from(agent_dir_str));
        let sealed_proxy = crate::runtime::sealed_network_proxy::setup_sealed_proxy_for_exec(
            manifest.sandbox_network,
            sealed_proxy_fixture_root,
            &mut extra_env,
            &mut overrides,
            gateway_dir,
            session_id,
            gateway_store.clone(),
            Some(&manifest.agent.id),
        )?;

        // #1002 slices 2-3: declared host mounts. The tier check runs first
        // (a wasm manifest declaring mounts is a manifest bug, loud), then
        // each declaration is resolved against the operator's
        // `sandbox.allowed_mount_roots`. Anything not granted fails the exec
        // with a structured mount_denied envelope naming the missing grant —
        // a request the allowlist doesn't cover is a decision, not a silent
        // drop or a mysterious in-sandbox ENOENT.
        let mut declared_granted_mounts: Vec<crate::sandbox::SandboxMount> = Vec::new();
        if !manifest.runtime.mounts.is_empty() {
            driver.driver()?.check_mount_support(&manifest.runtime.mounts)?;
            let allowed_roots = config
                .map(|c| c.sandbox.allowed_mount_roots.as_slice())
                .unwrap_or(&[]);
            let (granted, denied) =
                crate::sandbox::resolve_declared_mounts(&manifest.runtime.mounts, allowed_roots);
            if !denied.is_empty() {
                let denials: Vec<serde_json::Value> = denied
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "host_path": d.host_path,
                            "canonical_path": d.canonical_path,
                            "reason": d.reason,
                        })
                    })
                    .collect();
                return Ok(serde_json::json!({
                    "ok": false,
                    "error_type": "permission",
                    "message": format!(
                        "sandbox_exec: {} declared runtime.mounts entr{} not covered by \
                         sandbox.allowed_mount_roots — ask the operator to extend the \
                         allowlist (config) or remove the declaration(s).",
                        denials.len(),
                        if denials.len() == 1 { "y is" } else { "ies are" }
                    ),
                    "mount_denied": denials,
                    "enforced_rules": ["P-1.5"],
                })
                .to_string());
            }
            declared_granted_mounts = granted;
        }

        // Merge runtime.lock mounts into session content mounts
        let mut all_mounts = session_content_mounts;
        if !runtime_lock_mounts.is_empty() {
            all_mounts.extend(runtime_lock_mounts);
        }
        // Declared mounts are additive in BOTH modes today (legacy still
        // ro-binds `/`, so an allowed mount is already visible — binding it
        // explicitly makes the grant visible in the mount set and keeps the
        // same manifest working unchanged the day allow_set lands).
        all_mounts.extend(declared_granted_mounts);

        // #1002 slice 1: record what this execution can see, as asserted here
        // (the SDK bridge socket mount is added later, inside spawn, and is
        // intentionally not listed — it is a gateway-internal socket, not host
        // filesystem reach). Bubblewrap ro-binds the whole host `/` today
        // (legacy mode); docker/microvm/wasm do not.
        let mount_set: Vec<String> = {
            const MOUNT_SET_CAP: usize = 64;
            let mut entries: Vec<String> = Vec::with_capacity(2 + all_mounts.len());
            if driver == crate::sandbox::driver::SandboxDriverKind::Bubblewrap {
                entries.push("ro:host_root".to_string());
            }
            entries.push(format!("rw:{agent_dir_str}"));
            for mount in &all_mounts {
                entries.push(format!(
                    "{}:{}",
                    if mount.readonly { "ro" } else { "rw" },
                    mount.source.display()
                ));
            }
            if entries.len() > MOUNT_SET_CAP {
                // Reserve a slot for the marker so the capped list, marker
                // included, never exceeds MOUNT_SET_CAP entries.
                let overflow = entries.len() - (MOUNT_SET_CAP - 1);
                entries.truncate(MOUNT_SET_CAP - 1);
                entries.push(format!("truncated:+{overflow}"));
            }
            entries
        };

        // sandbox.exec is free-form shell on the native tier.
        let exec_kind = crate::exec_request::ExecutionKind::shell(effective_command.clone());
        let runner = if all_mounts.is_empty() {
            SandboxRunner::spawn_with_driver_and_dependencies_and_env(
                driver,
                agent_dir_str,
                require_gateway_dir(gateway_dir)?,
                &exec_kind,
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
                require_gateway_dir(gateway_dir)?,
                &exec_kind,
                dep_plan.as_ref(),
                all_mounts,
                Some(&overrides),
                &extra_env,
                root_session_id,
            )?
        };

        let _sandbox_pid_guard = sandbox_exec_pid_guard(&runner, run_context);
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
        let ok = compute_sandbox_exec_ok(exit_code);

        let mut body = serde_json::json!({
            "ok": ok,
            "command_succeeded": command_succeeded,
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "mount_set": mount_set
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

        // R++8: Sandbox-escape-attempt accounting. Detect escape indicators
        // in stderr/exit code, record per session, and trigger degradation or
        // emergency stop when thresholds are crossed.
        {
            let escape_indicators = detect_sandbox_escape_indicators(&stderr, output.status.code());
            if !escape_indicators.is_empty() {
                let root_sid = root_session_id
                    .as_deref()
                    .or(session_id)
                    .unwrap_or("unknown");
                if let Some(ref store) = gateway_store {
                    let sid = session_id.unwrap_or("unknown");
                    for attempt in &escape_indicators {
                        if let Err(e) = store.record_sandbox_escape_attempt(
                            sid,
                            root_sid,
                            &manifest.agent.id,
                            &attempt.indicator,
                            &attempt.detail,
                            output.status.code(),
                        ) {
                            tracing::warn!(
                                target: "sandbox_exec",
                                error = %e,
                                "Failed to record sandbox escape attempt (R++8)"
                            );
                        }
                    }
                    if let Ok(count) = store.count_sandbox_escape_attempts_for_session(sid) {
                        let (degrade_threshold, emergency_threshold) = config
                            .map(|c| {
                                (
                                    c.escape_attempt_degrade_threshold,
                                    c.escape_attempt_emergency_threshold,
                                )
                            })
                            .unwrap_or((5, 20));
                        if emergency_threshold > 0 && count >= emergency_threshold {
                            tracing::error!(
                                target: "sandbox_exec",
                                session_id = %sid,
                                count = count,
                                threshold = emergency_threshold,
                                "Sandbox escape attempts exceeded emergency threshold (R++8)"
                            );
                            if let Err(e) = store.emit_escape_threshold_event(
                                sid,
                                root_sid,
                                count,
                                emergency_threshold,
                                "emergency",
                            ) {
                                tracing::warn!(
                                    target: "sandbox_exec",
                                    error = %e,
                                    "Failed to emit escape threshold causal event"
                                );
                            }
                        } else if degrade_threshold > 0 && count >= degrade_threshold {
                            tracing::warn!(
                                target: "sandbox_exec",
                                session_id = %sid,
                                count = count,
                                threshold = degrade_threshold,
                                "Sandbox escape attempts exceeded degradation threshold (R++8)"
                            );
                            if let Err(e) = store.emit_escape_threshold_event(
                                sid,
                                root_sid,
                                count,
                                degrade_threshold,
                                "degradation",
                            ) {
                                tracing::warn!(
                                    target: "sandbox_exec",
                                    error = %e,
                                    "Failed to emit escape threshold causal event"
                                );
                            }
                        }
                        body["escape_attempt_count"] = serde_json::json!(count);
                    }
                }
            }
        }

        if let Some(ref capture_paths) = args.capture_paths {
            if !capture_paths.is_empty() {
                if let Some(gw_dir) = gateway_dir {
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
                            let captured = capture_layers_from_paths(
                                &layer_store,
                                capture_paths,
                                agent_dir,
                                capture_approval_scope.as_ref(),
                            );
                            if !captured.is_empty() {
                                body["captured_layers"] = serde_json::Value::Array(captured);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(target: "sandbox", error = %e, "Failed to create layer store for capture");
                        }
                    }
                }
            }
        }

        // Auto-capture: if the agent ran a package install command without
        // capture_paths, infer the target directory and capture it anyway.
        if args.capture_paths.as_ref().map_or(true, |p| p.is_empty()) {
            if let Some(inferred) = infer_capture_paths_from_command(&args.command) {
                if let Some(gw_dir) = gateway_dir {
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
                            let captured = capture_layers_from_paths(
                                &layer_store,
                                &inferred,
                                agent_dir,
                                capture_approval_scope.as_ref(),
                            );
                            if !captured.is_empty() {
                                tracing::info!(
                                    target: "sandbox",
                                    command = %args.command,
                                    inferred_count = inferred.len(),
                                    "Auto-captured dependency layer(s) from install command"
                                );
                                body["captured_layers"] = serde_json::Value::Array(captured);
                                body["auto_captured"] = serde_json::json!(true);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(target: "sandbox", error = %e, "Failed to create layer store for auto-capture");
                        }
                    }
                }
            }
        }

        // Post-exec envelope discovery: propose and auto-lock any newly
        // observed remote hosts so subsequent tool calls are covered by
        // grants without manual operator intervention.
        if let (Some(gs), Some(root)) = (gateway_store.as_ref(), root_session_id.as_deref()) {
            match crate::runtime::session_envelope::propose_discovered_envelope(
                gs,
                root,
                "sandbox_exec",
                None,
                &manifest.agent.id,
            ) {
                // Surface the grant so the agent KNOWS whether these hosts are
                // now covered and need no re-approval — the silent auto-lock was
                // a prime cause of redundant approval loops. Report `locked` from
                // the ACTUAL grant coverage, not optimistically: auto-lock can
                // fail (it logs and leaves the envelope merely proposed), and a
                // false `locked:true` would tell the agent not to seek approval
                // it still needs.
                Ok(Some(result)) if !result.hosts.is_empty() => {
                    let covered =
                        gs.session_grants_cover_targets(root, &manifest.agent.id, &result.hosts);
                    body["network_grant"] = serde_json::json!({
                        "hosts": result.hosts,
                        "locked": covered,
                        "note": if covered {
                            "These hosts are now covered by a session grant — subsequent \
                             calls to them this session are auto-approved; do not re-request \
                             approval for them."
                        } else {
                            "These hosts were proposed but not yet locked — a later call may \
                             still require operator approval."
                        },
                    });
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(
                        target: "session_envelope",
                        error = %e,
                        root_session_id = root,
                        "envelope proposal after sandbox_exec failed"
                    );
                }
            }
        }

        // Per-host probe-budget accounting (#853). Classify this probe against
        // each targeted host — a failure or a success repeating content already
        // seen from that host is a "strike"; a novel success is progress and
        // resets the count. When a host first reaches the strike cap, surface an
        // operator triage signal; the NEXT probe of it is refused up top.
        if let (Some(store), Some(sid)) = (gateway_store.as_ref(), session_id) {
            if store.host_probe_budget.cap() > 0 {
                let probe_hosts = crate::runtime::approved_exec_cache::normalize_targets(
                    &remote_analysis.detected_patterns,
                );
                if !probe_hosts.is_empty() {
                    let probe_ok = body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    let stdout_str = body.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                    let output_hash =
                        crate::runtime::host_probe_budget::content_hash(stdout_str);
                    let root = root_session_id.as_deref().unwrap_or(sid);
                    for host in &probe_hosts {
                        if let crate::runtime::host_probe_budget::ProbeOutcome::Strike {
                            strikes,
                            reached_cap,
                            ..
                        } = store
                            .host_probe_budget
                            .record(sid, host, probe_ok, &output_hash)
                        {
                            if reached_cap {
                                if let Err(e) = store.emit_host_probe_budget_exhausted_event(
                                    sid,
                                    root,
                                    &manifest.agent.id,
                                    host,
                                    strikes,
                                    store.host_probe_budget.cap(),
                                ) {
                                    tracing::warn!(
                                        target: "sandbox_exec",
                                        error = %e,
                                        host = %host,
                                        "Failed to emit host_budget_exhausted event (#853)"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        serde_json::to_string(&body).map_err(Into::into)
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

    #[test]
    fn script_classification_returns_none_on_clean_output() {
        let diag = super::classify_script_network_failure("some output", "", true, false);
        assert!(diag.is_none());
        let diag = super::classify_script_network_failure("", "log: connection reset by policy", false, false);
        assert!(diag.is_none());
    }

    #[test]
    fn script_classification_ignores_http_level_library_errors() {
        let stderr = "Traceback...\nrequests.exceptions.HTTPError: 404 Client Error: Not Found\n";
        let diag = super::classify_script_network_failure("", stderr, true, false);
        assert!(
            diag.is_none(),
            "HTTP-level errors are not connectivity failures and must not be \
             classified as network_isolated: {diag:?}"
        );
        let stderr = "urllib.error.HTTPError: HTTP Error 403: Forbidden\n";
        let diag = super::classify_script_network_failure("", stderr, true, false);
        assert!(diag.is_none(), "{diag:?}");
    }

    #[test]
    fn script_classification_flags_errno_101_network_unreachable() {
        let stderr = "error: cannot connect to imap.gmail.com:993: [Errno 101] Network is unreachable\n";
        let diag = super::classify_script_network_failure("", stderr, true, false).unwrap();
        assert!(diag.starts_with("network_isolated:"), "{diag}");
        assert!(diag.contains("Network unreachable"), "{diag}");
    }

    #[test]
    fn script_classification_network_cap_case_names_connection_layer_and_egress() {
        let stderr = "error: cannot connect to imap.gmail.com:993: [Errno 101] Network is unreachable\n";
        let diag = super::classify_script_network_failure("", stderr, true, false).unwrap();
        assert!(diag.contains("failed at the connection layer"), "{diag}");
        assert!(
            diag.contains("environmental egress problem"),
            "{diag}"
        );
        assert!(diag.contains("approval cannot create connectivity"), "{diag}");
        assert!(!diag.contains("R+16"), "{diag}");
    }

    #[test]
    fn script_classification_no_cap_case_names_missing_capability() {
        let stderr = "URLError: <urlopen error [Errno 111] Connection refused>\n";
        let diag = super::classify_script_network_failure("", stderr, false, false).unwrap();
        assert!(diag.contains("does not declare NetworkAccess"), "{diag}");
        assert!(diag.contains("metadata.autonoetic.remote_access.targets"), "{diag}");
    }

    #[test]
    fn script_classification_evaluation_blocked_case_names_r16() {
        let stderr = "requests.exceptions.ConnectionError: boom\n";
        let diag = super::classify_script_network_failure("", stderr, true, true).unwrap();
        assert!(diag.contains("R+16"), "{diag}");
        assert!(diag.contains("Mock all external services"), "{diag}");
    }
}

#[cfg(test)]
mod exec_ok_tests {
    use super::compute_sandbox_exec_ok;

    #[test]
    fn ok_true_for_normal_nonzero_exit() {
        assert!(compute_sandbox_exec_ok(Some(1)));
        assert!(compute_sandbox_exec_ok(Some(127)));
    }

    #[test]
    fn ok_true_for_exit_zero() {
        assert!(compute_sandbox_exec_ok(Some(0)));
    }

    #[test]
    fn ok_false_for_signal_derived_exit() {
        assert!(!compute_sandbox_exec_ok(Some(128)));
        assert!(!compute_sandbox_exec_ok(Some(137)));
        assert!(!compute_sandbox_exec_ok(Some(143)));
        assert!(!compute_sandbox_exec_ok(Some(159)));
    }

    #[test]
    fn ok_false_when_no_exit_code() {
        assert!(!compute_sandbox_exec_ok(None));
    }
}

#[cfg(test)]
mod remote_access_declaration_tests {
    use crate::runtime::network_policy::load_manifest_remote_access_declaration;

    #[test]
    fn loads_nested_autonoetic_remote_access_declaration() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("SKILL.md"),
            r#"---
metadata:
  autonoetic:
    remote_access:
      enabled_languages: ["python", "javascript", "rust", "go"]
      python_imports: ["requests"]
      js_imports: ["axios"]
      rust_imports: ["reqwest"]
      go_imports: ["net/http"]
      function_calls: ["requests.get", "axios.get"]
      shell_commands: ["curl"]
      package_manager_commands: ["pip install"]
---
# Test
"#,
        )
        .expect("write skill");

        let decl = load_manifest_remote_access_declaration(tmp.path())
            .expect("nested remote_access declaration should parse");
        assert_eq!(decl.enabled_languages.len(), 4);
        assert_eq!(decl.python_imports, vec!["requests".to_string()]);
        assert_eq!(decl.js_imports, vec!["axios".to_string()]);
        assert_eq!(decl.rust_imports, vec!["reqwest".to_string()]);
        assert_eq!(decl.go_imports, vec!["net/http".to_string()]);
    }

    #[test]
    fn loads_top_level_remote_access_declaration() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("SKILL.md"),
            r#"---
remote_access:
  shell_commands: ["curl", "wget"]
---
# Test
"#,
        )
        .expect("write skill");

        let decl = load_manifest_remote_access_declaration(tmp.path())
            .expect("top-level remote_access declaration should parse");
        assert_eq!(
            decl.shell_commands,
            vec!["curl".to_string(), "wget".to_string()]
        );
    }
}

#[cfg(test)]
mod approval_message_tests {
    use super::{sandbox_approval_operator_reason, sandbox_approval_summary_line};
    use crate::runtime::remote_access::{DetectedPattern, DetectedPatternCategory};

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
            category: DetectedPatternCategory::Import,
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

/// Execute layer capture for a list of paths. Returns JSON objects for
/// `captured_layers`.
fn capture_layers_from_paths(
    layer_store: &crate::layer_store::LayerStore,
    capture_paths: &[crate::runtime::tools::CapturePath],
    agent_dir: &Path,
    approval_scope: Option<&LayerApprovalScope>,
) -> Vec<serde_json::Value> {
    let mut captured = Vec::new();
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

        if !host_path.exists() {
            tracing::warn!(
                target: "sandbox",
                path = %cap.path,
                host_path = %host_path.display(),
                "Capture path does not exist in sandbox workspace"
            );
            continue;
        }

        match layer_store.create_from_dir(
            &host_path,
            &cap.path,
            &cap.mount_as,
            approval_scope.cloned(),
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
                captured.push(serde_json::json!({
                    "path": cap.path,
                    "mount_as": cap.mount_as,
                    "layer_id": layer.layer_id,
                    "digest": layer.digest,
                    "file_count": layer.file_count,
                    "size_bytes": layer.size_bytes,
                    "approval_scope": layer.approval_scope,
                    "resolved_packages": layer.resolved_packages,
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
    }
    captured
}

/// Detect package install commands that deposit files into a known directory
/// but were called without explicit `capture_paths`. Returns inferred
/// `CapturePath` entries for each detected target directory.
fn infer_capture_paths_from_command(
    command: &str,
) -> Option<Vec<crate::runtime::tools::CapturePath>> {
    let cmd = command.trim();
    let lower = cmd.to_ascii_lowercase();

    // pip/pip3 install ... --target <dir>
    if lower.starts_with("pip ") || lower.starts_with("pip3 ") {
        if lower.contains(" install ") {
            if let Some(target) = extract_flag_value(cmd, "--target") {
                return Some(vec![crate::runtime::tools::CapturePath {
                    path: target.clone(),
                    mount_as: target,
                }]);
            }
        }
    }

    // python3 -m pip install ... --target <dir>
    if lower.starts_with("python3 -m pip ") || lower.starts_with("python -m pip ") {
        if lower.contains(" install ") {
            if let Some(target) = extract_flag_value(cmd, "--target") {
                return Some(vec![crate::runtime::tools::CapturePath {
                    path: target.clone(),
                    mount_as: target,
                }]);
            }
        }
    }

    // npm install [--prefix <dir>]
    if lower.starts_with("npm install") || lower.starts_with("npm i ") {
        let prefix_dir = extract_flag_value(cmd, "--prefix");
        let node_modules_path = prefix_dir
            .map(|p| format!("{}/node_modules", p.trim_end_matches('/')))
            .unwrap_or_else(|| "/tmp/node_modules".to_string());
        return Some(vec![crate::runtime::tools::CapturePath {
            path: node_modules_path.clone(),
            mount_as: node_modules_path,
        }]);
    }

    // yarn install / yarn add [--cwd <dir>]
    if lower.starts_with("yarn install") || lower.starts_with("yarn add ") {
        let cwd_dir = extract_flag_value(cmd, "--cwd");
        let node_modules_path = cwd_dir
            .map(|p| format!("{}/node_modules", p.trim_end_matches('/')))
            .unwrap_or_else(|| "/tmp/node_modules".to_string());
        return Some(vec![crate::runtime::tools::CapturePath {
            path: node_modules_path.clone(),
            mount_as: node_modules_path,
        }]);
    }

    // pnpm install [--dir <dir>]
    if lower.starts_with("pnpm install") {
        let dir = extract_flag_value(cmd, "--dir");
        let node_modules_path = dir
            .map(|p| format!("{}/node_modules", p.trim_end_matches('/')))
            .unwrap_or_else(|| "/tmp/node_modules".to_string());
        return Some(vec![crate::runtime::tools::CapturePath {
            path: node_modules_path.clone(),
            mount_as: node_modules_path,
        }]);
    }

    // go mod download [-modcacherw]
    if lower.starts_with("go mod download") {
        let gopath = std::env::var("GOPATH").unwrap_or_else(|_| "/tmp/go".to_string());
        let go_cache = format!("{}/pkg/mod", gopath);
        return Some(vec![crate::runtime::tools::CapturePath {
            path: go_cache.clone(),
            mount_as: go_cache,
        }]);
    }

    // cargo fetch / cargo build (captures registry + target)
    if lower.starts_with("cargo fetch") || lower.starts_with("cargo build") {
        let cargo_home =
            std::env::var("CARGO_HOME").unwrap_or_else(|_| "/tmp/cargo_registry".to_string());
        return Some(vec![crate::runtime::tools::CapturePath {
            path: cargo_home.clone(),
            mount_as: cargo_home,
        }]);
    }

    None
}

/// Extract the value of a `--flag <value>` or `--flag=<value>` from a command
/// string. Handles both space-separated and `=`-joined forms. Does minimal
/// shell-aware splitting (respects double-quoted segments).
fn extract_flag_value(command: &str, flag: &str) -> Option<String> {
    let tokens = shlex_split(command)?;
    for (i, token) in tokens.iter().enumerate() {
        if let Some(value) = token.strip_prefix(&format!("{}=", flag)) {
            let v = value.trim_matches('"').trim_matches('\'').to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
        if token == flag && i + 1 < tokens.len() {
            let value = tokens[i + 1]
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            if !value.is_empty() && !value.starts_with('-') {
                return Some(value);
            }
        }
    }
    None
}

/// Minimal shell-like token splitter that handles double-quoted and
/// single-quoted segments. Returns `None` on pathological input.
fn shlex_split(s: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_double = false;
    let mut in_single = false;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if !in_single => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            ' ' | '\t' if !in_double && !in_single => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Some(tokens)
}

#[cfg(test)]
mod approval_ref_binding_tests {
    use super::validate_approval_ref_context;
    use autonoetic_types::background::{
        ApprovalDecision, ApprovalLevel, ApprovalStatus, ScheduledAction,
    };

    fn decision(agent_id: &str, session_id: &str, root: &str) -> ApprovalDecision {
        ApprovalDecision {
            request_id: "apr-1".to_string(),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            action: ScheduledAction::SandboxExec {
                command: "echo ok".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
                intent: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: "2026-01-01T00:00:00Z".to_string(),
            decided_by: "operator".to_string(),
            reason: None,
            root_session_id: Some(root.to_string()),
            workflow_id: None,
            task_id: None,
            approval_level: ApprovalLevel::Operator,
        }
    }

    #[test]
    fn approval_ref_rejects_cross_agent_use() {
        let decision = decision("coder.default", "root/coder.default-1", "root");
        let err =
            validate_approval_ref_context(&decision, "evaluator.default", Some("root/eval-1"))
                .expect_err("cross-agent approval_ref should be rejected");
        assert!(err.to_string().contains("belongs to agent"));
    }

    #[test]
    fn approval_ref_rejects_cross_root_use() {
        let decision = decision("coder.default", "root-a/coder.default-1", "root-a");
        let err = validate_approval_ref_context(&decision, "coder.default", Some("root-b/coder-2"))
            .expect_err("cross-root approval_ref should be rejected");
        assert!(err.to_string().contains("root session"));
    }

    #[test]
    fn approval_ref_accepts_same_agent_and_root() {
        let decision = decision("coder.default", "root/coder.default-1", "root");
        validate_approval_ref_context(&decision, "coder.default", Some("root/coder.default-1"))
            .expect("same agent + same root should be accepted");
    }
}

/// #1002 slices 2-3: declared-mount denial envelope — hermetic (the refusal
/// precedes any sandbox spawn, so no host bwrap is needed).
#[cfg(test)]
mod declared_mount_gate_tests {
    use super::SandboxExecTool;
    use crate::policy::PolicyEngine;
    use crate::runtime::tools::NativeTool;
    use autonoetic_types::agent::AgentManifest;
    use autonoetic_types::config::GatewayConfig;
    use std::path::PathBuf;

    fn manifest_with_mounts(
        mounts: Vec<autonoetic_types::agent::DeclaredMount>,
    ) -> AgentManifest {
        use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};
        let mut m = AgentManifest {
            remote_access: None,
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
                mounts,
            },
            agent: AgentIdentity {
                id: "mount.tester".to_string(),
                name: "mount.tester".to_string(),
                description: "declared-mount gate tests".to_string(),
                singleton: false,
                resident_idle_ttl_secs: None,
            },
            capabilities: vec![autonoetic_types::capability::Capability::CodeExecution {
                patterns: vec!["*".to_string()],
                commands: Vec::new(),
            }],
            ..Default::default()
        };
        // execute() routes wasm manifests differently only via runtime.sandbox;
        // ensure the default stays bwrap for the non-wasm test.
        m.runtime.sandbox = "bubblewrap".to_string();
        m
    }

    fn config_with_roots(roots: Vec<String>) -> GatewayConfig {
        let mut c = GatewayConfig::default();
        c.sandbox.allowed_mount_roots = roots;
        c
    }

    fn run(
        manifest: &AgentManifest,
        config: &GatewayConfig,
        agent_dir: &PathBuf,
        store: std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>,
    ) -> serde_json::Value {
        let policy = PolicyEngine::new(manifest.clone());
        let out = SandboxExecTool
            .execute(
                manifest,
                &policy,
                agent_dir,
                None,
                r#"{"command":"echo hi","intent":"smoke"}"#,
                Some("sess-mounts"),
                None,
                Some(config),
                Some(store),
                None,
            )
            .expect("tool returns a structured result, never errors here");
        serde_json::from_str(&out).unwrap()
    }

    /// A declared mount the allowlist doesn't cover fails the exec with a
    /// structured mount_denied envelope that names the grant — not a bare
    /// ENOENT, and not a silent drop.
    #[test]
    fn uncovered_declared_mount_fails_with_denial_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&secret).unwrap();
        let manifest = manifest_with_mounts(vec![autonoetic_types::agent::DeclaredMount {
            host_path: secret.to_string_lossy().to_string(),
            readonly: true,
        }]);
        let config = config_with_roots(vec![tmp.path().join("granted").to_string_lossy().to_string()]);

        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&tmp.path().join(".gateway"))
                .unwrap(),
        );
        let v = run(&manifest, &config, &tmp.path().join("agent"), store);
        assert_eq!(v["ok"], serde_json::json!(false), "result: {v}");
        assert_eq!(v["error_type"], "permission");
        let denied = v["mount_denied"].as_array().expect("mount_denied array");
        assert_eq!(denied.len(), 1);
        assert!(
            denied[0]["reason"]
                .as_str()
                .unwrap()
                .contains("allowed_mount_roots"),
            "denial must name the grant: {denied:?}"
        );
    }

    /// wasm tier + declared mounts = loud tier rejection before any allowlist
    /// consideration (a wasm manifest has no host filesystem to mount into).
    #[test]
    fn wasm_manifest_with_mounts_is_rejected_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest =
            manifest_with_mounts(vec![autonoetic_types::agent::DeclaredMount {
                host_path: "/var/data/mail".to_string(),
                readonly: true,
            }]);
        manifest.runtime.sandbox = "wasm".to_string();
        let config = config_with_roots(vec!["/".to_string()]);

        // check_mount_support errors surface as a tool error (Err), not a
        // structured result — assert the error names the tier and the paths.
        let policy = PolicyEngine::new(manifest.clone());
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&tmp.path().join(".gateway"))
                .unwrap(),
        );
        let err = SandboxExecTool
            .execute(
                &manifest,
                &policy,
                &tmp.path().join("agent"),
                None,
                r#"{"command":"echo hi","intent":"smoke"}"#,
                Some("sess-wasm-mounts"),
                None,
                Some(&config),
                Some(store),
                None,
            )
            .expect_err("wasm + declared mounts must fail loudly");
        assert!(
            err.to_string().contains("wasm"),
            "error must name the tier: {err}"
        );
    }
}
