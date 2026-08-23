//! CLI ↔ gateway.db boundary guard (#1119).
//!
//! The CLI is a client of the gateway API, not a direct reader of
//! `gateway.db` (Separation of Powers). Migration to JSON-RPC happens in
//! tranches; this test pins the boundary so both lists only ever shrink:
//!
//! - a file in NEITHER list may not mention `GatewayStore` at all
//! - a file in `PENDING_MIGRATION` still owes migration — remove it in the
//!   same PR that migrates it
//! - a file in `BY_DESIGN_EMBEDDING` legitimately runs gateway machinery
//!   in-process (agent executors, native tool execution, offline capsule
//!   transfer) and structurally needs a store handle; each entry carries
//!   its rationale inline
//! - stale entries in either list (no longer referencing GatewayStore)
//!   fail the test so the lists cannot rot

use std::fs;
use std::path::PathBuf;

/// Files that still read/write gateway.db where an RPC should exist.
/// Every entry must be removed in the PR that migrates it.
const PENDING_MIGRATION: &[&str] = &[
    // Interactive approval/gate loop + grants + pending-interaction answers.
    // Many calls already have RPCs (approvals.*, gate.*, grants.*,
    // interaction.answer); the `gateway start` embedding stays regardless —
    // when migrating, split the read paths out of the start path.
    "gateway.rs",
    // Display commands over causal events / contract & civic health / forks.
    // Largest remaining read surface; needs trace.* RPCs.
    "trace.rs",
];

/// Files that run gateway machinery in-process and therefore hold a store
/// handle by design — NOT pending migration. Each comment is the rationale;
/// changing a file's classification requires changing it here.
const BY_DESIGN_EMBEDDING: &[&str] = &[
    // `agent run`/`agent bootstrap`: embeds AgentExecutor.
    "agent.rs",
    // Capsule export/import: offline transfer format — import targets a
    // receiver gateway that may have never booted (no gateway to RPC).
    "capsule.rs",
    // Chat TUI: embeds the in-process orchestrator shared with the gateway.
    "chat.rs",
    // improve: orchestrates native tools (promote, github_issue, ab_replay)
    // that require Arc<GatewayStore> + writes revision dirs + interactive
    // operator approval.
    "improve.rs",
    // sentinel-experiment: builds AgentExecutors and measures their store
    // side-effect rows around each run.
    "sentinel_experiment.rs",
    // watchdog: embeds the watchdog agent's AgentExecutor.
    "watchdog.rs",
];

#[test]
fn cli_files_do_not_touch_gateway_store_outside_the_guard_lists() {
    let cli_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("cli");
    let mut offenders: Vec<String> = Vec::new();
    let mut stale: Vec<&str> = Vec::new();
    let mut checked = 0usize;

    let mut stack: Vec<PathBuf> = fs::read_dir(&cli_dir)
        .expect("src/cli should be readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
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
        let references_store = body.contains("GatewayStore");
        let listed = PENDING_MIGRATION.contains(&rel.as_str())
            || BY_DESIGN_EMBEDDING.contains(&rel.as_str());
        if references_store && !listed {
            offenders.push(rel);
        } else if !references_store && listed {
            stale.push(rel.leak());
        }
    }

    assert!(
        offenders.is_empty(),
        "new GatewayStore usage in CLI files outside the #1119 guard lists: {offenders:?} \
         — route reads through JSON-RPC (crate::cli::rpc::GatewayRpc) instead, or classify \
         the file as by-design embedding with a rationale in this test"
    );
    assert!(
        stale.is_empty(),
        "stale #1119 guard entries (file no longer references GatewayStore): \
         {stale:?} — remove them from the lists in this test"
    );
    assert!(checked > 0, "guard found no CLI sources — is the path wrong?");
}
