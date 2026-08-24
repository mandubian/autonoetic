//! Regression guard: the gateway directory is **read from config**, never
//! derived from another path.
//!
//! It used to be `config.agents_dir.join(".gateway")`, and 52 production sites
//! spelled that out inline instead of calling `gateway_root_dir(config)`. Once
//! the expression is loose in the codebase, code that holds only *some* path
//! starts reconstructing it by walking — and four separate bugs came out of
//! that, each silent:
//!
//! - `bwrap_deny_path_flags` derived it as `agent_dir.parent()/.gateway`. Agents
//!   execute from inside the revision store, so that resolved to nothing and the
//!   sandbox secret mask emitted **zero flags** — `vault.key`, `vault.enc.json`,
//!   `gateway.db`, the identity key and every session transcript readable from
//!   inside the sandbox (#1145).
//! - `gateway_dir_from_agent_dir` did the same for the SDK bridge, and *created*
//!   the bogus directory, pointing agent memory at a stray per-revision path.
//! - Three call sites did `gateway_dir.parent()` to cancel a `.gateway` that the
//!   vault helpers re-appended — two hops that agreed only by coincidence.
//! - `JsonRpcRouter`'s default was the relative `".gateway"`, resolved against
//!   whatever CWD the process had.
//!
//! None of these failed loudly. Each produced a path that simply didn't exist,
//! and the code around them treats a missing path as "nothing to do".
//!
//! So: no `".gateway"` literal in production code, and no reconstructing the
//! gateway dir from an agent dir. `runtime_dir` is a config field;
//! `gateway_root_dir(config)` returns it verbatim; pass it down.

use std::fs;
use std::path::{Path, PathBuf};

/// Production sources only. Test fixtures legitimately build directory trees by
/// hand and may name them anything — what matters is that no *shipping* code
/// reconstructs the path.
const SCANNED_CRATE_DIRS: &[&str] = &[
    "autonoetic-gateway/src",
    "autonoetic/src",
    "autonoetic-types/src",
];

/// Files allowed to mention the string, because they explain the history.
const EXEMPT: &[&str] = &[
    // `gateway_root_dir` documents what it replaced.
    "autonoetic-gateway/src/execution.rs",
    // The config field documents the old layout it supersedes.
    "autonoetic-types/src/config.rs",
    // The vault helpers document why they no longer append it.
    "autonoetic-gateway/src/vault.rs",
    // The driver documents the mask bug in its own doc comment.
    "autonoetic-gateway/src/sandbox/driver/bubblewrap.rs",
    // The SDK bridge documents the deleted helper.
    "autonoetic-gateway/src/sandbox.rs",
];

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

/// Everything up to the first `#[cfg(test)]` — the shipping half of the file.
fn production_lines(body: &str) -> impl Iterator<Item = (usize, &str)> {
    let cut = body
        .lines()
        .position(|l| l.trim_start().starts_with("#[cfg(test)]"))
        .unwrap_or(usize::MAX);
    body.lines()
        .enumerate()
        .take_while(move |(i, _)| *i < cut)
        .map(|(i, l)| (i + 1, l))
}

fn scan(check: impl Fn(&str) -> bool) -> Vec<String> {
    let root = workspace_root();
    let mut files = Vec::new();
    for rel in SCANNED_CRATE_DIRS {
        let dir = root.join(rel);
        assert!(dir.is_dir(), "expected crate source dir at {}", dir.display());
        collect_rs_files(&dir, &mut files);
    }
    files.sort();
    assert!(
        files.len() > 100,
        "expected to scan the gateway + CLI sources, found only {}",
        files.len()
    );

    let mut violations = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        if EXEMPT.contains(&rel.as_str()) {
            continue;
        }
        let body = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
        for (line_no, line) in production_lines(&body) {
            let code = line.split("//").next().unwrap_or(line);
            if check(code) {
                violations.push(format!("{rel}:{line_no} {}", line.trim()));
            }
        }
    }
    violations
}

#[test]
fn no_production_code_spells_out_the_gateway_dir_name() {
    let violations = scan(|code| code.contains("\".gateway\""));
    assert!(
        violations.is_empty(),
        "the gateway directory is `config.runtime_dir` — call \
         `crate::execution::gateway_root_dir(config)` instead of naming \
         `.gateway`:\n{}",
        violations.join("\n")
    );
}

#[test]
fn no_production_code_derives_the_gateway_dir_from_an_agent_dir() {
    // `<something>.parent()` on a line that also mentions a gateway/vault dir:
    // the shape every one of the four bugs had.
    let violations = scan(|code| {
        code.contains(".parent()")
            && (code.contains("gateway_dir")
                || code.contains("runtime_dir")
                || code.contains("vault_dir")
                || code.contains("gw_dir"))
    });
    assert!(
        violations.is_empty(),
        "the gateway dir must be threaded from config, not reconstructed by \
         walking up from an agent dir (#1145):\n{}",
        violations.join("\n")
    );
}
