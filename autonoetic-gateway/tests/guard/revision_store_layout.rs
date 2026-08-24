//! Regression guard: the agent revision store's on-disk layout is spelled out
//! in exactly one place.
//!
//! `<gateway_dir>/revisions/agents/<agent_id>/<revision_id>/` used to be
//! open-coded at 19 call sites across `autonoetic-gateway` and `autonoetic`, in
//! two interchangeable spellings. Nothing detected an incomplete change, which
//! is what made the layout restructure in #2 a 19-site edit with no proof of
//! completeness — and what let `revisions` be coupled by *name* in places that
//! never mention the store (the bwrap secret mask, `gateway reset`).
//!
//! All of them now go through `agent::revision_paths`. This test fails if a new
//! one open-codes the layout again.
//!
//! If you are here because this test failed: call `agent_revision_dir(...)`
//! (or `agent_revisions_dir` / `agent_revisions_root` for the shallower forms)
//! instead of joining the components yourself. Inside `autonoetic-gateway` that
//! is `crate::agent::agent_revision_dir`; from the CLI and other crates it is
//! the re-export, `autonoetic_gateway::agent::agent_revision_dir`.

use std::fs;
use std::path::{Path, PathBuf};

/// The one module allowed to spell the layout out — including in its own docs.
const LAYOUT_OWNER: &str = "autonoetic-gateway/src/agent/revision_paths.rs";

/// Both spellings seen in the pre-refactor code. `join("revisions")` on its own
/// is the fragment that matters: every open-coded form started with it.
const OPEN_CODED_SPELLINGS: &[&str] = &[r#"join("revisions")"#, r#"join("revisions/agents")"#];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("gateway crate has a workspace parent")
        .to_path_buf()
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn revision_store_layout_is_centralized() {
    let root = workspace_root();
    // Production code only. Tests legitimately construct fixture trees by hand,
    // and forcing them through the accessor would make them assert the
    // accessor's opinion instead of the layout they mean to pin.
    let scanned_crates = ["autonoetic-gateway/src", "autonoetic/src", "autonoetic-types/src"];

    let mut files = Vec::new();
    for rel in scanned_crates {
        let dir = root.join(rel);
        assert!(dir.is_dir(), "expected crate source dir at {}", dir.display());
        collect_rs_files(&dir, &mut files);
    }
    // read_dir order is filesystem-dependent; sort so violation output is
    // deterministic across runs / CI.
    files.sort();
    assert!(
        files.len() > 100,
        "expected to scan the gateway + CLI sources, found only {} files",
        files.len()
    );

    let mut violations = Vec::new();
    let mut owner_seen = false;

    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == LAYOUT_OWNER {
            owner_seen = true;
            continue;
        }
        // Read failures (IO or non-UTF-8) must be loud — a silent empty body
        // would let a violation hide behind an unreadable file.
        let body = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
        for (idx, line) in body.lines().enumerate() {
            for spelling in OPEN_CODED_SPELLINGS {
                if line.contains(spelling) {
                    violations.push(format!("{}:{} {}", rel, idx + 1, line.trim()));
                }
            }
        }
    }

    assert!(
        owner_seen,
        "{LAYOUT_OWNER} not found — did the accessor module move? Update LAYOUT_OWNER."
    );
    assert!(
        violations.is_empty(),
        "the revision store layout must only be spelled out in {}; call \
         `crate::agent::agent_revision_dir(..)` (inside autonoetic-gateway) or \
         `autonoetic_gateway::agent::agent_revision_dir(..)` (elsewhere) \
         instead.\n{}",
        LAYOUT_OWNER,
        violations.join("\n")
    );
}
