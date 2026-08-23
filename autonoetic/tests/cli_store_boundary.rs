//! CLI ↔ gateway.db boundary guard (#1119).
//!
//! The CLI is a client of the gateway API, not a direct reader of
//! `gateway.db` (Separation of Powers). Migration to JSON-RPC happens in
//! tranches; this test pins the boundary so it only ever shrinks:
//!
//! - a file NOT in `ALLOWED` may not mention `GatewayStore` at all
//! - a file in `ALLOWED` still owes migration — remove it from the list in
//!   the same PR that migrates it, or the test fails the other way
//!   (`ALLOWED` entries must still contain the marker; stale entries are
//!   pruned so the list cannot rot)
//!
//! Legitimate in-process use (gateway start / agent execution / chat TUI
//! orchestrator) lives in the allowlisted files until their tranche lands.

use std::fs;
use std::path::PathBuf;

/// Files under `autonoetic/src/cli/` still allowed to reference GatewayStore.
/// Every entry must be removed in the PR that migrates it — except
/// `capsule.rs`, which is a permanent, by-design exception: capsule
/// export/import/verify run OFFLINE (import targets a receiver gateway that
/// has never booted, so there is no gateway to RPC; see #1119 tranche 3).
const ALLOWED: &[&str] = &[
    "agent.rs",
    "capsule.rs", // offline-by-design (see comment above)
    "chat.rs",
    "gateway.rs",
    "improve.rs",
    "sentinel_experiment.rs",
    "trace.rs",
    "watchdog.rs",
];

#[test]
fn cli_files_do_not_touch_gateway_store_outside_the_migration_allowlist() {
    let cli_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("cli");
    let mut offenders: Vec<String> = Vec::new();
    let mut stale_allowlist: Vec<&str> = Vec::new();
    let mut checked = 0usize;

    let mut entries: Vec<PathBuf> = fs::read_dir(&cli_dir)
        .expect("src/cli should be readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    // Nested modules (room/, etc.) count too.
    let mut stack = entries;
    let mut files = Vec::new();
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            for e in fs::read_dir(&path).expect("subdir readable").flatten() {
                stack.push(e.path());
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    files.sort();

    for path in files {
        let rel = path
            .strip_prefix(&cli_dir)
            .expect("path under src/cli")
            .to_string_lossy()
            .replace('\\', "/");
        let body = fs::read_to_string(&path).unwrap_or_default();
        checked += 1;
        if body.contains("GatewayStore") {
            if !ALLOWED.contains(&rel.as_str()) {
                offenders.push(rel);
            }
        } else if ALLOWED.contains(&rel.as_str()) {
            stale_allowlist.push(rel.leak());
        }
    }

    assert!(
        offenders.is_empty(),
        "new GatewayStore usage in CLI files outside the #1119 migration allowlist: {offenders:?} \
         — route reads through JSON-RPC (crate::cli::rpc::GatewayRpc) instead"
    );
    assert!(
        stale_allowlist.is_empty(),
        "stale #1119 allowlist entries (file no longer references GatewayStore): \
         {stale_allowlist:?} — remove them from ALLOWED in this test"
    );
    assert!(checked > 0, "guard found no CLI sources — is the path wrong?");
}
