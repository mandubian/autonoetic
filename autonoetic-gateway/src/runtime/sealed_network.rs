//! Sealed-network sandbox: fixture loader + egress decision (RFC scope 5.2a).
//!
//! When an artifact runs under `sandbox_network: sealed` (or `recording`),
//! the gateway intercepts every outbound HTTP request and routes it to this
//! module. The module decides whether to:
//!
//! - **Allow** the request to go to the live network (only possible in
//!   `normal` mode — sealed never allows live).
//! - **Serve a fixture** (the cached response stored alongside the artifact
//!   bundle in `<artifact-root>/fixtures/<host[-port]>/<METHOD>-<path>.json`).
//! - **Reject as unfixtured** (sealed mode, fixture missing) with a
//!   structured error envelope that names the expected fixture path so
//!   operators / developers can seed it.
//!
//! This module is the **decision layer**. The actual interception mechanism
//! — an HTTP proxy server (scope 5.2b) and its bubblewrap integration
//! (deferred 5.2c) — calls into this module for every request it sees.
//!
//! Refs: docs/archived/sealed-network-evaluation-plan.md §3.2 / §3.2.1.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use autonoetic_types::agent::SandboxNetworkPolicy;

/// A canned HTTP response loaded from a fixture file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FixtureResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Response body. Stored as UTF-8 string for readability; binary
    /// bodies should be base64-encoded by convention (callers handle).
    #[serde(default)]
    pub body: String,
}

/// Loader that maps `(host, port, method, path)` → fixture file under the
/// artifact root. Pure-function lookup; no caching, no side effects.
pub struct FixtureLoader {
    artifact_root: PathBuf,
}

impl FixtureLoader {
    pub fn new(artifact_root: impl Into<PathBuf>) -> Self {
        Self {
            artifact_root: artifact_root.into(),
        }
    }

    /// Resolve and read the fixture for `(host, port, method, path)`.
    ///
    /// Returns `Ok(Some(_))` on hit, `Ok(None)` on miss, `Err(_)` only on
    /// IO/parse errors that the caller should surface as
    /// `unfixtured_target` (corrupt fixture = miss with diagnostic).
    pub fn load(
        &self,
        host: &str,
        port: Option<u16>,
        method: &str,
        path: &str,
    ) -> anyhow::Result<Option<FixtureResponse>> {
        let fixture_path = self.fixture_path_for(host, port, method, path);
        match fs::read(&fixture_path) {
            Ok(bytes) => {
                let response: FixtureResponse =
                    serde_json::from_slice(&bytes).map_err(|e| {
                        anyhow::anyhow!(
                            "fixture file '{}' is not valid JSON: {}",
                            fixture_path.display(),
                            e
                        )
                    })?;
                Ok(Some(response))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!(
                "failed to read fixture '{}': {}",
                fixture_path.display(),
                e
            )),
        }
    }

    /// Compute the fixture file path for `(host, port, method, path)`.
    ///
    /// Layout:
    ///   `<artifact_root>/fixtures/<host[-port]>/<METHOD>-<encoded-path>.json`
    ///
    /// Path encoding rules (URL-safe, filesystem-safe):
    /// - `/` → `-`
    /// - leading `/` is stripped
    /// - empty path → `root`
    /// - query string included verbatim (with `?` and `&` left intact —
    ///   filesystem-safe on linux/mac; operators should canonicalise on
    ///   case-insensitive filesystems)
    pub fn fixture_path_for(
        &self,
        host: &str,
        port: Option<u16>,
        method: &str,
        path: &str,
    ) -> PathBuf {
        let host_dir = match port {
            Some(p) => format!("{}-{}", host, p),
            None => host.to_string(),
        };
        let trimmed = path.trim_start_matches('/');
        let encoded_path = if trimmed.is_empty() {
            "root".to_string()
        } else {
            trimmed.replace('/', "-")
        };
        let filename = format!("{}-{}.json", method.to_uppercase(), encoded_path);
        self.artifact_root
            .join("fixtures")
            .join(host_dir)
            .join(filename)
    }
}

/// What to do with an outbound request from a sealed/recording session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressDecision {
    /// Pass the request to the live network (only possible in `Normal` mode,
    /// or in `Recording` mode on a fixture miss — the latter handled by 5.3).
    Allow,
    /// Return the canned response in place of contacting the live host.
    Fixture(FixtureResponse),
    /// Sealed mode + fixture miss. Reject with a structured envelope so the
    /// artifact sees a clean error and developers / operators can seed the
    /// missing fixture.
    Unfixtured {
        /// The path on disk where the fixture is expected to live, expressed
        /// as a string for inclusion in the error envelope and causal event.
        expected_path: String,
    },
}

/// Compute the egress decision for one request.
///
/// `policy` is the agent manifest's declared `sandbox_network`. `loader`
/// is `Some(_)` for sealed/recording sessions and `None` for normal.
/// (The caller is responsible for constructing a loader rooted at the
/// artifact root.)
pub fn decide_egress(
    policy: SandboxNetworkPolicy,
    loader: Option<&FixtureLoader>,
    host: &str,
    port: Option<u16>,
    method: &str,
    path: &str,
) -> anyhow::Result<EgressDecision> {
    match policy {
        SandboxNetworkPolicy::Normal => Ok(EgressDecision::Allow),
        SandboxNetworkPolicy::Sealed | SandboxNetworkPolicy::Recording => {
            let Some(loader) = loader else {
                anyhow::bail!(
                    "sealed/recording mode requires a FixtureLoader; \
                     gateway must pass one for {:?} sessions",
                    policy
                );
            };
            match loader.load(host, port, method, path)? {
                Some(response) => Ok(EgressDecision::Fixture(response)),
                None => {
                    // For Recording (5.3), caller would fall back to live
                    // capture here. For Sealed, it's a hard miss. Caller
                    // distinguishes by inspecting the policy itself.
                    let expected = loader
                        .fixture_path_for(host, port, method, path)
                        .display()
                        .to_string();
                    Ok(EgressDecision::Unfixtured {
                        expected_path: expected,
                    })
                }
            }
        }
    }
}

/// Helper: a `host:port` string parses into `(host, Option<port>)`. Useful
/// when the egress layer receives a `Host:` header or `CONNECT` target.
pub fn parse_host_port(authority: &str) -> (String, Option<u16>) {
    if let Some((host, port_str)) = authority.rsplit_once(':') {
        if let Ok(port) = port_str.parse::<u16>() {
            return (host.to_string(), Some(port));
        }
    }
    (authority.to_string(), None)
}

/// Structured envelope returned to the artifact on a sealed miss. Matches
/// the gateway's `ToolError`-shape error envelopes so downstream consumers
/// (auditor, evaluator findings) can parse it uniformly.
pub fn unfixtured_envelope_body(
    host: &str,
    port: Option<u16>,
    method: &str,
    path: &str,
    expected_fixture_path: &str,
) -> String {
    let target = match port {
        Some(p) => format!("{}://{}:{}{}", "http", host, p, path),
        None => format!("{}://{}{}", "http", host, path),
    };
    serde_json::json!({
        "ok": false,
        "error_type": "unfixtured_target",
        "message": format!(
            "Sealed-network sandbox: outbound request to {} {} has no fixture. \
             Expected fixture at: {}",
            method.to_uppercase(),
            target,
            expected_fixture_path
        ),
        "method": method.to_uppercase(),
        "host": host,
        "port": port,
        "path": path,
        "expected_fixture_path": expected_fixture_path,
    })
    .to_string()
}

/// A full recorded HTTP round-trip saved during recording mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureRecord {
    pub request: RecordedRequest,
    pub response: RecordedResponse,
    pub recorded_at: String,
    #[serde(default)]
    pub redacted: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: String,
}

/// Header names whose values are redacted in recorded fixtures.
const SENSITIVE_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "x-api-key",
    "x-api-key",
    "proxy-authorization",
];

/// Header names whose values are redacted in recorded response fixtures.
const SENSITIVE_RESPONSE_HEADERS: &[&str] = &[
    "set-cookie",
    "www-authenticate",
    "proxy-authenticate",
];

/// Query parameter names whose values are redacted.
const SENSITIVE_QUERY_PARAMS: &[&str] = &[
    "token",
    "api_key",
    "apikey",
    "secret",
    "key",
    "password",
    "auth",
    "signature",
    "access_token",
    "refresh_token",
];

/// Redact sensitive values from a recorded fixture in-place.
///
/// Returns the list of field names that were redacted.
pub fn redact_fixture(record: &mut FixtureRecord) -> Vec<String> {
    let mut redacted_fields = Vec::new();

    // Redact sensitive request headers.
    for header_name in SENSITIVE_REQUEST_HEADERS {
        if let Some(value) = record.request.headers.get_mut(*header_name) {
            if !value.is_empty() && value != "[REDACTED]" {
                *value = "[REDACTED]".to_string();
                redacted_fields.push(header_name.to_string());
            }
        }
    }

    // Redact sensitive response headers.
    for header_name in SENSITIVE_RESPONSE_HEADERS {
        if let Some(value) = record.response.headers.get_mut(*header_name) {
            if !value.is_empty() && value != "[REDACTED]" {
                *value = "[REDACTED]".to_string();
                redacted_fields.push(header_name.to_string());
            }
        }
    }

    // Redact sensitive query parameters from the request URL.
    if let Some(query_start) = record.request.url.find('?') {
        let (base, query) = record.request.url.split_at(query_start);
        let params: Vec<String> = query[1..]
            .split('&')
            .map(|pair| {
                if let Some((key, _value)) = pair.split_once('=') {
                    if SENSITIVE_QUERY_PARAMS
                        .iter()
                        .any(|sensitive| key.eq_ignore_ascii_case(sensitive))
                    {
                        if !redacted_fields.contains(&format!("query_{}", key)) {
                            redacted_fields.push(format!("query_{}", key));
                        }
                        format!("{}={}", key, "[REDACTED]")
                    } else {
                        pair.to_string()
                    }
                } else {
                    pair.to_string()
                }
            })
            .collect();
        record.request.url = format!("{}?{}", base, params.join("&"));
    }

    // Redact Bearer tokens in Authorization header value copies that may be in body.
    let body_redacted = redact_body_bearer(&mut record.response.body);
    if body_redacted && !redacted_fields.contains(&"body_bearer".to_string()) {
        redacted_fields.push("body_bearer".to_string());
    }

    record.redacted = redacted_fields.clone();
    redacted_fields
}

/// Redact Bearer tokens from a response body string.
fn redact_body_bearer(body: &mut String) -> bool {
    let before = body.clone();
    use regex::Regex;
    let re = Regex::new(r"(?i)(bearer\s+)([^\s,;}\]]+)").expect("valid bearer regex");
    *body = re.replace_all(body, "${1}[REDACTED]").to_string();
    before != *body
}

/// Write a recorded fixture to the staging directory.
///
/// Returns the path of the written fixture file.
pub fn write_recording_fixture(
    staging_dir: &Path,
    host: &str,
    port: Option<u16>,
    method: &str,
    path: &str,
    record: &FixtureRecord,
) -> anyhow::Result<PathBuf> {
    let host_dir = match port {
        Some(p) => format!("{}-{}", host, p),
        None => host.to_string(),
    };
    let trimmed = path.trim_start_matches('/');
    let encoded_path = if trimmed.is_empty() {
        "root".to_string()
    } else {
        trimmed.replace('/', "-")
    };
    let filename = format!("{}-{}.json", method.to_uppercase(), encoded_path);

    let fixture_path = staging_dir.join(&host_dir).join(&filename);
    std::fs::create_dir_all(fixture_path.parent().unwrap())?;
    let json = serde_json::to_string_pretty(record)?;
    std::fs::write(&fixture_path, json)?;
    Ok(fixture_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_fixture(root: &Path, rel: &str, body: &str) {
        let p = root.join("fixtures").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn fixture_path_layout_host_only() {
        let loader = FixtureLoader::new("/x");
        let p = loader.fixture_path_for("api.example.com", None, "get", "/v1/widgets");
        assert_eq!(
            p,
            PathBuf::from("/x/fixtures/api.example.com/GET-v1-widgets.json")
        );
    }

    #[test]
    fn fixture_path_layout_host_port() {
        let loader = FixtureLoader::new("/x");
        let p = loader.fixture_path_for("localhost", Some(9876), "POST", "/status");
        assert_eq!(
            p,
            PathBuf::from("/x/fixtures/localhost-9876/POST-status.json")
        );
    }

    #[test]
    fn fixture_path_root_path() {
        let loader = FixtureLoader::new("/x");
        let p = loader.fixture_path_for("a.b", None, "get", "/");
        assert_eq!(p, PathBuf::from("/x/fixtures/a.b/GET-root.json"));
        let p2 = loader.fixture_path_for("a.b", None, "get", "");
        assert_eq!(p2, PathBuf::from("/x/fixtures/a.b/GET-root.json"));
    }

    #[test]
    fn load_returns_none_when_fixture_missing() {
        let dir = tempdir().unwrap();
        let loader = FixtureLoader::new(dir.path());
        assert!(loader
            .load("nowhere.invalid", None, "get", "/")
            .unwrap()
            .is_none());
    }

    #[test]
    fn load_returns_canned_response() {
        let dir = tempdir().unwrap();
        write_fixture(
            dir.path(),
            "api.example.com/GET-v1-widgets.json",
            r#"{"status": 200, "headers": {"Content-Type": "application/json"}, "body": "{\"items\":[]}"}"#,
        );
        let loader = FixtureLoader::new(dir.path());
        let r = loader
            .load("api.example.com", None, "get", "/v1/widgets")
            .unwrap()
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.headers.get("Content-Type").unwrap(), "application/json");
        assert!(r.body.contains("items"));
    }

    #[test]
    fn load_corrupt_fixture_errors() {
        let dir = tempdir().unwrap();
        write_fixture(dir.path(), "a.b/GET-root.json", "not json");
        let loader = FixtureLoader::new(dir.path());
        let err = loader.load("a.b", None, "get", "/").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn decide_normal_always_allows() {
        let d = decide_egress(SandboxNetworkPolicy::Normal, None, "x", None, "GET", "/").unwrap();
        assert_eq!(d, EgressDecision::Allow);
    }

    #[test]
    fn decide_sealed_hit_returns_fixture() {
        let dir = tempdir().unwrap();
        write_fixture(
            dir.path(),
            "x/POST-y.json",
            r#"{"status":201,"headers":{},"body":"ok"}"#,
        );
        let loader = FixtureLoader::new(dir.path());
        let d = decide_egress(SandboxNetworkPolicy::Sealed, Some(&loader), "x", None, "POST", "/y")
            .unwrap();
        match d {
            EgressDecision::Fixture(r) => {
                assert_eq!(r.status, 201);
                assert_eq!(r.body, "ok");
            }
            other => panic!("expected Fixture, got {:?}", other),
        }
    }

    #[test]
    fn decide_sealed_miss_returns_unfixtured_with_expected_path() {
        let dir = tempdir().unwrap();
        let loader = FixtureLoader::new(dir.path());
        let d = decide_egress(
            SandboxNetworkPolicy::Sealed,
            Some(&loader),
            "missing.example.com",
            Some(443),
            "GET",
            "/v2/items",
        )
        .unwrap();
        match d {
            EgressDecision::Unfixtured { expected_path } => {
                assert!(expected_path.contains("missing.example.com-443"));
                assert!(expected_path.contains("GET-v2-items.json"));
            }
            other => panic!("expected Unfixtured, got {:?}", other),
        }
    }

    #[test]
    fn decide_sealed_without_loader_is_a_caller_bug() {
        let err = decide_egress(SandboxNetworkPolicy::Sealed, None, "x", None, "GET", "/")
            .unwrap_err();
        assert!(err.to_string().contains("requires a FixtureLoader"));
    }

    #[test]
    fn parse_host_port_with_and_without_port() {
        assert_eq!(
            parse_host_port("localhost:9876"),
            ("localhost".to_string(), Some(9876))
        );
        assert_eq!(
            parse_host_port("api.example.com"),
            ("api.example.com".to_string(), None)
        );
        // Bracketed IPv6 not yet supported — document by failing-test
        // intent: future work.
        assert_eq!(
            parse_host_port("[::1]:8080"),
            ("[::1]".to_string(), Some(8080))
        );
    }

    #[test]
    fn unfixtured_envelope_shape() {
        let body = unfixtured_envelope_body("api.example.com", None, "POST", "/v1/echo", "/x/fixtures/api.example.com/POST-v1-echo.json");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error_type"], "unfixtured_target");
        assert_eq!(parsed["method"], "POST");
        assert_eq!(parsed["host"], "api.example.com");
        assert!(parsed["message"]
            .as_str()
            .unwrap()
            .contains("Expected fixture at"));
    }
}
