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
use autonoetic_gateway::sandbox::{BwrapIsolationOverrides, SandboxDriverKind};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;

#[test]
fn driver_network_off_guarantee_truth_table() {
    let gate = BwrapIsolationOverrides::promotion_gate_overrides();
    let normal = BwrapIsolationOverrides::default();

    // Bubblewrap: offline only when force_network_off is set.
    assert!(SandboxDriverKind::Bubblewrap.guarantees_network_off(&gate));
    assert!(!SandboxDriverKind::Bubblewrap.guarantees_network_off(&normal));

    // Docker (`--network none`) and wasm (WASI, no sockets) are always offline.
    assert!(SandboxDriverKind::Docker.guarantees_network_off(&gate));
    assert!(SandboxDriverKind::Docker.guarantees_network_off(&normal));
    assert!(SandboxDriverKind::Wasm.guarantees_network_off(&gate));
    assert!(SandboxDriverKind::Wasm.guarantees_network_off(&normal));

    // MicroVm: operator firecracker config controls the NIC — never asserted.
    assert!(!SandboxDriverKind::MicroVm.guarantees_network_off(&gate));
    assert!(!SandboxDriverKind::MicroVm.guarantees_network_off(&normal));
}

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
            singleton: false,
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
            excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
            open_web: false,
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
fn drivers_that_guarantee_offline_also_run_isolated() {
    // docker (`--network none`) and wasm (WASI, no sockets) are always offline,
    // so promotion-verdict roles on them must ALSO run rather than be pre-denied.
    for driver in ["docker", "wasm"] {
        assert!(
            promotion_run_is_network_isolated(&manifest("unit_test_runner.default", driver)),
            "driver {driver} guarantees network-off; the suite must run, not be pre-denied"
        );
    }
}

#[test]
fn microvm_keeps_predeny_for_promotion_roles() {
    // microvm's NIC is controlled by the operator firecracker --config-file; the
    // gateway cannot assert it is offline, so the deterministic-without-network
    // pre-deny (P-3.10) must be preserved.
    assert!(
        !promotion_run_is_network_isolated(&manifest("unit_test_runner.default", "microvm")),
        "microvm cannot guarantee network-off; pre-deny must be kept"
    );
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
