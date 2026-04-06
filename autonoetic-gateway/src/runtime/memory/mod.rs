//! Agent Memory Tier 1 and Tier 2 with provenance tracking.
//!
//! The memory system is split into two tiers:
//! - **Tier 1**: Working state directory (`state/`) — flat files for immediate use
//! - **Tier 2**: Gateway-managed long-term storage with provenance, backed by a
//!   pluggable [`MemoryStore`] trait

mod sqlite_store;

use async_trait::async_trait;
use autonoetic_types::memory::MemoryObject;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::scheduler::gateway_store::GatewayStore;

pub use sqlite_store::SqliteMemoryStore;

// ---------------------------------------------------------------------------
// MemoryStore trait
// ---------------------------------------------------------------------------

/// Pluggable backend for Tier 2 memory persistence.
///
/// Implementations store and retrieve [`MemoryObject`] records. The default
/// backend is [`SqliteMemoryStore`] which wraps the existing `GatewayStore`
/// SQLite methods.
///
/// # Adding a new backend
///
/// 1. Implement this trait (e.g. `HonchoMemoryStore`)
/// 2. Add a feature-gated branch in `build_memory_store()` in `server/mod.rs`
/// 3. Set `memory_backend.backend_type` in gateway config YAML
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Insert or replace a memory record.
    async fn upsert(&self, memory: &MemoryObject) -> anyhow::Result<()>;

    /// Retrieve a memory by ID, without visibility checks.
    /// Returns `None` if not found.
    async fn get(&self, memory_id: &str) -> anyhow::Result<Option<MemoryObject>>;

    /// List memory IDs for a given scope, optionally filtered by content substring.
    /// Results ordered by `updated_at` descending.
    async fn list_ids_for_scope(
        &self,
        scope: &str,
        content_substr: Option<&str>,
    ) -> anyhow::Result<Vec<String>>;

    /// List memory IDs matching ALL given tags within a scope, filtered by
    /// visibility for `agent_id`. Optional content substring filter.
    /// Results ordered by `updated_at` descending, capped by `limit`.
    async fn list_ids_matching_tags(
        &self,
        scope: &str,
        agent_id: &str,
        tags: &[String],
        content_substr: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<String>>;

    /// List memory IDs owned by a specific agent.
    /// Results ordered by `created_at` descending.
    async fn list_ids_owned_by(&self, owner_agent_id: &str) -> anyhow::Result<Vec<String>>;

    /// List distinct scopes visible to an agent (based on visibility rules).
    /// Results ordered alphabetically.
    async fn list_scopes_for_agent(&self, agent_id: &str) -> anyhow::Result<Vec<String>>;
}

// ---------------------------------------------------------------------------
// Tier 1 Memory
// ---------------------------------------------------------------------------

/// Tier 1 Memory: Working state directory (`state/`).
/// Flat files for the agent's immediate situational awareness.
pub struct Tier1Memory {
    state_dir: PathBuf,
}

impl Tier1Memory {
    pub fn new(agent_dir: &Path) -> anyhow::Result<Self> {
        let state_dir = agent_dir.join("state");
        std::fs::create_dir_all(&state_dir)?;
        Ok(Self { state_dir })
    }

    pub fn write_file(&self, filename: &str, content: &str) -> anyhow::Result<()> {
        // Basic path traversal prevention
        if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
            anyhow::bail!("Invalid filename mapping");
        }
        std::fs::write(self.state_dir.join(filename), content)?;
        Ok(())
    }

    pub fn read_file(&self, filename: &str) -> anyhow::Result<String> {
        let path = self.state_dir.join(filename);
        if !path.exists() {
            anyhow::bail!("File not found in Tier 1 memory");
        }
        Ok(std::fs::read_to_string(path)?)
    }
}

// ---------------------------------------------------------------------------
// Tier 2 Memory
// ---------------------------------------------------------------------------

/// Tier 2 Memory: Gateway-managed long-term storage with provenance tracking.
///
/// This is the gateway-owned source of truth for durable facts and cross-agent recall.
/// All memory records include full provenance (writer, source, timestamps, content hash).
///
/// Storage is delegated to a [`MemoryStore`] backend (default: SQLite via
/// [`SqliteMemoryStore`]).
pub struct Tier2Memory {
    store: Arc<dyn MemoryStore>,
    /// The agent ID that is currently using this memory instance.
    current_agent_id: String,
}

impl Tier2Memory {
    /// Primary constructor — takes any [`MemoryStore`] implementation.
    pub fn new(store: Arc<dyn MemoryStore>, agent_id: impl Into<String>) -> Self {
        Self {
            store,
            current_agent_id: agent_id.into(),
        }
    }

    /// Convenience: opens SQLite store at `gateway_dir` for backwards compatibility.
    pub fn open_sqlite(gateway_dir: &Path, agent_id: &str) -> anyhow::Result<Self> {
        let gw = Arc::new(GatewayStore::open(gateway_dir)?);
        let store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(gw));
        Ok(Self::new(store, agent_id.to_string()))
    }

    /// Uses an existing memory store when provided; otherwise opens SQLite at `gateway_dir`.
    pub fn open_for_agent(
        gateway_dir: &Path,
        memory_store: Option<Arc<dyn MemoryStore>>,
        agent_id: &str,
    ) -> anyhow::Result<Self> {
        match memory_store {
            Some(store) => Ok(Self::new(store, agent_id.to_string())),
            None => Self::open_sqlite(gateway_dir, agent_id),
        }
    }

    /// Stores a new memory record or updates an existing one.
    ///
    /// # Arguments
    /// * `memory_id` - Unique identifier for the memory
    /// * `scope` - Scope/namespace for organizing memory
    /// * `owner_agent_id` - Agent that owns this memory
    /// * `source_ref` - Reference to causal chain entry or session
    /// * `content` - The content to store
    pub async fn remember(
        &self,
        memory_id: &str,
        scope: &str,
        owner_agent_id: &str,
        source_ref: &str,
        content: &str,
    ) -> anyhow::Result<MemoryObject> {
        let memory = MemoryObject::new(
            memory_id.to_string(),
            scope.to_string(),
            owner_agent_id.to_string(),
            self.current_agent_id.clone(),
            source_ref.to_string(),
            content.to_string(),
        );

        self.save_memory(&memory).await
    }

    /// Saves a MemoryObject to the database.
    pub async fn save_memory(&self, memory: &MemoryObject) -> anyhow::Result<MemoryObject> {
        self.store.upsert(memory).await?;
        Ok(memory.clone())
    }

    /// Recalls a memory by its ID.
    ///
    /// Enforces visibility/ACL checks based on the current agent.
    pub async fn recall(&self, memory_id: &str) -> anyhow::Result<MemoryObject> {
        let Some(memory) = self.store.get(memory_id).await? else {
            anyhow::bail!("Memory '{}' not found", memory_id);
        };

        // Enforce visibility check
        if !memory.is_readable_by(&self.current_agent_id) {
            anyhow::bail!(
                "Memory '{}' is not accessible to agent '{}'",
                memory_id,
                self.current_agent_id
            );
        }

        Ok(memory)
    }

    /// Searches memories by scope and optional query terms.
    ///
    /// Returns memories that match the scope and are visible to the current agent.
    pub async fn search(
        &self,
        scope: &str,
        query: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryObject>> {
        let ids = self.store.list_ids_for_scope(scope, query).await?;

        let mut results = Vec::new();
        for memory_id in ids {
            // Only include memories visible to current agent
            // Propagate errors for debugging DB/serde issues
            match self.recall(&memory_id).await {
                Ok(memory) => results.push(memory),
                Err(e) => {
                    // Log the error for debugging but don't fail the entire search
                    tracing::warn!(
                        "Failed to recall memory '{}' during search: {}",
                        memory_id,
                        e
                    );
                }
            }
        }

        Ok(results)
    }

    /// Returns memories in `scope` whose JSON `tags` array contains every string in `tags`,
    /// optionally filtered by substring match on `text`, visible to the current agent.
    ///
    /// `tags` must be non-empty.
    pub async fn search_by_tags(
        &self,
        scope: &str,
        tags: &[String],
        text: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryObject>> {
        anyhow::ensure!(!tags.is_empty(), "tags must not be empty");
        anyhow::ensure!(
            (1..=100).contains(&limit),
            "limit must be between 1 and 100 inclusive"
        );

        let ids = self
            .store
            .list_ids_matching_tags(scope, &self.current_agent_id, tags, text, limit as i64)
            .await?;

        let mut results = Vec::new();
        for memory_id in ids {
            if results.len() >= limit {
                break;
            }
            let memory = match self.recall(&memory_id).await {
                Ok(m) => m,
                Err(_) => continue,
            };
            results.push(memory);
        }

        Ok(results)
    }

    /// Shares a memory with specific agents.
    ///
    /// Requires the current agent to be the owner or writer.
    pub async fn share_with(
        &self,
        memory_id: &str,
        target_agents: Vec<String>,
    ) -> anyhow::Result<MemoryObject> {
        let memory = self.recall(memory_id).await?;

        // Only owner or writer can share
        if memory.owner_agent_id != self.current_agent_id
            && memory.writer_agent_id != self.current_agent_id
        {
            anyhow::bail!("Only the owner or writer can share a memory");
        }

        let updated = memory.share_with(target_agents);
        self.save_memory(&updated).await?;

        Ok(updated)
    }

    /// Makes a memory globally visible.
    pub async fn make_global(&self, memory_id: &str) -> anyhow::Result<MemoryObject> {
        let memory = self.recall(memory_id).await?;

        // Only owner can make global
        if memory.owner_agent_id != self.current_agent_id {
            anyhow::bail!("Only the owner can make a memory global");
        }

        let updated = memory.make_global();
        self.save_memory(&updated).await?;

        Ok(updated)
    }

    /// Lists all scopes available to the current agent.
    /// Only returns scopes where the agent has at least one visible memory.
    pub async fn list_scopes(&self) -> anyhow::Result<Vec<String>> {
        self.store
            .list_scopes_for_agent(&self.current_agent_id)
            .await
    }

    /// Lists all memories owned by the current agent.
    pub async fn list_memories(&self) -> anyhow::Result<Vec<MemoryObject>> {
        let ids = self
            .store
            .list_ids_owned_by(&self.current_agent_id)
            .await?;

        let mut memories = Vec::new();
        for memory_id in ids {
            match self.recall(&memory_id).await {
                Ok(memory) => memories.push(memory),
                Err(e) => {
                    tracing::warn!("Failed to recall memory '{}' during list: {}", memory_id, e);
                }
            }
        }

        Ok(memories)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::memory::{MemorySourceType, MemoryVisibility};

    #[test]
    fn test_tier1_memory() {
        let temp = tempfile::tempdir().unwrap();
        let mem = Tier1Memory::new(temp.path()).unwrap();

        mem.write_file("notes.txt", "hello world").unwrap();
        assert_eq!(mem.read_file("notes.txt").unwrap(), "hello world");
        assert!(mem.write_file("../out.txt", "hacker").is_err());
    }

    #[tokio::test]
    async fn test_tier2_memory_basic() {
        let temp = tempfile::tempdir().unwrap();
        let mem = Tier2Memory::open_sqlite(temp.path(), "agent-1").unwrap();

        let memory = mem
            .remember(
                "fact_1",
                "general",
                "agent-1",
                "session:test:turn:1",
                "The sky is blue",
            )
            .await
            .unwrap();

        assert_eq!(memory.memory_id, "fact_1");
        assert_eq!(memory.content, "The sky is blue");
        assert_eq!(memory.owner_agent_id, "agent-1");
        assert_eq!(memory.visibility, MemoryVisibility::Private);

        // Verify content hash is set
        assert!(!memory.content_hash.is_empty());
    }

    #[tokio::test]
    async fn test_tier2_memory_recall() {
        let temp = tempfile::tempdir().unwrap();
        let mem = Tier2Memory::open_sqlite(temp.path(), "agent-1").unwrap();

        mem.remember(
            "fact_1",
            "general",
            "agent-1",
            "session:test:turn:1",
            "The sky is blue",
        )
        .await
        .unwrap();

        let recalled = mem.recall("fact_1").await.unwrap();
        assert_eq!(recalled.content, "The sky is blue");

        // Non-existent memory should fail
        assert!(mem.recall("fact_2").await.is_err());
    }

    #[tokio::test]
    async fn test_tier2_memory_visibility_private() {
        let temp = tempfile::tempdir().unwrap();
        let mem1 = Tier2Memory::open_sqlite(temp.path(), "agent-1").unwrap();
        let mem2 = Tier2Memory::open_sqlite(temp.path(), "agent-2").unwrap();

        mem1
            .remember(
                "fact_1",
                "general",
                "agent-1",
                "session:test:turn:1",
                "Private fact",
            )
            .await
            .unwrap();

        // agent-1 can read its own memory
        assert!(mem1.recall("fact_1").await.is_ok());

        // agent-2 cannot read agent-1's private memory
        assert!(mem2.recall("fact_1").await.is_err());
    }

    #[tokio::test]
    async fn test_tier2_memory_sharing() {
        let temp = tempfile::tempdir().unwrap();
        let mem1 = Tier2Memory::open_sqlite(temp.path(), "agent-1").unwrap();
        let mem2 = Tier2Memory::open_sqlite(temp.path(), "agent-2").unwrap();

        mem1
            .remember(
                "fact_1",
                "general",
                "agent-1",
                "session:test:turn:1",
                "Shared fact",
            )
            .await
            .unwrap();

        // Share with agent-2
        mem1
            .share_with("fact_1", vec!["agent-2".to_string()])
            .await
            .unwrap();

        // Now agent-2 can read it
        let recalled = mem2.recall("fact_1").await.unwrap();
        assert_eq!(recalled.content, "Shared fact");
        assert_eq!(recalled.visibility, MemoryVisibility::Shared);
        assert!(recalled.allowed_agents.contains(&"agent-2".to_string()));
    }

    #[tokio::test]
    async fn test_tier2_memory_global() {
        let temp = tempfile::tempdir().unwrap();
        let mem1 = Tier2Memory::open_sqlite(temp.path(), "agent-1").unwrap();
        let mem2 = Tier2Memory::open_sqlite(temp.path(), "agent-2").unwrap();

        mem1
            .remember(
                "fact_1",
                "general",
                "agent-1",
                "session:test:turn:1",
                "Global fact",
            )
            .await
            .unwrap();

        // Make global
        mem1.make_global("fact_1").await.unwrap();

        // All agents can read it
        assert!(mem1.recall("fact_1").await.is_ok());
        assert!(mem2.recall("fact_1").await.is_ok());
    }

    #[tokio::test]
    async fn test_tier2_memory_search() {
        let temp = tempfile::tempdir().unwrap();
        let mem = Tier2Memory::open_sqlite(temp.path(), "agent-1").unwrap();

        mem.remember(
            "fact_1",
            "weather",
            "agent-1",
            "session:test:turn:1",
            "Paris is sunny",
        )
        .await
        .unwrap();

        mem.remember(
            "fact_2",
            "weather",
            "agent-1",
            "session:test:turn:2",
            "London is rainy",
        )
        .await
        .unwrap();

        mem.remember(
            "fact_3",
            "geography",
            "agent-1",
            "session:test:turn:3",
            "Paris is in France",
        )
        .await
        .unwrap();

        // Search by scope
        let results = mem.search("weather", None).await.unwrap();
        assert_eq!(results.len(), 2);

        // Search by scope and query
        let results = mem.search("weather", Some("Paris")).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory_id, "fact_1");
    }

    #[tokio::test]
    async fn test_tier2_memory_search_by_tags_requires_nonempty_tags() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
        let mem_store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(store));
        let mem = Tier2Memory::new(mem_store, "agent-1");
        assert!(mem
            .search_by_tags("general", &[], None, 10)
            .await
            .unwrap_err()
            .to_string()
            .contains("tags must not be empty"));
    }

    #[tokio::test]
    async fn test_tier2_memory_search_by_tags_limit_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
        let mem_store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(store));
        let mem = Tier2Memory::new(mem_store, "agent-1");
        let err0 = mem
            .search_by_tags("general", &["t".to_string()], None, 0)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err0.contains("limit must be between 1 and 100 inclusive"),
            "{}",
            err0
        );
        let err101 = mem
            .search_by_tags("general", &["t".to_string()], None, 101)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err101.contains("limit must be between 1 and 100 inclusive"),
            "{}",
            err101
        );
    }

    #[tokio::test]
    async fn test_tier2_memory_search_by_tags_filters() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
        let mem_store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(Arc::clone(&store)));
        let mem = Tier2Memory::new(mem_store, "agent-1");

        let mut m1 = MemoryObject::new(
            "m1".into(),
            "lessons".into(),
            "agent-1".into(),
            "agent-1".into(),
            "ref:1".into(),
            "async needs Send".into(),
        );
        m1.tags = vec!["type:error_lesson".to_string(), "domain:http".to_string()];
        mem.save_memory(&m1).await.unwrap();

        let mut m2 = MemoryObject::new(
            "m2".into(),
            "lessons".into(),
            "agent-1".into(),
            "agent-1".into(),
            "ref:2".into(),
            "other".into(),
        );
        m2.tags = vec!["type:fact".to_string()];
        mem.save_memory(&m2).await.unwrap();

        let found = mem
            .search_by_tags("lessons", &["type:error_lesson".to_string()], None, 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].memory_id, "m1");

        let found2 = mem
            .search_by_tags(
                "lessons",
                &["type:error_lesson".to_string(), "domain:http".to_string()],
                None,
                10,
            )
            .await
            .unwrap();
        assert_eq!(found2.len(), 1);

        let found_text = mem
            .search_by_tags(
                "lessons",
                &["type:error_lesson".to_string()],
                Some("Send"),
                10,
            )
            .await
            .unwrap();
        assert_eq!(found_text.len(), 1);
    }

    #[tokio::test]
    async fn test_tier2_memory_search_by_tags_limit_applies_after_visibility() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
        let mem_store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(Arc::clone(&store)));
        let writer = Tier2Memory::new(Arc::clone(&mem_store), "writer-agent");
        let reader = Tier2Memory::new(mem_store, "reader-agent");

        // Write a shared match first (older row).
        let mut shared = MemoryObject::new(
            "shared-hit".into(),
            "lessons".into(),
            "writer-agent".into(),
            "writer-agent".into(),
            "ref:shared".into(),
            "Readable memory".into(),
        );
        shared.tags = vec!["topic:rust".to_string()];
        writer.save_memory(&shared).await.unwrap();
        writer
            .share_with("shared-hit", vec!["reader-agent".to_string()])
            .await
            .unwrap();

        // Then write many newer private matches that reader cannot access.
        for i in 0..150 {
            let mut private = MemoryObject::new(
                format!("private-{}", i),
                "lessons".into(),
                "writer-agent".into(),
                "writer-agent".into(),
                format!("ref:{}", i),
                format!("Private {}", i),
            );
            private.tags = vec!["topic:rust".to_string()];
            writer.save_memory(&private).await.unwrap();
        }

        let found = reader
            .search_by_tags("lessons", &["topic:rust".to_string()], None, 1)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].memory_id, "shared-hit");
    }

    #[tokio::test]
    async fn test_tier2_memory_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let mem = Tier2Memory::open_sqlite(temp.path(), "agent-1").unwrap();

        let memory = mem
            .remember(
                "fact_1",
                "general",
                "agent-1",
                "session:abc123:turn:5",
                "Important fact",
            )
            .await
            .unwrap();

        // Verify provenance fields
        assert_eq!(memory.writer_agent_id, "agent-1");
        assert_eq!(memory.source_ref, "session:abc123:turn:5");
        assert_eq!(memory.source_type, MemorySourceType::AgentWrite);
        assert!(!memory.created_at.is_empty());
        assert!(!memory.updated_at.is_empty());
        assert!(!memory.content_hash.is_empty());
    }
}
