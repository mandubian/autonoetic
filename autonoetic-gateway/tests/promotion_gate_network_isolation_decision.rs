//! Promotion-gate network-isolation decision (P-3.10).
//!
//! A promotion-verdict role (e.g. `unit_test_runner.default`) that runs on a
//! driver enforcing `force_network_off` must NOT be statically pre-denied just
//! because `RemoteAccessAnalyzer` detects a network import (e.g. `import urllib`).
//! The deterministic suite runs inside the physically network-isolated sandbox:
//! a suite that mocks the HTTP caller passes; a suite that genuinely reaches the
//! network fails at runtime and the role reports `unable_to_evaluate`.
//!
//! These tests pin the decision predicate so the false-deny regression (a
//! mocked service importing `urllib` blocked from promotion) cannot return.
//! They are pure (no sandbox), so they run in CI without bubblewrap.

use autonoetic_gateway::runtime::tools::artifact_exec::promotion_run_is_network_isolated;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;

fn manifest(agent_id: &str, sandbox: &str) -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: sandbox.to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "Test agent".to_string(),
        },
        capabilities: vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
        llm_overrides: None,
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

#[test]
fn unit_test_runner_on_bubblewrap_runs_isolated_not_predenied() {
    // The mocked-urllib false-deny case: unit_test_runner on bubblewrap is
    // physically network-off, so detections are informational, not a pre-deny.
    assert!(
        promotion_run_is_network_isolated(&manifest("unit_test_runner.default", "bubblewrap")),
        "unit_test_runner on bubblewrap must run in the isolated sandbox, not be pre-denied"
    );
}

#[test]
fn all_promotion_verdict_roles_isolated_on_bubblewrap() {
    for role in [
        "unit_test_runner.default",
        "sealed_evaluator.default",
        "static_evaluator.default",
        "auditor.default",
    ] {
        assert!(
            promotion_run_is_network_isolated(&manifest(role, "bubblewrap")),
            "{role} must run network-isolated on bubblewrap"
        );
    }
}

#[test]
fn non_isolating_drivers_keep_predeny_for_promotion_roles() {
    // docker/microvm/wasm do not honor force_network_off, so the
    // deterministic-without-network pre-deny (P-3.10) must be preserved.
    for driver in ["docker", "microvm", "wasm"] {
        assert!(
            !promotion_run_is_network_isolated(&manifest("unit_test_runner.default", driver)),
            "driver {driver} cannot guarantee network-off; pre-deny must be kept"
        );
    }
}

#[test]
fn non_promotion_role_is_not_treated_as_isolated_promotion_run() {
    // A plain executor is not a promotion-verdict role: this predicate is false
    // (its network handling goes through the normal operator-approval path).
    assert!(
        !promotion_run_is_network_isolated(&manifest("executor.default", "bubblewrap")),
        "non-promotion roles are not promotion-isolated runs"
    );
}
