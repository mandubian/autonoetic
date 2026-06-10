//! WASM execution backend (RFC `docs/rfc/portable-wasm-execution-tier.md`, P4).
//!
//! Gated behind the `wasm-tier` Cargo feature so the native build never pays
//! wasmtime's compile-time/binary-size cost. This first increment embeds the
//! runtime and proves a module instantiates and runs in-process; WASI preopens,
//! stdout/stderr capture, the host-function SDK bridge, and resource limits land
//! in subsequent increments.

use wasmtime::{Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p1::{add_to_linker_sync, WasiP1Ctx};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{DirPerms, FilePerms, I32Exit, WasiCtxBuilder};

/// Result of a WASI module run: captured streams + the process exit code.
#[derive(Debug, Clone)]
pub struct WasiRunOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Resource bounds for a WASM run (constitution P-3.7). `fuel` caps CPU work
/// (instructions); `max_memory_bytes` caps linear-memory growth. Tunables live
/// in code, not the constitution.
#[derive(Debug, Clone, Copy)]
pub struct WasmLimits {
    pub fuel: u64,
    pub max_memory_bytes: usize,
}

impl Default for WasmLimits {
    fn default() -> Self {
        // Generous enough for real interpreter runs (e.g. python.wasm), finite
        // so a runaway can't spin or balloon unbounded.
        Self {
            fuel: 20_000_000_000,
            max_memory_bytes: 512 << 20, // 512 MiB
        }
    }
}

/// Store data for a WASI run: the WASI context plus the resource limiter.
struct WasmStoreData {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

/// Per-stream capture cap (1 MiB) — bounds in-memory stdout/stderr buffering.
const WASI_PIPE_CAP: usize = 1 << 20;

/// Run a WASI Preview 1 command module (`_start`) in-process: the host directory
/// is preopened at the guest path `guest_dir`, `args`/`env` are passed through,
/// stdout/stderr are captured, and CPU/memory are bounded by `limits` (P-3.7).
/// Network is not granted. WASI `proc_exit` surfaces as `I32Exit` → exit code;
/// fuel/limit exhaustion surfaces as a trap (mapped to a clear error).
///
/// `guest_dir` is the path the workspace appears at *inside* the module. The
/// agent execution path passes the same workspace path the process tiers use
/// (`BWRAP_WORKSPACE_DIR`), so input-file env vars resolve identically across
/// tiers — a libc-based module (e.g. python.wasm) maps an absolute guest path
/// onto this preopen by prefix.
pub fn run_wasi_module(
    wasm: &[u8],
    preopen_host_dir: &std::path::Path,
    guest_dir: &str,
    args: &[String],
    env: &[(String, String)],
    limits: &WasmLimits,
) -> anyhow::Result<WasiRunOutput> {
    let mut config = Config::new();
    config.consume_fuel(true); // enables the CPU bound via set_fuel
    let engine =
        Engine::new(&config).map_err(|e| anyhow::anyhow!("building wasm engine: {e}"))?;
    let module =
        Module::new(&engine, wasm).map_err(|e| anyhow::anyhow!("compiling wasm module: {e}"))?;

    let stdout = MemoryOutputPipe::new(WASI_PIPE_CAP);
    let stderr = MemoryOutputPipe::new(WASI_PIPE_CAP);

    let mut builder = WasiCtxBuilder::new();
    builder.stdout(stdout.clone());
    builder.stderr(stderr.clone());
    builder.arg("program"); // argv[0]
    for a in args {
        builder.arg(a);
    }
    for (k, v) in env {
        builder.env(k, v);
    }
    builder
        .preopened_dir(preopen_host_dir, guest_dir, DirPerms::all(), FilePerms::all())
        .map_err(|e| anyhow::anyhow!("preopening workspace dir: {e}"))?;

    let data = WasmStoreData {
        wasi: builder.build_p1(),
        limits: StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes)
            .build(),
    };

    let mut linker: wasmtime::Linker<WasmStoreData> = wasmtime::Linker::new(&engine);
    add_to_linker_sync(&mut linker, |d: &mut WasmStoreData| &mut d.wasi)
        .map_err(|e| anyhow::anyhow!("adding wasi to linker: {e}"))?;
    let mut store = Store::new(&engine, data);
    store.limiter(|d| &mut d.limits);
    store
        .set_fuel(limits.fuel)
        .map_err(|e| anyhow::anyhow!("setting fuel: {e}"))?;

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| anyhow::anyhow!("instantiating wasi module: {e}"))?;
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(|e| anyhow::anyhow!("resolving _start: {e}"))?;

    let exit_code = match start.call(&mut store, ()) {
        Ok(()) => 0,
        // `proc_exit` is reported as I32Exit, not a real trap.
        Err(e) => match e.downcast_ref::<I32Exit>() {
            Some(exit) => exit.0,
            // Includes fuel exhaustion ("all fuel consumed") and memory-limit traps.
            None => return Err(anyhow::anyhow!("wasm execution failed (trap or resource limit): {e}")),
        },
    };
    drop(store); // release the pipe writers before reading

    Ok(WasiRunOutput {
        exit_code,
        stdout: String::from_utf8_lossy(&stdout.contents()).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.contents()).into_owned(),
    })
}

/// Instantiate a WebAssembly module (raw `.wasm` bytes, or `.wat` text when the
/// runtime's `wat` support is enabled) and call an exported `() -> i32` function.
/// Minimal, version-robust smoke over the core engine/module/instance API —
/// the seam the full WASI-backed executor builds on.
///
/// `wasmtime::Error` is the runtime's own (vendored) error type, so we map it
/// into this crate's `anyhow` via its `Display` rather than `?`/`.context()`.
pub fn run_export_i32(wasm: &[u8], func: &str) -> anyhow::Result<i32> {
    let engine = Engine::default();
    let module =
        Module::new(&engine, wasm).map_err(|e| anyhow::anyhow!("compiling wasm module: {e}"))?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])
        .map_err(|e| anyhow::anyhow!("instantiating wasm module: {e}"))?;
    let typed = instance
        .get_typed_func::<(), i32>(&mut store, func)
        .map_err(|e| anyhow::anyhow!("resolving exported fn '{func}' as () -> i32: {e}"))?;
    typed
        .call(&mut store, ())
        .map_err(|e| anyhow::anyhow!("calling exported fn '{func}': {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_trivial_wasm_module() {
        // Inline WAT compiled by the runtime — proves wasmtime is embedded and
        // can instantiate + call an export in-process.
        let wat = r#"(module (func (export "run") (result i32) i32.const 42))"#;
        let result = run_export_i32(wat.as_bytes(), "run").expect("module should run");
        assert_eq!(result, 42);
    }

    #[test]
    fn missing_export_errors_clearly() {
        let wat = r#"(module (func (export "run") (result i32) i32.const 1))"#;
        let err = run_export_i32(wat.as_bytes(), "nope").unwrap_err().to_string();
        assert!(err.contains("nope"), "error should name the missing export: {err}");
    }

    #[test]
    fn wasi_module_stdout_is_captured() {
        // Minimal WASI p1 module: write "hi\n" to fd 1 (stdout) via fd_write.
        // iovec at addr 0 → {ptr: 8, len: 3}; payload "hi\n" at addr 8.
        let wat = r#"(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 8) "hi\n")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 8))
    (i32.store (i32.const 4) (i32.const 3))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 20)))))"#;
        let dir = tempfile::tempdir().unwrap();
        let out = run_wasi_module(wat.as_bytes(), dir.path(), "/tmp", &[], &[], &WasmLimits::default())
            .expect("wasi module should run");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, "hi\n");
        assert_eq!(out.stderr, "");
    }

    #[test]
    fn fuel_exhaustion_is_bounded_not_hung() {
        // An infinite loop must be stopped by the fuel bound (P-3.7), not hang.
        let wat = r#"(module (func (export "_start") (loop br 0)))"#;
        let dir = tempfile::tempdir().unwrap();
        let limits = WasmLimits {
            fuel: 100_000,
            ..WasmLimits::default()
        };
        let err = run_wasi_module(wat.as_bytes(), dir.path(), "/tmp", &[], &[], &limits)
            .expect_err("infinite loop should hit the fuel bound");
        assert!(
            err.to_string().contains("resource limit") || err.to_string().contains("fuel"),
            "got: {err}"
        );
    }
}
