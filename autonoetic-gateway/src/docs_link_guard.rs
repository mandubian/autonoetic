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
//! - **Rendered diagrams** (`.svg` / `.html` under `docs/`), for a different
//!   property: not whether a cited path resolves, but whether a printed
//!   **clause ID** exists in the active constitution. See
//!   `every_clause_id_in_a_diagram_resolves`.
//!
//! A citation counts when it ends in `.md` / `.toml` / `.json` / `.py`, or when
//! it names an extensionless **pointer file** by the uppercase convention
//! (`docs/constitution/CURRENT`) — see [`is_pointer_file`] for why the rule is
//! not simply "no extension".
//!
//! # Why clause IDs get the same treatment
//!
//! Diagrams were the only doc assets with no mechanical check, because the
//! path/symbol scans read `.md` and `.rs` only. Unguarded, a pedagogical map
//! accumulated a fabricated `U-4` marked "enforced", Rule Zero mislabelled as
//! `I-1`, and section numbers (`§3`) printed as clause IDs (`P-3`) — a reader
//! who looked any of them up landed nowhere. That is the same defect class as a
//! dangling path, in the artefact whose whole subject is that every denial
//! names a real rule (Ri-0.3).
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

/// Collect rendered diagram sources (`.svg`, `.html`) under `docs/`.
///
/// Kept separate from [`collect_sources`] because these files are scanned for a
/// different property: not whether a cited path resolves, but whether a cited
/// **clause ID** exists. See `every_clause_id_in_a_diagram_resolves`.
fn collect_diagram_sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.join("docs")];
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
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if kind.is_dir() {
                if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if SKIP_PREFIXES
                    .iter()
                    .any(|p| rel.starts_with(p.trim_end_matches('/')))
                {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            if name.ends_with(".svg") || name.ends_with(".html") {
                out.push(PathBuf::from(rel));
            }
        }
    }
    out.sort();
    out
}

/// Every clause ID declared by the **active** constitution: `Ri-*` rights,
/// `P-*` rules, `O-*` decider obligations, `U-*` served-party rights (from the
/// leading cell of a table row), plus `I-*` cross-cutting invariants (declared
/// as `**I-N**` bullets rather than table rows).
fn active_constitution_clause_ids(root: &Path) -> BTreeSet<String> {
    let path = root.join(autonoetic_types::config::default_constitution_source_path());
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read active constitution at {}: {e}", path.display()));

    let mut ids = BTreeSet::new();
    for line in text.lines() {
        // Table rows: `| P-2.20 | … |`. The ID is the first cell.
        if let Some(rest) = line.strip_prefix("| ") {
            let cell = rest.split('|').next().unwrap_or_default().trim();
            if parse_clause_id(cell).is_some_and(|len| len == cell.len()) {
                ids.insert(cell.to_string());
            }
        }
        // Invariant bullets: `- **I-12** Any collective decision mechanism …`.
        let mut hay = line;
        while let Some(at) = hay.find("**I-") {
            let after = &hay[at + 2..];
            if let Some(len) = parse_clause_id(after) {
                ids.insert(after[..len].to_string());
            }
            hay = &hay[at + 2..];
        }
    }
    ids
}

/// If `s` starts with a clause ID (`Ri-0.2`, `P-2.20`, `O-1`, `U-3`, `I-12`),
/// return its byte length. Returns `None` otherwise.
fn parse_clause_id(s: &str) -> Option<usize> {
    let family = ["Ri-", "P-", "O-", "U-", "I-"]
        .into_iter()
        .find(|f| s.starts_with(f))?;
    let mut len = family.len();
    let bytes = s.as_bytes();

    let digits = |len: &mut usize| {
        let start = *len;
        while *len < bytes.len() && bytes[*len].is_ascii_digit() {
            *len += 1;
        }
        *len > start
    };

    if !digits(&mut len) {
        return None;
    }
    // Optional `.N` minor part — `O-1` and `I-12` have none, `P-2.20` does.
    if len < bytes.len() && bytes[len] == b'.' {
        let mut probe = len + 1;
        if digits(&mut probe) {
            len = probe;
        }
    }
    Some(len)
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

/// Extract `(label, target)` for every Markdown link on the line.
///
/// Sibling of [`extract_relative_links`], which needs only targets. This one
/// keeps the label so the two halves of a link can be compared.
fn extract_labelled_links(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        let label_start = i + 1;
        let Some(label_len) = line[label_start..].find(']') else {
            break;
        };
        let label_end = label_start + label_len;
        // The `](` must be adjacent for this to be a link rather than a
        // bracketed aside.
        if line.as_bytes().get(label_end + 1) != Some(&b'(') {
            i = label_end + 1;
            continue;
        }
        let target_start = label_end + 2;
        let Some(target_len) = line[target_start..].find(')') else {
            break;
        };
        let raw = &line[target_start..target_start + target_len];
        let target = raw.split_whitespace().next().unwrap_or("").to_string();
        if !target.is_empty() {
            out.push((line[label_start..label_end].to_string(), target));
        }
        i = target_start + target_len + 1;
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

/// GitHub-style heading slugs plus explicit `name=`/`id=` anchors in a file.
///
/// Approximates GitHub's algorithm: strip inline markup, drop anything that is
/// not word/space/hyphen, lowercase, spaces to hyphens.
fn anchor_targets(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let title = rest.trim_start_matches('#').trim();
            let cleaned: String = title
                .chars()
                .filter(|c| !matches!(c, '`' | '*' | '_' | '[' | ']' | '(' | ')'))
                .collect();
            let slug: String = cleaned
                .chars()
                .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '-')
                .collect::<String>()
                .trim()
                .to_lowercase()
                .replace(' ', "-");
            if !slug.is_empty() {
                out.insert(slug);
            }
        }
        for key in ["name=\"", "id=\""] {
            let mut rest = line;
            while let Some(i) = rest.find(key) {
                rest = &rest[i + key.len()..];
                if let Some(end) = rest.find('"') {
                    out.insert(rest[..end].to_lowercase());
                    rest = &rest[end..];
                }
            }
        }
    }
    out
}

/// Backticked tokens that look like a Rust/SDK type or path: CamelCase, no
/// dots or spaces, optionally `Type::member`.
///
/// Excludes anything with a `.` (so `SKILL.md` and `foo.rs` are not symbols)
/// and requires an initial capital followed by a lowercase somewhere, which
/// filters bare acronyms (`JSON`, `HTTP`, `USD`) that are prose, not symbols.
fn extract_symbol_citations(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in line.split('`').skip(1).step_by(2) {
        let tok = chunk.trim();
        if tok.is_empty() || tok.contains('.') || tok.contains(' ') || tok.contains('(') {
            continue;
        }
        let mut chars = tok.chars();
        if !chars.next().is_some_and(|c| c.is_ascii_uppercase()) {
            continue;
        }
        if !tok.chars().any(|c| c.is_ascii_lowercase()) {
            continue;
        }
        if !tok
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
        {
            continue;
        }
        out.push(tok.to_string());
    }
    out
}

/// Does a documented symbol resolve against the source haystack?
///
/// Literal first: `GatewayStore::contract_health` written exactly is the
/// strongest evidence. Falling back to **every** segment existing (not just
/// the last) is required because Rust defines `Type::method` inside
/// `impl Type`, so the qualified string never appears for 22 of the 63
/// qualified citations in the current corpus — while checking only the last
/// segment would accept `NoSuchType::run` on the strength of some unrelated
/// `run` elsewhere, which is the permissiveness this guard exists to avoid.
fn symbol_resolves(sym: &str, haystack: &str) -> bool {
    if haystack.contains(sym) {
        return true;
    }
    // Whole-word per segment, so a documented `Foo` is not satisfied by an
    // unrelated `FooBarBaz`. Measured against the current corpus: zero
    // citations rely on the looser substring behaviour.
    sym.split("::").all(|seg| contains_word(haystack, seg))
}

/// Whole-word containment: `Foo` must not be satisfied by `FooBarBaz`.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        from = start + 1;
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Parse a `— reason`-annotated allow file into its bare entries.
fn load_allow_entries(root: &Path, name: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Ok(text) = std::fs::read_to_string(root.join("docs").join(name)) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let entry = line.split('—').next().unwrap_or(line).trim();
        if !entry.is_empty() {
            out.insert(entry.to_string());
        }
    }
    out
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
    fn clause_id_parser_accepts_every_family_and_rejects_lookalikes() {
        // Both shapes: major-only (`O-1`, `I-12`) and major.minor (`P-2.20`).
        for (input, want) in [
            ("Ri-0.2", Some(6)),
            ("Ri-0.18 rest", Some(7)),
            ("P-2.20", Some(6)),
            ("P-15.1)", Some(6)),
            ("O-1", Some(3)),
            ("O-7 owes", Some(3)),
            ("U-3", Some(3)),
            ("I-12", Some(4)),
            ("I-1 bullet", Some(3)),
            // Section shorthand: `P-3` parses as an ID and is *meant* to fail
            // the resolution check, not the parse.
            ("P-3", Some(3)),
            // Not clause IDs.
            ("P-*", None),
            ("Ri-*", None),
            ("Inter", None),
            ("-apple-system", None),
            ("stroke-width", None),
        ] {
            assert_eq!(parse_clause_id(input), want, "parsing {input:?}");
        }

        // A trailing dot with no digits is not part of the ID.
        assert_eq!(parse_clause_id("P-15. and"), Some(4));
    }

    #[test]
    fn constitution_clause_extraction_covers_all_five_families() {
        let ids = active_constitution_clause_ids(&workspace_root());
        for expect in [
            "Ri-0.1", "Ri-0.18", "P-8.1", "O-1", "O-7", "U-1", "U-3", "I-1", "I-14",
        ] {
            assert!(ids.contains(expect), "missing {expect} from extracted set");
        }
        // The defects this guard was written for must not resolve.
        for reject in ["U-4", "P-3", "P-1", "P-7", "O-3", "Ri-0.19"] {
            assert!(!ids.contains(reject), "unexpectedly resolved {reject}");
        }
    }

    /// A clause ID printed on a diagram is the same promise as a `docs/…` path:
    /// a reader can look it up. Nothing checked the rendered assets, so a
    /// pedagogical map accumulated a fabricated `U-4` marked "enforced", Rule
    /// Zero mislabelled as `I-1`, and section numbers (`§3`) printed as clause
    /// IDs (`P-3`) — the exact defect Ri-0.3 exists to prevent, in the artefact
    /// that teaches Ri-0.3.
    ///
    /// Wildcards (`Ri-*`, `P-15.*`) are prose, not citations, and are skipped.
    #[test]
    fn every_clause_id_in_a_diagram_resolves() {
        let root = workspace_root();
        let known = active_constitution_clause_ids(&root);
        assert!(
            known.len() > 100,
            "clause-ID extraction produced only {} ids — the constitution's table \
             format probably changed and this guard has gone blind",
            known.len()
        );

        let mut failures: Vec<String> = Vec::new();
        for rel in collect_diagram_sources(&root) {
            let Ok(text) = std::fs::read_to_string(root.join(&rel)) else {
                continue;
            };
            for (lineno, line) in text.lines().enumerate() {
                for (col, _) in line.char_indices() {
                    // Only start a match at a boundary, so `SHA-256` and
                    // `-apple-system` cannot masquerade as clause IDs.
                    if col > 0 && line.as_bytes()[col - 1].is_ascii_alphanumeric() {
                        continue;
                    }
                    let tail = &line[col..];
                    let Some(len) = parse_clause_id(tail) else {
                        continue;
                    };
                    // `P-15.*` — a family reference, not a clause citation.
                    if tail[len..].starts_with(".*") {
                        continue;
                    }
                    let id = &tail[..len];
                    if known.contains(id) {
                        continue;
                    }
                    failures.push(format!(
                        "{}:{} cites clause `{}`, which the active constitution \
                         ({}) does not declare\n      {}",
                        rel.display(),
                        lineno + 1,
                        id,
                        autonoetic_types::config::ACTIVE_CONSTITUTION_VERSION,
                        line.trim()
                    ));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{} diagram(s) cite a clause ID that does not exist. A printed \
             clause ID is a promise a reader can look it up — fix the ID, or \
             reword to a section reference (`§3`) or a family (`P-*`) if no \
             single clause is meant.\n\n  {}\n",
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

    /// A `#anchor` in a link must exist as a heading in the target.
    ///
    /// The third invisible half of a link. The citation check proves the file
    /// exists and the label check proves the name is honest; neither can tell
    /// that `](ARCHITECTURE.md#causal-chain)` lands nowhere, because an anchor
    /// is not a path. Nothing is broken today — this is preventive, and it is
    /// what makes splitting a large doc safe: a split moves headings, and
    /// every inbound `#section` link silently rots.
    #[test]
    fn anchor_links_resolve() {
        let root = workspace_root();
        let mut failures: Vec<String> = Vec::new();

        for rel in collect_sources(&root) {
            if rel.extension().is_some_and(|e| e != "md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(root.join(&rel)) else {
                continue;
            };
            for (lineno, line) in text.lines().enumerate() {
                for (_label, link) in extract_labelled_links(line) {
                    if link.contains("://")
                        || link.starts_with(['/', '<', '$'])
                        || link.contains('{')
                    {
                        continue;
                    }
                    let (path, anchor) = match link.split_once('#') {
                        Some((p, a)) if !a.is_empty() => (p, a),
                        _ => continue,
                    };
                    // An empty path means "this file".
                    let target = if path.is_empty() {
                        rel.clone()
                    } else {
                        resolve_relative(&rel, path)
                    };
                    if !target.to_string_lossy().ends_with(".md") {
                        continue;
                    }
                    let Ok(target_text) = std::fs::read_to_string(root.join(&target)) else {
                        continue; // missing file is the citation check's finding
                    };
                    if anchor_targets(&target_text).contains(&anchor.to_lowercase()) {
                        continue;
                    }
                    failures.push(format!(
                        "{}:{} anchor `#{}` has no matching heading in {}\n      {}",
                        rel.display(),
                        lineno + 1,
                        anchor,
                        target.display(),
                        line.trim()
                    ));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{} link(s) pointing at a heading that does not exist. Renaming or \
             moving a heading breaks every inbound anchor — update the link, or \
             restore the heading.\n\n  {}\n",
            failures.len(),
            failures.join("\n  ")
        );
    }

    /// A backticked type name in `reference/` or `internals/` must exist.
    ///
    /// Added because the docs shipped three API-accuracy errors that review
    /// caught and no guard could: a **`ToolErrorKind` that does not exist**
    /// (the real enum is `FailureClass`), a missing `RetryAdvice` variant, and
    /// a hash description copied from a stale code comment. Paths and anchors
    /// were checked; the symbols a reader would actually type into a grep were
    /// not.
    ///
    /// Sources scanned are Rust plus the Python and TypeScript SDKs, since
    /// reference docs legitimately name SDK classes. Symbols a doc names as
    /// *not yet existing* (a proposed extraction, a "would clarify" table) go
    /// in `docs/.symbol-guard-allow` with a reason.
    #[test]
    fn documented_symbols_exist() {
        let root = workspace_root();
        let allow = load_allow_entries(&root, ".symbol-guard-allow");

        // One concatenated haystack: cheaper than re-reading per candidate.
        let mut haystack = String::new();
        for dir in [
            "autonoetic-gateway/src",
            "autonoetic-types/src",
            "autonoetic/src",
            "autonoetic-ofp/src",
            "autonoetic-mcp/src",
            "autonoetic-sdk/python",
            "autonoetic-sdk/typescript/src",
        ] {
            let mut stack = vec![root.join(dir)];
            while let Some(d) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&d) else {
                    continue;
                };
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if p
                        .extension()
                        .is_some_and(|x| matches!(x.to_str(), Some("rs") | Some("py") | Some("ts")))
                    {
                        if let Ok(t) = std::fs::read_to_string(&p) {
                            // Production prefix only. Test modules contain
                            // fixture strings naming deliberately-absent
                            // symbols — including this guard's own
                            // `NoSuchType::run` fixture, which otherwise
                            // defines itself into existence and makes the
                            // check vacuous for exactly the drift it targets.
                            haystack.push_str(production_prefix(&t));
                            haystack.push('\n');
                        }
                    }
                }
            }
        }

        let mut failures: Vec<String> = Vec::new();
        for rel in collect_sources(&root) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if !rel_str.starts_with("docs/reference/") && !rel_str.starts_with("docs/internals/") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(root.join(&rel)) else {
                continue;
            };
            for (lineno, line) in text.lines().enumerate() {
                for sym in extract_symbol_citations(line) {
                    if allow.contains(&sym) {
                        continue;
                    }
                    if symbol_resolves(&sym, &haystack) {
                        continue;
                    }
                    failures.push(format!(
                        "{}:{} cites `{}` — no such symbol in Rust or SDK sources",
                        rel.display(),
                        lineno + 1,
                        sym
                    ));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{} documented symbol(s) that do not exist. A backticked type name \
             is a claim a reader will grep for — fix the name, or add it to \
             `docs/.symbol-guard-allow` with a reason if the doc deliberately \
             names something not yet built.\n\n  {}\n",
            failures.len(),
            failures.join("\n  ")
        );
    }

    /// A link label that names a file must name the file it links to.
    ///
    /// Every reference sweep in the docs reorganisation rewrote link
    /// *targets* and left the visible *labels* alone, so a reader saw
    /// `[design/post-promotion-review-design.md](../proposals/post-promotion-review.md)`
    /// — a path that no longer exists, presented as the name of the thing being
    /// linked. 24 of these accumulated across three PRs before anyone noticed,
    /// and neither the citation check nor the relative-link check can see them:
    /// the target resolves, so both pass. Only the human-visible half is wrong,
    /// which makes greps and mental models stale.
    ///
    /// Descriptive labels are untouched; the rule applies only when the label
    /// itself ends in `.md`, i.e. when it claims to be a filename.
    #[test]
    fn link_labels_naming_a_file_match_their_target() {
        let root = workspace_root();
        let mut failures: Vec<String> = Vec::new();

        for rel in collect_sources(&root) {
            if rel.extension().is_some_and(|e| e != "md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(root.join(&rel)) else {
                continue;
            };
            for (lineno, line) in text.lines().enumerate() {
                for (label, target) in extract_labelled_links(line) {
                    let core = label.trim_matches(|c| c == '`' || c == '*' || c == ' ');
                    if !core.ends_with(".md") || target.contains("://") {
                        continue;
                    }
                    let target_base = target
                        .split('#')
                        .next()
                        .unwrap_or(&target)
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    let label_base = core.rsplit('/').next().unwrap_or(core);
                    if target_base.is_empty() || label_base == target_base {
                        continue;
                    }
                    failures.push(format!(
                        "{}:{} label `{}` names a different file than its target `{}`\n      {}",
                        rel.display(),
                        lineno + 1,
                        core,
                        target,
                        line.trim()
                    ));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{} link label(s) naming a file other than the one linked. A label \
             that looks like a path is read as one — make it the target's \
             filename, or write a descriptive label instead.\n\n  {}\n",
            failures.len(),
            failures.join("\n  ")
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
        for entry in std::fs::read_dir(&dir)
            .expect("docs/proposals must exist")
            .flatten()
        {
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
    fn symbol_resolution_is_literal_first_then_all_segments() {
        let src = "struct GatewayStore; impl GatewayStore { fn contract_health() {} } \
                   enum SessionRole { Operator } fn run() {}";
        // Literal qualified form present.
        assert!(symbol_resolves(
            "SessionRole::Operator",
            "SessionRole::Operator"
        ));
        // Not literal, but every segment exists (the `impl Type` case).
        assert!(symbol_resolves("GatewayStore::contract_health", src));
        // The permissiveness this replaced: last segment alone must NOT pass.
        assert!(
            !symbol_resolves("NoSuchType::run", src),
            "checking only the last segment would accept this on the strength \
             of an unrelated `run`"
        );
        // Whole-word: a longer identifier must not satisfy a shorter one.
        assert!(!contains_word("FooBarBaz", "Foo"));
        assert!(contains_word("a FooBarBaz Foo b", "Foo"));
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
