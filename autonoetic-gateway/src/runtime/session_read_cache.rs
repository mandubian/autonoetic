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
//! - **Content reads are keyed by exact `session_id`, not root.** `resolve`
//!   content reads honour per-session visibility — content can be registered
//!   with `ContentVisibility::Private`, which the store isolates to the
//!   writing session (see `test_private_visibility_isolates_from_root`).
//!   Caching a content read under the exact session that produced it means a
//!   sibling session can never be served another session's private content
//!   from the cache. This is why content `resolve` reads use `CacheStable`
//!   (exact-session) and NOT root-scoping.
//! - **Artifact reads are keyed by `root_session_id`.** `artifact_build`
//!   registers every `ar.` ref at root-session scope (`scope_id =
//!   root_session_id(sid)`) by design — integration test
//!   `test_artifact_build_scopes_to_root_session_for_child_without_workflow`
//!   proves a sibling child resolves a ref built by another child via
//!   `resolve_artifact_ref_any_scope`. Artifact bundles are immutable and
//!   content-addressed, so memoizing under the root does not broaden
//!   visibility beyond what the store already permits. This is the fix for
//!   the sibling-duplicate-reads problem (#841): without root-scoping, every
//!   fan-out child re-reads the same artifact file the parent/siblings
//!   already cached, defeating the cache entirely.
//! - **Only the raw `registry.execute` output is cached.** Disclosure
//!   registration and secret-store redaction still run on every hit in
//!   the caller, so caching is transparent to those invariants.
//! - **Invalidation is coarse but obviously correct.** Agent-mutating and
//!   artifact-building tools clear the corresponding tag class across all
//!   session caches. Content (CacheStable) entries are never invalidated;
//!   artifact entries clear on artifact_build — for root-scoped entries
//!   this clears the shared root bucket, so a freshly-built artifact's
//!   metadata is always re-read.
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
///
/// The variant also chooses the *bucket key*: exact `session_id` (the
/// default, for per-session-private reads) or `root_session_id` (for reads
/// whose underlying store entry is already root-scoped, so siblings sharing
/// the cache doesn't broaden visibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadCachePolicy {
    /// Cache forever within the session (content-addressed; e.g. a `resolve` content read).
    ///
    /// Keyed by exact `session_id` — content may be `ContentVisibility::Private`,
    /// so a sibling must never be served another session's cached bytes.
    CacheStable,
    /// Cache, but invalidate when the given tag is cleared.
    ///
    /// Keyed by exact `session_id`.
    CacheUnderTag(CacheTag),
    /// Cache under a tag (cleared as a class), keyed by `root_session_id`
    /// instead of the exact session. Safe only for reads whose underlying
    /// store entry is already root-scoped: `artifact_build` registers `ar.`
    /// refs at `scope_id = root_session_id(sid)` and artifact bundles are
    /// immutable + content-addressed, so memoizing under the root does not
    /// broaden visibility beyond what `resolve_artifact_ref_any_scope`
    /// already permits. This is what makes sibling fan-out children share
    /// cached artifact reads (#841).
    CacheUnderTagScopedToRoot(CacheTag),
}

impl ReadCachePolicy {
    /// Whether lookups/stores for this policy use `root_session_id` as the
    /// bucket key (true) or the exact `session_id` (false).
    fn scoped_to_root(self) -> bool {
        matches!(self, ReadCachePolicy::CacheUnderTagScopedToRoot(_))
    }

    /// The invalidation tag, if any (`CacheStable` returns `None`).
    fn tag(self) -> Option<CacheTag> {
        match self {
            ReadCachePolicy::CacheStable => None,
            ReadCachePolicy::CacheUnderTag(t) | ReadCachePolicy::CacheUnderTagScopedToRoot(t) => {
                Some(t)
            }
        }
    }
}

/// Returns the caching policy for a read tool, or `None` if the tool is
/// not a cacheable pure read. Takes `arguments_json` because `resolve` is
/// polymorphic — its caching depends on the handle being resolved.
pub fn read_cache_policy(tool_name: &str, arguments_json: &str) -> Option<ReadCachePolicy> {
    match tool_name {
        "agent_inspect" => Some(ReadCachePolicy::CacheUnderTag(CacheTag::AgentExistence)),
        // Artifacts are root-scoped by design (`artifact_build` writes the
        // ref at `scope_id = root_session_id`), so sibling children under the
        // same root can all resolve the same `ar.` ref via
        // `resolve_artifact_ref_any_scope`. Caching under the root bucket lets
        // fan-out siblings share the memoized read (#841) without broadening
        // visibility beyond what the store already permits.
        "artifact_inspect" => Some(ReadCachePolicy::CacheUnderTagScopedToRoot(
            CacheTag::ArtifactMetadata,
        )),
        // `resolve` reads either an artifact (`art_`/`ar.` — root-scoped,
        // invalidated by artifact_build) or content (content-addressed,
        // stable, per-session-private). Classify by the `ref` shape without
        // resolving it.
        "resolve" => Some(resolve_cache_policy(arguments_json)),
        _ => None,
    }
}

/// Cache policy for a `resolve` call, derived from the `ref` it targets.
/// Artifact handles (`art_`/`ar.`) cache under [`CacheTag::ArtifactMetadata`]
/// scoped to the root session (siblings share — #841); content handles are
/// content-addressed, may be `Private`, and cache stably under the exact
/// session.
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
        ReadCachePolicy::CacheUnderTagScopedToRoot(CacheTag::ArtifactMetadata)
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

/// Resolve the registry bucket key (the outer `HashMap` key — one
/// `SessionReadCache` per bucket) for a given policy.
///
/// - exact-session policies (`CacheStable`, `CacheUnderTag`) → the session id
/// - root-scoped policies (`CacheUnderTagScopedToRoot`) → the root session id.
///   The caller-supplied `root_session_id` wins; otherwise the root is
///   derived from `session_id` via `content_store::root_session_id`.
fn bucket_key_for(
    session_id: &str,
    root_session_id: Option<&str>,
    policy: ReadCachePolicy,
) -> String {
    if policy.scoped_to_root() {
        root_session_id
            .map(str::to_string)
            .unwrap_or_else(|| {
                crate::runtime::content_store::root_session_id(session_id).to_string()
            })
    } else {
        session_id.to_string()
    }
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
    ///
    /// `root_session_id` is the bucket key for root-scoped policies
    /// (`CacheUnderTagScopedToRoot`); pass `Some(root)` so artifact reads
    /// are shared across sibling sessions under the same root. When `None`,
    /// the root is derived from `session_id` (handles the common
    /// `"root/child-abc"` shape) — a miss is always safe, so the fallback
    /// never risks a wrong-bucket hit.
    pub fn get(
        &self,
        session_id: &str,
        root_session_id: Option<&str>,
        tool_name: &str,
        arguments_json: &str,
    ) -> Option<String> {
        let policy = read_cache_policy(tool_name, arguments_json)?;
        let bucket = bucket_key_for(session_id, root_session_id, policy);
        let key = cache_key(tool_name, arguments_json);
        let mut guard = self.inner.lock().ok()?;
        guard.get_mut(&bucket)?.get(&key)
    }

    /// Store a result if the tool is cacheable and the value fits the size
    /// guard. No-op otherwise. See [`Self::get`] for the `root_session_id`
    /// bucket semantics.
    pub fn put(
        &self,
        session_id: &str,
        root_session_id: Option<&str>,
        tool_name: &str,
        arguments_json: &str,
        value: &str,
    ) {
        let Some(policy) = read_cache_policy(tool_name, arguments_json) else {
            return;
        };
        let tag = policy.tag();
        let bucket = bucket_key_for(session_id, root_session_id, policy);
        let key = cache_key(tool_name, arguments_json);
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let cache = guard
            .entry(bucket)
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
    ///
    /// `bucket` is the outer registry key — the exact session id for
    /// per-session policies, or the root session id for root-scoped policies.
    pub fn entry_count(&self, bucket: &str) -> usize {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.get(bucket).map(|c| c.len()))
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
            "content ref → stable, exact-session"
        );
        assert_eq!(
            read_cache_policy("resolve", r#"{"ref":"cnt_abcd1234"}"#),
            Some(ReadCachePolicy::CacheStable)
        );
        assert_eq!(
            read_cache_policy("resolve", r#"{"ref":"ar.aabb11223344","include":"files"}"#),
            Some(ReadCachePolicy::CacheUnderTagScopedToRoot(CacheTag::ArtifactMetadata)),
            "artifact ref → root-scoped (siblings share, #841), invalidated by artifact_build"
        );
        assert_eq!(
            read_cache_policy("resolve", r#"{"ref":"art_aabb1234"}"#),
            Some(ReadCachePolicy::CacheUnderTagScopedToRoot(CacheTag::ArtifactMetadata))
        );
        assert_eq!(
            read_cache_policy("agent_inspect", "{}"),
            Some(ReadCachePolicy::CacheUnderTag(CacheTag::AgentExistence))
        );
        assert_eq!(
            read_cache_policy("artifact_inspect", "{}"),
            Some(ReadCachePolicy::CacheUnderTagScopedToRoot(CacheTag::ArtifactMetadata)),
            "artifact_inspect → root-scoped (same reasoning as artifact resolve)"
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
        assert!(reg.get(S, None, "resolve", r#"{"name":"f"}"#).is_none());
        reg.put(S, None, "resolve", r#"{"name":"f"}"#, "BYTES");
        assert_eq!(
            reg.get(S, None, "resolve", r#"{"name":"f"}"#).as_deref(),
            Some("BYTES")
        );
    }

    #[test]
    fn intent_field_is_ignored_in_key() {
        let reg = SessionReadCacheRegistry::default();
        reg.put(S, None, "resolve", r#"{"name":"f","intent":"first"}"#, "BYTES");
        // Different intent, same logical args → hit.
        assert_eq!(
            reg.get(S, None, "resolve", r#"{"name":"f","intent":"second"}"#).as_deref(),
            Some("BYTES")
        );
    }

    #[test]
    fn distinct_args_do_not_collide() {
        let reg = SessionReadCacheRegistry::default();
        reg.put(S, None, "resolve", r#"{"name":"a"}"#, "AAA");
        reg.put(S, None, "resolve", r#"{"name":"b"}"#, "BBB");
        assert_eq!(reg.get(S, None, "resolve", r#"{"name":"a"}"#).as_deref(), Some("AAA"));
        assert_eq!(reg.get(S, None, "resolve", r#"{"name":"b"}"#).as_deref(), Some("BBB"));
    }

    #[test]
    fn sessions_are_isolated() {
        let reg = SessionReadCacheRegistry::default();
        reg.put("sess-A", None, "resolve", r#"{"name":"f"}"#, "A_PRIVATE");
        // A sibling session must NOT be served A's cached content.
        assert!(reg.get("sess-B", None, "resolve", r#"{"name":"f"}"#).is_none());
    }

    #[test]
    fn non_cacheable_tool_is_never_stored() {
        let reg = SessionReadCacheRegistry::default();
        reg.put(S, None, "sandbox_exec", r#"{"command":"ls"}"#, "out");
        assert!(reg.get(S, None, "sandbox_exec", r#"{"command":"ls"}"#).is_none());
        assert_eq!(reg.entry_count(S), 0);
    }

    #[test]
    fn agent_existence_invalidation_clears_only_that_tag() {
        let reg = SessionReadCacheRegistry::default();
        reg.put(S, None, "resolve", r#"{"name":"f"}"#, "BYTES");
        reg.put(S, None, "agent_inspect", r#"{"agent_id":"x"}"#, r#"{"exists":false}"#);
        reg.put(S, None, "artifact_inspect", r#"{"artifact_ref":"a"}"#, r#"{"files":[]}"#);

        reg.invalidate_tag_all_sessions(CacheTag::AgentExistence);

        // agent_inspect cleared; resolve(content) + artifact_inspect untouched.
        assert!(reg.get(S, None, "agent_inspect", r#"{"agent_id":"x"}"#).is_none());
        assert_eq!(reg.get(S, None, "resolve", r#"{"name":"f"}"#).as_deref(), Some("BYTES"));
        assert!(reg.get(S, None, "artifact_inspect", r#"{"artifact_ref":"a"}"#).is_some());
    }

    #[test]
    fn invalidation_spans_all_sessions() {
        let reg = SessionReadCacheRegistry::default();
        reg.put("sess-A", None, "agent_inspect", r#"{"agent_id":"x"}"#, "false");
        reg.put("sess-B", None, "agent_inspect", r#"{"agent_id":"x"}"#, "false");
        // A mutation in any session invalidates the existence class everywhere.
        reg.invalidate_tag_all_sessions(CacheTag::AgentExistence);
        assert!(reg.get("sess-A", None, "agent_inspect", r#"{"agent_id":"x"}"#).is_none());
        assert!(reg.get("sess-B", None, "agent_inspect", r#"{"agent_id":"x"}"#).is_none());
    }

    #[test]
    fn lru_eviction_bounds_entries() {
        let reg = SessionReadCacheRegistry::new(128, DEFAULT_MAX_VALUE_BYTES);
        for i in 0..200 {
            reg.put(S, None, "resolve", &format!(r#"{{"name":"f{i}"}}"#), &format!("v{i}"));
        }
        assert_eq!(reg.entry_count(S), 128, "cache must be bounded to max_entries");
        // The oldest (f0..f71) evicted; the newest 128 (f72..f199) retained.
        assert!(reg.get(S, None, "resolve", r#"{"name":"f0"}"#).is_none());
        assert_eq!(
            reg.get(S, None, "resolve", r#"{"name":"f199"}"#).as_deref(),
            Some("v199")
        );
    }

    #[test]
    fn lru_access_protects_recently_used() {
        let reg = SessionReadCacheRegistry::new(3, DEFAULT_MAX_VALUE_BYTES);
        reg.put(S, None, "resolve", r#"{"name":"a"}"#, "A");
        reg.put(S, None, "resolve", r#"{"name":"b"}"#, "B");
        reg.put(S, None, "resolve", r#"{"name":"c"}"#, "C");
        // Touch "a" so it is most-recently-used.
        assert_eq!(reg.get(S, None, "resolve", r#"{"name":"a"}"#).as_deref(), Some("A"));
        // Insert "d" → should evict "b" (the LRU), not "a".
        reg.put(S, None, "resolve", r#"{"name":"d"}"#, "D");
        assert_eq!(reg.get(S, None, "resolve", r#"{"name":"a"}"#).as_deref(), Some("A"));
        assert!(reg.get(S, None, "resolve", r#"{"name":"b"}"#).is_none());
        assert_eq!(reg.get(S, None, "resolve", r#"{"name":"d"}"#).as_deref(), Some("D"));
    }

    #[test]
    fn size_guard_skips_large_values() {
        let reg = SessionReadCacheRegistry::new(128, 1024);
        let big = "x".repeat(2048);
        reg.put(S, None, "resolve", r#"{"name":"big"}"#, &big);
        assert!(reg.get(S, None, "resolve", r#"{"name":"big"}"#).is_none());
        assert_eq!(reg.entry_count(S), 0, "large value must not be stored");
        // A small value alongside still caches fine.
        reg.put(S, None, "resolve", r#"{"name":"small"}"#, "ok");
        assert_eq!(
            reg.get(S, None, "resolve", r#"{"name":"small"}"#).as_deref(),
            Some("ok")
        );
    }

    // --- #841: root-scoped artifact reads shared across sibling sessions ---

    #[test]
    fn artifact_read_shared_across_sibling_sessions() {
        // The fan-out scenario: two child sessions under the same root both
        // resolve the same artifact ref. The parent (root) caches the read;
        // the sibling hits the cache instead of re-reading the store.
        let reg = SessionReadCacheRegistry::default();
        let parent = "root-sess";
        let coder_a = "root-sess/coder.default-aaa";
        let coder_b = "root-sess/coder.default-bbb";
        let args = r#"{"ref":"ar.aabb11223344","include":"files"}"#;

        // Parent reads the artifact first.
        reg.put(parent, Some(parent), "resolve", args, "ARTIFACT_FILES");
        // Sibling coder-a hits the shared root bucket.
        assert_eq!(
            reg.get(coder_a, Some(parent), "resolve", args).as_deref(),
            Some("ARTIFACT_FILES"),
            "sibling must hit the root-scoped artifact cache"
        );
        // Sibling coder-b also hits.
        assert_eq!(
            reg.get(coder_b, Some(parent), "resolve", args).as_deref(),
            Some("ARTIFACT_FILES")
        );
        // Stored exactly once under the root bucket (not duplicated per child).
        assert_eq!(reg.entry_count(parent), 1);
        assert_eq!(
            reg.entry_count(coder_a),
            0,
            "artifact reads must NOT be cached under the child session"
        );
    }

    #[test]
    fn artifact_read_root_derived_from_session_when_root_not_supplied() {
        // Same fan-out, but the caller passes `None` for root — the registry
        // must still derive the root from the `"root/child"` session id so
        // siblings share.
        let reg = SessionReadCacheRegistry::default();
        let coder_a = "root-sess/coder.default-aaa";
        let coder_b = "root-sess/coder.default-bbb";
        let args = r#"{"ref":"art_aabb1234"}"#;

        reg.put(coder_a, None, "resolve", args, "ART_BYTES");
        assert_eq!(
            reg.get(coder_b, None, "resolve", args).as_deref(),
            Some("ART_BYTES"),
            "root derived from session id must still let siblings share"
        );
        assert_eq!(reg.entry_count("root-sess"), 1);
    }

    #[test]
    fn content_read_stays_isolated_per_session() {
        // Regression guard for the visibility invariant: content reads
        // (`CacheStable`) must remain keyed by exact session, even when a
        // root is supplied, because content may be `ContentVisibility::Private`.
        let reg = SessionReadCacheRegistry::default();
        let coder_a = "root-sess/coder.default-aaa";
        let coder_b = "root-sess/coder.default-bbb";
        let args = r#"{"ref":"scratch.txt"}"#;

        reg.put(coder_a, Some("root-sess"), "resolve", args, "PRIVATE_BYTES");
        // Sibling coder-b must NOT be served coder-a's private content,
        // even though they share a root.
        assert!(
            reg.get(coder_b, Some("root-sess"), "resolve", args).is_none(),
            "content reads must stay isolated to the exact session"
        );
        // coder-a still reads its own cached value.
        assert_eq!(
            reg.get(coder_a, Some("root-sess"), "resolve", args).as_deref(),
            Some("PRIVATE_BYTES")
        );
    }

    #[test]
    fn artifact_invalidation_clears_root_scoped_entry() {
        // `artifact_build` clears ArtifactMetadata across ALL session caches.
        // A root-scoped artifact entry lives under the root bucket, which the
        // global invalidation sweep must still reach.
        let reg = SessionReadCacheRegistry::default();
        let coder_a = "root-sess/coder.default-aaa";
        let coder_b = "root-sess/coder.default-bbb";
        let args = r#"{"ref":"ar.aabb11223344","include":"content","file":"main.py"}"#;

        reg.put(coder_a, Some("root-sess"), "resolve", args, "MAIN_PY_BYTES");
        assert!(reg.get(coder_b, Some("root-sess"), "resolve", args).is_some());

        reg.invalidate_tag_all_sessions(CacheTag::ArtifactMetadata);

        assert!(
            reg.get(coder_b, Some("root-sess"), "resolve", args).is_none(),
            "root-scoped artifact entry must be cleared by artifact_build invalidation"
        );
    }

    #[test]
    fn distinct_roots_do_not_share_artifact_reads() {
        // Two unrelated root sessions with the same `ar.` ref string must NOT
        // share the cache — different roots may have independently-built
        // artifacts under the same short ref id.
        let reg = SessionReadCacheRegistry::default();
        let args = r#"{"ref":"ar.deadbeef"}"#;
        reg.put("root-a/coder", Some("root-a"), "resolve", args, "A_BYTES");
        assert!(
            reg.get("root-b/coder", Some("root-b"), "resolve", args).is_none(),
            "distinct roots must not cross-pollinate"
        );
    }
}
