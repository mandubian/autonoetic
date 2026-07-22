//! Issue #853: per-host `sandbox_exec` probe budget.
//!
//! The budget's logic is unit-tested in `runtime::host_probe_budget`; these
//! integration tests cover the two seams the `sandbox_exec` dispatch relies on:
//! the host-extraction plumbing (`normalize_targets` over the remote-access
//! analyzer) and the store-level trip event that surfaces the cap to operators.

use std::sync::Arc;

use autonoetic_gateway::runtime::approved_exec_cache::normalize_targets;
use autonoetic_gateway::runtime::host_probe_budget::{content_hash, ProbeOutcome};
use autonoetic_gateway::runtime::remote_access::RemoteAccessAnalyzer;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

fn open_store() -> (tempfile::TempDir, Arc<GatewayStore>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(GatewayStore::open(&tmp.path().join(".gateway")).expect("open store"));
    (tmp, store)
}

/// The exact command shape from `session-0718349d`: a python one-liner fetching
/// a host. The pre-check/record both key off `normalize_targets` over the
/// remote-access analyzer, so this must extract the host the agent is probing.
#[test]
fn host_extraction_from_realistic_probe_command() {
    let cmd = "python3 -c \"import urllib.request; \
               print(urllib.request.urlopen('https://open-meteo.com/en/docs').read())\"";
    let analysis = RemoteAccessAnalyzer::analyze_code(cmd);
    let hosts = normalize_targets(&analysis.detected_patterns);
    assert!(
        hosts.contains(&"open-meteo.com".to_string()),
        "expected open-meteo.com among extracted hosts, got {hosts:?}"
    );
}

/// End-to-end at the store level: identical SPA content strikes until the cap,
/// after which the pre-check (`exhausted`) refuses the next probe, and the trip
/// emits an operator-visible causal event keyed to the root session.
#[test]
fn duplicate_content_exhausts_budget_and_emits_operator_event() -> anyhow::Result<()> {
    let (_tmp, store) = open_store();
    store.host_probe_budget.set_cap(3);

    let sid = "root-x/researcher.default-db9d1d2b";
    let host = "open-meteo.com";
    let spa = content_hash("<!doctype html><div id=svelte>weather SPA shell</div>");

    // First fetch is new information → progress, not a strike.
    assert_eq!(
        store.host_probe_budget.record(sid, host, true, &spa),
        ProbeOutcome::Progress
    );
    // Each subsequent identical (exit-0) fetch is a wasted probe.
    for expected in 1..=3u32 {
        match store.host_probe_budget.record(sid, host, true, &spa) {
            ProbeOutcome::Strike { strikes, duplicate, .. } => {
                assert_eq!(strikes, expected);
                assert!(duplicate, "same content ⇒ duplicate strike");
            }
            other => panic!("expected a strike, got {other:?}"),
        }
    }

    // The pre-check the tool runs before executing now refuses the host.
    assert_eq!(store.host_probe_budget.exhausted(sid, host), Some(3));

    // The trip is surfaced to the operator as a queryable causal event with the
    // host as target.
    store.emit_host_probe_budget_exhausted_event(
        sid,
        "root-x",
        "researcher.default-db9d1d2b",
        host,
        3,
        3,
    )?;
    let events = store.search_causal_events(Some(sid), None, 50)?;
    assert!(
        events.iter().any(|e| e.action == "sandbox.host_budget_exhausted"
            && e.target.as_deref() == Some(host)),
        "expected a sandbox.host_budget_exhausted causal event targeting {host}"
    );
    Ok(())
}

/// A fresh store has cap 0 (the daemon sets it from config at startup); with the
/// budget disabled nothing is tracked and no host is ever refused.
#[test]
fn budget_disabled_when_cap_unset() {
    let (_tmp, store) = open_store();
    let sid = "root-y/researcher.default-aaaa1111";
    for _ in 0..10 {
        assert_eq!(
            store.host_probe_budget.record(sid, "example.com", false, ""),
            ProbeOutcome::Disabled
        );
    }
    assert_eq!(store.host_probe_budget.exhausted(sid, "example.com"), None);
}

/// A failing host also exhausts the budget, and clearing the session (session
/// close) frees it so a re-spawn gets a fresh budget.
#[test]
fn failures_exhaust_and_session_clear_frees_budget() {
    let (_tmp, store) = open_store();
    store.host_probe_budget.set_cap(2);
    let sid = "root-z/researcher.default-bbbb2222";
    let host = "api.open-meteo.com";
    let h = content_hash("connection refused");

    assert!(matches!(
        store.host_probe_budget.record(sid, host, false, &h),
        ProbeOutcome::Strike { strikes: 1, .. }
    ));
    assert!(matches!(
        store.host_probe_budget.record(sid, host, false, &h),
        ProbeOutcome::Strike { strikes: 2, reached_cap: true, .. }
    ));
    assert_eq!(store.host_probe_budget.exhausted(sid, host), Some(2));

    // Session close clears the budget.
    store.host_probe_budget.clear_session(sid);
    assert_eq!(store.host_probe_budget.exhausted(sid, host), None);
}
