//! Constitution R++8: Sandbox-escape-attempt accounting.
//!
//! Sandbox escape indicators (SIGSYS, seccomp denials, mount/ptrace attempts)
//! are detected in sandbox output, recorded per session, and counted.
//! Crossing the degradation threshold triggers R-7.18 degraded mode;
//! crossing the emergency threshold triggers emergency stop.

mod support;

use autonoetic_gateway::runtime::tools::sandbox::detect_sandbox_escape_indicators;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use std::sync::Arc;

#[test]
fn r_plus_plus_8_detects_sigsys_exit_code() {
    let attempts = detect_sandbox_escape_indicators("some stderr", Some(159));
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].indicator, "SIGSYS");
}

#[test]
fn r_plus_plus_8_detects_bad_system_call_in_stderr() {
    let attempts = detect_sandbox_escape_indicators("Bad system call", None);
    assert!(!attempts.is_empty());
    assert!(attempts.iter().any(|a| a.indicator == "SIGSYS"));
}

#[test]
fn r_plus_plus_8_detects_seccomp_violation() {
    let attempts = detect_sandbox_escape_indicators("seccomp violation detected", None);
    assert!(!attempts.is_empty());
    assert!(attempts.iter().any(|a| a.indicator == "SECCOMP_DENY"));
}

#[test]
fn r_plus_plus_8_detects_operation_not_permitted() {
    let attempts = detect_sandbox_escape_indicators("mount: Operation not permitted", None);
    assert!(attempts.iter().any(|a| a.indicator == "SECCOMP_DENY"));
}

#[test]
fn r_plus_plus_8_detects_mount_attempt() {
    let attempts = detect_sandbox_escape_indicators("mount: /dev/sda1 is write-protected", None);
    assert!(attempts.iter().any(|a| a.indicator == "ESCAPE_SYSCALL"));
}

#[test]
fn r_plus_plus_8_detects_ptrace_reference() {
    let attempts = detect_sandbox_escape_indicators("ptrace: Operation not permitted", None);
    assert!(attempts.iter().any(|a| a.indicator == "ESCAPE_SYSCALL"));
}

#[test]
fn r_plus_plus_8_detects_proc_self_exe() {
    let attempts = detect_sandbox_escape_indicators("cat /proc/self/exe", None);
    assert!(attempts.iter().any(|a| a.indicator == "ESCAPE_SYSCALL"));
}

#[test]
fn r_plus_plus_8_no_false_positives_on_clean_output() {
    let attempts = detect_sandbox_escape_indicators("hello world\nall good\n", Some(0));
    assert!(attempts.is_empty());
}

#[test]
fn r_plus_plus_8_records_escape_attempt_to_db() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    store.record_sandbox_escape_attempt(
        "sess-1",
        "root-1",
        "agent-1",
        "SIGSYS",
        "Bad system call detected",
        Some(159),
    )?;

    let count = store.count_sandbox_escape_attempts_for_session("sess-1")?;
    assert_eq!(count, 1);

    let root_count = store.count_sandbox_escape_attempts_for_root("root-1")?;
    assert_eq!(root_count, 1);

    Ok(())
}

#[test]
fn r_plus_plus_8_counts_multiple_attempts_per_session() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    for i in 0..5 {
        store.record_sandbox_escape_attempt(
            "sess-2",
            "root-2",
            "agent-2",
            "ESCAPE_SYSCALL",
            &format!("mount attempt {}", i),
            None,
        )?;
    }

    let count = store.count_sandbox_escape_attempts_for_session("sess-2")?;
    assert_eq!(count, 5);

    Ok(())
}

#[test]
fn r_plus_plus_8_config_default_thresholds() {
    let config = autonoetic_types::config::GatewayConfig::default();
    assert_eq!(config.escape_attempt_degrade_threshold, 5);
    assert_eq!(config.escape_attempt_emergency_threshold, 20);
}

#[test]
fn r_plus_plus_8_counts_are_session_scoped() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    for i in 0..3 {
        store.record_sandbox_escape_attempt(
            "sess-a",
            "root-x",
            "agent-1",
            "SIGSYS",
            &format!("attempt {}", i),
            Some(159),
        )?;
        store.record_sandbox_escape_attempt(
            "sess-b",
            "root-x",
            "agent-1",
            "ESCAPE_SYSCALL",
            &format!("attempt {}", i),
            None,
        )?;
    }

    assert_eq!(store.count_sandbox_escape_attempts_for_session("sess-a")?, 3);
    assert_eq!(store.count_sandbox_escape_attempts_for_session("sess-b")?, 3);
    assert_eq!(store.count_sandbox_escape_attempts_for_root("root-x")?, 6);

    Ok(())
}
