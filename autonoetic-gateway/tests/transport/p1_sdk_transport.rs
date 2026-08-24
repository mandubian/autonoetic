//! P1 — SDK transport abstraction (issue #439).
//!
//! The SDK bridge today is wired for **bubblewrap only** (`sandbox.rs:252`,
//! `if driver == SandboxDriverKind::Bubblewrap`). P1 extracts a transport
//! abstraction and brings **docker** to parity so a docker agent can call SDK
//! methods too. Per RFC §4.1, every early-phase acceptance check runs on **both**
//! bubblewrap and docker, availability-gated so a contributor with only one
//! driver still gets a green local run.
//!
//! This file seeds the phase with the driver-availability helpers the P1 tests
//! build on. The end-to-end bridge-parity assertions land with the refactor.

use serial_test::serial;

/// True when `bwrap --version` succeeds (bubblewrap installed).
pub fn is_bwrap_available() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// True when `docker version` succeeds (docker CLI + reachable daemon).
pub fn is_docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn driver_availability_probes_do_not_panic() {
    // Probing is side-effect free and must never panic, so availability-gated
    // tests can call it unconditionally to decide skip-vs-run.
    let _ = is_bwrap_available();
    let _ = is_docker_available();
}

/// P1 acceptance: a **docker** agent can reach the SDK bridge (closes the
/// bubblewrap-only gap). A stdlib-only Python probe inside the docker sandbox
/// connects to the bridge socket (exposed via `-v` + `CCOS_SOCKET_PATH` `-e`)
/// and performs one JSON-RPC round-trip. It uses an unknown method on purpose:
/// the bridge replies with a structured JSON-RPC *error*, which still proves the
/// transport end-to-end without depending on the Python SDK package layout or
/// gateway session state. Skipped unless docker is available.
#[test]
#[serial] // mutates AUTONOETIC_DOCKER_IMAGE (process-global); don't race other env tests
fn docker_agent_reaches_sdk_bridge() {
    if !is_docker_available() {
        eprintln!("skipping docker_agent_reaches_sdk_bridge: docker not available");
        return;
    }
    use autonoetic_gateway::sandbox::SandboxRunner;

    // Capture + restore so we don't leak global env to later tests.
    let prev_image = std::env::var("AUTONOETIC_DOCKER_IMAGE").ok();
    let image = prev_image
        .clone()
        .unwrap_or_else(|| "python:3.12-slim".to_string());
    std::env::set_var("AUTONOETIC_DOCKER_IMAGE", &image);

    let dir = tempfile::tempdir().expect("tempdir");
    let probe = r#"import os, socket, json, sys
path = os.environ["CCOS_SOCKET_PATH"]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(path)
s.sendall((json.dumps({"jsonrpc": "2.0", "id": 1, "method": "p1.probe", "params": {}}) + "\n").encode())
sys.stdout.write(s.recv(65536).decode())
sys.stdout.flush()
"#;
    std::fs::write(dir.path().join("p1_probe.py"), probe).expect("write probe");
    let agent_dir = dir.path().to_str().expect("utf8 path");

    let gateway_dir = dir.path().join("runtime");
    let runner = SandboxRunner::spawn_for_driver(
        "docker",
        agent_dir,
        &gateway_dir,
        "python3 /workspace/p1_probe.py",
    )
    .expect("spawn docker sandbox");
    // `wait_with_output` drains BOTH stdout and stderr — reading only stdout
    // could deadlock if the child fills the stderr pipe. The bridge guard stays
    // alive in `runner` (SandboxRunner has no Drop) until end of scope.
    let output = runner
        .process
        .wait_with_output()
        .expect("wait for docker sandbox");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    match prev_image {
        Some(v) => std::env::set_var("AUTONOETIC_DOCKER_IMAGE", v),
        None => std::env::remove_var("AUTONOETIC_DOCKER_IMAGE"),
    }

    assert!(
        output.status.success(),
        "docker sandbox exited non-zero.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("\"jsonrpc\""),
        "expected a JSON-RPC response from the docker SDK bridge.\nstdout: {stdout}\nstderr: {stderr}"
    );
}
