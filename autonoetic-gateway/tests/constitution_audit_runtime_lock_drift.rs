//! Constitution R+7 / R+18 — Runtime-lock drift check at session start.

mod support;

use autonoetic_gateway::runtime::install_contract::GATEWAY_BUILD_SHA256;
use autonoetic_gateway::runtime_lock::check_runtime_lock_drift;
use autonoetic_types::runtime_lock::{LockedGateway, LockedSandbox, LockedSdk, RuntimeLock};
use support::TestWorkspace;

fn write_runtime_lock(dir: &std::path::Path, lock: &RuntimeLock) {
    let yaml = serde_yaml::to_string(lock).unwrap();
    std::fs::write(dir.join("runtime.lock"), yaml).unwrap();
}

fn lock_with_build_sha(sha: &str) -> RuntimeLock {
    RuntimeLock {
        gateway: LockedGateway {
            artifact: "marketplace://gateway/autonoetic-gateway".into(),
            version: "0.1.0".into(),
            sha256: sha.into(),
            binary_sha256: None,
            build_tag: None,
            signature: None,
        },
        sdk: LockedSdk {
            version: "0.1.0".into(),
        },
        sandbox: LockedSandbox {
            backend: "bubblewrap".into(),
        },
        dependencies: vec![],
        artifacts: vec![],
        layers: vec![],
    }
}

#[test]
fn drift_rejected_when_build_sha_mismatches() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    write_runtime_lock(&agent_dir, &lock_with_build_sha("sha256:deadbeef"));

    let result = check_runtime_lock_drift(&agent_dir);
    assert!(result.is_err(), "should detect drift when build SHA differs");
    let drift = result.unwrap_err();
    assert_eq!(drift.locked_build_sha256, "sha256:deadbeef");
    assert_ne!(
        drift.current_build_sha256, "sha256:deadbeef",
        "current SHA should differ from the fake one"
    );
}

#[test]
fn no_drift_when_build_sha_matches_current_gateway() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    write_runtime_lock(&agent_dir, &lock_with_build_sha(GATEWAY_BUILD_SHA256));

    let result = check_runtime_lock_drift(&agent_dir);
    assert!(
        result.is_ok(),
        "no drift expected when lock matches current gateway build SHA"
    );
}

#[test]
fn no_drift_when_runtime_lock_absent() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let result = check_runtime_lock_drift(&agent_dir);
    assert!(
        result.is_ok(),
        "no drift when runtime.lock does not exist"
    );
}

#[test]
fn no_drift_when_runtime_lock_malformed() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    std::fs::write(agent_dir.join("runtime.lock"), "not: valid: yaml: {{{").unwrap();

    let result = check_runtime_lock_drift(&agent_dir);
    assert!(
        result.is_ok(),
        "malformed lock should not block (graceful degradation)"
    );
}

#[test]
fn drift_payload_contains_both_shas() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    write_runtime_lock(&agent_dir, &lock_with_build_sha("sha256:aabbccdd"));

    let drift = check_runtime_lock_drift(&agent_dir).unwrap_err();
    assert!(drift.locked_build_sha256.starts_with("sha256:"));
    assert!(drift.current_build_sha256.starts_with("sha256:"));
    assert_ne!(drift.locked_build_sha256, drift.current_build_sha256);
}
