//! Full-sandbox e2e for the promotion-gate network-isolation fix (P-3.10).
//!
//! These tests are `#[ignore]`d because they require a working **bubblewrap**
//! (`bwrap`) on the host and actually execute Python in the sandbox — they are
//! NOT run in CI. The CI-safe decision coverage lives in
//! `promotion_gate_network_isolation_decision.rs`; this file proves the
//! end-to-end runtime behavior that CI cannot.
//!
//! Run locally with:
//! ```bash
//! cargo test -p autonoetic-gateway --test promotion_gate_mocked_network_e2e -- --ignored --nocapture
//! ```
//!
//! What they prove (the original bug + its boundary):
//! 1. `unit_test_runner.default` runs a suite that imports `urllib` but **mocks**
//!    the HTTP caller. The gate no longer statically pre-denies on import
//!    detection: the suite runs in the network-off bubblewrap sandbox and
//!    **passes** (exit 0, no `promotion_gate_network_denied`). This is the
//!    false-deny the fix removes.
//! 2. A suite that makes a **real** network call (no mock) still runs, but fails
//!    at runtime inside the offline sandbox (URLError/ConnectionError → non-zero
//!    exit), which the verdict role maps to `unable_to_evaluate`.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;

fn base_manifest(id: &str, name: &str, capabilities: Vec<Capability>) -> AgentManifest {
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
            id: id.to_string(),
            name: name.to_string(),
            description: "test agent".to_string(),
        },
        capabilities,
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
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

/// Writer (needs WriteAccess) used to mint the artifact_ref.
fn writer_manifest() -> AgentManifest {
    base_manifest(
        "coder.default",
        "coder",
        vec![Capability::WriteAccess {
            scopes: vec!["*".to_string()],
        }],
    )
}

/// `unit_test_runner.default` — promotion federation exec gate on bubblewrap with
/// no `CodeExecution` / `Evaluation` caps (gateway grants `artifact_exec` by role
/// + `SandboxFunctions` allowlist).
fn unit_test_runner_manifest() -> AgentManifest {
    base_manifest(
        "unit_test_runner.default",
        "Unit Test Runner",
        vec![
            Capability::SandboxFunctions {
                allowed: vec![
                    "knowledge_".to_string(),
                    "artifact_inspect".to_string(),
                    "artifact_exec".to_string(),
                    "promotion_".to_string(),
                ],
            },
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
        ],
    )
}

/// Build an artifact from the given (filename, content) files in `session`, run
/// `entrypoint` through `artifact_exec` as the unit_test_runner, and return the
/// parsed response JSON.
fn build_and_exec(
    session: &str,
    files: &[(&str, &str)],
    entrypoint: &str,
) -> serde_json::Value {
    let temp = tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir");
    let writer_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&writer_dir).expect("writer dir");
    let runner_dir = agents_dir.join("unit_test_runner.default");
    std::fs::create_dir_all(&runner_dir).expect("runner dir");

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).expect("gateway store"));
    let writer = writer_manifest();
    let writer_policy = PolicyEngine::new(writer.clone());
    let runner = unit_test_runner_manifest();
    let runner_policy = PolicyEngine::new(runner.clone());
    let registry = default_registry();

    // Register the artifact files in the content store, then mint a ref.
    let cs = ContentStore::new(&gateway_dir).expect("content store");
    let inputs: Vec<String> = files
        .iter()
        .map(|(name, content)| {
            let h = cs.write(content.as_bytes()).expect("cs write");
            cs.register_name(session, name, &h).expect("register name");
            name.to_string()
        })
        .collect();

    let build_args = serde_json::json!({ "inputs": inputs });
    let build_out = registry
        .execute(
            "artifact_build",
            &writer,
            &writer_policy,
            &writer_dir,
            Some(&gateway_dir),
            &build_args.to_string(),
            Some(session),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .expect("artifact_build");
    let build_v: serde_json::Value = serde_json::from_str(&build_out).expect("build json");
    let artifact_ref = build_v["artifact_ref"]
        .as_str()
        .expect("artifact_ref")
        .to_string();

    let exec_args = serde_json::json!({
        "artifact_ref": artifact_ref,
        "entrypoint": entrypoint,
    });
    let exec_out = registry
        .execute(
            "artifact_exec",
            &runner,
            &runner_policy,
            &runner_dir,
            Some(&gateway_dir),
            &exec_args.to_string(),
            Some(session),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .expect("artifact_exec");
    serde_json::from_str(&exec_out).expect("exec json")
}

/// Imports `urllib` but mocks the HTTP caller — deterministic, hermetic. Must
/// run offline and PASS (this is the false-deny the fix removes).
const MOCKED_SUITE: &str = r#"
import urllib.request
import unittest
from unittest import mock

def fetch(url):
    with urllib.request.urlopen(url) as r:
        return r.read().decode()

class TestFetch(unittest.TestCase):
    @mock.patch("urllib.request.urlopen")
    def test_fetch_mocked(self, m):
        cm = mock.MagicMock()
        cm.read.return_value = b"hello"
        m.return_value.__enter__.return_value = cm
        self.assertEqual(fetch("http://example.com"), "hello")

if __name__ == "__main__":
    unittest.main(verbosity=2)
"#;

/// Makes a REAL network call with no mock — must fail in the offline sandbox.
const LIVE_SUITE: &str = r#"
import urllib.request
import unittest

class TestLive(unittest.TestCase):
    def test_real_call(self):
        with urllib.request.urlopen("http://example.com", timeout=5) as r:
            self.assertTrue(len(r.read()) > 0)

if __name__ == "__main__":
    unittest.main(verbosity=2)
"#;

#[test]
#[ignore = "requires bubblewrap; runs Python in the sandbox — not run in CI"]
fn mocked_urllib_suite_runs_and_passes_offline() {
    let v = build_and_exec(
        "sess-mocked",
        &[("test_service.py", MOCKED_SUITE)],
        "test_service.py",
    );
    assert_ne!(
        v.get("promotion_gate_network_denied"),
        Some(&serde_json::json!(true)),
        "fixed gate must NOT statically pre-deny a mocked suite: {v}"
    );
    assert_eq!(
        v.get("ok"),
        Some(&serde_json::json!(true)),
        "mocked urllib suite should run and pass offline: {v}"
    );
    assert_eq!(
        v.get("exit_code"),
        Some(&serde_json::json!(0)),
        "unittest should exit 0: {v}"
    );
    // The detected import is surfaced as informational, not a block.
    assert_eq!(
        v.get("network_isolated_run"),
        Some(&serde_json::json!(true)),
        "isolated promotion run should flag network_isolated_run: {v}"
    );
}

#[test]
#[ignore = "requires bubblewrap; runs Python in the sandbox — not run in CI"]
fn live_network_suite_runs_but_fails_offline() {
    let v = build_and_exec(
        "sess-live",
        &[("test_service.py", LIVE_SUITE)],
        "test_service.py",
    );
    assert_ne!(
        v.get("promotion_gate_network_denied"),
        Some(&serde_json::json!(true)),
        "the gate runs the suite rather than pre-denying: {v}"
    );
    assert_eq!(
        v.get("ok"),
        Some(&serde_json::json!(false)),
        "a real network call must fail in the offline sandbox: {v}"
    );
    let stderr = v.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
    assert!(
        stderr.contains("URLError")
            || stderr.contains("ConnectionError")
            || stderr.contains("Network is unreachable")
            || stderr.contains("Name or service not known")
            || stderr.contains("getaddrinfo")
            || v.get("network_blocked") == Some(&serde_json::json!(true)),
        "expected a runtime network failure signal, got: {v}"
    );
}
