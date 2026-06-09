//! Constitution Phase 4.6 pin: manifest-declared loop-guard limits are honored
//! as stricter bounds within gateway system ceilings.

use std::sync::Arc;

use autonoetic_gateway::llm::{CompletionRequest, CompletionResponse, LlmDriver};
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::{GatewayConfig, LoopGuardConfig};
use tempfile::tempdir;

struct NoopLlm;

#[async_trait::async_trait]
impl LlmDriver for NoopLlm {
    async fn complete(&self, _request: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse::text_only("{}".to_string()))
    }
}

fn manifest(agent_id: &str) -> AgentManifest {
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
            description: "test agent".to_string(),
        },
        capabilities: vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
        llm_preset: None,
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
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn write_skill_with_loop_guard(
    agent_dir: &std::path::Path,
    loop_guard_yaml: &str,
) -> anyhow::Result<()> {
    let skill = format!(
        r#"---
metadata:
  autonoetic:
    loop_guard:
{}
---
# Test
"#,
        loop_guard_yaml
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill)?;
    Ok(())
}

fn write_skill_without_loop_guard(agent_dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::write(
        agent_dir.join("SKILL.md"),
        r#"---
metadata:
  autonoetic:
    version: "1.0"
---
# Test
"#,
    )?;
    Ok(())
}

#[test]
fn declared_loop_guard_values_are_applied_when_stricter() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    write_skill_with_loop_guard(
        tmp.path(),
        r#"      max_loops_without_progress: 2
      max_tool_failures: 1
      max_consecutive_same_progress: 1
      max_child_failures: 2"#,
    )?;

    let cfg = GatewayConfig {
        loop_guard: LoopGuardConfig {
            max_loops_without_progress: 5,
            max_tool_failures: 5,
            max_consecutive_same_progress: 3,
            max_child_failures: 4,
            ..Default::default()
        },
        ..GatewayConfig::default()
    };

    let exec = AgentExecutor::new(
        manifest("loop-guard.strict.default"),
        "test".to_string(),
        Arc::new(NoopLlm),
        tmp.path().to_path_buf(),
        default_registry(),
        None,
    )
    .with_config(Arc::new(cfg));

    let snap = exec.guard.snapshot();
    assert_eq!(snap.max_loops_without_progress, 2);
    assert_eq!(snap.max_tool_failures, 1);
    assert_eq!(snap.max_consecutive_same_progress, 1);
    assert_eq!(snap.max_child_failures, 2);
    Ok(())
}

#[test]
fn declared_loop_guard_values_are_capped_by_system_ceiling() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    write_skill_with_loop_guard(
        tmp.path(),
        r#"      max_loops_without_progress: 50
      max_tool_failures: 50
      max_consecutive_same_progress: 50
      max_child_failures: 50"#,
    )?;

    let cfg = GatewayConfig {
        loop_guard: LoopGuardConfig {
            max_loops_without_progress: 5,
            max_tool_failures: 5,
            max_consecutive_same_progress: 2,
            max_child_failures: 3,
            ..Default::default()
        },
        ..GatewayConfig::default()
    };

    let exec = AgentExecutor::new(
        manifest("loop-guard.capped.default"),
        "test".to_string(),
        Arc::new(NoopLlm),
        tmp.path().to_path_buf(),
        default_registry(),
        None,
    )
    .with_config(Arc::new(cfg));

    let snap = exec.guard.snapshot();
    assert_eq!(snap.max_loops_without_progress, 5);
    assert_eq!(snap.max_tool_failures, 5);
    assert_eq!(snap.max_consecutive_same_progress, 2);
    assert_eq!(snap.max_child_failures, 3);
    Ok(())
}

#[test]
fn absent_manifest_declaration_uses_system_loop_guard_limits() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    write_skill_without_loop_guard(tmp.path())?;

    let cfg = GatewayConfig {
        loop_guard: LoopGuardConfig {
            max_loops_without_progress: 6,
            max_tool_failures: 4,
            max_consecutive_same_progress: 2,
            max_child_failures: 5,
            ..Default::default()
        },
        ..GatewayConfig::default()
    };

    let exec = AgentExecutor::new(
        manifest("loop-guard.default.default"),
        "test".to_string(),
        Arc::new(NoopLlm),
        tmp.path().to_path_buf(),
        default_registry(),
        None,
    )
    .with_config(Arc::new(cfg));

    let snap = exec.guard.snapshot();
    assert_eq!(snap.max_loops_without_progress, 6);
    assert_eq!(snap.max_tool_failures, 4);
    assert_eq!(snap.max_consecutive_same_progress, 2);
    assert_eq!(snap.max_child_failures, 5);
    Ok(())
}
