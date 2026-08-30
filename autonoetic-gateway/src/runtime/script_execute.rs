//! Script-mode fast path — execute script agents directly in sandbox, bypassing the LLM.

use autonoetic_types::causal_chain::CausalEventRecord;
use autonoetic_types::config::GatewayConfig;
use secrecy::ExposeSecret;
use std::path::{Path, PathBuf};

const AUTONOETIC_INPUT_ENV: &str = "AUTONOETIC_INPUT";
const AUTONOETIC_META_ENV: &str = "AUTONOETIC_META";
const AUTONOETIC_INPUT_PATH_ENV: &str = "AUTONOETIC_INPUT_PATH";
const AUTONOETIC_META_PATH_ENV: &str = "AUTONOETIC_META_PATH";

pub(crate) struct ScriptInvocationFiles {
    pub runtime_dir_host: PathBuf,
    pub input_path_sandbox: String,
    pub meta_path_sandbox: Option<String>,
}

impl ScriptInvocationFiles {
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.runtime_dir_host);
    }
}

pub(crate) fn sandbox_workspace_path(agent_dir: &Path, host_path: &Path) -> String {
    match host_path.strip_prefix(agent_dir) {
        Ok(relative) => format!(
            "{}/{}",
            crate::sandbox::BWRAP_WORKSPACE_DIR,
            relative.to_string_lossy()
        ),
        Err(_) => host_path.to_string_lossy().to_string(),
    }
}

pub(crate) fn write_script_invocation_files(
    agent_dir: &Path,
    input_payload: &str,
    metadata: Option<&serde_json::Value>,
) -> anyhow::Result<ScriptInvocationFiles> {
    let nonce = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let runtime_dir_host = agent_dir.join(".autonoetic_runtime").join(nonce);
    std::fs::create_dir_all(&runtime_dir_host)?;

    let input_path_host = runtime_dir_host.join("input.json");
    std::fs::write(&input_path_host, input_payload)?;

    let meta_path_sandbox = if let Some(meta) = metadata {
        let meta_path_host = runtime_dir_host.join("meta.json");
        std::fs::write(&meta_path_host, meta.to_string())?;
        Some(sandbox_workspace_path(agent_dir, &meta_path_host))
    } else {
        None
    };

    Ok(ScriptInvocationFiles {
        runtime_dir_host,
        input_path_sandbox: sandbox_workspace_path(agent_dir, &input_path_host),
        meta_path_sandbox,
    })
}

pub(crate) fn normalize_script_input_payload(
    input_payload: &str,
    metadata: Option<&serde_json::Value>,
) -> String {
    let Some(meta) = metadata else {
        return input_payload.to_string();
    };
    let meta_text = meta.to_string();

    if let Some(stripped) =
        input_payload.strip_suffix(&format!("\n\nDelegation metadata: {}", meta_text))
    {
        return stripped.to_string();
    }

    let task_marker = "\n\n[Task]\n";
    let metadata_marker = "\n\n[Metadata]\n";
    if input_payload.starts_with("[Context]\n") && input_payload.ends_with(&meta_text) {
        if let (Some(task_start), Some(metadata_start)) = (
            input_payload.find(task_marker),
            input_payload.rfind(metadata_marker),
        ) {
            if task_start < metadata_start {
                let task_body_start = task_start + task_marker.len();
                let task_body = &input_payload[task_body_start..metadata_start];
                return task_body.to_string();
            }
        }
    }

    input_payload.to_string()
}

/// The sandbox spawn path needs a real gateway dir: the bubblewrap driver builds
/// its secret mask from it, so an absent one would mean an unmasked sandbox.
/// Fail closed rather than substituting a derived or default path — deriving it
/// is exactly what emitted an empty mask in #1145.
fn require_gateway_dir(gateway_dir: Option<&Path>) -> anyhow::Result<&Path> {
    gateway_dir.ok_or_else(|| {
        anyhow::anyhow!(
            "gateway_dir is required to execute a script agent: the sandbox \
             secret mask is built from it"
        )
    })
}

fn prepare_runtime_lock_layer_mounts(
    agent_dir: &Path,
    runtime_lock_rel_path: &str,
    gateway_dir: Option<&Path>,
) -> anyhow::Result<(Vec<crate::sandbox::SandboxMount>, Vec<String>, Vec<String>)> {
    let Some(gw_dir) = gateway_dir else {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    };

    let lock_path = agent_dir.join(runtime_lock_rel_path);
    if !lock_path.exists() {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }

    let parsed_lock = match crate::runtime_lock::resolve_runtime_lock(&lock_path) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(
                target: "script_execute",
                path = %lock_path.display(),
                error = %error,
                "Failed to parse runtime.lock; skipping layer mounting for script execution"
            );
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }
    };

    if parsed_lock.layers.is_empty() {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }

    let lock_layers: Vec<crate::runtime::tools::sandbox::LayerMount> = parsed_lock
        .layers
        .iter()
        .map(|layer| crate::runtime::tools::sandbox::LayerMount {
            layer_id: layer.layer_id.clone(),
            mount_path: layer.mount_path.clone(),
        })
        .collect();
    let mut mounts = Vec::new();
    let mut python_paths = Vec::new();
    let mut node_paths = Vec::new();
    crate::runtime::tools::sandbox::extract_and_mount_layers(
        &lock_layers,
        gw_dir,
        "runtime.lock",
        &mut mounts,
        &mut python_paths,
        &mut node_paths,
    )?;

    Ok((mounts, python_paths, node_paths))
}

/// Execute a script agent directly in sandbox, bypassing the LLM.
pub(crate) async fn execute_script_in_sandbox(
    agent_dir: &PathBuf,
    script_path: &PathBuf,
    input_payload: &str,
    metadata: Option<&serde_json::Value>,
    sandbox_type: &str,
    _config: &GatewayConfig,
    sandbox_kill: Option<(
        std::sync::Arc<crate::runtime::active_execution_registry::ActiveExecutionRegistry>,
        String,
    )>,
    capabilities: &[autonoetic_types::capability::Capability],
    input_mode: autonoetic_types::agent::ScriptInputMode,
    credential_env: Vec<(String, String)>,
    runtime_lock_rel_path: &str,
    gateway_dir: Option<&Path>,
    middleware: Option<&autonoetic_types::agent::Middleware>,
) -> anyhow::Result<String> {
    use std::io::Write;

    tracing::info!(
        agent_dir = %agent_dir.display(),
        script = %script_path.display(),
        sandbox = %sandbox_type,
        input_mode = ?input_mode,
        "Executing script agent"
    );

    let driver = crate::sandbox::SandboxDriverKind::parse(sandbox_type)?;
    let mut overrides = crate::sandbox::BwrapIsolationOverrides::from_capabilities(capabilities);
    let has_evaluation_cap = capabilities.iter().any(|c| {
        matches!(
            c,
            autonoetic_types::capability::Capability::Evaluation { .. }
        )
    });
    if has_evaluation_cap {
        overrides.force_network_off = true;
        overrides.share_net = false;
    }
    let normalized_input = normalize_script_input_payload(input_payload, metadata);
    // #1222: script-mode middleware mirrors the LLM loop's hooks at the payload
    // boundary. pre_process transforms the normalized payload before it lands
    // in AUTONOETIC_INPUT_PATH / AUTONOETIC_INPUT / argv; post_process
    // transforms stdout before it becomes the reply, trace, and timeline row.
    // Fail-closed like the LLM path: a broken hook fails the turn rather than
    // feeding the entrypoint untransformed input or leaking raw output.
    // Hooks inherit the entrypoint's isolation overrides and its kill
    // registration, so an emergency stop takes them down with the turn.
    let hook_kill = sandbox_kill
        .as_ref()
        .map(|(reg, root)| (reg, root.as_str()));
    let normalized_input = match middleware.and_then(|m| m.pre_process.as_deref()) {
        Some(hook) => {
            run_script_middleware_hook(
                driver,
                agent_dir,
                gateway_dir,
                &overrides,
                hook_kill,
                hook,
                "pre_process",
                &normalized_input,
            )
            .await?
        }
        None => normalized_input,
    };
    let invocation_files = write_script_invocation_files(agent_dir, &normalized_input, metadata)?;
    let entrypoint_relative = match script_path.strip_prefix(agent_dir) {
        Ok(relative) => format!(
            "{}/{}",
            crate::sandbox::BWRAP_WORKSPACE_DIR,
            relative.to_string_lossy()
        ),
        Err(_) => script_path.to_string_lossy().to_string(),
    };

    let script_args = match input_mode {
        autonoetic_types::agent::ScriptInputMode::Args => vec![normalized_input.clone()],
        autonoetic_types::agent::ScriptInputMode::Stdin => vec![],
    };

    // Script-mode is intent-based exec: run the declared entry file. `language:
    // None` execs it directly (shebang-driven), matching prior behavior; the
    // Process backend renders it back to a shell line.
    let exec_kind = crate::exec_request::ExecutionKind::Code {
        language: None,
        source: crate::exec_request::CodeSource::Entry(entrypoint_relative),
        args: script_args.clone(),
    };

    // Primary contract for script agents: file-backed payload + metadata paths.
    // Keep normalized env payloads for compatibility with older scripts.
    let mut autonoetic_env = vec![
        (
            AUTONOETIC_INPUT_PATH_ENV.to_string(),
            invocation_files.input_path_sandbox.clone(),
        ),
        (AUTONOETIC_INPUT_ENV.to_string(), normalized_input.clone()),
    ];
    if let Some(meta) = metadata {
        autonoetic_env.push((AUTONOETIC_META_ENV.to_string(), meta.to_string()));
    }
    if let Some(meta_path) = invocation_files.meta_path_sandbox.as_ref() {
        autonoetic_env.push((AUTONOETIC_META_PATH_ENV.to_string(), meta_path.clone()));
    }
    for (k, v) in &credential_env {
        autonoetic_env.push((k.clone(), v.clone()));
    }

    let (runtime_lock_mounts, layer_python_paths, layer_node_paths) =
        prepare_runtime_lock_layer_mounts(agent_dir, runtime_lock_rel_path, gateway_dir)?;
    if !layer_python_paths.is_empty() {
        autonoetic_env.push(("PYTHONPATH".to_string(), layer_python_paths.join(":")));
    }
    if !layer_node_paths.is_empty() {
        autonoetic_env.push(("NODE_PATH".to_string(), layer_node_paths.join(":")));
    }

    // WASM tier runs in-process, not via the POSIX spawn path: route it through
    // the unified `run_to_output` entry. The entry must be the workspace-relative
    // path (the backend joins it onto `agent_dir`), not the `BWRAP_WORKSPACE_DIR`-
    // prefixed path the process backend renders into a shell line.
    if driver.runs_in_process() {
        let entry_relative = script_path
            .strip_prefix(agent_dir)
            .map(|relative| relative.to_string_lossy().to_string())
            .unwrap_or_else(|_| script_path.to_string_lossy().to_string());
        let wasm_request = crate::exec_request::ExecutionKind::Code {
            language: None,
            source: crate::exec_request::CodeSource::Entry(entry_relative),
            args: script_args,
        };
        // Stdin mode delivers the payload on the module's stdin (Args mode already
        // carries it in argv); mirrors the process tier's stdin handling.
        let stdin_bytes = match input_mode {
            autonoetic_types::agent::ScriptInputMode::Stdin => {
                Some(normalized_input.clone().into_bytes())
            }
            autonoetic_types::agent::ScriptInputMode::Args => None,
        };
        let result = crate::sandbox::SandboxRunner::run_to_output(
            driver,
            &agent_dir.to_string_lossy(),
            require_gateway_dir(gateway_dir)?,
            &wasm_request,
            None,
            Some(&overrides),
            &autonoetic_env,
            None,
            stdin_bytes,
        );
        invocation_files.cleanup();
        let out = result?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        if out.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&out.stderr);
            tracing::error!(stderr = %stderr, stdout = %stdout, exit_code = out.exit_code, "WASM script execution failed");
            anyhow::bail!(
                "Script execution failed with code {}: stdout={}, stderr={}",
                out.exit_code,
                stdout,
                stderr
            );
        }
        tracing::info!(stdout_len = stdout.len(), "WASM script execution completed");
        return apply_script_post_hook(
            middleware,
            driver,
            agent_dir,
            gateway_dir,
            &overrides,
            hook_kill,
            stdout,
        )
        .await;
    }

    let mut runner = match crate::sandbox::SandboxRunner::spawn_with_session_content_and_env(
        driver,
        &agent_dir.to_string_lossy(),
        require_gateway_dir(gateway_dir)?,
        &exec_kind,
        None,
        runtime_lock_mounts,
        Some(&overrides),
        &autonoetic_env,
        None,
    ) {
        Ok(runner) => runner,
        Err(error) => {
            invocation_files.cleanup();
            return Err(error);
        }
    };

    let _script_sandbox_guard = sandbox_kill.as_ref().and_then(|(reg, root)| {
        let pid = runner.process.id();
        (pid > 0).then(|| reg.register_sandbox_child_pid(root, pid))
    });

    if input_mode == autonoetic_types::agent::ScriptInputMode::Stdin {
        if let Some(mut stdin) = runner.process.stdin.take() {
            stdin
                .write_all(normalized_input.as_bytes())
                .map_err(|e| anyhow::anyhow!("Failed to write to script stdin: {}", e))?;
        }
    }

    let output = match tokio::task::spawn_blocking(move || runner.process.wait_with_output()).await
    {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            invocation_files.cleanup();
            return Err(anyhow::anyhow!("Failed to execute script: {}", error));
        }
        Err(error) => {
            invocation_files.cleanup();
            return Err(anyhow::anyhow!("Task join error: {}", error));
        }
    };

    invocation_files.cleanup();

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        tracing::error!(stderr = %stderr, stdout = %stdout, status = ?output.status.code(), "Script execution failed");
        let has_network_cap = capabilities.iter().any(|c| {
            matches!(
                c,
                autonoetic_types::capability::Capability::NetworkAccess { .. }
            )
        });
        let diag = crate::runtime::tools::sandbox::classify_script_network_failure(
            &stdout,
            &stderr,
            has_network_cap,
            has_evaluation_cap,
        );
        anyhow::bail!(
            "Script execution failed with code {:?}: stdout={}, stderr={}{}",
            output.status.code(),
            stdout,
            stderr,
            diag.map(|d| format!("\n{}", d)).unwrap_or_default()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    tracing::info!(stdout_len = stdout.len(), "Script execution completed");

    apply_script_post_hook(
        middleware,
        driver,
        agent_dir,
        gateway_dir,
        &overrides,
        hook_kill,
        stdout,
    )
    .await
}

/// Run one script-mode middleware hook (#1222): `command` executes in the
/// agent workspace under the manifest's sandbox driver — the same shape as the
/// LLM path's `run_middleware_script`, plus the entrypoint's isolation
/// overrides and kill registration so hooks share the turn's isolation and
/// emergency-stop semantics. `payload` goes in verbatim on stdin; stdout
/// comes back verbatim as the replacement, so JSON-to-JSON mapping scripts
/// written to the contract work unchanged and text payloads stay expressible.
#[allow(clippy::too_many_arguments)]
async fn run_script_middleware_hook(
    driver: crate::sandbox::SandboxDriverKind,
    agent_dir: &Path,
    gateway_dir: Option<&Path>,
    overrides: &crate::sandbox::BwrapIsolationOverrides,
    sandbox_kill: Option<(
        &std::sync::Arc<crate::runtime::active_execution_registry::ActiveExecutionRegistry>,
        &str,
    )>,
    command: &str,
    hook: &str,
    payload: &str,
) -> anyhow::Result<String> {
    use std::io::Write;

    let gateway_dir = require_gateway_dir(gateway_dir)?;
    let mut runner = crate::sandbox::SandboxRunner::spawn_with_driver_and_dependencies(
        driver,
        &agent_dir.to_string_lossy(),
        gateway_dir,
        command,
        None,
        Some(overrides),
    )?;
    let _hook_kill_guard = sandbox_kill.and_then(|(reg, root)| {
        let pid = runner.process.id();
        (pid > 0).then(|| reg.register_sandbox_child_pid(root, pid))
    });
    if let Some(mut stdin) = runner.process.stdin.take() {
        stdin
            .write_all(payload.as_bytes())
            .map_err(|e| anyhow::anyhow!("script middleware {hook} ({command}) stdin: {e}"))?;
    }
    // Mirrors the entry script's wait: blocking waits belong off the Tokio
    // worker thread.
    let output =
        match tokio::task::spawn_blocking(move || runner.process.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                return Err(anyhow::anyhow!(
                    "script middleware {hook} ({command}) wait failed: {error}"
                ));
            }
            Err(error) => {
                return Err(anyhow::anyhow!("Task join error: {error}"));
            }
        };
    if !output.status.success() {
        anyhow::bail!(
            "script middleware {} hook ({}) failed with {}: {}",
            hook,
            command,
            output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string()),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).map_err(|_| {
        anyhow::anyhow!(
            "script middleware {} hook ({}) returned non-UTF-8 stdout",
            hook,
            command
        )
    })
}

/// Apply the configured `post_process` hook to script stdout, if any.
#[allow(clippy::too_many_arguments)]
async fn apply_script_post_hook(
    middleware: Option<&autonoetic_types::agent::Middleware>,
    driver: crate::sandbox::SandboxDriverKind,
    agent_dir: &Path,
    gateway_dir: Option<&Path>,
    overrides: &crate::sandbox::BwrapIsolationOverrides,
    sandbox_kill: Option<(
        &std::sync::Arc<crate::runtime::active_execution_registry::ActiveExecutionRegistry>,
        &str,
    )>,
    stdout: String,
) -> anyhow::Result<String> {
    match middleware.and_then(|m| m.post_process.as_deref()) {
        Some(hook) => {
            run_script_middleware_hook(
                driver,
                agent_dir,
                gateway_dir,
                overrides,
                sandbox_kill,
                hook,
                "post_process",
                &stdout,
            )
            .await
        }
        None => Ok(stdout),
    }
}

/// Write a single `causal_events` row for script-agent fast-path execution.
/// Called in place of the former no-op `log_gateway_causal_event` so that
/// script agent runs are visible in `execution_search` and session_overview.
pub(crate) fn script_causal_event(
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    agent_id: &str,
    session_id: &str,
    event_seq: u64,
    action: &str,
    status: &str,
    payload: serde_json::Value,
) {
    let Some(store) = store else { return };
    let _ = store.create_causal_event(&CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        event_seq,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "script".to_string(),
        action: action.to_string(),
        status: status.to_string(),
        enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
        target: None,
        payload: Some(payload.to_string()),
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    });
}

/// Emit an `agent.message` live-digest timeline row carrying a script agent's
/// stdout, so the room TUI shows script output inline at the default (`Normal`)
/// altitude — the same way a reasoning agent's narrative reaches the operator.
///
/// Why `agent.message` and not `tool.completed`: the room TUI reads
/// `live_digest_events` (not `causal_events`) and its default floor is
/// `Normal`; `tool.completed` is `Detail` (hidden at the default floor), so
/// surfacing the run as `tool.completed` would leave it invisible without the
/// operator dialing the floor down. A script's stdout *is* its reply — the
/// direct analog of a reasoning agent's end-turn text, which `log_llm_completion`
/// mirrors onto the timeline as `agent.message` (`session_tracer.rs:698`). We
/// reuse that event so a `print("toto")` reads as a conversational line.
///
/// The row is built with the shared [`build_timeline_event`] so its shape
/// (attribution, node id, payload serialization, refs) stays identical to every
/// other timeline producer. The stdout is redacted (`redact_embedded_secrets`,
/// matching `agent.message`) and capped at the shared narrative cap. Empty
/// output is skipped, mirroring the reasoning path.
pub(crate) fn emit_script_message_timeline(
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    agent_id: &str,
    session_id: &str,
    stdout: &str,
) {
    let Some(store) = store else {
        return;
    };
    let message = stdout.trim();
    if message.is_empty() {
        return;
    }
    let role = crate::runtime::session_timeline::derive_role(agent_id);
    let principal = autonoetic_types::principal::Principal::agent(agent_id.to_string());
    let capped = cap_chars(
        &autonoetic_types::redaction::redact_embedded_secrets(message),
        crate::runtime::session_tracer::TIMELINE_AGENT_NARRATIVE_MAX_CHARS,
    );
    let row = crate::runtime::session_timeline::build_timeline_event(
        crate::runtime::live_digest::base_session_id(session_id).to_string(),
        session_id.to_string(),
        None,
        &principal,
        &role,
        "agent.message",
        None, // altitude derived from (event_type, role) -> Normal for agent.message
        Some(serde_json::json!({ "message": capped })),
        autonoetic_types::session_timeline::TimelineRefs::default(),
    );
    if let Err(e) = store.create_live_digest_event(&row) {
        tracing::debug!(
            target: "live_digest",
            error = %e,
            "script agent.message timeline emit failed"
        );
    }
}

/// Truncate to `max` visible chars, appending an ellipsis (which counts toward
/// the cap) when truncation occurs — matches the timeline preview cap style.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let truncated: String = s.chars().take(max - 1).collect();
    format!("{truncated}…")
}

/// Resolve credential env vars from a runtime.lock's `credentials` section.
///
/// For each `LockedCredentialMount`, looks up credentials in the store and
/// resolves their secrets from the vault. Each credential is injected under
/// its own env-var name: an `inject_as` holding a valid env-var identifier
/// (e.g. `GMAIL_EMAIL`) is honored as-is; anything else (NULL, or an HTTP
/// injection style like `bearer` / `header:X-…` used by `credential_request`)
/// falls back to the service-derived [`inject_as_for_service`] name. A service
/// may legitimately resolve to several env vars (multi-secret services such as
/// gmail → `GMAIL_EMAIL` + `GMAIL_APP_PASSWORD`).
///
/// `spawn_bindings` provides credential overrides from `agent_spawn` — entries
/// here take precedence over runtime.lock entries for the same service.
///
/// Fail-closed: when credential mounts are declared (lock or bindings) but a
/// declared service resolves to zero env vars (no credential record, or the
/// vault secret is missing), the spawn fails with a
/// `credential_injection_failed` error naming the service. Running the script
/// without its declared credentials only produced cryptic "missing env var"
/// script errors and agent onboarding loops — the typed failure surfaces the
/// real cause immediately.
pub(crate) fn resolve_credential_env(
    agent_dir: &Path,
    gateway_dir: &Path,
    store: &crate::scheduler::gateway_store::GatewayStore,
) -> anyhow::Result<Vec<(String, String)>> {
    resolve_credential_env_with_bindings(agent_dir, gateway_dir, store, &[])
}

/// The env-var name a credential is injected under, and whether the name came
/// from the credential's own `inject_as` (`true`) or from the service-derived
/// fallback (`false`). `inject_as` is overloaded: HTTP injection styles
/// (`bearer`, `Authorization`, `header:X-…`) belong to `credential_request`,
/// not env injection, so they take the fallback.
pub(crate) fn env_var_name_for_credential(
    cred: &autonoetic_types::agent::CredentialRecord,
    service: &str,
) -> (String, bool) {
    if let Some(name) = cred.inject_as.as_deref() {
        let lower = name.to_ascii_lowercase();
        let http_style =
            lower == "bearer" || lower == "authorization" || lower.starts_with("header:");
        if !http_style && is_injectable_env_var_name(name) {
            return (name.to_string(), true);
        }
    }
    (
        autonoetic_types::runtime_lock::inject_as_for_service(service),
        false,
    )
}

/// True when `name` is a safe env-var identifier to inject into a sandbox:
/// POSIX-ish shape, and not one of the names that would let a credential
/// record hijack the process runtime (PATH, LD_*, …). `inject_as` values are
/// operator-approved, but defense in depth applies: an unsafe name is never
/// honored as an env var — the service-derived fallback is used instead.
fn is_injectable_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    !matches!(
        name,
        "PATH"
            | "HOME"
            | "IFS"
            | "ENV"
            | "BASH_ENV"
            | "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "LD_AUDIT"
            | "DYLD_INSERT_LIBRARIES"
            | "PYTHONPATH"
            | "NODE_PATH"
    )
}

/// If `secret` is a flat JSON object whose values are all strings, return its
/// (field, value) pairs. Multi-field credentials (e.g. a `user_prompt` flow
/// that collected several secret fields) are stored in this shape under the
/// credential id, so spawn-time injection can expose both the combined object
/// and each field individually. Returns `None` for scalars, nested objects,
/// and non-JSON values.
pub(crate) fn flat_json_object_fields(secret: &str) -> Option<Vec<(String, String)>> {
    let value: serde_json::Value = serde_json::from_str(secret).ok()?;
    let obj = value.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let mut fields = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        fields.push((k.clone(), v.as_str()?.to_string()));
    }
    Some(fields)
}

/// Env-var name for one field of a multi-field credential:
/// `<SERVICE>_<FIELD>`, both parts uppercase-sanitized. `None` when the field
/// name has no usable characters or the composed name is not a safe injection
/// target.
pub(crate) fn field_env_var_name(service: &str, field: &str) -> Option<String> {
    let mut f = String::new();
    for c in field.chars() {
        if c.is_ascii_alphanumeric() {
            f.push(c.to_ascii_uppercase());
        } else {
            f.push('_');
        }
    }
    let f = f.trim_matches('_');
    if f.is_empty() {
        return None;
    }
    let name = format!(
        "{}_{}",
        autonoetic_types::runtime_lock::env_prefix_for_service(service),
        f
    );
    is_injectable_env_var_name(&name).then_some(name)
}

/// A resolved credential env var, with the provenance needed for deterministic
/// collision handling.
struct ResolvedCredentialEnv {
    env_var: String,
    secret: String,
    /// True when the env-var name came from the credential's own `inject_as`;
    /// false when derived from the service name.
    explicit: bool,
    credential_id: String,
}

/// Push a resolved env var, resolving collisions deterministically: an
/// explicit `inject_as` name shadows the service-derived fallback; otherwise
/// the first match wins. Both cases are logged loudly — one secret silently
/// shadowing another under the same env var must never be quiet.
fn push_credential_env(
    resolved: &mut Vec<ResolvedCredentialEnv>,
    env_var: String,
    secret: String,
    explicit: bool,
    credential_id: &str,
) {
    if let Some(pos) = resolved.iter().position(|e| e.env_var == env_var) {
        let existing = &resolved[pos];
        if explicit && !existing.explicit {
            tracing::warn!(
                target: "script_execute",
                env_var = %env_var,
                shadowed_credential_id = %existing.credential_id,
                winner_credential_id = %credential_id,
                "Credential env-var collision: explicit inject_as shadows the service-derived fallback"
            );
            resolved[pos] = ResolvedCredentialEnv {
                env_var,
                secret,
                explicit,
                credential_id: credential_id.to_string(),
            };
        } else {
            tracing::warn!(
                target: "script_execute",
                env_var = %env_var,
                kept_credential_id = %existing.credential_id,
                ignored_credential_id = %credential_id,
                "Credential env-var collision: keeping first match — pin a credential_id in spawn bindings to disambiguate"
            );
        }
        return;
    }
    resolved.push(ResolvedCredentialEnv {
        env_var,
        secret,
        explicit,
        credential_id: credential_id.to_string(),
    });
}

/// Push a credential's secret under its env-var name, and — when the secret
/// is a multi-field blob (flat JSON object) — additionally push each field
/// under `<SERVICE>_<FIELD>` so scripts can consume either the combined value
/// or individual variables. The combined value is always pushed under the
/// primary name; field vars are additive.
///
/// Order matters: the combined value goes in **first** so that a primary name
/// which happens to collide with a derived field name (`inject_as:
/// SERVICE_FIELD`) keeps the combined value — `push_credential_env` resolves
/// same-provenance collisions first-match-wins, so pushing fields first would
/// silently drop the primary contract.
fn push_credential_secret_env(
    resolved: &mut Vec<ResolvedCredentialEnv>,
    env_var: String,
    secret: String,
    explicit: bool,
    credential_id: &str,
    service: &str,
) {
    let fields = flat_json_object_fields(&secret);
    push_credential_env(resolved, env_var, secret, explicit, credential_id);
    if let Some(fields) = fields {
        for (field, value) in fields {
            if let Some(field_env) = field_env_var_name(service, &field) {
                push_credential_env(resolved, field_env, value, explicit, credential_id);
            }
        }
    }
}

/// Like [`resolve_credential_env`] but accepts spawn-time credential bindings
/// that override runtime.lock entries for matching services.
pub(crate) fn resolve_credential_env_with_bindings(
    agent_dir: &Path,
    gateway_dir: &Path,
    store: &crate::scheduler::gateway_store::GatewayStore,
    spawn_bindings: &[autonoetic_types::runtime_lock::LockedCredentialMount],
) -> anyhow::Result<Vec<(String, String)>> {
    let lock_path = agent_dir.join(
        "runtime.lock",
    );
    let lock: autonoetic_types::runtime_lock::RuntimeLock = match std::fs::read_to_string(&lock_path)
    {
        Ok(content) => match serde_yaml::from_str(&content) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    target: "script_execute",
                    path = %lock_path.display(),
                    error = %e,
                    "Failed to parse runtime.lock; skipping credential resolution"
                );
                return Ok(vec![]);
            }
        },
        Err(_) => return Ok(vec![]),
    };

    if lock.credentials.is_empty() && spawn_bindings.is_empty() {
        return Ok(vec![]);
    }

    // Merge: spawn_bindings override lock.credentials for matching services.
    let merged: Vec<autonoetic_types::runtime_lock::LockedCredentialMount> = {
        let mut result: Vec<autonoetic_types::runtime_lock::LockedCredentialMount> = Vec::new();
        let binding_services: std::collections::HashSet<&str> = spawn_bindings
            .iter()
            .map(|b| b.service.as_str())
            .collect();
        for cm in &lock.credentials {
            if !binding_services.contains(cm.service.as_str()) {
                result.push(cm.clone());
            }
        }
        for b in spawn_bindings {
            result.push(b.clone());
        }
        result
    };

    if crate::vault::ensure_default_key(gateway_dir).is_err() {
        tracing::warn!(target: "script_execute", "Failed to ensure vault key; skipping credential resolution");
        return Ok(vec![]);
    }
    let vault_path = crate::vault::default_vault_path(gateway_dir);
    let vault = match crate::vault::Vault::load_from_file(&vault_path) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "script_execute", error = %e, "Failed to load vault; skipping credential resolution");
            return Ok(vec![]);
        }
    };

    let mut resolved: Vec<ResolvedCredentialEnv> = Vec::new();
    let mut unsatisfied_services: Vec<String> = Vec::new();
    for cm in &merged {
        let mut mount_resolved = 0usize;

        // If a specific credential_id is declared in runtime.lock, resolve directly.
        if let Some(ref cred_id) = cm.credential_id {
            match store.get_credential(cred_id) {
                Ok(Some(cred)) => {
                    let (env_var, explicit) = env_var_name_for_credential(&cred, &cm.service);
                    if let Some(secret) = vault.get_secret(&cred.secret_name) {
                        tracing::info!(
                            target: "script_execute",
                            service = %cm.service,
                            credential_id = %cred_id,
                            env_var = %env_var,
                            "Resolved credential by ID for script agent"
                        );
                        push_credential_secret_env(
                            &mut resolved,
                            env_var,
                            secret.expose_secret().to_string(),
                            explicit,
                            cred_id,
                            &cm.service,
                        );
                        mount_resolved += 1;
                    } else {
                        tracing::warn!(
                            target: "script_execute",
                            service = %cm.service,
                            credential_id = %cred_id,
                            secret_name = %cred.secret_name,
                            "Secret not found in vault for pinned credential_id"
                        );
                    }
                    if mount_resolved == 0 {
                        unsatisfied_services.push(cm.service.clone());
                    }
                    continue;
                }
                Ok(None) => {
                    tracing::warn!(
                        target: "script_execute",
                        credential_id = %cred_id,
                        "Pinned credential_id not found in store; falling back to service resolution"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "script_execute",
                        credential_id = %cred_id,
                        error = %e,
                        "Failed to look up pinned credential_id"
                    );
                }
            }
        }

        // Fallback: resolve by service name. Every credential stored for the
        // service is injected under its own env-var name — an env-var-shaped
        // `inject_as` resolves under that name, anything else under the
        // service-derived `<SERVICE>_SECRET`. Multi-secret services (e.g.
        // gmail → GMAIL_EMAIL + GMAIL_APP_PASSWORD) resolve fully instead of
        // silently picking a first match.
        let creds = match store.list_credentials_by_service(&cm.service) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "script_execute",
                    service = %cm.service,
                    error = %e,
                    "Failed to list credentials for service"
                );
                unsatisfied_services.push(cm.service.clone());
                continue;
            }
        };
        if creds.is_empty() {
            tracing::warn!(
                target: "script_execute",
                service = %cm.service,
                "No credential found for service"
            );
        }
        for cred in &creds {
            let (env_var, explicit) = env_var_name_for_credential(cred, &cm.service);
            match vault.get_secret(&cred.secret_name) {
                Some(secret) => {
                    tracing::info!(
                        target: "script_execute",
                        service = %cm.service,
                        credential_id = %cred.credential_id,
                        env_var = %env_var,
                        "Resolved credential for script agent"
                    );
                    push_credential_secret_env(
                        &mut resolved,
                        env_var,
                        secret.expose_secret().to_string(),
                        explicit,
                        &cred.credential_id,
                        &cm.service,
                    );
                    mount_resolved += 1;
                }
                None => {
                    tracing::warn!(
                        target: "script_execute",
                        service = %cm.service,
                        credential_id = %cred.credential_id,
                        secret_name = %cred.secret_name,
                        "Secret not found in vault; skipping credential injection"
                    );
                }
            }
        }
        if mount_resolved == 0 {
            unsatisfied_services.push(cm.service.clone());
        }
    }

    if !unsatisfied_services.is_empty() {
        anyhow::bail!(
            "credential_injection_failed: credential mount(s) declared for service(s) [{}] but no secret resolved (no credential record, or the vault secret is missing). Onboard via credential_setup or pin a valid credential_id; refusing to spawn the script agent without its declared credentials.",
            unsatisfied_services.join(", ")
        );
    }

    Ok(resolved
        .into_iter()
        .map(|entry| (entry.env_var, entry.secret))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_script_input_payload_strips_delegation_suffix() {
        let metadata = serde_json::json!({
            "delegated_role": "weather.forecast",
            "reply_to_agent_id": "planner.default"
        });
        let payload = r#"{"location":"Paris, France","date":"tomorrow"}"#;
        let kickoff = format!("{payload}\n\nDelegation metadata: {}", metadata);
        assert_eq!(
            normalize_script_input_payload(&kickoff, Some(&metadata)),
            payload
        );
    }

    /// Process-env guard for the vault key, restored on drop (tests in this
    /// binary share one process under `cargo test`).
    struct VaultKeyGuard {
        old_key: Option<String>,
        old_key_path: Option<String>,
    }

    impl VaultKeyGuard {
        fn set_test_key() -> Self {
            let old_key = std::env::var("AUTONOETIC_VAULT_KEY").ok();
            std::env::set_var(
                "AUTONOETIC_VAULT_KEY",
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            );
            let old_key_path = std::env::var("AUTONOETIC_VAULT_KEY_PATH").ok();
            std::env::remove_var("AUTONOETIC_VAULT_KEY_PATH");
            Self {
                old_key,
                old_key_path,
            }
        }
    }

    impl Drop for VaultKeyGuard {
        fn drop(&mut self) {
            match &self.old_key {
                Some(v) => std::env::set_var("AUTONOETIC_VAULT_KEY", v),
                None => std::env::remove_var("AUTONOETIC_VAULT_KEY"),
            }
            match &self.old_key_path {
                Some(v) => std::env::set_var("AUTONOETIC_VAULT_KEY_PATH", v),
                None => std::env::remove_var("AUTONOETIC_VAULT_KEY_PATH"),
            }
        }
    }

    /// Shared fixture: a temp gateway dir with a vault holding `secrets`, an
    /// agent dir whose runtime.lock declares the given credential mounts
    /// (JSON array fragment), and an open store.
    fn credential_resolver_fixture(
        secrets: &[(&str, &str)],
        lock_credentials_json: &str,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        crate::scheduler::gateway_store::GatewayStore,
    ) {
        let temp = tempfile::tempdir().expect("tempdir");
        let gateway_dir = temp.path().join(".gateway");
        let agent_dir = temp.path().join("agent");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();

        // Vault at the path resolve_credential_env derives
        // (<gateway_dir>/vault.enc.json).
        let mut vault = crate::vault::Vault::new();
        for (name, value) in secrets {
            vault.set_secret(name, value.to_string());
        }
        vault
            .persist_to_file(&gateway_dir.join("vault.enc.json"))
            .expect("vault persist");

        std::fs::write(
            agent_dir.join("runtime.lock"),
            format!(
                r#"{{"gateway":{{"artifact":"marketplace://gateway/autonoetic-gateway","version":"0.1.0","sha256":"sha256:abc","binary_sha256":"sha256:def","build_tag":"0.1.0","signature":null}},"sdk":{{"version":"0.1.0"}},"sandbox":{{"backend":"bubblewrap"}},"credentials":{}}}"#,
                lock_credentials_json
            ),
        )
        .expect("lock write");

        let store = crate::scheduler::gateway_store::GatewayStore::open(temp.path()).unwrap();
        (temp, gateway_dir, agent_dir, store)
    }

    fn resolver_test_credential(
        credential_id: &str,
        service: &str,
        secret_name: &str,
        inject_as: Option<&str>,
    ) -> autonoetic_types::agent::CredentialRecord {
        autonoetic_types::agent::CredentialRecord {
            credential_id: credential_id.to_string(),
            service: service.to_string(),
            secret_name: secret_name.to_string(),
            inject_as: inject_as.map(str::to_string),
            created_by_agent: None,
            expires_at: None,
            shared_with: vec![],
            allowed_hosts: vec![],
            refresh_token_secret_name: None,
            refresh_url: None,
            refresh_method: None,
            refresh_headers: None,
            refresh_extract_access_token: None,
            refresh_extract_refresh_token: None,
            refresh_extract_expires_in: None,
            label: None,
        }
    }

    /// Resolver contract for the env-var name: the stored `inject_as` IS the
    /// injection target when it holds a valid env-var identifier. NULL and
    /// HTTP injection styles (`bearer`/`header:…`, used by
    /// `credential_request`) fall back to the service-derived
    /// `<SERVICE>_SECRET`; unsafe names (PATH, LD_…) are never honored.
    /// Diagnosed from session-d1d8c2bb: credentials stored with
    /// inject_as=GMAIL_EMAIL/GMAIL_APP_PASSWORD never reached the script
    /// because the resolver only ever injected the service-derived name.
    #[test]
    #[serial_test::serial]
    fn resolve_credential_env_inject_as_is_the_env_var_name() {
        let _guard = VaultKeyGuard::set_test_key();
        let (_temp, gateway_dir, agent_dir, store) = credential_resolver_fixture(
            &[("api-key-secret", "top-secret-value")],
            r#"[{"service":"github"}]"#,
        );

        // NULL inject_as → resolves under the service-derived env var.
        store
            .upsert_credential(&resolver_test_credential(
                "cred_a",
                "github",
                "api-key-secret",
                None,
            ))
            .unwrap();
        assert_eq!(
            resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap(),
            vec![("GITHUB_SECRET".to_string(), "top-secret-value".to_string())],
            "NULL inject_as must resolve under the service-derived env var"
        );

        // Explicit env-var inject_as → honored as the injection target.
        store
            .upsert_credential(&resolver_test_credential(
                "cred_a",
                "github",
                "api-key-secret",
                Some("GITHUB_TOKEN"),
            ))
            .unwrap();
        assert_eq!(
            resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap(),
            vec![("GITHUB_TOKEN".to_string(), "top-secret-value".to_string())],
            "an env-var-shaped inject_as is the injection target"
        );

        // HTTP injection styles are not env names → service-derived fallback.
        for style in ["bearer", "Authorization", "header:X-Custom-Auth"] {
            store
                .upsert_credential(&resolver_test_credential(
                    "cred_a",
                    "github",
                    "api-key-secret",
                    Some(style),
                ))
                .unwrap();
            assert_eq!(
                resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap(),
                vec![("GITHUB_SECRET".to_string(), "top-secret-value".to_string())],
                "HTTP injection style {style} must fall back to the service-derived env var"
            );
        }

        // Unsafe env names are never honored → service-derived fallback.
        store
            .upsert_credential(&resolver_test_credential(
                "cred_a",
                "github",
                "api-key-secret",
                Some("PATH"),
            ))
            .unwrap();
        assert_eq!(
            resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap(),
            vec![("GITHUB_SECRET".to_string(), "top-secret-value".to_string())],
            "unsafe inject_as must fall back to the service-derived env var"
        );
    }

    /// Multi-secret services: every credential stored for the service is
    /// injected under its own env-var name. The session-d1d8c2bb scenario —
 /// one NULL placeholder plus two properly named credentials — must yield
    /// all three env vars.
    #[test]
    #[serial_test::serial]
    fn resolve_credential_env_injects_every_credential_for_a_service() {
        let _guard = VaultKeyGuard::set_test_key();
        let (_temp, gateway_dir, agent_dir, store) = credential_resolver_fixture(
            &[
                ("app_password", "pw"),
                ("gmail_email", "u@gmail.com"),
                ("gmail_app_password", "pw"),
            ],
            r#"[{"service":"gmail"}]"#,
        );
        store
            .upsert_credential(&resolver_test_credential(
                "cred_old",
                "gmail",
                "app_password",
                None,
            ))
            .unwrap();
        store
            .upsert_credential(&resolver_test_credential(
                "cred_email",
                "gmail",
                "gmail_email",
                Some("GMAIL_EMAIL"),
            ))
            .unwrap();
        store
            .upsert_credential(&resolver_test_credential(
                "cred_pw",
                "gmail",
                "gmail_app_password",
                Some("GMAIL_APP_PASSWORD"),
            ))
            .unwrap();

        let mut resolved =
            resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap();
        resolved.sort();
        assert_eq!(
            resolved,
            vec![
                ("GMAIL_APP_PASSWORD".to_string(), "pw".to_string()),
                ("GMAIL_EMAIL".to_string(), "u@gmail.com".to_string()),
                ("GMAIL_SECRET".to_string(), "pw".to_string()),
            ],
            "all credentials for the service must be injected under their own names"
        );
    }

    /// Collision contract: a NULL placeholder and an explicit credential
    /// targeting the same env var — the explicit `inject_as` wins regardless
    /// of insertion order.
    #[test]
    #[serial_test::serial]
    fn resolve_credential_env_collision_explicit_shadows_fallback() {
        let _guard = VaultKeyGuard::set_test_key();
        let (_temp, gateway_dir, agent_dir, store) = credential_resolver_fixture(
            &[("old-secret", "old"), ("new-secret", "new")],
            r#"[{"service":"github"}]"#,
        );
        store
            .upsert_credential(&resolver_test_credential(
                "cred_old", "github", "old-secret", None,
            ))
            .unwrap();
        store
            .upsert_credential(&resolver_test_credential(
                "cred_new",
                "github",
                "new-secret",
                Some("GITHUB_SECRET"),
            ))
            .unwrap();
        assert_eq!(
            resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap(),
            vec![("GITHUB_SECRET".to_string(), "new".to_string())],
            "explicit inject_as must shadow the service-derived fallback"
        );
    }

    /// By-ID pinning: a runtime.lock `credential_id` mount resolves under the
    /// credential's own `inject_as` — the by-ID path previously always used
    /// the service-derived name (the session-d1d8c2bb bug).
    #[test]
    #[serial_test::serial]
    fn resolve_credential_env_by_id_honors_stored_inject_as() {
        let _guard = VaultKeyGuard::set_test_key();
        let (_temp, gateway_dir, agent_dir, store) = credential_resolver_fixture(
            &[("gmail_app_password", "pw")],
            r#"[{"service":"gmail","credential_id":"cred_pw"}]"#,
        );
        store
            .upsert_credential(&resolver_test_credential(
                "cred_pw",
                "gmail",
                "gmail_app_password",
                Some("GMAIL_APP_PASSWORD"),
            ))
            .unwrap();
        assert_eq!(
            resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap(),
            vec![("GMAIL_APP_PASSWORD".to_string(), "pw".to_string())],
            "a pinned credential must be injected under its stored inject_as"
        );
    }

    /// Fail-closed: a declared credential mount that resolves to zero env vars
    /// is a hard `credential_injection_failed` error, not a silent skip — the
    /// silent skip was the loop fuel in session-d1d8c2bb (script ran without
    /// its declared credentials and failed with cryptic missing-env errors).
    #[test]
    #[serial_test::serial]
    fn resolve_credential_env_fails_closed_on_unresolvable_declaration() {
        let _guard = VaultKeyGuard::set_test_key();

        // Declared service with no credential record at all → hard error.
        let (_temp, gateway_dir, agent_dir, store) =
            credential_resolver_fixture(&[], r#"[{"service":"github"}]"#);
        let err = resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("credential_injection_failed"),
            "expected a credential_injection_failed error, got: {msg}"
        );
        assert!(msg.contains("github"), "error must name the service: {msg}");

        // Pinned credential_id whose vault secret is missing → hard error.
        let (_temp2, gateway_dir2, agent_dir2, store2) = credential_resolver_fixture(
            &[],
            r#"[{"service":"gmail","credential_id":"cred_pw"}]"#,
        );
        store2
            .upsert_credential(&resolver_test_credential(
                "cred_pw",
                "gmail",
                "gmail_app_password",
                Some("GMAIL_APP_PASSWORD"),
            ))
            .unwrap();
        let err = resolve_credential_env(&agent_dir2, &gateway_dir2, &store2).unwrap_err();
        assert!(
            err.to_string().contains("credential_injection_failed"),
            "missing vault secret for a pinned credential must fail closed, got: {err}"
        );
    }

    /// Multi-field credentials are stored as a flat JSON object under the
    /// credential id. Injection must deliver the combined value under the
    /// credential's env-var name AND each field under `<SERVICE>_<FIELD>`,
    /// so scripts can consume either shape.
    #[test]
    #[serial_test::serial]
    fn resolve_credential_env_multi_field_blob_injects_each_field() {
        let _guard = VaultKeyGuard::set_test_key();
        let blob = r#"{"account_name":"acct-1","app-token":"tok-9"}"#;
        let (_temp, gateway_dir, agent_dir, store) = credential_resolver_fixture(
            &[("cred_multi", blob)],
            r#"[{"service":"photos"}]"#,
        );
        // NULL inject_as → combined value under the service-derived name.
        store
            .upsert_credential(&resolver_test_credential(
                "cred_multi",
                "photos",
                "cred_multi",
                None,
            ))
            .unwrap();

        let mut resolved = resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap();
        resolved.sort();
        assert_eq!(
            resolved,
            vec![
                ("PHOTOS_ACCOUNT_NAME".to_string(), "acct-1".to_string()),
                ("PHOTOS_APP_TOKEN".to_string(), "tok-9".to_string()),
                ("PHOTOS_SECRET".to_string(), blob.to_string()),
            ],
            "the combined value and each <SERVICE>_<FIELD> var must all be injected"
        );
    }

    /// An explicit env-var `inject_as` on a multi-field credential retargets
    /// the combined value; per-field vars still derive from the service.
    #[test]
    #[serial_test::serial]
    fn resolve_credential_env_multi_field_blob_honors_explicit_inject_as() {
        let _guard = VaultKeyGuard::set_test_key();
        let blob = r#"{"account_name":"acct-1","app_token":"tok-9"}"#;
        let (_temp, gateway_dir, agent_dir, store) = credential_resolver_fixture(
            &[("cred_multi", blob)],
            r#"[{"service":"photos","credential_id":"cred_multi"}]"#,
        );
        store
            .upsert_credential(&resolver_test_credential(
                "cred_multi",
                "photos",
                "cred_multi",
                Some("PHOTOS_LOGIN"),
            ))
            .unwrap();

        let mut resolved = resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap();
        resolved.sort();
        assert_eq!(
            resolved,
            vec![
                ("PHOTOS_ACCOUNT_NAME".to_string(), "acct-1".to_string()),
                ("PHOTOS_APP_TOKEN".to_string(), "tok-9".to_string()),
                ("PHOTOS_LOGIN".to_string(), blob.to_string()),
            ],
            "explicit inject_as retargets the combined value; fields keep the service prefix"
        );
    }

    /// Non-flat secrets (nested objects, arrays, scalars that happen to be
    /// JSON) are injected only under the primary name — no field expansion.
    #[test]
    #[serial_test::serial]
    fn resolve_credential_env_non_flat_secret_gets_no_field_expansion() {
        let _guard = VaultKeyGuard::set_test_key();
        for secret in [
            r#"{"outer":{"inner":"v"}}"#,
            r#"["a","b"]"#,
            r#""just-a-string""#,
            r#"{"ok": "v", "count": 3}"#,
            r#"{}"#,
        ] {
            let (_temp, gateway_dir, agent_dir, store) = credential_resolver_fixture(
                &[("cred_json", secret)],
                r#"[{"service":"photos"}]"#,
            );
            store
                .upsert_credential(&resolver_test_credential(
                    "cred_json", "photos", "cred_json", None,
                ))
                .unwrap();
            assert_eq!(
                resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap(),
                vec![("PHOTOS_SECRET".to_string(), secret.to_string())],
                "secret {secret} must not get per-field expansion"
            );
        }
    }

    /// An explicit `inject_as` that collides with one of the credential's own
    /// derived field names must keep the combined value — the primary name is
    /// the injection contract, so the field var is the one that yields.
    #[test]
    #[serial_test::serial]
    fn resolve_credential_env_explicit_inject_as_wins_over_own_field_var() {
        let _guard = VaultKeyGuard::set_test_key();
        let blob = r#"{"account_name":"acct-1","app_token":"tok-9"}"#;
        let (_temp, gateway_dir, agent_dir, store) = credential_resolver_fixture(
            &[("cred_multi", blob)],
            r#"[{"service":"photos","credential_id":"cred_multi"}]"#,
        );
        // inject_as is exactly the env var the `account_name` field derives.
        store
            .upsert_credential(&resolver_test_credential(
                "cred_multi",
                "photos",
                "cred_multi",
                Some("PHOTOS_ACCOUNT_NAME"),
            ))
            .unwrap();

        let mut resolved = resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap();
        resolved.sort();
        assert_eq!(
            resolved,
            vec![
                ("PHOTOS_ACCOUNT_NAME".to_string(), blob.to_string()),
                ("PHOTOS_APP_TOKEN".to_string(), "tok-9".to_string()),
            ],
            "the combined value must survive a collision with its own field var"
        );
    }

    /// A field name with no usable characters is skipped; the combined value
    /// and other fields still inject.
    #[test]
    #[serial_test::serial]
    fn resolve_credential_env_skips_unnameable_fields() {
        let _guard = VaultKeyGuard::set_test_key();
        let blob = r#"{"!!!":"v0","token":"t1"}"#;
        let (_temp, gateway_dir, agent_dir, store) = credential_resolver_fixture(
            &[("cred_multi", blob)],
            r#"[{"service":"photos"}]"#,
        );
        store
            .upsert_credential(&resolver_test_credential(
                "cred_multi",
                "photos",
                "cred_multi",
                None,
            ))
            .unwrap();

        let mut resolved = resolve_credential_env(&agent_dir, &gateway_dir, &store).unwrap();
        resolved.sort();
        assert_eq!(
            resolved,
            vec![
                ("PHOTOS_SECRET".to_string(), blob.to_string()),
                ("PHOTOS_TOKEN".to_string(), "t1".to_string()),
            ],
            "unnameable fields are skipped without losing the combined value"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::prepare_runtime_lock_layer_mounts;
        use autonoetic_types::layer::ArtifactLayer;

        #[test]
        fn runtime_lock_layers_add_mounts_and_pythonpath_entries() {
            let temp = tempfile::tempdir().expect("tempdir should create");
            let gateway_dir = temp.path().join(".gateway");
            let agent_dir = temp.path().join("agent");
            let layer_src = temp.path().join("layer-src");

            std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
            std::fs::create_dir_all(&agent_dir).expect("agent dir should create");
            std::fs::create_dir_all(&layer_src).expect("layer source should create");
            std::fs::write(layer_src.join("depmod.py"), "VALUE = 1\n")
                .expect("layer file should write");

            let layer_store =
                crate::layer_store::LayerStore::new(&gateway_dir, Default::default()).unwrap();
            let captured = layer_store
                .create_from_dir(&layer_src, "python-deps", "/tmp/venv", None)
                .expect("layer should capture");

            let runtime_lock = crate::runtime::install_contract::scaffold_runtime_lock(
                None,
                None,
                &[ArtifactLayer {
                    layer_id: captured.layer_id.clone(),
                    name: captured.name.clone(),
                    mount_path: captured.mount_path.clone(),
                    digest: captured.digest.clone(),
                }],
            )
            .expect("runtime lock should scaffold");
            let runtime_lock_yaml = serde_yaml::to_string(&runtime_lock).expect("runtime lock yaml");
            std::fs::write(agent_dir.join("runtime.lock"), runtime_lock_yaml)
                .expect("runtime lock should write");

            let (mounts, python_paths, node_paths) =
                prepare_runtime_lock_layer_mounts(&agent_dir, "runtime.lock", Some(&gateway_dir))
                    .expect("runtime lock layers should resolve");

            assert_eq!(mounts.len(), 1);
            assert_eq!(mounts[0].dest, "/tmp/venv");
            assert!(python_paths.contains(&"/tmp/venv".to_string()));
            assert!(node_paths.is_empty());
        }

        #[test]
        fn runtime_lock_layers_detect_python_version_dynamically() {
            let temp = tempfile::tempdir().expect("tempdir should create");
            let gateway_dir = temp.path().join(".gateway");
            let agent_dir = temp.path().join("agent");
            let layer_src = temp.path().join("layer-src");

            std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
            std::fs::create_dir_all(&agent_dir).expect("agent dir should create");
            std::fs::create_dir_all(layer_src.join("lib").join("python3.13").join("site-packages"))
                .expect("site-packages should create");
            std::fs::write(
                layer_src.join("lib").join("python3.13").join("site-packages").join("depmod.py"),
                "VALUE = 1\n",
            )
            .expect("layer file should write");

            let layer_store =
                crate::layer_store::LayerStore::new(&gateway_dir, Default::default()).unwrap();
            let captured = layer_store
                .create_from_dir(&layer_src, "python-deps", "/tmp/venv", None)
                .expect("layer should capture");

            let runtime_lock = crate::runtime::install_contract::scaffold_runtime_lock(
                None,
                None,
                &[ArtifactLayer {
                    layer_id: captured.layer_id.clone(),
                    name: captured.name.clone(),
                    mount_path: captured.mount_path.clone(),
                    digest: captured.digest.clone(),
                }],
            )
            .expect("runtime lock should scaffold");
            let runtime_lock_yaml = serde_yaml::to_string(&runtime_lock).expect("runtime lock yaml");
            std::fs::write(agent_dir.join("runtime.lock"), runtime_lock_yaml)
                .expect("runtime lock should write");

            let (mounts, python_paths, node_paths) =
                prepare_runtime_lock_layer_mounts(&agent_dir, "runtime.lock", Some(&gateway_dir))
                    .expect("runtime lock layers should resolve");

            assert_eq!(mounts.len(), 1);
            assert!(python_paths.contains(&"/tmp/venv/lib/python3.13/site-packages".to_string()));
            assert!(python_paths.contains(&"/tmp/venv".to_string()));
            assert!(node_paths.is_empty());
        }
    }

    #[test]
    fn normalize_script_input_payload_extracts_task_block() {
        let metadata = serde_json::json!({
            "delegated_role": "weather.forecast",
            "reply_to_agent_id": "planner.default"
        });
        let task = r#"{"location":"Paris, France","date":"tomorrow"}"#;
        let kickoff = format!(
            "[Context]\nVerify the agent.\n\n[Task]\n{task}\n\n[Metadata]\n{}",
            metadata
        );
        assert_eq!(
            normalize_script_input_payload(&kickoff, Some(&metadata)),
            task
        );
    }

    #[test]
    fn script_stdout_surfaces_as_agent_message_on_timeline() {
        use crate::scheduler::gateway_store::GatewayStore;
        use autonoetic_types::session_timeline::Altitude;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();

        // A child script session under root "root-1": base_session_id splits on
        // '/', so the row lands under the root the room TUI renders.
        emit_script_message_timeline(
            Some(&store),
            "weather.default",
            "root-1/weather.default-abc",
            "toto\n",
        );

        let page = store
            .list_session_timeline("root-1", None, 10, None, None)
            .unwrap();
        assert_eq!(page.entries.len(), 1, "stdout should emit one timeline row");
        let row = &page.entries[0];
        assert_eq!(row.event_type, "agent.message");
        assert_eq!(row.altitude, Altitude::Normal, "must show at the default floor");
        assert_eq!(row.source_session_id, "root-1/weather.default-abc");
        let payload: serde_json::Value =
            serde_json::from_str(row.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["message"], "toto", "trailing newline is trimmed");
    }

    #[test]
    fn script_empty_stdout_emits_no_timeline_row() {
        use crate::scheduler::gateway_store::GatewayStore;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        emit_script_message_timeline(Some(&store), "a.default", "root-2/a", "   \n  ");
        let page = store
            .list_session_timeline("root-2", None, 10, None, None)
            .unwrap();
        assert!(
            page.entries.is_empty(),
            "empty/whitespace-only stdout should not emit a row"
        );
    }

    #[test]
    fn script_stdout_timeline_row_is_capped() {
        use crate::runtime::session_tracer::TIMELINE_AGENT_NARRATIVE_MAX_CHARS;
        use crate::scheduler::gateway_store::GatewayStore;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        let big = "x".repeat(TIMELINE_AGENT_NARRATIVE_MAX_CHARS + 5_000);
        emit_script_message_timeline(Some(&store), "a.default", "root-3/a", &big);

        let page = store
            .list_session_timeline("root-3", None, 10, None, None)
            .unwrap();
        let row = &page.entries[0];
        let payload: serde_json::Value =
            serde_json::from_str(row.payload.as_deref().unwrap()).unwrap();
        let msg = payload["message"].as_str().unwrap();
        assert_eq!(
            msg.chars().count(),
            TIMELINE_AGENT_NARRATIVE_MAX_CHARS,
            "timeline mirrors a capped preview; full stdout lives in execution_traces"
        );
        assert!(msg.ends_with('…'));
    }

    #[test]
    fn script_stdout_timeline_row_redacts_embedded_secrets() {
        use crate::scheduler::gateway_store::GatewayStore;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        // A Bearer token in the script output must be masked before it reaches
        // the timeline surface (`redact_embedded_secrets`, same as agent.message).
        let stdout = "downloaded using Authorization: Bearer sk-live-abc123secret then exited";
        emit_script_message_timeline(Some(&store), "a.default", "root-4/a", stdout);

        let page = store
            .list_session_timeline("root-4", None, 10, None, None)
            .unwrap();
        let row = &page.entries[0];
        let payload: serde_json::Value =
            serde_json::from_str(row.payload.as_deref().unwrap()).unwrap();
        let msg = payload["message"].as_str().unwrap();
        assert!(
            !msg.contains("sk-live-abc123secret"),
            "raw secret must not reach the timeline; got: {msg}"
        );
        assert!(
            msg.contains("***REDACTED***"),
            "bearer token must be masked; got: {msg}"
        );
    }
}
