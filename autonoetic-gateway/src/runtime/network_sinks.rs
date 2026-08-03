//! Structural network-sink detection for Python and JavaScript (#1021).
//!
//! # Why this exists
//!
//! [`super::remote_access`]'s import detectors are hand-maintained lists of
//! *library names*: `requests`, `httpx`, `imaplib`, `boto3`, `psycopg`… Every
//! new or newly-popular client is an "add a row" event (#1019 added
//! `imaplib`/`poplib`/`nntplib`/`telnetlib` because they were missing), and the
//! list will always lag reality — grpc, kafka, elasticsearch, ldap3, asyncpg,
//! snowflake, `psycopg2` vs `psycopg`, generated clients, …
//!
//! The enumeration problem is avoidable because the network surface is **closed
//! at the sink layer**. Every third-party client eventually calls into a small,
//! stable set of platform primitives:
//!
//! * Python — `socket`, `ssl`, `http.client`, `urllib.request`, `ftplib`,
//!   `imaplib`, `poplib`, `smtplib`, `nntplib`, `telnetlib`, asyncio streams,
//!   `xmlrpc.client`, `multiprocessing.connection`.
//! * Node — `net`, `tls`, `http`, `https`, `http2`, `dgram`, `dns`.
//!
//! Resolve a call to one of those and the originating library's name stops
//! mattering. So this module detects *sinks reached through the code's own
//! bindings* rather than *libraries named in imports*.
//!
//! # How it resolves
//!
//! Comments and string bodies are blanked first
//! ([`mask_strings_and_comments`]) so sink-shaped *text* cannot read as a sink
//! *call*, then two passes over the source:
//!
//! 1. **Collect bindings** introduced by imports — `import urllib.request as u`
//!    binds `u → urllib.request`; `from http.client import HTTPConnection as HC`
//!    binds `HC → http.client.HTTPConnection`; `const {connect} =
//!    require("net")` binds `connect → net.connect`.
//! 2. **Resolve call heads** against those bindings, then match the canonical
//!    dotted path against the language's sink table.
//!
//! The two passes read *differently masked* source: bindings need string bodies
//! intact, because a JavaScript module specifier lives inside the quotes
//! (`require("net")`), while calls must not see string bodies at all.
//!
//! A call whose head is *not* bound by an import resolves to nothing and is not
//! a sink. That is the precision property that name matching lacks: `mail.fetch(`
//! (imaplib) and `dict.get("http://…")` cannot be mistaken for sinks, because
//! neither `mail` nor `dict` is bound to a network module.
//!
//! # Why not a real parser
//!
//! A `python3 -c` subprocess would give exact Python fidelity (~15ms/analysis),
//! but it cannot generalise: JavaScript would need `node` on the host, Go a Go
//! toolchain, Rust `rustc`. Since Python **and** JavaScript are the two
//! languages this runtime actually executes ([`crate::exec_request::CodeLanguage`]),
//! a resolver written in Rust is the shape that covers the executable surface —
//! and it returns the same answer on every host, which a
//! host-toolchain-dependent tier does not.
//!
//! # Known limits
//!
//! Deliberately not a parse. These escape resolution:
//!
//! * value aliasing — `s = socket.socket; s()`
//! * dynamic import — `__import__(name)`, `require(expr)`, `importlib`
//! * indirection — `getattr(mod, "urlopen")()`
//! * a sink reached only *inside* a third-party package's own code (nothing
//!   in-file to resolve; workspace-local modules are covered one hop by
//!   `analyze_code_with_workspace`)
//! * the Node built-in globals `fetch`/`WebSocket`/`XMLHttpRequest`, which need
//!   no import and so have no binding to anchor on — those stay name-based, on
//!   top of #1020's language scoping and #1019's `fetch(` byte-boundary guard.
//! * JavaScript regex literals are not masked (they cannot be told from division
//!   without parsing), so `/net\.connect\(/` after `require("net")` is the
//!   remaining known way to produce a spurious sink. That direction is a false
//!   positive, not a missed detection.
//!
//! None of these are new gaps; they are the gaps a regex table has too. The
//! point is that the *library enumeration* treadmill is gone: adding a client
//! library no longer requires a code change.
//!
//! # Languages
//!
//! Python and JavaScript only. **Go** would work well — its sink set is equally
//! closed (`net`, `net/http`, `crypto/tls`) and its import signal is the
//! strongest of any language here, since Go makes an unused import a compile
//! error — but Go is not executable in this runtime, so it is deferred. **Rust**
//! genuinely does not fit: there is no stdlib HTTP, and `tokio::net`/`reqwest`/
//! `hyper` reach the network without touching `std::net`, so no closed stdlib
//! sink set exists. For Rust, the declared `Cargo.toml` dependency set is a
//! better signal than parsing source.

use std::collections::HashMap;

use regex::Regex;

/// A network sink call resolved through the code's own import bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedSink {
    /// Canonical sink path the call resolved to, e.g. `urllib.request.urlopen`.
    pub sink: String,
    /// The call head as written in the source, e.g. `u.urlopen`.
    pub matched: String,
    /// 1-indexed line of the call.
    pub line: usize,
    /// Why this is a network sink (operator-facing).
    pub reason: String,
}

/// Python stdlib network sinks. Closed set: every third-party Python client
/// bottoms out here.
///
/// Entries are calls whose *purpose* is a connection — clients, listeners, DNS.
/// Pure local object construction with no network purpose is deliberately
/// excluded (`urllib.request.Request` only builds a request object,
/// `ssl.create_default_context` only builds a context, `socket.socketpair` is
/// process-local), because every entry here can turn a previously-silent exec
/// into an approval gate, and under a session taint that excludes
/// `Sink::Network` an unresolved signal is a hard refuse. Client *constructors*
/// like `http.client.HTTPConnection` are kept even though the connection opens
/// on first use: their purpose is unambiguous.
const PYTHON_SINKS: &[(&str, &str)] = &[
    ("socket.socket", "Raw socket creation"),
    ("socket.create_connection", "Outbound TCP connection"),
    ("socket.create_server", "Listening TCP socket"),
    ("socket.getaddrinfo", "DNS resolution"),
    ("socket.gethostbyname", "DNS resolution"),
    ("ssl.wrap_socket", "TLS socket wrap"),
    ("http.client.HTTPConnection", "HTTP client connection"),
    ("http.client.HTTPSConnection", "HTTPS client connection"),
    ("urllib.request.urlopen", "Opening a URL connection"),
    ("urllib.request.urlretrieve", "Downloading a URL to disk"),
    ("urllib.request.build_opener", "Building a URL opener"),
    ("ftplib.FTP", "FTP client connection"),
    ("ftplib.FTP_TLS", "FTP-over-TLS client connection"),
    ("imaplib.IMAP4", "IMAP client connection"),
    ("imaplib.IMAP4_SSL", "IMAP-over-TLS client connection"),
    ("imaplib.IMAP4_stream", "IMAP stream connection"),
    ("poplib.POP3", "POP3 client connection"),
    ("poplib.POP3_SSL", "POP3-over-TLS client connection"),
    ("smtplib.SMTP", "SMTP client connection"),
    ("smtplib.SMTP_SSL", "SMTP-over-TLS client connection"),
    ("smtplib.LMTP", "LMTP client connection"),
    ("nntplib.NNTP", "NNTP client connection"),
    ("nntplib.NNTP_SSL", "NNTP-over-TLS client connection"),
    ("telnetlib.Telnet", "Telnet client connection"),
    ("asyncio.open_connection", "Async outbound TCP connection"),
    ("asyncio.start_server", "Async listening TCP server"),
    (
        "asyncio.open_unix_connection",
        "Async Unix socket connection",
    ),
    (
        "asyncio.streams.open_connection",
        "Async outbound TCP connection",
    ),
    ("xmlrpc.client.ServerProxy", "XML-RPC client connection"),
    (
        "multiprocessing.connection.Client",
        "multiprocessing client connection",
    ),
    (
        "multiprocessing.connection.Listener",
        "multiprocessing listener socket",
    ),
];

/// Node built-in network sinks. Every npm HTTP client (axios, got, node-fetch,
/// undici, superagent, ws) bottoms out here.
const JAVASCRIPT_SINKS: &[(&str, &str)] = &[
    ("net.connect", "Outbound TCP connection"),
    ("net.createConnection", "Outbound TCP connection"),
    ("net.createServer", "Listening TCP server"),
    ("net.Socket", "Raw TCP socket"),
    ("tls.connect", "Outbound TLS connection"),
    ("tls.createServer", "Listening TLS server"),
    ("http.request", "HTTP client request"),
    ("http.get", "HTTP GET request"),
    ("http.createServer", "Listening HTTP server"),
    ("https.request", "HTTPS client request"),
    ("https.get", "HTTPS GET request"),
    ("https.createServer", "Listening HTTPS server"),
    ("http2.connect", "HTTP/2 client session"),
    ("http2.createServer", "Listening HTTP/2 server"),
    ("dgram.createSocket", "UDP socket creation"),
    ("dns.lookup", "DNS resolution"),
    ("dns.resolve", "DNS resolution"),
    ("dns.promises.lookup", "DNS resolution"),
    ("dns.promises.resolve", "DNS resolution"),
];

/// Python: names each import statement brings into scope, mapped to the dotted
/// path they denote.
///
/// * `import socket` → `socket → socket`
/// * `import urllib.request` → `urllib → urllib` (the top package is what binds,
///   so `urllib.request.urlopen(...)` resolves by concatenation)
/// * `import urllib.request as u` → `u → urllib.request`
/// * `from http.client import HTTPConnection` → `HTTPConnection → http.client.HTTPConnection`
/// * `from http import client as c` → `c → http.client`
fn python_bindings(code: &str) -> HashMap<String, String> {
    let mut bindings: HashMap<String, String> = HashMap::new();

    // `import a.b.c [as x][, d [as y]]`
    let plain = Regex::new(r"(?m)^[ \t]*import[ \t]+(.+)$").unwrap();
    // `from a.b import c [as x][, d [as y]]`
    let from = Regex::new(r"(?m)^[ \t]*from[ \t]+([\w.]+)[ \t]+import[ \t]+(.+)$").unwrap();

    for caps in from.captures_iter(code) {
        let module = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let names = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        for item in split_import_items(names) {
            let (name, alias) = parse_as_clause(&item);
            if name == "*" {
                continue;
            }
            let bound = alias.unwrap_or_else(|| name.clone());
            bindings.insert(bound, format!("{module}.{name}"));
        }
    }

    for caps in plain.captures_iter(code) {
        let rest = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        // Skip `from x import y` already handled, and `import` inside a string
        // would need a parser — the anchored `^\s*import` keeps false hits rare.
        for item in split_import_items(rest) {
            let (path, alias) = parse_as_clause(&item);
            if path.is_empty() {
                continue;
            }
            match alias {
                // `import a.b as x` binds only `x`, denoting `a.b`.
                Some(a) => {
                    bindings.insert(a, path);
                }
                // `import a.b` binds the top package `a`, denoting `a`.
                None => {
                    let top = path.split('.').next().unwrap_or(&path).to_string();
                    bindings.insert(top.clone(), top);
                }
            }
        }
    }

    bindings
}

/// JavaScript: names each `import`/`require` brings into scope, mapped to the
/// dotted path they denote. Module specifiers are normalised by stripping the
/// `node:` prefix so `node:https` and `https` resolve identically.
///
/// * `import https from "https"` → `https → https`
/// * `import * as net from "node:net"` → `net → net`
/// * `import { request as req } from "node:http"` → `req → http.request`
/// * `const n = require("net")` → `n → net`
/// * `const { connect } = require("net")` → `connect → net.connect`
fn javascript_bindings(code: &str) -> HashMap<String, String> {
    let mut bindings: HashMap<String, String> = HashMap::new();

    // import <clause> from "mod"
    let esm =
        Regex::new(r#"(?m)^[ \t]*import[ \t]+(.+?)[ \t]+from[ \t]*["']([^"']+)["']"#).unwrap();
    // const|let|var <clause> = require("mod")
    let cjs = Regex::new(
        r#"(?m)^[ \t]*(?:const|let|var)[ \t]+(.+?)[ \t]*=[ \t]*require\([ \t]*["']([^"']+)["'][ \t]*\)"#,
    )
    .unwrap();

    for caps in esm.captures_iter(code).chain(cjs.captures_iter(code)) {
        let clause = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        let module = normalize_js_module(caps.get(2).map(|m| m.as_str()).unwrap_or(""));
        if module.is_empty() {
            continue;
        }
        bind_js_clause(clause, &module, &mut bindings);
    }

    bindings
}

/// Bind one import/require clause: a namespace form (`* as ns`, `ns`), a
/// destructured/named form (`{ a, b as c }`), or a mix (`def, { a }`).
fn bind_js_clause(clause: &str, module: &str, bindings: &mut HashMap<String, String>) {
    let mut named_part: Option<&str> = None;
    let mut bare_part = clause;

    if let Some(open) = clause.find('{') {
        if let Some(close) = clause[open..].find('}') {
            named_part = Some(&clause[open + 1..open + close]);
            bare_part = &clause[..open];
        }
    }

    // Namespace / default binding: `ns`, `* as ns`, `def,`
    for token in bare_part.split(',') {
        let token = token.trim().trim_end_matches(',').trim();
        if token.is_empty() {
            continue;
        }
        let name = match token.strip_prefix('*') {
            // `* as ns`
            Some(rest) => rest.trim().strip_prefix("as").map(str::trim).unwrap_or(""),
            None => token,
        };
        if is_identifier(name) {
            bindings.insert(name.to_string(), module.to_string());
        }
    }

    // Named bindings: `{ request, get as fetchIt }` / `{ connect: c }`
    for item in named_part.unwrap_or("").split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        // `a as b` (ESM) or `a: b` (CJS destructuring)
        let (original, bound) = if let Some((l, r)) = split_once_keyword(item, " as ") {
            (l, r)
        } else if let Some((l, r)) = item.split_once(':') {
            (l.trim(), r.trim())
        } else {
            (item, item)
        };
        if is_identifier(original) && is_identifier(bound) {
            bindings.insert(bound.to_string(), format!("{module}.{original}"));
        }
    }
}

/// `node:https` → `https`; leaves bare and scoped specifiers untouched.
fn normalize_js_module(spec: &str) -> String {
    spec.trim()
        .strip_prefix("node:")
        .unwrap_or(spec.trim())
        .to_string()
}

fn split_once_keyword<'a>(s: &'a str, kw: &str) -> Option<(&'a str, &'a str)> {
    s.find(kw)
        .map(|i| (s[..i].trim(), s[i + kw.len()..].trim()))
}

/// Split a comma-separated import list, ignoring commas inside parentheses
/// (`from x import (a, b)`).
fn split_import_items(s: &str) -> Vec<String> {
    let s = s.trim().trim_start_matches('(').trim_end_matches(')');
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// `urllib.request as u` → (`urllib.request`, Some(`u`)).
fn parse_as_clause(item: &str) -> (String, Option<String>) {
    match split_once_keyword(item, " as ") {
        Some((name, alias)) => (name.to_string(), Some(alias.to_string())),
        None => (item.trim().to_string(), None),
    }
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// Which comment/string forms to blank out. The two languages this module
/// covers differ enough (triple-quoted strings vs block comments and template
/// literals) that the masker is told which grammar it is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskDialect {
    Python,
    JavaScript,
}

/// How much of the source to blank out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskScope {
    /// Comments only; string bodies survive.
    ///
    /// Required for the **binding** pass, because JavaScript module specifiers
    /// live inside quotes — masking them would turn `require("net")` into
    /// `require("   ")` and erase every JS binding.
    Comments,
    /// Comments *and* string bodies — for the **call** pass, where sink-shaped
    /// text inside a literal must not read as a call.
    CommentsAndStrings,
}

/// Blank out comments (and optionally string bodies), replacing each masked
/// character with a space and preserving newlines — so every byte offset and
/// line number in the result still matches the original source.
///
/// Without this, sink-shaped *text* was indistinguishable from a sink *call*: a
/// comment reading `# never call socket.socket() here` or a message
/// `print("socket.socket(")` raised a real `network_sink` signal, which becomes
/// an approval gate and — under a session taint that excludes `Sink::Network` —
/// a hard refuse.
///
/// String state is tracked even under [`MaskScope::Comments`], so a `#` or `//`
/// appearing inside a string literal does not start a comment.
///
/// JavaScript template literals keep their `${…}` interpolations unmasked,
/// because those hold real code: `` `${net.connect(80)}` `` is a genuine call.
///
/// Not masked: JS regex literals (`/net\.connect\(/`), which cannot be told from
/// division without parsing. A regex literal containing an imported module name
/// followed by `(` is the remaining known way to spoof a sink; it is a false
/// positive rather than a missed detection, and detection is a precision layer
/// (see `docs/sandbox-network-grant.md` for why it is not the boundary).
fn mask_strings_and_comments(code: &str, dialect: MaskDialect, scope: MaskScope) -> String {
    #[derive(PartialEq)]
    enum State {
        Code,
        LineComment,
        BlockComment,
        Quoted { delim: char, triple: bool },
        Template,
    }

    let chars: Vec<char> = code.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut state = State::Code;
    // Nesting depth of `${ … }` inside template literals. `> 0` means the
    // characters are code, not string body.
    let mut interp_depth: usize = 0;
    let mut i = 0;

    // Keep a character as-is; masked characters become spaces (newlines always
    // survive so line counts are unaffected).
    macro_rules! keep {
        ($n:expr) => {
            for _ in 0..$n {
                out.push(chars[i]);
                i += 1;
            }
        };
    }
    macro_rules! blank {
        ($n:expr) => {
            for _ in 0..$n {
                out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
        };
    }
    // Inside a string body: blank it only when strings are in scope, but always
    // consume it as string state so delimiters and comment markers within the
    // literal are handled correctly.
    macro_rules! body {
        ($n:expr) => {
            if scope == MaskScope::CommentsAndStrings {
                blank!($n)
            } else {
                keep!($n)
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match state {
            State::Code => {
                match (dialect, c, next) {
                    (MaskDialect::Python, '#', _) => {
                        state = State::LineComment;
                        blank!(1);
                    }
                    (MaskDialect::JavaScript, '/', Some('/')) => {
                        state = State::LineComment;
                        blank!(2);
                    }
                    (MaskDialect::JavaScript, '/', Some('*')) => {
                        state = State::BlockComment;
                        blank!(2);
                    }
                    (MaskDialect::JavaScript, '`', _) => {
                        state = State::Template;
                        body!(1);
                    }
                    (_, '\'', _) | (_, '"', _) => {
                        // Python triple-quote opens a multi-line body.
                        let triple = dialect == MaskDialect::Python
                            && next == Some(c)
                            && chars.get(i + 2).copied() == Some(c);
                        state = State::Quoted { delim: c, triple };
                        body!(if triple { 3 } else { 1 });
                    }
                    // A `}` while inside an interpolation returns to the
                    // template body.
                    (MaskDialect::JavaScript, '}', _) if interp_depth > 0 => {
                        interp_depth -= 1;
                        state = State::Template;
                        body!(1);
                    }
                    _ => keep!(1),
                }
            }
            State::LineComment => {
                if c == '\n' {
                    state = State::Code;
                }
                blank!(1);
            }
            State::BlockComment => {
                if c == '*' && next == Some('/') {
                    state = State::Code;
                    blank!(2);
                } else {
                    blank!(1);
                }
            }
            State::Quoted { delim, triple } => {
                if c == '\\' && next.is_some() {
                    body!(2); // escape pair — never terminates the string
                } else if c == delim {
                    if triple {
                        if next == Some(delim) && chars.get(i + 2).copied() == Some(delim) {
                            state = State::Code;
                            body!(3);
                        } else {
                            body!(1);
                        }
                    } else {
                        state = State::Code;
                        body!(1);
                    }
                } else if c == '\n' && !triple {
                    // Unterminated single-quoted string: recover at end of line
                    // rather than swallowing the rest of the file.
                    state = State::Code;
                    body!(1);
                } else {
                    body!(1);
                }
            }
            State::Template => {
                if c == '\\' && next.is_some() {
                    body!(2);
                } else if c == '$' && next == Some('{') {
                    // `${` — the interpolation body is code.
                    interp_depth += 1;
                    state = State::Code;
                    body!(2);
                } else if c == '`' {
                    state = State::Code;
                    body!(1);
                } else {
                    body!(1);
                }
            }
        }
    }

    out.into_iter().collect()
}

/// Resolve a dotted call head through `bindings` into its canonical path.
/// Returns `None` when the head is not bound by any import — the property that
/// keeps `mail.fetch(` and `dict.get(` from being mistaken for sinks.
fn resolve_call(head: &str, bindings: &HashMap<String, String>) -> Option<String> {
    let mut parts = head.split('.');
    let first = parts.next()?;
    let base = bindings.get(first)?;
    let rest: Vec<&str> = parts.collect();
    if rest.is_empty() {
        Some(base.clone())
    } else {
        Some(format!("{}.{}", base, rest.join(".")))
    }
}

/// Find every `<dotted.head>(` call in `code`, resolve it, and match against
/// `sinks`.
fn detect_sinks(
    code: &str,
    bindings: &HashMap<String, String>,
    sinks: &[(&str, &str)],
) -> Vec<DetectedSink> {
    let call_re = Regex::new(r"([A-Za-z_$][A-Za-z0-9_$]*(?:\.[A-Za-z_$][A-Za-z0-9_$]*)*)\s*\(")
        .expect("static call regex");

    let mut found: Vec<DetectedSink> = Vec::new();
    for caps in call_re.captures_iter(code) {
        let Some(head_match) = caps.get(1) else {
            continue;
        };
        let head = head_match.as_str();
        // `new net.Socket(` / `await tls.connect(` — the head match already
        // excludes the keyword, so nothing extra is needed here.
        let Some(canonical) = resolve_call(head, bindings) else {
            continue;
        };
        let Some((sink, reason)) = sinks
            .iter()
            .find(|(sink, _)| *sink == canonical.as_str())
            .copied()
        else {
            continue;
        };
        let line = code[..head_match.start()].matches('\n').count() + 1;
        if found
            .iter()
            .any(|f| f.sink == sink && f.line == line && f.matched == head)
        {
            continue;
        }
        found.push(DetectedSink {
            sink: sink.to_string(),
            matched: head.to_string(),
            line,
            reason: reason.to_string(),
        });
    }
    found
}

/// Detect Python stdlib network sinks reached through the code's own imports.
///
/// Comments and string bodies are blanked first
/// ([`mask_strings_and_comments`]), so sink-shaped *text* is not read as a sink
/// *call*.
pub fn detect_python_sinks(code: &str) -> Vec<DetectedSink> {
    // Bindings read comment-masked source (a commented-out import must not
    // bind); calls read fully-masked source (sink-shaped text must not fire).
    let for_bindings = mask_strings_and_comments(code, MaskDialect::Python, MaskScope::Comments);
    let for_calls =
        mask_strings_and_comments(code, MaskDialect::Python, MaskScope::CommentsAndStrings);
    detect_sinks(&for_calls, &python_bindings(&for_bindings), PYTHON_SINKS)
}

/// Detect Node built-in network sinks reached through the code's own
/// imports/requires.
///
/// Comments and string bodies are blanked first
/// ([`mask_strings_and_comments`]), template-literal `${…}` interpolations
/// excepted — those hold real code.
pub fn detect_javascript_sinks(code: &str) -> Vec<DetectedSink> {
    // Bindings must keep string bodies: the module specifier lives inside the
    // quotes (`require("net")`). Calls must not see them.
    let for_bindings =
        mask_strings_and_comments(code, MaskDialect::JavaScript, MaskScope::Comments);
    let for_calls =
        mask_strings_and_comments(code, MaskDialect::JavaScript, MaskScope::CommentsAndStrings);
    detect_sinks(
        &for_calls,
        &javascript_bindings(&for_bindings),
        JAVASCRIPT_SINKS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sinks_of(found: &[DetectedSink]) -> Vec<&str> {
        found.iter().map(|f| f.sink.as_str()).collect()
    }

    // --- Python ---

    /// The headline case: an unknown third-party wrapper is irrelevant, because
    /// the sink it bottoms out on is what gets detected.
    #[test]
    fn python_unknown_library_detected_via_stdlib_sink() {
        let code = r#"
import http.client
# `acme_sdk` is on no list anywhere, and does not need to be.
def call(host):
    conn = http.client.HTTPSConnection(host)
    conn.request("GET", "/v1/data")
    return conn.getresponse().read()
"#;
        let found = detect_python_sinks(code);
        assert_eq!(sinks_of(&found), vec!["http.client.HTTPSConnection"]);
        assert_eq!(found[0].line, 5);
    }

    #[test]
    fn python_alias_is_followed() {
        let code = r#"
import urllib.request as u
u.urlopen("https://example.org")
"#;
        let found = detect_python_sinks(code);
        assert_eq!(sinks_of(&found), vec!["urllib.request.urlopen"]);
        assert_eq!(found[0].matched, "u.urlopen");
    }

    #[test]
    fn python_from_import_with_alias_is_followed() {
        let code = r#"
from http.client import HTTPConnection as HC
from socket import create_connection
HC("example.org")
create_connection(("example.org", 443))
"#;
        let found = detect_python_sinks(code);
        let mut got = sinks_of(&found);
        got.sort();
        assert_eq!(
            got,
            vec!["http.client.HTTPConnection", "socket.create_connection"]
        );
    }

    #[test]
    fn python_dotted_import_resolves_by_concatenation() {
        // `import urllib.request` binds `urllib`, so the full path must still
        // resolve.
        let code = "import urllib.request\nurllib.request.urlopen(dest)\n";
        assert_eq!(
            sinks_of(&detect_python_sinks(code)),
            vec!["urllib.request.urlopen"]
        );
    }

    /// The precision property. `mail.fetch(` is imaplib's `IMAP4.fetch`, not the
    /// JS Fetch API — and `mail` is bound to nothing, so it resolves to nothing.
    /// This is the #1019 false positive, now impossible by construction rather
    /// than by a byte-boundary special case.
    #[test]
    fn python_method_calls_on_instances_are_not_sinks() {
        let code = r#"
import imaplib
mail = imaplib.IMAP4_SSL("imap.example.com")
mail.fetch(b"1", "(RFC822)")
data = {}
data.get("http://example.org")
"#;
        let found = detect_python_sinks(code);
        // The constructor IS the sink; the instance methods are not.
        assert_eq!(sinks_of(&found), vec!["imaplib.IMAP4_SSL"]);
    }

    #[test]
    fn python_unimported_names_are_not_sinks() {
        // No import at all → nothing is bound → no sink, however suggestive the
        // name is.
        let code = "urlopen(\"https://example.org\")\nsocket.socket()\n";
        assert!(detect_python_sinks(code).is_empty());
    }

    #[test]
    fn python_inert_code_has_no_sinks() {
        let code = "import json\nimport math\nprint(json.dumps({\"a\": math.sqrt(4)}))\n";
        assert!(detect_python_sinks(code).is_empty());
    }

    #[test]
    fn python_detects_mail_and_async_sinks() {
        let code = r#"
import smtplib, asyncio
from poplib import POP3_SSL
smtplib.SMTP_SSL("smtp.example.com")
asyncio.open_connection("example.org", 80)
POP3_SSL("pop.example.com")
"#;
        let found = detect_python_sinks(code);
        let mut got = sinks_of(&found);
        got.sort();
        assert_eq!(
            got,
            vec![
                "asyncio.open_connection",
                "poplib.POP3_SSL",
                "smtplib.SMTP_SSL"
            ]
        );
    }

    // --- JavaScript ---

    #[test]
    fn javascript_esm_named_import_is_followed() {
        let code = r#"
import { request } from "node:https";
request({ host: "example.org" });
"#;
        let found = detect_javascript_sinks(code);
        assert_eq!(sinks_of(&found), vec!["https.request"]);
    }

    #[test]
    fn javascript_require_default_and_destructured() {
        let code = r#"
const net = require("net");
const { connect } = require("node:tls");
net.createConnection(443, "example.org");
connect({ host: "example.org" });
"#;
        let found = detect_javascript_sinks(code);
        let mut got = sinks_of(&found);
        got.sort();
        assert_eq!(got, vec!["net.createConnection", "tls.connect"]);
    }

    #[test]
    fn javascript_namespace_and_rename_are_followed() {
        let code = r#"
import * as h from "http";
import { get as grab } from "https";
h.request({});
grab("https://example.org");
"#;
        let found = detect_javascript_sinks(code);
        let mut got = sinks_of(&found);
        got.sort();
        assert_eq!(got, vec!["http.request", "https.get"]);
    }

    /// `node:https` and `https` must resolve identically.
    #[test]
    fn javascript_node_prefix_is_normalized() {
        let prefixed = detect_javascript_sinks(
            "import https from \"node:https\";\nhttps.get(\"https://example.org\");\n",
        );
        let bare = detect_javascript_sinks(
            "import https from \"https\";\nhttps.get(\"https://example.org\");\n",
        );
        assert_eq!(sinks_of(&prefixed), vec!["https.get"]);
        assert_eq!(sinks_of(&prefixed), sinks_of(&bare));
    }

    /// An npm client that is on no list: the sink it bottoms out on is caught.
    #[test]
    fn javascript_unknown_package_detected_via_builtin_sink() {
        let code = r#"
const { Socket } = require("net");
class Mystery {
  open(host) { return new Socket().connect(443, host); }
}
"#;
        assert_eq!(sinks_of(&detect_javascript_sinks(code)), vec!["net.Socket"]);
    }

    /// A same-named method on an unrelated object resolves to nothing.
    #[test]
    fn javascript_unbound_heads_are_not_sinks() {
        let code = r#"
const db = openDatabase();
db.connect();
router.get("/health");
http.request({});
"#;
        assert!(detect_javascript_sinks(code).is_empty());
    }

    #[test]
    fn javascript_side_effect_import_binds_nothing() {
        // `import "net"` introduces no name, so there is nothing to resolve.
        let code = "import \"net\";\nnet.connect(80);\n";
        assert!(detect_javascript_sinks(code).is_empty());
    }

    // --- cross-language ---

    /// Python sinks must not fire on JS source and vice versa: the tables are
    /// separate and each is driven by its own binding grammar. (`require` is not
    /// Python syntax, so a Python scan of JS source binds nothing.)
    #[test]
    fn tables_do_not_cross_languages() {
        let js = "const net = require(\"net\");\nnet.connect(80, \"example.org\");\n";
        assert!(detect_python_sinks(js).is_empty());

        let py = "import socket\nsocket.socket()\n";
        assert!(detect_javascript_sinks(py).is_empty());
    }

    #[test]
    fn duplicate_calls_on_one_line_are_deduped() {
        let code = "import socket\nsocket.socket(); socket.socket()\n";
        assert_eq!(detect_python_sinks(code).len(), 1);
    }

    // --- strings and comments must not read as calls (PR #1033 review) ---

    /// Sink-shaped *text* is not a sink *call*. Every one of these fired before
    /// masking, turning a comment or a log message into an approval gate — and
    /// under a taint that excludes `Sink::Network`, into a hard refuse.
    #[test]
    fn python_sink_text_in_strings_and_comments_is_not_a_call() {
        let cases = [
            ("string literal", "import socket\nprint(\"socket.socket(\")\n"),
            (
                "line comment",
                "import socket\n# never call socket.socket() here\nx = 1\n",
            ),
            (
                "docstring",
                "import urllib.request\n\"\"\"Do not use urllib.request.urlopen() directly.\"\"\"\nx = 1\n",
            ),
            (
                "single-quoted with escape",
                "import socket\nmsg = 'call \\'socket.socket(\\' never'\n",
            ),
            (
                "comment after real code on the same line",
                "import json\nj = json.dumps({})  # not socket.socket()\n",
            ),
        ];
        for (label, code) in cases {
            let found = detect_python_sinks(code);
            assert!(
                found.is_empty(),
                "{label}: sink-shaped text must not be detected, got {:?}",
                sinks_of(&found)
            );
        }
    }

    #[test]
    fn javascript_sink_text_in_strings_and_comments_is_not_a_call() {
        let cases = [
            (
                "line comment",
                "const net = require(\"net\");\n// net.connect(80) is forbidden\nlet x = 1;\n",
            ),
            (
                "block comment",
                "const net = require(\"net\");\n/* net.connect(80)\n   spans lines */\nlet x = 1;\n",
            ),
            (
                "template literal",
                "const net = require(\"net\");\nconst msg = `call net.connect(80)`;\n",
            ),
            (
                "double-quoted string",
                "const net = require(\"net\");\nconst m = \"net.connect(80)\";\n",
            ),
        ];
        for (label, code) in cases {
            let found = detect_javascript_sinks(code);
            assert!(
                found.is_empty(),
                "{label}: sink-shaped text must not be detected, got {:?}",
                sinks_of(&found)
            );
        }
    }

    /// The trap masking could easily introduce: JS module specifiers live
    /// *inside* the quotes, so blanking string bodies before collecting bindings
    /// would erase every JS binding and silently disable detection. Bindings read
    /// comment-masked source; only the call pass masks strings.
    #[test]
    fn javascript_module_specifiers_survive_masking() {
        let code = "const net = require(\"net\");\nnet.createConnection(443, host);\n";
        assert_eq!(
            sinks_of(&detect_javascript_sinks(code)),
            vec!["net.createConnection"],
            "masking must not erase the module specifier"
        );

        let esm = "import { request } from \"node:https\";\nrequest(opts);\n";
        assert_eq!(
            sinks_of(&detect_javascript_sinks(esm)),
            vec!["https.request"]
        );
    }

    /// A template literal's `${…}` interpolation is real code, so a sink called
    /// there must still be detected — masking the whole literal would be a false
    /// negative.
    #[test]
    fn javascript_sink_inside_template_interpolation_is_detected() {
        let code = "const net = require(\"net\");\nconst s = `${net.connect(80)}`;\n";
        assert_eq!(
            sinks_of(&detect_javascript_sinks(code)),
            vec!["net.connect"]
        );
    }

    /// A commented-out import binds nothing, so a later same-named call cannot
    /// resolve through it.
    #[test]
    fn commented_out_import_does_not_bind() {
        let py = "# import socket\nsocket.socket()\n";
        assert!(detect_python_sinks(py).is_empty());

        let js = "// const net = require(\"net\");\nnet.connect(80);\n";
        assert!(detect_javascript_sinks(js).is_empty());
    }

    /// A comment marker inside a string must not open a comment, or everything
    /// after it on the line would be masked and real calls would be missed.
    #[test]
    fn comment_markers_inside_strings_do_not_start_comments() {
        let py = "import socket\nsep = \"#\"\nsocket.socket()\n";
        assert_eq!(sinks_of(&detect_python_sinks(py)), vec!["socket.socket"]);

        let js = "const net = require(\"net\");\nconst u = \"http://x/a//b\";\nnet.connect(80);\n";
        assert_eq!(sinks_of(&detect_javascript_sinks(js)), vec!["net.connect"]);
    }

    /// Masking preserves offsets and newlines, so reported line numbers stay
    /// exact even when earlier lines are blanked.
    #[test]
    fn masking_preserves_line_numbers() {
        let code = "import socket\n# socket.socket() mentioned\n\"\"\"and\nhere too\n\"\"\"\nsocket.socket()\n";
        let found = detect_python_sinks(code);
        assert_eq!(sinks_of(&found), vec!["socket.socket"]);
        assert_eq!(found[0].line, 6, "line number must survive masking");
    }

    #[test]
    fn masking_preserves_length_and_newlines() {
        let code = "import socket\n# comment\nx = \"str\"\n";
        for scope in [MaskScope::Comments, MaskScope::CommentsAndStrings] {
            let masked = mask_strings_and_comments(code, MaskDialect::Python, scope);
            assert_eq!(
                masked.chars().count(),
                code.chars().count(),
                "{scope:?} changed length"
            );
            assert_eq!(
                masked.matches('\n').count(),
                code.matches('\n').count(),
                "{scope:?} changed newline count"
            );
        }
    }

    /// An unterminated quote must not swallow the rest of the file — otherwise
    /// one stray apostrophe would disable detection for everything below it.
    #[test]
    fn unterminated_string_recovers_at_end_of_line() {
        let code = "import socket\nbad = 'oops\nsocket.socket()\n";
        assert_eq!(sinks_of(&detect_python_sinks(code)), vec!["socket.socket"]);
    }
}
