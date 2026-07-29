//! Integration tests for Tier 2 memory provenance and session/global visibility.

use std::sync::Arc;

use autonoetic_gateway::runtime::memory::{MemoryStore, SqliteMemoryStore, Tier2Memory};
use autonoetic_types::memory::{MemoryObject, MemoryVisibility};
use tempfile::tempdir;

/// Helper to create a test gateway directory with memory database
fn create_test_gateway() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".gateway")).unwrap();
    dir
}

/// Session-visible facts are readable by any agent in the same session_id.
#[tokio::test]
async fn test_tier2_memory_cross_agent_session_visibility() {
    let ws = create_test_gateway();
    let gateway_dir = ws.path().join(".gateway");

    let mem_writer = autonoetic_gateway::runtime::memory::Tier2Memory::open_for_agent(
        &gateway_dir,
        None,
        "writer-agent",
        Some("demo-session"),
    )
    .unwrap();

    let memory = mem_writer
        .remember(
            "fact_123",
            "general",
            "writer-agent",
            "session:demo-session:turn:1",
            "Paris is the capital of France",
        )
        .await
        .unwrap();

    assert_eq!(memory.memory_id, "fact_123");
    assert_eq!(memory.content, "Paris is the capital of France");
    assert_eq!(memory.visibility, MemoryVisibility::Private);

    let mem_reader = autonoetic_gateway::runtime::memory::Tier2Memory::open_for_agent(
        &gateway_dir,
        None,
        "reader-agent",
        Some("demo-session"),
    )
    .unwrap();

    let err = mem_reader.recall("fact_123").await.unwrap_err();
    assert!(err.to_string().contains("not accessible"));

    // Widen to session (same pattern as knowledge.store upsert)
    let mut m = mem_writer.recall("fact_123").await.unwrap();
    m.visibility = MemoryVisibility::Session {
        session_id: "demo-session".into(),
    };
    mem_writer.save_memory(&m).await.unwrap();

    let recalled = mem_reader.recall("fact_123").await.unwrap();
    assert_eq!(recalled.content, "Paris is the capital of France");
    assert_eq!(recalled.owner_agent_id, "writer-agent");
    match &recalled.visibility {
        MemoryVisibility::Session { session_id } => assert_eq!(session_id, "demo-session"),
        _ => panic!("expected session visibility"),
    }
}

/// Test that unauthorized agents cannot read private memories.
#[tokio::test]
async fn test_tier2_memory_unauthorized_access_denied() {
    let ws = create_test_gateway();
    let gateway_dir = ws.path().join(".gateway");

    let mem_a =
        autonoetic_gateway::runtime::memory::Tier2Memory::new(&gateway_dir, "agent-a").unwrap();

    mem_a
        .remember(
            "private_fact",
            "secrets",
            "agent-a",
            "test:unauthorized",
            "This is agent A's secret",
        )
        .await
        .unwrap();

    let recalled = mem_a.recall("private_fact").await.unwrap();
    assert_eq!(recalled.content, "This is agent A's secret");

    let mem_b =
        autonoetic_gateway::runtime::memory::Tier2Memory::new(&gateway_dir, "agent-b").unwrap();

    let err = mem_b.recall("private_fact").await.unwrap_err();
    assert!(err.to_string().contains("not accessible"));
    assert!(err.to_string().contains("agent-b"));
}

#[tokio::test]
async fn test_tier2_memory_provenance_tracking() {
    let ws = create_test_gateway();
    let gateway_dir = ws.path().join(".gateway");

    let mem =
        autonoetic_gateway::runtime::memory::Tier2Memory::new(&gateway_dir, "test-agent").unwrap();

    let memory = mem
        .remember(
            "provenance_test",
            "test",
            "test-agent",
            "session:abc123:turn:5",
            "Test content for provenance",
        )
        .await
        .unwrap();

    assert_eq!(memory.memory_id, "provenance_test");
    assert_eq!(memory.scope, "test");
    assert_eq!(memory.owner_agent_id, "test-agent");
    assert_eq!(memory.writer_agent_id, "test-agent");
    assert_eq!(memory.source_ref, "session:abc123:turn:5");
    assert!(!memory.created_at.is_empty());
    assert!(!memory.updated_at.is_empty());
    assert!(!memory.content_hash.is_empty());

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update("Test content for provenance".as_bytes());
    let expected_hash = hex::encode(hasher.finalize());
    assert_eq!(memory.content_hash, expected_hash);
}

#[tokio::test]
async fn test_tier2_memory_search_with_visibility() {
    let ws = create_test_gateway();
    let gateway_dir = ws.path().join(".gateway");

    let mem = autonoetic_gateway::runtime::memory::Tier2Memory::new(&gateway_dir, "search-agent")
        .unwrap();

    mem.remember(
        "fact_1",
        "weather",
        "search-agent",
        "test:1",
        "Paris is sunny",
    )
    .await
    .unwrap();

    mem.remember(
        "fact_2",
        "weather",
        "search-agent",
        "test:2",
        "London is rainy",
    )
    .await
    .unwrap();

    mem.remember(
        "fact_3",
        "geography",
        "search-agent",
        "test:3",
        "Paris is in France",
    )
    .await
    .unwrap();

    let results = mem.search("weather", None).await.unwrap();
    assert_eq!(results.len(), 2);

    let results = mem.search("weather", Some("Paris")).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].memory_id, "fact_1");

    let results = mem.search("nonexistent", None).await.unwrap();
    assert_eq!(results.len(), 0);
}

#[tokio::test]
async fn test_tier2_memory_global_visibility() {
    let ws = create_test_gateway();
    let gateway_dir = ws.path().join(".gateway");

    let mem_owner =
        autonoetic_gateway::runtime::memory::Tier2Memory::new(&gateway_dir, "owner-agent").unwrap();

    let mem_reader1 =
        autonoetic_gateway::runtime::memory::Tier2Memory::new(&gateway_dir, "reader-agent-1")
            .unwrap();

    let mem_reader2 =
        autonoetic_gateway::runtime::memory::Tier2Memory::new(&gateway_dir, "reader-agent-2")
            .unwrap();

    mem_owner
        .remember(
            "global_fact",
            "public",
            "owner-agent",
            "test:global",
            "This is public knowledge",
        )
        .await
        .unwrap();

    assert!(mem_reader1.recall("global_fact").await.is_err());
    assert!(mem_reader2.recall("global_fact").await.is_err());

    let global = mem_owner.make_global("global_fact").await.unwrap();
    assert_eq!(global.visibility, MemoryVisibility::Global);

    let r1 = mem_reader1.recall("global_fact").await.unwrap();
    assert_eq!(r1.content, "This is public knowledge");

    let r2 = mem_reader2.recall("global_fact").await.unwrap();
    assert_eq!(r2.content, "This is public knowledge");
}

#[tokio::test]
async fn test_tier2_memory_only_owner_can_make_global() {
    let ws = create_test_gateway();
    let gateway_dir = ws.path().join(".gateway");

    let mem_owner = autonoetic_gateway::runtime::memory::Tier2Memory::open_for_agent(
        &gateway_dir,
        None,
        "owner-agent",
        Some("wf-1"),
    )
    .unwrap();

    let mem_non_owner = autonoetic_gateway::runtime::memory::Tier2Memory::open_for_agent(
        &gateway_dir,
        None,
        "non-owner-agent",
        Some("wf-1"),
    )
    .unwrap();

    let mut obj = MemoryObject::new(
        "owned_fact".into(),
        "test".into(),
        "owner-agent".into(),
        "owner-agent".into(),
        "test:owned".into(),
        "Owned fact".into(),
    );
    obj.visibility = MemoryVisibility::Session {
        session_id: "wf-1".into(),
    };
    mem_owner.save_memory(&obj).await.unwrap();

    assert!(mem_non_owner.recall("owned_fact").await.is_ok());

    let err = mem_non_owner.make_global("owned_fact").await.unwrap_err();
    assert!(err
        .to_string()
        .contains("Only the owner can make a memory global"));
}

#[tokio::test]
async fn test_tier2_memory_list_scopes() {
    let ws = create_test_gateway();
    let gateway_dir = ws.path().join(".gateway");

    let mem =
        autonoetic_gateway::runtime::memory::Tier2Memory::new(&gateway_dir, "scope-agent").unwrap();

    let scopes = mem.list_scopes().await.unwrap();
    assert!(scopes.is_empty());

    mem.remember("f1", "scope_a", "scope-agent", "t:1", "content1")
        .await
        .unwrap();
    mem.remember("f2", "scope_b", "scope-agent", "t:2", "content2")
        .await
        .unwrap();
    mem.remember("f3", "scope_a", "scope-agent", "t:3", "content3")
        .await
        .unwrap();

    let scopes = mem.list_scopes().await.unwrap();
    assert_eq!(scopes.len(), 2);
    assert!(scopes.contains(&"scope_a".to_string()));
    assert!(scopes.contains(&"scope_b".to_string()));
}

#[tokio::test]
async fn test_tier2_memory_search_by_tags_cross_agent_session() {
    let ws = create_test_gateway();
    let gateway_dir = ws.path().join(".gateway");
    let store = Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );
    let mem_store: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::new(Arc::clone(&store)));

    let writer = Tier2Memory::with_store(Arc::clone(&mem_store), "writer-agent", None);

    let mut memory = MemoryObject::new(
        "tagged_fact".into(),
        "lessons".into(),
        "writer-agent".into(),
        "writer-agent".into(),
        "session:x:turn:1".into(),
        "Never block the runtime thread.".into(),
    );
    memory.tags = vec!["type:error_lesson".to_string(), "domain:async".to_string()];
    memory.visibility = MemoryVisibility::Session {
        session_id: "root-sess".into(),
    };
    writer.save_memory(&memory).await.unwrap();

    let reader = Tier2Memory::with_store(mem_store, "reader-agent", Some("root-sess".into()));
    let found = reader
        .search_by_tags(
            "lessons",
            &["type:error_lesson".to_string(), "domain:async".to_string()],
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].memory_id, "tagged_fact");

    let not_found = reader
        .search_by_tags(
            "lessons",
            &["type:error_lesson".to_string(), "missing".to_string()],
            None,
            10,
        )
        .await
        .unwrap();
    assert!(not_found.is_empty());
}
