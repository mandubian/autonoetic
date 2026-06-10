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
