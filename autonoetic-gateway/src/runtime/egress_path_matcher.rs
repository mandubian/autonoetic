//! Egress path matcher for `sandbox.exec` — RFC data-envelopes §4.2.
//!
//! A sibling of [`crate::runtime::remote_access::RemoteAccessAnalyzer`]: the
//! same "analyze command + dependencies before execute" idiom, different
//! predicate. Where `RemoteAccessAnalyzer` scans for *network* patterns to
//! decide whether an exec needs `share_net` approval, this matcher scans for
//! *labeled paths* to decide whether the exec's stdout/stderr envelope should
//! inherit a label.
//!
//! This is what keeps the RFC §1.1 / §5.6 scenario safe when the **script**,
//! not a structured tool, does the reading: a `sandbox.exec` whose command or
//! script body touches `~/mail/**` produces a labeled result envelope even
//! though no `fs.read` was ever called.
//!
//! ## Honest limits (RFC §11)
//!
//! Static path matching is defeated by indirection (symlinks, env vars,
//! `$(cat ...)`). Backstops documented in the RFC: the outbound content
//! assertion (when the gateway holds the content — phase 1b #905), the
//! `Network`-sink escalation (exfil needs network — phase 4 #909), and
//! compartments / `default_label: local_only` for high-sensitivity work.

use std::path::Path;

/// A path pattern configured as labeled (typically from `egress.rules` with a
/// `path:` field, e.g. `~/mail/**`).
#[derive(Debug, Clone)]
pub struct LabeledPathPattern {
    /// The raw pattern as written in config (`~/mail/**`, `state/secrets/*`).
    pub pattern: String,
}

impl LabeledPathPattern {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
        }
    }
}

/// Result of matching labeled paths against an exec.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathMatchResult {
    /// Patterns that matched at least one observed path token. Empty when
    /// nothing matched (the exec's envelope stays at the default label).
    pub matched_patterns: Vec<String>,
}

impl PathMatchResult {
    pub fn matched(&self) -> bool {
        !self.matched_patterns.is_empty()
    }
}

/// A zero-struct analyzer (mirrors `RemoteAccessAnalyzer`'s shape): all state
/// is in the arguments passed to each call.
pub struct EgressPathMatcher;

impl EgressPathMatcher {
    /// Scan a `sandbox.exec` command + optional script body for path tokens
    /// that match any labeled pattern.
    ///
    /// `command` is the shell command line (the `command` arg of `sandbox.exec`);
    /// `script_body` is the script source when the exec runs an inline script
    /// (the `code`/`script` arg). Either may be empty.
    ///
    /// Matching is intentionally conservative: a pattern matches if any token
    /// extracted from the command/script *starts with* the pattern's literal
    /// prefix (everything before a trailing `*`). This catches direct reads
    /// (`cat ~/mail/inbox/1`) and dependency reads (a script that opens
    /// `~/mail/archive.mbox`). It does **not** resolve indirection — see the
    /// module docs.
    pub fn analyze(command: &str, script_body: Option<&str>, patterns: &[LabeledPathPattern]) -> PathMatchResult {
        let sources: Vec<&str> = script_body.into_iter().collect();
        Self::analyze_sources(command, &sources, patterns)
    }

    /// Same as [`Self::analyze`] over any number of source texts — the command
    /// plus every dependency source resolved by
    /// [`collect_exec_dependency_sources`] (artifact bundle files, workspace
    /// scripts).
    pub fn analyze_sources(
        command: &str,
        sources: &[&str],
        patterns: &[LabeledPathPattern],
    ) -> PathMatchResult {
        let tokens = extract_path_tokens(command, sources);
        let mut matched: Vec<String> = Vec::new();
        for pat in patterns {
            if tokens.iter().any(|tok| pattern_matches_token(&pat.pattern, tok)) {
                matched.push(pat.pattern.clone());
            }
        }
        // Dedup while preserving order.
        let mut seen = std::collections::HashSet::new();
        matched.retain(|p| seen.insert(p.clone()));
        PathMatchResult { matched_patterns: matched }
    }
}

/// Does a single configured pattern match a single observed token?
///
/// Mirrors [`autonoetic_types::egress::matches_simple_glob`] but operates on
/// the prefix relationship that matters for paths: a pattern `~/mail/**`
/// matches any token that starts with `~/mail/`.
fn pattern_matches_token(pattern: &str, token: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    // Normalize `~` is left to the operator (we match literally against what
    // the command contains). A trailing `*` (single or double) means prefix.
    let prefix = pattern.trim_end_matches('*');
    if prefix.is_empty() {
        return !token.is_empty();
    }
    if pattern.ends_with('*') {
        token.starts_with(prefix)
    } else {
        token == prefix
    }
}

/// Extract candidate path tokens from a command line and optional script body.
///
/// Does two passes per chunk of text:
/// 1. Whitespace tokenization + punctuation trimming — catches shell-style
///    `cat ~/mail/inbox/1` where the path is its own token.
/// 2. A regex sweep for path-like substrings *inside* tokens — catches code
///    like `mailbox.mbox("~/mail/archive.mbox")` or `open("/etc/passwd")` where
///    the path is embedded in a function-call token with no surrounding space.
///
/// Deliberately over-broad: false positives only produce an over-restricted
/// (safer) label; false negatives are the residual risk documented in the
/// module docs.
fn extract_path_tokens(command: &str, sources: &[&str]) -> Vec<String> {
    use std::sync::OnceLock;
    // Match `~`-prefixed or `/`-containing path runs, including dots, hyphens,
    // underscores, alphanumerics. Requires at least one `/` to qualify.
    static PATH_RE: OnceLock<regex::Regex> = OnceLock::new();
    let path_re = PATH_RE.get_or_init(|| {
        // ~-prefixed relative path, OR an absolute/relative path with a slash.
        regex::Regex::new(r"~[[:alnum:]._/\-]*|[._[:alnum:]\-]+/[[:alnum:]._/\-]+").unwrap()
    });

    let mut tokens: Vec<String> = Vec::new();
    let push_if_pathlike = |s: &str, out: &mut Vec<String>| {
        // Trim shell/code punctuation that hugs path literals in real commands
        // and scripts: quotes, commas, parens, brackets, colons, semicolons.
        let t = s.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | ',' | '(' | ')' | '[' | ']' | '{' | '}' | ':' | ';' | '`'
            )
        });
        if t.is_empty() {
            return;
        }
        // Heuristic: looks like a path if it contains '/', starts with '~',
        // or starts with './' / '../'.
        let pathlike = t.contains('/')
            || t.starts_with('~')
            || t.starts_with("./")
            || t.starts_with("../");
        if pathlike {
            out.push(t.to_string());
        }
    };

    for chunk in std::iter::once(command).chain(sources.iter().copied()) {
        // Pass 1: whitespace tokens.
        for tok in chunk.split_whitespace() {
            push_if_pathlike(tok, &mut tokens);
        }
        // Pass 2: embedded path substrings inside any token (e.g. inside a
        // function-call token with no surrounding whitespace).
        for m in path_re.find_iter(chunk) {
            let s = m.as_str();
            // Require at least one '/' so bare words don't sneak in via the
            // `[._[:alnum:]\-]+` branch matching a single segment.
            if s.contains('/') || s.starts_with('~') {
                tokens.push(s.to_string());
            }
        }
    }

    // Dedup.
    tokens.sort();
    tokens.dedup();
    tokens
}

// ---------------------------------------------------------------------------
// Dependency sources — the "and its script dependencies" half of RFC §4.2.
// ---------------------------------------------------------------------------

/// Where an exec-shaped tool call's dependency sources can be found.
///
/// Scanning only the command line covers the *direct* read (`cat ~/mail/…`).
/// The RFC also requires the **dependency** read: `python3 parse_mail.py`, where
/// the labeled path appears in the script, not on the command line. This context
/// is what lets the matcher go find that script — the same resolution
/// `extract_code_for_analysis` (`runtime/tools/sandbox.rs`) already performs for
/// the network predicate.
// No `Debug`: `GatewayStore` is not `Debug`, and a context carrying a live
// store handle is not something to format into a log line anyway.
#[derive(Clone, Copy, Default)]
pub struct ExecSourceContext<'a> {
    /// The agent's directory — the root for relative script paths.
    pub agent_dir: Option<&'a Path>,
    /// The gateway directory — hosts the content store and artifact store.
    pub gateway_dir: Option<&'a Path>,
    /// Session id, for resolving `/tmp/<name>` session content mounts and the
    /// session-scoped artifact-ref registry.
    pub session_id: Option<&'a str>,
    /// The store that resolves a short `ar.*` artifact ref to its canonical
    /// `art_*` bundle id. Without it, an exec driven by `artifact_ref` (the
    /// form the tool schema tells agents to prefer) has no scannable bundle.
    pub gateway_store: Option<&'a crate::scheduler::gateway_store::GatewayStore>,
}

/// Max dependency files read per exec, and max bytes per file. Static analysis
/// runs inline on the tool-result path, so it stays cheap by construction: a
/// pathological command naming hundreds of scripts costs a bounded number of
/// reads, and the residual risk (a labeled path only in the 9th script) is the
/// same class of miss the module docs already own.
const MAX_DEPENDENCY_FILES: usize = 8;
const MAX_DEPENDENCY_BYTES: u64 = 256 * 1024;

/// File extensions treated as script sources worth scanning. Extension-gated on
/// purpose: it keeps the collector from reading arbitrary files whose names
/// happen to appear on a command line.
const SCRIPT_EXTENSIONS: &[&str] = &[
    "py", "js", "mjs", "cjs", "ts", "sh", "bash", "zsh", "rb", "pl", "lua", "r", "jl", "php",
    "awk", "sql", "ps1",
];

/// Collect the source text of an exec's dependencies: the artifact bundle it
/// runs (when `artifact_id`/`artifact_ref` is present) plus script files named
/// on its command line.
///
/// Best-effort throughout — an unreadable file is skipped, not an error. A miss
/// yields a less restrictive label, which is why the RFC pairs static analysis
/// with the runtime backstops (§11); it is never a hard failure.
pub fn collect_exec_dependency_sources(
    arguments_json: &str,
    command: &str,
    ctx: &ExecSourceContext<'_>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    if let Some(gw_dir) = ctx.gateway_dir {
        for id in canonical_artifact_ids(arguments_json, gw_dir, ctx) {
            collect_artifact_sources(gw_dir, &id, &mut out);
            if out.len() >= MAX_DEPENDENCY_FILES {
                return out;
            }
        }
    }

    for token in script_tokens(command) {
        if out.len() >= MAX_DEPENDENCY_FILES {
            break;
        }
        if let Some(text) = read_script_source(&token, ctx) {
            out.push(text);
        }
    }

    out
}

/// Canonical `art_*` bundle ids named by an exec's arguments.
///
/// `artifact_id` is already canonical. `artifact_ref` is the short `ar.*` form
/// the tool schema tells agents to *prefer*, and `ArtifactStore::inspect`
/// rejects it outright (it asserts the `art_` prefix) — so it must go through
/// the same `resolve_artifact_ref_or_canonical` flow `sandbox_exec` itself
/// uses. Skipping that resolution would leave every ref-driven exec with no
/// bundle to scan, i.e. a path-bearing rule silently failing to label.
fn canonical_artifact_ids(
    arguments_json: &str,
    gateway_dir: &Path,
    ctx: &ExecSourceContext<'_>,
) -> Vec<String> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(arguments_json) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = Vec::new();
    let push = |id: String, ids: &mut Vec<String>| {
        if !id.is_empty() && !ids.iter().any(|existing| *existing == id) {
            ids.push(id);
        }
    };

    if let Some(id) = parsed.get("artifact_id").and_then(|v| v.as_str()) {
        push(id.to_string(), &mut ids);
    }

    if let Some(aref) = parsed.get("artifact_ref").and_then(|v| v.as_str()) {
        if !aref.is_empty() {
            match (ctx.gateway_store, ctx.session_id) {
                (Some(store), Some(session_id)) => {
                    // Best-effort, like everything else in this module: an
                    // unresolvable ref just means no bundle to scan.
                    if let Ok(resolved) =
                        crate::runtime::tools::artifact::resolve_artifact_ref_or_canonical(
                            aref,
                            session_id,
                            store,
                            gateway_dir,
                        )
                    {
                        push(resolved.artifact_id, &mut ids);
                    }
                }
                _ => {
                    tracing::debug!(
                        target: "egress_path_matcher",
                        artifact_ref = %aref,
                        "no store/session to resolve artifact_ref; bundle not scanned for labeled paths"
                    );
                }
            }
        }
    }

    ids
}

/// Read the text files of an artifact bundle into `out`.
///
/// Unlike a bare command-line token — which could name any file on the host,
/// hence the extension gate there — these files *are* the code this exec is
/// about to run, so reading them is neither an overreach nor a guess. They are
/// not extension-filtered because a labeled path is as likely to sit in a
/// bundled config as in the script that opens it, and a miss here is a
/// fail-open (an unlabeled envelope), not a cheap mistake.
///
/// Script sources are taken **first** so a bundle carrying assets cannot
/// exhaust [`MAX_DEPENDENCY_FILES`] before the code is ever looked at.
fn collect_artifact_sources(gateway_dir: &Path, artifact_id: &str, out: &mut Vec<String>) {
    let Ok(store) = crate::artifact_store::ArtifactStore::new(gateway_dir) else {
        return;
    };
    let Ok(files) = store.resolve_files(artifact_id) else {
        return;
    };
    let (scripts, others): (Vec<_>, Vec<_>) = files
        .into_iter()
        .partition(|(name, _)| has_script_extension(name));
    for (_name, bytes) in scripts.into_iter().chain(others) {
        if out.len() >= MAX_DEPENDENCY_FILES {
            return;
        }
        if bytes.len() as u64 > MAX_DEPENDENCY_BYTES {
            continue;
        }
        if let Ok(text) = String::from_utf8(bytes) {
            out.push(text);
        }
    }
}

/// Does this filename carry one of [`SCRIPT_EXTENSIONS`]?
fn has_script_extension(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| SCRIPT_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Command-line tokens that name a script file worth scanning.
fn script_tokens(command: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for raw in command.split_whitespace() {
        let tok = raw.trim_matches(|c: char| {
            matches!(c, '"' | '\'' | ',' | '(' | ')' | ';' | '`' | '&' | '|')
        });
        if tok.is_empty() || tok.starts_with('-') {
            continue;
        }
        if has_script_extension(tok) && !tokens.iter().any(|t| t == tok) {
            tokens.push(tok.to_string());
        }
    }
    tokens
}

/// Resolve one script token to its source text.
///
/// Mirrors the resolution order `extract_code_for_analysis` uses: session
/// content mounts (`/tmp/<name>`) first, then the agent directory, then the
/// literal path.
fn read_script_source(token: &str, ctx: &ExecSourceContext<'_>) -> Option<String> {
    if let Some(name) = token.strip_prefix("/tmp/") {
        if let (Some(gw_dir), Some(sid)) = (ctx.gateway_dir, ctx.session_id) {
            if let Ok(store) = crate::runtime::content_store::ContentStore::new(gw_dir) {
                if let Ok(bytes) = store.read_by_name_or_handle(sid, name) {
                    if let Ok(text) = String::from_utf8(bytes) {
                        return Some(text);
                    }
                }
            }
        }
        if let Some(agent_dir) = ctx.agent_dir {
            if let Some(text) = read_bounded(&agent_dir.join(name)) {
                return Some(text);
            }
        }
    }

    let path = Path::new(token);
    if path.is_absolute() {
        return read_bounded(path);
    }
    ctx.agent_dir
        .and_then(|agent_dir| read_bounded(&agent_dir.join(token)))
}

/// Read a file, refusing anything over [`MAX_DEPENDENCY_BYTES`].
fn read_bounded(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_DEPENDENCY_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Resolve a `~`-prefixed pattern/token against a home dir for matching, when
/// the operator's rules use `~` but the sandbox command uses an absolute path.
///
/// Best-effort: if either side doesn't resolve, the raw forms are compared.
pub fn normalize_home(pattern_or_token: &str, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if pattern_or_token.starts_with("~/") {
            if let Some(h) = home.to_str() {
                return format!("{}{}", h, &pattern_or_token[1..]);
            }
        }
    }
    pattern_or_token.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(items: &[&str]) -> Vec<LabeledPathPattern> {
        items.iter().map(|i| LabeledPathPattern::new(*i)).collect()
    }

    #[test]
    fn direct_command_path_match() {
        // `cat ~/mail/inbox/1` → matches `~/mail/**`
        let r = EgressPathMatcher::analyze(
            "cat ~/mail/inbox/1",
            None,
            &pats(&["~/mail/**"]),
        );
        assert!(r.matched());
        assert_eq!(r.matched_patterns, vec!["~/mail/**"]);
    }

    #[test]
    fn no_match_when_path_absent() {
        let r = EgressPathMatcher::analyze(
            "echo hello && ls /tmp",
            None,
            &pats(&["~/mail/**"]),
        );
        assert!(!r.matched());
    }

    #[test]
    fn script_body_dependency_read_matches() {
        // A Python script that opens a labeled path.
        let script = r#"
import mailbox
mb = mailbox.mbox("~/mail/archive.mbox")
for msg in mb:
    print(msg['Subject'])
"#;
        let r = EgressPathMatcher::analyze("python3 script.py", Some(script), &pats(&["~/mail/**"]));
        assert!(r.matched(), "script body should be scanned");
        assert_eq!(r.matched_patterns, vec!["~/mail/**"]);
    }

    #[test]
    fn multiple_patterns_intersect_via_labeler() {
        // Two patterns; both present in the command → both matched (the labeler
        // intersects the labels, not the matcher).
        let r = EgressPathMatcher::analyze(
            "cp ~/mail/inbox/1 /secrets/key",
            None,
            &pats(&["~/mail/**", "/secrets/*"]),
        );
        assert_eq!(r.matched_patterns.len(), 2);
    }

    #[test]
    fn exact_pattern_matches_exact_token_only() {
        let r = EgressPathMatcher::analyze("cat /etc/passwd", None, &pats(&["/etc/passwd"]));
        assert!(r.matched());
        let r = EgressPathMatcher::analyze("cat /etc/passwd.bak", None, &pats(&["/etc/passwd"]));
        assert!(!r.matched(), "exact pattern should not prefix-match");
    }

    #[test]
    fn star_only_pattern_matches_any_pathlike_token() {
        let r = EgressPathMatcher::analyze("ls /var/log", None, &pats(&["*"]));
        assert!(r.matched());
    }

    #[test]
    fn quoted_paths_are_extracted() {
        let r = EgressPathMatcher::analyze(
            r#"cat "~/mail/inbox/1""#,
            None,
            &pats(&["~/mail/**"]),
        );
        assert!(r.matched(), "quoted path should be matched after trim");
    }

    #[test]
    fn empty_command_and_body_matches_nothing() {
        let r = EgressPathMatcher::analyze("", None, &pats(&["~/mail/**"]));
        assert!(!r.matched());
    }

    #[test]
    fn dedup_match_list() {
        // Same pattern matched via command + script — deduped to one entry.
        let r = EgressPathMatcher::analyze(
            "cat ~/mail/inbox/1",
            Some("grep foo ~/mail/archive.mbox"),
            &pats(&["~/mail/**"]),
        );
        assert_eq!(r.matched_patterns, vec!["~/mail/**"]);
    }

    #[test]
    fn analyze_sources_scans_every_source() {
        // The labeled path lives only in the second dependency source.
        let r = EgressPathMatcher::analyze_sources(
            "python3 main.py",
            &["import helper", "open(\"~/mail/archive.mbox\")"],
            &pats(&["~/mail/**"]),
        );
        assert!(r.matched());
    }

    // ── dependency source collection ──────────────────────────────────────

    #[test]
    fn collects_workspace_script_named_on_the_command_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("parse.py"), "open('~/mail/x')").unwrap();
        let ctx = ExecSourceContext {
            agent_dir: Some(dir.path()),
            gateway_dir: None,
            session_id: None,
            gateway_store: None,
        };
        let sources = collect_exec_dependency_sources("{}", "python3 parse.py --verbose", &ctx);
        assert_eq!(sources, vec!["open('~/mail/x')".to_string()]);
    }

    #[test]
    fn skips_non_script_and_flag_tokens() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), "~/mail/secret").unwrap();
        std::fs::write(dir.path().join("-oops.py"), "~/mail/secret").unwrap();
        let ctx = ExecSourceContext {
            agent_dir: Some(dir.path()),
            gateway_dir: None,
            session_id: None,
            gateway_store: None,
        };
        // `data.bin` has no script extension; `-oops.py` looks like a flag.
        let sources = collect_exec_dependency_sources("{}", "cat data.bin -oops.py", &ctx);
        assert!(sources.is_empty());
    }

    #[test]
    fn missing_script_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ExecSourceContext {
            agent_dir: Some(dir.path()),
            gateway_dir: None,
            session_id: None,
            gateway_store: None,
        };
        assert!(collect_exec_dependency_sources("{}", "python3 absent.py", &ctx).is_empty());
    }

    #[test]
    fn oversized_script_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_DEPENDENCY_BYTES as usize + 1);
        std::fs::write(dir.path().join("big.py"), big).unwrap();
        let ctx = ExecSourceContext {
            agent_dir: Some(dir.path()),
            gateway_dir: None,
            session_id: None,
            gateway_store: None,
        };
        assert!(collect_exec_dependency_sources("{}", "python3 big.py", &ctx).is_empty());
    }

    #[test]
    fn dependency_file_count_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = String::from("bash");
        for i in 0..(MAX_DEPENDENCY_FILES + 4) {
            let name = format!("s{i}.sh");
            std::fs::write(dir.path().join(&name), "echo hi").unwrap();
            command.push(' ');
            command.push_str(&name);
        }
        let ctx = ExecSourceContext {
            agent_dir: Some(dir.path()),
            gateway_dir: None,
            session_id: None,
            gateway_store: None,
        };
        let sources = collect_exec_dependency_sources("{}", &command, &ctx);
        assert_eq!(sources.len(), MAX_DEPENDENCY_FILES);
    }

    #[test]
    fn no_context_paths_collect_nothing() {
        let ctx = ExecSourceContext::default();
        assert!(collect_exec_dependency_sources("{}", "python3 parse.py", &ctx).is_empty());
    }

    #[test]
    fn script_extension_check_is_case_insensitive_and_gated() {
        assert!(has_script_extension("parse.py"));
        assert!(has_script_extension("dir/Parse.PY"));
        assert!(has_script_extension("run.sh"));
        assert!(!has_script_extension("data.bin"));
        assert!(!has_script_extension("README"));
    }

    // ── artifact bundles ──────────────────────────────────────────────────

    /// Build a real artifact bundle and return its canonical `art_*` id.
    fn build_bundle(gateway_dir: &Path, files: &[(&str, &str)]) -> String {
        let store = crate::artifact_store::ArtifactStore::new(gateway_dir).unwrap();
        let content = crate::runtime::content_store::ContentStore::new(gateway_dir).unwrap();
        let mut inputs: Vec<String> = Vec::new();
        for (name, body) in files {
            let handle = content.write(body.as_bytes()).unwrap();
            content.register_name("sess", name, &handle).unwrap();
            inputs.push((*name).to_string());
        }
        store
            .build(&inputs, None, None, "sess")
            .unwrap()
            .artifact_id
    }

    #[test]
    fn artifact_id_bundle_is_scanned() {
        let tmp = tempfile::tempdir().unwrap();
        let id = build_bundle(
            tmp.path(),
            &[("main.py", "open('~/mail/archive.mbox')")],
        );
        let ctx = ExecSourceContext {
            agent_dir: None,
            gateway_dir: Some(tmp.path()),
            session_id: Some("sess"),
            gateway_store: None,
        };
        let args = format!(r#"{{"artifact_id":"{id}","command":"python3 main.py"}}"#);
        let sources = collect_exec_dependency_sources(&args, "python3 main.py", &ctx);
        assert_eq!(sources, vec!["open('~/mail/archive.mbox')".to_string()]);
    }

    /// A bundle full of assets must not exhaust the budget before the code is
    /// looked at — scripts are taken first.
    #[test]
    fn script_files_are_taken_before_assets() {
        let tmp = tempfile::tempdir().unwrap();
        let mut files: Vec<(String, String)> = (0..MAX_DEPENDENCY_FILES + 4)
            .map(|i| (format!("asset{i}.bin"), format!("asset-{i}")))
            .collect();
        files.push(("reader.py".to_string(), "open('~/mail/x')".to_string()));
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(n, c)| (n.as_str(), c.as_str()))
            .collect();
        let id = build_bundle(tmp.path(), &refs);

        let ctx = ExecSourceContext {
            agent_dir: None,
            gateway_dir: Some(tmp.path()),
            session_id: Some("sess"),
            gateway_store: None,
        };
        let args = format!(r#"{{"artifact_id":"{id}"}}"#);
        let sources = collect_exec_dependency_sources(&args, "", &ctx);
        assert_eq!(sources.len(), MAX_DEPENDENCY_FILES);
        assert!(
            sources.iter().any(|s| s.contains("~/mail/x")),
            "the script must survive the budget even behind {} assets",
            MAX_DEPENDENCY_FILES + 4
        );
    }

    /// `artifact_ref` (the short `ar.*` form the tool schema tells agents to
    /// prefer) is not a bundle id — without resolution there is nothing to
    /// scan, and a path-bearing rule silently fails to label.
    #[test]
    fn unresolvable_artifact_ref_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ExecSourceContext {
            agent_dir: None,
            gateway_dir: Some(tmp.path()),
            session_id: Some("sess"),
            gateway_store: None,
        };
        // No store to resolve with → no sources, no panic.
        let sources =
            collect_exec_dependency_sources(r#"{"artifact_ref":"ar.mailparse"}"#, "", &ctx);
        assert!(sources.is_empty());
    }
}
