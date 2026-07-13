//! Regression guard for the agent-prompt factorization (#466, roadmap item F).
//!
//! Doctrine that has been centralized into tool-contributed guidance blocks must
//! NOT be re-pasted into individual `SKILL.md` files — that is exactly the
//! "built on the fly" duplication the migration removed (and where three latent
//! prose-vs-enforcement inaccuracies had drifted). This test fails if any
//! `agents/**/SKILL.md` re-introduces a migrated doctrine phrase.
//!
//! When you centralize a new doctrine block, add its distinctive fingerprint
//! here. Keep fingerprints specific enough that they only match the migrated
//! prose, not legitimately-kept role-specific wording.

use std::fs;
use std::path::{Path, PathBuf};

/// `(fingerprint, owning guidance block — where the doctrine lives now)`.
/// Each phrase was verified absent from every `SKILL.md` at migration time.
const MIGRATED_DOCTRINE_FINGERPRINTS: &[(&str, &str)] = &[
    ("Forbidden shell commands", "sandbox.forbidden_commands (sandbox_exec.guidance)"),
    (
        "requires both `name` and `content`",
        "content.write_protocol (content_write.guidance)",
    ),
    (
        "alternate names like `outcome`",
        "promotion.record_protocol (promotion_record.guidance)",
    ),
    (
        "do not invent or guess",
        "exec.approval_continuation (sandbox_exec/artifact_exec.guidance)",
    ),
    (
        "never restart from scratch",
        "resumption.workflow_state_first (workflow_state.guidance)",
    ),
    (
        "warrant a round-trip",
        "clarification.ask_or_default (builtin block)",
    ),
    (
        "wrap JSON in markdown code fences",
        "the io.returns Output Contract renderer (context.rs) — declare io.returns instead",
    ),
    (
        "Return a single raw JSON object",
        "the io.returns Output Contract renderer (context.rs) — declare io.returns instead",
    ),
    // Centralized into foundation_core.md §7 — the rights/self-describe/community
    // doctrine every agent already receives. Keep these specific enough that they
    // only match the centralized phrasing, not legitimate role-specific wording.
    (
        "Your headline rights, in force every turn",
        "foundation_core.md §7 (the constitution is your contract)",
    ),
    (
        "are one call away: `self_describe()`",
        "foundation_core.md §7 (self_describe nudge)",
    ),
    (
        "its rights bind the gateway as its rules bind you",
        "foundation_core.md §7 (community / social-contract framing)",
    ),
    (
        "standing witness contract",
        "the io.returns Output Contract renderer (context.rs) — `anomalies` is gateway-injected (RFC C.2, #770), declare it in your own schema only if you need custom fields",
    ),
];

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
