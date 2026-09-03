//! Constitution P-7.22: Sandbox-escape-attempt accounting.
//!
//! Sandbox escape indicators (SIGSYS, seccomp denials, mount/ptrace attempts)
//! are detected in sandbox output, recorded per session, and counted.
//! Crossing the degradation threshold triggers P-7.18 degraded mode;
//! crossing the emergency threshold triggers emergency stop.


use autonoetic_gateway::runtime::tools::sandbox::detect_sandbox_escape_indicators;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use std::sync::Arc;

#[test]
fn p_7_22_detects_sigsys_exit_code() {
    let attempts = detect_sandbox_escape_indicators("some stderr", Some(159));
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].indicator, "SIGSYS");
}

#[test]
fn p_7_22_detects_bad_system_call_in_stderr() {
    let attempts = detect_sandbox_escape_indicators("Bad system call", None);
    assert!(!attempts.is_empty());
    assert!(attempts.iter().any(|a| a.indicator == "SIGSYS"));
}

#[test]
fn p_7_22_detects_seccomp_violation() {
    let attempts = detect_sandbox_escape_indicators("seccomp violation detected", None);
    assert!(!attempts.is_empty());
    assert!(attempts.iter().any(|a| a.indicator == "SECCOMP_DENY"));
}

#[test]
fn p_7_22_detects_operation_not_permitted() {
    let attempts = detect_sandbox_escape_indicators("mount: Operation not permitted", None);
    assert!(attempts.iter().any(|a| a.indicator == "SECCOMP_DENY"));
}

#[test]
fn p_7_22_detects_mount_attempt() {
    let attempts = detect_sandbox_escape_indicators("mount: /dev/sda1 is write-protected", None);
    assert!(attempts.iter().any(|a| a.indicator == "ESCAPE_SYSCALL"));
}

#[test]
fn p_7_22_detects_ptrace_reference() {
    let attempts = detect_sandbox_escape_indicators("ptrace: Operation not permitted", None);
    assert!(attempts.iter().any(|a| a.indicator == "ESCAPE_SYSCALL"));
}

#[test]
fn p_7_22_detects_proc_self_exe() {
    let attempts = detect_sandbox_escape_indicators("cat /proc/self/exe", None);
    assert!(attempts.iter().any(|a| a.indicator == "ESCAPE_SYSCALL"));
}

#[test]
fn p_7_22_no_false_positives_on_clean_output() {
    let attempts = detect_sandbox_escape_indicators("hello world\nall good\n", Some(0));
    assert!(attempts.is_empty());
}

#[test]
fn p_7_22_records_escape_attempt_to_db() -> anyhow::Result<()> {
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
fn p_7_22_counts_multiple_attempts_per_session() -> anyhow::Result<()> {
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
fn p_7_22_config_default_thresholds() {
    let config = autonoetic_types::config::GatewayConfig::default();
    assert_eq!(config.escape_attempt_degrade_threshold, 5);
    assert_eq!(config.escape_attempt_emergency_threshold, 20);
}

#[test]
fn p_7_22_counts_are_session_scoped() -> anyhow::Result<()> {
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

    assert_eq!(
        store.count_sandbox_escape_attempts_for_session("sess-a")?,
        3
    );
    assert_eq!(
        store.count_sandbox_escape_attempts_for_session("sess-b")?,
        3
    );
    assert_eq!(store.count_sandbox_escape_attempts_for_root("root-x")?, 6);

    Ok(())
}

#[test]
fn p_7_22_threshold_zero_means_disabled() {
    let config = autonoetic_types::config::GatewayConfig {
        escape_attempt_degrade_threshold: 0,
        escape_attempt_emergency_threshold: 0,
        ..Default::default()
    };
    assert_eq!(config.escape_attempt_degrade_threshold, 0);
    assert_eq!(config.escape_attempt_emergency_threshold, 0);
}

#[test]
fn p_7_22_sessions_exceeding_threshold_returns_matching_sessions() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    for i in 0..6 {
        store.record_sandbox_escape_attempt(
            "sess-degrade",
            "root-degrade",
            "agent-1",
            "SIGSYS",
            &format!("attempt {}", i),
            Some(159),
        )?;
    }
    for i in 0..3 {
        store.record_sandbox_escape_attempt(
            "sess-ok",
            "root-ok",
            "agent-1",
            "ESCAPE_SYSCALL",
            &format!("attempt {}", i),
            None,
        )?;
    }

    let above5 = store.sessions_exceeding_escape_threshold(5)?;
    assert_eq!(above5.len(), 1);
    assert_eq!(above5[0].0, "sess-degrade");
    assert!(above5[0].2 >= 5);

    let above2 = store.sessions_exceeding_escape_threshold(2)?;
    assert_eq!(above2.len(), 2);

    let above10 = store.sessions_exceeding_escape_threshold(10)?;
    assert!(above10.is_empty());

    Ok(())
}

#[test]
fn p_7_22_sessions_exceeding_threshold_zero_returns_empty() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    for i in 0..50 {
        store.record_sandbox_escape_attempt(
            "sess-many",
            "root-many",
            "agent-1",
            "SIGSYS",
            &format!("attempt {}", i),
            Some(159),
        )?;
    }

    let result = store.sessions_exceeding_escape_threshold(0)?;
    assert!(result.is_empty(), "threshold=0 must disable matching");

    Ok(())
}

#[test]
fn p_7_22_emit_escape_threshold_event_creates_causal_event() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    store.emit_escape_threshold_event("sess-1", "root-1", 7, 5, "degradation")?;

    let events = store.search_causal_events(Some("sess-1"), None, 10)?;
    assert_eq!(events.len(), 1);
    assert!(events[0].action.contains("escape_threshold_degradation"));
    assert_eq!(events[0].category, "security");

    Ok(())
}

#[test]
fn p_7_22_escape_threshold_surfaces_on_timeline() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    store.emit_escape_threshold_event("sess-esc", "root-esc", 7, 5, "degradation")?;

    let timeline = store.list_session_timeline("root-esc", None, 50, None, None)?;
    let ev = timeline
        .entries
        .iter()
        .find(|e| e.event_type == "security.escape_threshold")
        .expect("escape threshold must surface on timeline");
    assert_eq!(
        ev.altitude,
        autonoetic_types::session_timeline::Altitude::Attention
    );
    let payload: serde_json::Value =
        serde_json::from_str(ev.payload.as_deref().unwrap()).unwrap();
    assert_eq!(payload["level"], "degradation");
    assert_eq!(payload["count"], 7);
    assert_eq!(payload["threshold"], 5);

    Ok(())
}
