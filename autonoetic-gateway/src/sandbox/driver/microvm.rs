//! MicroVM (firecracker) driver.
//!
//! The gateway passes only the operator's `--config-file`; the VM's devices,
//! rootfs, and NIC are declared there, which is why this driver makes no
//! network guarantee and cannot bootstrap dependencies.

use std::process::Command;

use super::{DriverTier, SandboxDriver, SandboxDriverKind, SpawnSpec};
use crate::sandbox::{BwrapIsolationOverrides, DependencyPlan};

const FIRECRACKER_CONFIG_ENV: &str = "AUTONOETIC_FIRECRACKER_CONFIG";

pub struct MicroVmDriver;

impl SandboxDriver for MicroVmDriver {
    fn kind(&self) -> SandboxDriverKind {
        SandboxDriverKind::MicroVm
    }

    fn names(&self) -> &'static [&'static str] {
        &["microvm", "firecracker"]
    }

    fn tier(&self) -> DriverTier {
        DriverTier::Process
    }

    /// NOT guaranteed — network is whatever the operator's firecracker
    /// `--config-file` declares; the gateway passes only that file and cannot
    /// assert the absence of a NIC. Conservative `false`.
    fn guarantees_network_off(&self, _overrides: &BwrapIsolationOverrides) -> bool {
        false
    }

    fn check_dependency_support(&self, _plan: &DependencyPlan) -> anyhow::Result<()> {
        anyhow::bail!("MicroVM dependency bootstrap is not implemented yet")
    }

    // No SDK bridge yet (deferred to P5) — `sdk_socket_path` keeps its `None`
    // default, so no socket is started for this driver.

    fn build_command(&self, spec: &SpawnSpec<'_>) -> anyhow::Result<(String, Vec<String>)> {
        microvm_command(spec.entrypoint)
    }

    /// No container indirection: `extra_env` goes straight on the child.
    fn apply_child_env(
        &self,
        command: &mut Command,
        _socket_path_sandbox: Option<&str>,
        extra_env: &[(String, String)],
    ) {
        for (key, value) in extra_env {
            command.env(key, value);
        }
    }
}

fn microvm_command(_entrypoint: &str) -> anyhow::Result<(String, Vec<String>)> {
    let cfg = std::env::var(FIRECRACKER_CONFIG_ENV).map_err(|_| {
        anyhow::anyhow!(
            "Missing required environment variable {}",
            FIRECRACKER_CONFIG_ENV
        )
    })?;
    let argv = vec!["--config-file".to_string(), cfg];
    Ok(("firecracker".to_string(), argv))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial] // mutates AUTONOETIC_FIRECRACKER_CONFIG (process-global)
    fn test_microvm_command_requires_env() {
        let old = std::env::var(FIRECRACKER_CONFIG_ENV).ok();
        std::env::remove_var(FIRECRACKER_CONFIG_ENV);
        let err = microvm_command("ignored").expect_err("microvm command should fail without env");
        assert!(
            err.to_string().contains(FIRECRACKER_CONFIG_ENV),
            "error should mention missing firecracker env"
        );
        if let Some(v) = old {
            std::env::set_var(FIRECRACKER_CONFIG_ENV, v);
        }
    }

    #[test]
    fn dependency_plans_are_rejected() {
        let plan = DependencyPlan {
            runtime: crate::sandbox::DependencyRuntime::Python,
            packages: vec!["requests".to_string()],
        };
        let err = MicroVmDriver
            .check_dependency_support(&plan)
            .expect_err("microvm cannot bootstrap dependencies");
        assert!(
            err.to_string().contains("MicroVM dependency bootstrap"),
            "got: {err}"
        );
    }
}
