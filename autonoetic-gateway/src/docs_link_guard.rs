//! Repo-hygiene guard: every `docs/…` path cited by a live document, an agent
//! bundle, or production Rust code must resolve to a file that exists.
//!
//! # Why this is a test and not a linter script
//!
//! Documentation links rotted silently for months. At the time this guard was
//! written, **19 `docs/…` citations** from live docs, agent bundles, and
//! production Rust pointed at nothing — most of them docs that had been moved
//! into `docs/archived/` without a reference sweep. A reader (human
//! *or* agent, since agents are told to read these paths) lands nowhere, and
//! nothing failed. Same class of defect as a hallucinated config key in a wiki
//! page, and it gets the same treatment as
//! `enforcement_register::tests::every_parseable_citation_resolves`: a
//! mechanical check, not a convention.
//!
//! It lives in the **lib** target on purpose. PR CI runs
//! `cargo nextest run --workspace --lib --bins`; the `tests/` integration
//! binaries are only *compiled* per-PR and run nightly (see
//! `.github/workflows/ci.yml`). A guard in `tests/` would not gate a PR.
//!
//! # What is scanned
//!
//! - **Markdown**, walking the workspace root, excluding hidden directories
//!   (`.git`, `.claude/worktrees`, …), `target/`, `node_modules/`, and the two
//!   frozen/historical corpora below.
//! - **Rust**, the *production prefix* of every `.rs` file — the file truncated
//!   at its first `#[cfg(test)]` / `mod tests {`. Comments **and** string
//!   literals are covered (operator-facing error messages cite docs too, e.g.
//!   `constitution_digest.rs`), while test fixture data that merely looks like
//!   a doc path is out of scope by construction.
//!
//! A citation counts when it ends in `.md` / `.toml` / `.json` / `.py`, or when
//! it names an extensionless **pointer file** by the uppercase convention
//! (`docs/constitution/CURRENT`) — see [`is_pointer_file`] for why the rule is
//! not simply "no extension".
//!
//! # What is not scanned, and why
//!
//! - `docs/archived/**` — historical records. A shipped plan saying
//!   "Delete `docs/adaptation-composition-model.md`" is *correct as written*;
//!   rewriting it would falsify the record.
//! - `docs/constitution/versions/**` — digest-signed bytes. Editing a link in
//!   a ratified constitution would break its signature.
//!
//! # Intentional exceptions
//!
//! `docs/.link-guard-allow` — one `path — reason` per line, `#` comments
//! allowed, a trailing `*` makes it a prefix glob. Use it for paths a document
//! deliberately names but that do not exist (a plan describing a tree it
//! proposes to create). Prefer rewording over an entry: a path in backticks is
//! a promise that it resolves.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Directories never descended into, anywhere in the tree.
const SKIP_DIRS: &[&str] = &["target", "node_modules"];

/// Sources excluded from the scan (see module docs).
const SKIP_PREFIXES: &[&str] = &["docs/archived/", "docs/constitution/versions/"];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("gateway crate always has a workspace parent")
        .to_path_buf()
}

/// Collect `.md` and `.rs` files worth scanning, as workspace-relative paths.
fn collect_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                // Hidden dirs cover `.git` and `.claude/worktrees` (checkouts
                // of this same repo, whose copies would double every finding).
                if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if SKIP_PREFIXES.iter().any(|p| rel.starts_with(p)) {
                continue;
            }
            if name.ends_with(".md") || name.ends_with(".rs") {
                out.push(PathBuf::from(rel));
            }
        }
    }
    out.sort();
    out
}

/// Everything before the first `#[cfg(test)]` / `mod tests {`.
///
/// Test modules hold fixture data that can look exactly like a doc path
/// (`"operator_added": ["docs/intro.md"]`) without being a reference to one.
fn production_prefix(text: &str) -> &str {
    let cut = text
        .find("#[cfg(test)]")
        .into_iter()
        .chain(text.find("\nmod tests {"))
        .min();
    match cut {
        Some(idx) => &text[..idx],
        None => text,
    }
}

/// An extensionless pointer file: last segment all-uppercase, as in
/// `docs/constitution/CURRENT` (the active-constitution pointer, cited 24
/// times). Without this, extensionless citations of load-bearing files would
/// be a blind spot — and `CURRENT` is exactly the kind of path whose breakage
/// matters, since the runtime sync-checks it.
///
/// Deliberately narrow. Accepting *any* extensionless path would flag prose
/// fragments and line-wrapped paths (`docs/design/principal-model-and-`,
/// `docs/constitution/...`, "docs/tests."), which is why the rule is the
/// uppercase convention rather than "no extension".
fn is_pointer_file(path: &str) -> bool {
    let Some(last) = path.rsplit('/').next() else {
        return false;
    };
    !last.is_empty()
        && !last.contains('.')
        && last
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
}

/// Extract `docs/…` citations: paths with a documentation extension, or
/// extensionless pointer files (see [`is_pointer_file`]).
fn extract_doc_paths(line: &str) -> Vec<String> {
    const EXTS: &[&str] = &[".md", ".toml", ".json", ".py"];
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = line[search_from..].find("docs/") {
        let start = search_from + rel;
        search_from = start + 5;
        // Must start a path segment: not `mydocs/`, not `../docs/` chained
        // onto a word character.
        if start > 0 {
            let prev = bytes[start - 1];
            if prev.is_ascii_alphanumeric() || prev == b'-' || prev == b'_' {
                continue;
            }
        }
        let tail: String = line[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
            .collect();
        // A trailing `.` is sentence punctuation, never part of a path.
        let tail = tail.trim_end_matches('.');
        if EXTS.iter().any(|e| tail.ends_with(e)) || is_pointer_file(tail) {
            found.push(tail.to_string());
        }
    }
    found
}

/// Extract Markdown link targets that are repo-relative file paths.
///
/// `docs/…`-prefixed citations are covered by [`extract_doc_paths`], but most
/// intra-docs navigation is *relative* — `](./agent-learning.md)`,
/// `](../design/README.md)` — and a relative link breaks the moment its file
/// or its target moves directory. 246 such links exist inside `docs/`, so a
/// reorganisation that only checks `docs/…` citations would silently shred
/// navigation.
///
/// Returns targets with any `#anchor` and `"title"` stripped. External links,
/// anchors-only, and non-file schemes are skipped: this guard checks paths,
/// not the network.
///
/// Known limitation: link syntax written as a *literal example* inside inline
/// code is still read as a link, because inline-code spans are not tracked
/// (correctly skipping them means not skipping the target in the common
/// ``[`label`](path)`` form). Prose that demonstrates link syntax should name
/// the target on its own instead — rare enough that parsing spans is not worth
/// the complexity.
fn extract_relative_links(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        // Find `](`.
        if bytes[i] != b']' || bytes[i + 1] != b'(' {
            i += 1;
            continue;
        }
        let start = i + 2;
        let Some(close_rel) = line[start..].find(')') else {
            break;
        };
        let raw = &line[start..start + close_rel];
        i = start + close_rel + 1;
        // Strip an optional link title: `path "Title"`.
        let target = raw.split_whitespace().next().unwrap_or("").trim();
        if target.is_empty() || target.starts_with('#') {
            continue;
        }
        // Not a repo path: URLs, protocol-relative, mail, templates.
        if target.contains("://")
            || target.starts_with("//")
            || target.starts_with("mailto:")
            || target.starts_with('<')
            || target.contains('{')
            || target.starts_with('$')
        {
            continue;
        }
        // Absolute filesystem paths are not repo-relative navigation.
        if target.starts_with('/') {
            continue;
        }
        let path = target.split('#').next().unwrap_or(target);
        if path.is_empty() {
            continue;
        }
        out.push(path.to_string());
    }
    out
}

/// Resolve `link` relative to the directory of `from_rel`, collapsing `..`.
fn resolve_relative(from_rel: &Path, link: &str) -> PathBuf {
    let mut parts: Vec<String> = Vec::new();
    if let Some(dir) = from_rel.parent() {
        for c in dir.components() {
            parts.push(c.as_os_str().to_string_lossy().to_string());
        }
    }
    for seg in link.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_string()),
        }
    }
    PathBuf::from(parts.join("/"))
}

/// Parse `docs/.link-guard-allow` into exact paths and prefix globs.
fn load_allowlist(root: &Path) -> (BTreeSet<String>, Vec<String>) {
    let mut exact = BTreeSet::new();
    let mut globs = Vec::new();
    let path = root.join("docs/.link-guard-allow");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (exact, globs);
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `path — reason` (em dash) or bare path.
        let entry = line.split('—').next().unwrap_or(line).trim();
        if entry.is_empty() {
            continue;
        }
        match entry.strip_suffix('*') {
            Some(prefix) => globs.push(prefix.to_string()),
            None => {
                exact.insert(entry.to_string());
            }
        }
    }
    (exact, globs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cited_docs_path_resolves() {
        let root = workspace_root();
        let (allow_exact, allow_globs) = load_allowlist(&root);
        let mut failures: Vec<String> = Vec::new();

        for rel in collect_sources(&root) {
            let Ok(raw) = std::fs::read_to_string(root.join(&rel)) else {
                continue;
            };
            let is_rust = rel.extension().is_some_and(|e| e == "rs");
            let text = if is_rust {
                production_prefix(&raw)
            } else {
                raw.as_str()
            };
            for (lineno, line) in text.lines().enumerate() {
                for cited in extract_doc_paths(line) {
                    if root.join(&cited).exists() {
                        continue;
                    }
                    if allow_exact.contains(&cited)
                        || allow_globs.iter().any(|g| cited.starts_with(g.as_str()))
                    {
                        continue;
                    }
                    failures.push(format!(
                        "{}:{} cites `{}` which does not exist\n      {}",
                        rel.display(),
                        lineno + 1,
                        cited,
                        line.trim()
                    ));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{} dangling docs reference(s). A cited path is a promise that it \
             resolves — fix the path, or reword so it is not written as a path. \
             For a path a document deliberately names but that does not exist \
             (e.g. a tree a plan proposes), add it to `docs/.link-guard-allow` \
             with a reason.\n\n  {}\n",
            failures.len(),
            failures.join("\n  ")
        );
    }

    #[test]
    fn every_relative_markdown_link_resolves() {
        let root = workspace_root();
        let (allow_exact, allow_globs) = load_allowlist(&root);
        let mut failures: Vec<String> = Vec::new();

        for rel in collect_sources(&root) {
            if rel.extension().is_some_and(|e| e != "md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(root.join(&rel)) else {
                continue;
            };
            for (lineno, line) in text.lines().enumerate() {
                for link in extract_relative_links(line) {
                    let target = resolve_relative(&rel, &link);
                    if root.join(&target).exists() {
                        continue;
                    }
                    let as_str = target.to_string_lossy().replace('\\', "/");
                    if allow_exact.contains(&as_str)
                        || allow_globs.iter().any(|g| as_str.starts_with(g.as_str()))
                    {
                        continue;
                    }
                    failures.push(format!(
                        "{}:{} links to `{}` → `{}` which does not exist\n      {}",
                        rel.display(),
                        lineno + 1,
                        link,
                        as_str,
                        line.trim()
                    ));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{} broken relative Markdown link(s). Moving a doc rewrites every \
             relative link into and out of it — fix the target, or add it to \
             `docs/.link-guard-allow` with a reason.\n\n  {}\n",
            failures.len(),
            failures.join("\n  ")
        );
    }

    #[test]
    fn relative_link_extraction_and_resolution() {
        assert_eq!(
            extract_relative_links("see [a](./x.md) and [b](../design/y.md#frag)"),
            vec!["./x.md".to_string(), "../design/y.md".to_string()]
        );
        // External and anchor-only links are not repo paths.
        assert!(extract_relative_links("[x](https://example.com/a.md)").is_empty());
        assert!(extract_relative_links("[x](#section)").is_empty());
        assert!(extract_relative_links("[x](mailto:a@b.c)").is_empty());
        // Resolution is relative to the *linking file's* directory.
        //
        // Deliberately synthetic paths: a unit test of path arithmetic must not
        // name real docs, or a future reorganisation's reference sweep rewrites
        // the fixture and the assertion fails for a reason unrelated to the
        // logic under test. (Exactly what happened during the PR-2 move.)
        assert_eq!(
            resolve_relative(Path::new("dir/sub/from.md"), "../other/to.md"),
            PathBuf::from("dir/other/to.md")
        );
        assert_eq!(
            resolve_relative(Path::new("dir/from.md"), "./to.md"),
            PathBuf::from("dir/to.md")
        );
    }

    /// Every proposal is listed in `docs/proposals/README.md`.
    ///
    /// The index that preceded this one silently missed 11 of 27 docs, and the
    /// sibling `rfc/` folder had no index at all — so "is anyone still working
    /// on this?" had no answer you could trust. An unlisted proposal is an
    /// invisible one.
    #[test]
    fn every_proposal_is_listed_in_the_index() {
        let root = workspace_root();
        let dir = root.join("docs/proposals");
        let index_path = dir.join("README.md");
        let index = std::fs::read_to_string(&index_path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", index_path.display()));

        let mut unlisted: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("docs/proposals must exist").flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "README.md" || !name.ends_with(".md") {
                continue;
            }
            // Listed means linked, not merely mentioned in prose.
            if !index.contains(&format!("({name})")) {
                unlisted.push(name);
            }
        }
        unlisted.sort();

        assert!(
            unlisted.is_empty(),
            "{} proposal(s) not linked from docs/proposals/README.md: {}.\n\
             Add a row with its status. A proposal nobody can find is a \
             proposal nobody will finish — which is how the previous index \
             came to miss 11 of 27 docs.",
            unlisted.len(),
            unlisted.join(", ")
        );
    }

    #[test]
    fn scanner_ignores_test_fixture_paths_but_reads_production_strings() {
        let src = r#"
//! See `docs/real-doc.md` for details.
fn f() -> &'static str { "docs/error-message.md" }

#[cfg(test)]
mod tests {
    const FIXTURE: &str = "docs/not-a-reference.md";
}
"#;
        let prod = production_prefix(src);
        assert!(prod.contains("docs/real-doc.md"));
        assert!(prod.contains("docs/error-message.md"));
        assert!(
            !prod.contains("docs/not-a-reference.md"),
            "fixture data inside #[cfg(test)] must not be treated as a citation"
        );
    }

    #[test]
    fn extractor_finds_paths_and_skips_lookalikes() {
        assert_eq!(
            extract_doc_paths("see [x](docs/a/b.md) and `docs/c.toml`"),
            vec!["docs/a/b.md".to_string(), "docs/c.toml".to_string()]
        );
        // Not a docs/ path: embedded in a longer word.
        assert!(extract_doc_paths("mydocs/thing.md").is_empty());
        // No documentation extension.
        assert!(extract_doc_paths("docs/wiki/ holds pages").is_empty());
    }

    #[test]
    fn extractor_covers_extensionless_pointer_files() {
        // `docs/constitution/CURRENT` is load-bearing (the runtime sync-checks
        // it) and has no extension — it must not be a blind spot.
        assert_eq!(
            extract_doc_paths("recorded in `docs/constitution/CURRENT`."),
            vec!["docs/constitution/CURRENT".to_string()]
        );
        // Lowercase extensionless fragments stay out: prose and line-wrapped
        // paths would otherwise be reported as dangling citations.
        assert!(extract_doc_paths("everything under docs/design for plans").is_empty());
        assert!(extract_doc_paths("split across `docs/design/principal-model-and-").is_empty());
        assert!(!is_pointer_file("docs/design/README.md"));
        assert!(is_pointer_file("docs/constitution/CURRENT"));
    }

    #[test]
    fn allowlist_parses_reasons_and_globs() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join(".link-guard-allow"),
            "# comment\ndocs/future.md — proposed by the reorg plan\ndocs/internals/*\n\n",
        )
        .unwrap();
        let (exact, globs) = load_allowlist(dir.path());
        assert!(exact.contains("docs/future.md"));
        assert_eq!(globs, vec!["docs/internals/".to_string()]);
    }
}
