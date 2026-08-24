//! WASM (WASI preview1) driver — the portable in-process tier.
//!
//! The only [`DriverTier::InProcess`] backend: there is no host `(program,
//! argv)`, so the process spawn path rejects it and
//! [`crate::sandbox::SandboxRunner::run_to_output`] routes here instead. The
//! driver is always registered so `sandbox: "wasm"` resolves on every build;
//! only the execution body is behind the `wasm-tier` feature, which turns a
//! missing feature into a clear error instead of an unknown-driver one.

use super::{DriverTier, InProcessRequest, SandboxDriver, SandboxDriverKind};
use crate::sandbox::{BwrapIsolationOverrides, ExecOutput};

pub struct WasmDriver;

impl SandboxDriver for WasmDriver {
    fn kind(&self) -> SandboxDriverKind {
        SandboxDriverKind::Wasm
    }

    fn names(&self) -> &'static [&'static str] {
        &["wasm", "wasi"]
    }

    fn tier(&self) -> DriverTier {
        DriverTier::InProcess
    }

    /// Always offline — the in-process WASI preview1 tier exposes no sockets
    /// (only a preopened workspace dir, args, env).
    fn guarantees_network_off(&self, _overrides: &BwrapIsolationOverrides) -> bool {
        true
    }

    /// No host filesystem beyond the preopened workspace dir — declared
    /// `runtime.mounts` cannot be honoured and must not be silently ignored
    /// (#1002 slice 3).
    fn check_mount_support(
        &self,
        mounts: &[autonoetic_types::agent::DeclaredMount],
    ) -> anyhow::Result<()> {
        if mounts.is_empty() {
            return Ok(());
        }
        let paths: Vec<&str> = mounts.iter().map(|m| m.host_path.as_str()).collect();
        anyhow::bail!(
            "wasm tier has no host filesystem: runtime.mounts are not supported \
             (declared: [{}]). Remove the declarations or select a process sandbox \
             driver (bubblewrap/docker).",
            paths.join(", ")
        );
    }

    // No SDK socket bridge: the tier uses host-function imports (P4).

    fn run_in_process(&self, req: &InProcessRequest<'_>) -> anyhow::Result<ExecOutput> {
        run_wasm_request(req)
    }
}

/// Run a request on the WASM tier (`wasm-tier` feature): resolve the `Code`
/// entry to a `.wasm` file under the agent dir and execute it via the WASI
/// backend, preopening the agent dir. Free-form shell / inline source are
/// rejected — the portable tier runs declared code, not arbitrary shell.
#[cfg(feature = "wasm-tier")]
fn run_wasm_request(req: &InProcessRequest<'_>) -> anyhow::Result<ExecOutput> {
    use crate::exec_request::{CodeSource, ExecutionKind};
    use crate::sandbox::driver::bubblewrap::BWRAP_WORKSPACE_DIR;
    use std::path::Path;

    let (entry, args) = match req.request {
        ExecutionKind::Code {
            source: CodeSource::Entry(path),
            args,
            ..
        } => (path.clone(), args.clone()),
        ExecutionKind::Code {
            source: CodeSource::Inline(_),
            ..
        } => anyhow::bail!("wasm tier: inline source is not supported yet (declare a .wasm entry)"),
        ExecutionKind::Shell { .. } => {
            anyhow::bail!("wasm tier does not support free-form shell execution")
        }
    };
    // Keep the module strictly under the agent dir: reject absolute paths and any
    // `..` traversal so a manifest entry can't read/execute outside the bundle.
    let entry_path = Path::new(&entry);
    anyhow::ensure!(
        entry_path.is_relative()
            && !entry_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
        "wasm entry must be a relative path within the agent dir (no `..`): {entry}"
    );
    let wasm_path = Path::new(req.agent_dir).join(&entry);
    let wasm = std::fs::read(&wasm_path)
        .map_err(|e| anyhow::anyhow!("reading wasm entry {}: {e}", wasm_path.display()))?;
    let out = crate::wasm_backend::run_wasi_module(
        &wasm,
        Path::new(req.agent_dir),
        // Same guest workspace path the process tiers use, so input-file env
        // vars (built against BWRAP_WORKSPACE_DIR) resolve inside the module too.
        BWRAP_WORKSPACE_DIR,
        &args,
        req.extra_env,
        req.stdin.clone().unwrap_or_default(),
        &crate::wasm_backend::WasmLimits::default(),
    )?;
    Ok(ExecOutput {
        exit_code: out.exit_code,
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

#[cfg(not(feature = "wasm-tier"))]
fn run_wasm_request(_req: &InProcessRequest<'_>) -> anyhow::Result<ExecOutput> {
    anyhow::bail!("wasm sandbox tier requires the `wasm-tier` build feature")
}
