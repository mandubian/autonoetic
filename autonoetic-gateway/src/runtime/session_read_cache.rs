//! Session-scoped result cache for pure read tools (issue #289).
//!
//! Several read tools re-execute on every call even though their result is
//! stable within a session:
//!
//! - `resolve` — content reads are content-addressed (`sha256:` handle →
//!   identical bytes) so they cache stably; artifact reads cache under
//!   `ArtifactMetadata` (invalidated by `artifact_build`).
//! - `agent_inspect` — agent existence + active metadata; changes only
//!   via explicit agent-mutating tools.
//! - `artifact_inspect` — artifact metadata; changes only via
//!   `artifact_build`.
//!
//! In long orchestration sessions the same handles get read dozens of
//! times, re-injecting identical content into the LLM transcript each
//! turn. This module memoizes the raw tool result keyed by
//! `(tool_name, sha256(normalized_args))`, scoped to one session.
//!
//! Constitutional grounding: same family as P-2.6 / P-2.7 (deterministic
//! operations skip re-execution), extended to pure reads where the safety
//! argument is stronger — there is no side effect to skip.
//!
//! ## Safety choices
//!
//! - **Keyed by exact `session_id`, not root.** `resolve` content reads honour
//!   per-session visibility; caching a result under the exact session
//!   that produced it means a sibling session can never be served another
//!   session's private content from the cache.
//! - **Only the raw `registry.execute` output is cached.** Disclosure
//!   registration and secret-store redaction still run on every hit in
//!   the caller, so caching is transparent to those invariants.
//! - **Invalidation is coarse but obviously correct.** Agent-mutating and
//!   artifact-building tools clear the corresponding tag class across all
//!   session caches. Content (CacheStable) entries are never invalidated; artifact entries clear on artifact_build.
//! - **Bounded + size-guarded.** Per-session LRU of `max_entries`
//!   (default 128); results larger than `max_value_bytes` (default 1 MiB)
//!   are not stored, so the cache can't balloon memory.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

/// Default per-session entry cap.
pub const DEFAULT_MAX_ENTRIES: usize = 128;
/// Default max cached value size in bytes (1 MiB).
pub const DEFAULT_MAX_VALUE_BYTES: usize = 1024 * 1024;

/// Invalidation tag classes. A cacheable read belongs to at most one tag;
/// a mutating tool clears one tag across every session cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheTag {
    /// `agent_inspect` results — cleared by agent install / revision
    /// create / promote / rollback (anything that changes an agent's
    /// existence or active metadata).
    AgentExistence,
    /// `artifact_inspect` results — cleared by `artifact_build`.
    ArtifactMetadata,
}

/// Cacheability decision for a read tool: cacheable under a tag (or no
/// tag, meaning never invalidated), or not cacheable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadCachePolicy {
    /// Cache forever within the session (content-addressed; e.g. a `resolve` content read).
    CacheStable,
    /// Cache, but invalidate when the given tag is cleared.
    CacheUnderTag(CacheTag),
}

/// Returns the caching policy for a read tool, or `None` if the tool is
/// not a cacheable pure read. Takes `arguments_json` because `resolve` is
/// polymorphic — its caching depends on the handle being resolved.
pub fn read_cache_policy(tool_name: &str, arguments_json: &str) -> Option<ReadCachePolicy> {
    match tool_name {
        "agent_inspect" => Some(ReadCachePolicy::CacheUnderTag(CacheTag::AgentExistence)),
        "artifact_inspect" => Some(ReadCachePolicy::CacheUnderTag(CacheTag::ArtifactMetadata)),
        // `resolve` reads either an artifact (`art_`/`ar.` — invalidated by
        // artifact_build, like artifact_inspect) or content (content-addressed,
        // stable). Classify by the `ref` shape without resolving it.
        "resolve" => Some(resolve_cache_policy(arguments_json)),
        _ => None,
    }
}

/// Cache policy for a `resolve` call, derived from the `ref` it targets.
/// Artifact handles (`art_`/`ar.`) cache under [`CacheTag::ArtifactMetadata`]
/// (invalidated by `artifact_build`, matching `artifact_inspect`); content
/// handles are content-addressed and cache stably.
fn resolve_cache_policy(arguments_json: &str) -> ReadCachePolicy {
    let is_artifact = serde_json::from_str::<serde_json::Value>(arguments_json)
        .ok()
        .and_then(|v| {
            v.get("ref")
                .and_then(|r| r.as_str())
                .map(|s| {
                    let s = s.trim();
                    s.starts_with("art_") || s.starts_with("ar.")
                })
        })
        .unwrap_or(false);
    if is_artifact {
        ReadCachePolicy::CacheUnderTag(CacheTag::ArtifactMetadata)
    } else {
        ReadCachePolicy::CacheStable
    }
}

/// Returns the tag a mutating tool invalidates, or `None` if the tool does
/// not affect any cached read class.
pub fn invalidation_tag_for(tool_name: &str) -> Option<CacheTag> {
    match tool_name {
        // Tools that change what `agent_inspect` would return. A revision
        // exists (and is inspectable) as soon as it is created, so revision
        // *creation* — not just promotion — must invalidate. `skill_install`
        // installs a SKILL.md as a brand-new agent. (There is no
        // `agent_install` tool; that string is only an approval-action kind.)
        "skill_install"
        | "agent_revision_create"
        | "agent_revision_create_from_intent"
        | "agent_revision_promote"
        | "agent_revision_rollback" => Some(CacheTag::AgentExistence),
        "artifact_build" => Some(CacheTag::ArtifactMetadata),
        _ => None,
    }
}

/// Normalize tool arguments before hashing into the cache key. Strips
/// volatile fields that do not affect a read's result (currently the
/// echoed `intent` string that models attach), mirroring the loop-guard
/// fingerprint normalization so semantically-identical calls share a key.
fn normalize_args(arguments_json: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(arguments_json) else {
        return arguments_json.to_string();
    };
    if let Some(obj) = v.as_object_mut() {
        obj.remove("intent");
    }
    serde_json::to_string(&v).unwrap_or_else(|_| arguments_json.to_string())
}

fn cache_key(tool_name: &str, arguments_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update([0u8]);
    hasher.update(normalize_args(arguments_json).as_bytes());
    format!("{:x}", hasher.finalize())
}

struct Entry {
    value: String,
    tag: Option<CacheTag>,
}

/// A single session's bounded LRU result cache.
struct SessionReadCache {
    entries: HashMap<String, Entry>,
    /// Access order, least-recently-used at the front.
    order: Vec<String>,
    max_entries: usize,
    max_value_bytes: usize,
}

impl SessionReadCache {
    fn new(max_entries: usize, max_value_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: Vec::new(),
            max_entries: max_entries.max(1),
            max_value_bytes,
        }
    }

    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(pos);
            self.order.push(k);
        }
    }

    fn get(&mut self, key: &str) -> Option<String> {
        if self.entries.contains_key(key) {
            self.touch(key);
            return self.entries.get(key).map(|e| e.value.clone());
        }
        None
    }

    fn put(&mut self, key: String, value: String, tag: Option<CacheTag>) {
        if value.len() > self.max_value_bytes {
            return; // size guard: do not poison the cache with large payloads
        }
        if !self.entries.contains_key(&key) {
            while self.order.len() >= self.max_entries {
                let evicted = self.order.remove(0);
                self.entries.remove(&evicted);
            }
            self.order.push(key.clone());
        } else {
            self.touch(&key);
        }
        self.entries.insert(key, Entry { value, tag });
    }

    fn invalidate_tag(&mut self, tag: CacheTag) {
        let to_remove: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.tag == Some(tag))
            .map(|(k, _)| k.clone())
            .collect();
        for k in to_remove {
            self.entries.remove(&k);
            if let Some(pos) = self.order.iter().position(|o| o == &k) {
                self.order.remove(pos);
            }
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Registry of per-session read caches, shared on `GatewayStore`.
///
/// Cloning returns another handle to the same underlying map. Caches are
/// created lazily on first store. Entries are not currently GC'd on
/// session end (a session that never reads has no cache); periodic
/// compaction is a possible follow-up — see issue #289.
#[derive(Clone)]
pub struct SessionReadCacheRegistry {
    inner: Arc<Mutex<HashMap<String, SessionReadCache>>>,
    max_entries: usize,
    max_value_bytes: usize,
}

impl Default for SessionReadCacheRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ENTRIES, DEFAULT_MAX_VALUE_BYTES)
    }
}

impl SessionReadCacheRegistry {
    pub fn new(max_entries: usize, max_value_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            max_entries,
            max_value_bytes,
        }
    }

    /// Look up a cached result for `(session_id, tool_name, arguments)`.
    /// Returns `None` on miss or if the tool is not cacheable.
    pub fn get(&self, session_id: &str, tool_name: &str, arguments_json: &str) -> Option<String> {
        read_cache_policy(tool_name, arguments_json)?;
        let key = cache_key(tool_name, arguments_json);
        let mut guard = self.inner.lock().ok()?;
        guard.get_mut(session_id)?.get(&key)
    }

    /// Store a result if the tool is cacheable and the value fits the size
    /// guard. No-op otherwise.
    pub fn put(&self, session_id: &str, tool_name: &str, arguments_json: &str, value: &str) {
        let Some(policy) = read_cache_policy(tool_name, arguments_json) else {
            return;
        };
        let tag = match policy {
            ReadCachePolicy::CacheStable => None,
            ReadCachePolicy::CacheUnderTag(t) => Some(t),
        };
        let key = cache_key(tool_name, arguments_json);
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let cache = guard
            .entry(session_id.to_string())
            .or_insert_with(|| SessionReadCache::new(self.max_entries, self.max_value_bytes));
        cache.put(key, value.to_string(), tag);
    }

    /// Invalidate every entry of `tag`'s class across **all** session
    /// caches. Called when a mutating tool runs. Coarse by design — these
    /// mutations are infrequent and the cleared reads are cheap to
    /// recompute.
    pub fn invalidate_tag_all_sessions(&self, tag: CacheTag) {
        if let Ok(mut guard) = self.inner.lock() {
            for cache in guard.values_mut() {
                cache.invalidate_tag(tag);
            }
        }
    }

    /// Test/diagnostic: number of cached entries for a session.
    pub fn entry_count(&self, session_id: &str) -> usize {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.get(session_id).map(|c| c.len()))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "sess-1";

    #[test]
    fn policy_table_matches_issue_289() {
        // resolve is polymorphic by ref shape.
        assert_eq!(
            read_cache_policy("resolve", r#"{"ref":"main.py"}"#),
            Some(ReadCachePolicy::CacheStable),
            "content ref → stable"
        );
        assert_eq!(
            read_cache_policy("resolve", r#"{"ref":"cnt_abcd1234"}"#),
            Some(ReadCachePolicy::CacheStable)
        );
        assert_eq!(
            read_cache_policy("resolve", r#"{"ref":"ar.aabb11223344","include":"files"}"#),
            Some(ReadCachePolicy::CacheUnderTag(CacheTag::ArtifactMetadata)),
            "artifact ref → invalidated by artifact_build"
        );
        assert_eq!(
            read_cache_policy("resolve", r#"{"ref":"art_aabb1234"}"#),
            Some(ReadCachePolicy::CacheUnderTag(CacheTag::ArtifactMetadata))
        );
        assert_eq!(
            read_cache_policy("agent_inspect", "{}"),
            Some(ReadCachePolicy::CacheUnderTag(CacheTag::AgentExistence))
        );
        assert_eq!(
            read_cache_policy("artifact_inspect", "{}"),
            Some(ReadCachePolicy::CacheUnderTag(CacheTag::ArtifactMetadata))
        );
        assert_eq!(read_cache_policy("sandbox_exec", "{}"), None);
        assert_eq!(read_cache_policy("agent_spawn", "{}"), None);
    }

    #[test]
    fn invalidation_tag_table() {
        for t in [
            "skill_install",
            "agent_revision_create",
            "agent_revision_create_from_intent",
            "agent_revision_promote",
            "agent_revision_rollback",
        ] {
            assert_eq!(invalidation_tag_for(t), Some(CacheTag::AgentExistence), "{t}");
        }
        assert_eq!(
            invalidation_tag_for("artifact_build"),
            Some(CacheTag::ArtifactMetadata)
        );
        assert_eq!(invalidation_tag_for("resolve"), None);
        // `agent_install` is NOT a real tool (only an approval-action kind),
        // so it must not be in the invalidation set.
        assert_eq!(invalidation_tag_for("agent_install"), None);
        // Read-only revision tools must not invalidate.
        assert_eq!(invalidation_tag_for("agent_revision_list"), None);
    }

    #[test]
    fn hit_returns_cached_value() {
        let reg = SessionReadCacheRegistry::default();
        assert!(reg.get(S, "resolve", r#"{"name":"f"}"#).is_none());
        reg.put(S, "resolve", r#"{"name":"f"}"#, "BYTES");
        assert_eq!(reg.get(S, "resolve", r#"{"name":"f"}"#).as_deref(), Some("BYTES"));
    }

    #[test]
    fn intent_field_is_ignored_in_key() {
        let reg = SessionReadCacheRegistry::default();
        reg.put(S, "resolve", r#"{"name":"f","intent":"first"}"#, "BYTES");
        // Different intent, same logical args → hit.
        assert_eq!(
            reg.get(S, "resolve", r#"{"name":"f","intent":"second"}"#).as_deref(),
            Some("BYTES")
        );
    }

    #[test]
    fn distinct_args_do_not_collide() {
        let reg = SessionReadCacheRegistry::default();
        reg.put(S, "resolve", r#"{"name":"a"}"#, "AAA");
        reg.put(S, "resolve", r#"{"name":"b"}"#, "BBB");
        assert_eq!(reg.get(S, "resolve", r#"{"name":"a"}"#).as_deref(), Some("AAA"));
        assert_eq!(reg.get(S, "resolve", r#"{"name":"b"}"#).as_deref(), Some("BBB"));
    }

    #[test]
    fn sessions_are_isolated() {
        let reg = SessionReadCacheRegistry::default();
        reg.put("sess-A", "resolve", r#"{"name":"f"}"#, "A_PRIVATE");
        // A sibling session must NOT be served A's cached content.
        assert!(reg.get("sess-B", "resolve", r#"{"name":"f"}"#).is_none());
    }

    #[test]
    fn non_cacheable_tool_is_never_stored() {
        let reg = SessionReadCacheRegistry::default();
        reg.put(S, "sandbox_exec", r#"{"command":"ls"}"#, "out");
        assert!(reg.get(S, "sandbox_exec", r#"{"command":"ls"}"#).is_none());
        assert_eq!(reg.entry_count(S), 0);
    }

    #[test]
    fn agent_existence_invalidation_clears_only_that_tag() {
        let reg = SessionReadCacheRegistry::default();
        reg.put(S, "resolve", r#"{"name":"f"}"#, "BYTES");
        reg.put(S, "agent_inspect", r#"{"agent_id":"x"}"#, r#"{"exists":false}"#);
        reg.put(S, "artifact_inspect", r#"{"artifact_ref":"a"}"#, r#"{"files":[]}"#);

        reg.invalidate_tag_all_sessions(CacheTag::AgentExistence);

        // agent_inspect cleared; resolve(content) + artifact_inspect untouched.
        assert!(reg.get(S, "agent_inspect", r#"{"agent_id":"x"}"#).is_none());
        assert_eq!(reg.get(S, "resolve", r#"{"name":"f"}"#).as_deref(), Some("BYTES"));
        assert!(reg.get(S, "artifact_inspect", r#"{"artifact_ref":"a"}"#).is_some());
    }

    #[test]
    fn invalidation_spans_all_sessions() {
        let reg = SessionReadCacheRegistry::default();
        reg.put("sess-A", "agent_inspect", r#"{"agent_id":"x"}"#, "false");
        reg.put("sess-B", "agent_inspect", r#"{"agent_id":"x"}"#, "false");
        // A mutation in any session invalidates the existence class everywhere.
        reg.invalidate_tag_all_sessions(CacheTag::AgentExistence);
        assert!(reg.get("sess-A", "agent_inspect", r#"{"agent_id":"x"}"#).is_none());
        assert!(reg.get("sess-B", "agent_inspect", r#"{"agent_id":"x"}"#).is_none());
    }

    #[test]
    fn lru_eviction_bounds_entries() {
        let reg = SessionReadCacheRegistry::new(128, DEFAULT_MAX_VALUE_BYTES);
        for i in 0..200 {
            reg.put(S, "resolve", &format!(r#"{{"name":"f{i}"}}"#), &format!("v{i}"));
        }
        assert_eq!(reg.entry_count(S), 128, "cache must be bounded to max_entries");
        // The oldest (f0..f71) evicted; the newest 128 (f72..f199) retained.
        assert!(reg.get(S, "resolve", r#"{"name":"f0"}"#).is_none());
        assert_eq!(
            reg.get(S, "resolve", r#"{"name":"f199"}"#).as_deref(),
            Some("v199")
        );
    }

    #[test]
    fn lru_access_protects_recently_used() {
        let reg = SessionReadCacheRegistry::new(3, DEFAULT_MAX_VALUE_BYTES);
        reg.put(S, "resolve", r#"{"name":"a"}"#, "A");
        reg.put(S, "resolve", r#"{"name":"b"}"#, "B");
        reg.put(S, "resolve", r#"{"name":"c"}"#, "C");
        // Touch "a" so it is most-recently-used.
        assert_eq!(reg.get(S, "resolve", r#"{"name":"a"}"#).as_deref(), Some("A"));
        // Insert "d" → should evict "b" (the LRU), not "a".
        reg.put(S, "resolve", r#"{"name":"d"}"#, "D");
        assert_eq!(reg.get(S, "resolve", r#"{"name":"a"}"#).as_deref(), Some("A"));
        assert!(reg.get(S, "resolve", r#"{"name":"b"}"#).is_none());
        assert_eq!(reg.get(S, "resolve", r#"{"name":"d"}"#).as_deref(), Some("D"));
    }

    #[test]
    fn size_guard_skips_large_values() {
        let reg = SessionReadCacheRegistry::new(128, 1024);
        let big = "x".repeat(2048);
        reg.put(S, "resolve", r#"{"name":"big"}"#, &big);
        assert!(reg.get(S, "resolve", r#"{"name":"big"}"#).is_none());
        assert_eq!(reg.entry_count(S), 0, "large value must not be stored");
        // A small value alongside still caches fine.
        reg.put(S, "resolve", r#"{"name":"small"}"#, "ok");
        assert_eq!(reg.get(S, "resolve", r#"{"name":"small"}"#).as_deref(), Some("ok"));
    }
}
