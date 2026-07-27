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
        let tokens = extract_path_tokens(command, script_body);
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
fn extract_path_tokens(command: &str, script_body: Option<&str>) -> Vec<String> {
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

    for chunk in [Some(command), script_body].into_iter().flatten() {
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
}
