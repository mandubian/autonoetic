//! Regression guard for the agent-prompt factorization (#466, roadmap item F).
//!
//! Doctrine that has been centralized into tool-contributed guidance blocks must
//! NOT be re-pasted into individual `SKILL.md` files — that is exactly the
//! "built on the fly" duplication the migration removed (and where three latent
//! prose-vs-enforcement inaccuracies had drifted). This test fails if any
//! `agents/**/SKILL.md` re-introduces a migrated doctrine phrase.
//!
//! When you centralize a new doctrine block, add its distinctive fingerprint
//! to `MIGRATED_DOCTRINE_FINGERPRINTS` in `runtime::guidance` (the doctrine's
//! home module — also consulted by the create-time scan, RFC #799 F.4b). Keep
//! fingerprints specific enough that they only match the migrated prose, not
//! legitimately-kept role-specific wording.

use std::fs;
use std::path::{Path, PathBuf};

use autonoetic_gateway::runtime::guidance::MIGRATED_DOCTRINE_FINGERPRINTS;

fn agents_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("agents")
}

fn collect_skill_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_skill_files(&path, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
            out.push(path);
        }
    }
}

#[test]
fn skill_md_does_not_reintroduce_migrated_doctrine() {
    let root = agents_root();
    assert!(root.is_dir(), "agents/ directory not found at {}", root.display());

    let mut files = Vec::new();
    collect_skill_files(&root, &mut files);
    // read_dir order is filesystem-dependent; sort so violation output is
    // deterministic across runs / CI.
    files.sort();
    assert!(
        files.len() > 10,
        "expected many SKILL.md files under {}, found {}",
        root.display(),
        files.len()
    );

    let mut violations = Vec::new();
    for file in &files {
        // Read failures (IO or non-UTF-8) must be loud — a silent empty body
        // would let drift hide behind an unreadable file.
        let body = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
        let rel = file.strip_prefix(&root).unwrap_or(file);
        for (fingerprint, owner) in MIGRATED_DOCTRINE_FINGERPRINTS.iter().copied() {
            if body.contains(fingerprint) {
                violations.push(format!(
                    "  agents/{}: re-introduces migrated doctrine \"{}\"\n    \
                     → this now lives in the {} block; delete the prose. \
                     See docs/design/agent-prompt-factorization.md.",
                    rel.display(),
                    fingerprint,
                    owner
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "SKILL.md doctrine drift detected ({} violation(s)):\n{}",
        violations.len(),
        violations.join("\n")
    );
}
