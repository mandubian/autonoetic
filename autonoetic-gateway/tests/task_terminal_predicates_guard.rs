//! Regression guard for issue #740 — terminal-state predicates.
//!
//! Terminal-state membership should go through `TaskRunStatus::is_terminal()`,
//! `is_terminal_for_join()`, or `is_resumable()` (defined in
//! `autonoetic-types/src/workflow.rs`). Hand-written `matches!(...Succeeded |
//! Failed | Cancelled | Aborted ...)` blocks outside `workflow.rs` create
//! exactly the #722 Stage 2 bug class: a new variant has to find every site
//! by hand, and an off-by-one membership in one site silently blocks
//! recovery.
//!
//! This guard fails CI if production code outside the allowlist reintroduces
//! the inline `Succeeded | Failed | Cancelled | Aborted` `matches!` pattern.
//! The allowlist contains files that still have pending migrations. When
//! you migrate a caller to the predicate, remove it from the allowlist.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const SRC_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

/// Files that may still contain hand-written terminal `matches!` patterns.
/// All other source files must route through `TaskRunStatus` predicates.
const ALLOWLIST: &[&str] = &[
    // The type definition itself owns the pattern.
    "../../autonoetic-types/src/workflow.rs",
    // Pending migrations tracked by #740.
    "scheduler/workflow_store.rs", // large file, transitioning incrementally
    "runtime/tools/workflow.rs",  // tool-side status checks, transitioning
    "scheduler.rs",                // broad sweep of join/active checks
];

/// The terminal-state membership pattern we're guarding against. The `|`
/// between the four variants is mandatory (we only care about the AND-of-OR
/// shape; an `Succeeded | ...` inside a *different* match still trips the
/// guard, which is what we want — those should also be routed through
/// predicates).
const PATTERN: &str = "Succeeded | Failed | Cancelled | Aborted";

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn allowed_files() -> HashSet<String> {
    ALLOWLIST.iter().map(|s| s.to_string()).collect()
}

fn relative_to_src(path: &Path) -> Option<String> {
    let stripped = path.strip_prefix(SRC_ROOT).ok()?;
    Some(stripped.to_string_lossy().replace('\\', "/"))
}

fn strip_test_and_cfg_items(content: &str) -> String {
    // Strip line comments and block comments. We don't try to handle strings
    // or lifetimes — the pattern we're guarding against doesn't appear inside
    // doc comments or string literals in practice, and the false-positive
    // cost of skipping an entire file is higher than missing a comment.
    let mut out = String::with_capacity(content.len());
    let mut in_block = false;
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if in_block {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block = false;
            }
        } else if c == '/' && chars.peek() == Some(&'/') {
            // Skip to end of line.
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_block = true;
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn no_handwritten_terminal_matches_outside_workflow() {
    let mut files = Vec::new();
    collect_rs_files(Path::new(SRC_ROOT), &mut files);

    let allowed = allowed_files();
    let mut violations = Vec::new();

    for path in &files {
        let rel = match relative_to_src(path) {
            Some(r) => r,
            None => continue,
        };
        if allowed.contains(&rel) {
            continue;
        }
        let raw = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let stripped = strip_test_and_cfg_items(&raw);
        if stripped.contains(PATTERN) {
            violations.push(rel);
        }
    }

    assert!(
        violations.is_empty(),
        "Hand-written terminal `matches!` blocks found outside allowlist. \
         Route through `TaskRunStatus::is_terminal()` / `is_terminal_for_join()` \
         / `is_resumable()` instead. Violating files:\n  {}",
        violations.join("\n  ")
    );
}
