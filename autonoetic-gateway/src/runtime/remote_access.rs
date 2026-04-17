//! Static analysis for detecting remote/network access in code.
//!
//! Analyzes code before execution to detect patterns that require
//! network access (HTTP requests, socket connections, etc.).
//! If detected, the code execution requires operator approval.

use regex::Regex;

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

/// Static analyzer for detecting remote access patterns in code.
pub struct RemoteAccessAnalyzer;

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
        let mut patterns = Vec::new();

        // Check for network-related imports
        patterns.extend(Self::detect_imports(code));

        // Check for network-related function calls
        patterns.extend(Self::detect_function_calls(code));

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

    /// Detects import statements for network libraries.
    fn detect_imports(code: &str) -> Vec<DetectedPattern> {
        let mut patterns = Vec::new();

        // Python import patterns
        let import_patterns = vec![
            ("requests", "HTTP client library"),
            ("urllib", "URL handling library"),
            ("urllib.request", "URL request library"),
            ("urllib3", "HTTP client library"),
            ("httpx", "Async HTTP client library"),
            ("aiohttp", "Async HTTP client library"),
            ("socket", "Low-level networking library"),
            ("ftplib", "FTP client library"),
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
            // Match: import X, from X import, import X as Y
            let import_regex = format!(
                r"(?m)^\s*(?:import\s+{}|from\s+{}\s+import)",
                regex::escape(lib),
                regex::escape(lib)
            );
            if let Ok(re) = Regex::new(&import_regex) {
                for mat in re.find_iter(code) {
                    let line_num = code[..mat.start()].matches('\n').count() + 1;
                    patterns.push(DetectedPattern {
                        category: "import".to_string(),
                        pattern: mat.as_str().trim().to_string(),
                        line_number: Some(line_num),
                        reason: reason.to_string(),
                    });
                }
            }
        }

        patterns
    }

    /// Detects function/method calls for network operations.
    fn detect_function_calls(code: &str) -> Vec<DetectedPattern> {
        let mut patterns = Vec::new();

        let call_patterns = vec![
            (r"\.connect\s*\(", "Socket connection initiation"),
            (r"\.send\s*\(", "Sending data over network"),
            (r"\.recv\s*\(", "Receiving data from network"),
            (r"\.bind\s*\(", "Socket binding"),
            (r"\.listen\s*\(", "Socket listening"),
            (r"\.accept\s*\(", "Socket accept connection"),
            (r"urlopen\s*\(", "Opening URL connection"),
            (
                r"requests\.(get|post|put|delete|patch|head|options)\s*\(",
                "HTTP request",
            ),
            (
                r"httpx\.(get|post|put|delete|patch|head|options)\s*\(",
                "HTTP request",
            ),
            (r"\.get\s*\(.*http", "HTTP GET request"),
            (r"\.post\s*\(.*http", "HTTP POST request"),
            (r"fetch\s*\(", "Fetch API call"),
            (r"WebSocket\s*\(", "WebSocket connection"),
            (r"connect\s*\(.*ws://", "WebSocket connection"),
            (r"connect\s*\(.*wss://", "Secure WebSocket connection"),
        ];

        for (pattern, reason) in &call_patterns {
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
        let mut patterns = Self::analyze_code(code);

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
