//! Constitution R+7 / R+18 — Runtime-lock drift check at session start.


use autonoetic_gateway::runtime::install_contract::GATEWAY_BUILD_SHA256;
use autonoetic_gateway::runtime_lock::{
    check_runtime_lock_drift, DriftCheckResult, DriftSkippedReason,
};
use autonoetic_types::runtime_lock::{LockedGateway, LockedSandbox, LockedSdk, RuntimeLock};
use crate::support::TestWorkspace;

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
        credentials: vec![],
    }
}

fn lock_with_binary_sha(build_sha: &str, binary_sha: &str) -> RuntimeLock {
    RuntimeLock {
        gateway: LockedGateway {
            artifact: "marketplace://gateway/autonoetic-gateway".into(),
            version: "0.1.0".into(),
            sha256: build_sha.into(),
            binary_sha256: Some(binary_sha.into()),
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
        credentials: vec![],
    }
}

#[test]
fn drift_rejected_when_build_sha_mismatches() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    write_runtime_lock(&agent_dir, &lock_with_build_sha("sha256:deadbeef"));

    let result = check_runtime_lock_drift(&agent_dir);
    match result {
        DriftCheckResult::Drift(drift) => {
            assert_eq!(drift.locked_build_sha256, "sha256:deadbeef");
            assert_ne!(
                drift.current_build_sha256, "sha256:deadbeef",
                "current SHA should differ from the fake one"
            );
        }
        other => panic!("expected Drift, got {:?}", other),
    }
}

#[test]
fn drift_rejected_when_binary_sha_mismatches() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    write_runtime_lock(
        &agent_dir,
        &lock_with_binary_sha(GATEWAY_BUILD_SHA256, "sha256:bada55"),
    );

    let result = check_runtime_lock_drift(&agent_dir);
    match result {
        DriftCheckResult::Drift(drift) => {
            assert_eq!(drift.locked_binary_sha256.as_deref(), Some("sha256:bada55"));
            assert_ne!(
                drift.current_binary_sha256.as_deref(),
                Some("sha256:bada55"),
                "current binary SHA should differ from the fake one"
            );
        }
        other => panic!("expected Drift, got {:?}", other),
    }
}

#[test]
fn no_drift_when_build_sha_matches_current_gateway() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    write_runtime_lock(&agent_dir, &lock_with_build_sha(GATEWAY_BUILD_SHA256));

    let result = check_runtime_lock_drift(&agent_dir);
    assert!(
        matches!(result, DriftCheckResult::Clean),
        "no drift expected when lock matches current gateway build SHA, got {:?}",
        result
    );
}

#[test]
fn skipped_when_runtime_lock_absent() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let result = check_runtime_lock_drift(&agent_dir);
    assert!(
        matches!(
            result,
            DriftCheckResult::Skipped(DriftSkippedReason::LockAbsent)
        ),
        "absent lock should be Skipped(LockAbsent), got {:?}",
        result
    );
}

#[test]
fn skipped_when_runtime_lock_malformed() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    std::fs::write(agent_dir.join("runtime.lock"), "not: valid: yaml: {{{").unwrap();

    let result = check_runtime_lock_drift(&agent_dir);
    assert!(
        matches!(
            result,
            DriftCheckResult::Skipped(DriftSkippedReason::LockMalformed(_))
        ),
        "malformed lock should be Skipped(LockMalformed), got {:?}",
        result
    );
}

#[test]
fn drift_payload_contains_both_shas() {
    let ws = TestWorkspace::new().unwrap();
    let agent_dir = ws.agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    write_runtime_lock(&agent_dir, &lock_with_build_sha("sha256:aabbccdd"));

    match check_runtime_lock_drift(&agent_dir) {
        DriftCheckResult::Drift(drift) => {
            assert!(drift.locked_build_sha256.starts_with("sha256:"));
            assert!(drift.current_build_sha256.starts_with("sha256:"));
            assert_ne!(drift.locked_build_sha256, drift.current_build_sha256);
        }
        other => panic!("expected Drift, got {:?}", other),
    }
}
