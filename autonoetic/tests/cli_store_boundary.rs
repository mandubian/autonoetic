//! CLI ↔ gateway.db boundary guard (#1119).
//!
//! The CLI is a client of the gateway API, not a direct reader of
//! `gateway.db` (Separation of Powers). Migration to JSON-RPC happened in
//! tranches (#1135 → #1152); this test pins the boundary:
//!
//! - a file in NEITHER list may not mention `GatewayStore` at all
//! - `PENDING_MIGRATION` is empty by design — #1119 is closed. If a new
//!   surface needs migration, the list must be re-populated and the test
//!   comment here updated; the assertion below makes reopening explicit.
//! - a file in `BY_DESIGN_EMBEDDING` legitimately runs gateway machinery
//!   in-process (agent executors, native tool execution, offline capsule
//!   transfer) and structurally needs a store handle; each entry carries
//!   its rationale inline
//! - stale entries (no longer referencing GatewayStore) fail the test so
//!   the list cannot rot

use std::fs;
use std::path::PathBuf;

/// Files that still read/write gateway.db where an RPC should exist.
/// EMPTY by design — #1119 (CLI → JSON-RPC migration) is closed.
/// Re-populate deliberately if a new read/act surface appears.
const PENDING_MIGRATION: &[&str] = &[];

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
    // gateway.rs after the #1119 tranches (approvals, escalations,
    // interactions, grants, cron, constitution proposals, egress-audit all
    // migrated to RPC): the residue is machinery/offline surfaces —
    // `gateway start` embedding, exec-cache (file cache + one audit write),
    // system-agent bootstrap, constitution release, workflow commands,
    // egress-declassify intake, memory relabel — all without RPC surfaces
    // or offline-by-design, like capsule.
    "gateway.rs",
];

#[test]
fn cli_files_do_not_touch_gateway_store_outside_the_guard_lists() {
    let cli_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("cli");
    let mut offenders: Vec<String> = Vec::new();
    let mut stale: Vec<&str> = Vec::new();
    let mut checked = 0usize;

    // #1119 is closed: no surfaces may be re-added to the pending list
    // without an explicit decision (any new read/act path must go over RPC).
    assert!(
        PENDING_MIGRATION.is_empty(),
        "#1119 is closed — PENDING_MIGRATION must stay empty. A new surface needs \
         deliberate re-opening of the issue, not an allowlist entry."
    );

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
