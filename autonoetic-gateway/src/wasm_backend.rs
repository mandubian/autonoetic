//! WASM execution backend (RFC `docs/rfc/portable-wasm-execution-tier.md`, P4).
//!
//! Gated behind the `wasm-tier` Cargo feature so the native build never pays
//! wasmtime's compile-time/binary-size cost. This first increment embeds the
//! runtime and proves a module instantiates and runs in-process; WASI preopens,
//! stdout/stderr capture, the host-function SDK bridge, and resource limits land
//! in subsequent increments.

use wasmtime::{Engine, Instance, Module, Store};

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
}
