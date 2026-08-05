//! Gateway-owned network host contract — install-time detection and runtime checks.

use autonoetic_types::capability::Capability;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::runtime::remote_access::extract_host_from_url_literal;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostContractValidation {
    pub detected_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub superfluous_hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostContractValidationError {
    pub error_type: String,
    pub undeclared_hosts: Vec<String>,
    pub suggested_hosts: Vec<String>,
    pub repair_hint: String,
}

impl HostContractValidationError {
    pub fn to_error_response(&self) -> String {
        serde_json::json!({
            "ok": false,
            "error_type": "validation",
            "error": self.error_type,
            "message": format!(
                "NetworkAccess capability hosts do not cover all network targets detected in the artifact. \
                 Undeclared hosts: {:?}.",
                self.undeclared_hosts
            ),
            "undeclared_hosts": self.undeclared_hosts,
            "suggested_hosts": self.suggested_hosts,
            "repair_hint": self.repair_hint,
        })
        .to_string()
    }
}

/// Check whether a declared NetworkAccess host pattern covers a detected host.
pub fn capability_host_allows(declared: &str, host: &str) -> bool {
    let declared = declared.trim().to_ascii_lowercase();
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if declared == "*" {
        return true;
    }
    if declared.is_empty() || host.is_empty() {
        return false;
    }
    if host == declared {
        return true;
    }
    if !declared.contains('*') {
        return host.ends_with(&format!(".{}", declared));
    }
    wildcard_match(&declared, &host)
}

pub fn declared_network_hosts(capabilities: &[Capability]) -> Vec<String> {
    capabilities
        .iter()
        .filter_map(|cap| match cap {
            Capability::NetworkAccess { hosts } => Some(hosts.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

pub fn detect_network_hosts_from_file_map(file_map: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    let mut detected_hosts: BTreeSet<String> = BTreeSet::new();
    for (path, bytes) in file_map {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !matches!(
            ext,
            "py" | "js" | "ts" | "rs" | "go" | "java" | "kt" | "swift" | "c" | "cpp" | "h"
                | "sh" | "bash" | "yaml" | "yml" | "json" | "toml"
        ) {
            continue;
        }
        let Ok(code) = std::str::from_utf8(bytes) else {
            continue;
        };
        let analysis = crate::runtime::remote_access::default_remote_access_detector()
            .analyze_code(code);
        for pattern in &analysis.detected_patterns {
            match pattern.category {
                crate::runtime::remote_access::DetectedPatternCategory::UrlLiteral => {
                    if let Some(host) = extract_host_from_url_literal(&pattern.pattern) {
                        if !host.is_empty()
                            && host != "localhost"
                            && !host.ends_with(".localhost")
                        {
                            detected_hosts.insert(host);
                        }
                    }
                }
                crate::runtime::remote_access::DetectedPatternCategory::IpAddress => {
                    let host = pattern.pattern.trim().to_ascii_lowercase();
                    if !host.is_empty() && !host.starts_with("127.") && host != "0.0.0.0" {
                        detected_hosts.insert(host);
                    }
                }
                crate::runtime::remote_access::DetectedPatternCategory::HostConstant => {
                    let host = pattern
                        .pattern
                        .trim()
                        .trim_end_matches('.')
                        .to_ascii_lowercase();
                    if !host.is_empty() && host != "localhost" && !host.ends_with(".localhost") {
                        detected_hosts.insert(host);
                    }
                }
                _ => {}
            }
        }
    }
    detected_hosts.into_iter().collect()
}

/// Returns true when the manifest's NetworkAccess hosts cover `host`.
pub fn manifest_network_hosts_cover(manifest: &autonoetic_types::agent::AgentManifest, host: &str) -> bool {
    let declared = declared_network_hosts(&manifest.capabilities);
    if declared.is_empty() {
        return false;
    }
    declared
        .iter()
        .any(|pattern| capability_host_allows(pattern, host))
}

/// Runtime check: NetworkAccess capability covers `host`, honoring `open_web` for wildcards.
pub fn network_access_allows_host(
    manifest: &autonoetic_types::agent::AgentManifest,
    host: &str,
) -> bool {
    let declared = declared_network_hosts(&manifest.capabilities);
    if declared.is_empty() {
        return false;
    }
    if declared.iter().any(|h| h.trim() == "*") {
        return manifest.open_web;
    }
    manifest_network_hosts_cover(manifest, host)
}

/// Grant drift: host is outside the gateway-persisted install-time contract.
pub fn host_outside_revision_contract(
    detected_network_hosts: Option<&[String]>,
    host: &str,
) -> bool {
    let Some(contract_hosts) = detected_network_hosts else {
        return false;
    };
    if contract_hosts.is_empty() {
        return false;
    }
    let normalized = host.trim().trim_end_matches('.').to_ascii_lowercase();
    !contract_hosts
        .iter()
        .any(|contract| capability_host_allows(contract, &normalized))
}

pub fn validate_network_host_contract(
    capabilities: &[Capability],
    file_map: &BTreeMap<String, Vec<u8>>,
    open_web: bool,
) -> Result<HostContractValidation, HostContractValidationError> {
    let detected_hosts = detect_network_hosts_from_file_map(file_map);
    let declared_patterns = declared_network_hosts(capabilities);
    let has_universal_wildcard = declared_patterns.iter().any(|h| h.trim() == "*");

    if has_universal_wildcard && !open_web {
        return Err(HostContractValidationError {
            error_type: "undeclared_hosts".to_string(),
            undeclared_hosts: detected_hosts.clone(),
            suggested_hosts: detected_hosts.clone(),
            repair_hint: "Add specific hosts to NetworkAccess, or declare open_web: true for genuine open-web agents.".to_string(),
        });
    }

    let undeclared: Vec<String> = detected_hosts
        .iter()
        .filter(|host| {
            !declared_patterns
                .iter()
                .any(|declared| capability_host_allows(declared, host))
        })
        .cloned()
        .collect();

    if !undeclared.is_empty() {
        return Err(HostContractValidationError {
            error_type: "undeclared_hosts".to_string(),
            undeclared_hosts: undeclared.clone(),
            suggested_hosts: detected_hosts.clone(),
            repair_hint: "Add these hosts to NetworkAccess, or declare open_web: true for genuine open-web agents.".to_string(),
        });
    }

    let detected_set: BTreeSet<String> = detected_hosts.iter().cloned().collect();
    let superfluous_hosts: Vec<String> = declared_patterns
        .iter()
        .filter(|declared| {
            let declared = declared.trim();
            if declared.is_empty() || declared == "*" {
                return false;
            }
            !detected_set
                .iter()
                .any(|host| capability_host_allows(declared, host))
        })
        .cloned()
        .collect();

    if !superfluous_hosts.is_empty() {
        tracing::warn!(
            target: "host_contract",
            superfluous_hosts = ?superfluous_hosts,
            "Declared NetworkAccess hosts were not detected in artifact source"
        );
    }

    Ok(HostContractValidation {
        detected_hosts,
        superfluous_hosts,
    })
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return value.ends_with(suffix);
    }
    pattern == value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_host_allows_exact_and_subdomain() {
        assert!(capability_host_allows("api.example.com", "api.example.com"));
        assert!(capability_host_allows("example.com", "api.example.com"));
        assert!(!capability_host_allows("api.example.com", "other.example.com"));
    }

    #[test]
    fn rejects_wildcard_without_open_web() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "main.py".to_string(),
            b"import urllib.request\nurllib.request.urlopen('https://api.open-meteo.com/v1')".to_vec(),
        );
        let caps = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }];
        let err = validate_network_host_contract(&caps, &file_map, false).unwrap_err();
        assert_eq!(err.error_type, "undeclared_hosts");
        assert!(err.suggested_hosts.iter().any(|h| h.contains("open-meteo")));
    }

    #[test]
    fn accepts_specific_hosts() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "weather.py".to_string(),
            b"GEOCODING='https://geocoding-api.open-meteo.com/v1/search'\nFORECAST='https://api.open-meteo.com/v1/forecast'".to_vec(),
        );
        let caps = vec![Capability::NetworkAccess {
            hosts: vec![
                "api.open-meteo.com".to_string(),
                "geocoding-api.open-meteo.com".to_string(),
            ],
        }];
        let result = validate_network_host_contract(&caps, &file_map, false).unwrap();
        assert_eq!(result.detected_hosts.len(), 2);
    }
}
