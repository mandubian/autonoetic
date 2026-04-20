//! Log redaction helpers to avoid leaking secrets in traces.

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

const REDACTED: &str = "***REDACTED***";

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
        Value::String(s) => {
            Value::String(redact_embedded_secrets(s))
        }
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
            if lower.contains("api_key") || lower.contains("apikey")
            {
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
        let input =
            r#"{"command":"export OPENWEATHER_API_KEY=TEST_PLACEHOLDER_DO_NOT_USE_XXXX && python3 /tmp/weather.py"}"#;
        let out = redact_text_for_logs(input);
        assert!(out.contains("OPENWEATHER_API_KEY=***REDACTED***"));
        assert!(!out.contains("TEST_PLACEHOLDER_DO_NOT_USE_XXXX"));
    }

    #[test]
    fn test_redacts_api_key_in_query_string() {
        let input = "http://api.openweathermap.org/data/2.5/weather?appid=TEST_PLACEHOLDER_DO_NOT_USE_XXXX&q=Paris";
        let out = redact_text_for_logs(input);
        assert!(out.contains("appid=***REDACTED***"));
        assert!(!out.contains("TEST_PLACEHOLDER_DO_NOT_USE_XXXX"));
    }
}
