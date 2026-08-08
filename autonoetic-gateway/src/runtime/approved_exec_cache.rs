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

/// TTL for exec-cache lookups: the operator-configured
/// `default_grant_ttl_secs`, or the shared 24h default when the tool ran
/// without gateway config (tests, one-off drivers). Keeping the cache on the
/// grant TTL is the alignment decided under "Gap 3" of the grant-strategy
/// review: the two approval-reuse layers must not have different horizons.
pub fn cache_ttl_secs(config: Option<&autonoetic_types::config::GatewayConfig>) -> u64 {
    config
        .map(|c| c.default_grant_ttl_secs)
        .unwrap_or(autonoetic_types::config::DEFAULT_GRANT_TTL_SECS)
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
    /// The TTL the existing-entry check is evaluated against — the same
    /// `cache_ttl_secs(config)` the gated lookup used. Carried on the backfill
    /// so `record_if_missing` decides for itself rather than inheriting
    /// whatever a prior lookup happened to prune.
    pub ttl_secs: u64,
}

impl ApprovedExecCacheBackfill {
    /// Records the entry unless a *still-valid* entry with the same
    /// fingerprint already exists. This is the single implementation of
    /// exec-cache backfill; both the `GateService` clearance path and the
    /// `sandbox_exec` approval_ref validation path route through it.
    ///
    /// The lookup runs at the backfill's own `ttl_secs`, so a stale entry is
    /// pruned and replaced by the approval that just cleared — otherwise the
    /// fresh approval would be dropped on the floor and the entry would keep
    /// its long-expired `approved_at`, re-prompting the operator on every
    /// subsequent exec.
    pub fn record_if_missing(&self) -> anyhow::Result<()> {
        let cache = ApprovedExecCache::new(&self.gateway_dir)?;
        if cache.find(&self.fingerprint, self.ttl_secs).is_some() {
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

    /// Whether an entry is older than `ttl_secs` (measured from `approved_at`,
    /// mirroring grant expiry which measures from `granted_at`). `ttl_secs ==
    /// 0` disables expiry, matching `default_grant_ttl_secs = 0`. An
    /// unparseable `approved_at` fails closed (treated as expired): the cost
    /// is one re-approval, the benefit is a corrupt entry never mints a
    /// permanent bypass.
    fn is_expired(entry: &ApprovedExecEntry, ttl_secs: u64, now: chrono::DateTime<chrono::Utc>) -> bool {
        if ttl_secs == 0 {
            return false;
        }
        let Ok(approved) = chrono::DateTime::parse_from_rfc3339(&entry.approved_at) else {
            return true;
        };
        let ttl = chrono::Duration::seconds(i64::try_from(ttl_secs).unwrap_or(i64::MAX));
        approved.with_timezone(&chrono::Utc) + ttl <= now
    }

    /// Looks up an entry by fingerprint, honoring `ttl_secs` (aligned with
    /// `GatewayConfig::default_grant_ttl_secs` — the same budget that expires
    /// session grants expires cache entries). An expired entry is pruned and
    /// persisted, so the next matching exec goes back through approval.
    /// Pass `0` for a pure existence check that ignores age.
    pub fn find(&self, fingerprint: &str, ttl_secs: u64) -> Option<ApprovedExecEntry> {
        let now = chrono::Utc::now();
        let expired = {
            let entries = self.entries.lock().unwrap();
            entries
                .get(fingerprint)
                .map(|e| Self::is_expired(e, ttl_secs, now))
        };
        match expired {
            None => None,
            Some(false) => {
                let entries = self.entries.lock().unwrap();
                entries.get(fingerprint).cloned()
            }
            Some(true) => {
                let mut entries = self.entries.lock().unwrap();
                entries.remove(fingerprint);
                if let Err(e) = self.flush(&entries) {
                    tracing::warn!(
                        target: "approved_exec_cache",
                        error = %e,
                        "Failed to persist exec-cache expiry prune"
                    );
                }
                tracing::info!(
                    target: "approved_exec_cache",
                    fingerprint = %fingerprint,
                    ttl_secs,
                    "Approved exec cache entry expired — next exec requires approval"
                );
                None
            }
        }
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

    /// The shipped grant TTL, which is what the gated lookups pass. Expiry
    /// tests derive their ages from it rather than hard-coding 24h, so they
    /// keep testing "past/within the TTL" if the default ever moves.
    const TTL: u64 = autonoetic_types::config::DEFAULT_GRANT_TTL_SECS;

    fn entry_with_age(approved_at: chrono::DateTime<chrono::Utc>) -> ApprovedExecEntry {
        ApprovedExecEntry {
            fingerprint: "sha256:test".to_string(),
            agent_id: "agent.id".to_string(),
            remote_targets: vec!["host.com".to_string()],
            code_content: "code".to_string(),
            approval_request_id: "apr-test".to_string(),
            approved_at: approved_at.to_rfc3339(),
            approved_by: "operator".to_string(),
            last_used_at: approved_at.to_rfc3339(),
        }
    }

    #[test]
    fn find_expires_entries_past_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ApprovedExecCache::new(tmp.path()).unwrap();
        let old = chrono::Utc::now() - chrono::Duration::seconds(TTL as i64 + 3600);
        cache.record(entry_with_age(old)).unwrap();

        // An entry an hour past the TTL is a miss and gets pruned.
        assert!(cache.find("sha256:test", TTL).is_none());
        assert_eq!(cache.len(), 0, "expired entry must be pruned");
    }

    #[test]
    fn find_returns_entries_within_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ApprovedExecCache::new(tmp.path()).unwrap();
        let recent = chrono::Utc::now() - chrono::Duration::seconds(TTL as i64 / 2);
        cache.record(entry_with_age(recent)).unwrap();

        assert!(cache.find("sha256:test", TTL).is_some());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn find_ttl_zero_never_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ApprovedExecCache::new(tmp.path()).unwrap();
        let ancient = chrono::Utc::now() - chrono::Duration::days(365);
        cache.record(entry_with_age(ancient)).unwrap();

        assert!(
            cache.find("sha256:test", 0).is_some(),
            "ttl 0 disables expiry, matching default_grant_ttl_secs = 0"
        );
    }

    #[test]
    fn find_unparseable_approved_at_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ApprovedExecCache::new(tmp.path()).unwrap();
        let mut entry = entry_with_age(chrono::Utc::now());
        entry.approved_at = "not-a-timestamp".to_string();
        cache.record(entry).unwrap();

        assert!(
            cache.find("sha256:test", TTL).is_none(),
            "corrupt timestamp must not mint a permanent bypass"
        );
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_ttl_secs_aligns_with_grant_config() {
        let cfg = autonoetic_types::config::GatewayConfig {
            default_grant_ttl_secs: 3600,
            ..Default::default()
        };
        assert_eq!(cache_ttl_secs(Some(&cfg)), 3600);
        assert_eq!(
            cache_ttl_secs(None),
            autonoetic_types::config::DEFAULT_GRANT_TTL_SECS
        );
    }

    fn backfill_for(dir: &std::path::Path, ttl_secs: u64) -> ApprovedExecCacheBackfill {
        ApprovedExecCacheBackfill {
            gateway_dir: dir.to_path_buf(),
            fingerprint: "sha256:test".to_string(),
            agent_id: "agent.id".to_string(),
            remote_targets: vec!["host.com".to_string()],
            code_content: "code".to_string(),
            approval_request_id: "apr-fresh".to_string(),
            ttl_secs,
        }
    }

    /// A fresh operator approval must replace a stale entry. Skipping the
    /// write would leave the long-expired `approved_at` in place, so the very
    /// next exec would expire it again and re-prompt the operator forever.
    #[test]
    fn backfill_replaces_an_expired_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ApprovedExecCache::new(tmp.path()).unwrap();
        let old = chrono::Utc::now() - chrono::Duration::seconds(TTL as i64 + 3600);
        cache.record(entry_with_age(old)).unwrap();

        backfill_for(tmp.path(), TTL).record_if_missing().unwrap();

        let reloaded = ApprovedExecCache::new(tmp.path()).unwrap();
        let entry = reloaded
            .find("sha256:test", TTL)
            .expect("re-approval must be cached under a current timestamp");
        assert_eq!(entry.approval_request_id, "apr-fresh");
    }

    /// A live entry is left alone: backfill is a repair, not a refresh, so it
    /// must not extend an unexpired approval's horizon.
    #[test]
    fn backfill_leaves_a_live_entry_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ApprovedExecCache::new(tmp.path()).unwrap();
        let recent = chrono::Utc::now() - chrono::Duration::seconds(TTL as i64 / 2);
        cache.record(entry_with_age(recent)).unwrap();

        backfill_for(tmp.path(), TTL).record_if_missing().unwrap();

        let entry = ApprovedExecCache::new(tmp.path())
            .unwrap()
            .find("sha256:test", TTL)
            .expect("live entry survives");
        assert_eq!(entry.approval_request_id, "apr-test");
    }
}
