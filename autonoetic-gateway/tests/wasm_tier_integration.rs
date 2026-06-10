//! P4 inc 2c — the wasm tier wired into the unified execution entry.
//! A `sandbox: "wasm"` request runs in-process through `run_to_output` end to
//! end (no child process), capturing stdout + exit code. Compiles only with the
//! `wasm-tier` feature.
#![cfg(feature = "wasm-tier")]

use autonoetic_gateway::exec_request::{CodeSource, ExecutionKind};
use autonoetic_gateway::sandbox::{SandboxDriverKind, SandboxRunner};
use tempfile::tempdir;

#[test]
fn wasm_agent_entry_runs_through_run_to_output() {
    let dir = tempdir().unwrap();
    // The agent's entry is a WASI module that writes "hi\n" to stdout. The
    // runtime compiles WAT directly, so the entry file can carry WAT text.
    let wat = r#"(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 8) "hi\n")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 8))
    (i32.store (i32.const 4) (i32.const 3))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 20)))))"#;
    std::fs::write(dir.path().join("main.wasm"), wat).unwrap();

    let request = ExecutionKind::Code {
        language: None,
        source: CodeSource::Entry("main.wasm".to_string()),
        args: vec![],
    };

    let out = SandboxRunner::run_to_output(
        SandboxDriverKind::Wasm,
        dir.path().to_str().unwrap(),
        &request,
        None,
        None,
        &[],
        None,
        None,
    )
    .expect("wasm agent entry should run via run_to_output");

    assert_eq!(out.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    assert!(out.stderr.is_empty());
}

#[test]
fn wasm_agent_receives_stdin_through_run_to_output() {
    let dir = tempdir().unwrap();
    // Echo entry: read up to 16 bytes from stdin, write them back to stdout.
    let wat = r#"(module
  (import "wasi_snapshot_preview1" "fd_read"
    (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 16))
    (i32.store (i32.const 4) (i32.const 16))
    (drop (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 8)))
    (i32.store (i32.const 0) (i32.const 16))
    (i32.store (i32.const 4) (i32.load (i32.const 8)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 12)))))"#;
    std::fs::write(dir.path().join("main.wasm"), wat).unwrap();

    let request = ExecutionKind::Code {
        language: None,
        source: CodeSource::Entry("main.wasm".to_string()),
        args: vec![],
    };

    let out = SandboxRunner::run_to_output(
        SandboxDriverKind::Wasm,
        dir.path().to_str().unwrap(),
        &request,
        None,
        None,
        &[],
        None,
        Some(b"pong\n"),
    )
    .expect("wasm agent should receive stdin via run_to_output");

    assert_eq!(out.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "pong\n");
}

#[test]
fn wasm_tier_rejects_free_form_shell() {
    let dir = tempdir().unwrap();
    let request = ExecutionKind::shell("echo hi");
    let err = SandboxRunner::run_to_output(
        SandboxDriverKind::Wasm,
        dir.path().to_str().unwrap(),
        &request,
        None,
        None,
        &[],
        None,
        None,
    )
    .expect_err("wasm tier must reject free-form shell");
    assert!(
        err.to_string().contains("shell"),
        "error should explain shell is unsupported: {err}"
    );
}

#[test]
fn wasm_tier_rejects_path_traversal_entry() {
    let dir = tempdir().unwrap();
    // A `..` entry must not escape the agent dir to read/execute a foreign module.
    let request = ExecutionKind::Code {
        language: None,
        source: CodeSource::Entry("../escape.wasm".to_string()),
        args: vec![],
    };
    let err = SandboxRunner::run_to_output(
        SandboxDriverKind::Wasm,
        dir.path().to_str().unwrap(),
        &request,
        None,
        None,
        &[],
        None,
        None,
    )
    .expect_err("wasm tier must reject path-traversal entries");
    assert!(
        err.to_string().contains(".."),
        "error should explain the entry must stay within the agent dir: {err}"
    );
}
