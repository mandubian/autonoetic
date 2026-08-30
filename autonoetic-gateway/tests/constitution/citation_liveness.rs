//! Constitution-text citation liveness.
//!
//! Nothing else parses the clause text: `docs_link_guard` deliberately skips
//! `docs/constitution/versions/**` (digest-signed bytes), so citations in the
//! active version can rot silently — 2026.07.30 was signed with ~45 broken
//! citations (#953 renamed the test files one day before signing; #1173 moved
//! six cited docs). This module pins every path-shaped citation in the active
//! version's `constitution.md` and `RATIFY.md` to a file that exists.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

const CITATION_EXTENSIONS: [&str; 5] = ["rs", "md", "toml", "yaml", "yml"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("manifest dir has a parent")
        .to_path_buf()
}

fn active_version() -> &'static str {
    include_str!("../../../docs/constitution/CURRENT").trim()
}

/// Extract path-shaped citations: backticked tokens and markdown link
/// targets that end in a known source/doc extension. A trailing
/// `::symbol` chain is stripped (`runtime/checkpoint.rs::YieldReason`
/// cites the file), as are `#` anchors.
fn citations(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut push = |token: &str| {
        let token = token.trim();
        if !token.ends_with('.'){
            return;
        }
        let token = match token.rfind("::") {
            Some(i) => &token[..i],
            None => token,
        };
        let token = token.split('#').next().unwrap_or(token);
        let ext = token.rsplit('.').next().unwrap_or("");
        if CITATION_EXTENSIONS.contains(&ext) && !token.contains('{') {
            out.insert(token.to_string());
        }
    };
    for segment in text.split('`').skip(1).step_by(2) {
        push(segment);
    }
    for segment in text.split("](").skip(1) {
        let target = segment.split(')').next().unwrap_or("");
        if !target.starts_with("http://") && !target.starts_with("https://") {
            push(target);
        }
    }
    out
}

/// Every file basename under the source trees that bare citations
/// (`vault.rs`, `guard.rs`, …) may resolve against — the same loose
/// basename matching the enforcement register's citation guard allows.
fn known_basenames(root: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    for tree in [
        "autonoetic-gateway/src",
        "autonoetic-gateway/tests",
        "autonoetic-types/src",
        "docs",
        "config",
        "agents",
    ] {
        let stack = vec![root.join(tree)];
        let mut stack = stack;
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    out.insert(name.to_string());
                }
            }
        }
    }
    out
}

fn resolves(token: &str, root: &Path, basenames: &HashSet<String>) -> bool {
    for base in [
        root.to_path_buf(),
        root.join("autonoetic-gateway"),
        root.join("autonoetic-gateway/src"),
        root.join("autonoetic-gateway/tests"),
        root.join("autonoetic-types"),
    ] {
        if base.join(token).is_file() {
            return true;
        }
    }
    basenames.contains(token)
}

#[test]
fn active_constitution_text_citations_resolve() {
    let root = repo_root();
    let version = active_version();
    let basenames = known_basenames(&root);

    let mut all_citations = BTreeSet::new();
    for file in ["constitution.md", "RATIFY.md"] {
        let path = root
            .join("docs/constitution/versions")
            .join(version)
            .join(file);
        let body = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("active version file {} is readable: {e}", path.display())
        });
        all_citations.extend(citations(&body));
    }

    let broken: Vec<String> = all_citations
        .iter()
        .filter(|token| !resolves(token, &root, &basenames))
        .cloned()
        .collect();

    assert!(
        broken.is_empty(),
        "broken citations in active constitution {version} (fix the text; \
         line numbers are not checked by design):\n  {}",
        broken.join("\n  ")
    );
}
