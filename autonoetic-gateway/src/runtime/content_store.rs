//! Content-addressable storage for Autonoetic agents.
//!
//! Provides SHA-256 based content addressing that works locally and remotely.
//! Content is stored as immutable blobs; session manifests map names to handles.
//!
//! Visibility model:
//! - `private`: visible only to the writing session
//! - `session`: visible to all sessions under the same root_session_id (default)
//! - `global`: durable and cross-session readable

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A content handle is a SHA-256 hash prefixed with "sha256:".
pub type ContentHandle = String;

/// Visibility scope for content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContentVisibility {
    /// Visible only to the writing session.
    Private,
    /// Visible to all sessions under the same root_session_id.
    /// This is the default and matches the collaboration model.
    #[default]
    Session,
    /// Durable and cross-session readable.
    Global,
}

/// Returns the root session id — the portion before the first `/`.
///
/// `"demo-session/coder.default-abc"` → `"demo-session"`
/// `"demo-session"` → `"demo-session"`
pub fn root_session_id(session_id: &str) -> &str {
    session_id.split('/').next().unwrap_or(session_id)
}

/// Session manifest mapping content names to handles.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SessionManifest {
    /// Map of content name → handle
    pub names: HashMap<String, ContentHandle>,
    /// Map of short alias (8 hex chars) → full handle for LLM-friendly lookup
    pub aliases: HashMap<String, ContentHandle>,
    /// Root session ID for content visibility.
    /// All sessions sharing the same root_session_id can read each other's
    /// session-visible content.
    #[serde(default)]
    pub root_session_id: Option<String>,
    /// Per-handle visibility tracking.
    #[serde(default)]
    pub visibility: HashMap<ContentHandle, ContentVisibility>,
}

/// Session ID used for the global content manifest.
/// Sentinel session id under which the content store tracks globally-visible
/// handles. Exposed so callers (e.g. the `content.list` JSON-RPC method)
/// can probe the global manifest to resolve cross-session visibility.
pub const GLOBAL_SESSION_ID: &str = "__global__";

/// Short alias prefix length (8 hex chars = 32 bits, collision probability < 1/4B)
pub const SHORT_ALIAS_LEN: usize = 8;

/// Content-addressable store for agent artifacts.
///
/// Storage layout:
/// ```text
/// .gateway/content/
/// └── sha256/
///     └── ab/
///         └── c123...  ← immutable content blobs
/// ```
pub struct ContentStore {
    /// Root path for content storage (.gateway/content/)
    content_dir: PathBuf,
    /// Root path for session manifests (.gateway/sessions/)
    sessions_dir: PathBuf,
    /// In-memory cache of session manifests (loaded on demand)
    manifests: Arc<Mutex<HashMap<String, SessionManifest>>>,
}

/// True when `s` is safe to join under a base directory: relative, with no
/// escaping/absolute components, and at least one real path segment. Rejects
/// `""`, `.`, `..`, `/abs`, `C:\…`, and anything containing a `..` — so it can't
/// resolve to the base dir itself or escape it. Used by `project_live` for both
/// the `session_id` (which feeds `remove_dir_all`) and each content name.
fn safe_relative_path(s: &str) -> bool {
    let path = Path::new(s);
    if path.is_absolute() {
        return false;
    }
    let mut has_normal = false;
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => has_normal = true,
            std::path::Component::CurDir => {}
            // ParentDir, RootDir, Prefix — any of these can escape the base.
            _ => return false,
        }
    }
    has_normal
}

impl ContentStore {
    /// Creates a new ContentStore.
    pub fn new(gateway_dir: &Path) -> anyhow::Result<Self> {
        let content_dir = gateway_dir.join("content").join("sha256");
        let sessions_dir = gateway_dir.join("sessions");
        std::fs::create_dir_all(&content_dir)?;
        std::fs::create_dir_all(&sessions_dir)?;
        Ok(Self {
            content_dir,
            sessions_dir,
            manifests: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Computes the SHA-256 hash of content.
    pub fn compute_handle(content: &[u8]) -> ContentHandle {
        let mut hasher = Sha256::new();
        hasher.update(content);
        format!("sha256:{:x}", hasher.finalize())
    }

    /// Extracts short alias from a handle (first 8 hex chars after "sha256:").
    /// LLMs can reliably copy/reproduce this shorter identifier.
    pub fn handle_to_short_alias(handle: &ContentHandle) -> String {
        handle
            .strip_prefix("sha256:")
            .and_then(|h| h.get(..SHORT_ALIAS_LEN))
            .unwrap_or(handle)
            .to_string()
    }

    /// Computes the storage path for a content handle.
    fn handle_to_path(&self, handle: &ContentHandle) -> PathBuf {
        // sha256:ab12cd34... → sha256/ab/12cd34...
        let hash = handle.strip_prefix("sha256:").unwrap_or(handle);
        let prefix = &hash[..2];
        let rest = &hash[2..];
        self.content_dir.join(prefix).join(rest)
    }

    /// Writes content to the store and returns its handle.
    ///
    /// If content with the same hash already exists, returns the existing handle
    /// (natural deduplication).
    pub fn write(&self, content: &[u8]) -> anyhow::Result<ContentHandle> {
        let handle = Self::compute_handle(content);
        let path = self.handle_to_path(&handle);

        // Only write if not already stored
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)?;
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&path, perms)?;
            tracing::debug!(
                target: "content_store",
                handle = %handle,
                bytes = content.len(),
                "Stored new content"
            );
        }

        Ok(handle)
    }

    /// Returns the canonical on-disk path for an immutable content blob.
    pub fn blob_path(&self, handle: &ContentHandle) -> PathBuf {
        self.handle_to_path(handle)
    }

    /// Reads content by handle.
    pub fn read(&self, handle: &ContentHandle) -> anyhow::Result<Vec<u8>> {
        let path = self.handle_to_path(handle);
        if !path.exists() {
            anyhow::bail!("Content not found: {}", handle);
        }
        Ok(std::fs::read(&path)?)
    }

    /// Reads content as UTF-8 string.
    pub fn read_string(&self, handle: &ContentHandle) -> anyhow::Result<String> {
        let bytes = self.read(handle)?;
        String::from_utf8(bytes).map_err(|e| anyhow::anyhow!("Content is not valid UTF-8: {}", e))
    }

    /// Returns true if content exists in the store.
    pub fn exists(&self, handle: &ContentHandle) -> bool {
        self.handle_to_path(handle).exists()
    }

    /// Loads a session manifest from disk (or returns cached).
    pub fn load_manifest(&self, session_id: &str) -> anyhow::Result<SessionManifest> {
        {
            let manifests = self.manifests.lock().unwrap();
            if let Some(m) = manifests.get(session_id) {
                return Ok(m.clone());
            }
        }

        let manifest = self.load_manifest_from_disk_uncached(session_id)?;

        let mut manifests = self.manifests.lock().unwrap();
        manifests.insert(session_id.to_string(), manifest.clone());
        Ok(manifest)
    }

    fn load_manifest_from_disk_uncached(
        &self,
        session_id: &str,
    ) -> anyhow::Result<SessionManifest> {
        let path = self.manifest_path(session_id);
        if path.exists() {
            let json = std::fs::read_to_string(&path)?;
            let manifest: SessionManifest = serde_json::from_str(&json)?;
            Ok(manifest)
        } else {
            Ok(SessionManifest::default())
        }
    }

    /// Saves a session manifest to disk.
    fn save_manifest(&self, session_id: &str, manifest: &SessionManifest) -> anyhow::Result<()> {
        let path = self.manifest_path(session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(manifest)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Returns the path to a session's manifest file.
    fn manifest_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(session_id).join("manifest.json")
    }

    /// Registers a content name and short alias in a session manifest.
    /// The short alias (8 hex chars) is LLM-friendly for easy retrieval.
    pub fn register_name(
        &self,
        session_id: &str,
        name: &str,
        handle: &ContentHandle,
    ) -> anyhow::Result<()> {
        let mut manifests = self.manifests.lock().unwrap();
        if !manifests.contains_key(session_id) {
            let disk_manifest = self.load_manifest_from_disk_uncached(session_id)?;
            manifests.insert(session_id.to_string(), disk_manifest);
        }
        let manifest = manifests.get_mut(session_id).ok_or_else(|| {
            anyhow::anyhow!("Failed to load manifest for session '{}'", session_id)
        })?;
        manifest.names.insert(name.to_string(), handle.clone());

        // Also register the short alias for LLM-friendly lookup
        let short_alias = Self::handle_to_short_alias(handle);
        manifest.aliases.insert(short_alias, handle.clone());

        self.save_manifest(session_id, manifest)?;
        Ok(())
    }

    /// Sets the root session ID for content visibility.
    /// All sessions sharing the same root_session_id can read each other's
    /// session-visible content.
    pub fn set_root_session(&self, session_id: &str, root: &str) -> anyhow::Result<()> {
        let mut manifests = self.manifests.lock().unwrap();
        if !manifests.contains_key(session_id) {
            let disk_manifest = self.load_manifest_from_disk_uncached(session_id)?;
            manifests.insert(session_id.to_string(), disk_manifest);
        }
        let manifest = manifests.get_mut(session_id).ok_or_else(|| {
            anyhow::anyhow!("Failed to load manifest for session '{}'", session_id)
        })?;
        manifest.root_session_id = Some(root.to_string());
        self.save_manifest(session_id, manifest)?;
        Ok(())
    }

    /// Registers content with the given visibility.
    ///
    /// - `Private`: only registers in the current session's manifest.
    /// - `Session`: registers in both the current session AND the root session's manifest.
    /// - `Global`: registers in the current session, root session, AND a global index.
    pub fn register_name_with_visibility(
        &self,
        session_id: &str,
        name: &str,
        handle: &ContentHandle,
        visibility: ContentVisibility,
    ) -> anyhow::Result<()> {
        // Always register in current session
        self.register_name(session_id, name, handle)?;

        // Track visibility
        {
            let mut manifests = self.manifests.lock().unwrap();
            if !manifests.contains_key(session_id) {
                let disk_manifest = self.load_manifest_from_disk_uncached(session_id)?;
                manifests.insert(session_id.to_string(), disk_manifest);
            }
            if let Some(manifest) = manifests.get_mut(session_id) {
                manifest.visibility.insert(handle.clone(), visibility);
                self.save_manifest(session_id, manifest)?;
            }
        }

        // For session/global visibility, also register in root session
        if visibility != ContentVisibility::Private {
            let manifest = self.load_manifest(session_id)?;
            if let Some(root_id) = manifest.root_session_id {
                if root_id != session_id {
                    self.register_name(&root_id, name, handle)?;
                }
            }

            // For global visibility, also register in the global manifest
            if visibility == ContentVisibility::Global {
                self.register_name(GLOBAL_SESSION_ID, name, handle)?;

                tracing::debug!(
                    target: "content_store",
                    session_id = %session_id,
                    name = %name,
                    "Registered content in global manifest"
                );
            }
        }

        Ok(())
    }

    /// Resolves a name by checking current session, then root session, then global manifest.
    ///
    /// This enables session-visible and global content to be read by any session.
    pub fn resolve_name_with_root(
        &self,
        session_id: &str,
        name: &str,
    ) -> anyhow::Result<ContentHandle> {
        // 1. Try the caller's own session
        if let Ok(handle) = self.resolve_name(session_id, name) {
            return Ok(handle);
        }

        // 2. Try the root session (for root-level content)
        let manifest = self.load_manifest(session_id)?;
        if let Some(root_id) = manifest.root_session_id {
            if root_id != session_id {
                if let Ok(handle) = self.resolve_name(&root_id, name) {
                    return Ok(handle);
                }

                // 2b. Try sibling sessions under the same root
                // Content written by sibling agents (e.g., architect) should be
                // visible to other agents in the same workflow by name.
                if let Ok(handle) =
                    self.resolve_name_in_sibling_sessions(&root_id, name, session_id)
                {
                    return Ok(handle);
                }
            }
        }

        // 3. Try the global manifest (for global-visible content)
        if session_id != GLOBAL_SESSION_ID {
            if let Ok(handle) = self.resolve_name(GLOBAL_SESSION_ID, name) {
                return Ok(handle);
            }
        }

        Err(anyhow::anyhow!(
            "Content name '{}' not found in session '{}', root session, or siblings",
            name,
            session_id
        ))
    }

    /// Search for a content name across sibling sessions under the same root.
    fn resolve_name_in_sibling_sessions(
        &self,
        root_id: &str,
        name: &str,
        caller_id: &str,
    ) -> anyhow::Result<ContentHandle> {
        let root_dir = self.sessions_dir.join(root_id);
        if !root_dir.is_dir() {
            return Err(anyhow::anyhow!("no sibling sessions"));
        }

        // Scan subdirectories of the root session directory
        if let Ok(entries) = std::fs::read_dir(&root_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let child_id = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if child_id.is_empty() || child_id == root_id || child_id == caller_id {
                    continue;
                }
                // Check if this sibling has the named content
                if let Ok(handle) = self.resolve_name(child_id, name) {
                    return Ok(handle);
                }
            }
        }
        Err(anyhow::anyhow!(
            "name '{}' not found in any sibling session under '{}'",
            name,
            root_id
        ))
    }

    /// Resolves an alias by checking current session, then root session, then global.
    fn resolve_alias_with_root(
        &self,
        session_id: &str,
        alias: &str,
    ) -> anyhow::Result<ContentHandle> {
        // Check current session
        let manifest = self.load_manifest(session_id)?;
        if let Some(handle) = manifest.aliases.get(alias) {
            return Ok(handle.clone());
        }

        // Check root session
        if let Some(root_id) = manifest.root_session_id {
            if root_id != session_id {
                let root_manifest = self.load_manifest(&root_id)?;
                if let Some(handle) = root_manifest.aliases.get(alias) {
                    return Ok(handle.clone());
                }
            }
        }

        // Check global manifest
        if session_id != GLOBAL_SESSION_ID {
            let global_manifest = self.load_manifest(GLOBAL_SESSION_ID)?;
            if let Some(handle) = global_manifest.aliases.get(alias) {
                return Ok(handle.clone());
            }
        }

        Err(anyhow::anyhow!(
            "Content alias '{}' not found in session '{}', root session, or global",
            alias,
            session_id
        ))
    }

    /// Returns the short alias for a handle (for inclusion in API responses).
    pub fn get_short_alias(handle: &ContentHandle) -> String {
        Self::handle_to_short_alias(handle)
    }

    /// Resolves a name to a handle within a session.
    pub fn resolve_name(&self, session_id: &str, name: &str) -> anyhow::Result<ContentHandle> {
        let manifest = self.load_manifest(session_id)?;
        manifest.names.get(name).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "Content name '{}' not found in session '{}'",
                name,
                session_id
            )
        })
    }

    /// Finds the registered name(s) for a given content handle, searched across
    /// the session, its root session, and the global manifest. Used to map
    /// alias-style inputs (e.g. `cnt_3fc9d2bb`) back to human-readable names
    /// like `SKILL.md` when building artifacts.
    ///
    /// Results are sorted for deterministic ordering.
    pub fn find_names_for_handle(
        &self,
        session_id: &str,
        handle: &str,
    ) -> anyhow::Result<Vec<String>> {
        let manifest = self.load_manifest(session_id)?;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut names: Vec<String> = Vec::new();

        for (n, h) in &manifest.names {
            if h == handle && seen.insert(n.clone()) {
                names.push(n.clone());
            }
        }

        if let Some(root_id) = manifest.root_session_id {
            if root_id != session_id {
                let root_manifest = self.load_manifest(&root_id)?;
                for (n, h) in &root_manifest.names {
                    if h == handle && seen.insert(n.clone()) {
                        names.push(n.clone());
                    }
                }
            }
        }

        if session_id != GLOBAL_SESSION_ID {
            let global_manifest = self.load_manifest(GLOBAL_SESSION_ID)?;
            for (n, h) in &global_manifest.names {
                if h == handle && seen.insert(n.clone()) {
                    names.push(n.clone());
                }
            }
        }

        names.sort();
        Ok(names)
    }

    /// Reads content by name within a session.
    pub fn read_by_name(&self, session_id: &str, name: &str) -> anyhow::Result<Vec<u8>> {
        let handle = self.resolve_name(session_id, name)?;
        self.read(&handle)
    }

    /// Checks whether a handle is visible in the current session, its root session, or global.
    ///
    /// A handle is visible if it is registered (by name or alias) in:
    /// - The current session's manifest
    /// - The root session's manifest
    /// - The global manifest
    pub fn is_handle_visible(&self, session_id: &str, handle: &str) -> anyhow::Result<bool> {
        // Check current session
        let manifest = self.load_manifest(session_id)?;
        if manifest.names.values().any(|h| h == handle) {
            return Ok(true);
        }
        if manifest.aliases.values().any(|h| h == handle) {
            return Ok(true);
        }

        // Check root session
        if let Some(root_id) = manifest.root_session_id {
            if root_id != session_id {
                let root_manifest = self.load_manifest(&root_id)?;
                if root_manifest.names.values().any(|h| h == handle) {
                    return Ok(true);
                }
                if root_manifest.aliases.values().any(|h| h == handle) {
                    return Ok(true);
                }
            }
        }

        // Check global manifest
        if session_id != GLOBAL_SESSION_ID {
            let global_manifest = self.load_manifest(GLOBAL_SESSION_ID)?;
            if global_manifest.names.values().any(|h| h == handle) {
                return Ok(true);
            }
            if global_manifest.aliases.values().any(|h| h == handle) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Resolves a content name, `cnt_*` ref, bare alias, or `sha256:` handle the same way as
    /// [`read_by_name_or_handle`], returning the canonical store handle (no blob read).
    ///
    /// Use this anywhere session-visible content must be located consistently (e.g. `artifact.build`
    /// inputs must match `content.read`).
    pub fn resolve_name_or_handle_to_handle(
        &self,
        session_id: &str,
        name_or_handle: &str,
    ) -> anyhow::Result<ContentHandle> {
        let name_or_handle = name_or_handle.trim();

        // Agent-facing short ref from content.write / SpawnResult (`cnt_<8 hex>`)
        if let Some(rest) = name_or_handle.strip_prefix("cnt_") {
            if rest.len() == SHORT_ALIAS_LEN && rest.chars().all(|c| c.is_ascii_hexdigit()) {
                return self.resolve_alias_with_root(session_id, rest);
            }
        }
        if let Some(rest) = name_or_handle.strip_prefix("cnt:") {
            if rest.len() == SHORT_ALIAS_LEN && rest.chars().all(|c| c.is_ascii_hexdigit()) {
                return self.resolve_alias_with_root(session_id, rest);
            }
        }

        // "sha256:SHORT_ALIAS" — LLMs sometimes pass alias after sha256: prefix
        if name_or_handle.starts_with("sha256:") {
            let after_prefix = &name_or_handle["sha256:".len()..];
            if after_prefix.len() == SHORT_ALIAS_LEN
                && after_prefix.chars().all(|c| c.is_ascii_hexdigit())
            {
                return self.resolve_alias_with_root(session_id, after_prefix);
            }
            if !self.is_handle_visible(session_id, name_or_handle)? {
                anyhow::bail!(
                    "Content handle '{}' is not visible in session '{}' or its root session",
                    name_or_handle,
                    session_id
                );
            }
            return Ok(name_or_handle.to_string());
        }

        // Bare 8-char hex alias
        if name_or_handle.len() == SHORT_ALIAS_LEN
            && name_or_handle.chars().all(|c| c.is_ascii_hexdigit())
        {
            return self.resolve_alias_with_root(session_id, name_or_handle);
        }

        self.resolve_name_with_root(session_id, name_or_handle)
    }

    /// Reads content by name, handle, or short alias with root-based lookup.
    ///
    /// Resolution order:
    /// 0. Trim whitespace; if `cnt_<8 hex>` or `cnt:<8 hex>` → alias lookup (agent-facing ref)
    /// 1. If starts with "sha256:" → check visibility, then read
    ///    - Exception: if followed by exactly SHORT_ALIAS_LEN chars, treat as alias
    /// 2. If 8 hex chars → short alias lookup (session, then root)
    /// 3. Otherwise → name lookup (session, then root)
    pub fn read_by_name_or_handle(
        &self,
        session_id: &str,
        name_or_handle: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let handle = self.resolve_name_or_handle_to_handle(session_id, name_or_handle)?;
        self.read(&handle)
    }

    /// Lists all content names in a session.
    pub fn list_names(&self, session_id: &str) -> anyhow::Result<Vec<String>> {
        let manifest = self.load_manifest(session_id)?;
        let mut names: Vec<String> = manifest.names.keys().cloned().collect();
        names.sort();
        Ok(names)
    }

    /// Lists all content names with their handles in a session.
    pub fn list_names_with_handles(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let manifest = self.load_manifest(session_id)?;
        let mut entries: Vec<(String, String)> = manifest.names.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    /// Materialize the session's current content drafts into a real directory
    /// the operator can open in an external editor (`sessions/<id>/live/`).
    ///
    /// Read-only **snapshot**: the directory is rebuilt from the content store
    /// on every call, so it always reflects the current name→version mapping
    /// (and drops files whose names disappeared). Copying bytes out for viewing
    /// never feeds back into the store — immutability of the underlying blobs is
    /// untouched. Returns the directory and the names written.
    pub fn project_live(&self, session_id: &str) -> anyhow::Result<(PathBuf, Vec<String>)> {
        // `session_id` flows into a filesystem path AND a `remove_dir_all`, so a
        // traversal value ("../..", "/etc", "") could delete outside the sessions
        // tree. Reject anything that isn't a safe relative path before touching
        // the filesystem.
        anyhow::ensure!(
            safe_relative_path(session_id),
            "unsafe session_id for live projection: {session_id:?}"
        );
        let live_dir = self.sessions_dir.join(session_id).join("live");
        // Refresh from scratch so renames/deletions since the last call are
        // reflected rather than leaving stale files behind.
        if live_dir.exists() {
            std::fs::remove_dir_all(&live_dir)?;
        }
        std::fs::create_dir_all(&live_dir)?;

        let mut written = Vec::new();
        for (name, handle) in self.list_names_with_handles(session_id)? {
            // A content name is operator/agent-supplied; never let it escape the
            // live directory (absolute, `..`, drive prefixes) or resolve to the
            // directory itself (empty / `.`-only), which would error the write.
            if !safe_relative_path(&name) {
                tracing::warn!(
                    target: "content_store",
                    %name,
                    "skipping unsafe content name in live projection"
                );
                continue;
            }
            let out = live_dir.join(&name);
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = self.read_by_name_or_handle(session_id, &handle)?;
            std::fs::write(&out, bytes)?;
            written.push(name);
        }
        Ok((live_dir, written))
    }

    /// Clears a session manifest.
    pub fn cleanup_session(&self, session_id: &str) -> anyhow::Result<usize> {
        let manifest = self.load_manifest(session_id)?;
        let removed = manifest.names.len();

        tracing::debug!(
            target: "content_store",
            session_id = %session_id,
            names_removed = removed,
            "Session cleanup"
        );

        // Clear the manifest
        let mut manifests = self.manifests.lock().unwrap();
        manifests.insert(session_id.to_string(), SessionManifest::default());

        Ok(removed)
    }

    /// Returns the sessions directory path.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Returns statistics about the content store.
    pub fn stats(&self) -> anyhow::Result<ContentStoreStats> {
        let mut total_size = 0u64;
        let mut entry_count = 0u64;

        if self.content_dir.exists() {
            for prefix_entry in std::fs::read_dir(&self.content_dir)? {
                let prefix_entry = prefix_entry?;
                if prefix_entry.file_type()?.is_dir() {
                    for entry in std::fs::read_dir(prefix_entry.path())? {
                        let entry = entry?;
                        if entry.file_type()?.is_file() {
                            total_size += entry.metadata()?.len();
                            entry_count += 1;
                        }
                    }
                }
            }
        }

        Ok(ContentStoreStats {
            entry_count,
            total_size_bytes: total_size,
        })
    }

    /// Imports resource directories (scripts/, references/, assets/) from an
    /// external AgentSkills.io skill into the content store. Each file is
    /// stored as content-addressed blob and registered under a session-scoped
    /// name so the agent can access it via content.read.
    ///
    /// Returns the list of registered resource names.
    pub fn import_skill_resources(
        &self,
        skill_dir: &Path,
        session_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        let mut registered = Vec::new();
        for subdir in &["scripts", "references", "assets"] {
            let source = skill_dir.join(subdir);
            if !source.is_dir() {
                continue;
            }
            Self::import_dir_recursive(&self, &source, session_id, subdir, &mut registered)?;
        }
        Ok(registered)
    }

    fn import_dir_recursive(
        store: &ContentStore,
        dir: &Path,
        session_id: &str,
        prefix: &str,
        registered: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let new_prefix = format!("{}/{}", prefix, entry.file_name().to_string_lossy());
                Self::import_dir_recursive(store, &path, session_id, &new_prefix, registered)?;
            } else if path.is_file() {
                let content = std::fs::read(&path)?;
                let handle = store.write(&content)?;
                let name = format!("{}/{}", prefix, entry.file_name().to_string_lossy());
                store.register_name(session_id, &name, &handle)?;
                registered.push(name);
            }
        }
        Ok(())
    }
}

/// Statistics about the content store.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContentStoreStats {
    pub entry_count: u64,
    pub total_size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_content_store_write_and_read() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let content = b"Hello, World!";
        let handle = store.write(content).unwrap();

        assert!(handle.starts_with("sha256:"));
        assert_eq!(store.read(&handle).unwrap(), content);
    }

    #[test]
    fn project_live_materializes_drafts_refreshes_and_skips_unsafe_names() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();
        let session = "root-proj";

        let h1 = store.write(b"port: 8080\n").unwrap();
        let h2 = store.write(b"fn main() {}\n").unwrap();
        store.register_name(session, "config.yaml", &h1).unwrap();
        store.register_name(session, "src/main.rs", &h2).unwrap(); // nested
                                                                   // A malicious/odd name must never escape the live directory.
        store.register_name(session, "../escape.txt", &h1).unwrap();
        // Empty and `.`-only names resolve to the dir itself — must be skipped,
        // not error the whole projection.
        store.register_name(session, "", &h1).unwrap();
        store.register_name(session, ".", &h1).unwrap();

        let (dir, written) = store.project_live(session).unwrap();
        assert!(dir.ends_with("live"));
        assert!(written.contains(&"config.yaml".to_string()));
        assert!(written.contains(&"src/main.rs".to_string()));
        assert!(
            !written.iter().any(|n| n.contains("..")),
            "unsafe name must be skipped: {written:?}"
        );
        assert_eq!(
            std::fs::read(dir.join("config.yaml")).unwrap(),
            b"port: 8080\n"
        );
        assert_eq!(
            std::fs::read(dir.join("src/main.rs")).unwrap(),
            b"fn main() {}\n"
        );
        // The traversal name must not have written outside `live/`.
        assert!(!dir.parent().unwrap().join("escape.txt").exists());

        // Refresh: drop config.yaml from the manifest, reproject → stale file gone.
        store.cleanup_session(session).unwrap();
        store.register_name(session, "only.txt", &h2).unwrap();
        let (dir2, written2) = store.project_live(session).unwrap();
        assert_eq!(written2, vec!["only.txt".to_string()]);
        assert!(
            !dir2.join("config.yaml").exists(),
            "stale file should be cleared"
        );
        assert!(dir2.join("only.txt").exists());
    }

    #[test]
    fn project_live_rejects_unsafe_session_id() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();
        for bad in ["../escape", "/abs", "..", ""] {
            assert!(
                store.project_live(bad).is_err(),
                "unsafe session_id {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn safe_relative_path_classifies_correctly() {
        assert!(safe_relative_path("config.yaml"));
        assert!(safe_relative_path("src/main.rs"));
        assert!(safe_relative_path("./a")); // CurDir + Normal is fine
        assert!(safe_relative_path("root/agent.coder")); // nested session id
        assert!(!safe_relative_path(""));
        assert!(!safe_relative_path("."));
        assert!(!safe_relative_path(".."));
        assert!(!safe_relative_path("../x"));
        assert!(!safe_relative_path("/abs"));
    }

    #[test]
    fn test_content_store_deduplication() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let content = b"Same content";
        let handle1 = store.write(content).unwrap();
        let handle2 = store.write(content).unwrap();

        assert_eq!(handle1, handle2);
    }

    #[test]
    fn test_content_store_session_manifest() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let content = b"Script content";
        let handle = store.write(content).unwrap();

        store
            .register_name("session-1", "main.py", &handle)
            .unwrap();

        let resolved = store.resolve_name("session-1", "main.py").unwrap();
        assert_eq!(resolved, handle);

        let content_back = store.read_by_name("session-1", "main.py").unwrap();
        assert_eq!(content_back, content);
    }

    #[test]
    fn test_content_store_read_by_name_or_handle() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let content = b"Test content";
        let handle = store.write(content).unwrap();
        store
            .register_name("session-1", "test.txt", &handle)
            .unwrap();

        // Read by name
        let by_name = store
            .read_by_name_or_handle("session-1", "test.txt")
            .unwrap();
        assert_eq!(by_name, content);

        // Read by handle
        let by_handle = store.read_by_name_or_handle("session-1", &handle).unwrap();
        assert_eq!(by_handle, content);

        let alias = ContentStore::get_short_alias(&handle);
        let by_cnt = store
            .read_by_name_or_handle("session-1", &format!("cnt_{}", alias))
            .unwrap();
        assert_eq!(by_cnt, content);
    }

    #[test]
    fn test_resolve_name_or_handle_to_handle_matches_read() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let content = b"resolve probe";
        let handle = store.write(content).unwrap();
        store.register_name("session-1", "f.py", &handle).unwrap();
        let short = ContentStore::get_short_alias(&handle);
        let cnt_ref = format!("cnt_{}", short);

        for probe in ["f.py", handle.as_str(), cnt_ref.as_str(), short.as_str()] {
            let h = store
                .resolve_name_or_handle_to_handle("session-1", probe)
                .unwrap();
            assert_eq!(store.read(&h).unwrap(), content);
            assert_eq!(
                store.read_by_name_or_handle("session-1", probe).unwrap(),
                content
            );
        }
    }

    #[test]
    fn test_root_session_visibility() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let parent_session = "demo-session";
        let child_session = "demo-session/coder-abc123";

        store
            .set_root_session(child_session, parent_session)
            .unwrap();

        // Child writes content with session visibility
        let content = b"print('Hello from coder')";
        let handle = store.write(content).unwrap();
        store
            .register_name_with_visibility(
                child_session,
                "weather.py",
                &handle,
                ContentVisibility::Session,
            )
            .unwrap();

        // Child can read its own content
        let child_read = store
            .read_by_name_or_handle(child_session, "weather.py")
            .unwrap();
        assert_eq!(child_read, content);

        // Parent (root session) can read child's content
        let parent_read = store
            .read_by_name_or_handle(parent_session, "weather.py")
            .unwrap();
        assert_eq!(parent_read, content);

        // Full handle also works
        let parent_read_handle = store
            .read_by_name_or_handle(parent_session, &handle)
            .unwrap();
        assert_eq!(parent_read_handle, content);

        // Short alias also works
        let short_alias = ContentStore::get_short_alias(&handle);
        let parent_read_alias = store
            .read_by_name_or_handle(parent_session, &short_alias)
            .unwrap();
        assert_eq!(parent_read_alias, content);
    }

    #[test]
    fn test_private_visibility_isolates_from_root() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let parent_session = "demo-session";
        let child_session = "demo-session/coder-abc123";

        store
            .set_root_session(child_session, parent_session)
            .unwrap();

        // Child writes private content
        let content = b"private scratchpad";
        let handle = store.write(content).unwrap();
        store
            .register_name_with_visibility(
                child_session,
                "scratch.txt",
                &handle,
                ContentVisibility::Private,
            )
            .unwrap();

        // Child can read its own content
        let child_read = store
            .read_by_name_or_handle(child_session, "scratch.txt")
            .unwrap();
        assert_eq!(child_read, content);

        // Parent CANNOT read child's private content by name
        let parent_attempt = store.read_by_name_or_handle(parent_session, "scratch.txt");
        assert!(parent_attempt.is_err());
    }

    #[test]
    fn test_sibling_session_visibility() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let parent_session = "demo-session";
        let child1_session = "demo-session/coder-abc";
        let child2_session = "demo-session/coder-def";

        store
            .set_root_session(child1_session, parent_session)
            .unwrap();
        store
            .set_root_session(child2_session, parent_session)
            .unwrap();

        // Child1 writes session-visible content
        let content1 = b"child1 output";
        let handle1 = store.write(content1).unwrap();
        store
            .register_name_with_visibility(
                child1_session,
                "output.py",
                &handle1,
                ContentVisibility::Session,
            )
            .unwrap();

        // Child2 can read sibling's session-visible content via root
        let child2_read = store
            .read_by_name_or_handle(child2_session, "output.py")
            .unwrap();
        assert_eq!(child2_read, content1);

        // Child1 writes private content
        let content2 = b"child1 private";
        let handle2 = store.write(content2).unwrap();
        store
            .register_name_with_visibility(
                child1_session,
                "draft.py",
                &handle2,
                ContentVisibility::Private,
            )
            .unwrap();

        // Child2 cannot read sibling's private content
        let child2_attempt = store.read_by_name_or_handle(child2_session, "draft.py");
        assert!(child2_attempt.is_err());
    }

    #[test]
    fn test_root_session_last_writer_wins() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let parent_session = "demo-session";
        let child1_session = "demo-session/coder-abc";
        let child2_session = "demo-session/coder-def";

        store
            .set_root_session(child1_session, parent_session)
            .unwrap();
        store
            .set_root_session(child2_session, parent_session)
            .unwrap();

        // Both write to same filename
        let content1 = b"first version";
        let handle1 = store.write(content1).unwrap();
        store
            .register_name_with_visibility(
                child1_session,
                "output.txt",
                &handle1,
                ContentVisibility::Session,
            )
            .unwrap();

        let content2 = b"second version";
        let handle2 = store.write(content2).unwrap();
        store
            .register_name_with_visibility(
                child2_session,
                "output.txt",
                &handle2,
                ContentVisibility::Session,
            )
            .unwrap();

        // Root session gets the last writer's content
        let root_read = store
            .read_by_name_or_handle(parent_session, "output.txt")
            .unwrap();
        assert_eq!(root_read, content2);

        // Each child can still read its own version
        let child1_read = store
            .read_by_name_or_handle(child1_session, "output.txt")
            .unwrap();
        assert_eq!(child1_read, content1);

        let child2_read = store
            .read_by_name_or_handle(child2_session, "output.txt")
            .unwrap();
        assert_eq!(child2_read, content2);
    }

    #[test]
    fn test_content_store_list_names() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let h1 = store.write(b"file1").unwrap();
        let h2 = store.write(b"file2").unwrap();

        store.register_name("session-1", "a.txt", &h1).unwrap();
        store.register_name("session-1", "b.txt", &h2).unwrap();

        let names = store.list_names("session-1").unwrap();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn test_content_store_stats() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        store.write(b"content1").unwrap();
        store.write(b"content2").unwrap();
        store.write(b"content1").unwrap(); // duplicate

        let stats = store.stats().unwrap();
        assert_eq!(stats.entry_count, 2); // deduplicated
        assert!(stats.total_size_bytes > 0);
    }

    #[test]
    fn test_content_store_short_alias() {
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();

        let content = b"test content for alias";
        let handle = store.write(content).unwrap();
        store
            .register_name("session-1", "test.txt", &handle)
            .unwrap();

        // Get the short alias
        let short_alias = ContentStore::get_short_alias(&handle);
        assert_eq!(short_alias.len(), SHORT_ALIAS_LEN);
        assert!(short_alias.chars().all(|c| c.is_ascii_hexdigit()));

        // Read using short alias
        let result = store
            .read_by_name_or_handle("session-1", &short_alias)
            .unwrap();
        assert_eq!(result, content);

        // Verify full handle still works
        let result2 = store.read_by_name_or_handle("session-1", &handle).unwrap();
        assert_eq!(result2, content);
    }

    #[test]
    fn test_manifest_updates_merge_across_store_instances() {
        let temp = tempdir().unwrap();

        let store1 = ContentStore::new(temp.path()).unwrap();
        let h1 = store1.write(b"first").unwrap();
        store1.register_name("session-1", "a.txt", &h1).unwrap();

        // Simulate a later tool call with a fresh ContentStore instance.
        let store2 = ContentStore::new(temp.path()).unwrap();
        let h2 = store2.write(b"second").unwrap();
        store2.register_name("session-1", "b.txt", &h2).unwrap();

        let manifest = store2.load_manifest("session-1").unwrap();
        assert_eq!(manifest.names.get("a.txt"), Some(&h1));
        assert_eq!(manifest.names.get("b.txt"), Some(&h2));
    }

    #[test]
    fn test_root_session_preserved_across_instances() {
        let temp = tempdir().unwrap();
        let child = "demo-session/coder-123";
        let parent = "demo-session";

        let store1 = ContentStore::new(temp.path()).unwrap();
        store1.set_root_session(child, parent).unwrap();

        let store2 = ContentStore::new(temp.path()).unwrap();
        let h = store2.write(b"print('hi')").unwrap();
        store2.register_name(child, "weather.py", &h).unwrap();

        let manifest = store2.load_manifest(child).unwrap();
        assert_eq!(manifest.root_session_id.as_deref(), Some(parent));
        assert_eq!(manifest.names.get("weather.py"), Some(&h));
    }

    #[test]
    fn test_root_session_id_helper() {
        assert_eq!(root_session_id("demo-session"), "demo-session");
        assert_eq!(
            root_session_id("demo-session/coder.default-abc"),
            "demo-session"
        );
        assert_eq!(root_session_id("a/b/c"), "a");
    }

    #[test]
    fn list_with_handles_and_visibility_then_read_round_trip() {
        // Exercises the exact path the `content.list` + `content.read` JSON-RPC
        // methods (router.rs) use: write a few entries under different
        // visibilities, list them with handles + visibility, and read each back
        // by name. This is the foundation of the Pillar-D content-tree pane.
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();
        let session = "root-session-content";

        let h1 = store.write(b"# title\n\nbody of draft one").unwrap();
        store
            .register_name_with_visibility(
                session,
                "skills/weather/SKILL.md",
                &h1,
                ContentVisibility::Session,
            )
            .unwrap();
        let h2 = store.write(b"SECRET-LIKE").unwrap();
        store
            .register_name_with_visibility(
                session,
                "config/secrets.yaml",
                &h2,
                ContentVisibility::Private,
            )
            .unwrap();

        // list_names_with_handles returns (name, handle), sorted by name.
        let listed = store.list_names_with_handles(session).unwrap();
        assert_eq!(listed.len(), 2, "both drafts should be listed from t=0");
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["config/secrets.yaml", "skills/weather/SKILL.md"]
        );

        // load_manifest gives visibility — the RPC's visibility badge source.
        let manifest = store.load_manifest(session).unwrap();
        let vis_for = |name: &str| -> &'static str {
            let (_, handle) = listed.iter().find(|(n, _)| n == name).unwrap();
            match manifest
                .visibility
                .get(handle)
                .copied()
                .unwrap_or(ContentVisibility::Session)
            {
                ContentVisibility::Private => "private",
                ContentVisibility::Session => "session",
                ContentVisibility::Global => "global",
            }
        };
        assert_eq!(vis_for("config/secrets.yaml"), "private");
        assert_eq!(vis_for("skills/weather/SKILL.md"), "session");

        // read_by_name_or_handle resolves each name back to its bytes.
        let one = store
            .read_by_name_or_handle(session, "skills/weather/SKILL.md")
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&one),
            "# title\n\nbody of draft one"
        );
        let two = store
            .read_by_name_or_handle(session, "config/secrets.yaml")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&two), "SECRET-LIKE");
    }

    #[test]
    fn global_manifest_probe_for_cross_session_visibility() {
        // The `content.list` JSON-RPC method (router.rs) probes the
        // GLOBAL_SESSION_ID manifest so a global entry written by a
        // child session is labelled "global" (not "session") when the
        // operator lists from a parent session. This test exercises the
        // underlying ContentStore API that the router relies on.
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();
        let child = "root-x/child-a";

        let h = store.write(b"shared").unwrap();
        store
            .register_name_with_visibility(child, "shared/lib.py", &h, ContentVisibility::Global)
            .unwrap();

        // The local child manifest knows the handle + global visibility.
        let child_manifest = store.load_manifest(child).unwrap();
        assert_eq!(
            child_manifest.visibility.get(&h).copied(),
            Some(ContentVisibility::Global)
        );

        // The global sentinel manifest contains the handle (this is the
        // probe the router uses to promote missing-local entries to
        // "global").
        let global_manifest = store
            .load_manifest(GLOBAL_SESSION_ID)
            .expect("global manifest must exist after a global register");
        let global_handles: std::collections::HashSet<String> =
            global_manifest.names.values().cloned().collect();
        assert!(
            global_handles.contains(&h),
            "the global manifest must record the handle for cross-session probes"
        );

        let got = store
            .read_by_name_or_handle(child, "shared/lib.py")
            .unwrap();
        assert_eq!(got, b"shared");
    }

    #[test]
    fn resolve_handle_then_read_succeeds_for_named_entry() {
        // Mirrors the router `content.read` path (Fix 2): resolve the
        // handle once, then read by the resolved handle. Ensures the
        // name->handle->bytes path returns the same content and the
        // resolved handle is non-empty. Also locks in that a missing
        // name surfaces a clear error (the bug Fix 2 closed).
        let temp = tempdir().unwrap();
        let store = ContentStore::new(temp.path()).unwrap();
        let sid = "root-rs";
        let h = store.write(b"body-of-draft").unwrap();
        store
            .register_name_with_visibility(sid, "notes.md", &h, ContentVisibility::Session)
            .unwrap();

        let resolved = store
            .resolve_name_or_handle_to_handle(sid, "notes.md")
            .expect("name must resolve");
        assert_eq!(resolved, h);

        let bytes = store.read_by_name_or_handle(sid, &resolved).unwrap();
        assert_eq!(bytes, b"body-of-draft");

        let err = store
            .resolve_name_or_handle_to_handle(sid, "missing.md")
            .err()
            .expect("missing name must fail to resolve");
        assert!(
            err.to_string().contains("missing.md")
                || err.to_string().to_lowercase().contains("not found"),
            "resolve error should mention the missing name; got: {err}"
        );
    }
}
