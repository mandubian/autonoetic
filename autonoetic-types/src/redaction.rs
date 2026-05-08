//! Canonical redaction helpers shared across the workspace.
//!
//! These primitives detect and mask credential-shaped content in arbitrary
//! text or JSON payloads. They are used by:
//!
//! - **`autonoetic-types::causal_chain`** — when applying `redact_for_viewer`
//!   to non-Admin observers of `ExecutionTraceRecord` / `CausalEventRecord`.
//! - **`autonoetic-types::background`** — same, for `ScheduledAction`.
//! - **`autonoetic-gateway::log_redaction`** — when wrapping causal-chain
//!   payloads via `RedactedPayload` (the gateway-side R+9 invariant).
//!
//! Centralising them here removes the prior triplication (one copy in each
//! of those modules) and prevents drift.
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
/// Known gap: hyphenated names like `X-API-Key` do NOT match because the
/// catalogue uses the underscore form `api_key`. Adding `api-key` as an
/// alternative would fix this; left as a future change to keep the rule list
/// auditable.
pub fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("secret")
        || k.contains("token")
        || k.contains("password")
        || k.contains("api_key")
        || k.contains("apikey")
        || k.contains("authorization")
        || k.contains("access_key")
        || k.contains("access_token")
        || k.contains("refresh_token")
        || k.contains("client_secret")
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
    let masked_env = ENV_ASSIGN_RE
        .replace_all(text, "${1}=***REDACTED***")
        .to_string();
    let masked_query = QUERY_ASSIGN_RE
        .replace_all(&masked_env, "${1}***REDACTED***")
        .to_string();
    let masked_bearer = BEARER_RE
        .replace_all(&masked_query, "${1}***REDACTED***")
        .to_string();
    // A bare `sk-…` in the entire string (no JSON context) is treated as a
    // raw secret; we cannot mask just the value without a delimiter.
    if masked_bearer.starts_with("sk-") {
        return REDACTED.to_string();
    }
    masked_bearer
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
