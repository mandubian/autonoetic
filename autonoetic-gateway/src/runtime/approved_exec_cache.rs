//! Approved Sandbox Exec Replay Cache.
//!
//! Caches approved sandbox.exec fingerprints so identical future executions
//! skip creating new approval requests.
//!
//! Cache key = SHA256(agent_id + sorted_targets + identity + capability digest)
//!
//! Identity is:
//! - `artifact:<artifact_id>` when an artifact_id is provided (stable across shell wrappers)
//! - `code:<code_to_analyze>` otherwise (exact code match)
//!
//! The agent's **capability set** is folded into the key (#381): an approval is
//! granted under a specific capability scope, so if those capabilities change
//! (e.g. `NetworkAccess` is widened) the fingerprint changes, the cache misses,
//! and a fresh approval is required — a narrower prior approval can't be silently
//! reused under broader authority. Conservative by design: *any* capability
//! change yields a new fingerprint (re-approval is cheap; a stale reuse is not).
//!
//! Reuse eligibility is determined by `NetworkCoverage` classification:
//! - `Concrete`: cache/session-grant/approved-request reuse allowed
//! - `Unresolved`: skip reuse (network behavior present but targets unknown)
//! - `None`: no approval needed

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::runtime::remote_access::{DetectedPattern, DetectedPatternCategory};

/// A cached approved sandbox exec entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApprovedExecEntry {
    /// Unique cache key (SHA256 fingerprint).
    pub fingerprint: String,
    /// The agent that was approved.
    pub agent_id: String,
    /// Concrete remote targets extracted from code (sorted, deduplicated).
    pub remote_targets: Vec<String>,
    /// The analyzed code content that was approved.
    pub code_content: String,
    /// The original approval request ID.
    pub approval_request_id: String,
    /// ISO timestamp when approval was granted.
    pub approved_at: String,
    /// Who approved (typically "operator").
    pub approved_by: String,
    /// ISO timestamp of last successful use.
    pub last_used_at: String,
}

/// Thread-safe cache for approved sandbox exec fingerprints.
pub struct ApprovedExecCache {
    cache_path: std::path::PathBuf,
    entries: Arc<Mutex<HashMap<String, ApprovedExecEntry>>>,
}

/// Data needed to backfill the approved-exec cache when a sandbox exec gate is
/// cleared without a cache hit (e.g. by session grant or approval_ref).
#[derive(Debug, Clone)]
pub struct ApprovedExecCacheBackfill {
    pub gateway_dir: std::path::PathBuf,
    pub fingerprint: String,
    pub agent_id: String,
    pub remote_targets: Vec<String>,
    pub code_content: String,
    pub approval_request_id: String,
}

impl ApprovedExecCacheBackfill {
    /// Records the entry if no entry with the same fingerprint already exists.
    /// This is the single implementation of exec-cache backfill; both the
    /// `GateService` clearance path and the `sandbox_exec` approval_ref
    /// validation path route through it.
    pub fn record_if_missing(&self) -> anyhow::Result<()> {
        let cache = ApprovedExecCache::new(&self.gateway_dir)?;
        if cache.find(&self.fingerprint).is_some() {
            return Ok(());
        }
        let entry = ApprovedExecEntry {
            fingerprint: self.fingerprint.clone(),
            agent_id: self.agent_id.clone(),
            remote_targets: self.remote_targets.clone(),
            code_content: self.code_content.clone(),
            approval_request_id: self.approval_request_id.clone(),
            approved_at: chrono::Utc::now().to_rfc3339(),
            approved_by: "operator".to_string(),
            last_used_at: chrono::Utc::now().to_rfc3339(),
        };
        cache.record(entry)
    }
}

impl ApprovedExecCache {
    /// Creates a new ApprovedExecCache, loading existing entries from disk.
    pub fn new(gateway_dir: &Path) -> anyhow::Result<Self> {
        let cache_dir = gateway_dir
            .join("scheduler")
            .join("approvals")
            .join("exec_cache");
        let cache_path = cache_dir.join("index.json");

        let entries = if cache_path.exists() {
            let json = std::fs::read_to_string(&cache_path)?;
            let entries: HashMap<String, ApprovedExecEntry> = serde_json::from_str(&json)?;
            tracing::info!(
                target: "approved_exec_cache",
                path = %cache_path.display(),
                count = entries.len(),
                "Loaded existing approved exec cache"
            );
            entries
        } else {
            HashMap::new()
        };

        Ok(Self {
            cache_path,
            entries: Arc::new(Mutex::new(entries)),
        })
    }

    /// Records a new approved exec entry.
    pub fn record(&self, entry: ApprovedExecEntry) -> anyhow::Result<()> {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(entry.fingerprint.clone(), entry);
        self.flush(&entries)?;
        Ok(())
    }

    /// Looks up an entry by fingerprint.
    pub fn find(&self, fingerprint: &str) -> Option<ApprovedExecEntry> {
        let entries = self.entries.lock().unwrap();
        entries.get(fingerprint).cloned()
    }

    /// Returns all cached entries (cloned), sorted by `approved_at`. For
    /// operator inspection / revocation tooling (#380).
    pub fn all(&self) -> Vec<ApprovedExecEntry> {
        let entries = self.entries.lock().unwrap();
        let mut out: Vec<ApprovedExecEntry> = entries.values().cloned().collect();
        out.sort_by(|a, b| a.approved_at.cmp(&b.approved_at));
        out
    }

    /// Removes a cached approval by fingerprint, persisting the change. Returns
    /// `true` if an entry was removed (#380). Revoking forces the next matching
    /// exec back through approval.
    pub fn remove(&self, fingerprint: &str) -> anyhow::Result<bool> {
        let mut entries = self.entries.lock().unwrap();
        let removed = entries.remove(fingerprint).is_some();
        if removed {
            self.flush(&entries)?;
        }
        Ok(removed)
    }

    /// Removes all cached approvals, persisting the change. Returns the number
    /// removed (#380).
    pub fn clear(&self) -> anyhow::Result<usize> {
        let mut entries = self.entries.lock().unwrap();
        let n = entries.len();
        if n > 0 {
            entries.clear();
            self.flush(&entries)?;
        }
        Ok(n)
    }

    /// Updates the last_used_at timestamp for an entry.
    pub fn update_last_used(&self, fingerprint: &str) -> anyhow::Result<()> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(fingerprint) {
            entry.last_used_at = chrono::Utc::now().to_rfc3339();
            self.flush(&entries)?;
        }
        Ok(())
    }

    /// Returns the number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    fn flush(&self, entries: &HashMap<String, ApprovedExecEntry>) -> anyhow::Result<()> {
        if let Some(parent) = self.cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(entries)?;
        // Atomic write: write a temp file then rename, so a crash or a concurrent
        // reader never sees a torn/partial index. (Full cross-process
        // lost-update serialization is a separate follow-up — see #380.)
        let tmp = self.cache_path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.cache_path)?;
        Ok(())
    }
}

/// Extracts concrete host targets from detected patterns and normalizes them.
///
/// For URL literals: extracts the host (e.g., "https://api.example.com/path" → "api.example.com")
/// For IP addresses: uses the IP as-is (e.g., "192.168.1.100")
/// For host constants (a `HOST = "imap.gmail.com"` constant passed to a network
/// sink): uses the resolved hostname as-is.
///
/// Returns sorted, deduplicated list of hosts.
pub fn normalize_targets(patterns: &[DetectedPattern]) -> Vec<String> {
    let mut hosts = Vec::new();

    for pattern in patterns {
        match pattern.category {
            crate::runtime::remote_access::DetectedPatternCategory::UrlLiteral => {
                // Extract host from URL literal
                if let Some(host) = extract_host_from_url(&pattern.pattern) {
                    if !hosts.contains(&host) {
                        hosts.push(host);
                    }
                }
            }
            crate::runtime::remote_access::DetectedPatternCategory::IpAddress => {
                // IP address is already a host
                if !hosts.contains(&pattern.pattern) {
                    hosts.push(pattern.pattern.clone());
                }
            }
            crate::runtime::remote_access::DetectedPatternCategory::HostConstant => {
                // Resolved hostname constant — already normalized lowercase by
                // the resolver; trim defensively.
                let host = pattern.pattern.trim().trim_end_matches('.').to_ascii_lowercase();
                if !host.is_empty() && !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
            _ => {
                // Skip non-concrete patterns
            }
        }
    }

    hosts.sort();
    hosts
}

/// Computes the fingerprint for a sandbox exec request.
///
/// Fingerprint = SHA256(agent_id + "|" + sorted_targets + "|" + identity)
///
/// When `artifact_canonical_digest` is provided, identity is `artifact:<canonical_digest>` —
/// this makes the fingerprint stable across different shell wrappers for the same artifact
/// closure (canonical digest is stable across nodes/tenants for the same logical content).
///
/// When absent, identity is `code:<code_to_analyze>` — exact code match.
pub fn compute_fingerprint(
    agent_id: &str,
    targets: &[String],
    code_to_analyze: &str,
    artifact_canonical_digest: Option<&str>,
    capabilities: &[autonoetic_types::capability::Capability],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(agent_id.as_bytes());
    hasher.update(b"|");
    hasher.update(targets.join(",").as_bytes());
    hasher.update(b"|");
    if let Some(canonical) = artifact_canonical_digest {
        hasher.update(b"artifact:");
        hasher.update(canonical.as_bytes());
    } else {
        hasher.update(b"code:");
        hasher.update(code_to_analyze.as_bytes());
    }
    // Bind the approval to the capability scope it was granted under (#381).
    hasher.update(b"|caps:");
    hasher.update(capability_digest(capabilities).as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// Stable digest of an agent's capability set, order-independent. Folded into
/// the exec-cache fingerprint so a capability change forces re-approval (#381).
pub fn capability_digest(capabilities: &[autonoetic_types::capability::Capability]) -> String {
    let mut rendered: Vec<String> = capabilities
        .iter()
        // Debug fallback (never collapse to empty) if serialization ever fails,
        // so a serde error can't silently weaken the digest.
        .map(|c| serde_json::to_string(c).unwrap_or_else(|_| format!("{c:?}")))
        .collect();
    rendered.sort();
    let mut hasher = Sha256::new();
    // Length-prefix each entry instead of a `|` delimiter: capability fields are
    // arbitrary strings, so a delimiter inside one could make two different
    // capability lists hash identically (e.g. ["a","b|"] vs ["a|b",""]). A u64
    // length prefix is unambiguous.
    for r in &rendered {
        hasher.update((r.len() as u64).to_le_bytes());
        hasher.update(r.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Extracts the host from a URL literal using regex.
///
/// Examples:
/// - "https://api.example.com/v1/forecast" → "api.example.com"
/// - "http://192.168.1.1:8080/api" → "192.168.1.1"
fn extract_host_from_url(url: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?i)^[a-z]+://([^/:]+)").ok()?;
    let captures = re.captures(url)?;
    let host = captures.get(1)?.as_str();
    if host.is_empty() {
        None
    } else {
        Some(host.trim_end_matches('.').to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_targets_urls() {
        let patterns = vec![
            DetectedPattern {
                category: DetectedPatternCategory::UrlLiteral,
                pattern: "https://api.example.com/v1/data".to_string(),
                line_number: Some(1),
                reason: "URL literal".to_string(),
            },
            DetectedPattern {
                category: DetectedPatternCategory::UrlLiteral,
                pattern: "https://status.github.com/api".to_string(),
                line_number: Some(2),
                reason: "URL literal".to_string(),
            },
        ];
        let targets = normalize_targets(&patterns);
        assert_eq!(targets, vec!["api.example.com", "status.github.com"]);
    }

    #[test]
    fn test_normalize_targets_dedup() {
        let patterns = vec![
            DetectedPattern {
                category: DetectedPatternCategory::UrlLiteral,
                pattern: "https://api.example.com/v1".to_string(),
                line_number: Some(1),
                reason: "URL literal".to_string(),
            },
            DetectedPattern {
                category: DetectedPatternCategory::UrlLiteral,
                pattern: "https://api.example.com/v2".to_string(),
                line_number: Some(2),
                reason: "URL literal".to_string(),
            },
        ];
        let targets = normalize_targets(&patterns);
        assert_eq!(targets, vec!["api.example.com"]);
    }

    #[test]
    fn test_normalize_targets_ip() {
        let patterns = vec![DetectedPattern {
            category: DetectedPatternCategory::IpAddress,
            pattern: "192.168.1.100".to_string(),
            line_number: Some(1),
            reason: "IP address".to_string(),
        }];
        let targets = normalize_targets(&patterns);
        assert_eq!(targets, vec!["192.168.1.100"]);
    }

    #[test]
    fn test_normalize_targets_skips_imports() {
        let patterns = vec![
            DetectedPattern {
                category: DetectedPatternCategory::Import,
                pattern: "import requests".to_string(),
                line_number: Some(1),
                reason: "HTTP client".to_string(),
            },
            DetectedPattern {
                category: DetectedPatternCategory::UrlLiteral,
                pattern: "https://api.example.com/data".to_string(),
                line_number: Some(2),
                reason: "URL literal".to_string(),
            },
        ];
        let targets = normalize_targets(&patterns);
        // Only concrete hosts, imports are skipped
        assert_eq!(targets, vec!["api.example.com"]);
    }

    #[test]
    fn test_compute_fingerprint_deterministic() {
        let fp1 = compute_fingerprint("agent.id", &["host.com".to_string()], "code", None, &[]);
        let fp2 = compute_fingerprint("agent.id", &["host.com".to_string()], "code", None, &[]);
        assert_eq!(fp1, fp2);
        assert!(fp1.starts_with("sha256:"));
    }

    #[test]
    fn test_compute_fingerprint_different_agents() {
        let fp1 = compute_fingerprint("agent.a", &["host.com".to_string()], "code", None, &[]);
        let fp2 = compute_fingerprint("agent.b", &["host.com".to_string()], "code", None, &[]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_compute_fingerprint_different_code() {
        let fp1 = compute_fingerprint("agent.id", &["host.com".to_string()], "code_a", None, &[]);
        let fp2 = compute_fingerprint("agent.id", &["host.com".to_string()], "code_b", None, &[]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_compute_fingerprint_different_targets() {
        let fp1 = compute_fingerprint("agent.id", &["host_a.com".to_string()], "code", None, &[]);
        let fp2 = compute_fingerprint("agent.id", &["host_b.com".to_string()], "code", None, &[]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn test_compute_fingerprint_artifact_stable_across_code_changes() {
        let fp1 = compute_fingerprint(
            "agent.id",
            &["wttr.in".to_string()],
            "python3 -c 'old wrapper'",
            Some("art-abc123"),
            &[],        );
        let fp2 = compute_fingerprint(
            "agent.id",
            &["wttr.in".to_string()],
            "python3 /tmp/main.py",
            Some("art-abc123"),
            &[],        );
        assert_eq!(
            fp1, fp2,
            "same artifact_id + targets should produce same fingerprint regardless of code"
        );
    }

    #[test]
    fn test_compute_fingerprint_artifact_differs_from_code() {
        let fp_artifact = compute_fingerprint(
            "agent.id",
            &["host.com".to_string()],
            "code",
            Some("art-123"),
            &[],        );
        let fp_code = compute_fingerprint("agent.id", &["host.com".to_string()], "code", None, &[]);
        assert_ne!(
            fp_artifact, fp_code,
            "artifact and code fingerprints should differ"
        );
    }

    #[test]
    fn test_compute_fingerprint_different_artifacts() {
        let fp1 = compute_fingerprint("agent.id", &["host.com".to_string()], "code", Some("art-1"), &[]);
        let fp2 = compute_fingerprint("agent.id", &["host.com".to_string()], "code", Some("art-2"), &[]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn capability_change_changes_fingerprint() {
        use autonoetic_types::capability::Capability;
        let narrow = vec![Capability::NetworkAccess {
            hosts: vec!["api.open-meteo.com".to_string()],
        }];
        let wide = vec![Capability::NetworkAccess {
            hosts: vec!["api.open-meteo.com".to_string(), "evil.example.com".to_string()],
        }];
        let fp_narrow =
            compute_fingerprint("agent.id", &["api.open-meteo.com".to_string()], "code", None, &narrow);
        let fp_wide =
            compute_fingerprint("agent.id", &["api.open-meteo.com".to_string()], "code", None, &wide);
        // #381: widening NetworkAccess must change the fingerprint so a prior
        // (narrower) approval is not silently reused → cache miss → re-approval.
        assert_ne!(fp_narrow, fp_wide);
        // …but the same capability set is stable (deterministic reuse).
        let fp_narrow_again =
            compute_fingerprint("agent.id", &["api.open-meteo.com".to_string()], "code", None, &narrow);
        assert_eq!(fp_narrow, fp_narrow_again);
    }

    #[test]
    fn capability_digest_is_order_independent() {
        use autonoetic_types::capability::Capability;
        let net = Capability::NetworkAccess { hosts: vec!["x".to_string()] };
        let exec = Capability::CodeExecution { patterns: vec!["*".to_string()], commands: vec![] };
        assert_eq!(
            capability_digest(&[net.clone(), exec.clone()]),
            capability_digest(&[exec, net]),
            "digest must not depend on capability ordering"
        );
    }

    #[test]
    fn test_extract_host_from_url() {
        assert_eq!(
            extract_host_from_url("https://api.example.com/v1/forecast"),
            Some("api.example.com".to_string())
        );
        assert_eq!(
            extract_host_from_url("http://192.168.1.1:8080/api"),
            Some("192.168.1.1".to_string())
        );
        assert_eq!(
            extract_host_from_url("https://status.github.com"),
            Some("status.github.com".to_string())
        );
    }
}
