//! Static analysis for detecting remote/network access in code.
//!
//! Analyzes code before execution to detect patterns that require
//! network access (HTTP requests, socket connections, etc.).
//! If detected, the code execution requires operator approval.

use regex::Regex;
use std::collections::HashSet;

use crate::runtime::network_policy::declaration_allows_target;
use autonoetic_types::agent::{RemoteAccessDeclaration, RemoteAccessLanguage};

/// Result of analyzing code for remote access patterns.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteAccessAnalysis {
    /// Whether the code contains patterns that require network access.
    pub requires_approval: bool,
    /// List of detected patterns that triggered the flag.
    pub detected_patterns: Vec<DetectedPattern>,
    /// Summary of what was detected.
    pub summary: String,
}

/// A detected pattern indicating potential remote access.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DetectedPattern {
    /// Category of the pattern (import, function_call, url_literal, etc.)
    pub category: String,
    /// The specific pattern matched.
    pub pattern: String,
    /// Line number where the pattern was found (1-indexed, approximate).
    pub line_number: Option<usize>,
    /// Why this pattern indicates remote access.
    pub reason: String,
}

/// Classification of network behavior in analyzed code.
///
/// Used downstream in approval reuse decisions:
/// - `Concrete`: concrete hosts known → cache, session grants, and approved-request reuse allowed
/// - `Unresolved`: network signals present but no stable concrete host coverage → skip reuse
/// - `None`: no network behavior detected → no approval needed
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum NetworkCoverage {
    Concrete { targets: Vec<String> },
    Unresolved,
    None,
}

/// Classifies detected patterns into a network coverage assessment.
///
/// `concrete_targets` should be pre-extracted via `normalize_targets()`.
///
/// Classification rules:
/// - No patterns → `None`
/// - No concrete targets (only `import`/`function_call`/`network_command`/`dependency_install`) → `Unresolved`
/// - Concrete targets + `dependency_install` → `Unresolved` (package install targets unknown registries)
/// - Concrete targets + any other signals → `Concrete` (weak signals like `import`/`function_call` don't block reuse)
pub fn classify_network_coverage(
    patterns: &[DetectedPattern],
    concrete_targets: Vec<String>,
) -> NetworkCoverage {
    if concrete_targets.is_empty() {
        if patterns.is_empty() {
            NetworkCoverage::None
        } else {
            NetworkCoverage::Unresolved
        }
    } else if patterns.iter().any(|p| p.category == "dependency_install") {
        NetworkCoverage::Unresolved
    } else {
        NetworkCoverage::Concrete {
            targets: concrete_targets,
        }
    }
}

/// Max pattern text length included in [`approval_remote_operator_suffix`].
const APPROVAL_REMOTE_PATTERN_SNIPPET_CHARS: usize = 96;

/// Max distinct `category:snippet` hints appended when hosts are unknown.
const APPROVAL_REMOTE_HINT_CAP: usize = 8;

fn collapse_detected_pattern_text(raw: &str) -> String {
    raw.lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

fn truncate_unicode_hint(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        s.to_string()
    } else {
        let keep = max_chars.saturating_sub(3);
        format!("{}...", s.chars().take(keep).collect::<String>())
    }
}

/// Extra text appended to operator-facing sandbox approval strings.
///
/// When [`crate::runtime::approved_exec_cache::normalize_targets`] finds URL/IP literals, returns
/// ` → hosts: host1, host2`. Otherwise lists short `category:snippet` entries from detected patterns so
/// approvers see what triggered the gate when no literal URL could be extracted.
pub fn approval_remote_operator_suffix(
    concrete_hosts: &[String],
    patterns: &[DetectedPattern],
) -> String {
    if !concrete_hosts.is_empty() {
        return format!(" → hosts: {}", concrete_hosts.join(", "));
    }
    if patterns.is_empty() {
        return String::new();
    }

    let mut seen = std::collections::HashSet::<String>::new();
    let mut parts = Vec::new();

    for p in patterns {
        let snippet = truncate_unicode_hint(
            &collapse_detected_pattern_text(&p.pattern),
            APPROVAL_REMOTE_PATTERN_SNIPPET_CHARS,
        );
        if snippet.is_empty() {
            continue;
        }
        let label = format!("{}:{}", p.category, snippet);
        if seen.insert(label.clone()) {
            parts.push(label);
        }
        if parts.len() >= APPROVAL_REMOTE_HINT_CAP {
            break;
        }
    }

    if parts.is_empty() {
        seen.clear();
        for p in patterns {
            let label = format!("{}:*", p.category);
            if seen.insert(label.clone()) {
                parts.push(label);
            }
            if parts.len() >= APPROVAL_REMOTE_HINT_CAP {
                break;
            }
        }
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" → signals: {}", parts.join("; "))
    }
}

/// Static analyzer for detecting remote access patterns in code.
pub struct RemoteAccessAnalyzer;

trait ImportLanguageDetector {
    fn language(&self) -> RemoteAccessLanguage;
    fn detect(&self, code: &str) -> Vec<DetectedPattern>;
}

fn push_unique_pattern(
    patterns: &mut Vec<DetectedPattern>,
    category: &str,
    pat_str: &str,
    line_num: usize,
    reason: &str,
) {
    if !patterns
        .iter()
        .any(|p| p.pattern == pat_str && p.line_number == Some(line_num))
    {
        patterns.push(DetectedPattern {
            category: category.to_string(),
            pattern: pat_str.to_string(),
            line_number: Some(line_num),
            reason: reason.to_string(),
        });
    }
}

fn collect_regex_matches(
    code: &str,
    regex: &str,
    category: &str,
    reason: &str,
    patterns: &mut Vec<DetectedPattern>,
) {
    if let Ok(re) = Regex::new(regex) {
        for mat in re.find_iter(code) {
            let line_num = code[..mat.start()].matches('\n').count() + 1;
            let pat_str = mat.as_str().trim();
            push_unique_pattern(patterns, category, pat_str, line_num, reason);
        }
    }
}

/// Library-name import detector for Python.
///
/// **Do not extend this list to cover a newly-encountered client library.** That
/// was the treadmill #1021 removed: the list can never keep up (it was missing
/// `http.client` — Python's own stdlib HTTP client — until sink detection
/// landed). Reaching the network requires bottoming out on a stdlib primitive,
/// and [`crate::runtime::network_sinks`] detects those structurally, so a new
/// library is covered with no code change.
///
/// The list remains useful for two narrower jobs, and should only change for
/// them: it is a coarse signal that a *module was imported at all* (which
/// catches intent even where no sink call is resolvable), and it is what the
/// `python_imports` declaration field is matched against.
struct PythonImportDetector;
impl ImportLanguageDetector for PythonImportDetector {
    fn language(&self) -> RemoteAccessLanguage {
        RemoteAccessLanguage::Python
    }

    fn detect(&self, code: &str) -> Vec<DetectedPattern> {
        let mut patterns = Vec::new();
        let import_patterns = vec![
            ("requests", "HTTP client library"),
            ("urllib", "URL handling library"),
            ("urllib.request", "URL request library"),
            ("urllib3", "HTTP client library"),
            ("httpx", "Async HTTP client library"),
            ("aiohttp", "Async HTTP client library"),
            ("socket", "Low-level networking library"),
            ("ftplib", "FTP client library"),
            ("imaplib", "IMAP client library"),
            ("poplib", "POP3 client library"),
            ("nntplib", "NNTP client library"),
            ("telnetlib", "Telnet client library"),
            ("smtplib", "SMTP client library"),
            ("paramiko", "SSH client library"),
            ("fabric", "SSH execution library"),
            ("websockets", "WebSocket client library"),
            ("redis", "Redis client library"),
            ("pymongo", "MongoDB client library"),
            ("mysql", "MySQL client library"),
            ("psycopg", "PostgreSQL client library"),
            ("sqlalchemy", "SQL toolkit (can connect to remote DBs)"),
            ("boto3", "AWS SDK (cloud access)"),
            ("google.cloud", "Google Cloud SDK"),
            ("azure", "Azure SDK"),
        ];
        for (lib, reason) in &import_patterns {
            let import_regex = format!(
                r"(?m)^\s*(?:import\s+{}|from\s+{}\s+import)",
                regex::escape(lib),
                regex::escape(lib)
            );
            collect_regex_matches(code, &import_regex, "import", reason, &mut patterns);
        }
        patterns
    }
}

struct JsImportDetector;
impl ImportLanguageDetector for JsImportDetector {
    fn language(&self) -> RemoteAccessLanguage {
        RemoteAccessLanguage::Javascript
    }

    fn detect(&self, code: &str) -> Vec<DetectedPattern> {
        let mut patterns = Vec::new();
        let regexes = vec![
            (
                r#"(?m)^\s*import\s+.*\s+from\s+["'](axios|node-fetch|undici|got|ws|http|https|net|tls)["']"#,
                "JS/TS network-capable module import",
            ),
            (
                r#"(?m)^\s*import\s+["'](axios|node-fetch|undici|got|ws|http|https|net|tls)["']"#,
                "JS/TS side-effect module import",
            ),
            (
                r#"(?m)^\s*(?:const|let|var)\s+\w+\s*=\s*require\(["'](axios|node-fetch|undici|got|ws|http|https|net|tls)["']\)"#,
                "JS/TS require() of network-capable module",
            ),
        ];
        for (regex, reason) in regexes {
            collect_regex_matches(code, regex, "import", reason, &mut patterns);
        }
        patterns
    }
}

struct RustImportDetector;
impl ImportLanguageDetector for RustImportDetector {
    fn language(&self) -> RemoteAccessLanguage {
        RemoteAccessLanguage::Rust
    }

    fn detect(&self, code: &str) -> Vec<DetectedPattern> {
        let mut patterns = Vec::new();
        let regexes = vec![
            (
                r"(?m)^\s*use\s+(reqwest|hyper|ureq|tokio::net|std::net|tokio_tungstenite|redis)(::|;)",
                "Rust use import for network-capable crate/module",
            ),
            (
                r"(?m)^\s*extern\s+crate\s+(reqwest|hyper|ureq|redis)\s*;",
                "Rust extern crate network-capable import",
            ),
        ];
        for (regex, reason) in regexes {
            collect_regex_matches(code, regex, "import", reason, &mut patterns);
        }
        patterns
    }
}

struct GoImportDetector;
impl ImportLanguageDetector for GoImportDetector {
    fn language(&self) -> RemoteAccessLanguage {
        RemoteAccessLanguage::Go
    }

    fn detect(&self, code: &str) -> Vec<DetectedPattern> {
        let mut patterns = Vec::new();
        let regexes = vec![
            (r#"(?m)^\s*(?:\w+\s+)?"net/http"\s*$"#, "Go net/http import"),
            (r#"(?m)^\s*(?:\w+\s+)?"net"\s*$"#, "Go net import"),
            (
                r#"(?m)^\s*(?:\w+\s+)?"golang\.org/x/net/websocket"\s*$"#,
                "Go x/net/websocket import",
            ),
            (
                r#"(?m)^\s*(?:\w+\s+)?"google\.golang\.org/grpc"\s*$"#,
                "Go gRPC import",
            ),
        ];
        for (regex, reason) in regexes {
            collect_regex_matches(code, regex, "import", reason, &mut patterns);
        }
        patterns
    }
}

fn import_detector_registry() -> Vec<Box<dyn ImportLanguageDetector>> {
    vec![
        Box::new(PythonImportDetector),
        Box::new(JsImportDetector),
        Box::new(RustImportDetector),
        Box::new(GoImportDetector),
    ]
}

fn enabled_import_languages(
    declaration: Option<&RemoteAccessDeclaration>,
) -> Option<HashSet<RemoteAccessLanguage>> {
    declaration.and_then(|decl| {
        if decl.enabled_languages.is_empty() {
            None
        } else {
            Some(decl.enabled_languages.iter().copied().collect())
        }
    })
}

/// One import-detection pass: the deduped import patterns plus the set of
/// languages whose detectors actually fired.
///
/// That language set *is* the import signal used to scope function-call
/// heuristics ([`language_scope_for_code`]). Carrying it out of the pass that
/// collected the patterns keeps `analyze_code_with_declaration` to a single
/// import scan — this analysis sits on the `sandbox.exec` hot path, so running
/// every detector's regexes a second time just to re-derive the language set is
/// not affordable.
#[derive(Default)]
struct ImportScan {
    patterns: Vec<DetectedPattern>,
    languages: HashSet<RemoteAccessLanguage>,
}

/// Determines the languages whose function-call patterns should apply to the
/// code being analyzed. Precedence:
///   1. `enabled_languages` from the declaration (authoritative when set).
///   2. Otherwise the languages implied by the code's own import signals —
///      `import_signal`, the language set of the [`ImportScan`] the caller
///      already ran (no re-scan here).
///   3. Otherwise `None` — no language signal, so *all* tagged patterns stay
///      active (conservative: no detection is lost).
fn language_scope_for_code(
    enabled_languages: Option<&HashSet<RemoteAccessLanguage>>,
    import_signal: &HashSet<RemoteAccessLanguage>,
) -> Option<HashSet<RemoteAccessLanguage>> {
    if let Some(set) = enabled_languages {
        if !set.is_empty() {
            return Some(set.clone());
        }
    }
    if import_signal.is_empty() {
        // No import signal → language unknown → unrestricted.
        None
    } else {
        Some(import_signal.clone())
    }
}

/// Whether a call pattern tagged `tag` runs under `active` languages.
/// `None`-tagged patterns are language-agnostic and always run. When `active`
/// is `None` (language unknown) every tagged pattern runs — conservative.
fn pattern_applies_in_scope(
    tag: Option<RemoteAccessLanguage>,
    active: Option<&HashSet<RemoteAccessLanguage>>,
) -> bool {
    match tag {
        None => true,
        Some(lang) => match active {
            None => true,
            Some(set) => set.contains(&lang),
        },
    }
}

fn normalize_declared_pattern(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// Extract the host component from a URL literal string (e.g. `https://api.example.com/v1`
/// → `api.example.com`). Returns None for unparseable URLs.
pub fn extract_host_from_url_literal(url: &str) -> Option<String> {
    let re = Regex::new(r"(?i)^[a-z]+://([^/:]+)").ok()?;
    let captures = re.captures(url)?;
    let host = captures.get(1)?.as_str().trim_end_matches('.');
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

fn observed_matches_declared(observed: &str, declared_patterns: &[String]) -> bool {
    let observed = normalize_declared_pattern(observed);
    declared_patterns.iter().any(|raw| {
        let declared = normalize_declared_pattern(raw);
        !declared.is_empty()
            && (observed == declared
                || observed.starts_with(&declared)
                || observed.contains(&declared))
    })
}

fn is_package_manager_command_pattern(pattern: &str) -> bool {
    let p = normalize_declared_pattern(pattern);
    [
        "pip install",
        "pip3 install",
        "npm install",
        "yarn install",
        "yarn add",
        "pnpm install",
        "bun install",
        "go get",
        "go mod download",
        "cargo install",
        "gem install",
        "composer install",
        "composer require",
        "apt-get install",
        "apt-get update",
        "apk add",
        "yum install",
        "dnf install",
        "pacman -s",
    ]
    .iter()
    .any(|prefix| p.starts_with(prefix))
}

/// Signal categories that the declaration **gates**: an occurrence not covered by
/// the declaration fails shut with `undeclared_remote_pattern`.
///
/// These are the durable half of the contract (#1023). `url_literal`/`ip_address`
/// name a *reachable target*, checked against `remote_access.targets`;
/// `network_command`/`dependency_install` name a *shell execution surface*,
/// checked against `shell_commands`/`package_manager_commands`. All four are
/// things an agent can state from its own intent, without knowing anything about
/// the analyzer.
const GATING_CATEGORIES: &[&str] = &[
    "url_literal",
    "ip_address",
    "network_command",
    "dependency_install",
];

/// Signal categories that are **advisory**: detected, shown to the operator at the
/// approval gate, but never a reason to fail shut on their own (#1023).
///
/// These describe *how* code reaches the network rather than *what it reaches*,
/// and mirroring them required agents to know analyzer internals.
/// session-912c7791 is the evidence: the coder declared
/// `function_calls: ["imaplib.fetch("]` while the analyzer detects the bare
/// `fetch(` — a declaration that could never match — after ~30 turns of guessing
/// at the schema.
const ADVISORY_CATEGORIES: &[&str] = &["import", "function_call", "network_sink"];

/// Every declared import pattern, across languages.
fn declared_import_patterns(decl: &RemoteAccessDeclaration) -> Vec<String> {
    let mut out = Vec::new();
    out.extend(decl.python_imports.clone());
    out.extend(decl.js_imports.clone());
    out.extend(decl.rust_imports.clone());
    out.extend(decl.go_imports.clone());
    out
}

/// Whether one detected pattern is covered by the declaration, per category.
/// Used by both the gating and the advisory views.
fn declaration_covers_pattern(decl: &RemoteAccessDeclaration, p: &DetectedPattern) -> bool {
    match p.category.as_str() {
        "import" => observed_matches_declared(&p.pattern, &declared_import_patterns(decl)),
        "function_call" => observed_matches_declared(&p.pattern, &decl.function_calls),
        // A sink is resolved structurally by the gateway, never declared.
        "network_sink" => true,
        "url_literal" => extract_host_from_url_literal(&p.pattern)
            .map(|host| declaration_allows_target(decl, &host, Some(&p.pattern)))
            .unwrap_or(false),
        "ip_address" => declaration_allows_target(decl, &p.pattern, None),
        "network_command" => {
            if is_package_manager_command_pattern(&p.pattern) {
                observed_matches_declared(&p.pattern, &decl.package_manager_commands)
            } else {
                observed_matches_declared(&p.pattern, &decl.shell_commands)
            }
        }
        "dependency_install" => !decl.package_manager_commands.is_empty(),
        _ => true,
    }
}

/// Returns detected patterns the declaration must cover but does not — the set
/// whose non-emptiness fails an exec shut with `undeclared_remote_pattern`.
///
/// Enforcement is declaration-gated: an agent that declares no remote-access
/// surface at all gets an empty set here (the upstream
/// `missing_remote_access_declaration` check owns that case).
///
/// # What is gated, and why only that (#1023)
///
/// **`targets` (hosts) are the authoritative declaration.** A concrete
/// `url_literal`/`ip_address` must be covered by `remote_access.targets`, and a
/// `network_command`/`dependency_install` by
/// `shell_commands`/`package_manager_commands`. Those are statements of intent an
/// agent can write from what it is trying to do.
///
/// **Import and function-call lists are advisory.** They used to gate too, which
/// forced agents to mirror the analyzer's own pattern strings — a contract agents
/// demonstrably cannot satisfy (see [`ADVISORY_CATEGORIES`]). Since #1021 resolves
/// sinks structurally, requiring the agent to re-declare what the gateway already
/// derives is pure friction.
///
/// Demoting them does not make an undetected exec reachable, because the
/// declaration is not the network boundary. What remains between agent code and
/// the network: the `NetworkAccess` capability ceiling (with install-time
/// `detected_network_hosts` coverage, P-1.5), `targets` still gating concrete
/// hosts here, operator approval at the gate, and the per-exec grant from #1022
/// without which the sandbox has no network namespace at all. The advisory signals
/// are still shown to the operator when they approve.
pub fn undeclared_patterns_against_manifest(
    patterns: &[DetectedPattern],
    declaration: Option<&RemoteAccessDeclaration>,
) -> Vec<DetectedPattern> {
    let Some(decl) = declaration else {
        return Vec::new();
    };
    patterns
        .iter()
        .filter(|p| GATING_CATEGORIES.contains(&p.category.as_str()))
        .filter(|p| !declaration_covers_pattern(decl, p))
        .cloned()
        .collect()
}

/// Advisory signals the declaration does not name: import/function-call activity
/// outside what the agent declared.
///
/// Never a reason to refuse — this is drift, surfaced so an operator (or a curator
/// reviewing manifest hygiene) can see that a declaration has fallen behind the
/// code. `network_sink` is excluded: sinks are derived by the gateway and were
/// never something an agent declares.
pub fn advisory_undeclared_patterns(
    patterns: &[DetectedPattern],
    declaration: Option<&RemoteAccessDeclaration>,
) -> Vec<DetectedPattern> {
    let Some(decl) = declaration else {
        return Vec::new();
    };
    patterns
        .iter()
        .filter(|p| {
            ADVISORY_CATEGORIES.contains(&p.category.as_str()) && p.category != "network_sink"
        })
        .filter(|p| !declaration_covers_pattern(decl, p))
        .cloned()
        .collect()
}

/// JavaScript receivers whose `.fetch(` is the global Fetch API, not an
/// arbitrary object method. Keeping this an allowlist (rather than flagging
/// every `.fetch(`) preserves the disambiguation `detect_function_calls`
/// exists for — `IMAP4.fetch`, DBAPI `cursor.fetch`, collection `.fetch`, …
/// — so only the genuine JS global is admitted. See [`is_global_fetch_receiver`].
const GLOBAL_FETCH_RECEIVERS: &[&str] = &["globalThis", "window", "self"];

/// Is the `.fetch(` ending at `fetch_pos` (the index of the `f`) reached via a
/// known JavaScript global receiver? Returns true for `globalThis.fetch(`,
/// `window.fetch(`, `self.fetch(`. The receiver is the maximal `[A-Za-z0-9_]`
/// run immediately before the `.` at `fetch_pos - 1`. Byte-based by necessity
/// (the `regex` crate has no lookbehind); the split-by-whitespace form
/// (`obj.\nfetch(`) is tracked by #1020.
fn is_global_fetch_receiver(code: &str, fetch_pos: usize) -> bool {
    let bytes = code.as_bytes();
    // `fetch_pos` indexes 'f' in `fetch(`; the byte before it must be '.'.
    let mut end = match fetch_pos.checked_sub(2) {
        Some(e) if e < bytes.len() => e,
        _ => return false,
    };
    if !(bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        return false; // no receiver identifier directly before the '.'
    }
    let mut start = end;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    matches!(
        &code[start..=end],
        "globalThis" | "window" | "self"
    )
}

impl RemoteAccessAnalyzer {
    /// Analyzes code for patterns that require network/remote access.
    ///
    /// Detection categories:
    /// 1. Import statements for network libraries
    /// 2. Function/method calls for network operations
    /// 3. URL literals (http://, https://, ftp://)
    /// 4. IP address literals
    /// 5. Network shell commands (pip install, curl, git clone, etc.)
    pub fn analyze_code(code: &str) -> RemoteAccessAnalysis {
        Self::analyze_code_with_declaration(code, None)
    }

    /// Analyzes code while honoring optional manifest declaration knobs
    /// (e.g. `enabled_languages` for import detector selection).
    pub fn analyze_code_with_declaration(
        code: &str,
        declaration: Option<&RemoteAccessDeclaration>,
    ) -> RemoteAccessAnalysis {
        let mut patterns = Vec::new();
        let enabled_imports = enabled_import_languages(declaration);

        // Check for network-related imports. The scan also reports which
        // languages' detectors fired, so the function-call scope below comes out
        // of this single pass rather than a second one.
        let import_scan = Self::detect_imports(code, enabled_imports.as_ref());

        // Determine which languages are active for function-call scoping:
        //  1. `enabled_languages` declared by the agent (authoritative), or
        //  2. inferred from the import signal the scan just produced, or
        //  3. unknown (no import signal) → scope disabled → all patterns run.
        let active_languages =
            language_scope_for_code(enabled_imports.as_ref(), &import_scan.languages);

        patterns.extend(import_scan.patterns);

        // Check for network-related function calls
        patterns.extend(Self::detect_function_calls(code, active_languages.as_ref()));

        // Check for network sinks reached through the code's own import bindings
        // (#1021). Structural: a client library that is on no import list is
        // still detected, because it bottoms out on the closed stdlib/builtin
        // sink set. Scoped by the same active-language set as the call
        // heuristics.
        patterns.extend(Self::detect_network_sinks(code, active_languages.as_ref()));

        // Check for URL literals
        patterns.extend(Self::detect_url_literals(code));

        // Check for IP address literals
        patterns.extend(Self::detect_ip_addresses(code));

        // Check for network shell commands
        patterns.extend(Self::detect_network_commands(code));

        let requires_approval = !patterns.is_empty();

        let summary = if patterns.is_empty() {
            "No remote access patterns detected".to_string()
        } else {
            let categories: Vec<&str> = patterns.iter().map(|p| p.category.as_str()).collect();
            let unique_categories: Vec<&str> = categories
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            format!(
                "Detected {} remote access pattern(s) in categories: {}",
                patterns.len(),
                unique_categories.join(", ")
            )
        };

        RemoteAccessAnalysis {
            requires_approval,
            detected_patterns: patterns,
            summary,
        }
    }

    /// Detects import statements using registered language detectors.
    ///
    /// When `enabled_languages` is set, only those language detectors run.
    /// Returns the deduped patterns *and* the set of languages whose detectors
    /// fired, so the caller gets the import signal for function-call scoping out
    /// of this pass instead of re-running every detector.
    fn detect_imports(
        code: &str,
        enabled_languages: Option<&HashSet<RemoteAccessLanguage>>,
    ) -> ImportScan {
        let mut scan = ImportScan::default();
        for detector in import_detector_registry() {
            let lang = detector.language();
            let detector_enabled = match enabled_languages {
                None => true,
                Some(set) => set.contains(&lang),
            };
            if !detector_enabled {
                continue;
            }
            let detected = detector.detect(code);
            if !detected.is_empty() {
                scan.languages.insert(lang);
            }
            for p in detected {
                if !scan.patterns.iter().any(|existing| {
                    existing.pattern == p.pattern && existing.line_number == p.line_number
                }) {
                    scan.patterns.push(p);
                }
            }
        }
        scan
    }

    /// Detects network sinks resolved through the code's own import bindings
    /// (see [`crate::runtime::network_sinks`]).
    ///
    /// This is the structural counterpart to the library-name import lists: it
    /// answers "does this code reach a network primitive?" instead of "does this
    /// code name a library we happen to know about?", so an unlisted or brand-new
    /// client is still detected.
    ///
    /// Emitted under the `network_sink` category, which
    /// [`undeclared_patterns_against_manifest`] does not gate — the sink is a
    /// *detection* signal that raises the approval gate, and does not add
    /// anything new that agents must enumerate in their declaration. Narrowing
    /// what must be declared is #1023's decision, not this one's.
    ///
    /// When the language is unknown both resolvers run. That is safe rather than
    /// merely conservative: each resolver only matches calls bound by its own
    /// language's import grammar, so the Python resolver binds nothing in JS
    /// source and vice versa.
    fn detect_network_sinks(
        code: &str,
        active: Option<&HashSet<RemoteAccessLanguage>>,
    ) -> Vec<DetectedPattern> {
        use crate::runtime::network_sinks::{detect_javascript_sinks, detect_python_sinks};

        let mut found = Vec::new();
        if pattern_applies_in_scope(Some(RemoteAccessLanguage::Python), active) {
            found.extend(detect_python_sinks(code));
        }
        if pattern_applies_in_scope(Some(RemoteAccessLanguage::Javascript), active) {
            found.extend(detect_javascript_sinks(code));
        }

        let mut patterns: Vec<DetectedPattern> = Vec::new();
        for sink in found {
            // Name the resolved sink, and the as-written call when the two
            // differ, so an operator reading the approval sees both the
            // primitive and the alias it arrived through.
            let reason = if sink.matched == sink.sink {
                sink.reason.clone()
            } else {
                format!("{} (via `{}`)", sink.reason, sink.matched)
            };
            push_unique_pattern(
                &mut patterns,
                "network_sink",
                &sink.sink,
                sink.line,
                &reason,
            );
        }
        patterns
    }

    /// Detects function/method calls for network operations.
    fn detect_function_calls(
        code: &str,
        active: Option<&HashSet<RemoteAccessLanguage>>,
    ) -> Vec<DetectedPattern> {
        let mut patterns = Vec::new();

        // (language tag, pattern, reason). None = applies in any language.
        let call_patterns = vec![
            (None, r"\.connect\s*\(", "Socket connection initiation"),
            (None, r"\.send\s*\(", "Sending data over network"),
            (None, r"\.recv\s*\(", "Receiving data from network"),
            (None, r"\.bind\s*\(", "Socket binding"),
            (None, r"\.listen\s*\(", "Socket listening"),
            (None, r"\.accept\s*\(", "Socket accept connection"),
            (
                Some(RemoteAccessLanguage::Python),
                r"urlopen\s*\(",
                "Opening URL connection",
            ),
            (
                Some(RemoteAccessLanguage::Python),
                r"requests\.(get|post|put|delete|patch|head|options)\s*\(",
                "HTTP request",
            ),
            (
                Some(RemoteAccessLanguage::Python),
                r"httpx\.(get|post|put|delete|patch|head|options)\s*\(",
                "HTTP request",
            ),
            (None, r"\.get\s*\(.*http", "HTTP GET request"),
            (None, r"\.post\s*\(.*http", "HTTP POST request"),
            (
                Some(RemoteAccessLanguage::Javascript),
                r"axios\.(get|post|put|delete|patch|head|options)\s*\(",
                "Axios HTTP request",
            ),
            (
                Some(RemoteAccessLanguage::Javascript),
                r"(http|https)\.(get|request)\s*\(",
                "Node HTTP/HTTPS request",
            ),
            (
                Some(RemoteAccessLanguage::Javascript),
                r"net\.connect\s*\(",
                "Node net.connect call",
            ),
            (
                Some(RemoteAccessLanguage::Javascript),
                r"WebSocket\s*\(",
                "WebSocket connection",
            ),
            (None, r"connect\s*\(.*ws://", "WebSocket connection"),
            (None, r"connect\s*\(.*wss://", "Secure WebSocket connection"),
            (
                Some(RemoteAccessLanguage::Rust),
                r"(reqwest|ureq)::(get|post|put|delete|patch|head)\s*\(",
                "Rust HTTP request function call",
            ),
            (
                Some(RemoteAccessLanguage::Rust),
                r"(std::net|tokio::net)::TcpStream::connect\s*\(",
                "Rust TCP stream connect call",
            ),
            (
                Some(RemoteAccessLanguage::Go),
                r"http\.(Get|Post|Head)\s*\(",
                "Go net/http request call",
            ),
            (
                Some(RemoteAccessLanguage::Go),
                r"\.Do\s*\(",
                "HTTP client Do() call",
            ),
        ];

        for (tag, pattern, reason) in &call_patterns {
            if !pattern_applies_in_scope(*tag, active) {
                continue;
            }
            if let Ok(re) = Regex::new(pattern) {
                for mat in re.find_iter(code) {
                    let line_num = code[..mat.start()].matches('\n').count() + 1;
                    // Avoid duplicate detection
                    let pat_str = mat.as_str().trim().to_string();
                    if !patterns.iter().any(|p: &DetectedPattern| {
                        p.pattern == pat_str && p.line_number == Some(line_num)
                    }) {
                        patterns.push(DetectedPattern {
                            category: "function_call".to_string(),
                            pattern: pat_str,
                            line_number: Some(line_num),
                            reason: reason.to_string(),
                        });
                    }
                }
            }
        }

        // `fetch(` is the JavaScript global HTTP API. Disambiguate it from
        // same-named *method* calls on other objects — `imaplib`'s
        // `IMAP4.fetch()`, DBAPI cursor `.fetch()`, collection `.fetch()` —
        // which are either not HTTP or are already covered by import-level
        // detection. Only flag a `fetch(` that is NOT preceded by `.`, a word
        // char, `]`, or `)`, i.e. a standalone/global call. (The `regex` crate
        // has no lookbehind, so this boundary check is byte-based.)
        //
        // Exception: `globalThis.fetch(` / `window.fetch(` / `self.fetch(` are
        // the global Fetch API reached via a known global receiver, not an
        // arbitrary object method — so the `.`-preceded case still flags when
        // the receiver is in [`GLOBAL_FETCH_RECEIVERS`]. The split-by-whitespace
        // method form (`obj.\nfetch(`) is a parser-level concern tracked by
        // #1020; the byte boundary here is the first-aid fix for #1019.
        //
        // The whole block is JavaScript-scoped: when the code's language is
        // known to be something else (declared `enabled_languages`, or inferred
        // from import signals), `fetch(` is not treated as the JS Fetch API.
        if pattern_applies_in_scope(Some(RemoteAccessLanguage::Javascript), active) {
            if let Ok(re) = Regex::new(r"fetch\s*\(") {
                let bytes = code.as_bytes();
                for mat in re.find_iter(code) {
                    let prev_is_method_context = match mat.start().checked_sub(1) {
                        Some(i) if bytes[i] == b'.' => {
                            // `.fetch(` — flag only if the receiver is a known JS
                            // global (the Fetch API), not an arbitrary method call.
                            !is_global_fetch_receiver(code, mat.start())
                        }
                        Some(i) => {
                            matches!(bytes[i], b']' | b')')
                                || bytes[i].is_ascii_alphanumeric()
                                || bytes[i] == b'_'
                        }
                        None => false,
                    };
                    if prev_is_method_context {
                        continue;
                    }
                    let line_num = code[..mat.start()].matches('\n').count() + 1;
                    let pat_str = mat.as_str().trim().to_string();
                    if !patterns.iter().any(|p: &DetectedPattern| {
                        p.pattern == pat_str && p.line_number == Some(line_num)
                    }) {
                        patterns.push(DetectedPattern {
                            category: "function_call".to_string(),
                            pattern: pat_str,
                            line_number: Some(line_num),
                            reason: "Fetch API call".to_string(),
                        });
                    }
                }
            }
        }

        patterns
    }

    /// Detects URL literals in the code.
    fn detect_url_literals(code: &str) -> Vec<DetectedPattern> {
        let mut patterns = Vec::new();

        // Match http://, https://, ftp:// URLs
        let url_regex = r#"(https?|ftp)://[^\s"'`,;)}\]]+"#;
        if let Ok(re) = Regex::new(url_regex) {
            for mat in re.find_iter(code) {
                let line_num = code[..mat.start()].matches('\n').count() + 1;
                let url = mat.as_str();

                // Skip common false positives
                if url.contains("example.com") || url.contains("localhost") {
                    continue;
                }

                patterns.push(DetectedPattern {
                    category: "url_literal".to_string(),
                    pattern: url.to_string(),
                    line_number: Some(line_num),
                    reason: "URL literal indicates external resource access".to_string(),
                });
            }
        }

        patterns
    }

    /// Detects IP address literals in the code.
    fn detect_ip_addresses(code: &str) -> Vec<DetectedPattern> {
        let mut patterns = Vec::new();

        // Match IPv4 addresses (not 0.0.0.0 or 127.0.0.1 which are local)
        let ip_regex = r"\b(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\b";
        if let Ok(re) = Regex::new(ip_regex) {
            for mat in re.find_iter(code) {
                let line_num = code[..mat.start()].matches('\n').count() + 1;
                let ip = mat.as_str();

                // Skip local/loopback addresses
                if ip.starts_with("127.") || ip == "0.0.0.0" {
                    continue;
                }

                patterns.push(DetectedPattern {
                    category: "ip_address".to_string(),
                    pattern: ip.to_string(),
                    line_number: Some(line_num),
                    reason: "IP address literal indicates external network access".to_string(),
                });
            }
        }

        patterns
    }

    /// Detects shell commands that require network access.
    ///
    /// Catches package managers, download tools, and VCS network operations
    /// regardless of language. This is a generic capability detection mechanism —
    /// these commands *always* need network, so their presence means the sandbox
    /// must have network access (via approval or agent capability).
    pub fn detect_network_commands(code: &str) -> Vec<DetectedPattern> {
        let mut patterns = Vec::new();

        let command_patterns: &[(&str, &str)] = &[
            // Language package managers
            (
                "pip install",
                "pip install downloads packages from the network",
            ),
            (
                "pip3 install",
                "pip3 install downloads packages from the network",
            ),
            (
                "npm install",
                "npm install downloads packages from the network",
            ),
            (
                "yarn install",
                "yarn install downloads packages from the network",
            ),
            ("yarn add", "yarn add downloads packages from the network"),
            (
                "pnpm install",
                "pnpm install downloads packages from the network",
            ),
            (
                "bun install",
                "bun install downloads packages from the network",
            ),
            ("go get", "go get downloads modules from the network"),
            (
                "go mod download",
                "go mod download fetches modules from the network",
            ),
            (
                "cargo install",
                "cargo install downloads crates from the network",
            ),
            ("gem install", "gem install downloads gems from the network"),
            (
                "composer install",
                "composer install downloads packages from the network",
            ),
            (
                "composer require",
                "composer require downloads packages from the network",
            ),
            // Download tools
            ("curl ", "curl makes network requests"),
            ("wget ", "wget downloads files from the network"),
            // System package managers
            (
                "apt-get install",
                "apt-get install downloads packages from the network",
            ),
            (
                "apt-get update",
                "apt-get update fetches package lists from the network",
            ),
            ("apk add", "apk add downloads packages from the network"),
            (
                "yum install",
                "yum install downloads packages from the network",
            ),
            (
                "dnf install",
                "dnf install downloads packages from the network",
            ),
            ("pacman -S", "pacman -S downloads packages from the network"),
            // VCS network operations
            (
                "git clone",
                "git clone fetches a repository from the network",
            ),
            ("git fetch", "git fetch contacts a remote repository"),
            ("git pull", "git pull fetches and merges from a remote"),
            ("git push", "git push sends commits to a remote"),
        ];

        for (pattern_str, reason) in command_patterns {
            if let Some(line_num) = code.lines().enumerate().find_map(|(i, line)| {
                if line.contains(pattern_str) {
                    Some(i + 1)
                } else {
                    None
                }
            }) {
                // Deduplicate: only one detection per unique command pattern
                let pat = pattern_str.trim().to_string();
                if !patterns.iter().any(|p: &DetectedPattern| p.pattern == pat) {
                    patterns.push(DetectedPattern {
                        category: "network_command".to_string(),
                        pattern: pat,
                        line_number: Some(line_num),
                        reason: reason.to_string(),
                    });
                }
            }
        }

        patterns
    }

    /// Analyzes a sandbox exec command together with its declared dependency packages.
    ///
    /// This is the primary entry point for sandbox exec analysis. It combines:
    /// - Code-level pattern detection (imports, function calls, URLs, IPs, network commands)
    /// - Dependency awareness: non-empty `dep_packages` implies a package install step
    ///   that requires network access (pip install / npm install / etc.)
    ///
    /// The dependency check is a generic mechanism: if the agent declares packages,
    /// the sandbox MUST install them, which requires network. No intelligence needed.
    pub fn analyze_command_and_dependencies(
        code: &str,
        dep_packages: Option<&[String]>,
    ) -> RemoteAccessAnalysis {
        Self::analyze_command_and_dependencies_with_declaration(code, dep_packages, None)
    }

    /// Same as [`Self::analyze_command_and_dependencies`] with optional
    /// manifest declaration to constrain pluggable import detectors.
    pub fn analyze_command_and_dependencies_with_declaration(
        code: &str,
        dep_packages: Option<&[String]>,
        declaration: Option<&RemoteAccessDeclaration>,
    ) -> RemoteAccessAnalysis {
        let mut patterns = Self::analyze_code_with_declaration(code, declaration);

        if let Some(packages) = dep_packages {
            if !packages.is_empty() {
                patterns.detected_patterns.push(DetectedPattern {
                    category: "dependency_install".to_string(),
                    pattern: format!("packages: [{}]", packages.join(", ")),
                    line_number: None,
                    reason: format!(
                        "Package installation ({} package(s)) requires network access",
                        packages.len()
                    ),
                });
                patterns.requires_approval = true;
            }
        }

        if !patterns.detected_patterns.is_empty() {
            let categories: Vec<&str> = patterns
                .detected_patterns
                .iter()
                .map(|p| p.category.as_str())
                .collect();
            let unique_categories: Vec<&str> = categories
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            patterns.summary = format!(
                "Detected {} remote access pattern(s) in categories: {}",
                patterns.detected_patterns.len(),
                unique_categories.join(", ")
            );
        }

        patterns
    }

    /// Analyzes code including transitively imported workspace files.
    ///
    /// Parses `import X` / `from X import` from the primary code, matches module names
    /// against workspace files (e.g., `import mymod` → `mymod.py`), and recursively
    /// analyzes each matched file. Results are merged (union of detected patterns).
    pub fn analyze_code_with_workspace(
        code: &str,
        workspace_files: &[(String, String)],
    ) -> RemoteAccessAnalysis {
        let mut patterns = Self::analyze_code(code);

        // Parse import module names from the primary code
        let import_re = Regex::new(r"(?m)^\s*(?:import\s+(\w+)|from\s+(\w+)\s+import)").unwrap();

        let module_names: std::collections::HashSet<String> = import_re
            .captures_iter(code)
            .filter_map(|cap| {
                cap.get(1)
                    .or_else(|| cap.get(2))
                    .map(|m| m.as_str().to_string())
            })
            .collect();

        if module_names.is_empty() || workspace_files.is_empty() {
            return patterns;
        }

        // Build filename → content lookup from workspace
        let workspace_map: std::collections::HashMap<String, &str> = workspace_files
            .iter()
            .map(|(name, content)| {
                // Strip directory prefix and extension for matching
                let base = std::path::Path::new(name)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(name)
                    .to_string();
                (base, content.as_str())
            })
            .collect();

        // Analyze each imported module found in workspace
        let mut analyzed: std::collections::HashSet<String> = std::collections::HashSet::new();
        for module in &module_names {
            if analyzed.contains(module) {
                continue;
            }
            if let Some(content) = workspace_map.get(module) {
                analyzed.insert(module.clone());
                let transitive = Self::analyze_code(content);
                for pat in transitive.detected_patterns {
                    // Avoid duplicates
                    if !patterns
                        .detected_patterns
                        .iter()
                        .any(|p| p.pattern == pat.pattern && p.category == pat.category)
                    {
                        patterns.detected_patterns.push(pat);
                    }
                }
            }
        }

        if !patterns.detected_patterns.is_empty() {
            patterns.requires_approval = true;
            let categories: Vec<&str> = patterns
                .detected_patterns
                .iter()
                .map(|p| p.category.as_str())
                .collect();
            let unique_categories: Vec<&str> = categories
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            patterns.summary = format!(
                "Detected {} remote access pattern(s) in categories: {}",
                patterns.detected_patterns.len(),
                unique_categories.join(", ")
            );
        }

        patterns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_remote_access() {
        let code = r#"
import json
import math

def calculate(x, y):
    return math.sqrt(x**2 + y**2)

result = calculate(3, 4)
print(json.dumps({"result": result}))
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(!analysis.requires_approval);
        assert!(analysis.detected_patterns.is_empty());
    }

    #[test]
    fn test_http_import_detected() {
        let code = r#"
import requests

def get_data(url):
    return requests.get(url).json()
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "import"));
    }

    #[test]
    fn test_urllib_import_detected() {
        let code = r#"
from urllib.request import urlopen

def fetch(url):
    with urlopen(url) as response:
        return response.read()
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.pattern.contains("urllib")));
    }

    #[test]
    fn test_socket_calls_detected() {
        let code = r#"
import socket

s = socket.socket()
s.connect(("example.com", 80))
s.send(b"GET / HTTP/1.1")
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "function_call"));
    }

    #[test]
    fn test_url_literal_detected() {
        let code = r#"
import json

API_URL = "https://api.open-meteo.com/v1/forecast"
data = {"temp": 22}
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "url_literal"));
    }

    #[test]
    fn test_ip_address_detected() {
        let code = r#"
SERVER_IP = "192.168.1.100"
PORT = 8080
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "ip_address"));
    }

    #[test]
    fn test_local_ip_not_flagged() {
        let code = r#"
LOCAL_HOST = "127.0.0.1"
LOOPBACK = "127.0.0.1"
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(!analysis.requires_approval);
    }

    #[test]
    fn test_requests_get_detected() {
        let code = r#"
import requests

response = requests.get("https://api.example.com/data")
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        // Should have both import and function_call detections
        assert!(analysis.detected_patterns.len() >= 2);
    }

    #[test]
    fn test_imaplib_and_stdlib_mail_imports_detected() {
        // imaplib (and poplib/nntplib/telnetlib) were missing from
        // PythonImportDetector — session-912c7791's fetch_gmail.py had no
        // import-level network signal, so no declaration could cover it.
        for (lib, needle) in [
            ("imaplib", "IMAP"),
            ("poplib", "POP3"),
            ("nntplib", "NNTP"),
            ("telnetlib", "Telnet"),
        ] {
            let code = format!("import {lib}\nconn = {lib}.IMAP4_SSL('imap.gmail.com')\n");
            let analysis = RemoteAccessAnalyzer::analyze_code(&code);
            assert!(
                analysis
                    .detected_patterns
                    .iter()
                    .any(|p| p.category == "import" && p.reason.contains(needle)),
                "expected {lib} import detection ({needle}) for code:\n{code}"
            );
        }
    }

    #[test]
    fn test_imaplib_fetch_method_not_flagged_as_fetch_api() {
        // `IMAP4.fetch()` collided with the generic `fetch(` heuristic.
        // The boundary disambiguation must NOT flag the method-call form.
        let code = r#"
import imaplib
mail = imaplib.IMAP4_SSL("imap.gmail.com")
typ, data = mail.fetch(b"1", "(RFC822)")
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        let fetch_api_count = analysis
            .detected_patterns
            .iter()
            .filter(|p| p.reason == "Fetch API call")
            .count();
        assert_eq!(
            fetch_api_count, 0,
            "imaplib mail.fetch() must not be flagged as a Fetch API call: {:?}",
            analysis.detected_patterns
        );
        // The imaplib import is still detected (so a declaration can cover it).
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "import" && p.reason.contains("IMAP")));
    }

    #[test]
    fn test_global_fetch_still_flagged() {
        // Standalone/global `fetch(` (the JS HTTP API) must still be detected.
        for code in [
            r#"const r = fetch("https://example.com")"#,
            r#"await fetch(url)"#,
            "\nfetch(\n  'https://x')",
        ] {
            let analysis = RemoteAccessAnalyzer::analyze_code(code);
            assert!(
                analysis
                    .detected_patterns
                    .iter()
                    .any(|p| p.reason == "Fetch API call"),
                "global fetch( must be flagged for code:\n{code}"
            );
        }
    }

    #[test]
    fn test_global_fetch_receivers_flagged() {
        // `globalThis.fetch` / `window.fetch` / `self.fetch` are the JS global
        // Fetch API reached via a known global receiver — they must be flagged,
        // even though the byte before `fetch(` is `.`. Review feedback on #1019:
        // the immediate-byte disambiguation otherwise treated these as method
        // calls and missed them.
        for code in [
            r#"globalThis.fetch("https://example.com")"#,
            r#"window.fetch(url)"#,
            r#"self.fetch("/api")"#,
        ] {
            let analysis = RemoteAccessAnalyzer::analyze_code(code);
            assert!(
                analysis
                    .detected_patterns
                    .iter()
                    .any(|p| p.reason == "Fetch API call"),
                "global-receiver fetch( must be flagged for code:\n{code}"
            );
        }
    }

    #[test]
    fn test_arbitrary_method_fetch_still_not_flagged() {
        // The allowlist must NOT admit arbitrary `.fetch(` method calls — only
        // the known JS globals. `mail.fetch(` (imaplib), `cursor.fetch(` (DBAPI)
        // and collection `.fetch(` stay excluded.
        for code in [
            r#"mail.fetch(b"1", "(RFC822)")"#,
            r#"cursor.fetchmany(10)"#,
            r#"collection.fetch()"#,
        ] {
            let analysis = RemoteAccessAnalyzer::analyze_code(code);
            let fetch_api_count = analysis
                .detected_patterns
                .iter()
                .filter(|p| p.reason == "Fetch API call")
                .count();
            assert_eq!(
                fetch_api_count, 0,
                "arbitrary .fetch() must not be flagged as Fetch API:\n{code}\n{:?}",
                analysis.detected_patterns
            );
        }
    }

    #[test]
    fn test_httpx_detected() {
        let code = r#"
import httpx

async def fetch():
    async with httpx.AsyncClient() as client:
        return await client.get("https://example.com")
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
    }

    #[test]
    fn test_js_import_detected() {
        let code = r#"
import axios from "axios";
const http = require("http");
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "import" && p.pattern.contains("axios")));
    }

    #[test]
    fn test_rust_import_detected() {
        let code = r#"
use reqwest::Client;
use std::net::TcpStream;
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "import" && p.pattern.contains("reqwest")));
    }

    #[test]
    fn test_go_import_detected() {
        let code = r#"
import (
    "net/http"
)
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "import" && p.pattern.contains("net/http")));
    }

    #[test]
    fn test_import_detector_selection_respects_enabled_languages() {
        let code = r#"
import requests
import axios from "axios";
"#;
        let declaration = autonoetic_types::agent::RemoteAccessDeclaration {
            approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
            targets: vec![autonoetic_types::background::GrantTarget::Any],
            enabled_languages: vec![RemoteAccessLanguage::Python],
            python_imports: vec!["requests".to_string()],
            js_imports: vec!["axios".to_string()],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec![],
            shell_commands: vec![],
            package_manager_commands: vec![],
        };
        let analysis =
            RemoteAccessAnalyzer::analyze_code_with_declaration(code, Some(&declaration));
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "import" && p.pattern.contains("import requests")));
        assert!(
            !analysis
                .detected_patterns
                .iter()
                .any(|p| p.category == "import" && p.pattern.contains("axios")),
            "javascript detector should be disabled by enabled_languages"
        );
    }

    #[test]
    fn test_cross_language_function_calls_detected() {
        let code = r#"
const res = axios.get("https://example.org");
let _ = reqwest::get("https://example.org");
resp, err := http.Get("https://example.org")
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "function_call" && p.pattern.contains("axios.get")));
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "function_call" && p.pattern.contains("reqwest::get")));
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "function_call" && p.pattern.contains("http.Get")));
    }

    #[test]
    fn test_js_call_patterns_not_fired_on_python_source() {
        // Language-scoped patterns (#1020): a Python source with import signals
        // must not fire JS-specific call patterns (axios, Node http, fetch).
        let code = r#"
import requests
res = requests.get("https://example.org")
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        // The Python HTTP call IS flagged (via the requests pattern)...
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "function_call" && p.pattern.contains("requests.get")));
        // ...but the JS-only axios heuristic must NOT fire on Python source.
        assert!(
            !analysis
                .detected_patterns
                .iter()
                .any(|p| p.category == "function_call" && p.pattern.contains("axios")),
            "axios (JS) must not be flagged in Python source: {:?}",
            analysis.detected_patterns
        );
    }

    #[test]
    fn test_python_call_patterns_not_fired_on_js_source() {
        // Python-specific call patterns (urlopen, requests, httpx) must not
        // fire when the source is JS with import signals.
        let code = r#"
import axios from "axios";
const res = axios.get("https://example.org");
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        // The JS call IS flagged (axios is JS-scoped)...
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "function_call" && p.pattern.contains("axios.get")));
        // ...but Python-only `urlopen(` must not fire on JS source.
        assert!(
            !analysis
                .detected_patterns
                .iter()
                .any(|p| p.category == "function_call" && p.pattern.contains("urlopen")),
            "urlopen (Python) must not be flagged in JS source: {:?}",
            analysis.detected_patterns
        );
    }

    #[test]
    fn test_agnostic_patterns_fire_regardless_of_language() {
        // Socket primitives (.connect/.send/.recv/.bind/.listen/.accept) are
        // language-agnostic: they must fire even when the language is known.
        let python_code = r#"
import socket
s = socket.socket()
s.connect(("imap.example.com", 143))
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(python_code);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "function_call" && p.pattern.contains(".connect")));

        let js_code = r#"
const net = require("net");
const s = net.createConnection(80, "example.com");
s.connect(443, "example.com");
"#;
        let js_analysis = RemoteAccessAnalyzer::analyze_code(js_code);
        assert!(js_analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "function_call" && p.pattern.contains(".connect")));
    }

    #[test]
    fn test_enabled_languages_scopes_function_call_patterns() {
        // `enabled_languages` on the declaration is authoritative for call
        // pattern scoping: with Javascript enabled, a Python snippet's
        // Python-tagged call patterns are suppressed, JS-tagged ones still run.
        let declaration = RemoteAccessDeclaration {
            enabled_languages: vec![RemoteAccessLanguage::Javascript],
            ..Default::default()
        };
        let code = r#"
fetch("https://example.org")
urlopen("https://example.org")
"#;
        let analysis =
            RemoteAccessAnalyzer::analyze_code_with_declaration(code, Some(&declaration));
        // JS-tagged `fetch(` still fires under the Javascript declaration...
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.reason == "Fetch API call"));
        // ...but Python-tagged `urlopen(` is suppressed by the same declaration.
        assert!(
            !analysis
                .detected_patterns
                .iter()
                .any(|p| p.category == "function_call" && p.pattern.contains("urlopen")),
            "urlopen (Python) must be suppressed by enabled_languages=[javascript]: {:?}",
            analysis.detected_patterns
        );
    }

    #[test]
    fn test_import_scan_reports_firing_languages() {
        // The import signal used for call-pattern scoping comes out of the same
        // pass that collects the import patterns — `detect_imports` reports which
        // detectors fired so nothing downstream has to re-scan the code.
        let python = RemoteAccessAnalyzer::detect_imports("import requests\n", None);
        assert!(!python.patterns.is_empty());
        assert_eq!(
            python.languages,
            HashSet::from([RemoteAccessLanguage::Python])
        );

        let mixed = RemoteAccessAnalyzer::detect_imports(
            "import requests\nconst axios = require(\"axios\");\n",
            None,
        );
        assert_eq!(
            mixed.languages,
            HashSet::from([
                RemoteAccessLanguage::Python,
                RemoteAccessLanguage::Javascript
            ])
        );

        // No network imports → no language signal at all.
        let inert = RemoteAccessAnalyzer::detect_imports("import json\n", None);
        assert!(inert.patterns.is_empty());
        assert!(inert.languages.is_empty());

        // A detector disabled by `enabled_languages` cannot contribute a signal.
        let scoped = RemoteAccessAnalyzer::detect_imports(
            "import requests\n",
            Some(&HashSet::from([RemoteAccessLanguage::Javascript])),
        );
        assert!(scoped.patterns.is_empty());
        assert!(scoped.languages.is_empty());
    }

    #[test]
    fn test_language_scope_precedence() {
        let declared = HashSet::from([RemoteAccessLanguage::Javascript]);
        let signal = HashSet::from([RemoteAccessLanguage::Python]);

        // 1. Declaration is authoritative — it wins over the import signal.
        assert_eq!(
            language_scope_for_code(Some(&declared), &signal),
            Some(declared.clone())
        );
        // 2. No declaration → the import signal scopes the call patterns.
        assert_eq!(language_scope_for_code(None, &signal), Some(signal.clone()));
        // 3. Neither → unknown language → unrestricted (all tagged patterns run).
        assert_eq!(language_scope_for_code(None, &HashSet::new()), None);
        // An empty declared set is not a scope; fall through to the signal.
        assert_eq!(
            language_scope_for_code(Some(&HashSet::new()), &signal),
            Some(signal)
        );
    }

    // --- Network sink detection tests (#1021) ---

    /// The treadmill fix, demonstrated on a real gap: `http.client` is Python's
    /// *stdlib* HTTP client and appears nowhere in `PythonImportDetector`'s
    /// library list, so this code produced **zero** signals before sink
    /// resolution. Nothing had to be added to a list to catch it — the call
    /// resolves to a sink.
    #[test]
    fn test_stdlib_sink_detected_without_any_import_list_entry() {
        let code = r#"
import http.client
conn = http.client.HTTPSConnection(target_host)
conn.request("GET", "/v1/data")
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(
            analysis
                .detected_patterns
                .iter()
                .any(|p| p.category == "network_sink"
                    && p.pattern == "http.client.HTTPSConnection"),
            "expected the stdlib sink to be detected: {:?}",
            analysis.detected_patterns
        );
    }

    /// An unlisted client library is irrelevant when the sink it bottoms out on
    /// is in the analyzed source — this is the issue's headline case.
    #[test]
    fn test_unlisted_library_detected_through_its_sink() {
        let code = r#"
import socket
# `acme_transport` is on no list in this codebase and never needs to be.
def send(payload, host):
    s = socket.create_connection((host, 9000))
    s.sendall(payload)
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "network_sink" && p.pattern == "socket.create_connection"));
    }

    /// Aliases are followed, and the operator-facing reason names the alias the
    /// sink arrived through.
    #[test]
    fn test_sink_alias_is_resolved_and_reported() {
        let code = "import urllib.request as u\nu.urlopen(dest)\n";
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        let sink = analysis
            .detected_patterns
            .iter()
            .find(|p| p.category == "network_sink")
            .expect("sink detected");
        assert_eq!(sink.pattern, "urllib.request.urlopen");
        assert!(
            sink.reason.contains("via `u.urlopen`"),
            "reason should name the alias: {}",
            sink.reason
        );
    }

    /// Sinks strengthen **detection** without widening what an agent must
    /// enumerate: `network_sink` is not gated by
    /// `undeclared_patterns_against_manifest`. Narrowing/expanding the
    /// declaration contract is #1023's decision, not #1021's — so a declaration
    /// that covers the agent's hosts does not also have to list every sink.
    #[test]
    fn test_network_sink_does_not_add_declaration_burden() {
        let code = "import http.client\nhttp.client.HTTPSConnection(h)\n";
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        let sinks: Vec<&DetectedPattern> = analysis
            .detected_patterns
            .iter()
            .filter(|p| p.category == "network_sink")
            .collect();
        assert!(!sinks.is_empty(), "precondition: a sink was detected");

        // A declaration that names no function calls at all.
        let declaration = RemoteAccessDeclaration {
            targets: vec![],
            ..Default::default()
        };
        let undeclared =
            undeclared_patterns_against_manifest(&analysis.detected_patterns, Some(&declaration));
        assert!(
            !undeclared.iter().any(|p| p.category == "network_sink"),
            "sinks must not fail shut as undeclared patterns: {undeclared:?}"
        );
    }

    /// Sink scoping honours `enabled_languages`, consistently with the #1020
    /// call-pattern scoping.
    #[test]
    fn test_enabled_languages_scopes_sink_detection() {
        let python_code = "import http.client\nhttp.client.HTTPSConnection(h)\n";
        let js_only = RemoteAccessDeclaration {
            enabled_languages: vec![RemoteAccessLanguage::Javascript],
            ..Default::default()
        };
        let analysis =
            RemoteAccessAnalyzer::analyze_code_with_declaration(python_code, Some(&js_only));
        assert!(
            !analysis
                .detected_patterns
                .iter()
                .any(|p| p.category == "network_sink"),
            "Python sinks must not fire when only Javascript is enabled: {:?}",
            analysis.detected_patterns
        );

        let py_only = RemoteAccessDeclaration {
            enabled_languages: vec![RemoteAccessLanguage::Python],
            ..Default::default()
        };
        let analysis =
            RemoteAccessAnalyzer::analyze_code_with_declaration(python_code, Some(&py_only));
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "network_sink"));
    }

    /// A `node:`-prefixed Node built-in import matches none of the JS import
    /// regexes (they alternate on bare specifiers), so this was another
    /// zero-signal case that sink resolution closes.
    #[test]
    fn test_node_prefixed_builtin_import_detected_through_sink() {
        let code = "import { request } from \"node:https\";\nrequest(opts);\n";
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(
            analysis
                .detected_patterns
                .iter()
                .any(|p| p.category == "network_sink" && p.pattern == "https.request"),
            "expected node:https sink: {:?}",
            analysis.detected_patterns
        );
    }

    /// Inert code stays inert — sink resolution must not manufacture signals,
    /// since every new signal can turn a silent exec into an approval gate.
    #[test]
    fn test_sinks_add_no_false_positives_to_inert_code() {
        let code = r#"
import json
import math
data = {}
data.get("http")
print(json.dumps({"a": math.sqrt(4)}))
"#;
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(
            !analysis
                .detected_patterns
                .iter()
                .any(|p| p.category == "network_sink"),
            "no sinks expected: {:?}",
            analysis.detected_patterns
        );
    }

    // --- Network command detection tests ---

    #[test]
    fn test_pip_install_detected() {
        let code = "cd /tmp && pip install requests pydantic && python3 app.py";
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "network_command" && p.pattern.contains("pip install")));
    }

    #[test]
    fn test_npm_install_detected() {
        let code = "cd /tmp && npm install express && node server.js";
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "network_command" && p.pattern.contains("npm install")));
    }

    #[test]
    fn test_curl_detected() {
        let code = "curl https://api.example.com/data";
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "network_command" && p.pattern.contains("curl")));
    }

    #[test]
    fn test_wget_detected() {
        let code = "wget https://example.com/file.tar.gz";
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "network_command" && p.pattern.contains("wget")));
    }

    #[test]
    fn test_git_clone_detected() {
        let code = "git clone https://github.com/example/repo.git /tmp/repo";
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "network_command" && p.pattern.contains("git clone")));
    }

    #[test]
    fn test_apt_get_install_detected() {
        let code = "apt-get update && apt-get install -y build-essential";
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "network_command" && p.pattern.contains("apt-get")));
    }

    // --- Dependency-aware analysis tests ---

    #[test]
    fn test_dependencies_imply_network() {
        let code = "print('hello')";
        let packages = vec!["requests".to_string(), "pydantic".to_string()];
        let analysis =
            RemoteAccessAnalyzer::analyze_command_and_dependencies(code, Some(&packages));
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "dependency_install"));
    }

    #[test]
    fn test_dependencies_empty_no_flag() {
        let code = "print('hello')";
        let packages: Vec<String> = vec![];
        let analysis =
            RemoteAccessAnalyzer::analyze_command_and_dependencies(code, Some(&packages));
        assert!(!analysis.requires_approval);
    }

    #[test]
    fn test_dependencies_none_no_flag() {
        let code = "print('hello')";
        let analysis = RemoteAccessAnalyzer::analyze_command_and_dependencies(code, None);
        assert!(!analysis.requires_approval);
    }

    #[test]
    fn test_dependencies_combine_with_code_patterns() {
        let code = r#"
import socket
s = socket.socket()
"#;
        let packages = vec!["requests".to_string()];
        let analysis =
            RemoteAccessAnalyzer::analyze_command_and_dependencies(code, Some(&packages));
        assert!(analysis.requires_approval);
        // Should have both import and dependency_install
        assert!(analysis.detected_patterns.len() >= 2);
    }

    // --- Transitive workspace analysis tests ---

    #[test]
    fn test_transitive_import_detected() {
        let main_code = r#"
import mymod
mymod.do_thing()
"#;
        let mymod_content = r#"
import requests
def do_thing():
    return requests.get("https://example.com")
"#;
        let workspace = vec![
            ("mymod.py".to_string(), mymod_content.to_string()),
            ("other.py".to_string(), "import os".to_string()),
        ];
        let analysis = RemoteAccessAnalyzer::analyze_code_with_workspace(main_code, &workspace);
        assert!(analysis.requires_approval);
        assert!(analysis
            .detected_patterns
            .iter()
            .any(|p| p.category == "import" && p.pattern.contains("requests")));
    }

    #[test]
    fn test_transitive_no_match() {
        let main_code = r#"
import nonexistent
print("hello")
"#;
        let workspace = vec![("other.py".to_string(), "import os".to_string())];
        let analysis = RemoteAccessAnalyzer::analyze_code_with_workspace(main_code, &workspace);
        assert!(!analysis.requires_approval);
    }

    #[test]
    fn test_transitive_empty_workspace() {
        let main_code = r#"
import mymod
mymod.do_thing()
"#;
        let workspace: Vec<(String, String)> = vec![];
        let analysis = RemoteAccessAnalyzer::analyze_code_with_workspace(main_code, &workspace);
        assert!(!analysis.requires_approval);
    }

    #[test]
    fn test_safe_inspection_commands() {
        assert!(is_safe_inspection_command("pip list"));
        assert!(is_safe_inspection_command("pip show requests"));
        assert!(is_safe_inspection_command("pip --version"));
        assert!(is_safe_inspection_command("pip3 list"));
        assert!(is_safe_inspection_command("npm list"));
        assert!(is_safe_inspection_command("npm version"));
        assert!(!is_safe_inspection_command("pip install requests"));
        assert!(!is_safe_inspection_command("npm install express"));
        assert!(!is_safe_inspection_command("curl http://example.com"));
    }

    #[test]
    fn test_classify_coverage_none() {
        let patterns: Vec<DetectedPattern> = vec![];
        let coverage = classify_network_coverage(&patterns, vec![]);
        assert_eq!(coverage, NetworkCoverage::None);
    }

    #[test]
    fn test_classify_coverage_unresolved_import_only() {
        let patterns = vec![DetectedPattern {
            category: "import".to_string(),
            pattern: "import requests".to_string(),
            line_number: Some(1),
            reason: "HTTP client".to_string(),
        }];
        let coverage = classify_network_coverage(&patterns, vec![]);
        assert_eq!(coverage, NetworkCoverage::Unresolved);
    }

    #[test]
    fn test_classify_coverage_concrete_url_only() {
        let patterns = vec![DetectedPattern {
            category: "url_literal".to_string(),
            pattern: "https://wttr.in/Paris".to_string(),
            line_number: Some(1),
            reason: "URL literal".to_string(),
        }];
        let coverage = classify_network_coverage(&patterns, vec!["wttr.in".to_string()]);
        assert_eq!(
            coverage,
            NetworkCoverage::Concrete {
                targets: vec!["wttr.in".to_string()]
            }
        );
    }

    #[test]
    fn test_classify_coverage_concrete_with_import_and_function_call() {
        let patterns = vec![
            DetectedPattern {
                category: "import".to_string(),
                pattern: "import requests".to_string(),
                line_number: Some(1),
                reason: "HTTP client".to_string(),
            },
            DetectedPattern {
                category: "function_call".to_string(),
                pattern: "requests.get(".to_string(),
                line_number: Some(2),
                reason: "HTTP GET".to_string(),
            },
            DetectedPattern {
                category: "url_literal".to_string(),
                pattern: "https://wttr.in/Paris".to_string(),
                line_number: Some(2),
                reason: "URL literal".to_string(),
            },
        ];
        let coverage = classify_network_coverage(&patterns, vec!["wttr.in".to_string()]);
        assert_eq!(
            coverage,
            NetworkCoverage::Concrete {
                targets: vec!["wttr.in".to_string()]
            }
        );
    }

    #[test]
    fn test_classify_coverage_unresolved_with_dependency_install() {
        let patterns = vec![
            DetectedPattern {
                category: "import".to_string(),
                pattern: "import requests".to_string(),
                line_number: Some(1),
                reason: "HTTP client".to_string(),
            },
            DetectedPattern {
                category: "url_literal".to_string(),
                pattern: "https://wttr.in/Paris".to_string(),
                line_number: Some(2),
                reason: "URL literal".to_string(),
            },
            DetectedPattern {
                category: "dependency_install".to_string(),
                pattern: "packages: [requests]".to_string(),
                line_number: None,
                reason: "Package installation requires network".to_string(),
            },
        ];
        let coverage = classify_network_coverage(&patterns, vec!["wttr.in".to_string()]);
        assert_eq!(coverage, NetworkCoverage::Unresolved);
    }

    #[test]
    fn test_classify_coverage_concrete_with_network_command() {
        let patterns = vec![
            DetectedPattern {
                category: "network_command".to_string(),
                pattern: "curl".to_string(),
                line_number: Some(1),
                reason: "curl makes network requests".to_string(),
            },
            DetectedPattern {
                category: "url_literal".to_string(),
                pattern: "https://api.example.com/data".to_string(),
                line_number: Some(1),
                reason: "URL literal".to_string(),
            },
        ];
        let coverage = classify_network_coverage(&patterns, vec!["api.example.com".to_string()]);
        assert_eq!(
            coverage,
            NetworkCoverage::Concrete {
                targets: vec!["api.example.com".to_string()]
            }
        );
    }

    #[test]
    fn test_classify_coverage_unresolved_function_call_no_url() {
        let patterns = vec![
            DetectedPattern {
                category: "import".to_string(),
                pattern: "import requests".to_string(),
                line_number: Some(1),
                reason: "HTTP client".to_string(),
            },
            DetectedPattern {
                category: "function_call".to_string(),
                pattern: "requests.get(".to_string(),
                line_number: Some(2),
                reason: "HTTP GET".to_string(),
            },
        ];
        let coverage = classify_network_coverage(&patterns, vec![]);
        assert_eq!(coverage, NetworkCoverage::Unresolved);
    }

    #[test]
    fn test_approval_remote_suffix_prefers_hosts() {
        let patterns = vec![DetectedPattern {
            category: "function_call".to_string(),
            pattern: "urlopen(".to_string(),
            line_number: Some(1),
            reason: "open URL".to_string(),
        }];
        let s = approval_remote_operator_suffix(&["example.com".to_string()], &patterns);
        assert_eq!(s, " → hosts: example.com");
    }

    #[test]
    fn test_approval_remote_suffix_signals_when_no_hosts() {
        let patterns = vec![
            DetectedPattern {
                category: "function_call".to_string(),
                pattern: "urlopen(".to_string(),
                line_number: Some(1),
                reason: "open URL".to_string(),
            },
            DetectedPattern {
                category: "import".to_string(),
                pattern: "from urllib.request import urlopen".to_string(),
                line_number: Some(1),
                reason: "import".to_string(),
            },
        ];
        let s = approval_remote_operator_suffix(&[], &patterns);
        assert!(s.starts_with(" → signals: "));
        assert!(s.contains("function_call:urlopen("));
        assert!(s.contains("import:from urllib.request import urlopen"));
    }

    #[test]
    fn test_approval_remote_suffix_empty_without_patterns_or_hosts() {
        assert!(approval_remote_operator_suffix(&[], &[]).is_empty());
    }

    #[test]
    fn test_approval_remote_suffix_category_fallback_when_pattern_empty() {
        let patterns = vec![DetectedPattern {
            category: "import".to_string(),
            pattern: "\n\t".to_string(),
            line_number: Some(1),
            reason: "x".to_string(),
        }];
        assert_eq!(
            approval_remote_operator_suffix(&[], &patterns),
            " → signals: import:*"
        );
    }

    #[test]
    fn test_undeclared_patterns_against_manifest_no_declaration_is_noop() {
        let patterns = vec![DetectedPattern {
            category: "network_command".to_string(),
            pattern: "curl".to_string(),
            line_number: Some(1),
            reason: "curl".to_string(),
        }];
        let undeclared = undeclared_patterns_against_manifest(&patterns, None);
        assert!(undeclared.is_empty());
    }

    #[test]
    fn test_undeclared_patterns_against_manifest_declared_patterns_pass() {
        let patterns = vec![
            DetectedPattern {
                category: "import".to_string(),
                pattern: "import requests".to_string(),
                line_number: Some(1),
                reason: "import".to_string(),
            },
            DetectedPattern {
                category: "function_call".to_string(),
                pattern: "requests.get(".to_string(),
                line_number: Some(2),
                reason: "call".to_string(),
            },
            DetectedPattern {
                category: "network_command".to_string(),
                pattern: "curl".to_string(),
                line_number: Some(3),
                reason: "cmd".to_string(),
            },
            DetectedPattern {
                category: "network_command".to_string(),
                pattern: "pip install".to_string(),
                line_number: Some(4),
                reason: "pkg".to_string(),
            },
        ];
        let declaration = autonoetic_types::agent::RemoteAccessDeclaration {
            approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
            targets: vec![autonoetic_types::background::GrantTarget::Any],
            enabled_languages: vec![],
            python_imports: vec!["requests".to_string()],
            js_imports: vec![],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec!["requests.get".to_string()],
            shell_commands: vec!["curl".to_string()],
            package_manager_commands: vec!["pip install".to_string()],
        };
        let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&declaration));
        assert!(undeclared.is_empty(), "expected no undeclared patterns");
    }

    #[test]
    fn test_undeclared_patterns_against_manifest_missing_command_fails_shut() {
        let patterns = vec![DetectedPattern {
            category: "network_command".to_string(),
            pattern: "wget".to_string(),
            line_number: Some(1),
            reason: "cmd".to_string(),
        }];
        let declaration = autonoetic_types::agent::RemoteAccessDeclaration {
            approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
            targets: vec![autonoetic_types::background::GrantTarget::Any],
            enabled_languages: vec![],
            python_imports: vec![],
            js_imports: vec![],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec![],
            shell_commands: vec!["curl".to_string()],
            package_manager_commands: vec![],
        };
        let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&declaration));
        assert_eq!(undeclared.len(), 1);
        assert_eq!(undeclared[0].pattern, "wget");
    }

    #[test]
    fn test_undeclared_patterns_against_manifest_accepts_non_python_import_fields() {
        let patterns = vec![
            DetectedPattern {
                category: "import".to_string(),
                pattern: r#"import axios from "axios""#.to_string(),
                line_number: Some(1),
                reason: "js import".to_string(),
            },
            DetectedPattern {
                category: "import".to_string(),
                pattern: "use reqwest::Client;".to_string(),
                line_number: Some(2),
                reason: "rust import".to_string(),
            },
            DetectedPattern {
                category: "import".to_string(),
                pattern: r#""net/http""#.to_string(),
                line_number: Some(3),
                reason: "go import".to_string(),
            },
        ];
        let declaration = autonoetic_types::agent::RemoteAccessDeclaration {
            approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
            targets: vec![autonoetic_types::background::GrantTarget::Any],
            enabled_languages: vec![
                RemoteAccessLanguage::Javascript,
                RemoteAccessLanguage::Rust,
                RemoteAccessLanguage::Go,
            ],
            python_imports: vec![],
            js_imports: vec!["axios".to_string()],
            rust_imports: vec!["reqwest".to_string()],
            go_imports: vec!["net/http".to_string()],
            function_calls: vec![],
            shell_commands: vec![],
            package_manager_commands: vec![],
        };
        let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&declaration));
        assert!(
            undeclared.is_empty(),
            "expected non-python import declarations to match"
        );

        // Imports no longer gate (#1023), so the assertion above would hold even
        // if the per-language fields were ignored. Assert the matching itself via
        // the advisory view, which is where import coverage is still computed.
        assert!(
            advisory_undeclared_patterns(&patterns, Some(&declaration)).is_empty(),
            "js/rust/go import fields must still be honoured when computing coverage"
        );
        let unmatched = advisory_undeclared_patterns(
            &patterns,
            Some(&autonoetic_types::agent::RemoteAccessDeclaration {
                targets: vec![autonoetic_types::background::GrantTarget::Any],
                ..Default::default()
            }),
        );
        assert_eq!(
            unmatched.len(),
            3,
            "with no import fields declared, all three imports are advisory drift"
        );
    }

    #[test]
    fn test_undeclared_patterns_against_manifest_url_target_allowed() {
        let patterns = vec![DetectedPattern {
            category: "url_literal".to_string(),
            pattern: "https://api.example.com/v1/items".to_string(),
            line_number: Some(1),
            reason: "url".to_string(),
        }];
        let declaration = autonoetic_types::agent::RemoteAccessDeclaration {
            approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
            targets: vec![autonoetic_types::background::GrantTarget::ExactHost(
                "api.example.com".to_string(),
            )],
            enabled_languages: vec![],
            python_imports: vec![],
            js_imports: vec![],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec![],
            shell_commands: vec!["curl".to_string()],
            package_manager_commands: vec![],
        };
        let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&declaration));
        assert!(undeclared.is_empty(), "expected URL host to be allowlisted");
    }

    #[test]
    fn test_undeclared_patterns_against_manifest_url_target_not_allowed() {
        let patterns = vec![DetectedPattern {
            category: "url_literal".to_string(),
            pattern: "https://api.example.com/v1/items".to_string(),
            line_number: Some(1),
            reason: "url".to_string(),
        }];
        let declaration = autonoetic_types::agent::RemoteAccessDeclaration {
            approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
            targets: vec![autonoetic_types::background::GrantTarget::ExactHost(
                "api.other.com".to_string(),
            )],
            enabled_languages: vec![],
            python_imports: vec![],
            js_imports: vec![],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec![],
            shell_commands: vec!["curl".to_string()],
            package_manager_commands: vec![],
        };
        let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&declaration));
        assert_eq!(
            undeclared.len(),
            1,
            "expected undeclared URL target to fail shut"
        );
        assert_eq!(undeclared[0].category, "url_literal");
    }

    #[test]
    fn test_undeclared_patterns_against_manifest_url_target_wildcard_suffix_allowed() {
        let patterns = vec![DetectedPattern {
            category: "url_literal".to_string(),
            pattern: "https://api.example.com/v1/items".to_string(),
            line_number: Some(1),
            reason: "url".to_string(),
        }];
        let declaration = autonoetic_types::agent::RemoteAccessDeclaration {
            approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
            targets: vec![autonoetic_types::background::GrantTarget::HostSuffix(
                "*.example.com".to_string(),
            )],
            enabled_languages: vec![],
            python_imports: vec![],
            js_imports: vec![],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec![],
            shell_commands: vec!["curl".to_string()],
            package_manager_commands: vec![],
        };
        let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&declaration));
        assert!(
            undeclared.is_empty(),
            "expected wildcard suffix to allow target"
        );
    }

    #[test]
    fn test_undeclared_patterns_against_manifest_ip_target_global_wildcard_allowed() {
        let patterns = vec![DetectedPattern {
            category: "ip_address".to_string(),
            pattern: "10.0.0.10".to_string(),
            line_number: Some(1),
            reason: "ip".to_string(),
        }];
        let declaration = autonoetic_types::agent::RemoteAccessDeclaration {
            approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
            targets: vec![autonoetic_types::background::GrantTarget::Any],
            enabled_languages: vec![],
            python_imports: vec![],
            js_imports: vec![],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec![],
            shell_commands: vec![],
            package_manager_commands: vec![],
        };
        let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&declaration));
        assert!(
            undeclared.is_empty(),
            "expected wildcard allowlist to allow IP target"
        );
    }

    // --- targets are the durable contract; import/call lists are advisory (#1023) ---

    fn decl_with_targets(
        targets: Vec<autonoetic_types::background::GrantTarget>,
    ) -> RemoteAccessDeclaration {
        RemoteAccessDeclaration {
            targets,
            ..Default::default()
        }
    }

    /// The behaviour change: an import the declaration does not name no longer
    /// fails the exec shut. Before #1023 this returned the pattern and
    /// `sandbox_exec` refused with `undeclared_remote_pattern`.
    #[test]
    fn test_undeclared_import_is_advisory_not_gating() {
        let patterns = vec![DetectedPattern {
            category: "import".to_string(),
            pattern: "import imaplib".to_string(),
            line_number: Some(1),
            reason: "IMAP client library".to_string(),
        }];
        let declaration =
            decl_with_targets(vec![autonoetic_types::background::GrantTarget::ExactHost(
                "imap.example.com".to_string(),
            )]);

        assert!(
            undeclared_patterns_against_manifest(&patterns, Some(&declaration)).is_empty(),
            "an undeclared import must not fail shut"
        );
        // …but the drift is still observable.
        let advisory = advisory_undeclared_patterns(&patterns, Some(&declaration));
        assert_eq!(advisory.len(), 1);
        assert_eq!(advisory[0].pattern, "import imaplib");
    }

    /// Same for function calls — the category agents could least reliably mirror.
    #[test]
    fn test_undeclared_function_call_is_advisory_not_gating() {
        let patterns = vec![DetectedPattern {
            category: "function_call".to_string(),
            pattern: ".connect(".to_string(),
            line_number: Some(4),
            reason: "Socket connection initiation".to_string(),
        }];
        let declaration = decl_with_targets(vec![autonoetic_types::background::GrantTarget::Any]);

        assert!(undeclared_patterns_against_manifest(&patterns, Some(&declaration)).is_empty());
        assert_eq!(
            advisory_undeclared_patterns(&patterns, Some(&declaration)).len(),
            1
        );
    }

    /// The session-912c7791 failure, reproduced: the coder declared
    /// `function_calls: ["imaplib.fetch("]` while the analyzer detects the bare
    /// `fetch(`, so the declaration could never match and the exec failed shut —
    /// ~30 turns of guessing. With hosts declared and calls advisory, this now
    /// proceeds to the approval gate instead of refusing.
    #[test]
    fn test_session_912c7791_unmatchable_function_call_declaration_no_longer_fails_shut() {
        let patterns = vec![
            DetectedPattern {
                category: "import".to_string(),
                pattern: "import imaplib".to_string(),
                line_number: Some(1),
                reason: "IMAP client library".to_string(),
            },
            DetectedPattern {
                category: "function_call".to_string(),
                pattern: "fetch(".to_string(),
                line_number: Some(9),
                reason: "Fetch API call".to_string(),
            },
        ];
        let declaration = RemoteAccessDeclaration {
            targets: vec![autonoetic_types::background::GrantTarget::HostAndPort {
                host: "imap.gmail.com".to_string(),
                port: 993,
            }],
            python_imports: vec!["imaplib".to_string()],
            // The unmatchable entry the agent actually wrote.
            function_calls: vec!["imaplib.fetch(".to_string()],
            ..Default::default()
        };

        assert!(
            undeclared_patterns_against_manifest(&patterns, Some(&declaration)).is_empty(),
            "a well-declared host must not be blocked by an unmatchable call pattern"
        );
        // The import matched; only the call pattern is drift.
        let advisory = advisory_undeclared_patterns(&patterns, Some(&declaration));
        assert_eq!(advisory.len(), 1);
        assert_eq!(advisory[0].category, "function_call");
    }

    /// `targets` keeps its authority: a concrete host outside the declaration still
    /// fails shut. Demoting the import lists must not weaken this.
    #[test]
    fn test_targets_remain_authoritative_for_concrete_hosts() {
        let patterns = vec![
            DetectedPattern {
                category: "url_literal".to_string(),
                pattern: "https://evil.example.net/exfil".to_string(),
                line_number: Some(2),
                reason: "url".to_string(),
            },
            DetectedPattern {
                category: "ip_address".to_string(),
                pattern: "203.0.113.9".to_string(),
                line_number: Some(3),
                reason: "ip".to_string(),
            },
        ];
        let declaration =
            decl_with_targets(vec![autonoetic_types::background::GrantTarget::ExactHost(
                "api.example.com".to_string(),
            )]);

        let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&declaration));
        assert_eq!(undeclared.len(), 2, "{undeclared:?}");
    }

    /// Shell/package-manager surfaces stay gating — they name an execution surface
    /// rather than analyzer internals, and `dependency_install` is what routes work
    /// to `packager.default`.
    #[test]
    fn test_shell_and_dependency_surfaces_remain_gating() {
        let wget = vec![DetectedPattern {
            category: "network_command".to_string(),
            pattern: "wget".to_string(),
            line_number: Some(1),
            reason: "command".to_string(),
        }];
        let declaration = RemoteAccessDeclaration {
            targets: vec![autonoetic_types::background::GrantTarget::Any],
            shell_commands: vec!["curl".to_string()],
            ..Default::default()
        };
        assert_eq!(
            undeclared_patterns_against_manifest(&wget, Some(&declaration)).len(),
            1
        );

        let dep = vec![DetectedPattern {
            category: "dependency_install".to_string(),
            pattern: "requests".to_string(),
            line_number: None,
            reason: "dependency".to_string(),
        }];
        assert_eq!(
            undeclared_patterns_against_manifest(&dep, Some(&declaration)).len(),
            1,
            "no declared package_manager_commands ⇒ dependency install still gated"
        );
    }

    /// Sinks are derived by the gateway, so they are neither gating nor reported as
    /// declaration drift — an agent cannot be expected to declare them.
    #[test]
    fn test_network_sink_is_neither_gating_nor_advisory_drift() {
        let patterns = vec![DetectedPattern {
            category: "network_sink".to_string(),
            pattern: "http.client.HTTPSConnection".to_string(),
            line_number: Some(3),
            reason: "HTTPS client connection".to_string(),
        }];
        let declaration = decl_with_targets(vec![autonoetic_types::background::GrantTarget::Any]);

        assert!(undeclared_patterns_against_manifest(&patterns, Some(&declaration)).is_empty());
        assert!(
            advisory_undeclared_patterns(&patterns, Some(&declaration)).is_empty(),
            "sinks must not be reported as something the agent failed to declare"
        );
    }

    /// No declaration at all ⇒ both views are empty; the
    /// `missing_remote_access_declaration` check upstream owns that case.
    #[test]
    fn test_advisory_view_is_empty_without_a_declaration() {
        let patterns = vec![DetectedPattern {
            category: "import".to_string(),
            pattern: "import requests".to_string(),
            line_number: Some(1),
            reason: "import".to_string(),
        }];
        assert!(advisory_undeclared_patterns(&patterns, None).is_empty());
        assert!(undeclared_patterns_against_manifest(&patterns, None).is_empty());
    }
}

/// Commands that inspect the local package environment without network access.
/// These are always safe to run without approval — they read local state only.
const SAFE_INSPECTION_COMMANDS: &[&str] = &[
    "pip list",
    "pip show ",
    "pip --version",
    "pip3 list",
    "pip3 show ",
    "pip3 --version",
    "npm list",
    "npm version",
    "npm --version",
];

/// Returns true if the command is a safe local inspection that does not
/// require network access. These commands only read the local package index.
pub fn is_safe_inspection_command(command: &str) -> bool {
    let lower = command.trim().to_ascii_lowercase();
    SAFE_INSPECTION_COMMANDS
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}
