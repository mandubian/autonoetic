//! Log redaction helpers to avoid leaking secrets in traces.
//!
//! The [`RedactedPayload`] newtype enforces R+9: redaction happens **before**
//! causal-chain append. Callers must wrap payloads through one of the
//! constructors; raw `serde_json::Value` cannot be passed to
//! [`CausalLogger::log`] or [`CausalLogger::log_durable`].

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

const REDACTED: &str = "***REDACTED***";

#[derive(Debug, Clone)]
pub struct RedactedPayload(Value);

impl RedactedPayload {
    pub fn from_raw(value: Value) -> Self {
        Self(redact_json_value(&value))
    }

    pub fn from_raw_str(text: &str) -> Self {
        let redacted = redact_text_for_logs(text);
        Self(
            serde_json::from_str(&redacted).unwrap_or(Value::String(redacted)),
        )
    }

    pub fn from_redacted(value: Value) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> Value {
        self.0
    }

    pub fn as_inner(&self) -> &Value {
        &self.0
    }
}

impl From<Value> for RedactedPayload {
    fn from(value: Value) -> Self {
        Self::from_raw(value)
    }
}

static ENV_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b([A-Z][A-Z0-9_]*(?:API[_-]?KEY|TOKEN|SECRET|PASSWORD|ACCESS[_-]?KEY|ACCESS[_-]?TOKEN|REFRESH[_-]?TOKEN|AUTHORIZATION)[A-Z0-9_]*)=("[^"]*"|'[^']*'|[^\s&;]+)"#,
    )
    .expect("valid env assignment redaction regex")
});

static QUERY_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)([?&](?:api[_-]?key|apikey|appid|token|access_token|refresh_token|client_secret|password|secret|authorization)=)([^&#\s]+)",
    )
    .expect("valid query assignment redaction regex")
});

static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(bearer\s+)([^\s,;]+)").expect("valid bearer redaction regex")
});

static SECRET_ENV_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b[A-Z][A-Z0-9_]*(?:API[_-]?KEY|TOKEN|SECRET|PASSWORD|ACCESS[_-]?TOKEN|AUTHORIZATION)[A-Z0-9_]*\s*=\s*\S+",
    )
    .expect("valid secret env assignment regex")
});

static LONG_HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[a-fA-F0-9]{24,}\b").expect("valid long hex regex"));

static JWT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b")
        .expect("valid jwt regex")
});

fn is_sensitive_key(key: &str) -> bool {
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

fn redact_json_value(value: &Value) -> Value {
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
        Value::String(s) => Value::String(redact_embedded_secrets(s)),
        other => other.clone(),
    }
}

fn redact_embedded_secrets(text: &str) -> String {
    let masked_env = ENV_ASSIGN_RE
        .replace_all(text, "${1}=***REDACTED***")
        .to_string();
    let masked_query = QUERY_ASSIGN_RE
        .replace_all(&masked_env, "${1}***REDACTED***")
        .to_string();
    let masked_bearer = BEARER_RE
        .replace_all(&masked_query, "${1}***REDACTED***")
        .to_string();
    if masked_bearer.starts_with("sk-") {
        return REDACTED.to_string();
    }
    masked_bearer
}

/// Detect whether free-form text appears to contain secret material.
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

/// Detect whether a prompt appears to solicit secret input from users.
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

/// Redact potentially sensitive content for structured logging.
pub fn redact_text_for_logs(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => {
            serde_json::to_string(&redact_json_value(&v)).unwrap_or_else(|_| REDACTED.to_string())
        }
        Err(_) => {
            let lower = text.to_ascii_lowercase();
            if lower.contains("token")
                || lower.contains("secret")
                || lower.contains("authorization")
            {
                return REDACTED.to_string();
            }

            let redacted = redact_embedded_secrets(text);
            if redacted != text {
                return redacted;
            }

            // Non-JSON payloads: avoid accidentally dumping long secrets.
            if lower.contains("api_key") || lower.contains("apikey") {
                REDACTED.to_string()
            } else {
                text.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::redact_text_for_logs;

    #[test]
    fn test_redacts_sensitive_json_keys() {
        let input = r#"{"token":"abc","nested":{"client_secret":"xyz"},"safe":"ok"}"#;
        let out = redact_text_for_logs(input);
        assert!(out.contains("***REDACTED***"));
        assert!(out.contains("\"safe\":\"ok\""));
        assert!(!out.contains("\"abc\""));
        assert!(!out.contains("\"xyz\""));
    }

    #[test]
    fn test_redacts_secret_like_plain_text() {
        let out = redact_text_for_logs("Authorization: Bearer very-secret-value");
        assert_eq!(out, "***REDACTED***");
    }

    #[test]
    fn test_redacts_api_key_assignment_in_json_string_value() {
        let input = r#"{"command":"export OPENWEATHER_API_KEY=testplaceholder_not_a_real_key_0000 && python3 /tmp/weather.py"}"#;
        let out = redact_text_for_logs(input);
        assert!(out.contains("OPENWEATHER_API_KEY=***REDACTED***"));
        assert!(!out.contains("testplaceholder_not_a_real_key_0000"));
    }

    #[test]
    fn test_redacts_api_key_in_query_string() {
        let input = "http://api.openweathermap.org/data/2.5/weather?appid=testplaceholder_not_a_real_key_0000&q=Paris";
        let out = redact_text_for_logs(input);
        assert!(out.contains("appid=***REDACTED***"));
        assert!(!out.contains("testplaceholder_not_a_real_key_0000"));
    }

    #[test]
    fn test_detects_secret_like_value() {
        assert!(super::looks_like_secret_value(
            "OPENWEATHER_API_KEY=testplaceholder_not_a_real_key_0000"
        ));
    }

    #[test]
    fn test_detects_secret_collection_prompt() {
        assert!(super::looks_like_secret_collection_prompt(
            "Please paste your API key to continue"
        ));
    }
}
