//! SQLite-backed MemoryStore implementation wrapping GatewayStore.

use anyhow::Result;
use async_trait::async_trait;
use autonoetic_types::memory::MemoryObject;
use std::sync::Arc;

use crate::scheduler::gateway_store::GatewayStore;
use crate::runtime::memory::MemoryStore;

/// SQLite-backed memory store that delegates to the existing `GatewayStore` methods.
///
/// This is the default backend — all memory operations use the `memories` and
/// `memory_tags` tables in `gateway.db`.
pub struct SqliteMemoryStore {
    store: Arc<GatewayStore>,
}

impl SqliteMemoryStore {
    pub fn new(store: Arc<GatewayStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl MemoryStore for SqliteMemoryStore {
    async fn upsert(&self, memory: &MemoryObject) -> Result<()> {
        self.store.memory_upsert(memory)
    }

    async fn get(&self, memory_id: &str) -> Result<Option<MemoryObject>> {
        self.store.memory_get(memory_id)
    }

    async fn list_ids_for_scope(
        &self,
        scope: &str,
        content_substr: Option<&str>,
    ) -> Result<Vec<String>> {
        self.store.memory_list_ids_for_scope(scope, content_substr)
    }

    async fn list_ids_matching_tags(
        &self,
        scope: &str,
        agent_id: &str,
        reader_session_id: Option<&str>,
        tags: &[String],
        content_substr: Option<&str>,
        limit: i64,
    ) -> Result<Vec<String>> {
        self.store
            .memory_list_ids_matching_tags(
                scope,
                agent_id,
                reader_session_id,
                tags,
                content_substr,
                limit,
            )
    }

    async fn list_ids_owned_by(&self, owner_agent_id: &str) -> Result<Vec<String>> {
        self.store.memory_list_ids_owned_by(owner_agent_id)
    }

    async fn list_scopes_for_agent(
        &self,
        agent_id: &str,
        reader_session_id: Option<&str>,
    ) -> Result<Vec<String>> {
        self.store
            .memory_list_scopes_for_agent(agent_id, reader_session_id)
    }
}
