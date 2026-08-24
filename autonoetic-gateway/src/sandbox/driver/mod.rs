//! Pluggable sandbox execution drivers.
//!
//! Before #1117 the driver set was an enum matched at seven sites across
//! `sandbox.rs` (command construction ×2, SDK socket path, bridge wiring, child
//! env, network-isolation semantics, dependency support), so adding a backend
//! meant editing every one of them and hoping none was missed.
//!
//! Here each backend is a [`SandboxDriver`] impl in its own file, and the
//! [`SandboxDriverRegistry`] maps the manifest-facing name (`runtime.sandbox` in
//! SKILL.md) to that impl. `sandbox.rs` orchestrates — start the SDK bridge,
//! compose the entrypoint, spawn and wait — but no longer knows which drivers
//! exist or how any of them behave.
//!
//! **Adding a driver**: one new file implementing [`SandboxDriver`], one
//! [`SandboxDriverKind`] variant (the selection key, which is public API), and
//! one line in [`builtin_registry`]. The kind→impl coverage is asserted by
//! `every_kind_is_registered` below, so a variant added without a registry entry
//! fails the test suite instead of failing at runtime.

pub mod bubblewrap;
pub mod docker;
pub mod microvm;
pub mod wasm;

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, LazyLock};

use crate::exec_request::ExecutionKind;
use crate::sandbox::{
    BwrapIsolationOverrides, DependencyPlan, ExecOutput, SandboxMount, SdkBridgeWiring,
};

/// Where a driver's work actually happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverTier {
    /// Spawns a host child process (bubblewrap, docker, firecracker).
    Process,
    /// Runs inside the gateway process (the WASI tier) — no `(program, argv)`.
    InProcess,
}

/// Selection key for a sandbox driver.
///
/// Kept as a closed enum because it is public API (`runtime.sandbox` manifests
/// parse into it, and callers match on it); the *behavior* behind each variant
/// lives in a [`SandboxDriver`] impl, not in matches on this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxDriverKind {
    Bubblewrap,
    Docker,
    MicroVm,
    /// In-process WebAssembly (WASI) tier — the portable execution backend
    /// (RFC `docs/rfc/portable-wasm-execution-tier.md`, P4). Selected via
    /// `sandbox: "wasm"`; runs declared modules in-process through
    /// [`crate::sandbox::SandboxRunner::run_to_output`] when built with the
    /// `wasm-tier` feature (without it, selecting this driver returns a clear
    /// build-feature error).
    Wasm,
}

impl SandboxDriverKind {
    /// Resolve a manifest-declared driver name (or alias) to its kind.
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        builtin_registry().resolve(name).map(|driver| driver.kind())
    }

    /// The registered driver behind this kind.
    pub fn driver(self) -> anyhow::Result<&'static Arc<dyn SandboxDriver>> {
        builtin_registry().get(self)
    }

    /// Whether this driver runs inside the gateway process rather than spawning
    /// a host child. Callers that must build a different request shape for the
    /// in-process tier ask this instead of comparing against a specific variant.
    pub fn runs_in_process(self) -> bool {
        self.driver()
            .map(|d| d.tier() == DriverTier::InProcess)
            .unwrap_or(false)
    }

    /// Whether this driver guarantees the run has **no network access** under the
    /// given isolation overrides. Single source of truth for "is this execution
    /// physically offline" — used by the promotion gate to decide whether a
    /// deterministic test suite can be trusted to run in isolation (P-3.10)
    /// instead of being statically pre-denied on mere import detection.
    ///
    /// Each driver answers for itself (see the per-driver impls); an
    /// unregistered kind answers `false`, so the gate fails closed.
    pub fn guarantees_network_off(self, overrides: &BwrapIsolationOverrides) -> bool {
        self.driver()
            .map(|d| d.guarantees_network_off(overrides))
            .unwrap_or(false)
    }
}

/// Everything a process-tier driver needs to build one execution.
pub struct SpawnSpec<'a> {
    /// Host directory bound in as the sandbox workspace.
    pub agent_dir: &'a str,
    /// The gateway's own directory, as the execution engine resolved it.
    ///
    /// Threaded rather than derived: agents execute from *inside* the revision
    /// store, so `agent_dir.parent()` is `<gateway_dir>/revisions/agents/<id>`,
    /// not the agents root. Deriving the gateway dir that way is what made the
    /// secret mask emit zero flags (#1145).
    pub gateway_dir: &'a Path,
    /// Shell line to run inside the sandbox, already dependency-composed.
    pub entrypoint: &'a str,
    /// Bind mounts to expose inside the sandbox: session content plus whatever
    /// [`SandboxDriver::wire_sdk_bridge`] added.
    pub mounts: &'a [SandboxMount],
    /// Per-execution isolation overrides; `None` falls back to global config.
    pub overrides: Option<&'a BwrapIsolationOverrides>,
    /// Env the caller wants inside the sandbox. Drivers whose child inherits the
    /// gateway env apply this in [`SandboxDriver::apply_child_env`]; drivers
    /// whose child does not (containers) must bake it into the argv here.
    pub extra_env: &'a [(String, String)],
    /// SDK bridge plumbing produced by [`SandboxDriver::wire_sdk_bridge`].
    pub bridge: &'a SdkBridgeWiring,
}

/// Everything an in-process-tier driver needs to run one execution.
pub struct InProcessRequest<'a> {
    pub agent_dir: &'a str,
    /// The intent request — in-process tiers run declared code, not a rendered
    /// shell line, so they receive the request rather than an entrypoint string.
    pub request: &'a ExecutionKind,
    pub extra_env: &'a [(String, String)],
    pub stdin: Option<Vec<u8>>,
}

/// One sandbox execution backend.
///
/// Implementors own every behavior that used to be a `match driver { … }` arm
/// in `sandbox.rs`. The defaults describe a driver that spawns a host process,
/// runs no SDK bridge, inherits no env, accepts dependency plans, and makes no
/// network guarantee — each impl overrides what it actually does.
pub trait SandboxDriver: Send + Sync {
    /// Selection key; must match the registry entry.
    fn kind(&self) -> SandboxDriverKind;

    /// Manifest-facing names accepted by [`SandboxDriverKind::parse`], matched
    /// case-insensitively. The first entry is the canonical name.
    fn names(&self) -> &'static [&'static str];

    /// Where this driver runs.
    fn tier(&self) -> DriverTier {
        DriverTier::Process
    }

    /// Whether this driver guarantees no network reachability under `overrides`.
    /// Conservative default: a driver that has not reasoned about it says `false`.
    fn guarantees_network_off(&self, _overrides: &BwrapIsolationOverrides) -> bool {
        false
    }

    /// `Err` when this driver cannot honour a dependency-install plan.
    fn check_dependency_support(&self, _plan: &DependencyPlan) -> anyhow::Result<()> {
        Ok(())
    }

    /// In-sandbox path where the SDK bridge socket is exposed for `socket_name`.
    /// `None` means this driver runs no SDK bridge.
    fn sdk_socket_path(&self, _socket_name: &str) -> Option<String> {
        None
    }

    /// Whether this driver runs the SDK bridge at all. Derived from
    /// [`Self::sdk_socket_path`] so the two cannot disagree.
    fn runs_sdk_bridge(&self) -> bool {
        self.sdk_socket_path("probe.sock").is_some()
    }

    /// Fill in the plumbing that exposes an already-started bridge socket —
    /// bind mounts, container volumes, container env. Called only when
    /// [`Self::runs_sdk_bridge`] is true.
    fn wire_sdk_bridge(
        &self,
        _host_socket: &Path,
        _sandbox_socket: &str,
        _wiring: &mut SdkBridgeWiring,
    ) {
    }

    /// Build the host `(program, argv)` for a process-tier execution.
    fn build_command(&self, _spec: &SpawnSpec<'_>) -> anyhow::Result<(String, Vec<String>)> {
        anyhow::bail!(
            "sandbox driver '{}' does not support process-tier execution",
            self.names()[0]
        )
    }

    /// Apply env to the child `Command`. Drivers that bake env into their argv
    /// (containers) leave this empty.
    fn apply_child_env(
        &self,
        _command: &mut Command,
        _socket_path_sandbox: Option<&str>,
        _extra_env: &[(String, String)],
    ) {
    }

    /// Run an execution inside the gateway process. Called only when
    /// [`Self::tier`] is [`DriverTier::InProcess`].
    fn run_in_process(&self, _req: &InProcessRequest<'_>) -> anyhow::Result<ExecOutput> {
        anyhow::bail!(
            "sandbox driver '{}' does not support in-process execution",
            self.names()[0]
        )
    }
}

/// Name → driver lookup for the drivers this build ships.
pub struct SandboxDriverRegistry {
    drivers: Vec<Arc<dyn SandboxDriver>>,
}

impl SandboxDriverRegistry {
    /// Build a registry from a driver list. Panics on a duplicate kind or name —
    /// a registry that resolves one key to two backends is a build-time bug, and
    /// `builtin_registry` is constructed once at first use.
    pub fn new(drivers: Vec<Arc<dyn SandboxDriver>>) -> Self {
        let registry = Self { drivers };
        for (i, driver) in registry.drivers.iter().enumerate() {
            for other in &registry.drivers[i + 1..] {
                assert_ne!(
                    driver.kind(),
                    other.kind(),
                    "duplicate sandbox driver kind {:?}",
                    driver.kind()
                );
                for name in driver.names() {
                    assert!(
                        !other.names().iter().any(|n| n.eq_ignore_ascii_case(name)),
                        "duplicate sandbox driver name '{name}'"
                    );
                }
            }
        }
        registry
    }

    /// Resolve a manifest-declared name or alias, case-insensitively.
    pub fn resolve(&self, name: &str) -> anyhow::Result<&Arc<dyn SandboxDriver>> {
        self.drivers
            .iter()
            .find(|driver| driver.names().iter().any(|n| n.eq_ignore_ascii_case(name)))
            .ok_or_else(|| anyhow::anyhow!("Unsupported sandbox driver '{}'", name))
    }

    /// Look up the driver registered for a kind.
    pub fn get(&self, kind: SandboxDriverKind) -> anyhow::Result<&Arc<dyn SandboxDriver>> {
        self.drivers
            .iter()
            .find(|driver| driver.kind() == kind)
            .ok_or_else(|| anyhow::anyhow!("No sandbox driver registered for {:?}", kind))
    }

    /// Every registered driver, in registration order.
    pub fn drivers(&self) -> impl Iterator<Item = &Arc<dyn SandboxDriver>> {
        self.drivers.iter()
    }
}

static BUILTIN_DRIVERS: LazyLock<SandboxDriverRegistry> = LazyLock::new(|| {
    SandboxDriverRegistry::new(vec![
        Arc::new(bubblewrap::BubblewrapDriver),
        Arc::new(docker::DockerDriver),
        Arc::new(microvm::MicroVmDriver),
        Arc::new(wasm::WasmDriver),
    ])
});

/// The drivers this build ships. Bubblewrap stays the default backend; the
/// `wasm` driver is always registered (so `sandbox: "wasm"` resolves and gives a
/// clear build-feature error) while only its execution body is feature-gated.
pub fn builtin_registry() -> &'static SandboxDriverRegistry {
    &BUILTIN_DRIVERS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kind→impl mapping must be total: a new [`SandboxDriverKind`] variant
    /// added without a [`builtin_registry`] entry fails here rather than at
    /// runtime, where it would surface as a resolve error mid-execution.
    #[test]
    fn every_kind_is_registered() {
        for kind in [
            SandboxDriverKind::Bubblewrap,
            SandboxDriverKind::Docker,
            SandboxDriverKind::MicroVm,
            SandboxDriverKind::Wasm,
        ] {
            let driver = kind
                .driver()
                .unwrap_or_else(|e| panic!("{kind:?} must have a registered driver: {e}"));
            assert_eq!(driver.kind(), kind, "driver must report the kind it is keyed by");
            assert!(!driver.names().is_empty(), "{kind:?} must declare a name");
        }
    }

    /// Names are the manifest contract (`runtime.sandbox`); every declared name
    /// and alias must round-trip back to its own driver.
    #[test]
    fn every_declared_name_resolves_to_its_driver() {
        for driver in builtin_registry().drivers() {
            for name in driver.names() {
                assert_eq!(
                    SandboxDriverKind::parse(name).expect("declared name must parse"),
                    driver.kind(),
                    "name '{name}' must resolve to {:?}",
                    driver.kind()
                );
                assert_eq!(
                    SandboxDriverKind::parse(&name.to_ascii_uppercase())
                        .expect("names match case-insensitively"),
                    driver.kind()
                );
            }
        }
    }

    #[test]
    fn test_parse_driver_kind() {
        assert_eq!(
            SandboxDriverKind::parse("bubblewrap").expect("bubblewrap should parse"),
            SandboxDriverKind::Bubblewrap
        );
        assert_eq!(
            SandboxDriverKind::parse("bwrap").expect("bwrap alias should parse"),
            SandboxDriverKind::Bubblewrap
        );
        assert_eq!(
            SandboxDriverKind::parse("docker").expect("docker should parse"),
            SandboxDriverKind::Docker
        );
        assert_eq!(
            SandboxDriverKind::parse("microvm").expect("microvm should parse"),
            SandboxDriverKind::MicroVm
        );
        assert_eq!(
            SandboxDriverKind::parse("firecracker").expect("firecracker alias should parse"),
            SandboxDriverKind::MicroVm
        );
        assert_eq!(
            SandboxDriverKind::parse("wasm").expect("wasm should parse"),
            SandboxDriverKind::Wasm
        );
        assert_eq!(
            SandboxDriverKind::parse("wasi").expect("wasi alias should parse"),
            SandboxDriverKind::Wasm
        );
        assert!(SandboxDriverKind::parse("nope").is_err());
    }

    #[test]
    fn test_sdk_socket_sandbox_path_per_driver() {
        let bwrap = SandboxDriverKind::Bubblewrap.driver().expect("bubblewrap");
        assert_eq!(
            bwrap.sdk_socket_path("s.sock"),
            Some(format!("{}/s.sock", bubblewrap::BWRAP_WORKSPACE_DIR))
        );
        assert!(bwrap.runs_sdk_bridge());

        let docker = SandboxDriverKind::Docker.driver().expect("docker");
        assert_eq!(
            docker.sdk_socket_path("s.sock"),
            Some(docker::DOCKER_SDK_SOCKET_PATH.to_string())
        );
        assert!(docker.runs_sdk_bridge());

        // microvm has no bridge yet (P5)
        let microvm = SandboxDriverKind::MicroVm.driver().expect("microvm");
        assert!(microvm.sdk_socket_path("s.sock").is_none());
        assert!(!microvm.runs_sdk_bridge());

        // wasm uses host-function imports, not a socket bridge.
        let wasm = SandboxDriverKind::Wasm.driver().expect("wasm");
        assert!(wasm.sdk_socket_path("s.sock").is_none());
        assert!(!wasm.runs_sdk_bridge());
    }

    /// Tier is what callers branch on instead of comparing to a variant.
    #[test]
    fn only_the_wasm_tier_runs_in_process() {
        assert!(SandboxDriverKind::Wasm.runs_in_process());
        assert!(!SandboxDriverKind::Bubblewrap.runs_in_process());
        assert!(!SandboxDriverKind::Docker.runs_in_process());
        assert!(!SandboxDriverKind::MicroVm.runs_in_process());
    }

    /// MicroVM is the one driver that cannot bootstrap dependencies; every other
    /// driver accepts a plan (the composed entrypoint installs them in-sandbox).
    #[test]
    fn microvm_is_the_only_driver_rejecting_dependency_plans() {
        let plan = DependencyPlan {
            runtime: crate::sandbox::DependencyRuntime::Python,
            packages: vec!["requests".to_string()],
        };
        for driver in builtin_registry().drivers() {
            let supported = driver.check_dependency_support(&plan).is_ok();
            assert_eq!(
                supported,
                driver.kind() != SandboxDriverKind::MicroVm,
                "unexpected dependency support for {:?}",
                driver.kind()
            );
        }
    }
}
