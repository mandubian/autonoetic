//! Sandbox e2e for artifact_exec `input` dual delivery (`#[ignore]`d).
//!
//! These tests require a working **bubblewrap** (`bwrap`) on the host and
//! actually execute Python in the sandbox — they are NOT run in CI (same
//! rationale as `tests/promotion/gate_mocked_network_e2e.rs`). The CI-safe
//! unit coverage for the SKILL.md mode extraction lives next to
//! `artifact_script_input_mode` in `artifact_exec.rs`.
//!
//! Run locally with:
//! ```bash
//! # libtest substring-matches the filter against the FULL test name, and the
//! # module path is part of it (`exec_stdin::artifact_exec_...`), so the
//! # `exec_stdin` filter below selects exactly the three tests in this module.
//! cargo test -p autonoetic-gateway --test artifact exec_stdin -- --ignored --nocapture
//! # Equivalent: run every ignored e2e in the artifact domain binary.
//! cargo test -p autonoetic-gateway --test artifact -- --ignored --nocapture
//! ```
//!
//! What they prove (the session-ed19b4ca silent-empty-stdin class):
//! 1. A stdin-reading entrypoint run ad-hoc via artifact_exec with `input`
//!    receives the payload on stdin when the artifact declares the default
//!    stdin input mode — env var stays set too (dual delivery, mirroring the
//!    agent-spawn fast path in `script_execute.rs`).
//! 2. An artifact explicitly declaring `script_input_mode: args` opts out of
//!    stdin delivery; the env var still carries the payload.
//! 3. Passing no `input` injects nothing — no phantom stdin/env content.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn base_manifest(id: &str, name: &str, capabilities: Vec<Capability>) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: id.to_string(),
            name: name.to_string(),
            description: "test agent".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities,
        ..TestManifest::new().build()
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

/// `unit_test_runner.default` — promotion federation exec gate on bubblewrap
/// (gateway grants artifact_exec by role + SandboxFunctions allowlist), the
/// same proven exec shape as gate_mocked_network_e2e.rs.
fn runner_manifest() -> AgentManifest {
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

/// Prints what the entrypoint actually saw on stdin vs the env var, so the
/// assertions below pin the exact dual-delivery contract.
const ECHO_SCRIPT: &str = r#"#!/usr/bin/env python3
import sys, os
stdin_data = sys.stdin.read()
env_data = os.environ.get('AUTONOETIC_INPUT', '')
print(f"STDIN<{stdin_data}>ENV<{env_data}>")
"#;

const SKILL_ARGS_MODE: &str = "---\nname: args-mode-artifact\ndescription: d\nmetadata:\n  autonoetic:\n    script_input_mode: args\n---\nbody";

/// Build an artifact from the given (filename, content) files in `session`,
/// run `entrypoint` through artifact_exec with `exec_extra` merged into the
/// arguments, and return the parsed response JSON.
fn build_and_exec(
    session: &str,
    files: &[(&str, &str)],
    entrypoint: &str,
    exec_extra: serde_json::Value,
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
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).expect("gateway store"));
    let writer = writer_manifest();
    let writer_policy = PolicyEngine::new(writer.clone());
    let runner = runner_manifest();
    let runner_policy = PolicyEngine::new(runner.clone());
    let registry = default_registry();

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

    let mut exec_args = serde_json::json!({
        "artifact_ref": artifact_ref,
        "entrypoint": entrypoint,
    });
    for (k, v) in exec_extra.as_object().expect("extra is an object") {
        exec_args[k.as_str()] = v.clone();
    }
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

fn stdout_of(result: &serde_json::Value) -> String {
    assert_eq!(
        result["ok"], true,
        "sandbox exec must run to completion: {result:?}"
    );
    result["stdout"].as_str().unwrap_or_default().to_string()
}

#[test]
#[ignore = "requires bwrap + python3 on the host; not runnable in CI"]
fn artifact_exec_input_reaches_stdin_for_default_stdin_mode() {
    // No SKILL.md → plain script artifact → stdin default → dual delivery.
    let result = build_and_exec(
        "stdin-dual-delivery",
        &[("main.py", ECHO_SCRIPT)],
        "main.py",
        serde_json::json!({ "input": "stdin-payload-marker" }),
    );
    let stdout = stdout_of(&result);
    assert!(
        stdout.contains("STDIN<stdin-payload-marker>"),
        "payload must arrive on the entrypoint's stdin: {stdout:?}"
    );
    assert!(
        stdout.contains("ENV<stdin-payload-marker>"),
        "AUTONOETIC_INPUT env var must stay set (load_input() compat): {stdout:?}"
    );
}

#[test]
#[ignore = "requires bwrap + python3 on the host; not runnable in CI"]
fn artifact_exec_input_skips_stdin_for_declared_args_mode() {
    let result = build_and_exec(
        "stdin-args-optout",
        &[("main.py", ECHO_SCRIPT), ("SKILL.md", SKILL_ARGS_MODE)],
        "main.py",
        serde_json::json!({ "input": "args-payload-marker" }),
    );
    let stdout = stdout_of(&result);
    assert!(
        stdout.contains("STDIN<>"),
        "script_input_mode: args must opt out of stdin delivery: {stdout:?}"
    );
    assert!(
        stdout.contains("ENV<args-payload-marker>"),
        "env delivery is unconditional: {stdout:?}"
    );
}

#[test]
#[ignore = "requires bwrap + python3 on the host; not runnable in CI"]
fn artifact_exec_without_input_injects_nothing() {
    let result = build_and_exec(
        "stdin-no-input",
        &[("main.py", ECHO_SCRIPT)],
        "main.py",
        serde_json::json!({}),
    );
    let stdout = stdout_of(&result);
    assert!(
        stdout.contains("STDIN<>ENV<>"),
        "no `input` must mean no phantom stdin/env payload: {stdout:?}"
    );
}
