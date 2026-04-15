//! Reevaluation State Management.
//!
//! Helpers for persisting and loading agent reevaluation state.

use crate::policy::PolicyEngine;
use crate::runtime::tools::NativeToolRegistry;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::{ReevaluationState, ScheduledAction};
use std::path::Path;

pub fn reevaluation_state_path(agent_dir: &Path) -> std::path::PathBuf {
    agent_dir.join("state").join("reevaluation.json")
}

pub fn load_reevaluation_state(agent_dir: &Path) -> anyhow::Result<ReevaluationState> {
    let path = reevaluation_state_path(agent_dir);
    if !path.exists() {
        return Ok(ReevaluationState::default());
    }
    let body = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&body)?)
}

pub fn persist_reevaluation_state<F>(
    agent_dir: &Path,
    mutate: F,
) -> anyhow::Result<ReevaluationState>
where
    F: FnOnce(&mut ReevaluationState),
{
    let mut state = load_reevaluation_state(agent_dir)?;
    mutate(&mut state);
    let path = reevaluation_state_path(agent_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&state)?)?;
    Ok(state)
}

pub fn execute_scheduled_action(
    manifest: &AgentManifest,
    agent_dir: &Path,
    action: &ScheduledAction,
    registry: &NativeToolRegistry,
    _config: Option<&autonoetic_types::config::GatewayConfig>,
    gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
) -> anyhow::Result<String> {
    let policy = PolicyEngine::new(manifest.clone());
    match action {
        ScheduledAction::AgentInstall { agent_id, .. } => anyhow::bail!(
            "Legacy scheduled action AgentInstall for '{}' is no longer executable: agent.install has been removed.",
            agent_id
        ),
        ScheduledAction::WriteFile { path, content, .. } => {
            anyhow::ensure!(
                !path.trim().is_empty(),
                "scheduled file path must not be empty"
            );
            anyhow::ensure!(
                !path.starts_with('/') && !path.split('/').any(|part| part == ".."),
                "scheduled file path must stay within the agent directory"
            );
            anyhow::ensure!(
                policy.can_write_path(path),
                "scheduled file write denied by WriteAccess policy"
            );
            let target = agent_dir.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, content)?;
            serde_json::to_string(
                &serde_json::json!({ "ok": true, "path": path, "bytes_written": content.len() }),
            )
            .map_err(Into::into)
        }
        ScheduledAction::SandboxExec {
            command,
            dependencies,
            ..
        } => {
            let args = serde_json::to_string(&serde_json::json!({
                "command": command,
                "dependencies": dependencies.as_ref().map(|deps| serde_json::json!({ "runtime": deps.runtime, "packages": deps.packages }))
            }))?;
            let result = registry.execute(
                "sandbox.exec",
                manifest,
                &policy,
                agent_dir,
                None,
                &args,
                None,
                None,
                _config,
                gateway_store,
                None,
            )?;

            let parsed: serde_json::Value = serde_json::from_str(&result).map_err(|error| {
                anyhow::anyhow!("sandbox.exec returned non-JSON result: {error}")
            })?;

            let ok = parsed
                .get("ok")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            anyhow::ensure!(
                ok,
                "scheduled sandbox_exec failed: {}",
                parsed
                    .get("stderr")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown error")
            );

            Ok(result)
        }
        ScheduledAction::CredentialPrompt { .. } => anyhow::bail!(
            "CredentialPrompt is not directly executable; secrets must be provided through the approval channel"
        ),
        ScheduledAction::SessionContinue { .. } => anyhow::bail!(
            "SessionContinue is not directly executable; it only gates session continuation by approval"
        ),
        ScheduledAction::ProfileShare { .. } => anyhow::bail!(
            "ProfileShare is not directly executable; bindings are created after approval"
        ),
        ScheduledAction::SessionEscalate { .. } => anyhow::bail!(
            "SessionEscalate is not directly executable; it only gates session continuation by operator approval"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};

    fn minimal_manifest() -> AgentManifest {
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
                id: "caller-agent".to_string(),
                name: "caller-agent".to_string(),
                description: "Test".to_string(),
            },
            capabilities: vec![],
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,

            response_contract: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
        }
    }

    /// Regression: AgentInstall is not executable by the scheduler.
    #[test]
    fn test_agent_install_in_background_path_is_rejected() {
        let action = ScheduledAction::AgentInstall {
            agent_id: "would-be-child".to_string(),
            summary: "Test install".to_string(),
            requested_by_agent_id: "caller-agent".to_string(),
            install_fingerprint: "abc123".to_string(),
            payload: None,
        };
        assert!(
            !action.is_executable_by_scheduler(),
            "AgentInstall must not be considered executable by the scheduler"
        );

        let manifest = minimal_manifest();
        let temp = tempfile::tempdir().expect("tempdir");
        let agent_dir = temp.path();
        let registry = crate::runtime::tools::default_registry();

        let err = execute_scheduled_action(&manifest, agent_dir, &action, &registry, None, None)
            .expect_err("execute_scheduled_action(AgentInstall) must fail");
        assert!(
            err.to_string().contains("no longer executable") || err.to_string().contains("removed"),
            "unexpected error: {}",
            err
        );

        // No install must have occurred: no new agent directory under agent_dir
        let state_dir = agent_dir.join("state");
        let skills_dir = agent_dir.join("skills");
        assert!(!state_dir.exists(), "AgentInstall must not create state");
        assert!(!skills_dir.exists(), "AgentInstall must not create skills");
        assert!(
            std::fs::read_dir(agent_dir).map(|d| d.count()).unwrap_or(0) == 0,
            "agent_dir must remain empty; no install side-effects"
        );
    }
}
