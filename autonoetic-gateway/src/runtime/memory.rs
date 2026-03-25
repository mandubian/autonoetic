//! Agent Memory Tier 1 and Tier 2 with provenance tracking.

use autonoetic_types::memory::MemoryObject;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::scheduler::gateway_store::GatewayStore;

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

/// Tier 2 Memory: Gateway-managed long-term storage with provenance tracking.
///
/// This is the gateway-owned source of truth for durable facts and cross-agent recall.
/// All memory records include full provenance (writer, source, timestamps, content hash).
/// Rows live in `gateway.db` (`memories` table).
pub struct Tier2Memory {
    store: Arc<GatewayStore>,
    /// The agent ID that is currently using this memory instance.
    current_agent_id: String,
}

impl Tier2Memory {
    pub fn with_store(store: Arc<GatewayStore>, agent_id: impl Into<String>) -> Self {
        Self {
            store,
            current_agent_id: agent_id.into(),
        }
    }

    /// Opens the gateway store for `gateway_dir` and constructs Tier 2 memory for `agent_id`.
    pub fn new(gateway_dir: &Path, agent_id: &str) -> anyhow::Result<Self> {
        let store = Arc::new(GatewayStore::open(gateway_dir)?);
        Ok(Self::with_store(store, agent_id.to_string()))
    }

    /// Uses an existing store when the runtime already holds `Arc<GatewayStore>`; otherwise opens `gateway_dir`.
    pub fn open_for_agent(
        gateway_dir: &Path,
        gateway_store: Option<Arc<GatewayStore>>,
        agent_id: &str,
    ) -> anyhow::Result<Self> {
        match gateway_store {
            Some(gs) => Ok(Self::with_store(gs, agent_id.to_string())),
            None => Self::new(gateway_dir, agent_id),
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
    pub fn remember(
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

        self.save_memory(&memory)
    }

    /// Saves a MemoryObject to the database.
    pub fn save_memory(&self, memory: &MemoryObject) -> anyhow::Result<MemoryObject> {
        self.store.memory_upsert(memory)?;
        Ok(memory.clone())
    }

    /// Recalls a memory by its ID.
    ///
    /// Enforces visibility/ACL checks based on the current agent.
    pub fn recall(&self, memory_id: &str) -> anyhow::Result<MemoryObject> {
        let Some(memory) = self.store.memory_get_unrestricted(memory_id)? else {
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
    pub fn search(&self, scope: &str, query: Option<&str>) -> anyhow::Result<Vec<MemoryObject>> {
        let ids = self.store.memory_list_ids_for_scope(scope, query)?;

        let mut results = Vec::new();
        for memory_id in ids {
            // Only include memories visible to current agent
            // Propagate errors for debugging DB/serde issues
            match self.recall(&memory_id) {
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
    pub fn search_by_tags(
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
            .memory_list_ids_matching_tags(scope, &self.current_agent_id, tags, text, limit as i64)?;

        let mut results = Vec::new();
        for memory_id in ids {
            if results.len() >= limit {
                break;
            }
            let memory = match self.recall(&memory_id) {
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
    pub fn share_with(
        &self,
        memory_id: &str,
        target_agents: Vec<String>,
    ) -> anyhow::Result<MemoryObject> {
        let memory = self.recall(memory_id)?;

        // Only owner or writer can share
        if memory.owner_agent_id != self.current_agent_id
            && memory.writer_agent_id != self.current_agent_id
        {
            anyhow::bail!("Only the owner or writer can share a memory");
        }

        let updated = memory.share_with(target_agents);
        self.save_memory(&updated)?;

        Ok(updated)
    }

    /// Makes a memory globally visible.
    pub fn make_global(&self, memory_id: &str) -> anyhow::Result<MemoryObject> {
        let memory = self.recall(memory_id)?;

        // Only owner can make global
        if memory.owner_agent_id != self.current_agent_id {
            anyhow::bail!("Only the owner can make a memory global");
        }

        let updated = memory.make_global();
        self.save_memory(&updated)?;

        Ok(updated)
    }

    /// Lists all scopes available to the current agent.
    /// Only returns scopes where the agent has at least one visible memory.
    pub fn list_scopes(&self) -> anyhow::Result<Vec<String>> {
        self.store
            .memory_list_scopes_for_agent(&self.current_agent_id)
    }

    /// Lists all memories owned by the current agent.
    pub fn list_memories(&self) -> anyhow::Result<Vec<MemoryObject>> {
        let ids = self
            .store
            .memory_list_ids_owned_by(&self.current_agent_id)?;

        let mut memories = Vec::new();
        for memory_id in ids {
            match self.recall(&memory_id) {
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

    #[test]
    fn test_tier2_memory_basic() {
        let temp = tempfile::tempdir().unwrap();
        let mem = Tier2Memory::new(temp.path(), "agent-1").unwrap();

        let memory = mem
            .remember(
                "fact_1",
                "general",
                "agent-1",
                "session:test:turn:1",
                "The sky is blue",
            )
            .unwrap();

        assert_eq!(memory.memory_id, "fact_1");
        assert_eq!(memory.content, "The sky is blue");
        assert_eq!(memory.owner_agent_id, "agent-1");
        assert_eq!(memory.visibility, MemoryVisibility::Private);

        // Verify content hash is set
        assert!(!memory.content_hash.is_empty());
    }

    #[test]
    fn test_tier2_memory_recall() {
        let temp = tempfile::tempdir().unwrap();
        let mem = Tier2Memory::new(temp.path(), "agent-1").unwrap();

        mem.remember(
            "fact_1",
            "general",
            "agent-1",
            "session:test:turn:1",
            "The sky is blue",
        )
        .unwrap();

        let recalled = mem.recall("fact_1").unwrap();
        assert_eq!(recalled.content, "The sky is blue");

        // Non-existent memory should fail
        assert!(mem.recall("fact_2").is_err());
    }

    #[test]
    fn test_tier2_memory_visibility_private() {
        let temp = tempfile::tempdir().unwrap();
        let mem1 = Tier2Memory::new(temp.path(), "agent-1").unwrap();
        let mem2 = Tier2Memory::new(temp.path(), "agent-2").unwrap();

        mem1.remember(
            "fact_1",
            "general",
            "agent-1",
            "session:test:turn:1",
            "Private fact",
        )
        .unwrap();

        // agent-1 can read its own memory
        assert!(mem1.recall("fact_1").is_ok());

        // agent-2 cannot read agent-1's private memory
        assert!(mem2.recall("fact_1").is_err());
    }

    #[test]
    fn test_tier2_memory_sharing() {
        let temp = tempfile::tempdir().unwrap();
        let mem1 = Tier2Memory::new(temp.path(), "agent-1").unwrap();
        let mem2 = Tier2Memory::new(temp.path(), "agent-2").unwrap();

        mem1.remember(
            "fact_1",
            "general",
            "agent-1",
            "session:test:turn:1",
            "Shared fact",
        )
        .unwrap();

        // Share with agent-2
        mem1.share_with("fact_1", vec!["agent-2".to_string()])
            .unwrap();

        // Now agent-2 can read it
        let recalled = mem2.recall("fact_1").unwrap();
        assert_eq!(recalled.content, "Shared fact");
        assert_eq!(recalled.visibility, MemoryVisibility::Shared);
        assert!(recalled.allowed_agents.contains(&"agent-2".to_string()));
    }

    #[test]
    fn test_tier2_memory_global() {
        let temp = tempfile::tempdir().unwrap();
        let mem1 = Tier2Memory::new(temp.path(), "agent-1").unwrap();
        let mem2 = Tier2Memory::new(temp.path(), "agent-2").unwrap();

        mem1.remember(
            "fact_1",
            "general",
            "agent-1",
            "session:test:turn:1",
            "Global fact",
        )
        .unwrap();

        // Make global
        mem1.make_global("fact_1").unwrap();

        // All agents can read it
        assert!(mem1.recall("fact_1").is_ok());
        assert!(mem2.recall("fact_1").is_ok());
    }

    #[test]
    fn test_tier2_memory_search() {
        let temp = tempfile::tempdir().unwrap();
        let mem = Tier2Memory::new(temp.path(), "agent-1").unwrap();

        mem.remember(
            "fact_1",
            "weather",
            "agent-1",
            "session:test:turn:1",
            "Paris is sunny",
        )
        .unwrap();

        mem.remember(
            "fact_2",
            "weather",
            "agent-1",
            "session:test:turn:2",
            "London is rainy",
        )
        .unwrap();

        mem.remember(
            "fact_3",
            "geography",
            "agent-1",
            "session:test:turn:3",
            "Paris is in France",
        )
        .unwrap();

        // Search by scope
        let results = mem.search("weather", None).unwrap();
        assert_eq!(results.len(), 2);

        // Search by scope and query
        let results = mem.search("weather", Some("Paris")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory_id, "fact_1");
    }

    #[test]
    fn test_tier2_memory_search_by_tags_requires_nonempty_tags() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
        let mem = Tier2Memory::with_store(store, "agent-1");
        assert!(mem
            .search_by_tags("general", &[], None, 10)
            .unwrap_err()
            .to_string()
            .contains("tags must not be empty"));
    }

    #[test]
    fn test_tier2_memory_search_by_tags_limit_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
        let mem = Tier2Memory::with_store(store, "agent-1");
        let err0 = mem
            .search_by_tags("general", &["t".to_string()], None, 0)
            .unwrap_err()
            .to_string();
        assert!(
            err0.contains("limit must be between 1 and 100 inclusive"),
            "{}",
            err0
        );
        let err101 = mem
            .search_by_tags("general", &["t".to_string()], None, 101)
            .unwrap_err()
            .to_string();
        assert!(
            err101.contains("limit must be between 1 and 100 inclusive"),
            "{}",
            err101
        );
    }

    #[test]
    fn test_tier2_memory_search_by_tags_filters() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
        let mem = Tier2Memory::with_store(Arc::clone(&store), "agent-1");

        let mut m1 = MemoryObject::new(
            "m1".into(),
            "lessons".into(),
            "agent-1".into(),
            "agent-1".into(),
            "ref:1".into(),
            "async needs Send".into(),
        );
        m1.tags = vec!["type:error_lesson".to_string(), "domain:http".to_string()];
        mem.save_memory(&m1).unwrap();

        let mut m2 = MemoryObject::new(
            "m2".into(),
            "lessons".into(),
            "agent-1".into(),
            "agent-1".into(),
            "ref:2".into(),
            "other".into(),
        );
        m2.tags = vec!["type:fact".to_string()];
        mem.save_memory(&m2).unwrap();

        let found = mem
            .search_by_tags(
                "lessons",
                &["type:error_lesson".to_string()],
                None,
                10,
            )
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
            .unwrap();
        assert_eq!(found2.len(), 1);

        let found_text = mem
            .search_by_tags(
                "lessons",
                &["type:error_lesson".to_string()],
                Some("Send"),
                10,
            )
            .unwrap();
        assert_eq!(found_text.len(), 1);
    }

    #[test]
    fn test_tier2_memory_search_by_tags_limit_applies_after_visibility() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
        let writer = Tier2Memory::with_store(Arc::clone(&store), "writer-agent");
        let reader = Tier2Memory::with_store(store, "reader-agent");

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
        writer.save_memory(&shared).unwrap();
        writer
            .share_with("shared-hit", vec!["reader-agent".to_string()])
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
            writer.save_memory(&private).unwrap();
        }

        let found = reader
            .search_by_tags("lessons", &["topic:rust".to_string()], None, 1)
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].memory_id, "shared-hit");
    }

    #[test]
    fn test_tier2_memory_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let mem = Tier2Memory::new(temp.path(), "agent-1").unwrap();

        let memory = mem
            .remember(
                "fact_1",
                "general",
                "agent-1",
                "session:abc123:turn:5",
                "Important fact",
            )
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
