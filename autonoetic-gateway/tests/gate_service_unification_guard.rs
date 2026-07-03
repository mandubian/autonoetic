//! Regression guard for the human-gate unification (#724).
//!
//! Approval-shaped decisions must flow through `GateService::check`, not call
//! `GatewayStore::create_approval` or `GatewayStore::create_escalation` directly.
//! Direct callers bypass typed `DecisionContext` enforcement, dedup, enrichment,
//! and the root-scoped identical-action join introduced in #723 (merged via PR #733).
//!
//! The allowlist below contains files that still have pending migrations.
//! When you migrate a caller to `GateService`, remove it from the allowlist.
//! Do not add new entries without a corresponding issue.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const SRC_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

/// Files that are permitted to call `create_approval` / `create_escalation`
/// directly. All other source files must route approval-shaped decisions through
/// `GateService`. Paths are relative to `autonoetic-gateway/src`.
const ALLOWLIST: &[&str] = &[
    // GateService itself — the single allowed caller.
    "runtime/human_gate.rs",
    // Pending migrations tracked by #724.
    "runtime/lifecycle.rs",              // SessionContinue
    "runtime/tools/credential.rs",       // CredentialPrompt
    "runtime/tools/federation.rs",       // promotion-review double-write
    "runtime/tools/plan_frame.rs",       // PlanFrame
    // Additional direct callers not yet classified as approval subjects.
    "post_promotion_review.rs",
    "runtime/tools/session.rs",
    "runtime/tools/user_profile.rs",
    "scheduler/gateway_store/session_envelopes.rs",
    "scheduler/runner.rs",
];

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

/// True if the line is inside a `#[cfg(test)]` module or a `mod tests {` block.
/// This is a best-effort textual check: it tracks open/close braces once either
/// marker is seen. The `mod tests {` opening brace on the marker line itself is
/// counted so that a bare `mod tests {` block (no preceding `#[cfg(test)]`) is
/// tracked correctly — without it, depth stays 0 and the first inner `}` would
/// prematurely end the module (review on #734).
fn is_in_test_module(lines: &[&str], idx: usize) -> bool {
    let mut depth: i32 = 0;
    let mut in_test = false;
    for (i, line) in lines.iter().enumerate() {
        if i > idx {
            break;
        }
        let trimmed = line.trim();
        // Markers must be evaluated before brace counting so the opening `{`
        // on a `mod tests {` line is included in the depth tally.
        if trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("# [cfg(test)]") {
            // A new test attribute starts a fresh module scope. Reset depth so
            // any braces accumulated before the attribute don't bleed in.
            in_test = true;
            depth = 0;
        }
        if trimmed == "mod tests {" || trimmed.starts_with("mod tests {") {
            in_test = true;
        }
        if in_test {
            for ch in line.chars() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth <= 0 {
                            in_test = false;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    in_test
}

#[test]
fn no_direct_create_approval_outside_gate_service() {
    let root = Path::new(SRC_ROOT);
    assert!(root.is_dir(), "src/ directory not found at {}", root.display());

    let mut files = Vec::new();
    collect_rs_files(root, &mut files);
    files.sort();

    let allowed: HashSet<&str> = ALLOWLIST.iter().copied().collect();
    let mut violations = Vec::new();

    for file in &files {
        let rel = file.strip_prefix(root).unwrap_or(file);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if allowed.contains(rel_str.as_str()) {
            continue;
        }

        let body = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
        let lines: Vec<&str> = body.lines().collect();

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            // Skip comments and function definitions.
            if trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("# ") || trimmed.starts_with("#[") {
                continue;
            }
            if trimmed.contains("fn create_approval(") || trimmed.contains("fn create_escalation(") {
                continue;
            }
            if line.contains("create_approval(") || line.contains("create_escalation(") {
                if is_in_test_module(&lines, i) {
                    continue;
                }
                violations.push(format!(
                    "  {}:{}: direct call to {}\n    \
                     → route this approval-shaped decision through GateService::check (#724)",
                    rel_str,
                    i + 1,
                    if line.contains("create_approval(") {
                        "create_approval"
                    } else {
                        "create_escalation"
                    }
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Direct approval/escalation creation outside GateService detected ({} violation(s)):\
         {}\n\
         If a caller is intentionally pending migration, add it to ALLOWLIST in \
         tests/gate_service_unification_guard.rs with a #724 reference.",
        violations.len(),
        violations.join("\n")
    );
}
