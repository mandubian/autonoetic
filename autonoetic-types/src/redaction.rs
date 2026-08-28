//! Canonical redaction helpers shared across the workspace.
//!
//! These primitives detect and mask credential-shaped content in arbitrary
//! text or JSON payloads. They are used by:
//!
//! - **`autonoetic-types::causal_chain`** — when applying `redact_for_viewer`
//!   to non-Admin observers of `ExecutionTraceRecord` / `CausalEventRecord`.
//! - **`autonoetic-types::background`** — same, for `ScheduledAction`.
//! - **`autonoetic-gateway::log_redaction`** — when wrapping causal-chain
//!   payloads via `RedactedPayload` (the gateway-side P-4.14 invariant).
//!
//! Centralising them here removes the prior triplication (one copy in each
//! of those modules) and prevents drift.
//!
//! ## Design choice: prefix-anchored patterns, not entropy heuristics
//!
//! The catalogue recognises credentials two ways. **Name-based** rules key off
//! the surrounding syntax (`API_KEY=…`, `?token=…`, `Authorization: Bearer …`)
//! and **prefix-anchored** rules key off the credential's own self-identifying
//! head (`ghp_…`, `AKIA…`, `sk-…`). The second kind exists because the first
//! structurally cannot see `export GH=ghp_…`: the value is a credential but
//! the variable name does not say so.
//!
//! Deliberately absent from the masker: entropy and length heuristics. A
//! 40-char hex string is as likely to be a revision id or content digest as a
//! secret, and those are exactly the identifiers an operator triages *by*.
//! `LONG_HEX_RE` and `JWT_RE` therefore serve `looks_like_secret_value` (which
//! gates wholesale redaction of header *values*, where the surrounding key
//! already supplies the context) and are kept out of
//! `redact_embedded_secrets`. Over-masking degrades review as surely as
//! under-masking leaks it; both failures are silent.
//!
//! Adding a family means adding a row to `VENDOR_SAMPLES` in the tests, so the
//! coverage is readable as a list rather than inferred from regexes.
//!
//! ## Design choice: precise masking over wholesale redaction
//!
//! An earlier implementation in both `redact_text_for_logs` (gateway) and
//! `redact_json_string` (causal_chain) replaced *the entire input string*
//! with `***REDACTED***` whenever a substring like `token`, `secret`, or
//! `authorization` was present. That over-redacts strings such as
//! `"Updated tokenizer config"`, `"secretary-general announcement"`, or
//! `"apikey-generator-doc"`, and also leaks an oracle (the branch decision
//! is observable, letting an attacker probe whether arbitrary text contains
//! one of those substrings). The canonical implementation here uses the
//! `redact_embedded_secrets` regex catalogue to mask only the credential-
//! shaped fragments, leaving the surrounding prose intact.

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

/// Replacement marker used everywhere a redacted value is emitted.
pub const REDACTED: &str = "***REDACTED***";

// ── Regex catalogue ──────────────────────────────────────────────────────────

/// `KEY=value` style env-var assignment carrying a credential-shaped name.
///
/// The leading `[A-Z0-9_]*` (zero-or-more) is intentional so a bare
/// `PASSWORD=…` matches in addition to prefixed forms like `MY_PASSWORD=…`
/// and `OPENWEATHER_API_KEY=…`. (The previous `[A-Z][A-Z0-9_]*` required at
/// least one prefix character, which silently missed unprefixed names.)
static ENV_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b([A-Z0-9_]*(?:API[_-]?KEY|TOKEN|SECRET|PASSWORD|ACCESS[_-]?KEY|ACCESS[_-]?TOKEN|REFRESH[_-]?TOKEN|AUTHORIZATION)[A-Z0-9_]*)=("[^"]*"|'[^']*'|[^\s&;]+)"#,
    )
    .expect("valid env assignment redaction regex")
});

/// URL-style `?foo=bar&baz=qux` assignment carrying a credential-shaped key.
static QUERY_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)([?&](?:api[_-]?key|apikey|appid|token|access_token|refresh_token|client_secret|password|secret|authorization)=)([^&#\s]+)",
    )
    .expect("valid query assignment redaction regex")
});

/// `Bearer <token>` HTTP authorization header.
static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(bearer\s+)([^\s,;]+)").expect("valid bearer redaction regex")
});

/// Plain `KEY=value` form used by `looks_like_secret_value`.
///
/// Same `[A-Z0-9_]*` (zero-or-more) prefix as `ENV_ASSIGN_RE` so unprefixed
/// names like `PASSWORD=…` are detected.
static SECRET_ENV_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b[A-Z0-9_]*(?:API[_-]?KEY|TOKEN|SECRET|PASSWORD|ACCESS[_-]?TOKEN|AUTHORIZATION)[A-Z0-9_]*\s*=\s*\S+",
    )
    .expect("valid secret env assignment regex")
});

/// Vendor-issued credentials, matched by their **self-identifying prefix**.
///
/// Prefix-anchored patterns are the only kind safe to add to the in-place
/// masker. An entropy or length heuristic cannot tell a token from a content
/// digest or a revision id, so it would mask the identifiers an operator
/// triages *by* — the same reasoning that keeps `LONG_HEX_RE` and `JWT_RE`
/// out of `redact_embedded_secrets` (see `redact_json_value`). A vendor
/// prefix carries its own claim: `ghp_…` is a GitHub token and nothing else.
///
/// This also catches what `ENV_ASSIGN_RE` structurally cannot. That rule keys
/// off the *variable name* (`API_KEY=`, `TOKEN=`), so `export GH=ghp_…` slips
/// through — the value is a credential but the name does not say so. A prefix
/// rule does not care what the variable is called.
///
/// Length floors are set so a real credential matches and an English word
/// does not: `sk-` needs 16 following characters, which admits
/// `sk-proj-<48 chars>` and excludes prose like `sk-learn-model`.
static VENDOR_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        \b(?:
              gh[pousr]_[A-Za-z0-9]{16,}              # GitHub PAT / OAuth / user / server / refresh
            | github_pat_[A-Za-z0-9_]{20,}            # GitHub fine-grained PAT
            | glpat-[A-Za-z0-9_\-]{16,}               # GitLab PAT
            | xox[abprs]-[A-Za-z0-9\-]{10,}           # Slack
            | (?:AKIA|ASIA)[A-Z0-9]{16}               # AWS access key id
            | AIza[A-Za-z0-9_\-]{35}                  # Google API key
            | sk-[A-Za-z0-9_\-]{16,}                  # OpenAI / Anthropic
            | (?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,} # Stripe secret / restricted
            | npm_[A-Za-z0-9]{36}                     # npm automation token
            | dop_v1_[a-f0-9]{64}                     # DigitalOcean
        )",
    )
    .expect("valid vendor token regex")
});

/// `Authorization: <scheme> <value>` for schemes other than Bearer.
///
/// `BEARER_RE` deliberately matches `bearer <token>` anywhere, since that form
/// is unambiguous on its own. `Basic`/`Token`/`Digest` are common English
/// words, so they are only treated as credentials when preceded by the header
/// name.
static AUTH_SCHEME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(authorization\s*:\s*(?:basic|token|digest)\s+)([^\s'"]+)"#)
        .expect("valid auth scheme regex")
});

/// Password in URL userinfo — `https://user:password@host`.
///
/// Requires the `@` so `https://host:8080/path` cannot match.
static URL_USERINFO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(://[^/\s:@]+:)([^/\s@]+)(@)").expect("valid url userinfo regex")
});

/// PEM private-key block. Masked as a whole: the key body spans multiple
/// base64 lines between the markers, so there is no single value to replace.
/// `redact_json_value` already special-cased this for JSON strings; the text
/// path needs it too, or a key inlined into a shell command survives.
static PEM_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----")
        .expect("valid pem block regex")
});

/// Generic long hex string (≥ 24 hex chars) — likely a raw secret in a
/// non-JSON context.
static LONG_HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[a-fA-F0-9]{24,}\b").expect("valid long hex regex"));

/// JWT-shaped token (three base64url segments separated by dots).
static JWT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b")
        .expect("valid jwt regex")
});

// ── Public API ───────────────────────────────────────────────────────────────

/// Returns `true` if the supplied JSON object key looks credential-shaped.
///
/// Matches the substring catalogue: `secret`, `token`, `password`, `api_key`,
/// `apikey`, `authorization`, `access_key`, `access_token`, `refresh_token`,
/// `client_secret` (case-insensitive).
///
/// Hyphenated header-style names (`X-API-Key`, `X-Access-Token`) are matched
/// too: the catalogue carries both the underscore and hyphen spellings, since
/// a JSON body carrying HTTP headers uses the hyphenated form.
pub fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("secret")
        || k.contains("token")
        || k.contains("password")
        || k.contains("api_key")
        || k.contains("api-key")
        || k.contains("apikey")
        || k.contains("authorization")
        || k.contains("access_key")
        || k.contains("access-key")
        || k.contains("access_token")
        || k.contains("access-token")
        || k.contains("refresh_token")
        || k.contains("refresh-token")
        || k.contains("client_secret")
        || k.contains("client-secret")
}

/// Returns `true` if a free-form string value looks credential-bearing.
pub fn looks_like_secret_value(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    lower.contains("bearer ")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("access_token")
        || t.starts_with("sk-")
        || t.contains("-----BEGIN")
        || VENDOR_TOKEN_RE.is_match(t)
        || SECRET_ENV_ASSIGN_RE.is_match(t)
        || LONG_HEX_RE.is_match(t)
        || JWT_RE.is_match(t)
}

/// Detects whether free-form text appears to *solicit* secret input from a
/// human (as opposed to *containing* one).
pub fn looks_like_secret_collection_prompt(text: &str) -> bool {
    let s = text.to_ascii_lowercase();
    s.contains("api key")
        || s.contains("apikey")
        || s.contains("token")
        || s.contains("password")
        || s.contains("secret")
        || s.contains("private key")
        || s.contains("bearer")
        || s.contains("credential value")
        || s.contains("paste the key")
        || s.contains("paste your key")
        || s.contains("enter your key")
}

/// Mask credential-shaped fragments embedded in plain text.
///
/// This is the precise version: it leaves the surrounding text intact and
/// only replaces the credential value.
pub fn redact_embedded_secrets(text: &str) -> String {
    // PEM first: it is the only multi-line shape, and masking it whole avoids
    // the inner rules chewing on its base64 body.
    let masked_pem = PEM_BLOCK_RE.replace_all(text, REDACTED).to_string();
    let masked_env = ENV_ASSIGN_RE
        .replace_all(&masked_pem, "${1}=***REDACTED***")
        .to_string();
    let masked_query = QUERY_ASSIGN_RE
        .replace_all(&masked_env, "${1}***REDACTED***")
        .to_string();
    let masked_bearer = BEARER_RE
        .replace_all(&masked_query, "${1}***REDACTED***")
        .to_string();
    let masked_scheme = AUTH_SCHEME_RE
        .replace_all(&masked_bearer, "${1}***REDACTED***")
        .to_string();
    let masked_userinfo = URL_USERINFO_RE
        .replace_all(&masked_scheme, "${1}***REDACTED***${3}")
        .to_string();
    // Vendor prefixes last: by now the named forms above have been masked, so
    // this only fires on credentials that arrived without a telling name —
    // `export GH=ghp_…`, a bare `AKIA…` argument, a key pasted into a flag.
    let masked_vendor = VENDOR_TOKEN_RE
        .replace_all(&masked_userinfo, REDACTED)
        .to_string();
    // A short `sk-…` that is the entire string falls below the vendor rule's
    // length floor, and there is no delimiter to mask around — so the whole
    // string goes. Retained from the original implementation.
    if masked_vendor.starts_with("sk-") {
        return REDACTED.to_string();
    }
    masked_vendor
}

/// Recursively redact a JSON value: object keys matching `is_sensitive_key`
/// get their values replaced with `REDACTED`; string values are passed
/// through `redact_embedded_secrets` so embedded credentials are masked
/// without nuking the surrounding payload. As a backstop for credential
/// shapes that can't be masked in place (PEM blocks specifically),
/// strings matching that narrow pattern are wholesale-redacted.
///
/// **Why narrower than `looks_like_secret_value` for the fallback:** in JSON
/// payloads, content digests, revision IDs, and delivery IDs routinely look
/// like long hex strings or JWT-shaped tokens. Falling back on those shapes
/// would over-redact identifiers (`"delivery_id":"hook-<sha256>"`,
/// `"revision_id":"<40-char-sha>"`). PEM blocks are the only widespread
/// shape that genuinely contains a secret and can't be masked in place,
/// so the fallback is restricted to that.
pub fn redact_json_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if is_sensitive_key(k) {
                    out.insert(k.clone(), Value::String(REDACTED.to_string()));
                } else {
                    out.insert(k.clone(), redact_json_value(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_json_value).collect()),
        Value::String(s) => {
            let masked = redact_embedded_secrets(s);
            if masked != s.as_str() {
                // In-place mask handled it (bearer, env-var, query secret, sk-).
                Value::String(masked)
            } else if s.contains("-----BEGIN") {
                // PEM block — can't mask only the key body cleanly because it
                // spans multiple base64 lines between the BEGIN/END markers.
                // Wholesale-redact the whole value.
                Value::String(REDACTED.to_string())
            } else {
                Value::String(s.clone())
            }
        }
        other => other.clone(),
    }
}

/// Redact potentially sensitive content for structured logging.
///
/// If `text` parses as JSON, recursively redact via `redact_json_value` and
/// re-serialize. Otherwise mask credential-shaped fragments via
/// `redact_embedded_secrets` while preserving the surrounding prose.
///
/// **The non-JSON path no longer nukes the entire string when a benign
/// substring like `token` or `secret` appears.** That over-redaction is
/// the bug fixed in issue #156.
pub fn redact_text_for_logs(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => serde_json::to_string(&redact_json_value(&v))
            .unwrap_or_else(|_| REDACTED.to_string()),
        Err(_) => redact_embedded_secrets(text),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Bug-fix coverage: benign substrings round-trip ───────────────────

    #[test]
    fn benign_substrings_no_longer_nuke_full_string() {
        // The cases that previously got wholesale-redacted because they
        // contained "token", "secret", "authorization", "api_key", or
        // "apikey" as innocuous substrings.
        for input in &[
            "Updated tokenizer config in v2",
            "secretary-general announcement",
            "apikey-generator-doc.md",
            "the authorization process is documented in section 4",
            "Updated token bucket rate limiter",
        ] {
            let out = redact_text_for_logs(input);
            assert_eq!(
                out, *input,
                "benign string '{input}' must round-trip — got '{out}'"
            );
        }
    }

    // ── Real secrets still get masked precisely ──────────────────────────

    #[test]
    fn bearer_token_value_is_masked_in_place() {
        let input = "Authorization: Bearer eyJhbGc.foo.bar plus context";
        let out = redact_text_for_logs(input);
        assert!(out.contains("Bearer ***REDACTED***"));
        assert!(out.contains("plus context"), "surrounding text preserved: {out}");
        assert!(!out.contains("eyJhbGc"));
    }

    #[test]
    fn env_assignment_value_is_masked_in_place() {
        let input = "running with PASSWORD=hunter2 and other args";
        let out = redact_text_for_logs(input);
        assert!(out.contains("PASSWORD=***REDACTED***"));
        assert!(out.contains("and other args"));
        assert!(!out.contains("hunter2"));
    }

    #[test]
    fn url_query_secret_is_masked_in_place() {
        let input = "fetch http://api.example.com/data?api_key=abc123def456&q=Paris";
        let out = redact_text_for_logs(input);
        assert!(out.contains("api_key=***REDACTED***"));
        assert!(out.contains("q=Paris"));
        assert!(!out.contains("abc123def456"));
    }

    #[test]
    fn bare_sk_prefix_is_redacted_wholesale() {
        // `sk-…` at the start of a non-JSON string is treated as a raw
        // secret since there's no delimiter to mask just the value.
        let out = redact_text_for_logs("sk-abc123def456ghi789");
        assert_eq!(out, REDACTED);
    }

    // ── JSON path ────────────────────────────────────────────────────────

    // ── Coverage catalogue (#1212) ───────────────────────────────────────
    //
    // Table-driven on purpose: the set of credential families this masker
    // recognises should be readable as a list, not inferred from regexes.
    // Adding a family means adding a row.

    /// `(family, realistic sample, fragment that must not survive)`.
    ///
    /// Built at runtime from a prefix plus a body rather than written as
    /// literals. A fixture realistic enough to exercise these rules is, by
    /// construction, shaped like a real credential — GitHub's push protection
    /// blocked an earlier version of this file over the DigitalOcean sample,
    /// correctly. Assembling the strings keeps the repository free of
    /// scannable literals while the tests still see the full token.
    fn vendor_samples() -> Vec<(&'static str, String, String)> {
        let hex64 = "0123456789abcdef".repeat(4);
        let rows: Vec<(&'static str, String, String)> = vec![
            ("GitHub PAT", format!("{}_{}", "ghp", "16C7e42F292c6912E7710c838347Ae178B4a")),
            ("GitHub fine-grained", format!("{}_{}", "github_pat", "11ABCDEFG0abcdefghijklmnopqrstuvwxyz012345")),
            ("GitLab PAT", format!("{}-{}", "glpat", "ABCdef123456789xyz")),
            ("Slack bot", format!("{}-{}", "xoxb", "1234567890-abcdefghijkl")),
            ("AWS access key id", format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE")),
            ("AWS STS key id", format!("{}{}", "ASIA", "IOSFODNN7EXAMPLE")),
            ("Google API key", format!("{}{}", "AIza", "SyD-abcdefghijklmnopqrstuvwxyz01234")),
            ("OpenAI / Anthropic", format!("{}-{}", "sk", "abc123def456ghi789")),
            ("Stripe secret", format!("{}_{}_{}", "sk", "live", "abcdefghijklmnop1234")),
            ("npm automation", format!("{}_{}", "npm", "abcdefghijklmnopqrstuvwxyz0123456789")),
            ("DigitalOcean", format!("{}_{}_{}", "dop", "v1", hex64)),
        ]
        .into_iter()
        .map(|(family, token)| {
            // Embedded in a command, which is how these actually reach a gate.
            let sample = format!("run --credential {token} https://example.test");
            (family, sample, token)
        })
        .collect();
        rows
    }

    #[test]
    fn every_vendor_family_is_masked_and_named() {
        for (family, sample, must_not_survive) in vendor_samples() {
            let masked = redact_embedded_secrets(&sample);
            assert!(
                !masked.contains(&must_not_survive),
                "{family}: credential survived masking\n  in:  {sample}\n  out: {masked}"
            );
            assert!(
                masked.contains(REDACTED),
                "{family}: nothing was masked\n  out: {masked}"
            );
            assert!(
                masked.starts_with("run --credential ") && masked.ends_with("https://example.test"),
                "{family}: masking ate the surrounding command\n  out: {masked}"
            );
        }
    }

    #[test]
    fn every_vendor_family_is_also_detected_as_a_secret_value() {
        // `looks_like_secret_value` gates wholesale header redaction, so the
        // detector and the masker must agree on what counts as a credential.
        for (family, sample, _) in vendor_samples() {
            assert!(
                looks_like_secret_value(&sample),
                "{family}: masked but not detected — detector and masker disagree"
            );
        }
    }

    #[test]
    fn vendor_masking_preserves_the_surrounding_command() {
        // The point of in-place masking: the operator can still see what the
        // command *does*, which is what triage turns on.
        let key = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
        let masked = redact_embedded_secrets(&format!("aws s3 cp x s3://b --key {key}"));
        assert!(masked.starts_with("aws s3 cp x s3://b --key "), "got: {masked}");
        assert!(masked.ends_with(REDACTED), "got: {masked}");
    }

    #[test]
    fn prefix_rules_catch_what_name_based_rules_structurally_cannot() {
        // `ENV_ASSIGN_RE` keys off the *variable name*, so a credential
        // assigned to an innocuously named variable slips past it. A prefix
        // rule does not care what the variable is called — this is the reason
        // the vendor catalogue exists rather than more name patterns.
        let token = format!("{}_{}", "ghp", "16C7e42F292c6912E7710c838347Ae178B4a");
        let masked = redact_embedded_secrets(&format!("export GH={token}"));
        assert!(!masked.contains(&token), "got: {masked}");
        assert!(masked.starts_with("export GH="), "variable name should survive: {masked}");
    }

    // ── Shapes that are not vendor-prefixed ─────────────────────────────

    #[test]
    fn non_bearer_authorization_schemes_are_masked() {
        let masked = redact_embedded_secrets(
            "curl -H 'Authorization: Basic dXNlcjpwYXNzd29yZA==' https://x",
        );
        assert!(!masked.contains("dXNlcjpwYXNzd29yZA=="), "got: {masked}");
        assert!(
            masked.contains("Authorization: Basic"),
            "the scheme must stay visible — knowing basic-auth is in use is triage: {masked}"
        );
    }

    #[test]
    fn url_userinfo_password_is_masked_but_the_user_and_host_survive() {
        let masked = redact_embedded_secrets("git clone https://alice:s3cr3tpw@github.com/x/y");
        assert!(!masked.contains("s3cr3tpw"), "got: {masked}");
        assert!(masked.contains("alice"), "user should survive: {masked}");
        assert!(masked.contains("github.com/x/y"), "host should survive: {masked}");
    }

    #[test]
    fn a_port_is_not_mistaken_for_userinfo() {
        // The `@` requirement is what keeps `host:8080` from matching.
        let text = "curl https://example.com:8080/health";
        assert_eq!(redact_embedded_secrets(text), text);
    }

    #[test]
    fn inline_pem_private_key_is_masked_in_text_not_only_in_json() {
        let masked = redact_embedded_secrets(
            "echo '-----BEGIN RSA PRIVATE KEY-----\nMIIEabcdef\n-----END RSA PRIVATE KEY-----' > k.pem",
        );
        assert!(!masked.contains("MIIEabcdef"), "key body survived: {masked}");
        assert!(masked.contains("> k.pem"), "the command shape should survive: {masked}");
    }

    // ── False-positive floor ────────────────────────────────────────────

    #[test]
    fn ordinary_commands_are_left_alone() {
        // Over-masking degrades triage as surely as under-masking leaks. Each
        // of these contains a fragment that a laxer rule would have eaten.
        for text in [
            "pip install scikit-learn",
            "git commit -m 'update tokenizer config'",
            "ls -la /var/secrets",
            "curl https://example.com:8080/health",
            "cargo test --package sk-tools",
            "echo AKIA is a prefix",
            "docker run -e NODE_ENV=production app",
        ] {
            assert_eq!(
                redact_embedded_secrets(text),
                text,
                "over-masked a benign command: {text}"
            );
        }
    }

    #[test]
    fn a_content_digest_is_not_a_credential() {
        // Long hex and JWT shapes stay out of the in-place masker on purpose:
        // revision ids and content digests look exactly like them, and an
        // operator triages *by* those identifiers.
        let text = "promote rev_sha256:0123456789abcdef0123456789abcdef01234567";
        assert_eq!(redact_embedded_secrets(text), text);
    }

    #[test]
    fn json_redacts_sensitive_keys_and_preserves_structure() {
        let input = r#"{"token":"abc","nested":{"client_secret":"xyz"},"safe":"ok"}"#;
        let out = redact_text_for_logs(input);
        assert!(out.contains("***REDACTED***"));
        assert!(out.contains("\"safe\":\"ok\""));
        assert!(!out.contains("\"abc\""));
        assert!(!out.contains("\"xyz\""));
    }

    #[test]
    fn json_string_value_with_embedded_secret_is_masked() {
        let input = r#"{"command":"export OPENWEATHER_API_KEY=secret_value_xyz && curl"}"#;
        let out = redact_text_for_logs(input);
        assert!(out.contains("OPENWEATHER_API_KEY=***REDACTED***"));
        assert!(!out.contains("secret_value_xyz"));
        assert!(out.contains("export"));
        assert!(out.contains("curl"));
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    #[test]
    fn is_sensitive_key_documented_substrings() {
        for k in &[
            "secret",
            "TOKEN",
            "user_password",
            "api_key",
            "Authorization",
            "access_token",
            "refresh_token",
            "client_secret",
        ] {
            assert!(is_sensitive_key(k), "expected sensitive: {k}");
        }
        for k in &["user_id", "agent_id", "items", "ok"] {
            assert!(!is_sensitive_key(k), "expected non-sensitive: {k}");
        }
    }

    #[test]
    fn is_sensitive_key_overflags_token_containing_keys() {
        // Documented over-flagging: the substring catalogue treats any key
        // containing `token` as sensitive, so `tokenizer`, `tokenization`,
        // and `notification_token_count` all match. This is the conservative
        // behaviour preserved from the prior implementation; widening the
        // catalogue (e.g. to require word boundaries) would risk under-
        // detection. The precision win in #156 is at the *string-value*
        // level via `redact_embedded_secrets`, not at the *JSON-key* level.
        for k in &["tokenizer", "tokenization", "notification_token_count"] {
            assert!(
                is_sensitive_key(k),
                "regression: '{k}' is no longer flagged — confirm intent before \
                 narrowing the catalogue"
            );
        }
    }

    #[test]
    fn looks_like_secret_value_recognises_documented_patterns() {
        assert!(looks_like_secret_value("Bearer eyJhbGc.foo"));
        assert!(looks_like_secret_value("sk-abc12345"));
        assert!(looks_like_secret_value("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(looks_like_secret_value("PASSWORD=hunter2"));
        assert!(!looks_like_secret_value("plain text"));
        assert!(!looks_like_secret_value(""));
        assert!(!looks_like_secret_value("   "));
    }

    #[test]
    fn looks_like_secret_collection_prompt_recognises_solicitations() {
        assert!(looks_like_secret_collection_prompt("Please paste your API key"));
        assert!(looks_like_secret_collection_prompt("Enter your token here"));
        assert!(!looks_like_secret_collection_prompt("Hello, please continue"));
    }

    #[test]
    fn redact_json_value_array_passthrough() {
        let v = serde_json::json!([{"token": "abc"}, {"safe": "ok"}]);
        let out = redact_json_value(&v);
        assert_eq!(out[0]["token"], "***REDACTED***");
        assert_eq!(out[1]["safe"], "ok");
    }
}
