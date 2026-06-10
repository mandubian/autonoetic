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

fn prepare_runtime_lock_layer_mounts(
    agent_dir: &Path,
    runtime_lock_rel_path: &str,
    gateway_dir: Option<&Path>,
) -> anyhow::Result<(Vec<crate::sandbox::SandboxMount>, Vec<String>)> {
    let Some(gw_dir) = gateway_dir else {
        return Ok((Vec::new(), Vec::new()));
    };

    let lock_path = agent_dir.join(runtime_lock_rel_path);
    if !lock_path.exists() {
        return Ok((Vec::new(), Vec::new()));
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
            return Ok((Vec::new(), Vec::new()));
        }
    };

    if parsed_lock.layers.is_empty() {
        return Ok((Vec::new(), Vec::new()));
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
    crate::runtime::tools::sandbox::extract_and_mount_layers(
        &lock_layers,
        gw_dir,
        "runtime.lock",
        &mut mounts,
        &mut python_paths,
    )?;

    Ok((mounts, python_paths))
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
    let invocation_files = write_script_invocation_files(agent_dir, &normalized_input, metadata)?;
    let entrypoint_relative = match script_path.strip_prefix(agent_dir) {
        Ok(relative) => format!(
            "{}/{}",
            crate::sandbox::BWRAP_WORKSPACE_DIR,
            relative.to_string_lossy()
        ),
        Err(_) => script_path.to_string_lossy().to_string(),
    };

    // Script-mode is intent-based exec: run the declared entry file. `language:
    // None` execs it directly (shebang-driven), matching prior behavior; the
    // Process backend renders it back to a shell line.
    let exec_kind = crate::exec_request::ExecutionKind::Code {
        language: None,
        source: crate::exec_request::CodeSource::Entry(entrypoint_relative),
        args: match input_mode {
            autonoetic_types::agent::ScriptInputMode::Args => vec![normalized_input.clone()],
            autonoetic_types::agent::ScriptInputMode::Stdin => vec![],
        },
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

    let (runtime_lock_mounts, layer_python_paths) =
        prepare_runtime_lock_layer_mounts(agent_dir, runtime_lock_rel_path, gateway_dir)?;
    if !layer_python_paths.is_empty() {
        autonoetic_env.push(("PYTHONPATH".to_string(), layer_python_paths.join(":")));
    }

    let mut runner = match crate::sandbox::SandboxRunner::spawn_with_session_content_and_env(
        driver,
        &agent_dir.to_string_lossy(),
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
        anyhow::bail!(
            "Script execution failed with code {:?}: stdout={}, stderr={}",
            output.status.code(),
            stdout,
            stderr
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    tracing::info!(stdout_len = stdout.len(), "Script execution completed");

    Ok(stdout)
}

/// Write a single `causal_events` row for script-agent fast-path execution.
/// Called in place of the former no-op `log_gateway_causal_event` so that
/// script agent runs are visible in `execution.search` and session_overview.
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

/// Resolve credential env vars from a runtime.lock's `credentials` section.
///
/// For each `LockedCredentialMount`, looks up credentials by service name in
/// the store, derives the env-var name via `inject_as_for_service()`, and
/// resolves the secret from the vault. Returns `(env_var, secret_value)` pairs
/// ready to inject into the sandbox environment.
///
/// `spawn_bindings` provides credential overrides from `agent_spawn` — entries
/// here take precedence over runtime.lock entries for the same service.
///
/// Failures are logged and skipped — a missing credential should not block
/// the agent spawn (the credential may not be needed this session).
pub(crate) fn resolve_credential_env(
    agent_dir: &Path,
    gateway_dir: &Path,
    store: &crate::scheduler::gateway_store::GatewayStore,
) -> Vec<(String, String)> {
    resolve_credential_env_with_bindings(agent_dir, gateway_dir, store, &[])
}

/// Like [`resolve_credential_env`] but accepts spawn-time credential bindings
/// that override runtime.lock entries for matching services.
pub(crate) fn resolve_credential_env_with_bindings(
    agent_dir: &Path,
    gateway_dir: &Path,
    store: &crate::scheduler::gateway_store::GatewayStore,
    spawn_bindings: &[autonoetic_types::runtime_lock::LockedCredentialMount],
) -> Vec<(String, String)> {
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
                return vec![];
            }
        },
        Err(_) => return vec![],
    };

    if lock.credentials.is_empty() && spawn_bindings.is_empty() {
        return vec![];
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

    let vault_dir = gateway_dir.parent().unwrap_or(gateway_dir);
    if crate::vault::ensure_default_key(vault_dir).is_err() {
        tracing::warn!(target: "script_execute", "Failed to ensure vault key; skipping credential resolution");
        return vec![];
    }
    let vault_path = crate::vault::default_vault_path(vault_dir);
    let vault = match crate::vault::Vault::load_from_file(&vault_path) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(target: "script_execute", error = %e, "Failed to load vault; skipping credential resolution");
            return vec![];
        }
    };

    let mut resolved = Vec::new();
    for cm in &merged {
        let env_var = autonoetic_types::runtime_lock::inject_as_for_service(&cm.service);

        // If a specific credential_id is declared in runtime.lock, resolve directly.
        if let Some(ref cred_id) = cm.credential_id {
            match store.get_credential(cred_id) {
                Ok(Some(cred)) => {
                    if let Some(secret) = vault.get_secret(&cred.secret_name) {
                        tracing::info!(
                            target: "script_execute",
                            service = %cm.service,
                            credential_id = %cred_id,
                            env_var = %env_var,
                            "Resolved credential by ID for script agent"
                        );
                        resolved.push((env_var, secret.expose_secret().to_string()));
                    } else {
                        tracing::warn!(
                            target: "script_execute",
                            service = %cm.service,
                            credential_id = %cred_id,
                            secret_name = %cred.secret_name,
                            "Secret not found in vault for pinned credential_id"
                        );
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

        // Fallback: resolve by service name (first match).
        let creds = match store.list_credentials_by_service(&cm.service) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "script_execute",
                    service = %cm.service,
                    error = %e,
                    "Failed to list credentials for service"
                );
                continue;
            }
        };
        let matched = creds
            .iter()
            .filter(|c| c.inject_as.as_deref() == Some(&env_var))
            .collect::<Vec<_>>();
        let cred = match matched.len() {
            0 => {
                tracing::warn!(
                    target: "script_execute",
                    service = %cm.service,
                    env_var = %env_var,
                    "No credential found for service with matching inject_as; skipping"
                );
                continue;
            }
            1 => matched[0],
            _ => {
                tracing::warn!(
                    target: "script_execute",
                    service = %cm.service,
                    env_var = %env_var,
                    count = matched.len(),
                    "Multiple credentials found for service+env_var; using first match. Pin a credential_id in runtime.lock to disambiguate."
                );
                matched[0]
            }
        };
        match vault.get_secret(&cred.secret_name) {
            Some(secret) => {
                tracing::info!(
                    target: "script_execute",
                    service = %cm.service,
                    env_var = %env_var,
                    "Resolved credential for script agent"
                );
                resolved.push((env_var, secret.expose_secret().to_string()));
            }
            None => {
                tracing::warn!(
                    target: "script_execute",
                    service = %cm.service,
                    secret_name = %cred.secret_name,
                    "Secret not found in vault; skipping credential injection"
                );
            }
        }
    }
    resolved
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

            let (mounts, python_paths) =
                prepare_runtime_lock_layer_mounts(&agent_dir, "runtime.lock", Some(&gateway_dir))
                    .expect("runtime lock layers should resolve");

            assert_eq!(mounts.len(), 1);
            assert_eq!(mounts[0].dest, "/tmp/venv");
            assert!(python_paths.contains(&"/tmp/venv".to_string()));
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

            let (mounts, python_paths) =
                prepare_runtime_lock_layer_mounts(&agent_dir, "runtime.lock", Some(&gateway_dir))
                    .expect("runtime lock layers should resolve");

            assert_eq!(mounts.len(), 1);
            assert!(python_paths.contains(&"/tmp/venv/lib/python3.13/site-packages".to_string()));
            assert!(python_paths.contains(&"/tmp/venv".to_string()));
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
}
