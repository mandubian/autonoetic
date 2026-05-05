//! Constitution R-3.7 — docker/microvm paths require explicit driver profiles.
//!
//! Resource quotas for these drivers are externalized to the selected runtime
//! profile (container image / firecracker config). The gateway must fail shut
//! if those profiles are missing.

use autonoetic_gateway::sandbox::{SandboxDriverKind, SandboxRunner};
use serial_test::serial;
use tempfile::tempdir;

struct EnvRestore {
    key: &'static str,
    previous: Option<String>,
}

impl EnvRestore {
    fn clear(key: &'static str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

#[test]
#[serial]
fn r_3_7_driver_specific_profiles_are_mandatory() {
    const DOCKER_IMAGE_ENV: &str = "AUTONOETIC_DOCKER_IMAGE";
    const FIRECRACKER_CONFIG_ENV: &str = "AUTONOETIC_FIRECRACKER_CONFIG";

    let _docker_guard = EnvRestore::clear(DOCKER_IMAGE_ENV);
    let _microvm_guard = EnvRestore::clear(FIRECRACKER_CONFIG_ENV);

    let temp = tempdir().expect("tempdir");
    let agent_dir = temp.path().to_string_lossy().to_string();

    let docker_err = match SandboxRunner::spawn_with_driver(
        SandboxDriverKind::Docker,
        &agent_dir,
        "echo hello",
    ) {
        Ok(_) => panic!("docker driver must fail without explicit image/profile"),
        Err(err) => err,
    };
    assert!(
        docker_err.to_string().contains(DOCKER_IMAGE_ENV),
        "expected missing docker profile env error, got: {docker_err}"
    );

    let microvm_err = match SandboxRunner::spawn_with_driver(
        SandboxDriverKind::MicroVm,
        &agent_dir,
        "echo hello",
    ) {
        Ok(_) => panic!("microvm driver must fail without explicit firecracker profile"),
        Err(err) => err,
    };
    assert!(
        microvm_err.to_string().contains(FIRECRACKER_CONFIG_ENV),
        "expected missing microvm profile env error, got: {microvm_err}"
    );
}
